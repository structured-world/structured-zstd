//! Dictionary-builder API of `zdict.h`: train / finalize / inspect, and the
//! COVER and FastCOVER trainers of the `ZDICT_STATIC_LINKING_ONLY` section.
//! Wraps the codec crate's `dictionary` module.

use core::ffi::{c_char, c_int, c_uint};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::Dictionary;
use codec::dictionary::{CoverOptions, FastCoverOptions, FinalizeOptions};

use crate::error::{ZSTD_ErrorCode, encode};
use crate::ffi::in_slice;

/// Little-endian `ZSTD_MAGIC_DICTIONARY` that prefixes a valid zstd dictionary
/// (the 4 bytes after it are the dictionary ID). Read off the codec's own magic
/// rather than re-declared, so the two cannot disagree.
const DICT_MAGIC: u32 = u32::from_le_bytes(codec::decoding::DICTIONARY_MAGIC);

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

/// Run `train` on the caller's samples and copy the dictionary it returns into
/// `dict_buffer`, returning its size or an error code.
///
/// # Safety
/// `dict_buffer` valid for `dict_capacity` bytes; `samples_buffer` valid for
/// the summed `samples_sizes`; `samples_sizes` valid for `nb_samples` entries.
unsafe fn train_into(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    train: impl FnOnce(&[u8], &[usize]) -> io::Result<Vec<u8>>,
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
    let sizes: &[usize] = if nb_samples == 0 {
        &[]
    } else {
        // Non-NULL: `total_sample_len` refused a NULL array for a non-zero count.
        unsafe { core::slice::from_raw_parts(samples_sizes, nb_samples as usize) }
    };
    let dict = match catch_unwind(AssertUnwindSafe(|| train(samples, sizes))) {
        Ok(Ok(dict)) => dict,
        Ok(Err(err)) => return encode(crate::error::code_for_training_error(&err)),
        Err(_) => return encode(ZSTD_ErrorCode::ZSTD_error_dictionaryCreation_failed),
    };
    if dict.len() > dict_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let out = unsafe { crate::ffi::out_slice(dict_buffer, dict_capacity) };
    out[..dict.len()].copy_from_slice(&dict);
    dict.len()
}

/// `size_t ZDICT_trainFromBuffer(void* dictBuffer, size_t dictBufferCapacity,
/// const void* samplesBuffer, const size_t* samplesSizes, unsigned nbSamples)`.
///
/// Trains a FastCOVER dictionary as the reference does here: `d` of 8, a
/// search over `k` in four steps, scored at the default level. Writes up to
/// `dictBufferCapacity` bytes into `dictBuffer`, returning the dictionary size
/// or an error code (test with `ZDICT_isError`).
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
    unsafe {
        train_into(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            |samples, sizes| {
                codec::dictionary::optimize_fastcover_dict(
                    samples,
                    sizes,
                    dict_capacity,
                    &FastCoverOptions::default(),
                    FinalizeOptions::default(),
                )
                .map(|(dict, _)| dict)
            },
        )
    }
}

/// `ZDICT_cover_params_t` — COVER tuning, ABI-identical to `zdict.h`.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZDICT_cover_params_t {
    /// Segment size (0 = search 50..=2000 when optimizing).
    pub k: c_uint,
    /// Dmer size, at most `k` (0 = try 6 and 8 when optimizing).
    pub d: c_uint,
    /// How many values of `k` the search tries (0 = 40).
    pub steps: c_uint,
    /// Training thread count; this build trains on the calling thread.
    pub nbThreads: c_uint,
    /// Share of samples trained on, the rest scoring (0 = 1.0: all do both).
    pub splitPoint: f64,
    /// Non-zero: try the content's trailing 256, 512, ... bytes and keep the
    /// first within `shrinkDictMaxRegression` percent of the full one.
    pub shrinkDict: c_uint,
    /// Regression the shrinking search allows, in percent.
    pub shrinkDictMaxRegression: c_uint,
    pub zParams: ZDICT_params_t,
}

impl ZDICT_cover_params_t {
    fn options(&self) -> CoverOptions {
        cover_options(
            self.k,
            self.d,
            self.steps,
            self.splitPoint,
            self.shrinkDict,
            self.shrinkDictMaxRegression,
            self.zParams.compressionLevel,
        )
    }

    fn finalize(&self) -> FinalizeOptions {
        finalize_options(&self.zParams)
    }
}

/// The codec's options from the fields both parameter structs share. Zero
/// means the same "search" or "default" in both.
fn cover_options(
    k: c_uint,
    d: c_uint,
    steps: c_uint,
    split_point: f64,
    shrink: c_uint,
    max_regression: c_uint,
    level: c_int,
) -> CoverOptions {
    CoverOptions {
        k,
        d,
        steps,
        split_point,
        shrink: (shrink != 0).then_some(max_regression),
        level,
    }
}

fn finalize_options(params: &ZDICT_params_t) -> FinalizeOptions {
    FinalizeOptions {
        dict_id: (params.dictID != 0).then_some(params.dictID),
    }
}

/// `size_t ZDICT_trainFromBuffer_cover(void* dictBuffer, size_t
/// dictBufferCapacity, const void* samplesBuffer, const size_t* samplesSizes,
/// unsigned nbSamples, ZDICT_cover_params_t parameters)` — COVER training with
/// the `k` and `d` given; every sample builds. `shrinkDict` takes effect.
///
/// # Safety
/// Buffer contracts of [`ZDICT_trainFromBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_trainFromBuffer_cover(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    parameters: ZDICT_cover_params_t,
) -> usize {
    unsafe {
        train_into(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            |samples, sizes| {
                codec::dictionary::train_cover_dict(
                    samples,
                    sizes,
                    dict_capacity,
                    &parameters.options(),
                    parameters.finalize(),
                )
            },
        )
    }
}

/// `size_t ZDICT_optimizeTrainFromBuffer_cover(void* dictBuffer, size_t
/// dictBufferCapacity, const void* samplesBuffer, const size_t* samplesSizes,
/// unsigned nbSamples, ZDICT_cover_params_t* parameters)` — COVER training
/// over a search of `k` and `d`, keeping the dictionary the scoring samples
/// compress best with. The chosen `k`, `d`, `steps` and `splitPoint` are
/// written back into `parameters` on success. `shrinkDict` takes effect here,
/// which the reference's optimizer ignores.
///
/// # Safety
/// Buffer contracts of [`ZDICT_trainFromBuffer`]; `parameters` must be a
/// valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZDICT_optimizeTrainFromBuffer_cover(
    dict_buffer: *mut u8,
    dict_capacity: usize,
    samples_buffer: *const u8,
    samples_sizes: *const usize,
    nb_samples: c_uint,
    parameters: *mut ZDICT_cover_params_t,
) -> usize {
    if parameters.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let params = unsafe { *parameters };
    let mut chosen = None;
    let written = unsafe {
        train_into(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            |samples, sizes| {
                let (dict, options) = codec::dictionary::optimize_cover_dict(
                    samples,
                    sizes,
                    dict_capacity,
                    &params.options(),
                    params.finalize(),
                )?;
                chosen = Some(options);
                Ok(dict)
            },
        )
    };
    if let (false, Some(options)) = (crate::error::result_is_error(written), chosen) {
        let p = unsafe { &mut *parameters };
        p.k = options.k;
        p.d = options.d;
        p.steps = options.steps;
        p.splitPoint = options.split_point;
    }
    written
}

/// `ZDICT_fastCover_params_t` — FastCOVER tuning, ABI-identical to `zdict.h`.
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZDICT_fastCover_params_t {
    /// Segment size (0 = optimize over the default candidate grid).
    pub k: c_uint,
    /// Dmer size (0 = optimize). Upstream accepts only 6 and 8 here; this
    /// build trains any `d` of 4 or more, as its Rust trainer does, so a
    /// caller asking for another size gets a dictionary rather than
    /// `parameter_outOfBound`.
    pub d: c_uint,
    /// Frequency-array log size (0 = default 20).
    pub f: c_uint,
    /// How many values of `k` the search tries (0 = 40).
    pub steps: c_uint,
    /// Training thread count; this build trains on the calling thread.
    pub nbThreads: c_uint,
    /// Share of samples trained on, the rest scoring (0 = 0.75).
    pub splitPoint: f64,
    /// Acceleration factor (0 = default 1).
    pub accel: c_uint,
    /// Non-zero: try the content's trailing 256, 512, ... bytes and keep the
    /// first within `shrinkDictMaxRegression` percent of the full one.
    pub shrinkDict: c_uint,
    /// Regression the shrinking search allows, in percent.
    pub shrinkDictMaxRegression: c_uint,
    pub zParams: ZDICT_params_t,
}

impl ZDICT_fastCover_params_t {
    fn options(&self) -> FastCoverOptions {
        FastCoverOptions {
            cover: cover_options(
                self.k,
                self.d,
                self.steps,
                self.splitPoint,
                self.shrinkDict,
                self.shrinkDictMaxRegression,
                self.zParams.compressionLevel,
            ),
            f: self.f,
            accel: self.accel,
        }
    }
}

/// `size_t ZDICT_trainFromBuffer_fastCover(void* dictBuffer, size_t
/// dictBufferCapacity, const void* samplesBuffer, const size_t* samplesSizes,
/// unsigned nbSamples, ZDICT_fastCover_params_t parameters)` — FastCOVER
/// training with the `k` and `d` given; every sample builds. `shrinkDict`
/// takes effect.
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
    unsafe {
        train_into(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            |samples, sizes| {
                codec::dictionary::train_fastcover_dict(
                    samples,
                    sizes,
                    dict_capacity,
                    &parameters.options(),
                    finalize_options(&parameters.zParams),
                )
            },
        )
    }
}

/// `size_t ZDICT_optimizeTrainFromBuffer_fastCover(void* dictBuffer, size_t
/// dictBufferCapacity, const void* samplesBuffer, const size_t* samplesSizes,
/// unsigned nbSamples, ZDICT_fastCover_params_t* parameters)` — FastCOVER
/// training over a search of `k` and `d`, keeping the dictionary the scoring
/// samples compress best with. The chosen `k`, `d`, `f`, `accel`, `steps` and
/// `splitPoint` are written back into `parameters` on success. `shrinkDict`
/// takes effect here, which the reference's optimizer ignores.
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
    let mut chosen = None;
    let written = unsafe {
        train_into(
            dict_buffer,
            dict_capacity,
            samples_buffer,
            samples_sizes,
            nb_samples,
            |samples, sizes| {
                let (dict, options) = codec::dictionary::optimize_fastcover_dict(
                    samples,
                    sizes,
                    dict_capacity,
                    &params.options(),
                    finalize_options(&params.zParams),
                )?;
                chosen = Some(options);
                Ok(dict)
            },
        )
    };
    if let (false, Some(options)) = (crate::error::result_is_error(written), chosen) {
        let p = unsafe { &mut *parameters };
        p.k = options.cover.k;
        p.d = options.cover.d;
        p.f = options.f;
        p.accel = options.accel;
        p.steps = options.cover.steps;
        p.splitPoint = options.cover.split_point;
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
