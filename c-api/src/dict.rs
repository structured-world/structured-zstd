//! Dictionary-builder API: the stable `ZDICTLIB_API` slice of `zdict.h`
//! (train / finalize / inspect). Wraps the codec crate's `dictionary` module
//! (FastCOVER trainer + raw-dictionary finalizer). The experimental
//! `ZDICT_STATIC_LINKING_ONLY` cover/fastcover/legacy entry points are not part
//! of the stable shared-library ABI and are intentionally not exported here.

use core::ffi::{c_char, c_int, c_uint};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::Dictionary;
use codec::dictionary::{FastCoverOptions, FinalizeOptions, create_fastcover_dict_from_source};

use crate::error::{ZSTD_ErrorCode, encode};
use crate::ffi::in_slice;

/// Little-endian `ZSTD_MAGIC_DICTIONARY` (0xEC30A437) that prefixes a valid
/// zstd dictionary; the 4 bytes after it are the dictionary ID.
const DICT_MAGIC: u32 = 0xEC30_A437;

/// `ZDICT_params_t` — finalize parameters, ABI-identical to `zdict.h`.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_snake_case)]
pub struct ZDICT_params_t {
    /// Compression level used while analysing the samples (0 = codec default).
    pub compressionLevel: c_int,
    /// Verbosity of the (no-op here) builder logging.
    pub notificationLevel: c_uint,
    /// Forced dictionary ID; 0 lets the builder derive a compliant one.
    pub dictID: c_uint,
}

/// Sum the per-sample sizes, returning `None` on overflow or a NULL array.
///
/// # Safety
/// `samples_sizes` must be valid for `nb_samples` `usize` reads, or NULL.
unsafe fn total_sample_len(samples_sizes: *const usize, nb_samples: c_uint) -> Option<usize> {
    if nb_samples == 0 {
        return Some(0);
    }
    if samples_sizes.is_null() {
        return None;
    }
    let sizes = unsafe { core::slice::from_raw_parts(samples_sizes, nb_samples as usize) };
    let mut total: usize = 0;
    for &s in sizes {
        total = total.checked_add(s)?;
    }
    Some(total)
}

/// `size_t ZDICT_trainFromBuffer(void* dictBuffer, size_t dictBufferCapacity,
/// const void* samplesBuffer, const size_t* samplesSizes, unsigned nbSamples)`.
///
/// Trains a dictionary (FastCOVER) from the concatenated `samplesBuffer` and
/// writes up to `dictBufferCapacity` bytes into `dictBuffer`, returning the
/// dictionary size or an error code (test with `ZDICT_isError`).
///
/// # Safety
/// `dictBuffer` valid for `dictBufferCapacity` bytes; `samplesBuffer` valid for
/// the summed `samplesSizes`; `samplesSizes` valid for `nbSamples` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_trainFromBuffer(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
) -> usize {
    let Some(total) = (unsafe { total_sample_len(samples_sizes, nb_samples) }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed);
    };
    let samples = unsafe { in_slice(samples_buffer, total) };

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        create_fastcover_dict_from_source(
            samples,
            &mut out,
            dict_capacity,
            &FastCoverOptions::default(),
            FinalizeOptions::default(),
        )
        .map(|_| out)
    }));
    let dict = match outcome {
        Ok(Ok(dict)) => dict,
        _ => return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed),
    };
    if dict.len() > dict_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let out = unsafe { crate::ffi::out_slice(dict_buffer, dict_capacity) };
    out[..dict.len()].copy_from_slice(&dict);
    dict.len()
}

/// `size_t ZDICT_finalizeDictionary(void* dstDictBuffer, size_t maxDictSize,
/// const void* dictContent, size_t dictContentSize, const void* samplesBuffer,
/// const size_t* samplesSizes, unsigned nbSamples, ZDICT_params_t parameters)`.
///
/// Wraps raw `dictContent` (plus entropy tables analysed from the samples) into
/// a full zstd dictionary, writing up to `maxDictSize` bytes into
/// `dstDictBuffer`. Returns the dictionary size or an error code.
///
/// # Safety
/// All buffers valid for their stated lengths; `samplesSizes` valid for
/// `nbSamples` entries.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ZDICT_finalizeDictionary(
    dst_dict_buffer: *mut u8,
    max_dict_size: usize,
    dict_content: *const u8,
    dict_content_size: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    parameters: ZDICT_params_t,
) -> usize {
    let Some(total) = (unsafe { total_sample_len(samples_sizes, nb_samples) }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed);
    };
    let content = unsafe { in_slice(dict_content, dict_content_size) };
    let samples = unsafe { in_slice(samples_buffer, total) };
    // dictID 0 means "derive a compliant id"; any non-zero value is forced.
    let finalize = FinalizeOptions {
        dict_id: (parameters.dictID != 0).then_some(parameters.dictID),
    };

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        codec::dictionary::finalize_raw_dict(content, samples, max_dict_size, finalize)
    }));
    let dict = match outcome {
        Ok(Ok(dict)) => dict,
        _ => return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed),
    };
    if dict.len() > max_dict_size {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let out = unsafe { crate::ffi::out_slice(dst_dict_buffer, max_dict_size) };
    out[..dict.len()].copy_from_slice(&dict);
    dict.len()
}

/// `unsigned ZDICT_getDictID(const void* dictBuffer, size_t dictSize)` — the
/// dictionary ID from the header, or 0 if `dictBuffer` is not a valid
/// dictionary (bad magic or too short).
///
/// # Safety
/// `dictBuffer` must be valid for `dictSize` bytes (or NULL with `dictSize == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_getDictID(dict_buffer: *const u8, dict_size: usize) -> c_uint {
    let dict = unsafe { in_slice(dict_buffer, dict_size) };
    if dict.len() < 8 {
        return 0;
    }
    if u32::from_le_bytes(dict[..4].try_into().expect("4 bytes")) != DICT_MAGIC {
        return 0;
    }
    u32::from_le_bytes(dict[4..8].try_into().expect("4 bytes"))
}

/// `size_t ZDICT_getDictHeaderSize(const void* dictBuffer, size_t dictSize)` —
/// the dictionary header length (everything before the raw content), or a ZSTD
/// error code on a malformed dictionary.
///
/// # Safety
/// `dictBuffer` must be valid for `dictSize` bytes (or NULL with `dictSize == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_getDictHeaderSize(
    dict_buffer: *const u8,
    dict_size: usize,
) -> usize {
    let dict = unsafe { in_slice(dict_buffer, dict_size) };
    let outcome = catch_unwind(AssertUnwindSafe(|| Dictionary::decode_dict(dict)));
    match outcome {
        // Header size is the prefix before the raw content (magic + id +
        // entropy tables + offset history): total minus the content length.
        Ok(Ok(parsed)) => dict_size - parsed.dict_content.len(),
        _ => encode(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted),
    }
}

/// `unsigned ZDICT_isError(size_t errorCode)` — non-zero iff `errorCode` is an
/// error. ZDICT shares ZSTD's `size_t` error encoding.
#[unsafe(no_mangle)]
pub extern "C" fn ZDICT_isError(error_code: usize) -> c_uint {
    crate::error::ZSTD_isError(error_code)
}

/// `const char* ZDICT_getErrorName(size_t errorCode)` — readable string for a
/// ZDICT error code (same table as ZSTD).
#[unsafe(no_mangle)]
pub extern "C" fn ZDICT_getErrorName(error_code: usize) -> *const c_char {
    crate::error::ZSTD_getErrorName(error_code)
}
