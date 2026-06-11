//! Compression / decompression dictionary objects: `ZSTD_CDict` / `ZSTD_DDict`
//! and the one-shot `*_usingCDict` / `*_usingDDict` entry points.
//!
//! These mirror upstream's prepared-dictionary model: a dictionary is parsed
//! once into a `CDict` (encoder side) or `DDict` (decoder side) and then used
//! across many compress / decompress calls. The encoder-side reuse (parsed
//! dictionary + primed match-finder snapshot) is realised by caching a
//! [`FrameCompressor`] on the `ZSTD_CCtx`, keyed by the `CDict`'s never-reused
//! serial; the decoder caches the loaded dictionary on the `ZSTD_DCtx` keyed by
//! the `DDict`'s serial (a raw-address key would be ABA-unsafe across free +
//! realloc). The match-finder tables a dictionary primes are sized to the
//! dictionary's own cParams tier (see the codec's `set_dictionary_size_hint`),
//! so a `CDict` does not drag a source-window-sized table around.

use core::ffi::{c_int, c_uint};
use core::sync::atomic::{AtomicU64, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::ContentChecksum;
use codec::encoding::{CompressionLevel, EncoderDictionary, FrameCompressor};

use crate::context::{ZSTD_CCtx, ZSTD_DCtx, free_boxed, try_box};
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

/// `ZSTD_dictMagicNumber` (`zstd.h`). A serialized zstd dictionary begins with
/// this little-endian magic; bytes `[4..8]` hold the dictionary ID. Raw-content
/// dictionaries carry no magic and report ID 0.
const DICT_MAGIC: u32 = 0xEC30_A437;

/// Parse the dictionary ID from a serialized dictionary header, or `0` for a
/// raw-content dictionary (no magic) / too-short buffer. Matches
/// `ZSTD_getDictID_fromDict` semantics.
fn dict_id_from_bytes(dict: &[u8]) -> u32 {
    if dict.len() < 8 || u32::from_le_bytes([dict[0], dict[1], dict[2], dict[3]]) != DICT_MAGIC {
        return 0;
    }
    u32::from_le_bytes([dict[4], dict[5], dict[6], dict[7]])
}

/// Opaque prepared compression dictionary. Owns the dictionary bytes plus the
/// compression level baked in at creation (upstream `ZSTD_createCDict` takes the
/// level here, and `ZSTD_compress_usingCDict` uses it). The bytes are parsed
/// into a `FrameCompressor` lazily on the owning `CCtx` the first time they are
/// used, so the `CDict` itself stays a cheap byte + id holder that many
/// contexts can reference concurrently.
#[allow(non_camel_case_types)]
pub struct ZSTD_CDict {
    pub(crate) raw: Vec<u8>,
    pub(crate) id: u32,
    pub(crate) level: c_int,
    /// Never-reused identity for context cache validation (see [`DICT_SERIAL`]).
    pub(crate) serial: u64,
}

/// Opaque prepared decompression dictionary: the dictionary bytes plus its ID.
#[allow(non_camel_case_types)]
pub struct ZSTD_DDict {
    pub(crate) raw: Vec<u8>,
    pub(crate) id: u32,
    /// Never-reused identity for context cache validation (see [`DICT_SERIAL`]).
    pub(crate) serial: u64,
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
    // Validate the dictionary parses for encoding (build the encoder entropy
    // tables once here as a fail-fast; the per-context attach re-parses from the
    // retained bytes). A dictionary that cannot be parsed yields NULL.
    let outcome = catch_unwind(AssertUnwindSafe(|| EncoderDictionary::from_bytes(dict)));
    match outcome {
        Ok(Ok(_)) => {}
        _ => return core::ptr::null_mut(),
    }
    let id = dict_id_from_bytes(dict);
    try_box(ZSTD_CDict {
        raw: dict.to_vec(),
        id,
        level: compression_level,
        serial: next_dict_serial(),
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
    core::mem::size_of::<ZSTD_CDict>() + cdict.raw.capacity()
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
    let cdict_ref = unsafe { &*cdict };
    let src = unsafe { in_slice(src, src_size) };
    let key = cdict_ref.serial;

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // (Re)build the cached compressor when the attached dictionary or level
        // changes. A new compressor primes the dictionary into a fresh
        // dict-tier matcher; the snapshot it then captures is reused on the next
        // same-CDict call.
        if cctx.dict_compressor.is_none()
            || cctx.dict_serial != key
            || cctx.dict_level != cdict_ref.level
        {
            let mut enc: FrameCompressor =
                FrameCompressor::new(CompressionLevel::from_level(cdict_ref.level));
            // Upstream ZSTD_compress_usingCDict leaves checksum off unless the
            // CDict's params enabled it; we match the plain default.
            enc.set_content_checksum(false);
            enc.set_dictionary_from_bytes(&cdict_ref.raw)
                .map_err(|_| ())?;
            cctx.dict_compressor = Some(enc);
            cctx.dict_serial = key;
            cctx.dict_level = cdict_ref.level;
        }
        // Disjoint borrows: the cached compressor and the scratch buffer are
        // distinct fields, so split the &mut here to avoid aliasing.
        let ZSTD_CCtx {
            scratch,
            dict_compressor,
            ..
        } = cctx;
        let enc = dict_compressor.as_mut().expect("just ensured Some");
        scratch.clear();
        enc.compress_independent_frame_into(src, scratch);
        Ok::<(), ()>(())
    }));
    match outcome {
        Ok(Ok(())) => {}
        _ => {
            // A panic / parse failure can leave the cached compressor in an
            // unknown state; drop it so the next call rebuilds cleanly.
            cctx.dict_compressor = None;
            cctx.dict_serial = 0;
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
    }
    let len = cctx.scratch.len();
    if len > dst_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    dst[..len].copy_from_slice(&cctx.scratch);
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
    let id = dict_id_from_bytes(dict);
    try_box(ZSTD_DDict {
        raw: dict.to_vec(),
        id,
        serial: next_dict_serial(),
    })
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
    core::mem::size_of::<ZSTD_DDict>() + ddict.raw.capacity()
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
/// decompress a dictionary-compressed frame. The `dctx` caches the loaded
/// dictionary keyed by the `DDict`'s serial, so repeated calls with the same
/// `DDict` skip re-loading it.
///
/// # Safety
/// `dctx` / `ddict` must be live handles; `dst` / `src` valid for their lengths
/// (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompress_usingDDict(
    dctx: *mut ZSTD_DCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    ddict: *const ZSTD_DDict,
) -> usize {
    if dctx.is_null() || ddict.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    let ddict_ref = unsafe { &*ddict };
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let key = ddict_ref.serial;

    if dctx.ddict_serial != key {
        let added = catch_unwind(AssertUnwindSafe(|| {
            dctx.decoder.add_dict_from_bytes(&ddict_ref.raw)
        }));
        match added {
            Ok(Ok(())) => dctx.ddict_serial = key,
            // A clean parse failure leaves the decoder untouched, so it stays
            // reusable as-is.
            Ok(Err(_)) => return encode(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted),
            // A panic may have mutated the decoder mid-load, leaving it poisoned;
            // replace it with a fresh one and drop the stale cache key so the
            // next call re-loads cleanly rather than trusting a broken decoder.
            Err(_) => {
                dctx.decoder = codec::decoding::FrameDecoder::new();
                dctx.stream_frame_done = true;
                dctx.ddict_serial = 0;
                return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
            }
        }
    }
    dctx.decoder.set_content_checksum(ContentChecksum::Verify);
    let outcome = catch_unwind(AssertUnwindSafe(|| dctx.decoder.decode_all(src, dst)));
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
            dctx.decoder = codec::decoding::FrameDecoder::new();
            dctx.stream_frame_done = true;
            dctx.ddict_serial = 0;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}
