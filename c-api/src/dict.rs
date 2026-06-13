//! Dictionary-builder API: the stable `ZDICTLIB_API` slice of `zdict.h`
//! (train / finalize / inspect). Wraps the codec crate's `dictionary` module
//! (FastCOVER trainer + raw-dictionary finalizer). The experimental
//! `ZDICT_STATIC_LINKING_ONLY` cover/fastcover/legacy entry points are not part
//! of the stable shared-library ABI and are intentionally not exported here.

use core::ffi::{c_char, c_int, c_uint};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::Dictionary;
use codec::dictionary::{
    FastCoverOptions, FastCoverTuned, FinalizeOptions, create_fastcover_dict_from_source,
};

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
    // A NULL buffer with a non-zero length is caller error, reported as an
    // encoded error code — it must never reach slice construction.
    if (samples_buffer.is_null() && total > 0) || (dict_buffer.is_null() && dict_capacity > 0) {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed);
    }
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

/// `ZDICT_fastCover_params_t` — FastCOVER tuning, ABI-identical to `zdict.h`.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZDICT_fastCover_params_t {
    /// Segment size (0 = optimize over the default candidate grid).
    pub k: c_uint,
    /// Dmer size (0 = optimize; only 6 / 8 are meaningful upstream).
    pub d: c_uint,
    /// Frequency-array log size (0 = default 20).
    pub f: c_uint,
    /// Optimization step count. Accepted for ABI compatibility but not a
    /// sweep bound here: upstream uses `steps` to budget how many (k, d)
    /// pairs its optimizer tries, while this trainer always sweeps the full
    /// candidate grid (a superset of any step budget), so honouring a
    /// smaller `steps` could only degrade the chosen dictionary.
    pub steps: c_uint,
    /// Training thread count; this build trains on the calling thread.
    pub nbThreads: c_uint,
    /// Fraction of samples used for training vs testing (0.0 = default).
    pub splitPoint: f64,
    /// Acceleration factor (0 = default 1).
    pub accel: c_uint,
    /// Shrink-dict toggle (accepted; the trainer always sizes to capacity).
    pub shrinkDict: c_uint,
    /// Shrink-dict regression bound (accepted with `shrinkDict`).
    pub shrinkDictMaxRegression: c_uint,
    pub zParams: ZDICT_params_t,
}

/// Map caller FastCOVER parameters onto the trainer's options. `optimize`
/// distinguishes the plain train entry (explicit k/d required upstream) from
/// the optimizing entry (0 = sweep the candidate grid).
fn fastcover_options(params: &ZDICT_fastCover_params_t, optimize: bool) -> FastCoverOptions {
    let mut opts = FastCoverOptions {
        optimize,
        ..FastCoverOptions::default()
    };
    if params.k != 0 {
        opts.k = params.k as usize;
        if optimize {
            opts.k_candidates = vec![params.k as usize];
        }
    }
    if params.d != 0 {
        opts.d = params.d as usize;
        if optimize {
            opts.d_candidates = vec![params.d as usize];
        }
    }
    if params.f != 0 {
        opts.f = params.f;
        if optimize {
            opts.f_candidates = vec![params.f];
        }
    }
    if params.accel != 0 {
        opts.accel = params.accel as usize;
    }
    if params.splitPoint > 0.0 {
        opts.split_point = params.splitPoint;
    }
    opts
}

/// Shared body of the FastCOVER train entry points.
///
/// # Safety
/// Buffer contracts of [`ZDICT_trainFromBuffer`].
/// Sweep mode for [`train_fastcover`]: plain training pins the caller's
/// explicit parameters; optimizing reports the sweep's winner back.
enum FastCoverMode<'a> {
    Plain,
    Optimize(&'a mut FastCoverTuned),
}

unsafe fn train_fastcover(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    params: &ZDICT_fastCover_params_t,
    mode: FastCoverMode<'_>,
) -> usize {
    let Some(total) = (unsafe { total_sample_len(samples_sizes, nb_samples) }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed);
    };
    // NULL + non-zero length is caller error, not a slice to build.
    if (samples_buffer.is_null() && total > 0) || (dict_buffer.is_null() && dict_capacity > 0) {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed);
    }
    let samples = unsafe { in_slice(samples_buffer, total) };
    let finalize = FinalizeOptions {
        dict_id: (params.zParams.dictID != 0).then_some(params.zParams.dictID),
    };
    let optimize = matches!(mode, FastCoverMode::Optimize(_));
    let opts = fastcover_options(params, optimize);

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        create_fastcover_dict_from_source(samples, &mut out, dict_capacity, &opts, finalize)
            .map(|tuned| (out, tuned))
    }));
    let (dict, tuned) = match outcome {
        Ok(Ok(pair)) => pair,
        _ => return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed),
    };
    if let FastCoverMode::Optimize(slot) = mode {
        *slot = tuned;
    }
    if dict.len() > dict_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let out = unsafe { crate::ffi::out_slice(dict_buffer, dict_capacity) };
    out[..dict.len()].copy_from_slice(&dict);
    dict.len()
}

/// `size_t ZDICT_trainFromBuffer_fastCover(void* dictBuffer, size_t
/// dictBufferCapacity, const void* samplesBuffer, const size_t* samplesSizes,
/// unsigned nbSamples, ZDICT_fastCover_params_t parameters)` — FastCOVER
/// training with explicit parameters (no optimization sweep).
///
/// # Safety
/// Buffer contracts of [`ZDICT_trainFromBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_trainFromBuffer_fastCover(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    parameters: ZDICT_fastCover_params_t,
) -> usize {
    // The non-optimizing entry requires explicit segment/dmer sizes:
    // upstream rejects 0 here (the optimizing entry is the one that sweeps
    // defaults), so silently substituting the candidate-grid defaults would
    // accept invalid input.
    if parameters.k == 0 || parameters.d == 0 {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    unsafe {
        train_fastcover(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            &parameters,
            FastCoverMode::Plain,
        )
    }
}

/// `size_t ZDICT_optimizeTrainFromBuffer_fastCover(void* dictBuffer, size_t
/// dictBufferCapacity, const void* samplesBuffer, const size_t* samplesSizes,
/// unsigned nbSamples, ZDICT_fastCover_params_t* parameters)` — FastCOVER
/// training with a parameter sweep over the candidate grid; explicit non-zero
/// `k` / `d` / `f` pin that axis. The chosen values are written back into
/// `parameters` on success, per upstream.
///
/// # Safety
/// Buffer contracts of [`ZDICT_trainFromBuffer`]; `parameters` must be a
/// valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_optimizeTrainFromBuffer_fastCover(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    parameters: *mut ZDICT_fastCover_params_t,
) -> usize {
    if parameters.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let params = unsafe { *parameters };
    let mut tuned = FastCoverTuned {
        k: 0,
        d: 0,
        f: 0,
        accel: 0,
        score: 0,
    };
    let written = unsafe {
        train_fastcover(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            &params,
            FastCoverMode::Optimize(&mut tuned),
        )
    };
    if !crate::error::result_is_error(written) {
        // Write back the sweep's actual winner (upstream semantics).
        let p = unsafe { &mut *parameters };
        p.k = tuned.k as c_uint;
        p.d = tuned.d as c_uint;
        p.f = tuned.f;
        p.accel = tuned.accel as c_uint;
    }
    written
}

/// `size_t ZDICT_finalizeDictionary(void* dstDictBuffer, size_t maxDictSize,
/// const void* dictContent, size_t dictContentSize, const void* samplesBuffer,
/// const size_t* samplesSizes, unsigned nbSamples, ZDICT_params_t parameters)`.
///
/// Wraps raw `dictContent` (plus entropy tables analysed from the samples) into
/// a full zstd dictionary, writing up to `maxDictSize` bytes into
/// `dstDictBuffer`. Returns the dictionary size or an error code.
///
/// Of `parameters`, only `dictID` is honoured (0 derives a compliant ID). The
/// FastCOVER finalizer builds the entropy tables directly from the samples, so
/// `compressionLevel` does not tune them, and `notificationLevel` (builder
/// verbosity) has no effect here; both are accepted for ABI compatibility.
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
    // NULL + non-zero length is caller error, not a slice to build.
    if (samples_buffer.is_null() && total > 0)
        || (dict_content.is_null() && dict_content_size > 0)
        || (dst_dict_buffer.is_null() && max_dict_size > 0)
    {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed);
    }
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
