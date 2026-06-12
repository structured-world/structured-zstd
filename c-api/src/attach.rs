//! Context-attached dictionaries: the `ZSTD_CCtx_loadDictionary` /
//! `ZSTD_CCtx_refCDict` / `ZSTD_CCtx_refPrefix` family and its `ZSTD_DCtx`
//! mirror, plus the `*_usingDict` one-shots.
//!
//! Attach state lives on the context and is applied at every frame start
//! (`ZSTD_compress2`, `ZSTD_compressStream2`, `ZSTD_decompressDCtx`,
//! `ZSTD_decompressStream`). `loadDictionary` / `refCDict` / `refDDict` are
//! sticky (they survive `ZSTD_reset_session_only`); `refPrefix` is single-use
//! and consumed by the next frame, both per upstream `zstd.h`.
//!
//! Raw-content dictionaries (no `ZSTD_MAGIC_DICTIONARY` prefix, and every
//! `refPrefix`) are modelled as a parsed dictionary with a synthetic non-zero
//! ID whose emission is suppressed (`set_dictionary_id_flag(false)`), so the
//! wire never carries the placeholder; decode-side the synthetic ID only has
//! to agree between the two attach paths, which it does by construction.

use core::ffi::{c_int, c_uint};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::{ContentChecksum, Dictionary, DictionaryHandle};
use codec::encoding::{CompressionLevel, FrameCompressor};

use crate::cdict::{ZSTD_CDict, ZSTD_DDict, next_dict_serial};
use crate::context::{ZSTD_CCtx, ZSTD_DCtx};
use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::ffi::{in_slice, out_slice};

/// `ZSTD_dictContentType_e` discriminants (`zstd.h`).
pub(crate) const ZSTD_DCT_AUTO: c_int = 0;
pub(crate) const ZSTD_DCT_RAW_CONTENT: c_int = 1;
pub(crate) const ZSTD_DCT_FULL_DICT: c_int = 2;

/// `ZSTD_dictMagicNumber` little-endian prefix of a serialized dictionary.
const DICT_MAGIC: u32 = 0xEC30_A437;

/// Synthetic non-zero ID for raw-content dictionaries. The encoder attach
/// path requires a non-zero ID, but raw-content frames never put it on the
/// wire (the ID flag is suppressed); the decoder side uses the same constant
/// so an ID-less frame resolves against the same dictionary object.
const RAW_CONTENT_DICT_ID: u32 = u32::MAX;

/// Dictionary state attached to a compression context.
pub(crate) enum CCtxDictAttach {
    /// No dictionary: frames compress dictionary-less.
    None,
    /// `ZSTD_CCtx_loadDictionary*`: sticky copied dictionary bytes.
    /// `raw_content` selects the raw-content modelling (synthetic ID,
    /// suppressed on the wire); `serial` keys the context's cached
    /// dict-compressor so a re-load invalidates it.
    Load {
        raw: Vec<u8>,
        raw_content: bool,
        serial: u64,
    },
    /// `ZSTD_CCtx_refCDict`: sticky reference to a caller-owned `ZSTD_CDict`.
    /// Per the C contract the CDict must outlive its use by this context.
    /// The serial snapshot keys the cached compressor (ABA-safe, see
    /// [`crate::cdict::next_dict_serial`]).
    RefCDict {
        cdict: *const ZSTD_CDict,
        serial: u64,
    },
    /// `ZSTD_CCtx_refPrefix*`: single-use raw-content dictionary for the
    /// next frame only.
    Prefix { content: Vec<u8>, serial: u64 },
}

impl CCtxDictAttach {
    /// Heap bytes owned by the attach state. Referenced (`RefCDict`)
    /// dictionaries are caller-owned and excluded, matching upstream's
    /// `ZSTD_sizeof_CCtx` treatment of referenced dictionaries.
    pub(crate) fn heap_size(&self) -> usize {
        match self {
            CCtxDictAttach::None | CCtxDictAttach::RefCDict { .. } => 0,
            CCtxDictAttach::Load { raw, .. } => raw.capacity(),
            CCtxDictAttach::Prefix { content, .. } => content.capacity(),
        }
    }
}

/// Dictionary state attached to a decompression context. All variants hold
/// the dictionary pre-parsed as a [`DictionaryHandle`] (`Arc`-shared), so the
/// per-frame application is a refcount bump.
pub(crate) enum DCtxDictAttach {
    None,
    /// `ZSTD_DCtx_loadDictionary*`: sticky.
    Load {
        handle: DictionaryHandle,
    },
    /// `ZSTD_DCtx_refDDict`: sticky reference; the parse of the DDict's bytes
    /// is cached here keyed by its serial.
    RefDDict {
        serial: u64,
        handle: DictionaryHandle,
    },
    /// `ZSTD_DCtx_refPrefix`: single-use raw-content dictionary.
    Prefix {
        handle: DictionaryHandle,
    },
}

impl DCtxDictAttach {
    /// Heap bytes owned by the attach state: the parsed dictionary behind
    /// `loadDictionary` / `refPrefix` is context-owned (counted); a
    /// referenced `DDict`'s parse is cached here but its bytes belong to
    /// the caller's handle, so only the parsed copy is reported.
    pub(crate) fn heap_size(&self) -> usize {
        match self {
            DCtxDictAttach::None => 0,
            DCtxDictAttach::Load { handle }
            | DCtxDictAttach::RefDDict { handle, .. }
            | DCtxDictAttach::Prefix { handle } => {
                // Content plus the parsed entropy tables' heap; the inline
                // FSE decode arrays are covered by the struct size term.
                handle.as_dict().heap_bytes() + core::mem::size_of::<codec::decoding::Dictionary>()
            }
        }
    }
}

/// Parse caller bytes into a decode-side [`DictionaryHandle`] honouring the
/// `ZSTD_dictContentType_e` selection.
pub(crate) fn parse_decode_dict(
    dict: &[u8],
    content_type: c_int,
) -> Result<DictionaryHandle, ZSTD_ErrorCode> {
    // The magic is 4 bytes; classify on those alone (a magic-prefixed blob
    // shorter than a full header then fails the parse as corrupted instead
    // of silently degrading to raw content).
    let has_magic =
        dict.len() >= 4 && u32::from_le_bytes([dict[0], dict[1], dict[2], dict[3]]) == DICT_MAGIC;
    let full = match content_type {
        ZSTD_DCT_AUTO => has_magic,
        ZSTD_DCT_RAW_CONTENT => false,
        ZSTD_DCT_FULL_DICT => {
            if !has_magic {
                return Err(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted);
            }
            true
        }
        _ => return Err(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound),
    };
    let parsed = if full {
        Dictionary::decode_dict(dict)
            .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?
    } else {
        Dictionary::from_raw_content(RAW_CONTENT_DICT_ID, dict.to_vec())
            .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?
    };
    Ok(DictionaryHandle::from_dictionary(parsed))
}

/// Whether `dict` selects raw-content modelling on the encode side.
fn encode_raw_content(dict: &[u8], content_type: c_int) -> Result<bool, ZSTD_ErrorCode> {
    let has_magic =
        dict.len() >= 4 && u32::from_le_bytes([dict[0], dict[1], dict[2], dict[3]]) == DICT_MAGIC;
    match content_type {
        ZSTD_DCT_AUTO => Ok(!has_magic),
        ZSTD_DCT_RAW_CONTENT => Ok(true),
        ZSTD_DCT_FULL_DICT => {
            if !has_magic {
                return Err(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted);
            }
            Ok(false)
        }
        _ => Err(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound),
    }
}

impl ZSTD_CCtx {
    /// Attach the context's dictionary state (if any) to a freshly-built
    /// frame compressor. Returns `Err` when the attached dictionary cannot
    /// be applied (corrupt bytes, freed CDict contract violation surfaces as
    /// UB per the C API and cannot be detected here).
    ///
    /// `Prefix` is single-use: the caller must invoke
    /// [`Self::consume_prefix`] once the frame has actually started.
    pub(crate) fn apply_attached_dict(
        &self,
        enc: &mut FrameCompressor,
    ) -> Result<(), ZSTD_ErrorCode> {
        match &self.attached_dict {
            CCtxDictAttach::None => Ok(()),
            CCtxDictAttach::Load {
                raw, raw_content, ..
            } => {
                if *raw_content {
                    let dict = Dictionary::from_raw_content(RAW_CONTENT_DICT_ID, raw.clone())
                        .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                    enc.set_dictionary(dict)
                        .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                    enc.set_dictionary_id_flag(false);
                } else {
                    enc.set_dictionary_from_bytes(raw)
                        .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                }
                Ok(())
            }
            CCtxDictAttach::RefCDict { cdict, .. } => {
                // SAFETY: C contract — the CDict outlives every context that
                // references it (`ZSTD_CCtx_refCDict` lifetime rule).
                let cdict = unsafe { &**cdict };
                cdict
                    .attach_to(enc)
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)
            }
            CCtxDictAttach::Prefix { content, .. } => {
                let dict = Dictionary::from_raw_content(RAW_CONTENT_DICT_ID, content.clone())
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                enc.set_dictionary(dict)
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                enc.set_dictionary_id_flag(false);
                Ok(())
            }
        }
    }

    /// [`Self::apply_attached_dict`] for the streaming encoder: same
    /// semantics, applied to the per-frame [`StreamingEncoder`].
    pub(crate) fn apply_attached_dict_streaming(
        &self,
        enc: &mut codec::encoding::StreamingEncoder<Vec<u8>>,
    ) -> Result<(), ZSTD_ErrorCode> {
        use codec::encoding::EncoderDictionary;
        let corrupted = |_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted;
        match &self.attached_dict {
            CCtxDictAttach::None => Ok(()),
            CCtxDictAttach::Load {
                raw, raw_content, ..
            } => {
                if *raw_content {
                    let dict = Dictionary::from_raw_content(RAW_CONTENT_DICT_ID, raw.clone())
                        .map_err(corrupted)?;
                    enc.set_encoder_dictionary(EncoderDictionary::from_dictionary(dict))
                        .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)
                } else {
                    enc.set_dictionary_from_bytes(raw)
                        .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)
                }
            }
            CCtxDictAttach::RefCDict { cdict, .. } => {
                // SAFETY: C contract — live CDict (see `apply_attached_dict`).
                let cdict = unsafe { &**cdict };
                cdict
                    .attach_to_streaming(enc)
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)
            }
            CCtxDictAttach::Prefix { content, .. } => {
                let dict = Dictionary::from_raw_content(RAW_CONTENT_DICT_ID, content.clone())
                    .map_err(corrupted)?;
                enc.set_encoder_dictionary(EncoderDictionary::from_dictionary(dict))
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)
            }
        }
    }

    /// Cache key for the dictionary-attached compressor: the attach
    /// identity (`0` = no dictionary).
    pub(crate) fn attach_serial(&self) -> u64 {
        match &self.attached_dict {
            CCtxDictAttach::None => 0,
            CCtxDictAttach::Load { serial, .. } => *serial,
            CCtxDictAttach::RefCDict { serial, .. } => *serial,
            CCtxDictAttach::Prefix { serial, .. } => *serial,
        }
    }

    /// The compression level frames must use under the current attach:
    /// a referenced CDict's parameters win over the context's sticky level
    /// (upstream rule); every other attach keeps the context level.
    pub(crate) fn attach_level(&self) -> c_int {
        match &self.attached_dict {
            // SAFETY: C contract — live CDict (see `apply_attached_dict`).
            CCtxDictAttach::RefCDict { cdict, .. } => unsafe { &**cdict }.level,
            _ => self.params.level,
        }
    }

    /// Drop a single-use prefix after the frame that consumed it started.
    pub(crate) fn consume_prefix(&mut self) {
        if matches!(self.attached_dict, CCtxDictAttach::Prefix { .. }) {
            self.attached_dict = CCtxDictAttach::None;
        }
    }

    /// Whether any dictionary is attached.
    pub(crate) fn has_attached_dict(&self) -> bool {
        !matches!(self.attached_dict, CCtxDictAttach::None)
    }

    /// Whether the attach models raw content (synthetic dictionary ID that
    /// must never reach the wire, regardless of `ZSTD_c_dictIDFlag`).
    pub(crate) fn attach_suppresses_dict_id(&self) -> bool {
        match &self.attached_dict {
            CCtxDictAttach::Load { raw_content, .. } => *raw_content,
            CCtxDictAttach::Prefix { .. } => true,
            // SAFETY: C contract — live CDict (see `apply_attached_dict`).
            CCtxDictAttach::RefCDict { cdict, .. } => unsafe { &**cdict }.raw_content,
            CCtxDictAttach::None => false,
        }
    }

    /// Whether compression parameters come from the referenced CDict rather
    /// than the context's sticky parameters (upstream: a referenced CDict's
    /// parameters win).
    pub(crate) fn attach_params_from_cdict(&self) -> bool {
        matches!(self.attached_dict, CCtxDictAttach::RefCDict { .. })
    }
}

impl ZSTD_DCtx {
    /// The dictionary handle the next frame must decode with, if any.
    pub(crate) fn attached_handle(&self) -> Option<DictionaryHandle> {
        match &self.attached_ddict {
            DCtxDictAttach::None => None,
            DCtxDictAttach::Load { handle } => Some(handle.clone()),
            DCtxDictAttach::RefDDict { handle, .. } => Some(handle.clone()),
            DCtxDictAttach::Prefix { handle } => Some(handle.clone()),
        }
    }

    /// Drop a single-use prefix after the frame that consumed it started.
    pub(crate) fn consume_prefix(&mut self) {
        if matches!(self.attached_ddict, DCtxDictAttach::Prefix { .. }) {
            self.attached_ddict = DCtxDictAttach::None;
        }
    }
}

/// Shared body of the `ZSTD_CCtx_loadDictionary*` family.
///
/// # Safety
/// `dict` valid for `dict_size` bytes (or `NULL` + 0); `cctx` live.
unsafe fn cctx_load_dictionary(
    cctx: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
    content_type: c_int,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    if cctx.stream_in_progress() {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    let dict = unsafe { in_slice(dict, dict_size) };
    if dict.is_empty() {
        // Upstream: loading a NULL / empty dictionary clears the attach.
        cctx.attached_dict = CCtxDictAttach::None;
        return 0;
    }
    let raw_content = match encode_raw_content(dict, content_type) {
        Ok(v) => v,
        Err(code) => return encode(code),
    };
    if !raw_content {
        // Fail-fast parse so a corrupt dictionary errors here, not at the
        // next compression (upstream parity).
        let parsed = catch_unwind(AssertUnwindSafe(|| {
            codec::encoding::EncoderDictionary::from_bytes(dict)
        }));
        match parsed {
            Ok(Ok(_)) => {}
            _ => return encode(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted),
        }
    }
    cctx.attached_dict = CCtxDictAttach::Load {
        raw: dict.to_vec(),
        raw_content,
        serial: next_dict_serial(),
    };
    0
}

/// `size_t ZSTD_CCtx_loadDictionary(ZSTD_CCtx* cctx, const void* dict, size_t
/// dictSize)` — sticky dictionary for all frames started after this call.
/// Content type is auto-detected from the magic. `NULL` / empty clears.
///
/// # Safety
/// `cctx` must be live; `dict` valid for `dictSize` bytes (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_loadDictionary(
    cctx: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    unsafe { cctx_load_dictionary(cctx, dict, dict_size, ZSTD_DCT_AUTO) }
}

/// `ZSTD_CCtx_loadDictionary_byReference` — we always copy the bytes, so this
/// is behaviourally identical to [`ZSTD_CCtx_loadDictionary`] (the caller's
/// buffer need not outlive the context).
///
/// # Safety
/// Same as [`ZSTD_CCtx_loadDictionary`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_loadDictionary_byReference(
    cctx: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    unsafe { cctx_load_dictionary(cctx, dict, dict_size, ZSTD_DCT_AUTO) }
}

/// `ZSTD_CCtx_loadDictionary_advanced` — explicit load method + content type.
/// The load method is accepted and ignored (bytes are always copied).
///
/// # Safety
/// Same as [`ZSTD_CCtx_loadDictionary`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_loadDictionary_advanced(
    cctx: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
    _load_method: c_int,
    content_type: c_int,
) -> usize {
    unsafe { cctx_load_dictionary(cctx, dict, dict_size, content_type) }
}

/// `size_t ZSTD_CCtx_refCDict(ZSTD_CCtx* cctx, const ZSTD_CDict* cdict)` —
/// sticky reference; the CDict's parameters win for referenced frames. `NULL`
/// clears the attach.
///
/// # Safety
/// `cctx` must be live; `cdict` must stay live for as long as this context
/// compresses with it (C contract), or be `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_refCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    if cctx.stream_in_progress() {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    if cdict.is_null() {
        cctx.attached_dict = CCtxDictAttach::None;
        return 0;
    }
    let serial = unsafe { &*cdict }.serial;
    cctx.attached_dict = CCtxDictAttach::RefCDict { cdict, serial };
    0
}

/// Shared body of `ZSTD_CCtx_refPrefix*`.
///
/// # Safety
/// `prefix` valid for `prefix_size` bytes (or `NULL` + 0); `cctx` live.
unsafe fn cctx_ref_prefix(
    cctx: *mut ZSTD_CCtx,
    prefix: *const u8,
    prefix_size: usize,
    content_type: c_int,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    if cctx.stream_in_progress() {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    // A prefix is raw content by definition: only the auto / rawContent
    // selectors are meaningful; everything else (fullDict, out-of-range
    // discriminants) is rejected.
    if !matches!(content_type, ZSTD_DCT_AUTO | ZSTD_DCT_RAW_CONTENT) {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    let prefix = unsafe { in_slice(prefix, prefix_size) };
    if prefix.is_empty() {
        cctx.attached_dict = CCtxDictAttach::None;
        return 0;
    }
    cctx.attached_dict = CCtxDictAttach::Prefix {
        content: prefix.to_vec(),
        serial: next_dict_serial(),
    };
    0
}

/// `size_t ZSTD_CCtx_refPrefix(ZSTD_CCtx* cctx, const void* prefix, size_t
/// prefixSize)` — single-use raw-content dictionary for the next frame only.
///
/// # Safety
/// `cctx` must be live; `prefix` valid for `prefixSize` bytes (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_refPrefix(
    cctx: *mut ZSTD_CCtx,
    prefix: *const u8,
    prefix_size: usize,
) -> usize {
    unsafe { cctx_ref_prefix(cctx, prefix, prefix_size, ZSTD_DCT_RAW_CONTENT) }
}

/// `ZSTD_CCtx_refPrefix_advanced` — explicit content type (`fullDict` is
/// rejected; a prefix is raw content).
///
/// # Safety
/// Same as [`ZSTD_CCtx_refPrefix`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtx_refPrefix_advanced(
    cctx: *mut ZSTD_CCtx,
    prefix: *const u8,
    prefix_size: usize,
    content_type: c_int,
) -> usize {
    unsafe { cctx_ref_prefix(cctx, prefix, prefix_size, content_type) }
}

/// Shared body of the `ZSTD_DCtx_loadDictionary*` family.
///
/// # Safety
/// `dict` valid for `dict_size` bytes (or `NULL` + 0); `dctx` live.
unsafe fn dctx_load_dictionary(
    dctx: *mut ZSTD_DCtx,
    dict: *const u8,
    dict_size: usize,
    content_type: c_int,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    if !dctx.stream_frame_done {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    let dict = unsafe { in_slice(dict, dict_size) };
    if dict.is_empty() {
        dctx.attached_ddict = DCtxDictAttach::None;
        return 0;
    }
    let parsed = catch_unwind(AssertUnwindSafe(|| parse_decode_dict(dict, content_type)));
    match parsed {
        Ok(Ok(handle)) => {
            dctx.attached_ddict = DCtxDictAttach::Load { handle };
            0
        }
        Ok(Err(code)) => encode(code),
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    }
}

/// `size_t ZSTD_DCtx_loadDictionary(ZSTD_DCtx* dctx, const void* dict, size_t
/// dictSize)` — sticky dictionary used to decode the following frames. `NULL`
/// / empty clears.
///
/// # Safety
/// `dctx` must be live; `dict` valid for `dictSize` bytes (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_loadDictionary(
    dctx: *mut ZSTD_DCtx,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    unsafe { dctx_load_dictionary(dctx, dict, dict_size, ZSTD_DCT_AUTO) }
}

/// `ZSTD_DCtx_loadDictionary_byReference` — identical to
/// [`ZSTD_DCtx_loadDictionary`]; the bytes are always copied.
///
/// # Safety
/// Same as [`ZSTD_DCtx_loadDictionary`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_loadDictionary_byReference(
    dctx: *mut ZSTD_DCtx,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    unsafe { dctx_load_dictionary(dctx, dict, dict_size, ZSTD_DCT_AUTO) }
}

/// `ZSTD_DCtx_loadDictionary_advanced` — explicit load method (ignored; bytes
/// are copied) + content type.
///
/// # Safety
/// Same as [`ZSTD_DCtx_loadDictionary`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_loadDictionary_advanced(
    dctx: *mut ZSTD_DCtx,
    dict: *const u8,
    dict_size: usize,
    _load_method: c_int,
    content_type: c_int,
) -> usize {
    unsafe { dctx_load_dictionary(dctx, dict, dict_size, content_type) }
}

/// `size_t ZSTD_DCtx_refDDict(ZSTD_DCtx* dctx, const ZSTD_DDict* ddict)` —
/// sticky reference. The DDict's bytes are parsed once here and cached on the
/// context keyed by the DDict's serial. `NULL` clears.
///
/// # Safety
/// `dctx` must be live; `ddict` must be a live `ZSTD_DDict`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_refDDict(
    dctx: *mut ZSTD_DCtx,
    ddict: *const ZSTD_DDict,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    if !dctx.stream_frame_done {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    if ddict.is_null() {
        dctx.attached_ddict = DCtxDictAttach::None;
        return 0;
    }
    let ddict = unsafe { &*ddict };
    if let DCtxDictAttach::RefDDict { serial, .. } = &dctx.attached_ddict
        && *serial == ddict.serial
    {
        return 0;
    }
    // Honour the DDict's creation-time content-type selection (same as
    // ZSTD_decompress_usingDDict): an explicit rawContent DDict must not be
    // re-classified on the magic at use time.
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        parse_decode_dict(&ddict.raw, ddict.content_type)
    }));
    match parsed {
        Ok(Ok(handle)) => {
            dctx.attached_ddict = DCtxDictAttach::RefDDict {
                serial: ddict.serial,
                handle,
            };
            0
        }
        Ok(Err(code)) => encode(code),
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    }
}

/// `size_t ZSTD_DCtx_refPrefix(ZSTD_DCtx* dctx, const void* prefix, size_t
/// prefixSize)` — single-use raw-content dictionary for the next frame only.
///
/// # Safety
/// `dctx` must be live; `prefix` valid for `prefixSize` bytes (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_refPrefix(
    dctx: *mut ZSTD_DCtx,
    prefix: *const u8,
    prefix_size: usize,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    if !dctx.stream_frame_done {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    }
    let prefix = unsafe { in_slice(prefix, prefix_size) };
    if prefix.is_empty() {
        dctx.attached_ddict = DCtxDictAttach::None;
        return 0;
    }
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        parse_decode_dict(prefix, ZSTD_DCT_RAW_CONTENT)
    }));
    match parsed {
        Ok(Ok(handle)) => {
            dctx.attached_ddict = DCtxDictAttach::Prefix { handle };
            0
        }
        Ok(Err(code)) => encode(code),
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    }
}

/// `ZSTD_DCtx_refPrefix_advanced` — explicit content type (`fullDict` is
/// rejected; a prefix is raw content).
///
/// # Safety
/// Same as [`ZSTD_DCtx_refPrefix`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_DCtx_refPrefix_advanced(
    dctx: *mut ZSTD_DCtx,
    prefix: *const u8,
    prefix_size: usize,
    content_type: c_int,
) -> usize {
    if !matches!(content_type, ZSTD_DCT_AUTO | ZSTD_DCT_RAW_CONTENT) {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    unsafe { ZSTD_DCtx_refPrefix(dctx, prefix, prefix_size) }
}

/// `size_t ZSTD_compress_usingDict(ZSTD_CCtx* ctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize, const void* dict, size_t
/// dictSize, int compressionLevel)` — one-shot compression with one-off
/// dictionary bytes at an explicit level. Per upstream this path rebuilds the
/// dictionary tables on every call; prefer a `ZSTD_CDict` for reuse.
///
/// # Safety
/// `ctx` must be live; `dst` / `src` / `dict` valid for their lengths (or
/// `NULL` + 0 each).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compress_usingDict(
    ctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    dict: *const u8,
    dict_size: usize,
    compression_level: c_int,
) -> usize {
    if ctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *ctx };
    let src = unsafe { in_slice(src, src_size) };
    let dict = unsafe { in_slice(dict, dict_size) };
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
        cctx.scratch.clear();
        let mut enc: FrameCompressor =
            FrameCompressor::new(CompressionLevel::from_level(compression_level));
        enc.set_content_checksum(false);
        if !dict.is_empty() {
            let raw_content = encode_raw_content(dict, ZSTD_DCT_AUTO)?;
            if raw_content {
                let parsed = Dictionary::from_raw_content(RAW_CONTENT_DICT_ID, dict.to_vec())
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                enc.set_dictionary(parsed)
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
                enc.set_dictionary_id_flag(false);
            } else {
                enc.set_dictionary_from_bytes(dict)
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
            }
        }
        enc.compress_independent_frame_into(src, &mut cctx.scratch);
        Ok(())
    }));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(code)) => return encode(code),
        Err(_) => return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    }
    let len = cctx.scratch.len();
    if len > dst_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    dst[..len].copy_from_slice(&cctx.scratch);
    len
}

/// `size_t ZSTD_decompress_usingDict(ZSTD_DCtx* dctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize, const void* dict, size_t
/// dictSize)` — one-shot decompression with one-off dictionary bytes.
///
/// # Safety
/// `dctx` must be live; `dst` / `src` / `dict` valid for their lengths (or
/// `NULL` + 0 each).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompress_usingDict(
    dctx: *mut ZSTD_DCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let dict = unsafe { in_slice(dict, dict_size) };
    // Deliberately NO `stream_frame_done` guard here: the stage_wrong check
    // is the contract of the ZSTD_DCtx_* dictionary MUTATORS (they must not
    // swap the dictionary under an open streaming frame). A one-shot decode
    // owns its frames whole — `decode_all*` re-initializes per frame (header
    // re-parse, entropy/offset/scratch reset), so a context abandoned
    // mid-stream cannot leak state in. Same semantics as ZSTD_decompressDCtx,
    // covered by the decompress_stream_recovers_after_oneshot_on_midframe_context
    // regression test; every exit path below restores the frame boundary.
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<usize, ZSTD_ErrorCode> {
        dctx.decoder.set_content_checksum(ContentChecksum::Verify);
        if dict.is_empty() {
            dctx.decoder
                .decode_all(src, dst)
                .map_err(|err| code_for_decoder_error(&err))
        } else {
            let handle = parse_decode_dict(dict, ZSTD_DCT_AUTO)?;
            dctx.decoder
                .decode_all_with_dict_handle(src, dst, &handle)
                .map_err(|err| code_for_decoder_error(&err))
        }
    }));
    match outcome {
        Ok(Ok(written)) => {
            dctx.stream_frame_done = true;
            written
        }
        Ok(Err(code)) => {
            dctx.stream_frame_done = true;
            encode(code)
        }
        Err(_) => {
            dctx.decoder = codec::decoding::FrameDecoder::new();
            dctx.stream_frame_done = true;
            dctx.ddict_serial = 0;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}

/// `unsigned ZSTD_getDictID_fromDict(const void* dict, size_t dictSize)` —
/// the dictionary ID from serialized dictionary bytes, or 0 for raw content /
/// too-short input.
///
/// # Safety
/// `dict` must be valid for `dictSize` bytes (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getDictID_fromDict(dict: *const u8, dict_size: usize) -> c_uint {
    let dict = unsafe { in_slice(dict, dict_size) };
    crate::cdict::dict_id_from_bytes(dict)
}

/// `unsigned ZSTD_getDictID_fromFrame(const void* src, size_t srcSize)` —
/// the dictionary ID declared in a frame header, or 0 when the header omits
/// it / cannot be parsed from the provided bytes.
///
/// # Safety
/// `src` must be valid for `srcSize` bytes (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getDictID_fromFrame(src: *const u8, src_size: usize) -> c_uint {
    let src = unsafe { in_slice(src, src_size) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        codec::decoding::read_frame_header_info(src, false)
    }));
    match outcome {
        Ok(Ok(info)) => info.dictionary_id.unwrap_or(0),
        _ => 0,
    }
}
