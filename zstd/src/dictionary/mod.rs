//! Dictionary training.
//!
//! Effective dictionaries are up to 1% the size of the complete training body,
//! and are trained on many examples of the original data. The trainers are
//! those of the reference implementation, each taking the samples back to back
//! with their sizes:
//!
//! - COVER ([`train_cover_dict`], [`optimize_cover_dict`]) indexes every dmer
//!   exactly and fills the dictionary with the segments whose dmers the most
//!   samples share.
//! - FastCOVER ([`train_fastcover_dict`], [`optimize_fastcover_dict`]) does the
//!   same over a hashed frequency table, faster and with less memory; it is
//!   what `zstd --train` runs.
//! - The legacy trainer ([`create_legacy_dict_from_slice`]) searches a suffix
//!   array for repeated segments.
//!
//! The `optimize_*` forms search segment and dmer sizes, scoring each candidate
//! by the total size of the scoring samples compressed with it.
//!
//! [`create_raw_dict_from_slice`] and its reader forms build raw content from an
//! undivided corpus instead, estimating segment value by k-mer frequency in a
//! reservoir sample (Liao, Petri, Moffat and Wirth, "Effective construction of
//! Relative Lempel-Ziv Dictionaries").
mod cover;
mod fastcover;
mod frequency;
mod legacy;
mod lmc;
mod reservoir;
mod samples;
mod selection;
mod suffix_array;

use crate::bit_io::BitWriter;
use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};
use crate::decoding::dictionary::MAGIC_NUM as DICT_MAGIC_NUM;
use crate::decoding::sequence_section_decoder::{LL_MAX_LOG, ML_MAX_LOG, OF_MAX_LOG};
use crate::dictionary::reservoir::create_sample;
use crate::fse::fse_encoder::{self, build_table_from_symbol_counts};
use crate::huff0::HuffmanTable as HuffmanDecoderTable;
use crate::huff0::huff0_encoder::{HuffmanEncoder, HuffmanTable as HuffmanEncoderTable};
use core::cmp::Reverse;
pub use legacy::DEFAULT_SELECTIVITY;
use lmc::*;
pub use samples::TrainingError;
use samples::refuse;
use std::{
    boxed::Box,
    collections::{BinaryHeap, HashMap},
    format,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    // `vec` import covers the `vec![..]` macro used below: this crate is
    // no_std-with-std-feature, so the std prelude isn't pulled in implicitly
    // for top-level items in this module. Removing this import fails the
    // build with `cannot find macro 'vec' in this scope` — verified.
    vec,
    vec::Vec,
};

const MAX_TRAINING_PREALLOC_BYTES: usize = 8 * 1024 * 1024;
const MAX_HUFFMAN_STATS_BYTES: usize = 64 * 1024;

/// Smallest size a trained dictionary can occupy, whatever it was trained on.
///
/// The magic number, the dictionary ID, the three repeat offsets and the
/// shortest content the writers emit are unconditional; a real dictionary is
/// larger still, since the entropy tables between them are never empty. Use it
/// to reject an impossible `dict_size` before spending the corpus: the training
/// entry points can only discover the true bound once those tables are built.
pub const MIN_TRAINED_DICT_SIZE: usize = DICT_MAGIC_NUM.len() + 4 + 12 + 8;

/// Tuning for COVER training, the knobs of the reference's
/// `ZDICT_cover_params_t`.
///
/// The defaults are those `zstd --train-cover` starts from: `d` of 8, a search
/// over `k` in four steps, every sample both building and scoring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverOptions {
    /// Segment size in bytes. Zero searches 50..=2000 when optimizing.
    pub k: u32,
    /// Dmer size in bytes, at most `k`. Zero searches 6 and 8 when optimizing.
    pub d: u32,
    /// How many values of `k` the search tries; zero is 40.
    pub steps: u32,
    /// Share of the samples dictionaries are built from, in `(0, 1]`; the rest
    /// score them. At 1 every sample does both. Zero or below is the trainer's
    /// default: 1 for COVER, 0.75 for FastCOVER.
    pub split_point: f64,
    /// Try the content's last 256, 512, 1024, ... bytes, each finalized into a
    /// dictionary of its own, and keep the first whose scoring samples
    /// compress to at most this many percent more than with the whole content
    /// (upstream zstd `COVER_selectDict`). A dictionary so found is its header
    /// plus that tail, so none is smaller than the header plus 256 bytes.
    pub shrink: Option<u32>,
    /// Compression level candidates are scored at; zero is the default level.
    pub level: i32,
}

impl Default for CoverOptions {
    fn default() -> Self {
        Self {
            k: 0,
            d: 8,
            steps: 4,
            split_point: 1.0,
            shrink: None,
            level: 0,
        }
    }
}

/// Tuning for FastCOVER training, the knobs of the reference's
/// `ZDICT_fastCover_params_t`.
///
/// The defaults are those `zstd --train` runs: `d` of 8, a table of 2^20
/// counts, a search over `k` in four steps, three quarters of the samples
/// building and the rest scoring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FastCoverOptions {
    /// Segment size, dmer size, search, split, shrink and scoring level, as for
    /// COVER. `d` is at least 4 here; the reference takes 6 and 8.
    pub cover: CoverOptions,
    /// Width of the dmer frequency table in bits, `1..=31`; its memory grows
    /// as `2^f`. Zero is 20.
    pub f: u32,
    /// Count every `accel`-th position, `1..=10`, and draw the entropy tables
    /// from a matching share of the samples. Zero is 1.
    pub accel: u32,
}

impl Default for FastCoverOptions {
    fn default() -> Self {
        Self {
            cover: CoverOptions {
                split_point: 0.75,
                ..CoverOptions::default()
            },
            f: 20,
            accel: 1,
        }
    }
}

/// Header options for a finalized dictionary.
#[derive(Debug, Clone, Copy, Default)]
pub struct FinalizeOptions {
    /// The dictionary id; `None` derives one from the content.
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
/// The resulting dictionary will be approximately `dict_size` or less, and written to `output`.
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
///
/// A corpus already in memory trains without this copy through
/// [`create_raw_dict_from_slice`].
///
/// # API note
/// This public API returns `io::Result<()>` and propagates source/output I/O failures.
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
    create_raw_dict_from_slice(&all, output, dict_size)
}

/// Create a "raw content" dictionary of at most `dict_size` bytes from a
/// corpus already in memory, writing it to `output`.
///
/// The same training as [`create_raw_dict_from_source`], reading `source` in
/// place: a caller that holds the samples anyway does not pay for a second
/// copy of them.
///
/// # Errors
/// Returns the error `output` reports while the dictionary is written.
///
/// # Examples
/// ```
/// use structured_zstd::dictionary::create_raw_dict_from_slice;
///
/// let corpus: Vec<u8> = (0..20_000u32)
///     .flat_map(|i| format!("record {} value {}\n", i % 100, i % 7).into_bytes())
///     .collect();
/// let mut dict = Vec::new();
/// create_raw_dict_from_slice(&corpus, &mut dict, 4096).unwrap();
/// assert!(!dict.is_empty() && dict.len() <= 4096);
/// ```
pub fn create_raw_dict_from_slice<W: io::Write>(
    all: &[u8],
    output: &mut W,
    dict_size: usize,
) -> io::Result<()> {
    if dict_size == 0 || all.is_empty() {
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
    let mut sample_reader = all;
    let collection_sample = create_sample(&mut sample_reader, sample_size);

    // A collection of segments to be used in the final dictionary.
    //
    // Contains the best segment from every epoch.
    // Reverse is used because we want a min heap, where
    // the lowest scoring items come first
    let mut pool: BinaryHeap<Reverse<Segment>> = BinaryHeap::new();
    let (num_epochs, epoch_size_kmers) = compute_epoch_info(&params, dict_size, source_size / K);
    // Plain `*`/`+` throughout the epoch walk below: epochs partition the
    // training source, so `epoch_size_kmers * K`, `epoch_idx * epoch_size`, and
    // `start + epoch_size` are all bounded by the source length (<= isize::MAX)
    // and cannot overflow usize.
    let epoch_size = usize::max(K, epoch_size_kmers * K);
    vprintln!("create_dict: computed epoch info, using {num_epochs} epochs of {epoch_size} bytes");
    let mut epoch_counter = 0;
    let mut ctx = Context {
        frequencies: HashMap::with_capacity(epoch_size / K),
    };
    // Score each segment in each planned epoch and select the highest-scoring
    // segment for the pool. Keep exactly `num_epochs` windows to avoid
    // emitting more segments than the requested dictionary budget allows.
    for epoch_idx in 0..num_epochs {
        let start = epoch_idx * epoch_size;
        if start >= all.len() {
            break;
        }
        let end = if epoch_idx + 1 == num_epochs {
            all.len()
        } else {
            usize::min(start + epoch_size, all.len())
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

/// The `i`th of [`MAX_HUFFMAN_STATS_BYTES`] samples spread evenly over `len`
/// bytes.
///
/// Computed in 64 bits: `i * len` reaches 2^48 for an addressable corpus, which
/// a 32-bit `usize` cannot hold — the product overflows for any corpus past
/// 64 KiB there, and the multiply panics rather than sampling. The quotient is
/// always below `len`, so the narrowing back is exact.
fn strided_index(i: usize, len: usize) -> usize {
    ((i as u64 * len as u64) / MAX_HUFFMAN_STATS_BYTES as u64) as usize
}

fn serialize_huffman_table(sample_data: &[u8], raw_content: &[u8]) -> io::Result<Vec<u8>> {
    fn bounded_huffman_stats(data: &[u8]) -> Vec<u8> {
        if data.len() <= MAX_HUFFMAN_STATS_BYTES {
            return data.to_vec();
        }

        let mut stats = Vec::with_capacity(MAX_HUFFMAN_STATS_BYTES);
        for i in 0..MAX_HUFFMAN_STATS_BYTES {
            stats.push(data[strided_index(i, data.len())]);
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
        // A corpus with no distribution to measure gets a synthetic one. It
        // stops at 128 symbols because a perfectly flat alphabet gives every
        // symbol the same weight: FSE cannot encode that (an RLE weight
        // stream), and the direct nibble form addresses at most 128 symbols, so
        // a full 0..=255 alphabet would have no description at all.
        stats = (0u8..128).collect();
    }

    let mut table = HuffmanEncoderTable::build_from_data(stats.as_slice());
    if table
        .writeable_table_description_size(&mut crate::fse::fse_encoder::FSETable::blank())
        .is_none()
    {
        // Sampled real data can land on the same shape: a flat alphabet wider
        // than 128 symbols. Fall back to the synthetic narrow one, which always
        // has a description.
        stats = (0u8..128).collect();
        table = HuffmanEncoderTable::build_from_data(stats.as_slice());
    }
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
        let idx = strided_index(i, data.len());
        out.push((u16::from(data[idx]) % modulo) as u8);
    }
    out
}

fn serialize_fse_table_from_corpus(
    sample_data: &[u8],
    raw_content: &[u8],
    max_symbol: u8,
    max_log: u8,
) -> io::Result<Vec<u8>> {
    fn counts_total_for_source(source: &[u8], max_symbol: u8, counts: &mut [usize]) -> usize {
        counts.fill(0);
        for symbol in bounded_fse_symbols(source, max_symbol) {
            counts[usize::from(symbol)] += 1;
        }
        counts.iter().sum::<usize>()
    }

    let mut counts = vec![0usize; usize::from(max_symbol) + 1];
    let using_sample = !sample_data.is_empty();
    let mut total = counts_total_for_source(
        if using_sample {
            sample_data
        } else {
            raw_content
        },
        max_symbol,
        &mut counts,
    );
    if total <= 1 && using_sample && !raw_content.is_empty() {
        total = counts_total_for_source(raw_content, max_symbol, &mut counts);
    }
    if total <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "insufficient symbol statistics for FSE table",
        ));
    }
    let table = build_table_from_symbol_counts(&counts, max_log, false);
    Ok(serialize_fse_table(&table))
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
    let mut tables = serialize_huffman_table(sample_data, raw_content)?;
    for (max_symbol, max_log) in ENTROPY_STREAMS {
        tables.extend_from_slice(&serialize_fse_table_from_corpus(
            sample_data,
            raw_content,
            max_symbol,
            max_log,
        )?);
    }
    let mut out = Vec::new();
    assemble_dict(&mut out, raw_content, &tables, dict_size, options)?;
    Ok(out)
}

/// The offset, match-length and literal-length streams, in the order their
/// tables follow the literals table in a dictionary.
const ENTROPY_STREAMS: [(u8, u8); 3] = [
    (MAX_OFFSET_CODE, OF_MAX_LOG),
    (MAX_MATCH_LENGTH_CODE, ML_MAX_LOG),
    (MAX_LITERAL_LENGTH_CODE, LL_MAX_LOG),
];

/// The entropy tables [`finalize_raw_dict`] writes, when `sample_data` alone
/// decides them; `None` when the samples are too thin and the tables would
/// fall back on the content, which then has to be finalized in full.
fn sample_entropy_tables(sample_data: &[u8]) -> Option<Vec<u8>> {
    if sample_data.len() < 2 {
        return None;
    }
    let mut tables = serialize_huffman_table(sample_data, &[]).ok()?;
    for (max_symbol, max_log) in ENTROPY_STREAMS {
        tables.extend_from_slice(
            &serialize_fse_table_from_corpus(sample_data, &[], max_symbol, max_log).ok()?,
        );
    }
    Some(tables)
}

/// A dictionary of `raw_content` behind already serialized entropy `tables`,
/// written over `out`, whose allocation a caller finalizing many candidates
/// keeps from one to the next.
fn assemble_dict(
    out: &mut Vec<u8>,
    raw_content: &[u8],
    tables: &[u8],
    dict_size: usize,
    options: FinalizeOptions,
) -> io::Result<()> {
    out.clear();
    out.reserve(dict_size.max(256));
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
    out.extend_from_slice(tables);

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
    Ok(())
}

/// Smallest dictionary, in bytes, any trainer here builds (upstream zstd
/// `ZDICT_DICTSIZE_MIN`); a smaller one is refused with
/// [`TrainingError::DictionaryTooSmall`]. Unlike [`MIN_TRAINED_DICT_SIZE`] it
/// bounds a training request, so a caller can refuse one before loading the
/// samples.
pub const TRAINER_DICT_SIZE_MIN: usize = 256;

/// The `k` and `d` values a training run tries, resolved from the options the
/// way the reference's optimizers resolve theirs.
struct SearchSpace {
    d: core::ops::RangeInclusive<u32>,
    k: core::ops::RangeInclusive<u32>,
    k_step: usize,
    steps: u32,
    split_point: f64,
}

impl SearchSpace {
    /// Upstream zstd `ZDICT_optimizeTrainFromBuffer_cover`: zero `d` tries 6
    /// and 8, zero `k` tries 50..=2000 in `steps` strides.
    fn optimizing(options: &CoverOptions, default_split: f64) -> io::Result<Self> {
        // NaN fails every comparison below, so it is refused on its own.
        if !options.split_point.is_finite() {
            return Err(refuse(
                TrainingError::Parameter,
                "the split point must lie in (0, 1]",
            ));
        }
        let split_point = if options.split_point <= 0.0 {
            default_split
        } else {
            options.split_point
        };
        if split_point > 1.0 {
            return Err(refuse(
                TrainingError::Parameter,
                "the split point must lie in (0, 1]",
            ));
        }
        let d = if options.d == 0 {
            6..=8
        } else {
            options.d..=options.d
        };
        let k = if options.k == 0 {
            50..=2000
        } else {
            options.k..=options.k
        };
        if *k.start() < *d.end() {
            return Err(refuse(TrainingError::Parameter, "k must be at least d"));
        }
        let steps = if options.steps == 0 {
            40
        } else {
            options.steps
        };
        let k_step = ((k.end() - k.start()) / steps).max(1) as usize;
        Ok(Self {
            d,
            k,
            k_step,
            steps,
            split_point,
        })
    }

    /// The single `k` and `d` a plain training run is given, with every sample
    /// building (upstream zstd `ZDICT_trainFromBuffer_cover`).
    fn fixed(options: &CoverOptions) -> io::Result<Self> {
        if options.k == 0 || options.d == 0 {
            return Err(refuse(
                TrainingError::Parameter,
                "k and d are required; zero asks for a search, which the optimize_* trainers run",
            ));
        }
        Ok(Self {
            d: options.d..=options.d,
            k: options.k..=options.k,
            k_step: 1,
            steps: options.steps,
            split_point: 1.0,
        })
    }

    fn pairs(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.d.clone().step_by(2).flat_map(move |d| {
            self.k
                .clone()
                .step_by(self.k_step)
                .map(move |k| (d as usize, k as usize))
        })
    }

    /// Refuse a space with no `k` and `d` a `dict_size` dictionary can hold,
    /// before any sample is indexed (upstream zstd `COVER_checkParameters`,
    /// which checks the parameters ahead of the samples).
    fn check_fits(&self, dict_size: usize) -> io::Result<()> {
        if self.pairs().any(|(d, k)| segment_fits(k, d, dict_size)) {
            return Ok(());
        }
        Err(refuse(
            TrainingError::Parameter,
            "no parameter combination is valid for this dictionary size",
        ))
    }

    /// The trainer `options` select, the way the CLI and the C ABI choose it:
    /// both `k` and `d` given train with them, anything less searches.
    fn selected(options: &CoverOptions, default_split: f64) -> io::Result<Self> {
        if options.k != 0 && options.d != 0 {
            Self::fixed(options)
        } else {
            Self::optimizing(options, default_split)
        }
    }
}

/// Refuse COVER tuning no training run can use for a `dict_size` dictionary,
/// from the options alone: what [`train_cover_dict`] (both `k` and `d` given)
/// or [`optimize_cover_dict`] (either left zero) would refuse before reading a
/// sample. Lets a caller holding a large corpus fail before loading it.
///
/// # Errors
///
/// `InvalidInput` for tuning out of range or for a `k` and `d` that no
/// dictionary of `dict_size` bytes holds, where [`TrainingError::of`] reports
/// [`TrainingError::Parameter`], and for a `dict_size` under
/// [`TRAINER_DICT_SIZE_MIN`], where it reports
/// [`TrainingError::DictionaryTooSmall`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{CoverOptions, check_cover_options};
///
/// let fits = CoverOptions { k: 256, d: 8, ..CoverOptions::default() };
/// assert!(check_cover_options(&fits, 4096).is_ok());
/// let too_long = CoverOptions { k: 8192, d: 8, ..CoverOptions::default() };
/// assert!(check_cover_options(&too_long, 4096).is_err());
/// ```
pub fn check_cover_options(options: &CoverOptions, dict_size: usize) -> io::Result<()> {
    // The trainers' order, and upstream zstd's: `COVER_checkParameters` before
    // the `ZDICT_DICTSIZE_MIN` check, so both name the same cause.
    let space = SearchSpace::selected(options, 1.0)?;
    space.check_fits(dict_size)?;
    check_dict_size(dict_size)
}

/// [`check_cover_options`] for FastCOVER: also refuses `f`, `accel` and a `d`
/// below 4 as [`train_fastcover_dict`] and [`optimize_fastcover_dict`] do.
///
/// # Errors
///
/// As [`check_cover_options`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{CoverOptions, FastCoverOptions, check_fastcover_options};
///
/// let options = FastCoverOptions {
///     cover: CoverOptions { k: 256, d: 8, ..FastCoverOptions::default().cover },
///     ..FastCoverOptions::default()
/// };
/// assert!(check_fastcover_options(&options, 4096).is_ok());
/// assert!(check_fastcover_options(&options, 128).is_err());
/// ```
pub fn check_fastcover_options(options: &FastCoverOptions, dict_size: usize) -> io::Result<()> {
    let space = SearchSpace::selected(&options.cover, 0.75)?;
    fastcover_knobs(options, &space)?;
    space.check_fits(dict_size)?;
    check_dict_size(dict_size)
}

/// Refuse `sample_count` samples the COVER training `options` select cannot
/// build from: too few once the split takes its scoring share, or none left to
/// score on. Needs only the count, so a caller can ask before reading a sample.
///
/// # Errors
///
/// `InvalidInput` for tuning out of range, where [`TrainingError::of`]
/// reports [`TrainingError::Parameter`], and for too few samples, where it
/// reports [`TrainingError::Samples`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{CoverOptions, check_cover_sample_count};
///
/// // The search builds from the leading three quarters of the samples.
/// let search = CoverOptions { split_point: 0.75, ..CoverOptions::default() };
/// assert!(check_cover_sample_count(&search, 6).is_err());
/// assert!(check_cover_sample_count(&search, 8).is_ok());
/// ```
pub fn check_cover_sample_count(options: &CoverOptions, sample_count: usize) -> io::Result<()> {
    let space = SearchSpace::selected(options, 1.0)?;
    samples::split_count(sample_count, space.split_point).map(drop)
}

/// [`check_cover_sample_count`] for FastCOVER, whose search scores on a
/// quarter of the samples unless `split_point` says otherwise.
///
/// # Errors
///
/// As [`check_cover_sample_count`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{FastCoverOptions, check_fastcover_sample_count};
///
/// let search = FastCoverOptions::default();
/// assert!(check_fastcover_sample_count(&search, 6).is_err());
/// assert!(check_fastcover_sample_count(&search, 8).is_ok());
/// ```
pub fn check_fastcover_sample_count(
    options: &FastCoverOptions,
    sample_count: usize,
) -> io::Result<()> {
    let space = SearchSpace::selected(&options.cover, 0.75)?;
    samples::split_count(sample_count, space.split_point).map(drop)
}

/// The table width and acceleration `options` run with, zero meaning upstream
/// zstd's defaults, refused where FastCOVER cannot run them.
fn fastcover_knobs(options: &FastCoverOptions, space: &SearchSpace) -> io::Result<(u32, u32)> {
    let f = if options.f == 0 { 20 } else { options.f };
    let accel = if options.accel == 0 { 1 } else { options.accel };
    if f > fastcover::MAX_F {
        return Err(refuse(
            TrainingError::Parameter,
            &format!("f must be in 1..={}, got {f}", fastcover::MAX_F),
        ));
    }
    if accel > fastcover::MAX_ACCEL {
        return Err(refuse(
            TrainingError::Parameter,
            &format!("accel must be in 1..={}, got {accel}", fastcover::MAX_ACCEL),
        ));
    }
    if *space.d.start() < 4 {
        return Err(refuse(
            TrainingError::Parameter,
            "FastCOVER needs d of at least 4",
        ));
    }
    Ok((f, accel))
}

/// Upstream zstd `COVER_checkParameters`: a segment fits the dictionary and
/// holds at least one dmer.
fn segment_fits(k: usize, d: usize, dict_size: usize) -> bool {
    d > 0 && d <= k && k <= dict_size
}

/// The samples and the dictionary size, checked in upstream zstd's order
/// (`ZDICT_trainFromBuffer_cover`): no samples at all, then a dictionary too
/// small; the samples are walked, and the remaining checks run, after both.
fn check_samples_and_dict_size<'s>(
    samples: &'s [u8],
    sample_sizes: &[usize],
    dict_size: usize,
) -> io::Result<samples::SampleSet<'s>> {
    if sample_sizes.is_empty() {
        return Err(refuse(TrainingError::Samples, "there are no samples"));
    }
    check_dict_size(dict_size)?;
    samples::SampleSet::new(samples, sample_sizes)
}

/// Refuse a dictionary smaller than any trainer builds.
fn check_dict_size(dict_size: usize) -> io::Result<()> {
    if dict_size < TRAINER_DICT_SIZE_MIN {
        return Err(refuse(
            TrainingError::DictionaryTooSmall,
            &format!("a dictionary must be at least {TRAINER_DICT_SIZE_MIN} bytes"),
        ));
    }
    Ok(())
}

/// Train a COVER dictionary of at most `dict_size` bytes with the `k` and `d`
/// given (the reference's `ZDICT_trainFromBuffer_cover`).
///
/// `samples` is every sample back to back and `sample_sizes` their lengths.
/// Every sample builds, and the entropy tables are drawn from all of them. With
/// [`CoverOptions::shrink`] the samples also score the shrinking search;
/// `steps` and `split_point` are ignored.
///
/// # Errors
///
/// `InvalidInput` when `k` or `d` is zero or `d > k`, when `k` exceeds
/// `dict_size`, when `dict_size` is under [`TRAINER_DICT_SIZE_MIN`], when there
/// are fewer than five samples or they do not add up to `samples.len()`;
/// [`TrainingError::of`] tells these causes apart.
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{CoverOptions, FinalizeOptions, train_cover_dict};
///
/// let mut samples = Vec::new();
/// let mut sizes = Vec::new();
/// for i in 0..200u32 {
///     let line = format!("tenant=demo table=orders key={i} region=eu status=shipped\n");
///     sizes.push(line.len());
///     samples.extend_from_slice(line.as_bytes());
/// }
/// let options = CoverOptions { k: 64, ..CoverOptions::default() };
/// let dict = train_cover_dict(&samples, &sizes, 4096, &options, FinalizeOptions::default())
///     .unwrap();
/// assert!(dict.starts_with(&[0x37, 0xA4, 0x30, 0xEC]) && dict.len() <= 4096);
/// ```
pub fn train_cover_dict(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &CoverOptions,
    finalize: FinalizeOptions,
) -> io::Result<Vec<u8>> {
    let space = SearchSpace::fixed(options)?;
    let (dict, _) = run_cover(samples, sample_sizes, dict_size, options, finalize, &space)?;
    Ok(dict)
}

/// Train COVER dictionaries over a range of `k` and `d` and keep the one the
/// scoring samples compress best with (the reference's
/// `ZDICT_optimizeTrainFromBuffer_cover`). Returns it with the options that
/// built it.
///
/// A zero `k` searches 50..=2000 in `steps` strides (zero `steps` is 40), a
/// zero `d` tries 6 and 8. The leading `split_point` of the samples build and
/// the rest score; at 1 every sample does both. [`CoverOptions::shrink`] takes
/// effect here, which the reference's optimizer does not honour.
///
/// # Errors
///
/// As [`train_cover_dict`], and when `split_point` exceeds 1, when `k` is
/// below `d`, or when the split leaves fewer than five samples to build or
/// none to score.
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{CoverOptions, FinalizeOptions, optimize_cover_dict};
///
/// let mut samples = Vec::new();
/// let mut sizes = Vec::new();
/// for i in 0..200u32 {
///     let line = format!("tenant=demo table=orders key={i} region=eu status=shipped\n");
///     sizes.push(line.len());
///     samples.extend_from_slice(line.as_bytes());
/// }
/// let (dict, chosen) = optimize_cover_dict(
///     &samples,
///     &sizes,
///     4096,
///     &CoverOptions::default(),
///     FinalizeOptions::default(),
/// )
/// .unwrap();
/// assert!(dict.len() <= 4096 && chosen.k >= 50);
/// ```
pub fn optimize_cover_dict(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &CoverOptions,
    finalize: FinalizeOptions,
) -> io::Result<(Vec<u8>, CoverOptions)> {
    let space = SearchSpace::optimizing(options, 1.0)?;
    run_cover(samples, sample_sizes, dict_size, options, finalize, &space)
}

fn run_cover(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &CoverOptions,
    finalize: FinalizeOptions,
    space: &SearchSpace,
) -> io::Result<(Vec<u8>, CoverOptions)> {
    space.check_fits(dict_size)?;
    let set = check_samples_and_dict_size(samples, sample_sizes, dict_size)?;
    let split = set.split(space.split_point)?;
    let plain = space.k.start() == space.k.end() && space.d.start() == space.d.end();
    // A plain run with no shrinking prices nothing: its one dictionary is the
    // answer, as the reference's plain trainer returns it unscored.
    let scored = !plain || options.shrink.is_some() || space.split_point < 1.0;
    let mut evaluator = selection::Evaluator::new(
        &set,
        split.train,
        split.test.clone(),
        dict_size,
        options.level,
        finalize,
    );
    let mut best = selection::Best::new();
    let mut state = Vec::new();
    let mut content_scratch = Vec::new();
    // `None` beside a `d` is a dmer size these samples cannot index.
    let mut context: Option<(usize, Option<cover::CoverContext<'_>>)> = None;
    for (d, k) in space.pairs() {
        if !segment_fits(k, d, dict_size) {
            continue;
        }
        let chosen = CoverOptions {
            k: k as u32,
            d: d as u32,
            steps: space.steps,
            split_point: space.split_point,
            ..*options
        };
        if context
            .as_ref()
            .is_none_or(|(built_for, _)| *built_for != d)
        {
            // The previous index and its scratch are corpus-sized: released
            // before the next is built, so the two never coexist.
            drop(context.take());
            state = Vec::new();
            // A size the samples cannot index is one failed candidate: the
            // search goes on with the other sizes, and its error is what is
            // returned only if no size builds anything.
            let built = match cover::CoverContext::new(&set, split.train, d) {
                Ok(ctx) => Some(ctx),
                Err(err) => {
                    best.offer(Err(err), chosen);
                    None
                }
            };
            context = Some((d, built));
        }
        let Some(ctx) = context.as_ref().and_then(|(_, built)| built.as_ref()) else {
            continue;
        };
        let content = ctx.build(&mut state, &mut content_scratch, dict_size, k);
        if !scored {
            return Ok((evaluator.finalize(content)?, chosen));
        }
        best.offer(evaluator.select(content, options.shrink), chosen);
    }
    best.finish()
}

/// Train a FastCOVER dictionary of at most `dict_size` bytes with the `k` and
/// `d` given (the reference's `ZDICT_trainFromBuffer_fastCover`).
///
/// As [`train_cover_dict`], over a hashed frequency table of `2^f` counts; the
/// entropy tables are drawn from the share of samples `accel` sets.
///
/// # Errors
///
/// As [`train_cover_dict`], and when `d` is below 4, `f` exceeds 31 or `accel`
/// exceeds 10. A table too large for memory is an `OutOfMemory` error.
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{
///     CoverOptions, FastCoverOptions, FinalizeOptions, train_fastcover_dict,
/// };
///
/// let mut samples = Vec::new();
/// let mut sizes = Vec::new();
/// for i in 0..200u32 {
///     let line = format!("tenant=demo table=orders key={i} region=eu status=shipped\n");
///     sizes.push(line.len());
///     samples.extend_from_slice(line.as_bytes());
/// }
/// let options = FastCoverOptions {
///     cover: CoverOptions { k: 64, ..FastCoverOptions::default().cover },
///     ..FastCoverOptions::default()
/// };
/// let dict = train_fastcover_dict(&samples, &sizes, 4096, &options, FinalizeOptions::default())
///     .unwrap();
/// assert!(dict.len() <= 4096);
/// ```
pub fn train_fastcover_dict(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &FastCoverOptions,
    finalize: FinalizeOptions,
) -> io::Result<Vec<u8>> {
    let space = SearchSpace::fixed(&options.cover)?;
    let (dict, _) = run_fastcover(samples, sample_sizes, dict_size, options, finalize, &space)?;
    Ok(dict)
}

/// Train FastCOVER dictionaries over a range of `k` and `d` and keep the one
/// the scoring samples compress best with (the reference's
/// `ZDICT_optimizeTrainFromBuffer_fastCover`, which `zstd --train` and
/// `ZDICT_trainFromBuffer` run). Returns it with the options that built it.
///
/// The search is [`optimize_cover_dict`]'s; a non-positive `split_point` is
/// 0.75 here.
///
/// # Errors
///
/// As [`optimize_cover_dict`] and [`train_fastcover_dict`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{FastCoverOptions, FinalizeOptions, optimize_fastcover_dict};
///
/// let mut samples = Vec::new();
/// let mut sizes = Vec::new();
/// for i in 0..200u32 {
///     let line = format!("tenant=demo table=orders key={i} region=eu status=shipped\n");
///     sizes.push(line.len());
///     samples.extend_from_slice(line.as_bytes());
/// }
/// let (dict, chosen) = optimize_fastcover_dict(
///     &samples,
///     &sizes,
///     4096,
///     &FastCoverOptions::default(),
///     FinalizeOptions::default(),
/// )
/// .unwrap();
/// assert!(dict.len() <= 4096 && chosen.cover.d == 8);
/// ```
pub fn optimize_fastcover_dict(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &FastCoverOptions,
    finalize: FinalizeOptions,
) -> io::Result<(Vec<u8>, FastCoverOptions)> {
    let space = SearchSpace::optimizing(&options.cover, 0.75)?;
    run_fastcover(samples, sample_sizes, dict_size, options, finalize, &space)
}

fn run_fastcover(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &FastCoverOptions,
    finalize: FinalizeOptions,
    space: &SearchSpace,
) -> io::Result<(Vec<u8>, FastCoverOptions)> {
    let (f, accel) = fastcover_knobs(options, space)?;
    space.check_fits(dict_size)?;
    let set = check_samples_and_dict_size(samples, sample_sizes, dict_size)?;
    let split = set.split(space.split_point)?;
    let plain = space.k.start() == space.k.end() && space.d.start() == space.d.end();
    let scored = !plain || options.cover.shrink.is_some() || space.split_point < 1.0;
    let mut window = fastcover::WindowCounts::default();
    let mut freqs = Vec::new();
    let mut content_scratch = Vec::new();
    let mut best = selection::Best::new();
    // `None` beside a `d` is a dmer size these samples cannot count.
    let mut context: Option<(usize, Option<fastcover::FastCoverContext<'_>>)> = None;
    let mut evaluator: Option<selection::Evaluator<'_>> = None;
    for (d, k) in space.pairs() {
        if !segment_fits(k, d, dict_size) {
            continue;
        }
        let chosen = FastCoverOptions {
            cover: CoverOptions {
                k: k as u32,
                d: d as u32,
                steps: space.steps,
                split_point: space.split_point,
                ..options.cover
            },
            f,
            accel,
        };
        if context
            .as_ref()
            .is_none_or(|(built_for, _)| *built_for != d)
        {
            // The previous count table is released before the next is built.
            drop(context.take());
            // As in the COVER search: a size the samples cannot count is one
            // failed candidate, returned only if no size builds anything.
            let built = match fastcover::FastCoverContext::new(&set, split.train, d, f, accel) {
                Ok(ctx) => Some(ctx),
                Err(err) => {
                    best.offer(Err(err), chosen);
                    None
                }
            };
            context = Some((d, built));
        }
        let Some(ctx) = context.as_ref().and_then(|(_, built)| built.as_ref()) else {
            continue;
        };
        // The finalize share depends on `accel` alone, so every context agrees.
        let evaluator = evaluator.get_or_insert_with(|| {
            selection::Evaluator::new(
                &set,
                ctx.finalize_samples(split.train),
                split.test.clone(),
                dict_size,
                options.cover.level,
                finalize,
            )
        });
        let content = ctx.build(&mut freqs, &mut window, &mut content_scratch, dict_size, k)?;
        if !scored {
            return Ok((evaluator.finalize(content)?, chosen));
        }
        best.offer(evaluator.select(content, options.cover.shrink), chosen);
    }
    best.finish()
}

/// Train and finalize a dictionary with the reference's original trainer, the
/// one `zstd --train-legacy` runs (`ZDICT_trainFromBuffer_legacy`).
///
/// `samples` is every sample back to back and `sample_sizes` their lengths:
/// the trainer searches the corpus as a whole, and the number of samples sets
/// how often a segment has to repeat to be kept, `samples >> selectivity`
/// times and at least 4. A higher `selectivity` keeps more, rarer segments;
/// zero is [`DEFAULT_SELECTIVITY`]. Corpus past 2000 MiB is dropped a whole
/// sample at a time from the end.
///
/// # Errors
///
/// `InvalidInput` when `dict_size` is below 256 bytes, when the corpus is under
/// 512 bytes, when it repeats too little to yield 128 bytes of content, or when
/// `sample_sizes` does not add up to `samples.len()`.
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{create_legacy_dict_from_slice, FinalizeOptions};
///
/// let mut samples = Vec::new();
/// let mut sizes = Vec::new();
/// for i in 0..200u32 {
///     let line = format!("tenant=demo table=orders key={i} region=eu status=shipped\n");
///     sizes.push(line.len());
///     samples.extend_from_slice(line.as_bytes());
/// }
/// let mut dict = Vec::new();
/// create_legacy_dict_from_slice(&samples, &sizes, &mut dict, 4096, 0, FinalizeOptions::default())
///     .unwrap();
/// assert!(dict.starts_with(&[0x37, 0xA4, 0x30, 0xEC]));
/// ```
pub fn create_legacy_dict_from_slice<W: io::Write>(
    samples: &[u8],
    sample_sizes: &[usize],
    output: &mut W,
    dict_size: usize,
    selectivity: u32,
    finalize: FinalizeOptions,
) -> io::Result<()> {
    let described = sample_sizes
        .iter()
        .try_fold(0usize, |total, &size| total.checked_add(size));
    if described != Some(samples.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the sample sizes do not add up to the corpus",
        ));
    }
    let content =
        legacy::train_legacy_raw(samples, sample_sizes, dict_size, selectivity).map_err(|err| {
            let reason = match err {
                legacy::TooSmall::Dictionary => format!(
                    "a legacy dictionary must be at least {} bytes",
                    legacy::DICT_SIZE_MIN
                ),
                legacy::TooSmall::Corpus => format!(
                    "the samples total {} bytes; the legacy trainer needs at least {}",
                    samples.len(),
                    legacy::MIN_SAMPLES_SIZE
                ),
                legacy::TooSmall::Content => {
                    "the samples repeat too little to yield dictionary content".into()
                }
            };
            io::Error::new(io::ErrorKind::InvalidInput, reason)
        })?;
    // Every sample, not only those the content search kept: the reference cuts
    // the corpus to its size limit inside the search alone, for its suffix
    // sort (zdict.c, `ZDICT_trainBuffer_legacy`), and builds the entropy tables
    // from all of them (`ZDICT_trainFromBuffer_unsafe_legacy`).
    let finalized = finalize_raw_dict(content.as_slice(), samples, dict_size, finalize)?;
    output.write_all(finalized.as_slice())
}

/// The legacy trainer's content alone, for the reference comparison in
/// `ffi-bench`.
#[cfg(feature = "bench-internals")]
pub(crate) fn legacy_dict_content(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    selectivity: u32,
) -> Option<Vec<u8>> {
    legacy::train_legacy_raw(samples, sample_sizes, dict_size, selectivity).ok()
}

/// Build a finalized FastCOVER dictionary, attach it to a fastest-level
/// frame compressor, and compress a fresh payload. Returns
/// `(finalized_dictionary, compressed_frame, original_payload)` so a
/// roundtrip check can decode `compressed_frame` against
/// `finalized_dictionary` and compare to `original_payload`. The
/// C-decoder roundtrip that consumes this lives in the `ffi-bench` crate;
/// this side stays pure Rust.
#[cfg(feature = "bench-internals")]
pub(crate) fn dict_roundtrip_fixture() -> (
    alloc::vec::Vec<u8>,
    alloc::vec::Vec<u8>,
    alloc::vec::Vec<u8>,
) {
    use crate::decoding::Dictionary;
    use crate::encoding::{CompressionLevel, FrameCompressor};

    let mut sample = alloc::vec::Vec::new();
    let mut sizes = alloc::vec::Vec::new();
    for i in 0..512u32 {
        let line = alloc::format!(
            "tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbcccccdddddeeeee\n"
        );
        sizes.push(line.len());
        sample.extend_from_slice(line.as_bytes());
    }

    let options = FastCoverOptions {
        cover: CoverOptions {
            k: 256,
            ..FastCoverOptions::default().cover
        },
        ..FastCoverOptions::default()
    };
    let finalized = train_fastcover_dict(
        sample.as_slice(),
        &sizes,
        4096,
        &options,
        FinalizeOptions::default(),
    )
    .expect("training should succeed");
    let parsed =
        Dictionary::decode_dict(finalized.as_slice()).expect("finalized dictionary should parse");
    assert!(!parsed.dict_content.is_empty());

    let mut payload = alloc::vec::Vec::new();
    for idx in 0..96u32 {
        payload.extend_from_slice(
            alloc::format!("tenant=demo op=put key={idx} value=aaaaabbbbbcccccdddddeeeee\n")
                .as_bytes(),
        );
    }

    let mut compressed = alloc::vec::Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor
        .set_dictionary(parsed)
        .expect("dictionary should attach");
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut compressed);
    compressor.compress();

    (finalized, compressed, payload)
}

#[cfg(test)]
mod tests;
