//! Code for creating a separate content dictionary.
//!
//! Effective dictionaries are up to 1% the size of the complete training body,
//! and are trained on many examples of the original data.
//!
//! Implemented following the paper "Effective construction of
//! Relative Lempel-Ziv Dictionaries", by Kewen Liao, Matthias Petri,
//! Alistair Moffat, and Anthony Wirth

// The algorithm is summarized here
// 1. The text is split into "epochs", or chunks from the original source
// 2. From within each epoch, we select the "segment", or 1 KiB contiguous section
//    that's predicted to be the best option to include in the dictionary. Concatenated,
//    these segments form the dictionary.
//
// This segment scoring algorithm operates as follows:
// For a given epoch:
//  - Run a reservoir sampler over the entire epoch, creating a
//    reservoir of n/t, where `t` is the desired number of occurrences
//    we want the most common k-mers to have
//  - Have the ability to estimate
//    the frequency of a given k-mer: `f(w: k-mer)` calculates
//    the frequency of w in the reservoir using a rolling karp-rabin hash
//  - The score of a segment is the sum of `f(w)` called on every kmer within the segment
mod cover;
mod fastcover;
mod frequency;
mod reservoir;

use crate::bit_io::BitWriter;
use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};
use crate::decoding::dictionary::MAGIC_NUM as DICT_MAGIC_NUM;
use crate::decoding::sequence_section_decoder::{LL_MAX_LOG, ML_MAX_LOG, OF_MAX_LOG};
use crate::dictionary::reservoir::create_sample;
use crate::fse::fse_encoder::{self, build_table_from_data};
use crate::huff0::HuffmanTable as HuffmanDecoderTable;
use crate::huff0::huff0_encoder::{HuffmanEncoder, HuffmanTable as HuffmanEncoderTable};
use core::cmp::Reverse;
use cover::*;
pub use fastcover::{
    DEFAULT_D_CANDIDATES, DEFAULT_F_CANDIDATES, DEFAULT_K_CANDIDATES, FastCoverParams,
    FastCoverTuned,
};
use std::{
    boxed::Box,
    collections::{BinaryHeap, HashMap},
    format,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    vec::Vec,
};

const MAX_TRAINING_PREALLOC_BYTES: usize = 8 * 1024 * 1024;
const MAX_HUFFMAN_STATS_BYTES: usize = 64 * 1024;

/// Tuning knobs for pure-Rust FastCOVER training.
#[derive(Debug, Clone)]
pub struct FastCoverOptions {
    pub optimize: bool,
    pub split_point: f64,
    pub accel: usize,
    pub k: usize,
    pub d: usize,
    pub f: u32,
    pub k_candidates: Vec<usize>,
    pub d_candidates: Vec<usize>,
    pub f_candidates: Vec<u32>,
}

impl Default for FastCoverOptions {
    fn default() -> Self {
        Self {
            optimize: true,
            split_point: 0.75,
            accel: 1,
            k: 256,
            d: 8,
            f: 20,
            k_candidates: DEFAULT_K_CANDIDATES.to_vec(),
            d_candidates: DEFAULT_D_CANDIDATES.to_vec(),
            f_candidates: DEFAULT_F_CANDIDATES.to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FinalizeOptions {
    pub dict_id: Option<u32>,
}

/// A set of values that are used during dictionary construction.
///
/// Changing these values can improve the resulting dictionary size for certain datasets.
// TODO: move `k` here.
pub(super) struct DictParams {
    /// Segment size.
    ///
    /// As found under "4. Experiments - Varying Segment Size" in the original paper, a
    /// segment size of 2 kiB was effective.
    ///
    /// "We explored a range of \[`segment_size`\] values and found the performance of LMC is insensitive
    /// to \[`segment_size`\]. We fix \[`segment_size`\] to 2kiB
    ///
    /// Reasonable range: [16, 2048+]
    pub segment_size: u32,
}

/// Creates a "raw content" dictionary, training off of every file in this directory and all
/// sub-directories.
///
/// The resulting dictionary will be approxamitely `dict_size` or less, and written to `output`.
///
/// # Errors
/// This function returns `Ok(())` if the dictionary was created successfully, and an
/// `Err(io::Error)` if an error was encountered reading the input directory or
/// writing dictionary bytes to `output`.
///
/// # Examples
/// ```no_run
/// use std::fs::File;
/// // Create a roughly 1mb dictionary, training off of file in `sample_files`
/// let input_folder = "sample_files/";
/// let mut output = File::create("output.dict").unwrap();
/// structured_zstd::dictionary::create_raw_dict_from_dir(input_folder, &mut output, 1_000_000)
///     .expect("dictionary training from sample_files should succeed");
/// ```
pub fn create_raw_dict_from_dir<P: AsRef<Path>, W: io::Write>(
    path: P,
    output: &mut W,
    dict_size: usize,
) -> Result<(), io::Error> {
    // Collect a list of a path to every file in the directory into `file_paths`
    let mut file_paths: Vec<PathBuf> = Vec::new();
    let dir: fs::ReadDir = fs::read_dir(path)?;
    fn recurse_read(dir: fs::ReadDir, file_paths: &mut Vec<PathBuf>) -> Result<(), io::Error> {
        for entry in dir {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                recurse_read(fs::read_dir(entry.path())?, file_paths)?;
            } else {
                file_paths.push(entry.path());
            }
        }
        Ok(())
    }
    recurse_read(dir, &mut file_paths)?;

    // Open each file and chain the readers together
    let mut total_file_len: u64 = 0;
    let mut file_handles: Vec<fs::File> = Vec::new();
    for path in file_paths {
        let handle = File::open(path)?;
        total_file_len += handle.metadata()?.len();
        file_handles.push(handle);
    }
    let empty_reader: Box<dyn Read> = Box::new(io::empty());
    let chained_files = file_handles
        .iter()
        .fold(empty_reader, |acc, reader| Box::new(acc.chain(reader)));

    // Create a dict using the new reader
    create_raw_dict_from_source(chained_files, total_file_len as usize, output, dict_size)?;
    Ok(())
}

/// Read from `source` to create a "raw content" dictionary of `dict_size`.
/// The completed dictionary is written to `output`.
///
/// - `source` will be used as training data for the entire dictionary.
/// - `source_size` is used only as a preallocation hint before reading `source` and
///   does not affect sampling once all data has been buffered.
/// - `output` is where the completed dictionary will be written.
/// - `dict_size` determines how large the complete dictionary should be. The completed
///   dictionary will be this size or smaller.
///
/// This function reads the entire `source` into an in-memory `Vec<u8>` before building
/// the dictionary. The provided reader need not be buffered, but callers should avoid
/// sources too large to fit comfortably in memory.
pub fn create_raw_dict_from_source<R: io::Read, W: io::Write>(
    mut source: R,
    source_size: usize,
    output: &mut W,
    dict_size: usize,
) -> io::Result<()> {
    if dict_size == 0 {
        return Ok(());
    }
    let prealloc = source_size.min(MAX_TRAINING_PREALLOC_BYTES);
    let mut all = Vec::with_capacity(prealloc);
    source.read_to_end(&mut all)?;
    if all.is_empty() {
        return Ok(());
    }

    if all.len() < K {
        let keep = usize::min(all.len(), dict_size);
        output.write_all(&all[all.len() - keep..])?;
        return Ok(());
    }

    let source_size = all.len();
    vprintln!("create_dict: creating {dict_size} byte dict from {source_size} byte source");

    let params = DictParams { segment_size: 2048 };
    let num_segments = usize::max(1, source_size / params.segment_size as usize);
    // According to 4. Experiments - Varying Reservoir Sampler Thresholds,
    // setting reservoir size to collection size / min{collection size / (2 * number of segments),
    // 256} was effective
    let denom = usize::max(1, source_size / (2 * num_segments));
    let sample_scale = usize::max(1, usize::min(denom, 256));
    let mut sample_size = source_size / sample_scale;
    sample_size = usize::max(sample_size, usize::min(source_size, 16));
    vprintln!("create_dict: creating {sample_size} byte sample of collection");
    let mut sample_reader = all.as_slice();
    let collection_sample = create_sample(&mut sample_reader, sample_size);

    // A collection of segments to be used in the final dictionary.
    //
    // Contains the best segment from every epoch.
    // Reverse is used because we want a min heap, where
    // the lowest scoring items come first
    let mut pool: BinaryHeap<Reverse<Segment>> = BinaryHeap::new();
    let (num_epochs, epoch_size_kmers) = compute_epoch_info(&params, dict_size, source_size / K);
    let epoch_size = usize::max(K, epoch_size_kmers.saturating_mul(K));
    vprintln!("create_dict: computed epoch info, using {num_epochs} epochs of {epoch_size} bytes");
    let mut epoch_counter = 0;
    let mut ctx = Context {
        frequencies: HashMap::with_capacity(epoch_size / K),
    };
    // Score each segment in each planned epoch and select the highest-scoring
    // segment for the pool. Keep exactly `num_epochs` windows to avoid
    // emitting more segments than the requested dictionary budget allows.
    for epoch_idx in 0..num_epochs {
        let start = epoch_idx.saturating_mul(epoch_size);
        if start >= all.len() {
            break;
        }
        let end = if epoch_idx + 1 == num_epochs {
            all.len()
        } else {
            usize::min(start.saturating_add(epoch_size), all.len())
        };
        let epoch = &all[start..end];
        epoch_counter += 1;
        let best_segment = pick_best_segment(&params, &mut ctx, epoch, &collection_sample);
        vprintln!(
            "\tcreate_dict: epoch {epoch_counter}/{num_epochs} has best segment score {}",
            best_segment.score
        );
        pool.push(Reverse(best_segment));
        // Wipe frequency list for next epoch
        ctx.frequencies.clear();
    }
    vprintln!(
        "create_dict: {epoch_counter} epochs written, writing {} segments",
        pool.len()
    );
    // Write the dictionary with the highest scoring segment last because
    // closer items can be represented with a smaller offset
    while let Some(segment) = pool.pop() {
        output.write_all(&segment.0.raw)?;
    }
    Ok(())
}

fn serialize_huffman_table(sample_data: &[u8], raw_content: &[u8]) -> io::Result<Vec<u8>> {
    fn bounded_huffman_stats(data: &[u8]) -> Vec<u8> {
        if data.len() <= MAX_HUFFMAN_STATS_BYTES {
            return data.to_vec();
        }

        let mut stats = Vec::with_capacity(MAX_HUFFMAN_STATS_BYTES);
        for i in 0..MAX_HUFFMAN_STATS_BYTES {
            let idx = i * data.len() / MAX_HUFFMAN_STATS_BYTES;
            stats.push(data[idx]);
        }
        stats
    }

    let source = if sample_data.len() >= 2 {
        sample_data
    } else {
        raw_content
    };
    let mut stats = bounded_huffman_stats(source);
    if stats.len() < 2 || stats.iter().all(|b| *b == stats[0]) {
        stats = (0u8..=255).collect();
    }

    let table = HuffmanEncoderTable::build_from_data(stats.as_slice());
    let mut writer = BitWriter::new();
    let mut encoder = HuffmanEncoder::new(&table, &mut writer);
    encoder.encode(&[stats[0]], true);
    let encoded = writer.dump();

    let mut decoder = HuffmanDecoderTable::new();
    let table_size = decoder
        .build_decoder(encoded.as_slice())
        .map_err(|e| io::Error::other(format!("failed to decode generated huffman table: {e}")))?;
    Ok(encoded[..table_size as usize].to_vec())
}

fn serialize_fse_table(table: &fse_encoder::FSETable) -> Vec<u8> {
    let mut writer = BitWriter::new();
    table.write_table(&mut writer);
    writer.dump()
}

fn bounded_fse_symbols(data: &[u8], max_symbol: u8) -> Vec<u8> {
    let modulo = u16::from(max_symbol) + 1;
    if data.is_empty() {
        return Vec::from([0u8]);
    }
    if data.len() <= MAX_HUFFMAN_STATS_BYTES {
        return data
            .iter()
            .map(|b| (u16::from(*b) % modulo) as u8)
            .collect();
    }

    let mut out = Vec::with_capacity(MAX_HUFFMAN_STATS_BYTES);
    for i in 0..MAX_HUFFMAN_STATS_BYTES {
        let idx = i * data.len() / MAX_HUFFMAN_STATS_BYTES;
        out.push((u16::from(data[idx]) % modulo) as u8);
    }
    out
}

fn serialize_fse_table_from_corpus(
    sample_data: &[u8],
    raw_content: &[u8],
    max_symbol: u8,
    max_log: u8,
) -> Vec<u8> {
    let source = if sample_data.is_empty() {
        raw_content
    } else {
        sample_data
    };
    let symbols = bounded_fse_symbols(source, max_symbol);
    let table = build_table_from_data(symbols.into_iter(), max_log, false);
    serialize_fse_table(&table)
}

fn finalized_content_budget(
    sample_data: &[u8],
    raw_fallback: &[u8],
    dict_size: usize,
) -> io::Result<usize> {
    let min_content_size = 8usize;
    let huf_len = serialize_huffman_table(sample_data, raw_fallback)?.len();
    let of_len =
        serialize_fse_table_from_corpus(sample_data, raw_fallback, MAX_OFFSET_CODE, OF_MAX_LOG)
            .len();
    let ml_len = serialize_fse_table_from_corpus(
        sample_data,
        raw_fallback,
        MAX_MATCH_LENGTH_CODE,
        ML_MAX_LOG,
    )
    .len();
    let ll_len = serialize_fse_table_from_corpus(
        sample_data,
        raw_fallback,
        MAX_LITERAL_LENGTH_CODE,
        LL_MAX_LOG,
    )
    .len();

    let header_len = DICT_MAGIC_NUM.len() + 4 + huf_len + of_len + ml_len + ll_len + 12;
    let max_content_budget = dict_size.saturating_sub(header_len);
    if max_content_budget < min_content_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dictionary size too small to fit header and offset history",
        ));
    }
    Ok(max_content_budget)
}

fn derive_dict_id(raw_content: &[u8]) -> u32 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in raw_content {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    let compliant = (h % ((1u64 << 31) - 32768)) + 32768;
    compliant as u32
}

/// Finalize raw dictionary content into a full zstd dictionary binary
/// (`magic + dict_id + entropy tables + offset history + content`).
pub fn finalize_raw_dict(
    raw_content: &[u8],
    sample_data: &[u8],
    dict_size: usize,
    options: FinalizeOptions,
) -> io::Result<Vec<u8>> {
    if raw_content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw dictionary content must not be empty",
        ));
    }
    let mut out = Vec::with_capacity(dict_size.max(256));
    out.extend_from_slice(&DICT_MAGIC_NUM);
    let dict_id = options
        .dict_id
        .unwrap_or_else(|| derive_dict_id(raw_content));
    if dict_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dictionary id must be non-zero",
        ));
    }
    out.extend_from_slice(&dict_id.to_le_bytes());
    out.extend_from_slice(serialize_huffman_table(sample_data, raw_content)?.as_slice());
    out.extend_from_slice(
        serialize_fse_table_from_corpus(sample_data, raw_content, MAX_OFFSET_CODE, OF_MAX_LOG)
            .as_slice(),
    );
    out.extend_from_slice(
        serialize_fse_table_from_corpus(
            sample_data,
            raw_content,
            MAX_MATCH_LENGTH_CODE,
            ML_MAX_LOG,
        )
        .as_slice(),
    );
    out.extend_from_slice(
        serialize_fse_table_from_corpus(
            sample_data,
            raw_content,
            MAX_LITERAL_LENGTH_CODE,
            LL_MAX_LOG,
        )
        .as_slice(),
    );

    // Repeat offsets: keep default bootstrap history.
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());

    let min_content_size = 8usize;
    let max_content_budget = dict_size.saturating_sub(out.len());
    if max_content_budget < min_content_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dictionary size too small to fit header and offset history",
        ));
    }

    let content = if raw_content.len() > max_content_budget {
        &raw_content[raw_content.len() - max_content_budget..]
    } else {
        raw_content
    };
    if content.len() < min_content_size {
        out.resize(out.len() + (min_content_size - content.len()), 0);
    }
    out.extend_from_slice(content);
    Ok(out)
}

/// Train a raw FastCOVER dictionary from a source stream.
fn train_fastcover_internal(
    sample: &[u8],
    dict_size: usize,
    options: &FastCoverOptions,
) -> (Vec<u8>, FastCoverTuned) {
    if options.optimize {
        fastcover::optimize_fastcover_raw(
            sample,
            dict_size,
            options.split_point,
            options.accel,
            options.d_candidates.as_slice(),
            options.f_candidates.as_slice(),
            options.k_candidates.as_slice(),
        )
    } else {
        let params = fastcover::normalize_fastcover_params(FastCoverParams {
            k: options.k,
            d: options.d,
            f: options.f,
            accel: options.accel,
        });
        (
            fastcover::train_fastcover_raw(sample, dict_size, params),
            FastCoverTuned {
                k: params.k,
                d: params.d,
                f: params.f,
                accel: params.accel,
                score: 0,
            },
        )
    }
}

/// Train a raw FastCOVER dictionary directly from an in-memory sample.
pub fn train_fastcover_raw_from_slice(
    sample: &[u8],
    dict_size: usize,
    options: &FastCoverOptions,
) -> io::Result<(Vec<u8>, FastCoverTuned)> {
    if sample.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source stream is empty",
        ));
    }
    let (dict, tuned) = train_fastcover_internal(sample, dict_size, options);
    if dict.is_empty() && dict_size > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "training sample is too small for FastCOVER",
        ));
    }
    Ok((dict, tuned))
}

/// Train a raw FastCOVER dictionary from a source stream.
///
/// This function fully buffers the entire training corpus into memory via
/// `read_to_end`, which can consume significant RAM for large inputs.
pub fn create_fastcover_raw_dict_from_source<R: io::Read, W: io::Write>(
    mut source: R,
    output: &mut W,
    dict_size: usize,
    options: &FastCoverOptions,
) -> io::Result<FastCoverTuned> {
    let mut sample = Vec::new();
    source.read_to_end(&mut sample)?;
    let (dict, tuned) = train_fastcover_raw_from_slice(sample.as_slice(), dict_size, options)?;
    output.write_all(dict.as_slice())?;
    Ok(tuned)
}

/// Train and finalize a FastCOVER dictionary in pure Rust.
///
/// This function fully buffers the entire training corpus into memory via
/// `read_to_end`, which can consume significant RAM for large inputs.
pub fn create_fastcover_dict_from_source<R: io::Read, W: io::Write>(
    mut source: R,
    output: &mut W,
    dict_size: usize,
    fastcover: &FastCoverOptions,
    finalize: FinalizeOptions,
) -> io::Result<FastCoverTuned> {
    let mut sample = Vec::new();
    source.read_to_end(&mut sample)?;
    let content_budget = finalized_content_budget(sample.as_slice(), sample.as_slice(), dict_size)?;
    let (raw_dict, tuned) =
        train_fastcover_raw_from_slice(sample.as_slice(), content_budget, fastcover)?;

    let finalized = finalize_raw_dict(raw_dict.as_slice(), sample.as_slice(), dict_size, finalize)?;
    output.write_all(finalized.as_slice())?;
    Ok(tuned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoding::Dictionary;
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use std::io::Cursor;
    use std::string::ToString;

    fn training_data() -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..512u32 {
            data.extend_from_slice(
                format!(
                    "tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbcccccdddddeeeee\n"
                )
                .as_bytes(),
            );
        }
        data
    }

    #[test]
    fn finalize_raw_dict_roundtrips_with_ffi_decoder() {
        let sample = training_data();
        let raw = fastcover::train_fastcover_raw(
            sample.as_slice(),
            4096,
            FastCoverParams {
                k: 256,
                d: 8,
                f: 20,
                accel: 1,
            },
        );
        let finalized = finalize_raw_dict(
            raw.as_slice(),
            sample.as_slice(),
            4096,
            FinalizeOptions::default(),
        )
        .expect("finalization should succeed");
        let parsed = Dictionary::decode_dict(finalized.as_slice())
            .expect("finalized dictionary should parse");
        assert!(!parsed.dict_content.is_empty());

        let mut payload = Vec::new();
        for idx in 0..96u32 {
            payload.extend_from_slice(
                format!("tenant=demo op=put key={idx} value=aaaaabbbbbcccccdddddeeeee\n")
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

    #[test]
    fn create_fastcover_dict_from_source_writes_non_empty_output() {
        let sample = training_data();
        let mut out = Vec::new();
        let tuned = create_fastcover_dict_from_source(
            Cursor::new(sample.as_slice()),
            &mut out,
            4096,
            &FastCoverOptions::default(),
            FinalizeOptions::default(),
        )
        .expect("fastcover+finalize should succeed");
        assert!(!out.is_empty());
        assert!(tuned.k > 0);
        assert!(tuned.d > 0);
    }

    #[test]
    fn create_fastcover_raw_dict_from_source_rejects_empty_source() {
        let mut out = Vec::new();
        let err = create_fastcover_raw_dict_from_source(
            Cursor::new(Vec::<u8>::new()),
            &mut out,
            1024,
            &FastCoverOptions::default(),
        )
        .expect_err("empty source must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn create_fastcover_dict_from_source_propagates_finalize_error() {
        let sample = training_data();
        let mut out = Vec::new();
        let err = create_fastcover_dict_from_source(
            Cursor::new(sample.as_slice()),
            &mut out,
            32,
            &FastCoverOptions::default(),
            FinalizeOptions::default(),
        )
        .expect_err("too-small dictionary budget must fail during finalize");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("dictionary size too small"));
    }

    #[test]
    fn create_raw_dict_from_source_early_returns_on_zero_dict_size() {
        let sample = training_data();
        let mut out = Vec::new();
        create_raw_dict_from_source(Cursor::new(sample.as_slice()), sample.len(), &mut out, 0)
            .expect("zero dict size should no-op");
        assert!(out.is_empty());
    }

    #[test]
    fn create_raw_dict_from_source_treats_source_size_as_hint() {
        let sample = training_data();
        let mut out = Vec::new();
        create_raw_dict_from_source(Cursor::new(sample.as_slice()), 0, &mut out, 1024)
            .expect("raw dictionary training should succeed");
        assert!(!out.is_empty());
    }

    #[test]
    fn create_raw_dict_from_source_handles_tiny_source_without_epochs() {
        let sample = b"short";
        let mut out = Vec::new();
        create_raw_dict_from_source(Cursor::new(sample.as_slice()), sample.len(), &mut out, 3)
            .expect("tiny source path should succeed");
        assert_eq!(out, b"ort");
    }

    #[test]
    fn create_raw_dict_from_source_propagates_read_error() {
        struct FailingReader;
        impl io::Read for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("read failed"))
            }
        }

        let mut out = Vec::new();
        let err = create_raw_dict_from_source(FailingReader, 1024, &mut out, 1024)
            .expect_err("read failures must be returned");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.to_string(), "read failed");
    }

    #[test]
    fn create_raw_dict_from_source_propagates_write_error() {
        struct FailingWriter;
        impl io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("write failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let sample = b"short";
        let mut out = FailingWriter;
        let err =
            create_raw_dict_from_source(Cursor::new(sample.as_slice()), sample.len(), &mut out, 3)
                .expect_err("write failures must be returned");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.to_string(), "write failed");
    }

    #[test]
    fn create_raw_dict_from_source_never_exceeds_requested_size() {
        let dict_size = 4096usize;
        let source: Vec<u8> = core::iter::repeat_n(b'a', 320_001).collect();
        let mut out = Vec::new();
        create_raw_dict_from_source(
            Cursor::new(source.as_slice()),
            source.len(),
            &mut out,
            dict_size,
        )
        .expect("raw dictionary training should succeed");
        assert!(
            out.len() <= dict_size,
            "raw dictionary exceeded requested size: {} > {}",
            out.len(),
            dict_size
        );
    }

    #[test]
    fn train_fastcover_raw_from_slice_rejects_empty_sample() {
        let err = train_fastcover_raw_from_slice(&[], 1024, &FastCoverOptions::default())
            .expect_err("empty sample must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn train_fastcover_raw_from_slice_supports_non_optimized_params() {
        let sample = training_data();
        let options = FastCoverOptions {
            optimize: false,
            k: 128,
            d: 6,
            f: 18,
            ..FastCoverOptions::default()
        };
        let (dict, tuned) =
            train_fastcover_raw_from_slice(sample.as_slice(), 2048, &options).expect("must train");
        assert!(!dict.is_empty());
        assert!(dict.len() <= 2048);
        assert_eq!(tuned.k, 128);
        assert_eq!(tuned.d, 6);
        assert_eq!(tuned.f, 18);
        assert_eq!(tuned.score, 0);
    }

    #[test]
    fn train_fastcover_raw_from_slice_rejects_tiny_sample_with_empty_dict() {
        let sample = b"tiny";
        let err = train_fastcover_raw_from_slice(sample, 1024, &FastCoverOptions::default())
            .expect_err("tiny sample should not produce an empty dictionary successfully");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "training sample is too small for FastCOVER"
        );
    }

    #[test]
    fn train_fastcover_raw_from_slice_normalizes_non_optimized_params() {
        let sample = training_data();
        let options = FastCoverOptions {
            optimize: false,
            k: 8,
            d: 64,
            f: 42,
            ..FastCoverOptions::default()
        };
        let (_, tuned) =
            train_fastcover_raw_from_slice(sample.as_slice(), 2048, &options).expect("must train");
        assert_eq!(tuned.k, 32);
        assert_eq!(tuned.d, 32);
        assert_eq!(tuned.f, 20);
    }

    #[test]
    fn finalize_raw_dict_rejects_empty_raw_content() {
        let sample = training_data();
        let err = finalize_raw_dict(&[], sample.as_slice(), 4096, FinalizeOptions::default())
            .expect_err("empty raw dictionary must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn finalize_raw_dict_rejects_too_small_budget() {
        let sample = training_data();
        let raw = b"some-raw-bytes";
        let err = finalize_raw_dict(raw, sample.as_slice(), 32, FinalizeOptions::default())
            .expect_err("tiny dict_size must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("dictionary size too small"));
    }

    #[test]
    fn finalize_raw_dict_pads_to_minimum_content_size() {
        let sample = training_data();
        let raw = b"x";
        let finalized = finalize_raw_dict(raw, sample.as_slice(), 4096, FinalizeOptions::default())
            .expect("finalize should pad small raw content");
        let parsed = Dictionary::decode_dict(finalized.as_slice()).expect("finalized dict parses");
        assert!(parsed.dict_content.len() >= 8);
        assert_eq!(parsed.dict_content.last(), Some(&b'x'));
    }

    #[test]
    fn finalize_raw_dict_rejects_zero_dict_id() {
        let sample = training_data();
        let raw = b"raw-fastcover-bytes";
        let err = finalize_raw_dict(
            raw,
            sample.as_slice(),
            4096,
            FinalizeOptions { dict_id: Some(0) },
        )
        .expect_err("dict_id=0 must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "dictionary id must be non-zero");
    }
}
