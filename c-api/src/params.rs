//! Advanced parameter API: `ZSTD_CCtx_setParameter` / `ZSTD_DCtx_setParameter`
//! families, bounds queries, pledged source size, and context reset.
//!
//! Parameters are sticky per context (upstream semantics): they survive
//! frames and are only cleared by `ZSTD_CCtx_reset` with a parameters
//! directive. `ZSTD_compress2` and the streaming entry points consume the
//! stored set when a frame starts.

use core::ffi::c_int;

use codec::encoding::{CParameter, CompressionLevel, CompressionParameters, Strategy};

use crate::context::{ZSTD_CCtx, ZSTD_DCtx};
use crate::error::{ZSTD_ErrorCode, encode};

/// `ZSTD_CONTENTSIZE_UNKNOWN` (`0ULL - 1`): the pledged-size sentinel for
/// "unknown", the default for every new frame.
pub(crate) const CONTENTSIZE_UNKNOWN: u64 = u64::MAX;

// `ZSTD_cParameter` discriminants (vendored `zstd.h` v1.5.7). The C enum
// arrives over the ABI as a plain `int`; wrappers match on these constants.
pub(crate) const ZSTD_C_COMPRESSION_LEVEL: c_int = 100;
pub(crate) const ZSTD_C_WINDOW_LOG: c_int = 101;
pub(crate) const ZSTD_C_HASH_LOG: c_int = 102;
pub(crate) const ZSTD_C_CHAIN_LOG: c_int = 103;
pub(crate) const ZSTD_C_SEARCH_LOG: c_int = 104;
pub(crate) const ZSTD_C_MIN_MATCH: c_int = 105;
pub(crate) const ZSTD_C_TARGET_LENGTH: c_int = 106;
pub(crate) const ZSTD_C_STRATEGY: c_int = 107;
pub(crate) const ZSTD_C_TARGET_CBLOCK_SIZE: c_int = 130;
pub(crate) const ZSTD_C_ENABLE_LDM: c_int = 160;
pub(crate) const ZSTD_C_LDM_HASH_LOG: c_int = 161;
pub(crate) const ZSTD_C_LDM_MIN_MATCH: c_int = 162;
pub(crate) const ZSTD_C_LDM_BUCKET_SIZE_LOG: c_int = 163;
pub(crate) const ZSTD_C_LDM_HASH_RATE_LOG: c_int = 164;
pub(crate) const ZSTD_C_CONTENT_SIZE_FLAG: c_int = 200;
pub(crate) const ZSTD_C_CHECKSUM_FLAG: c_int = 201;
pub(crate) const ZSTD_C_DICT_ID_FLAG: c_int = 202;
pub(crate) const ZSTD_C_NB_WORKERS: c_int = 400;
pub(crate) const ZSTD_C_JOB_SIZE: c_int = 401;
pub(crate) const ZSTD_C_OVERLAP_LOG: c_int = 402;

// `ZSTD_dParameter` discriminants.
pub(crate) const ZSTD_D_WINDOW_LOG_MAX: c_int = 100;

// `ZSTD_ResetDirective` discriminants.
pub(crate) const ZSTD_RESET_SESSION_ONLY: c_int = 1;
pub(crate) const ZSTD_RESET_PARAMETERS: c_int = 2;
pub(crate) const ZSTD_RESET_SESSION_AND_PARAMETERS: c_int = 3;

/// `ZSTD_TARGETCBLOCKSIZE_MIN` / `_MAX` (vendored header).
const TARGET_CBLOCK_SIZE_MIN: c_int = 1340;
const TARGET_CBLOCK_SIZE_MAX: c_int = 131_072;

/// Upstream `ZSTD_WINDOWLOG_LIMIT_DEFAULT`: the streaming decoder's default
/// window-size acceptance ceiling, `1 << 27` bytes.
pub(crate) const WINDOW_LOG_LIMIT_DEFAULT: c_int = 27;
/// Decoder-side window-log bounds (`ZSTD_WINDOWLOG_ABSOLUTEMIN` .. 64-bit
/// `ZSTD_WINDOWLOG_MAX`).
const D_WINDOW_LOG_MIN: c_int = 10;
const D_WINDOW_LOG_MAX: c_int = 31;

/// `ZSTD_bounds` — ABI mirror of the upstream struct: an error slot tested
/// with `ZSTD_isError` plus an inclusive `[lowerBound, upperBound]` range.
#[repr(C)]
#[allow(non_camel_case_types, non_snake_case)]
#[derive(Copy, Clone, Debug)]
pub struct ZSTD_bounds {
    pub error: usize,
    pub lowerBound: c_int,
    pub upperBound: c_int,
}

impl ZSTD_bounds {
    const fn ok(lower: c_int, upper: c_int) -> Self {
        Self {
            error: 0,
            lowerBound: lower,
            upperBound: upper,
        }
    }

    fn err(code: ZSTD_ErrorCode) -> Self {
        Self {
            error: encode(code),
            lowerBound: 0,
            upperBound: 0,
        }
    }
}

/// Sticky per-context compression parameters (upstream `ZSTD_CCtx_params`
/// equivalent). `None` knobs mean "auto" (value 0 in the C API): the level's
/// resolved defaults apply.
#[derive(Copy, Clone, Debug)]
pub(crate) struct CCtxParams {
    pub(crate) level: c_int,
    pub(crate) window_log: Option<u32>,
    pub(crate) hash_log: Option<u32>,
    pub(crate) chain_log: Option<u32>,
    pub(crate) search_log: Option<u32>,
    pub(crate) min_match: Option<u32>,
    pub(crate) target_length: Option<u32>,
    pub(crate) strategy: Option<Strategy>,
    pub(crate) enable_ldm: bool,
    pub(crate) ldm_hash_log: Option<u32>,
    pub(crate) ldm_min_match: Option<u32>,
    pub(crate) ldm_bucket_size_log: Option<u32>,
    pub(crate) ldm_hash_rate_log: Option<u32>,
    pub(crate) content_size_flag: bool,
    pub(crate) checksum_flag: bool,
    pub(crate) dict_id_flag: bool,
    /// Accepted and stored but single-threaded for now: real worker support
    /// is the multi-threading milestone; until then every value compresses
    /// on the calling thread (the blocking mode every consumer must already
    /// handle, since upstream blocks whenever it lacks worker budget too).
    pub(crate) nb_workers: c_int,
    /// Only meaningful with workers; stored for `getParameter` symmetry.
    pub(crate) job_size: c_int,
    /// Only meaningful with workers; stored for `getParameter` symmetry.
    pub(crate) overlap_log: c_int,
    /// Accepted and stored; the encoder's block sizing currently ignores the
    /// convergence target (upstream documents it as best-effort, not a
    /// guarantee).
    pub(crate) target_cblock_size: c_int,
    /// Pledged size of the next frame (`ZSTD_CCtx_setPledgedSrcSize`).
    /// Consumed by the next frame start, then reset to unknown.
    pub(crate) pledged_src_size: u64,
}

impl Default for CCtxParams {
    fn default() -> Self {
        Self {
            level: 3, // ZSTD_CLEVEL_DEFAULT
            window_log: None,
            hash_log: None,
            chain_log: None,
            search_log: None,
            min_match: None,
            target_length: None,
            strategy: None,
            enable_ldm: false,
            ldm_hash_log: None,
            ldm_min_match: None,
            ldm_bucket_size_log: None,
            ldm_hash_rate_log: None,
            content_size_flag: true,
            checksum_flag: false,
            dict_id_flag: true,
            nb_workers: 0,
            job_size: 0,
            overlap_log: 0,
            target_cblock_size: 0,
            pledged_src_size: CONTENTSIZE_UNKNOWN,
        }
    }
}

impl CCtxParams {
    /// Resolve the sticky knob set into the codec's validated
    /// [`CompressionParameters`]. `None` on an internally-inconsistent set
    /// (should be unreachable: every knob was bounds-checked on entry).
    pub(crate) fn resolve(&self) -> Option<CompressionParameters> {
        let mut builder = CompressionParameters::builder(CompressionLevel::from_level(self.level));
        if let Some(v) = self.window_log {
            builder = builder.window_log(v);
        }
        if let Some(v) = self.hash_log {
            builder = builder.hash_log(v);
        }
        if let Some(v) = self.chain_log {
            builder = builder.chain_log(v);
        }
        if let Some(v) = self.search_log {
            builder = builder.search_log(v);
        }
        if let Some(v) = self.min_match {
            builder = builder.min_match(v);
        }
        if let Some(v) = self.target_length {
            builder = builder.target_length(v);
        }
        if let Some(v) = self.strategy {
            builder = builder.strategy(v);
        }
        if self.enable_ldm {
            builder = builder.enable_long_distance_matching(true);
            if let Some(v) = self.ldm_hash_log {
                builder = builder.ldm_hash_log(v);
            }
            if let Some(v) = self.ldm_min_match {
                builder = builder.ldm_min_match(v);
            }
            if let Some(v) = self.ldm_bucket_size_log {
                builder = builder.ldm_bucket_size_log(v);
            }
            if let Some(v) = self.ldm_hash_rate_log {
                builder = builder.ldm_hash_rate_log(v);
            }
        }
        builder.build().ok()
    }
}

/// Bounds for one `ZSTD_cParameter` discriminant, or `None` for an unknown /
/// unsupported parameter.
fn c_param_bounds(param: c_int) -> Option<(c_int, c_int)> {
    let from_codec = |p: CParameter| {
        let b = p.bounds();
        (b.lower_bound as c_int, b.upper_bound as c_int)
    };
    Some(match param {
        ZSTD_C_COMPRESSION_LEVEL => (CompressionLevel::MIN_LEVEL, CompressionLevel::MAX_LEVEL),
        ZSTD_C_WINDOW_LOG => from_codec(CParameter::WindowLog),
        ZSTD_C_HASH_LOG => from_codec(CParameter::HashLog),
        ZSTD_C_CHAIN_LOG => from_codec(CParameter::ChainLog),
        ZSTD_C_SEARCH_LOG => from_codec(CParameter::SearchLog),
        ZSTD_C_MIN_MATCH => from_codec(CParameter::MinMatch),
        ZSTD_C_TARGET_LENGTH => from_codec(CParameter::TargetLength),
        ZSTD_C_STRATEGY => from_codec(CParameter::Strategy),
        ZSTD_C_TARGET_CBLOCK_SIZE => (TARGET_CBLOCK_SIZE_MIN, TARGET_CBLOCK_SIZE_MAX),
        ZSTD_C_ENABLE_LDM => (0, 1),
        ZSTD_C_LDM_HASH_LOG => from_codec(CParameter::LdmHashLog),
        ZSTD_C_LDM_MIN_MATCH => from_codec(CParameter::LdmMinMatch),
        ZSTD_C_LDM_BUCKET_SIZE_LOG => from_codec(CParameter::LdmBucketSizeLog),
        ZSTD_C_LDM_HASH_RATE_LOG => from_codec(CParameter::LdmHashRateLog),
        ZSTD_C_CONTENT_SIZE_FLAG | ZSTD_C_CHECKSUM_FLAG | ZSTD_C_DICT_ID_FLAG => (0, 1),
        // Single-threaded build surface: workers are accepted (stored) but
        // compression runs on the calling thread, so the advertised bounds
        // mirror upstream's non-multithreaded build.
        ZSTD_C_NB_WORKERS | ZSTD_C_JOB_SIZE | ZSTD_C_OVERLAP_LOG => (0, 0),
        _ => return None,
    })
}

/// `ZSTD_bounds ZSTD_cParam_getBounds(ZSTD_cParameter cParam)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_cParam_getBounds(param: c_int) -> ZSTD_bounds {
    match c_param_bounds(param) {
        Some((lower, upper)) => ZSTD_bounds::ok(lower, upper),
        None => ZSTD_bounds::err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// `ZSTD_bounds ZSTD_dParam_getBounds(ZSTD_dParameter dParam)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_dParam_getBounds(param: c_int) -> ZSTD_bounds {
    match param {
        ZSTD_D_WINDOW_LOG_MAX => ZSTD_bounds::ok(D_WINDOW_LOG_MIN, D_WINDOW_LOG_MAX),
        _ => ZSTD_bounds::err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// Validate `value` against the parameter's bounds. `0` is the universal
/// "auto / default" escape for the tunables and is always accepted; the
/// boolean / worker knobs treat their range literally.
fn check_bounds(param: c_int, value: c_int) -> Result<(), ZSTD_ErrorCode> {
    let Some((lower, upper)) = c_param_bounds(param) else {
        return Err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported);
    };
    let zero_is_auto = !matches!(
        param,
        ZSTD_C_CONTENT_SIZE_FLAG
            | ZSTD_C_CHECKSUM_FLAG
            | ZSTD_C_DICT_ID_FLAG
            | ZSTD_C_ENABLE_LDM
            | ZSTD_C_NB_WORKERS
            | ZSTD_C_JOB_SIZE
            | ZSTD_C_OVERLAP_LOG
            | ZSTD_C_COMPRESSION_LEVEL
    );
    if zero_is_auto && value == 0 {
        return Ok(());
    }
    if value < lower || value > upper {
        return Err(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    Ok(())
}

/// `size_t ZSTD_CCtx_setParameter(ZSTD_CCtx* cctx, ZSTD_cParameter param, int value)`.
///
/// Stores one sticky compression parameter. Rejected with
/// `parameter_outOfBound` / `parameter_unsupported` outside the advertised
/// bounds; `0` means "back to auto" for the tunables.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_setParameter(
    cctx: *mut ZSTD_CCtx,
    param: c_int,
    value: c_int,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    if cctx.stream_in_progress() {
        // Upstream forbids parameter changes mid-frame (stage != init).
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    if let Err(code) = check_bounds(param, value) {
        return encode(code);
    }
    let p = &mut cctx.params;
    let opt = |v: c_int| (v != 0).then_some(v as u32);
    match param {
        ZSTD_C_COMPRESSION_LEVEL => {
            // Value 0 selects the default level, mirroring upstream.
            p.level = if value == 0 { 3 } else { value };
        }
        ZSTD_C_WINDOW_LOG => p.window_log = opt(value),
        ZSTD_C_HASH_LOG => p.hash_log = opt(value),
        ZSTD_C_CHAIN_LOG => p.chain_log = opt(value),
        ZSTD_C_SEARCH_LOG => p.search_log = opt(value),
        ZSTD_C_MIN_MATCH => p.min_match = opt(value),
        ZSTD_C_TARGET_LENGTH => {
            // 0 is both "auto" and a legal literal for targetLength; treat
            // it as auto like upstream's special-value rule.
            p.target_length = opt(value);
        }
        ZSTD_C_STRATEGY => {
            p.strategy = if value == 0 {
                None
            } else {
                Strategy::from_ordinal(value as u32)
            };
        }
        ZSTD_C_TARGET_CBLOCK_SIZE => p.target_cblock_size = value,
        ZSTD_C_ENABLE_LDM => p.enable_ldm = value != 0,
        ZSTD_C_LDM_HASH_LOG => p.ldm_hash_log = opt(value),
        ZSTD_C_LDM_MIN_MATCH => p.ldm_min_match = opt(value),
        ZSTD_C_LDM_BUCKET_SIZE_LOG => p.ldm_bucket_size_log = opt(value),
        ZSTD_C_LDM_HASH_RATE_LOG => p.ldm_hash_rate_log = opt(value),
        ZSTD_C_CONTENT_SIZE_FLAG => p.content_size_flag = value != 0,
        ZSTD_C_CHECKSUM_FLAG => p.checksum_flag = value != 0,
        ZSTD_C_DICT_ID_FLAG => p.dict_id_flag = value != 0,
        ZSTD_C_NB_WORKERS => p.nb_workers = value,
        ZSTD_C_JOB_SIZE => p.job_size = value,
        ZSTD_C_OVERLAP_LOG => p.overlap_log = value,
        _ => return encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
    0
}

/// `size_t ZSTD_CCtx_getParameter(const ZSTD_CCtx* cctx, ZSTD_cParameter param, int* value)`.
///
/// Reads back the currently stored value (`0` = auto for unset tunables).
///
/// # Safety
/// `cctx` must be live or `NULL`; `value` must be a valid writable `int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_getParameter(
    cctx: *const ZSTD_CCtx,
    param: c_int,
    value: *mut c_int,
) -> usize {
    if cctx.is_null() || value.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let p = unsafe { &(*cctx).params };
    let opt = |v: Option<u32>| v.map_or(0, |x| x as c_int);
    let out = match param {
        ZSTD_C_COMPRESSION_LEVEL => p.level,
        ZSTD_C_WINDOW_LOG => opt(p.window_log),
        ZSTD_C_HASH_LOG => opt(p.hash_log),
        ZSTD_C_CHAIN_LOG => opt(p.chain_log),
        ZSTD_C_SEARCH_LOG => opt(p.search_log),
        ZSTD_C_MIN_MATCH => opt(p.min_match),
        ZSTD_C_TARGET_LENGTH => opt(p.target_length),
        ZSTD_C_STRATEGY => p.strategy.map_or(0, |s| s.ordinal() as c_int),
        ZSTD_C_TARGET_CBLOCK_SIZE => p.target_cblock_size,
        ZSTD_C_ENABLE_LDM => c_int::from(p.enable_ldm),
        ZSTD_C_LDM_HASH_LOG => opt(p.ldm_hash_log),
        ZSTD_C_LDM_MIN_MATCH => opt(p.ldm_min_match),
        ZSTD_C_LDM_BUCKET_SIZE_LOG => opt(p.ldm_bucket_size_log),
        ZSTD_C_LDM_HASH_RATE_LOG => opt(p.ldm_hash_rate_log),
        ZSTD_C_CONTENT_SIZE_FLAG => c_int::from(p.content_size_flag),
        ZSTD_C_CHECKSUM_FLAG => c_int::from(p.checksum_flag),
        ZSTD_C_DICT_ID_FLAG => c_int::from(p.dict_id_flag),
        ZSTD_C_NB_WORKERS => p.nb_workers,
        ZSTD_C_JOB_SIZE => p.job_size,
        ZSTD_C_OVERLAP_LOG => p.overlap_log,
        _ => return encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    };
    unsafe { value.write(out) };
    0
}

/// `size_t ZSTD_CCtx_setPledgedSrcSize(ZSTD_CCtx* cctx, unsigned long long pledgedSrcSize)`.
///
/// Records the total size of the NEXT frame (written into its header unless
/// `ZSTD_c_contentSizeFlag` forbids it, validated at frame end). Consumed by
/// the next frame start; `ZSTD_CONTENTSIZE_UNKNOWN` restores the default.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_setPledgedSrcSize(
    cctx: *mut ZSTD_CCtx,
    pledged_src_size: u64,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    if cctx.stream_in_progress() {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    cctx.params.pledged_src_size = pledged_src_size;
    0
}

/// `size_t ZSTD_CCtx_reset(ZSTD_CCtx* cctx, ZSTD_ResetDirective reset)`.
///
/// Session reset abandons any in-flight frame (never fails); parameter reset
/// restores defaults and drops dictionary references, and is only legal
/// between frames.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_reset(cctx: *mut ZSTD_CCtx, reset: c_int) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    match reset {
        ZSTD_RESET_SESSION_ONLY => {
            cctx.reset_session();
            0
        }
        ZSTD_RESET_PARAMETERS => {
            if cctx.stream_in_progress() {
                return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
            }
            cctx.reset_parameters();
            0
        }
        ZSTD_RESET_SESSION_AND_PARAMETERS => {
            cctx.reset_session();
            cctx.reset_parameters();
            0
        }
        _ => encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// `size_t ZSTD_DCtx_setParameter(ZSTD_DCtx* dctx, ZSTD_dParameter param, int value)`.
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_setParameter(
    dctx: *mut ZSTD_DCtx,
    param: c_int,
    value: c_int,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    match param {
        ZSTD_D_WINDOW_LOG_MAX => {
            if value != 0 && (value < D_WINDOW_LOG_MIN || value > D_WINDOW_LOG_MAX) {
                return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
            }
            dctx.window_log_max = if value == 0 {
                WINDOW_LOG_LIMIT_DEFAULT
            } else {
                value
            };
            0
        }
        _ => encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// `size_t ZSTD_DCtx_getParameter(const ZSTD_DCtx* dctx, ZSTD_dParameter param, int* value)`.
///
/// # Safety
/// `dctx` must be live or `NULL`; `value` must be a valid writable `int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_getParameter(
    dctx: *const ZSTD_DCtx,
    param: c_int,
    value: *mut c_int,
) -> usize {
    if dctx.is_null() || value.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    match param {
        ZSTD_D_WINDOW_LOG_MAX => {
            unsafe { value.write((*dctx).window_log_max) };
            0
        }
        _ => encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// `size_t ZSTD_DCtx_reset(ZSTD_DCtx* dctx, ZSTD_ResetDirective reset)`.
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_reset(dctx: *mut ZSTD_DCtx, reset: c_int) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    match reset {
        ZSTD_RESET_SESSION_ONLY => {
            dctx.reset_session();
            0
        }
        ZSTD_RESET_PARAMETERS => {
            dctx.reset_parameters();
            0
        }
        ZSTD_RESET_SESSION_AND_PARAMETERS => {
            dctx.reset_session();
            dctx.reset_parameters();
            0
        }
        _ => encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}
