//! Compression / decompression dictionary objects: `ZSTD_CDict` / `ZSTD_DDict`
//! and the one-shot `*_usingCDict` / `*_usingDDict` entry points.
//!
//! These mirror upstream's prepared-dictionary model: a dictionary is parsed
//! once into a `CDict` (encoder side) or `DDict` (decoder side) and then used
//! across many compress / decompress calls. The encoder-side reuse (parsed
//! dictionary + primed match-finder snapshot) is realised by caching a
//! [`FrameCompressor`](codec::encoding::FrameCompressor) on the `ZSTD_CCtx`, keyed by the `CDict`'s never-reused
//! serial; the decoder caches the loaded dictionary on the `ZSTD_DCtx` keyed by
//! the `DDict`'s serial (a raw-address key would be ABA-unsafe across free +
//! realloc). The match-finder tables a dictionary primes are sized to the
//! dictionary's own cParams tier (see the codec's `set_dictionary_size_hint`),
//! so a `CDict` does not drag a source-window-sized table around.

use core::ffi::{c_int, c_uint};
use core::sync::atomic::{AtomicU64, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::ContentChecksum;
use codec::encoding::{CompressionParameters, EncoderDictionary};

use crate::context::{
    ZSTD_CCtx, ZSTD_DCtx, free_boxed, kept_compressor, set_frame_parameters, try_box,
};
use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::ffi::{in_slice, out_slice};

/// Monotonic source for [`ZSTD_CDict`] / [`ZSTD_DDict`] identities. A context
/// caches its prepared compressor / loaded dictionary keyed by this serial, not
/// by the handle's raw address: an address can be recycled when a dictionary is
/// freed and a new one allocated in the same slot, so a pointer-keyed cache
/// would silently keep using the old prepared state for the new handle (ABA).
/// A serial is assigned once at creation and never reused, so identity stays
/// stable across free + realloc. `0` is reserved for "no dictionary attached".
static DICT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// Assign the next never-reused dictionary identity. See [`DICT_SERIAL`].
pub(crate) fn next_dict_serial() -> u64 {
    DICT_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// Parse the dictionary ID from a serialized dictionary header, or `0` for
/// raw content. Matches `ZSTD_getDictID_fromDict` semantics.
pub(crate) fn dict_id_from_bytes(dict: &[u8]) -> u32 {
    if !codec::decoding::Dictionary::is_serialized(dict) {
        return 0;
    }
    u32::from_le_bytes([dict[4], dict[5], dict[6], dict[7]])
}

/// Opaque prepared compression dictionary: the dictionary parsed and its
/// entropy tables built once, at creation, plus the compression level baked
/// in there (upstream `ZSTD_createCDict` takes the level, and
/// `ZSTD_compress_usingCDict` uses it). Contexts attach it by reference, so
/// many can use one `CDict` concurrently without preparing it again.
#[allow(non_camel_case_types)]
pub struct ZSTD_CDict {
    /// The prepared dictionary; `None` for one made from no bytes, which
    /// carries its parameters and no content, as upstream's does.
    pub(crate) dict: Option<EncoderDictionary>,
    pub(crate) id: u32,
    pub(crate) level: c_int,
    /// Never-reused identity for context cache validation (see [`DICT_SERIAL`]).
    pub(crate) serial: u64,
    /// Raw-content dictionary (no `ZSTD_MAGIC_DICTIONARY`): modelled with a
    /// synthetic ID that is suppressed on the wire.
    pub(crate) raw_content: bool,
    /// Explicit compression parameters (`ZSTD_createCDict_advanced`),
    /// resolved at creation; they win over a referencing context's sticky
    /// knobs. `None` compresses at `level`.
    pub(crate) params: Option<CompressionParameters>,
}

/// Opaque prepared decompression dictionary: parsed once at creation under the
/// content type it was made with, as upstream loads a DDict's entropy then,
/// and shared by every context that uses it, as upstream contexts point at it.
#[allow(non_camel_case_types)]
pub struct ZSTD_DDict {
    /// `None` for one made from no bytes, a no-dictionary handle.
    pub(crate) prepared: Option<codec::decoding::DictionaryHandle>,
    pub(crate) id: u32,
}

/// A DDict of `dict` read as `content_type`, `NULL` where upstream's
/// `ZSTD_createDDict_advanced` fails: a selector it does not know, fullDict
/// over bytes that are not a dictionary (nothing included), or a serialized
/// dictionary whose tables do not parse (`ZSTD_loadEntropy_intoDDict`).
fn create_ddict(dict: &[u8], content_type: c_int) -> *mut ZSTD_DDict {
    let prepared = if dict.is_empty() {
        if content_type != crate::attach::ZSTD_DCT_AUTO
            && content_type != crate::attach::ZSTD_DCT_RAW_CONTENT
        {
            return core::ptr::null_mut();
        }
        None
    } else {
        let parsed = catch_unwind(AssertUnwindSafe(|| {
            crate::attach::parse_decode_dict(dict, content_type)
        }));
        match parsed {
            Ok(Ok(handle)) => Some(handle),
            Ok(Err(_)) | Err(_) => return core::ptr::null_mut(),
        }
    };
    let id = if content_type == crate::attach::ZSTD_DCT_RAW_CONTENT {
        0
    } else {
        dict_id_from_bytes(dict)
    };
    try_box(ZSTD_DDict { prepared, id })
}

/// Prepare `dict` for compression the way a `CDict` holds it: a serialized
/// dictionary is parsed, raw content is modelled with the synthetic
/// suppressed ID (see [`crate::attach`]). `None` when it cannot be prepared.
pub(crate) fn prepare_compression_dictionary(
    dict: &[u8],
    raw_content: bool,
) -> Option<EncoderDictionary> {
    let prepared = catch_unwind(AssertUnwindSafe(|| {
        if raw_content {
            codec::decoding::Dictionary::from_raw_content(u32::MAX, dict.to_vec())
                .ok()
                .map(EncoderDictionary::from_dictionary)
        } else {
            EncoderDictionary::from_bytes(dict).ok()
        }
    }));
    prepared.ok().flatten()
}

/// What a CDict holds of `dict`: nothing for no bytes, which upstream takes as
/// a CDict of parameters alone (zstd_compress.c:5562-5598), else the prepared
/// dictionary. `Err` when the bytes cannot be prepared.
fn prepare_cdict_content(dict: &[u8], raw_content: bool) -> Result<Option<EncoderDictionary>, ()> {
    if dict.is_empty() {
        return Ok(None);
    }
    prepare_compression_dictionary(dict, raw_content)
        .map(Some)
        .ok_or(())
}

/// `ZSTD_CDict* ZSTD_createCDict(const void* dictBuffer, size_t dictSize, int
/// compressionLevel)` — parse and validate a dictionary for repeated
/// compression at `compressionLevel`. Returns `NULL` on allocation failure or
/// if the dictionary cannot be parsed for encoding, matching upstream's
/// NULL-on-failure contract.
///
/// # Safety
/// `dictBuffer` must be valid for `dictSize` bytes (or `NULL` with `dictSize == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createCDict(
    dict_buffer: *const u8,
    dict_size: usize,
    compression_level: c_int,
) -> *mut ZSTD_CDict {
    let dict = unsafe { in_slice(dict_buffer, dict_size) };
    // Auto content type (upstream createCDict): a serialized dictionary must
    // parse for encoding, anything else is a raw-content dictionary.
    let raw_content = !codec::decoding::Dictionary::is_serialized(dict);
    let Ok(prepared) = prepare_cdict_content(dict, raw_content) else {
        return core::ptr::null_mut();
    };
    try_box(ZSTD_CDict {
        dict: prepared,
        id: dict_id_from_bytes(dict),
        level: compression_level,
        serial: next_dict_serial(),
        raw_content,
        params: None,
    })
}

/// `ZSTD_CDict* ZSTD_createCDict_byReference(const void* dictBuffer, size_t
/// dictSize, int compressionLevel)` (static API). We always copy the dictionary
/// bytes into the `CDict`, so by-reference creation is identical to
/// [`ZSTD_createCDict`] from the caller's perspective (the caller's buffer need
/// not outlive the `CDict`).
///
/// # Safety
/// Same as [`ZSTD_createCDict`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createCDict_byReference(
    dict_buffer: *const u8,
    dict_size: usize,
    compression_level: c_int,
) -> *mut ZSTD_CDict {
    unsafe { ZSTD_createCDict(dict_buffer, dict_size, compression_level) }
}

/// `ZSTD_compressionParameters` — ABI mirror of the upstream struct
/// (7 × `unsigned`).
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZSTD_compressionParameters {
    pub windowLog: c_uint,
    pub chainLog: c_uint,
    pub hashLog: c_uint,
    pub searchLog: c_uint,
    pub minMatch: c_uint,
    pub targetLength: c_uint,
    /// `ZSTD_strategy` ordinal (`fast = 1` … `btultra2 = 9`).
    pub strategy: c_uint,
}

/// `ZSTD_frameParameters` — ABI mirror of the upstream struct (3 × `int`).
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZSTD_frameParameters {
    pub contentSizeFlag: c_int,
    pub checksumFlag: c_int,
    pub noDictIDFlag: c_int,
}

/// `ZSTD_customMem` — ABI mirror (alloc fn, free fn, opaque). Custom
/// allocators are not supported: only the default one (both callbacks `NULL`)
/// is accepted; anything else fails creation.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZSTD_customMem {
    pub customAlloc: *const core::ffi::c_void,
    pub customFree: *const core::ffi::c_void,
    pub opaque: *const core::ffi::c_void,
}

impl ZSTD_customMem {
    /// Whether this selects the default allocator, the only one accepted:
    /// both callbacks `NULL`, as upstream decides it; `opaque` is only ever
    /// handed to the callbacks, so it does not matter here.
    pub(crate) fn is_default(&self) -> bool {
        self.customAlloc.is_null() && self.customFree.is_null()
    }
}

/// Representative compression level for an explicit strategy ordinal: feeds
/// the level-derived defaults for the knobs `cParams` leaves at 0. `0` leaves
/// the strategy to the default level as well, as upstream's
/// `ZSTD_overrideCParams` leaves any 0 field. `None` for ordinals past
/// `ZSTD_strategy`'s 9 (invalid input, not a tier).
fn level_for_strategy(strategy: c_uint) -> Option<c_int> {
    Some(match strategy {
        0 => 3,  // ZSTD_CLEVEL_DEFAULT
        1 => 1,  // fast
        2 => 3,  // dfast
        3 => 5,  // greedy
        4 => 6,  // lazy
        5 => 9,  // lazy2
        6 => 13, // btlazy2
        7 => 16, // btopt
        8 => 18, // btultra
        9 => 19, // btultra2
        _ => return None,
    })
}

/// Map explicit `ZSTD_compressionParameters` to the sticky-parameter
/// snapshot the codec resolves (0 = auto for every knob, upstream rule).
/// `None` when the strategy ordinal is past `ZSTD_strategy`'s range.
fn cctx_params_from_cparams(
    cparams: &ZSTD_compressionParameters,
) -> Option<crate::cctx_params::CCtxParams> {
    use codec::encoding::Strategy;
    let opt = |v: c_uint| if v == 0 { None } else { Some(v) };
    Some(crate::cctx_params::CCtxParams {
        level: level_for_strategy(cparams.strategy)?,
        window_log: opt(cparams.windowLog),
        hash_log: opt(cparams.hashLog),
        chain_log: opt(cparams.chainLog),
        search_log: opt(cparams.searchLog),
        min_match: opt(cparams.minMatch),
        target_length: opt(cparams.targetLength),
        strategy: Strategy::from_ordinal(cparams.strategy),
        ..crate::cctx_params::CCtxParams::default()
    })
}

/// `ZSTD_CDict* ZSTD_createCDict_advanced(const void* dictBuffer, size_t
/// dictSize, ZSTD_dictLoadMethod_e dictLoadMethod, ZSTD_dictContentType_e
/// dictContentType, ZSTD_compressionParameters cParams, ZSTD_customMem
/// customMem)` — explicit content type + compression parameters. The load
/// method is ignored (bytes are always copied); custom allocators are not
/// supported (`customMem` must be all-`NULL`).
///
/// # Safety
/// `dict_buffer` must be valid for `dict_size` bytes (or `NULL` with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createCDict_advanced(
    dict_buffer: *const u8,
    dict_size: usize,
    _dict_load_method: c_int,
    dict_content_type: c_int,
    cparams: ZSTD_compressionParameters,
    custom_mem: ZSTD_customMem,
) -> *mut ZSTD_CDict {
    // Validate the explicit parameters up front: an invalid strategy ordinal
    // or an unsupported combination must fail creation (NULL) rather than
    // silently skip them at attach time.
    let Some(params) = cctx_params_from_cparams(&cparams) else {
        return core::ptr::null_mut();
    };
    let dict = unsafe { in_slice(dict_buffer, dict_size) };
    create_cdict_with_params(dict, dict_content_type, &params, &custom_mem)
}

/// The body of `ZSTD_createCDict_advanced` and `_advanced2`: a CDict that
/// compresses under the explicit `params`, `NULL` when the dictionary, the
/// content type, the parameters or the allocator are refused.
pub(crate) fn create_cdict_with_params(
    dict: &[u8],
    dict_content_type: c_int,
    params: &crate::cctx_params::CCtxParams,
    custom_mem: &ZSTD_customMem,
) -> *mut ZSTD_CDict {
    if !custom_mem.is_default() {
        return core::ptr::null_mut();
    }
    let Ok(raw_content) = crate::attach::encode_raw_content(dict, dict_content_type) else {
        return core::ptr::null_mut();
    };
    let Some(resolved) = params.resolve() else {
        return core::ptr::null_mut();
    };
    let Ok(prepared) = prepare_cdict_content(dict, raw_content) else {
        return core::ptr::null_mut();
    };
    try_box(ZSTD_CDict {
        dict: prepared,
        id: if raw_content {
            0
        } else {
            dict_id_from_bytes(dict)
        },
        level: params.level,
        serial: next_dict_serial(),
        raw_content,
        params: Some(resolved),
    })
}

/// `ZSTD_DDict* ZSTD_createDDict_advanced(const void* dictBuffer, size_t
/// dictSize, ZSTD_dictLoadMethod_e dictLoadMethod, ZSTD_dictContentType_e
/// dictContentType, ZSTD_customMem customMem)` — explicit content type. The
/// load method is ignored (bytes are always copied); custom allocators are
/// not supported.
///
/// # Safety
/// `dict_buffer` must be valid for `dict_size` bytes (or `NULL` with 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createDDict_advanced(
    dict_buffer: *const u8,
    dict_size: usize,
    _dict_load_method: c_int,
    dict_content_type: c_int,
    custom_mem: ZSTD_customMem,
) -> *mut ZSTD_DDict {
    if !custom_mem.is_default() {
        return core::ptr::null_mut();
    }
    let dict = unsafe { in_slice(dict_buffer, dict_size) };
    create_ddict(dict, dict_content_type)
}

/// `size_t ZSTD_freeCDict(ZSTD_CDict* CDict)` — free the dictionary; `NULL` is a
/// no-op returning 0.
///
/// # Safety
/// `cdict` must be a pointer from [`ZSTD_createCDict`] not already freed, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeCDict(cdict: *mut ZSTD_CDict) -> usize {
    unsafe { free_boxed(cdict) };
    0
}

/// `size_t ZSTD_sizeof_CDict(const ZSTD_CDict* cdict)` — heap footprint of the
/// dictionary object, or 0 for `NULL`.
///
/// # Safety
/// `cdict` must be a live pointer from [`ZSTD_createCDict`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_CDict(cdict: *const ZSTD_CDict) -> usize {
    if cdict.is_null() {
        return 0;
    }
    let cdict = unsafe { &*cdict };
    core::mem::size_of::<ZSTD_CDict>() + cdict.dict.as_ref().map_or(0, EncoderDictionary::heap_size)
}

/// `unsigned ZSTD_getDictID_fromCDict(const ZSTD_CDict* cdict)` — the dictionary
/// ID, or 0 for a raw-content dictionary / `NULL`.
///
/// # Safety
/// `cdict` must be a live pointer from [`ZSTD_createCDict`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getDictID_fromCDict(cdict: *const ZSTD_CDict) -> c_uint {
    if cdict.is_null() {
        return 0;
    }
    unsafe { &*cdict }.id
}

/// The parameters a `ZSTD_*_usingCDict*` frame of `src_size` bytes runs
/// under: the CDict's own, as the codec adapts them to a frame of that size.
pub(crate) fn cdict_frame_parameters(
    cdict: &ZSTD_CDict,
    src_size: Option<u64>,
) -> Option<CompressionParameters> {
    cdict
        .params
        .map(|params| params.for_dictionary_frame(src_size))
}

/// `size_t ZSTD_compress_usingCDict(ZSTD_CCtx* cctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize, const ZSTD_CDict* cdict)` —
/// compress `src` with `cdict` at the level baked into the `CDict`. The `cctx`
/// caches the parsed dictionary + primed matcher keyed by the `CDict`'s serial,
/// so repeated calls with the same `CDict` skip the re-parse / re-prime.
///
/// # Safety
/// `cctx` / `cdict` must be live handles; `dst` / `src` valid for their lengths
/// (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compress_usingCDict(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    cdict: *const ZSTD_CDict,
) -> usize {
    if cctx.is_null() || cdict.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    cctx.begin_simple_one_shot();
    let cdict_ref = unsafe { &*cdict };
    let src = unsafe { in_slice(src, src_size) };
    let key = cdict_ref.serial;

    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
        // Disjoint borrows: the kept compressor and the scratch buffer are
        // distinct fields, so split the &mut here to avoid aliasing.
        let ZSTD_CCtx {
            scratch,
            compressor,
            ..
        } = cctx;
        // The kept compressor attaches the CDict once; the next call with it
        // reuses the primed snapshot that first frame captured.
        let enc = kept_compressor(compressor, key, cdict_ref.dict.as_ref(), cdict_ref.level)?;
        let frame_params = cdict_frame_parameters(cdict_ref, Some(src.len() as u64));
        set_frame_parameters(enc, frame_params.as_ref(), cdict_ref.level);
        // Per-call frame flags: the compressor is shared with every other
        // one-shot entry point, whose flags must not leak into this one.
        // Upstream ZSTD_compress_usingCDict leaves the checksum off unless the
        // CDict's params enabled it; we match the plain default.
        enc.set_content_checksum(false);
        enc.set_content_size_flag(true);
        enc.set_dictionary_id_flag(!cdict_ref.raw_content);
        crate::context::plain_frame_shape(enc);
        scratch.clear();
        enc.compress_independent_frame_into(src, scratch);
        Ok(())
    }));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(code)) => return encode(code),
        Err(_) => {
            // A panic can leave the kept compressor torn; the next call
            // builds a new one.
            cctx.compressor = None;
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let len = match cctx.hand_out_frame(dst) {
        Ok(len) => len,
        Err(code) => return encode(code),
    };
    cctx.progress = crate::context::FrameProgress::one_shot(src.len(), len);
    len
}

/// `size_t ZSTD_compress_usingCDict_advanced(ZSTD_CCtx* cctx, void* dst,
/// size_t dstCapacity, const void* src, size_t srcSize, const ZSTD_CDict*
/// cdict, ZSTD_frameParameters fParams)` — [`ZSTD_compress_usingCDict`] with
/// explicit frame parameters (content-size / checksum / dictionary-ID flags).
///
/// # Safety
/// Same as [`ZSTD_compress_usingCDict`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compress_usingCDict_advanced(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    cdict: *const ZSTD_CDict,
    fparams: ZSTD_frameParameters,
) -> usize {
    if cctx.is_null() || cdict.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    cctx.begin_simple_one_shot();
    let cdict_ref = unsafe { &*cdict };
    let src = unsafe { in_slice(src, src_size) };
    let key = cdict_ref.serial;

    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
        let ZSTD_CCtx {
            scratch,
            compressor,
            ..
        } = cctx;
        let enc = kept_compressor(compressor, key, cdict_ref.dict.as_ref(), cdict_ref.level)?;
        let frame_params = cdict_frame_parameters(cdict_ref, Some(src.len() as u64));
        set_frame_parameters(enc, frame_params.as_ref(), cdict_ref.level);
        enc.set_content_checksum(fparams.checksumFlag != 0);
        enc.set_content_size_flag(fparams.contentSizeFlag != 0);
        // Raw-content dictionaries never emit their synthetic ID regardless
        // of the flag.
        enc.set_dictionary_id_flag(fparams.noDictIDFlag == 0 && !cdict_ref.raw_content);
        crate::context::plain_frame_shape(enc);
        scratch.clear();
        enc.compress_independent_frame_into(src, scratch);
        Ok(())
    }));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(code)) => return encode(code),
        Err(_) => {
            cctx.compressor = None;
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let len = match cctx.hand_out_frame(dst) {
        Ok(len) => len,
        Err(code) => return encode(code),
    };
    cctx.progress = crate::context::FrameProgress::one_shot(src.len(), len);
    len
}

/// `ZSTD_DDict* ZSTD_createDDict(const void* dictBuffer, size_t dictSize)` —
/// prepare a dictionary for repeated decompression. Returns `NULL` on
/// allocation failure.
///
/// # Safety
/// `dictBuffer` must be valid for `dictSize` bytes (or `NULL` with `dictSize == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createDDict(
    dict_buffer: *const u8,
    dict_size: usize,
) -> *mut ZSTD_DDict {
    let dict = unsafe { in_slice(dict_buffer, dict_size) };
    create_ddict(dict, crate::attach::ZSTD_DCT_AUTO)
}

/// `ZSTD_DDict* ZSTD_createDDict_byReference(const void* dictBuffer, size_t
/// dictSize)` (static API) — identical to [`ZSTD_createDDict`]; we copy the
/// bytes so the caller's buffer need not outlive the `DDict`.
///
/// # Safety
/// Same as [`ZSTD_createDDict`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_createDDict_byReference(
    dict_buffer: *const u8,
    dict_size: usize,
) -> *mut ZSTD_DDict {
    unsafe { ZSTD_createDDict(dict_buffer, dict_size) }
}

/// `size_t ZSTD_freeDDict(ZSTD_DDict* ddict)` — free the dictionary; `NULL` is a
/// no-op returning 0.
///
/// # Safety
/// `ddict` must be a pointer from [`ZSTD_createDDict`] not already freed, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeDDict(ddict: *mut ZSTD_DDict) -> usize {
    unsafe { free_boxed(ddict) };
    0
}

/// `size_t ZSTD_sizeof_DDict(const ZSTD_DDict* ddict)` — heap footprint, or 0
/// for `NULL`.
///
/// # Safety
/// `ddict` must be a live pointer from [`ZSTD_createDDict`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_DDict(ddict: *const ZSTD_DDict) -> usize {
    if ddict.is_null() {
        return 0;
    }
    let ddict = unsafe { &*ddict };
    core::mem::size_of::<ZSTD_DDict>()
        + ddict
            .prepared
            .as_ref()
            .map_or(0, codec::decoding::DictionaryHandle::heap_size)
}

/// `unsigned ZSTD_getDictID_fromDDict(const ZSTD_DDict* ddict)` — the dictionary
/// ID, or 0 for a raw-content dictionary / `NULL`.
///
/// # Safety
/// `ddict` must be a live pointer from [`ZSTD_createDDict`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getDictID_fromDDict(ddict: *const ZSTD_DDict) -> c_uint {
    if ddict.is_null() {
        return 0;
    }
    unsafe { &*ddict }.id
}

/// `size_t ZSTD_decompress_usingDDict(ZSTD_DCtx* dctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize, const ZSTD_DDict* ddict)` —
/// decompress a dictionary-compressed frame with the DDict's prepared
/// dictionary, borrowed for the call; an empty DDict decodes without one.
///
/// # Safety
/// `dctx` must be live, `ddict` live or `NULL`; `dst` / `src` valid for their
/// lengths (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompress_usingDDict(
    dctx: *mut ZSTD_DCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    ddict: *const ZSTD_DDict,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    // A NULL DDict decodes without a dictionary, as upstream's
    // ZSTD_decompressMultiFrame does.
    let prepared = unsafe { ddict.as_ref() }.and_then(|ddict| ddict.prepared.as_ref());
    let src = match unsafe { crate::ffi::decode_input(src, src_size, dst, dst_capacity) } {
        Ok(src) => src,
        Err(code) => return encode(code),
    };
    let src = &*src;
    let dst = unsafe { out_slice(dst, dst_capacity) };

    // The handle is forced on every frame, which also covers raw-content
    // dictionaries and ID-less frames.
    dctx.decoder.set_content_checksum(ContentChecksum::Verify);
    let outcome = catch_unwind(AssertUnwindSafe(|| match prepared {
        Some(handle) => dctx.decoder.decode_all_with_dict_handle(src, dst, handle),
        None => dctx.decoder.decode_all(src, dst),
    }));
    match outcome {
        Ok(Ok(written)) => {
            // One-shot consumed whole frames: the context is back at a
            // frame boundary (see ZSTD_decompressDCtx for the rationale).
            dctx.stream_frame_done = true;
            written
        }
        Ok(Err(err)) => {
            // An ordinary decode error leaves the decoder's state coherent
            // (the next one-shot re-initializes it per frame), but the
            // context must still sit at a frame boundary for the STREAMING
            // entry point — without this, a context abandoned mid-stream
            // would resume the failed one-shot's frame state on the next
            // ZSTD_decompressStream call. The decoder itself is kept: its
            // warm workspace survives routine corrupt-input failures, the
            // full replacement is reserved for the panic arm where the
            // state may be torn mid-unwind.
            dctx.stream_frame_done = true;
            encode(code_for_decoder_error(&err))
        }
        Err(_) => {
            dctx.replace_decoder();
            dctx.stream_frame_done = true;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}
