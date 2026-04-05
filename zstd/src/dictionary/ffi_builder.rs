use std::ffi::CStr;
use std::io::{self, Write};
use std::string::String;
use std::vec;
use std::vec::Vec;

use zstd_sys::{
    ZDICT_DICTSIZE_MIN, ZDICT_fastCover_params_t, ZDICT_finalizeDictionary,
    ZDICT_getDictID, ZDICT_getErrorName, ZDICT_isError,
    ZDICT_optimizeTrainFromBuffer_cover, ZDICT_optimizeTrainFromBuffer_fastCover,
    ZDICT_params_t, ZDICT_trainFromBuffer_cover, ZDICT_trainFromBuffer_fastCover,
    ZDICT_cover_params_t,
};

/// Dictionary training algorithm used by the zstd C dictionary builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrainingAlgorithm {
    Cover,
    FastCover,
}

/// Builder parameters used for COVER/FastCOVER dictionary training.
#[derive(Debug, Clone, Copy)]
pub struct TrainingParams {
    pub algorithm: TrainingAlgorithm,
    pub optimize: bool,
    pub k: u32,
    pub d: u32,
    pub f: u32,
    pub steps: u32,
    pub accel: u32,
    pub nb_threads: u32,
    pub split_point: f64,
    pub compression_level: i32,
    pub dict_id: Option<u32>,
    pub notification_level: u32,
}

impl Default for TrainingParams {
    fn default() -> Self {
        Self {
            algorithm: TrainingAlgorithm::FastCover,
            optimize: true,
            k: 0,
            d: 0,
            f: 0,
            steps: 0,
            accel: 0,
            nb_threads: 0,
            split_point: 0.0,
            compression_level: 0,
            dict_id: None,
            notification_level: 0,
        }
    }
}

/// Parameters for finalizing a raw-content dictionary into full zstd dictionary format.
#[derive(Debug, Clone, Copy, Default)]
pub struct FinalizeParams {
    pub compression_level: i32,
    pub dict_id: Option<u32>,
    pub notification_level: u32,
}

/// Result metadata for FFI dictionary training.
#[derive(Debug, Clone, Copy)]
pub struct TrainedDictionaryInfo {
    pub dict_size: usize,
    pub dict_id: u32,
    pub selected_k: u32,
    pub selected_d: u32,
    pub selected_f: u32,
    pub selected_steps: u32,
    pub selected_accel: u32,
}

fn validate_samples(sample_data: &[u8], sample_sizes: &[usize]) -> io::Result<()> {
    if sample_sizes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dictionary training requires at least one sample",
        ));
    }
    if sample_sizes.iter().sum::<usize>() != sample_data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sample sizes must sum to sample_data length",
        ));
    }
    Ok(())
}

fn parse_zdict_code(code: usize) -> io::Result<usize> {
    // SAFETY: ZDICT_isError and ZDICT_getErrorName are pure functions over the code value.
    let is_err = unsafe { ZDICT_isError(code) } != 0;
    if !is_err {
        return Ok(code);
    }
    // SAFETY: When ZDICT_isError(code) is non-zero, zstd returns a static error string.
    let msg = unsafe {
        let ptr = ZDICT_getErrorName(code);
        if ptr.is_null() {
            String::from("zstd dictionary builder error")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    Err(io::Error::other(msg))
}

fn zparams(compression_level: i32, dict_id: Option<u32>, notification_level: u32) -> ZDICT_params_t {
    ZDICT_params_t {
        compressionLevel: compression_level,
        notificationLevel: notification_level,
        dictID: dict_id.unwrap_or(0),
    }
}

/// Train a finalized zstd dictionary from contiguous sample data with explicit sample sizes.
pub fn train_dictionary_ffi<W: Write>(
    sample_data: &[u8],
    sample_sizes: &[usize],
    output: &mut W,
    dict_size: usize,
    params: TrainingParams,
) -> io::Result<TrainedDictionaryInfo> {
    validate_samples(sample_data, sample_sizes)?;
    if !params.optimize && (params.k == 0 || params.d == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "k and d must be set when optimize=false",
        ));
    }

    let max_size = dict_size.max(ZDICT_DICTSIZE_MIN as usize);
    let mut dict_buf = vec![0u8; max_size];
    let z_params = zparams(
        params.compression_level,
        params.dict_id,
        params.notification_level,
    );

    let (written, selected_k, selected_d, selected_f, selected_steps, selected_accel) =
        match params.algorithm {
        TrainingAlgorithm::Cover => {
            let mut cover = ZDICT_cover_params_t {
                k: params.k,
                d: params.d,
                steps: params.steps,
                nbThreads: params.nb_threads,
                splitPoint: params.split_point,
                shrinkDict: 0,
                shrinkDictMaxRegression: 0,
                zParams: z_params,
            };
            let rc = if params.optimize {
                // SAFETY: All pointers remain valid for the duration of the call.
                unsafe {
                    ZDICT_optimizeTrainFromBuffer_cover(
                        dict_buf.as_mut_ptr().cast(),
                        dict_buf.len(),
                        sample_data.as_ptr().cast(),
                        sample_sizes.as_ptr(),
                        sample_sizes.len() as u32,
                        &mut cover,
                    )
                }
            } else {
                // SAFETY: All pointers remain valid for the duration of the call.
                unsafe {
                    ZDICT_trainFromBuffer_cover(
                        dict_buf.as_mut_ptr().cast(),
                        dict_buf.len(),
                        sample_data.as_ptr().cast(),
                        sample_sizes.as_ptr(),
                        sample_sizes.len() as u32,
                        cover,
                    )
                }
            };
            (parse_zdict_code(rc)?, cover.k, cover.d, 0, cover.steps, 0)
        }
        TrainingAlgorithm::FastCover => {
            let mut fast = ZDICT_fastCover_params_t {
                k: params.k,
                d: params.d,
                f: params.f,
                steps: params.steps,
                nbThreads: params.nb_threads,
                splitPoint: params.split_point,
                accel: params.accel,
                shrinkDict: 0,
                shrinkDictMaxRegression: 0,
                zParams: z_params,
            };
            let rc = if params.optimize {
                // SAFETY: All pointers remain valid for the duration of the call.
                unsafe {
                    ZDICT_optimizeTrainFromBuffer_fastCover(
                        dict_buf.as_mut_ptr().cast(),
                        dict_buf.len(),
                        sample_data.as_ptr().cast(),
                        sample_sizes.as_ptr(),
                        sample_sizes.len() as u32,
                        &mut fast,
                    )
                }
            } else {
                // SAFETY: All pointers remain valid for the duration of the call.
                unsafe {
                    ZDICT_trainFromBuffer_fastCover(
                        dict_buf.as_mut_ptr().cast(),
                        dict_buf.len(),
                        sample_data.as_ptr().cast(),
                        sample_sizes.as_ptr(),
                        sample_sizes.len() as u32,
                        fast,
                    )
                }
            };
            (
                parse_zdict_code(rc)?,
                fast.k,
                fast.d,
                fast.f,
                fast.steps,
                fast.accel,
            )
        }
    };
    output.write_all(&dict_buf[..written])?;
    // SAFETY: dictionary slice is valid and length is from zstd return code.
    let dict_id = unsafe { ZDICT_getDictID(dict_buf.as_ptr().cast(), written) };
    Ok(TrainedDictionaryInfo {
        dict_size: written,
        dict_id,
        selected_k,
        selected_d,
        selected_f,
        selected_steps,
        selected_accel,
    })
}

/// Convenience helper: train dictionary from individual samples.
pub fn train_dictionary_from_samples_ffi<S: AsRef<[u8]>, W: Write>(
    samples: &[S],
    output: &mut W,
    dict_size: usize,
    params: TrainingParams,
) -> io::Result<TrainedDictionaryInfo> {
    let total_len = samples.iter().map(|s| s.as_ref().len()).sum();
    let mut data = Vec::with_capacity(total_len);
    let mut sizes = Vec::with_capacity(samples.len());
    for sample in samples {
        let sample = sample.as_ref();
        sizes.push(sample.len());
        data.extend_from_slice(sample);
    }
    train_dictionary_ffi(&data, &sizes, output, dict_size, params)
}

/// Finalize a raw-content dictionary into full zstd dictionary format with entropy tables.
pub fn finalize_raw_dictionary_ffi<W: Write>(
    raw_content: &[u8],
    sample_data: &[u8],
    sample_sizes: &[usize],
    output: &mut W,
    dict_size: usize,
    params: FinalizeParams,
) -> io::Result<TrainedDictionaryInfo> {
    validate_samples(sample_data, sample_sizes)?;
    if raw_content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw dictionary content must not be empty",
        ));
    }

    let max_size = dict_size
        .max(raw_content.len())
        .max(ZDICT_DICTSIZE_MIN as usize);
    let mut dict_buf = vec![0u8; max_size];
    let z_params = zparams(
        params.compression_level,
        params.dict_id,
        params.notification_level,
    );

    // SAFETY: All pointers remain valid for the duration of the call.
    let rc = unsafe {
        ZDICT_finalizeDictionary(
            dict_buf.as_mut_ptr().cast(),
            dict_buf.len(),
            raw_content.as_ptr().cast(),
            raw_content.len(),
            sample_data.as_ptr().cast(),
            sample_sizes.as_ptr(),
            sample_sizes.len() as u32,
            z_params,
        )
    };
    let written = parse_zdict_code(rc)?;
    output.write_all(&dict_buf[..written])?;
    // SAFETY: dictionary slice is valid and length is from zstd return code.
    let dict_id = unsafe { ZDICT_getDictID(dict_buf.as_ptr().cast(), written) };
    Ok(TrainedDictionaryInfo {
        dict_size: written,
        dict_id,
        selected_k: 0,
        selected_d: 0,
        selected_f: 0,
        selected_steps: 0,
        selected_accel: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoding::Dictionary;
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use std::format;
    use std::io::Cursor;

    fn sample_corpus() -> (Vec<u8>, Vec<usize>) {
        let mut sizes = Vec::new();
        let mut data = Vec::new();
        for i in 0..400u32 {
            let sample = format!("tenant=demo table=orders key={i} region=eu value=aaaaabbbbbccccc\n");
            sizes.push(sample.len());
            data.extend_from_slice(sample.as_bytes());
        }
        (data, sizes)
    }

    #[test]
    fn fastcover_training_produces_decodeable_dictionary() {
        let (sample_data, sample_sizes) = sample_corpus();
        let mut dict = Vec::new();
        let info = train_dictionary_ffi(
            &sample_data,
            &sample_sizes,
            &mut dict,
            4096,
            TrainingParams::default(),
        )
        .expect("fastcover training should succeed");
        assert!(info.dict_size > 0);
        let parsed = Dictionary::decode_dict(dict.as_slice()).expect("trained dict should decode");
        assert_eq!(parsed.id, info.dict_id);
        assert!(!parsed.dict_content.is_empty());
    }

    #[test]
    fn finalize_raw_dict_is_compatible_with_ffi_decoder() {
        let (sample_data, sample_sizes) = sample_corpus();

        let mut raw = Vec::new();
        crate::dictionary::create_raw_dict_from_source(
            Cursor::new(sample_data.as_slice()),
            sample_data.len(),
            &mut raw,
            4096,
        );
        assert!(!raw.is_empty(), "raw dictionary must not be empty");

        let mut finalized = Vec::new();
        let info = finalize_raw_dictionary_ffi(
            raw.as_slice(),
            &sample_data,
            &sample_sizes,
            &mut finalized,
            4096,
            FinalizeParams::default(),
        )
        .expect("finalization should succeed");
        assert!(info.dict_size > 0);

        let parsed = Dictionary::decode_dict(finalized.as_slice())
            .expect("finalized dictionary should decode");

        let mut payload = Vec::new();
        for idx in 0..128u32 {
            payload.extend_from_slice(
                format!("tenant=demo table=orders op=put key={idx} value=aaaaabbbbbcccccdddddeeeee\n")
                    .as_bytes(),
            );
        }

        let mut compressed = Vec::new();
        let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
        compressor
            .set_dictionary(parsed)
            .expect("dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let mut ffi_decoder = zstd::bulk::Decompressor::with_dictionary(finalized.as_slice())
            .expect("ffi decoder should accept finalized dictionary");
        let mut decoded = Vec::with_capacity(payload.len());
        let written = ffi_decoder
            .decompress_to_buffer(compressed.as_slice(), &mut decoded)
            .expect("ffi decoder should decode payload");
        assert_eq!(written, payload.len());
        assert_eq!(decoded, payload);
    }
}
