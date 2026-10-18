//! `ZSTD_CCtx_params`: a compression parameter set as an object of its own,
//! and the storage rules every compression parameter setter shares, the
//! context's `ZSTD_CCtx_setParameter` included.

use core::ffi::c_int;

use codec::encoding::{CompressionLevel, CompressionParameters, LiteralCompressionMode, Strategy};

use crate::cdict::ZSTD_compressionParameters;
use crate::context::{ZSTD_CCtx, free_boxed, try_box};
use crate::error::{ZSTD_ErrorCode, encode};
use crate::params::*;

/// `ZSTD_CLEVEL_DEFAULT`, the level a `0` selects.
const CLEVEL_DEFAULT: c_int = 3;

/// Upstream `ZSTD_ParamSwitch_e`: leave it to the library, force on, force off.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ParamSwitch {
    #[default]
    Auto,
    Enable,
    Disable,
}

impl ParamSwitch {
    fn from_c(value: c_int) -> Option<Self> {
        Some(match value {
            0 => Self::Auto,
            1 => Self::Enable,
            2 => Self::Disable,
            _ => return None,
        })
    }

    fn to_c(self) -> c_int {
        match self {
            Self::Auto => 0,
            Self::Enable => 1,
            Self::Disable => 2,
        }
    }
}

/// The experimental parameters that steer how upstream's own implementation
/// searches, schedules or checks its work, with no counterpart in this
/// encoder: stored as set so they read back, without effect on the frames.
/// The sequence-format ones are read by the sequence entry points.
const ADVISORY: [c_int; 14] = [
    ZSTD_C_FORCE_MAX_WINDOW,
    ZSTD_C_FORCE_ATTACH_DICT,
    ZSTD_C_ENABLE_DEDICATED_DICT_SEARCH,
    ZSTD_C_STABLE_IN_BUFFER,
    ZSTD_C_STABLE_OUT_BUFFER,
    ZSTD_C_BLOCK_DELIMITERS,
    ZSTD_C_VALIDATE_SEQUENCES,
    ZSTD_C_SPLIT_AFTER_SEQUENCES,
    ZSTD_C_USE_ROW_MATCH_FINDER,
    ZSTD_C_DETERMINISTIC_REF_PREFIX,
    ZSTD_C_PREFETCH_CDICT_TABLES,
    ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK,
    ZSTD_C_REPCODE_RESOLUTION,
    ZSTD_C_BLOCK_SPLITTER_LEVEL,
];

fn advisory_slot(param: c_int) -> Option<usize> {
    ADVISORY.iter().position(|&p| p == param)
}

/// A compression parameter set (upstream `ZSTD_CCtx_params`): the sticky
/// parameters of a context, or a free-standing set applied to one later.
/// `None` knobs mean "auto" (value 0 in the C API): the level's resolved
/// defaults apply.
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
    /// `ZSTD_c_enableLongDistanceMatching`; only `Enable` turns it on.
    pub(crate) ldm: ParamSwitch,
    pub(crate) ldm_hash_log: Option<u32>,
    pub(crate) ldm_min_match: Option<u32>,
    pub(crate) ldm_bucket_size_log: Option<u32>,
    pub(crate) ldm_hash_rate_log: Option<u32>,
    pub(crate) content_size_flag: bool,
    pub(crate) checksum_flag: bool,
    pub(crate) dict_id_flag: bool,
    /// Applied at frame start: the encoder caps every physical block's
    /// payload at the target (raw length <= target bounds the compressed
    /// length too), so blocks land at or under `target + 3` header bytes —
    /// a strict form of upstream's best-effort convergence target.
    pub(crate) target_cblock_size: c_int,
    /// `ZSTD_c_maxBlockSize`; 0 = the format's 128 KiB.
    pub(crate) max_block_size: c_int,
    /// `ZSTD_c_format`: frames without the magic number.
    pub(crate) magicless: bool,
    /// `ZSTD_c_literalCompressionMode`.
    pub(crate) literal_compression: LiteralCompressionMode,
    /// `ZSTD_c_srcSizeHint`; 0 = none.
    pub(crate) src_size_hint: c_int,
    /// The [`ADVISORY`] parameters, in that order.
    pub(crate) advisory: [c_int; ADVISORY.len()],
    /// Pledged size of the next frame (`ZSTD_CCtx_setPledgedSrcSize`).
    /// Consumed by the next frame start, then reset to unknown. Belongs to
    /// the context's session, not to the parameter set.
    pub(crate) pledged_src_size: u64,
    /// `ZSTD_registerSequenceProducer`: asked for each block's parse.
    pub(crate) producer: Option<crate::sequences::CSequenceProducer>,
}

impl Default for CCtxParams {
    fn default() -> Self {
        Self::with_level(CLEVEL_DEFAULT)
    }
}

impl CCtxParams {
    /// Defaults at `level` (`ZSTD_CCtxParams_init`), which is stored as given.
    pub(crate) fn with_level(level: c_int) -> Self {
        Self {
            level,
            window_log: None,
            hash_log: None,
            chain_log: None,
            search_log: None,
            min_match: None,
            target_length: None,
            strategy: None,
            ldm: ParamSwitch::Auto,
            ldm_hash_log: None,
            ldm_min_match: None,
            ldm_bucket_size_log: None,
            ldm_hash_rate_log: None,
            content_size_flag: true,
            checksum_flag: false,
            dict_id_flag: true,
            target_cblock_size: 0,
            max_block_size: 0,
            magicless: false,
            literal_compression: LiteralCompressionMode::Auto,
            src_size_hint: 0,
            advisory: [0; ADVISORY.len()],
            pledged_src_size: CONTENTSIZE_UNKNOWN,
            producer: None,
        }
    }

    /// The registered producer, boxed for the encoder and told the frame
    /// runs at `level`, and whether a block it fails on falls back to the
    /// built-in matchers (`ZSTD_c_enableSeqProducerFallback`).
    pub(crate) fn sequence_producer(
        &self,
        level: c_int,
    ) -> (
        Option<Box<dyn codec::encoding::sequences::SequenceProducer>>,
        bool,
    ) {
        let fallback = self
            .get(ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK)
            .is_ok_and(|v| v != 0);
        let producer = self.producer.map(|p| {
            Box::new(crate::sequences::CProducerCall { producer: p, level })
                as Box<dyn codec::encoding::sequences::SequenceProducer>
        });
        (producer, fallback)
    }

    /// Resolve the knob set into the codec's validated
    /// [`CompressionParameters`]. `None` on an internally-inconsistent set
    /// (should be unreachable: every knob was bounds-checked on entry).
    pub(crate) fn resolve(&self) -> Option<CompressionParameters> {
        let mut builder = CompressionParameters::builder(CompressionLevel::from_level(self.level))
            .literal_compression(self.literal_compression);
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
        if self.ldm == ParamSwitch::Enable {
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

    /// The target block size the encoder is given, if one is set.
    pub(crate) fn target_block_size(&self) -> Option<u32> {
        (self.target_cblock_size > 0).then_some(self.target_cblock_size as u32)
    }

    /// The maximum block size the encoder is given, if one is set.
    pub(crate) fn max_block_size(&self) -> Option<u32> {
        (self.max_block_size > 0).then_some(self.max_block_size as u32)
    }

    /// Store one parameter, the way upstream `ZSTD_CCtxParams_setParameter`
    /// does: `0` is "auto" for the tunables, the level is clamped into range,
    /// the on/off flags take any non-zero value as on, and everything else
    /// outside its bounds is refused.
    pub(crate) fn set(&mut self, param: c_int, value: c_int) -> Result<(), ZSTD_ErrorCode> {
        let Some((lower, upper)) = c_param_bounds(param) else {
            return Err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported);
        };
        let within = |v: c_int| {
            if (lower..=upper).contains(&v) {
                Ok(v)
            } else {
                Err(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound)
            }
        };
        // A tunable at 0 goes back to auto; anything else must be in range.
        let tunable = |v: c_int| -> Result<Option<u32>, ZSTD_ErrorCode> {
            if v == 0 {
                Ok(None)
            } else {
                within(v).map(|v| Some(v as u32))
            }
        };
        match param {
            ZSTD_C_COMPRESSION_LEVEL => {
                let level = value.clamp(lower, upper);
                self.level = if level == 0 { CLEVEL_DEFAULT } else { level };
            }
            ZSTD_C_WINDOW_LOG => self.window_log = tunable(value)?,
            ZSTD_C_HASH_LOG => self.hash_log = tunable(value)?,
            ZSTD_C_CHAIN_LOG => self.chain_log = tunable(value)?,
            ZSTD_C_SEARCH_LOG => self.search_log = tunable(value)?,
            ZSTD_C_MIN_MATCH => self.min_match = tunable(value)?,
            // 0 is in range here, and means "the level's own" like the rest.
            ZSTD_C_TARGET_LENGTH => self.target_length = tunable(within(value)?)?,
            ZSTD_C_STRATEGY => {
                // Bounds guarantee 1..=9, which `from_ordinal` covers.
                self.strategy = tunable(value)?.and_then(Strategy::from_ordinal);
            }
            ZSTD_C_ENABLE_LDM => {
                self.ldm = ParamSwitch::from_c(within(value)?)
                    .ok_or(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound)?;
            }
            ZSTD_C_LDM_HASH_LOG => self.ldm_hash_log = tunable(value)?,
            ZSTD_C_LDM_MIN_MATCH => self.ldm_min_match = tunable(value)?,
            ZSTD_C_LDM_BUCKET_SIZE_LOG => self.ldm_bucket_size_log = tunable(value)?,
            ZSTD_C_LDM_HASH_RATE_LOG => self.ldm_hash_rate_log = tunable(value)?,
            // Any non-zero value is on, read back as 1: upstream coerces these
            // (zstd_compress.c:835-846) and callers pass truthy values to it.
            ZSTD_C_CONTENT_SIZE_FLAG => self.content_size_flag = value != 0,
            ZSTD_C_CHECKSUM_FLAG => self.checksum_flag = value != 0,
            ZSTD_C_DICT_ID_FLAG => self.dict_id_flag = value != 0,
            // A target under the minimum is raised to it, as upstream does.
            ZSTD_C_TARGET_CBLOCK_SIZE => {
                self.target_cblock_size = if value == 0 {
                    0
                } else {
                    within(value.max(lower))?
                };
            }
            ZSTD_C_MAX_BLOCK_SIZE => {
                self.max_block_size = tunable(value)?.map_or(0, |v| v as c_int);
            }
            ZSTD_C_SRC_SIZE_HINT => self.src_size_hint = within(value)?,
            ZSTD_C_FORMAT => self.magicless = within(value)? == ZSTD_F_ZSTD1_MAGICLESS,
            ZSTD_C_LITERAL_COMPRESSION_MODE => {
                self.literal_compression = match ParamSwitch::from_c(within(value)?) {
                    Some(ParamSwitch::Enable) => LiteralCompressionMode::Enable,
                    Some(ParamSwitch::Disable) => LiteralCompressionMode::Disable,
                    _ => LiteralCompressionMode::Auto,
                };
            }
            // Multi-threading is not part of this build: only 0 is a value,
            // as in upstream's single-threaded one.
            ZSTD_C_NB_WORKERS | ZSTD_C_JOB_SIZE | ZSTD_C_OVERLAP_LOG | ZSTD_C_RSYNCABLE => {
                if value != 0 {
                    return Err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported);
                }
            }
            // Upstream takes any value for these two and keeps whether it is 0.
            ZSTD_C_FORCE_MAX_WINDOW | ZSTD_C_ENABLE_DEDICATED_DICT_SEARCH => {
                self.store_advisory(param, c_int::from(value != 0));
            }
            ZSTD_C_DETERMINISTIC_REF_PREFIX => {
                let value = c_int::from(within(value)? != 0);
                self.store_advisory(param, value);
            }
            _ => {
                let value = within(value)?;
                self.store_advisory(param, value);
            }
        }
        Ok(())
    }

    fn store_advisory(&mut self, param: c_int, value: c_int) {
        let slot = advisory_slot(param).expect("every bounded parameter is handled");
        self.advisory[slot] = value;
    }

    /// Read one parameter back as [`Self::set`] stored it (`0` = auto).
    pub(crate) fn get(&self, param: c_int) -> Result<c_int, ZSTD_ErrorCode> {
        let opt = |v: Option<u32>| v.map_or(0, |x| x as c_int);
        Ok(match param {
            ZSTD_C_COMPRESSION_LEVEL => self.level,
            ZSTD_C_WINDOW_LOG => opt(self.window_log),
            ZSTD_C_HASH_LOG => opt(self.hash_log),
            ZSTD_C_CHAIN_LOG => opt(self.chain_log),
            ZSTD_C_SEARCH_LOG => opt(self.search_log),
            ZSTD_C_MIN_MATCH => opt(self.min_match),
            ZSTD_C_TARGET_LENGTH => opt(self.target_length),
            ZSTD_C_STRATEGY => self.strategy.map_or(0, |s| s.ordinal() as c_int),
            ZSTD_C_ENABLE_LDM => self.ldm.to_c(),
            ZSTD_C_LDM_HASH_LOG => opt(self.ldm_hash_log),
            ZSTD_C_LDM_MIN_MATCH => opt(self.ldm_min_match),
            ZSTD_C_LDM_BUCKET_SIZE_LOG => opt(self.ldm_bucket_size_log),
            ZSTD_C_LDM_HASH_RATE_LOG => opt(self.ldm_hash_rate_log),
            ZSTD_C_CONTENT_SIZE_FLAG => c_int::from(self.content_size_flag),
            ZSTD_C_CHECKSUM_FLAG => c_int::from(self.checksum_flag),
            ZSTD_C_DICT_ID_FLAG => c_int::from(self.dict_id_flag),
            ZSTD_C_TARGET_CBLOCK_SIZE => self.target_cblock_size,
            ZSTD_C_MAX_BLOCK_SIZE => self.max_block_size,
            ZSTD_C_SRC_SIZE_HINT => self.src_size_hint,
            ZSTD_C_FORMAT => {
                if self.magicless {
                    ZSTD_F_ZSTD1_MAGICLESS
                } else {
                    ZSTD_F_ZSTD1
                }
            }
            ZSTD_C_LITERAL_COMPRESSION_MODE => match self.literal_compression {
                LiteralCompressionMode::Auto => ParamSwitch::Auto.to_c(),
                LiteralCompressionMode::Enable => ParamSwitch::Enable.to_c(),
                LiteralCompressionMode::Disable => ParamSwitch::Disable.to_c(),
            },
            ZSTD_C_NB_WORKERS => 0,
            // Upstream's single-threaded build has no value to report for these.
            ZSTD_C_JOB_SIZE | ZSTD_C_OVERLAP_LOG | ZSTD_C_RSYNCABLE => {
                return Err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported);
            }
            _ => match advisory_slot(param) {
                Some(slot) => self.advisory[slot],
                None => return Err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported),
            },
        })
    }

    /// Put explicit compression and frame parameters in place, every knob
    /// set, the others left as they are (upstream `ZSTD_CCtxParams_setZstdParams`):
    /// the level, recorded as 0, no longer chooses any.
    pub(crate) fn set_explicit(
        &mut self,
        cparams: &ZSTD_compressionParameters,
        fparams: &crate::cdict::ZSTD_frameParameters,
    ) {
        let opt = |v: u32| (v != 0).then_some(v);
        self.level = 0;
        self.window_log = opt(cparams.windowLog);
        self.chain_log = opt(cparams.chainLog);
        self.hash_log = opt(cparams.hashLog);
        self.search_log = opt(cparams.searchLog);
        self.min_match = opt(cparams.minMatch);
        self.target_length = opt(cparams.targetLength);
        self.strategy = Strategy::from_ordinal(cparams.strategy);
        self.content_size_flag = fparams.contentSizeFlag != 0;
        self.checksum_flag = fparams.checksumFlag != 0;
        self.dict_id_flag = fparams.noDictIDFlag == 0;
    }
}

/// Opaque `ZSTD_CCtx_params`: a parameter set built on its own and applied to
/// contexts with [`ZSTD_CCtx_setParametersUsingCCtxParams`].
#[allow(non_camel_case_types)]
pub struct ZSTD_CCtx_params {
    pub(crate) params: CCtxParams,
}

/// `ZSTD_CCtx_params* ZSTD_createCCtxParams(void)` — a parameter set at the
/// defaults, or `NULL` when it cannot be allocated.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCCtxParams() -> *mut ZSTD_CCtx_params {
    try_box(ZSTD_CCtx_params {
        params: CCtxParams::default(),
    })
}

/// `size_t ZSTD_freeCCtxParams(ZSTD_CCtx_params* params)` — `NULL` is a no-op.
///
/// # Safety
/// `params` must come from [`ZSTD_createCCtxParams`] and not be freed yet, or
/// be `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeCCtxParams(params: *mut ZSTD_CCtx_params) -> usize {
    unsafe { free_boxed(params) };
    0
}

/// `size_t ZSTD_CCtxParams_reset(ZSTD_CCtx_params* params)` — back to the
/// defaults.
///
/// # Safety
/// `params` must be a live parameter set, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtxParams_reset(params: *mut ZSTD_CCtx_params) -> usize {
    unsafe { ZSTD_CCtxParams_init(params, CLEVEL_DEFAULT) }
}

/// `size_t ZSTD_CCtxParams_init(ZSTD_CCtx_params* cctxParams, int
/// compressionLevel)` — the defaults, at `compressionLevel`.
///
/// # Safety
/// `cctx_params` must be a live parameter set, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtxParams_init(
    cctx_params: *mut ZSTD_CCtx_params,
    compression_level: c_int,
) -> usize {
    let Some(target) = (unsafe { cctx_params.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    target.params = CCtxParams::with_level(compression_level);
    0
}

/// `size_t ZSTD_CCtxParams_init_advanced(ZSTD_CCtx_params* cctxParams,
/// ZSTD_parameters params)` — the defaults with every compression and frame
/// parameter taken from `params`, which must pass `ZSTD_checkCParams`.
///
/// # Safety
/// `cctx_params` must be a live parameter set, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtxParams_init_advanced(
    cctx_params: *mut ZSTD_CCtx_params,
    params: ZSTD_parameters,
) -> usize {
    let Some(target) = (unsafe { cctx_params.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let checked = ZSTD_checkCParams(params.cParams);
    if crate::error::result_is_error(checked) {
        return checked;
    }
    // Upstream records `ZSTD_NO_CLEVEL`: the parameters are all explicit.
    let mut fresh = CCtxParams::with_level(0);
    fresh.set_explicit(&params.cParams, &params.fParams);
    target.params = fresh;
    0
}

/// `size_t ZSTD_CCtxParams_setParameter(ZSTD_CCtx_params* params,
/// ZSTD_cParameter param, int value)` — as `ZSTD_CCtx_setParameter`, on the
/// free-standing set.
///
/// # Safety
/// `params` must be a live parameter set, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtxParams_setParameter(
    params: *mut ZSTD_CCtx_params,
    param: c_int,
    value: c_int,
) -> usize {
    let Some(target) = (unsafe { params.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    match target.params.set(param, value) {
        Ok(()) => 0,
        Err(code) => encode(code),
    }
}

/// `size_t ZSTD_CCtxParams_getParameter(const ZSTD_CCtx_params* params,
/// ZSTD_cParameter param, int* value)`.
///
/// # Safety
/// `params` must be a live parameter set or `NULL`; `value` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtxParams_getParameter(
    params: *const ZSTD_CCtx_params,
    param: c_int,
    value: *mut c_int,
) -> usize {
    let Some(source) = (unsafe { params.as_ref() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    if value.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    match source.params.get(param) {
        Ok(out) => {
            unsafe { value.write(out) };
            0
        }
        Err(code) => encode(code),
    }
}

/// `size_t ZSTD_CCtx_setParametersUsingCCtxParams(ZSTD_CCtx* cctx, const
/// ZSTD_CCtx_params* params)` — replace the context's parameters with
/// `params` from the next frame on (a frame under way keeps its own), and not
/// while a `CDict` is referenced, whose parameters the frame inherits.
///
/// # Safety
/// `cctx` and `params` must be live, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_setParametersUsingCCtxParams(
    cctx: *mut ZSTD_CCtx,
    params: *const ZSTD_CCtx_params,
) -> usize {
    let (Some(cctx), Some(source)) = (unsafe { cctx.as_mut() }, unsafe { params.as_ref() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    // A set applied while a frame streams waits for the next frame, as zstd.h
    // promises for a single-threaded context: the open frame was configured
    // when it began and reads none of these again. Upstream's own build
    // refuses it (zstd_compress.c:1182); nothing depends on that refusal.
    if cctx.references_cdict() {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    // The pledge belongs to the session, which the parameters do not replace.
    let pledged = cctx.params.pledged_src_size;
    cctx.params = source.params;
    cctx.params.pledged_src_size = pledged;
    0
}

/// `size_t ZSTD_compress_advanced(ZSTD_CCtx* cctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize, const void* dict, size_t
/// dictSize, ZSTD_parameters params)` — one-shot compression under explicit
/// compression and frame parameters, with optional one-off dictionary bytes.
/// The context's sticky parameters are not read.
///
/// # Safety
/// `cctx` must be live or `NULL`; `dst` / `src` / `dict` valid for their
/// lengths (or `NULL` + 0 each).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compress_advanced(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    dict: *const u8,
    dict_size: usize,
    params: ZSTD_parameters,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let checked = ZSTD_checkCParams(params.cParams);
    if crate::error::result_is_error(checked) {
        return checked;
    }
    let mut explicit = CCtxParams::with_level(0);
    explicit.set_explicit(&params.cParams, &params.fParams);
    let Some(resolved) = explicit.resolve() else {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_combination_unsupported);
    };
    let frame = crate::attach::OneShotFrame {
        level: explicit.level,
        params: Some(resolved),
        checksum: explicit.checksum_flag,
        content_size: explicit.content_size_flag,
        dict_id: explicit.dict_id_flag,
    };
    let src = unsafe { crate::ffi::in_slice(src, src_size) };
    let dict = unsafe { crate::ffi::in_slice(dict, dict_size) };
    let dst = unsafe { crate::ffi::out_slice(dst, dst_capacity) };
    crate::attach::compress_with_dict_bytes(cctx, dst, src, dict, &frame)
}

/// `ZSTD_CDict* ZSTD_createCDict_advanced2(const void* dict, size_t dictSize,
/// ZSTD_dictLoadMethod_e dictLoadMethod, ZSTD_dictContentType_e
/// dictContentType, const ZSTD_CCtx_params* cctxParams, ZSTD_customMem
/// customMem)` — a CDict that compresses under the parameters of
/// `cctxParams`. The bytes are always copied; custom allocators are not
/// supported (`customMem` must be all-`NULL`).
///
/// # Safety
/// `dict` must be valid for `dict_size` bytes; `cctx_params` live or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createCDict_advanced2(
    dict: *const u8,
    dict_size: usize,
    _dict_load_method: c_int,
    dict_content_type: c_int,
    cctx_params: *const ZSTD_CCtx_params,
    custom_mem: crate::cdict::ZSTD_customMem,
) -> *mut crate::cdict::ZSTD_CDict {
    let Some(source) = (unsafe { cctx_params.as_ref() }) else {
        return core::ptr::null_mut();
    };
    let dict = unsafe { crate::ffi::in_slice(dict, dict_size) };
    crate::cdict::create_cdict_with_params(dict, dict_content_type, &source.params, &custom_mem)
}

/// The workspace `params` asks for, before the context's own struct.
fn workspace_for(params: &CCtxParams) -> Result<usize, ZSTD_ErrorCode> {
    let resolved = params
        .resolve()
        .ok_or(ZSTD_ErrorCode::ZSTD_error_parameter_combination_unsupported)?;
    let hint = (params.src_size_hint > 0).then_some(params.src_size_hint as u64);
    let workspace = codec::encoding::estimated_compression_workspace_bytes_for_parameters(
        &resolved, hint, None,
    );
    // A registered producer's parse is kept from block to block, sized for the
    // largest block the set allows.
    let parse = if params.producer.is_some() {
        let block = params
            .max_block_size()
            .map_or(codec::MAX_BLOCK_SIZE as usize, |size| size as usize);
        codec::encoding::sequences::producer_parse_heap_bytes(block)
    } else {
        0
    };
    workspace
        .checked_add(parse)
        .ok_or(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound)
}

/// `size_t ZSTD_estimateCCtxSize_usingCCtxParams(const ZSTD_CCtx_params*
/// params)` — budget for a one-shot context compressing under `params`.
///
/// # Safety
/// `params` must be a live parameter set, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_estimateCCtxSize_usingCCtxParams(
    params: *const ZSTD_CCtx_params,
) -> usize {
    let Some(source) = (unsafe { params.as_ref() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    // A sum past `usize` would read as an error code if saturated and as an
    // under-report if wrapped, so it is refused as the out-of-range set it is.
    match workspace_for(&source.params) {
        Ok(workspace) => (core::mem::size_of::<ZSTD_CCtx>() + crate::context::RETAINED_SCRATCH)
            .checked_add(workspace)
            .unwrap_or_else(|| encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound)),
        Err(code) => encode(code),
    }
}

/// `size_t ZSTD_estimateCStreamSize_usingCCtxParams(const ZSTD_CCtx_params*
/// params)` — [`ZSTD_estimateCCtxSize_usingCCtxParams`] plus the streaming
/// input and output staging.
///
/// # Safety
/// `params` must be a live parameter set, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_estimateCStreamSize_usingCCtxParams(
    params: *const ZSTD_CCtx_params,
) -> usize {
    let base = unsafe { ZSTD_estimateCCtxSize_usingCCtxParams(params) };
    if crate::error::result_is_error(base) {
        return base;
    }
    base.checked_add(2 * codec::MAX_BLOCK_SIZE as usize)
        .unwrap_or_else(|| encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound))
}
