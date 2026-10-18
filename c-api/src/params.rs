//! Advanced parameter API: `ZSTD_CCtx_setParameter` / `ZSTD_DCtx_setParameter`
//! families, bounds queries, pledged source size, and context reset.
//!
//! Parameters are sticky per context (upstream semantics): they survive
//! frames and are only cleared by `ZSTD_CCtx_reset` with a parameters
//! directive. `ZSTD_compress2` and the streaming entry points consume the
//! stored set when a frame starts.

use core::ffi::{c_int, c_uint};

use codec::decoding::ContentChecksum;
use codec::encoding::{CParameter, CompressionLevel, LevelParameters, Strategy};

use crate::cdict::{ZSTD_compressionParameters, ZSTD_frameParameters};
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
// Experimental `ZSTD_cParameter` discriminants (`ZSTD_c_experimentalParamN`).
pub(crate) const ZSTD_C_RSYNCABLE: c_int = 500;
pub(crate) const ZSTD_C_FORMAT: c_int = 10;
pub(crate) const ZSTD_C_FORCE_MAX_WINDOW: c_int = 1000;
pub(crate) const ZSTD_C_FORCE_ATTACH_DICT: c_int = 1001;
pub(crate) const ZSTD_C_LITERAL_COMPRESSION_MODE: c_int = 1002;
pub(crate) const ZSTD_C_SRC_SIZE_HINT: c_int = 1004;
pub(crate) const ZSTD_C_ENABLE_DEDICATED_DICT_SEARCH: c_int = 1005;
pub(crate) const ZSTD_C_STABLE_IN_BUFFER: c_int = 1006;
pub(crate) const ZSTD_C_STABLE_OUT_BUFFER: c_int = 1007;
pub(crate) const ZSTD_C_BLOCK_DELIMITERS: c_int = 1008;
pub(crate) const ZSTD_C_VALIDATE_SEQUENCES: c_int = 1009;
pub(crate) const ZSTD_C_SPLIT_AFTER_SEQUENCES: c_int = 1010;
pub(crate) const ZSTD_C_USE_ROW_MATCH_FINDER: c_int = 1011;
pub(crate) const ZSTD_C_DETERMINISTIC_REF_PREFIX: c_int = 1012;
pub(crate) const ZSTD_C_PREFETCH_CDICT_TABLES: c_int = 1013;
pub(crate) const ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK: c_int = 1014;
pub(crate) const ZSTD_C_MAX_BLOCK_SIZE: c_int = 1015;
pub(crate) const ZSTD_C_REPCODE_RESOLUTION: c_int = 1016;
pub(crate) const ZSTD_C_BLOCK_SPLITTER_LEVEL: c_int = 1017;

// Upstream's worker-pool limits (zstdmt_compress.h:29-36).
#[cfg(target_pointer_width = "32")]
const NB_WORKERS_MAX: c_int = 64;
#[cfg(not(target_pointer_width = "32"))]
const NB_WORKERS_MAX: c_int = 256;
/// The smallest job size other than 0 (`ZSTDMT_JOBSIZE_MIN`).
pub(crate) const JOB_SIZE_MIN: c_int = 512 << 10;
#[cfg(target_pointer_width = "32")]
const JOB_SIZE_MAX: c_int = 512 << 20;
#[cfg(not(target_pointer_width = "32"))]
const JOB_SIZE_MAX: c_int = 1 << 30;

// `ZSTD_dParameter` discriminants.
pub(crate) const ZSTD_D_WINDOW_LOG_MAX: c_int = 100;
pub(crate) const ZSTD_D_FORMAT: c_int = 1000;
pub(crate) const ZSTD_D_STABLE_OUT_BUFFER: c_int = 1001;
pub(crate) const ZSTD_D_FORCE_IGNORE_CHECKSUM: c_int = 1002;
pub(crate) const ZSTD_D_REF_MULTIPLE_DDICTS: c_int = 1003;
pub(crate) const ZSTD_D_DISABLE_HUFFMAN_ASSEMBLY: c_int = 1004;
pub(crate) const ZSTD_D_MAX_BLOCK_SIZE: c_int = 1005;

// `ZSTD_format_e` values.
pub(crate) const ZSTD_F_ZSTD1: c_int = 0;
pub(crate) const ZSTD_F_ZSTD1_MAGICLESS: c_int = 1;

// `ZSTD_ResetDirective` discriminants.
pub(crate) const ZSTD_RESET_SESSION_ONLY: c_int = 1;
pub(crate) const ZSTD_RESET_PARAMETERS: c_int = 2;
pub(crate) const ZSTD_RESET_SESSION_AND_PARAMETERS: c_int = 3;

/// `ZSTD_TARGETCBLOCKSIZE_MIN` / `_MAX` (vendored header).
// Single source of truth with the codec's `set_target_block_size` clamp.
const TARGET_CBLOCK_SIZE_MIN: c_int = codec::MIN_TARGET_BLOCK_SIZE as c_int;
const TARGET_CBLOCK_SIZE_MAX: c_int = codec::MAX_BLOCK_SIZE as c_int;
/// `ZSTD_BLOCKSIZE_MAX_MIN`, shared with the codec's `set_max_block_size`.
const MAX_BLOCK_SIZE_MIN: c_int = codec::MIN_MAX_BLOCK_SIZE as c_int;

/// Upstream `ZSTD_WINDOWLOG_LIMIT_DEFAULT`: the streaming decoder's default
/// window-size acceptance ceiling is `1 << 27` bytes.
const WINDOW_LOG_LIMIT_DEFAULT: c_int = 27;
/// That default as a byte count.
pub(crate) const MAX_WINDOW_SIZE_DEFAULT: u64 = 1 << WINDOW_LOG_LIMIT_DEFAULT;
/// Decoder-side window-log bounds (`ZSTD_WINDOWLOG_ABSOLUTEMIN` ..
/// `ZSTD_WINDOWLOG_MAX`). The ceiling is pointer-width dependent upstream
/// (`ZSTD_WINDOWLOG_MAX_32` = 30, `ZSTD_WINDOWLOG_MAX_64` = 31 via
/// `sizeof(size_t)`), so the advertised bounds match the header on 32-bit
/// builds too.
const D_WINDOW_LOG_MIN: c_int = 10;
#[cfg(target_pointer_width = "64")]
const D_WINDOW_LOG_MAX: c_int = 31;
#[cfg(not(target_pointer_width = "64"))]
const D_WINDOW_LOG_MAX: c_int = 30;

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

/// Bounds for one `ZSTD_cParameter` discriminant, or `None` for an unknown /
/// unsupported parameter.
pub(crate) fn c_param_bounds(param: c_int) -> Option<(c_int, c_int)> {
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
        // `ZSTD_ParamSwitch_e`: auto, enable, disable.
        ZSTD_C_ENABLE_LDM
        | ZSTD_C_LITERAL_COMPRESSION_MODE
        | ZSTD_C_SPLIT_AFTER_SEQUENCES
        | ZSTD_C_USE_ROW_MATCH_FINDER
        | ZSTD_C_PREFETCH_CDICT_TABLES
        | ZSTD_C_REPCODE_RESOLUTION => (0, 2),
        ZSTD_C_LDM_HASH_LOG => from_codec(CParameter::LdmHashLog),
        ZSTD_C_LDM_MIN_MATCH => from_codec(CParameter::LdmMinMatch),
        ZSTD_C_LDM_BUCKET_SIZE_LOG => from_codec(CParameter::LdmBucketSizeLog),
        ZSTD_C_LDM_HASH_RATE_LOG => from_codec(CParameter::LdmHashRateLog),
        ZSTD_C_CONTENT_SIZE_FLAG | ZSTD_C_CHECKSUM_FLAG | ZSTD_C_DICT_ID_FLAG => (0, 1),
        // The bounds of upstream's multi-threaded build, the one systems ship
        // (zstd_compress.c:480-506): its callers set these and expect them taken.
        ZSTD_C_NB_WORKERS => (0, NB_WORKERS_MAX),
        ZSTD_C_JOB_SIZE => (0, JOB_SIZE_MAX),
        ZSTD_C_OVERLAP_LOG => (0, 9),
        ZSTD_C_RSYNCABLE
        | ZSTD_C_FORMAT
        | ZSTD_C_FORCE_MAX_WINDOW
        | ZSTD_C_ENABLE_DEDICATED_DICT_SEARCH
        | ZSTD_C_STABLE_IN_BUFFER
        | ZSTD_C_STABLE_OUT_BUFFER
        | ZSTD_C_BLOCK_DELIMITERS
        | ZSTD_C_VALIDATE_SEQUENCES
        | ZSTD_C_DETERMINISTIC_REF_PREFIX
        | ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK => (0, 1),
        // `ZSTD_dictDefaultAttach` .. `ZSTD_dictForceLoad`.
        ZSTD_C_FORCE_ATTACH_DICT => (0, 3),
        ZSTD_C_SRC_SIZE_HINT => (0, c_int::MAX),
        ZSTD_C_MAX_BLOCK_SIZE => (MAX_BLOCK_SIZE_MIN, TARGET_CBLOCK_SIZE_MAX),
        // `ZSTD_BLOCKSPLITTER_LEVEL_MAX`.
        ZSTD_C_BLOCK_SPLITTER_LEVEL => (0, 6),
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
        ZSTD_D_FORMAT => ZSTD_bounds::ok(ZSTD_F_ZSTD1, ZSTD_F_ZSTD1_MAGICLESS),
        ZSTD_D_STABLE_OUT_BUFFER
        | ZSTD_D_FORCE_IGNORE_CHECKSUM
        | ZSTD_D_REF_MULTIPLE_DDICTS
        | ZSTD_D_DISABLE_HUFFMAN_ASSEMBLY => ZSTD_bounds::ok(0, 1),
        ZSTD_D_MAX_BLOCK_SIZE => ZSTD_bounds::ok(MAX_BLOCK_SIZE_MIN, TARGET_CBLOCK_SIZE_MAX),
        _ => ZSTD_bounds::err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// `ZSTD_parameters` — ABI mirror of the upstream struct: compression
/// parameters followed by frame parameters.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZSTD_parameters {
    pub cParams: ZSTD_compressionParameters,
    pub fParams: ZSTD_frameParameters,
}

impl From<LevelParameters> for ZSTD_compressionParameters {
    fn from(p: LevelParameters) -> Self {
        Self {
            windowLog: p.window_log,
            chainLog: p.chain_log,
            hashLog: p.hash_log,
            searchLog: p.search_log,
            minMatch: p.min_match,
            targetLength: p.target_length,
            strategy: p.strategy.ordinal(),
        }
    }
}

/// `ZSTD_getCParams` / `ZSTD_adjustCParams` spell an unknown source size as
/// `0`, as well as `ZSTD_CONTENTSIZE_UNKNOWN`.
fn known_source_size(size: u64) -> Option<u64> {
    (size != 0 && size != CONTENTSIZE_UNKNOWN).then_some(size)
}

/// `ZSTD_compressionParameters ZSTD_getCParams(int compressionLevel, unsigned
/// long long estimatedSrcSize, size_t dictSize)` — the parameters
/// `compressionLevel` selects for a source of that size (`0` = unknown) and
/// dictionary.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_getCParams(
    compression_level: c_int,
    estimated_src_size: u64,
    dict_size: usize,
) -> ZSTD_compressionParameters {
    LevelParameters::for_level(
        compression_level,
        known_source_size(estimated_src_size),
        dict_size,
    )
    .into()
}

/// `ZSTD_parameters ZSTD_getParams(int compressionLevel, unsigned long long
/// estimatedSrcSize, size_t dictSize)` — [`ZSTD_getCParams`] with the default
/// frame parameters (content size written, no checksum, dictionary ID written).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_getParams(
    compression_level: c_int,
    estimated_src_size: u64,
    dict_size: usize,
) -> ZSTD_parameters {
    ZSTD_parameters {
        cParams: ZSTD_getCParams(compression_level, estimated_src_size, dict_size),
        // Upstream leaves `noDictIDFlag` 0, which writes the ID.
        fParams: ZSTD_frameParameters {
            contentSizeFlag: 1,
            checksumFlag: 0,
            noDictIDFlag: 0,
        },
    }
}

/// The seven compression-parameter fields with the codec knob each one sets.
fn cparam_fields(cparams: &ZSTD_compressionParameters) -> [(CParameter, c_uint); 7] {
    [
        (CParameter::WindowLog, cparams.windowLog),
        (CParameter::ChainLog, cparams.chainLog),
        (CParameter::HashLog, cparams.hashLog),
        (CParameter::SearchLog, cparams.searchLog),
        (CParameter::MinMatch, cparams.minMatch),
        (CParameter::TargetLength, cparams.targetLength),
        (CParameter::Strategy, cparams.strategy),
    ]
}

/// `size_t ZSTD_checkCParams(ZSTD_compressionParameters params)` — 0 when every
/// field lies within its bounds, else `parameter_outOfBound`. Unlike
/// `ZSTD_CCtx_setParameter`, `0` is a value here, not "auto".
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_checkCParams(cparams: ZSTD_compressionParameters) -> usize {
    let in_bounds = cparam_fields(&cparams)
        .iter()
        .all(|&(parameter, value)| parameter.bounds().contains(i64::from(value)));
    if in_bounds {
        0
    } else {
        encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound)
    }
}

/// `ZSTD_compressionParameters ZSTD_adjustCParams(ZSTD_compressionParameters
/// cPar, unsigned long long srcSize, size_t dictSize)` — `cPar` with each field
/// brought within its bounds, then fitted to a source of `srcSize` bytes
/// (`0` = unknown) and a `dictSize`-byte dictionary.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_adjustCParams(
    cparams: ZSTD_compressionParameters,
    src_size: u64,
    dict_size: usize,
) -> ZSTD_compressionParameters {
    let [window, chain, hash, search, min_match, target, strategy] =
        cparam_fields(&cparams).map(|(parameter, value)| {
            // Every bound fits `u32`, so the clamped value does too.
            parameter.bounds().clamp(i64::from(value)) as u32
        });
    let clamped = LevelParameters {
        window_log: window,
        chain_log: chain,
        hash_log: hash,
        search_log: search,
        min_match,
        target_length: target,
        strategy: Strategy::from_ordinal(strategy).expect("clamped into 1..=9"),
    };
    clamped
        .adjusted(known_source_size(src_size), dict_size)
        .into()
}

/// `size_t ZSTD_CCtx_setParameter(ZSTD_CCtx* cctx, ZSTD_cParameter param, int value)`.
///
/// Stores one sticky compression parameter. Rejected with
/// `parameter_outOfBound` / `parameter_unsupported` outside the advertised
/// bounds; `0` means "back to auto" for the tunables, the level is clamped
/// into range rather than refused, and the on/off flags (content size,
/// checksum, dictionary ID, force max window, dedicated dictionary search)
/// take any non-zero value as on, as upstream does.
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
    match cctx.params.set(param, value) {
        Ok(()) => 0,
        Err(code) => encode(code),
    }
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
    match unsafe { &(*cctx).params }.get(param) {
        Ok(out) => {
            unsafe { value.write(out) };
            0
        }
        Err(code) => encode(code),
    }
}

/// `size_t ZSTD_CCtx_setCParams(ZSTD_CCtx* cctx, ZSTD_compressionParameters
/// cparams)` — set all seven compression parameters at once, or none of them
/// when one fails `ZSTD_checkCParams`.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_setCParams(
    cctx: *mut ZSTD_CCtx,
    cparams: ZSTD_compressionParameters,
) -> usize {
    let checked = ZSTD_checkCParams(cparams);
    if crate::error::result_is_error(checked) {
        return checked;
    }
    for (param, value) in [
        (ZSTD_C_WINDOW_LOG, cparams.windowLog),
        (ZSTD_C_CHAIN_LOG, cparams.chainLog),
        (ZSTD_C_HASH_LOG, cparams.hashLog),
        (ZSTD_C_SEARCH_LOG, cparams.searchLog),
        (ZSTD_C_MIN_MATCH, cparams.minMatch),
        (ZSTD_C_TARGET_LENGTH, cparams.targetLength),
        (ZSTD_C_STRATEGY, cparams.strategy),
    ] {
        // Checked above, so every value fits `int`.
        let rc = unsafe { ZSTD_CCtx_setParameter(cctx, param, value as c_int) };
        if crate::error::result_is_error(rc) {
            return rc;
        }
    }
    0
}

/// `size_t ZSTD_CCtx_setFParams(ZSTD_CCtx* cctx, ZSTD_frameParameters
/// fparams)` — set the content-size, checksum and dictionary-ID flags.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_setFParams(
    cctx: *mut ZSTD_CCtx,
    fparams: ZSTD_frameParameters,
) -> usize {
    for (param, value) in [
        (ZSTD_C_CONTENT_SIZE_FLAG, fparams.contentSizeFlag != 0),
        (ZSTD_C_CHECKSUM_FLAG, fparams.checksumFlag != 0),
        (ZSTD_C_DICT_ID_FLAG, fparams.noDictIDFlag == 0),
    ] {
        let rc = unsafe { ZSTD_CCtx_setParameter(cctx, param, c_int::from(value)) };
        if crate::error::result_is_error(rc) {
            return rc;
        }
    }
    0
}

/// `size_t ZSTD_CCtx_setParams(ZSTD_CCtx* cctx, ZSTD_parameters params)` —
/// [`ZSTD_CCtx_setFParams`] and [`ZSTD_CCtx_setCParams`] together, applying
/// neither when the compression parameters fail the check.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_setParams(
    cctx: *mut ZSTD_CCtx,
    params: ZSTD_parameters,
) -> usize {
    // The compression parameters first, so a bad set leaves everything as it
    // was; the frame flags next, which fail only on a context mid-frame.
    let checked = ZSTD_checkCParams(params.cParams);
    if crate::error::result_is_error(checked) {
        return checked;
    }
    let rc = unsafe { ZSTD_CCtx_setFParams(cctx, params.fParams) };
    if crate::error::result_is_error(rc) {
        return rc;
    }
    unsafe { ZSTD_CCtx_setCParams(cctx, params.cParams) }
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
    if !dctx.stream_frame_done {
        // Decoder parameters may only change between frames (upstream
        // stage contract); mutating the window ceiling mid-frame would
        // change acceptance rules under the active decode.
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    match param {
        ZSTD_D_WINDOW_LOG_MAX => {
            if value != 0 && !(D_WINDOW_LOG_MIN..=D_WINDOW_LOG_MAX).contains(&value) {
                return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
            }
            dctx.max_window_size = if value == 0 {
                MAX_WINDOW_SIZE_DEFAULT
            } else {
                1 << value
            };
            0
        }
        ZSTD_D_FORMAT => {
            let magicless = match value {
                ZSTD_F_ZSTD1 => false,
                ZSTD_F_ZSTD1_MAGICLESS => true,
                _ => return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound),
            };
            dctx.magicless = magicless;
            dctx.decoder.set_magicless(magicless);
            0
        }
        ZSTD_D_STABLE_OUT_BUFFER
        | ZSTD_D_FORCE_IGNORE_CHECKSUM
        | ZSTD_D_REF_MULTIPLE_DDICTS
        | ZSTD_D_DISABLE_HUFFMAN_ASSEMBLY => {
            let on = match value {
                0 => false,
                1 => true,
                _ => return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound),
            };
            match param {
                ZSTD_D_STABLE_OUT_BUFFER => dctx.stable_out_buffer = on,
                ZSTD_D_FORCE_IGNORE_CHECKSUM => {
                    dctx.content_checksum = if on {
                        ContentChecksum::None
                    } else {
                        ContentChecksum::Verify
                    };
                }
                ZSTD_D_REF_MULTIPLE_DDICTS => dctx.ref_multiple_ddicts = on,
                _ => dctx.disable_huffman_assembly = on,
            }
            0
        }
        ZSTD_D_MAX_BLOCK_SIZE => {
            if value != 0 && !(MAX_BLOCK_SIZE_MIN..=TARGET_CBLOCK_SIZE_MAX).contains(&value) {
                return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
            }
            dctx.max_block_size = value;
            dctx.apply_max_block_size();
            0
        }
        _ => encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    }
}

/// `size_t ZSTD_DCtx_setFormat(ZSTD_DCtx* dctx, ZSTD_format_e format)` —
/// [`ZSTD_DCtx_setParameter`] with `ZSTD_d_format`: whether frames carry the
/// magic number (`ZSTD_f_zstd1`) or start at the frame header
/// (`ZSTD_f_zstd1_magicless`).
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_setFormat(dctx: *mut ZSTD_DCtx, format: c_int) -> usize {
    unsafe { ZSTD_DCtx_setParameter(dctx, ZSTD_D_FORMAT, format) }
}

/// `size_t ZSTD_DCtx_setMaxWindowSize(ZSTD_DCtx* dctx, size_t maxWindowSize)` —
/// the largest window a streamed frame may declare, in bytes rather than the
/// power of two `ZSTD_d_windowLogMax` takes.
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_setMaxWindowSize(
    dctx: *mut ZSTD_DCtx,
    max_window_size: usize,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    if !dctx.stream_frame_done {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    let size = max_window_size as u64;
    if !(1 << D_WINDOW_LOG_MIN..=1 << D_WINDOW_LOG_MAX).contains(&size) {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    dctx.max_window_size = size;
    0
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
    let dctx = unsafe { &*dctx };
    let out = match param {
        // The power of two at or below the ceiling, which is the ceiling
        // itself unless `ZSTD_DCtx_setMaxWindowSize` set one between powers.
        ZSTD_D_WINDOW_LOG_MAX => dctx.max_window_size.ilog2() as c_int,
        ZSTD_D_FORMAT => {
            if dctx.magicless {
                ZSTD_F_ZSTD1_MAGICLESS
            } else {
                ZSTD_F_ZSTD1
            }
        }
        ZSTD_D_STABLE_OUT_BUFFER => c_int::from(dctx.stable_out_buffer),
        ZSTD_D_FORCE_IGNORE_CHECKSUM => c_int::from(dctx.content_checksum == ContentChecksum::None),
        ZSTD_D_REF_MULTIPLE_DDICTS => c_int::from(dctx.ref_multiple_ddicts),
        ZSTD_D_DISABLE_HUFFMAN_ASSEMBLY => c_int::from(dctx.disable_huffman_assembly),
        ZSTD_D_MAX_BLOCK_SIZE => dctx.max_block_size,
        _ => return encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
    };
    unsafe { value.write(out) };
    0
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
            // Parameters can only be reset between frames (upstream stage
            // contract); a session reset must accompany the request to
            // abandon an in-flight frame.
            if !dctx.stream_frame_done {
                return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
            }
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
