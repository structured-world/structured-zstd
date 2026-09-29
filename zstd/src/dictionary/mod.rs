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
//! [`create_raw_dict_from_slice`] and its reader and directory forms build raw
//! content, with no entropy tables, from a corpus without sample sizes: the
//! FastCOVER search `zstd --train` runs, over the corpus cut into samples.
mod cover;
mod fastcover;
mod finalize;
mod legacy;
mod samples;
mod selection;
mod suffix_array;

pub use legacy::DEFAULT_SELECTIVITY;
pub use samples::TrainingError;
use samples::refuse;
use std::{
    format,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    vec::Vec,
};

const MAX_TRAINING_PREALLOC_BYTES: usize = 8 * 1024 * 1024;

/// Smallest `dict_size` a dictionary is trained or finalized into (upstream
/// zstd `ZDICT_DICTSIZE_MIN`).
///
/// Use it to reject an impossible `dict_size` before spending the corpus. A
/// size at or above it can still be too small once the entropy tables the
/// samples produce are built, which only training can tell.
pub const MIN_TRAINED_DICT_SIZE: usize = finalize::DICT_SIZE_MIN;

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
    /// plus that tail, so none is smaller than the header plus 256 bytes. A
    /// search cuts its winner down this way; the parameters it chooses are
    /// unchanged.
    pub shrink: Option<u32>,
}

impl Default for CoverOptions {
    fn default() -> Self {
        Self {
            k: 0,
            d: 8,
            steps: 4,
            split_point: 1.0,
            shrink: None,
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
    /// Segment size, dmer size, search, split and shrink, as for COVER. `d` is
    /// at least 4 here; the reference takes 6 and 8.
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

/// How a dictionary is finalized and trained against, the reference's
/// `ZDICT_params_t`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FinalizeOptions {
    /// The dictionary id; `None` derives one from the content.
    pub dict_id: Option<u32>,
    /// The compression level the dictionary is built for: the entropy tables
    /// come from samples compressed at it, and the trainers score candidates
    /// at it. Zero is the default level.
    pub level: i32,
}

/// Create a "raw content" dictionary of at most `dict_size` bytes, with no
/// entropy tables, from every file in this directory and its subdirectories,
/// and write it to `output`.
///
/// Each file is one sample, and the content is what [`optimize_fastcover_dict`]
/// picks at its defaults, as `zstd --train` does; too few files to search is
/// trained as [`create_raw_dict_from_slice`] trains an undivided corpus.
///
/// # Errors
/// Returns an error reading the directory or its files, writing to `output`,
/// or allocating the trainer's tables.
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

    // Every file is one sample.
    let mut corpus = Vec::new();
    let mut sizes = Vec::with_capacity(file_paths.len());
    for path in file_paths {
        let before = corpus.len();
        File::open(path)?.read_to_end(&mut corpus)?;
        sizes.push(corpus.len() - before);
    }
    output.write_all(&fastcover_raw_content(&corpus, Some(&sizes), dict_size)?)
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
/// [`create_raw_dict_from_slice`], whose training this is.
///
/// # Errors
/// Returns an error reading `source`, writing to `output`, or allocating the
/// trainer's tables.
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

/// Create a "raw content" dictionary of at most `dict_size` bytes, with no
/// entropy tables, from a corpus already in memory, writing it to `output`.
///
/// The corpus has no sample sizes, so it is cut into at least sixteen samples
/// of at most 128 KiB, and the content is what [`optimize_fastcover_dict`]
/// picks over them at its defaults, as `zstd --train` does. A corpus no larger
/// than `dict_size` is its own content; one the trainer refuses (a
/// `dict_size` under [`MIN_TRAINED_DICT_SIZE`], too little to search) gives
/// its last `dict_size` bytes.
///
/// # Errors
/// Returns an error writing to `output` or allocating the trainer's tables.
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
    output.write_all(&fastcover_raw_content(all, None, dict_size)?)
}

/// Largest sample an undivided corpus is cut into: upstream zstd's largest
/// block, the most of a sample a dictionary's statistics read.
const RAW_SAMPLE_MAX: usize = 128 << 10;
/// Fewest samples an undivided corpus is cut into, so the search has samples
/// to build from and samples to score on.
const RAW_SAMPLES_MIN: usize = 16;

/// Raw content of at most `dict_size` bytes from `corpus`, as `zstd --train`
/// picks it: FastCOVER searching `k` over the samples `sizes` cuts, or over
/// even cuts of the corpus when there are none or too few to search.
fn fastcover_raw_content(
    corpus: &[u8],
    sizes: Option<&[usize]>,
    dict_size: usize,
) -> io::Result<Vec<u8>> {
    if corpus.len() <= dict_size {
        return Ok(corpus.to_vec());
    }
    let refused =
        |result: &io::Result<Vec<u8>>| result.as_ref().err().and_then(TrainingError::of).is_some();
    if let Some(sizes) = sizes {
        let trained = search_raw_content(corpus, sizes, dict_size);
        if !refused(&trained) {
            return trained;
        }
    }
    let cut = corpus.len().div_ceil(RAW_SAMPLES_MIN).min(RAW_SAMPLE_MAX);
    let even: Vec<usize> = corpus.chunks(cut).map(<[u8]>::len).collect();
    let trained = search_raw_content(corpus, &even, dict_size);
    if refused(&trained) {
        return Ok(corpus[corpus.len() - dict_size..].to_vec());
    }
    trained
}

/// The content of the dictionary [`optimize_fastcover_dict`] picks at its
/// defaults: the search keeps its winner's content and hands that back.
fn search_raw_content(corpus: &[u8], sizes: &[usize], dict_size: usize) -> io::Result<Vec<u8>> {
    let options = FastCoverOptions::default();
    let space = SearchSpace::optimizing(&options.cover, 0.75)?;
    let (content, _) = run_fastcover(
        corpus,
        sizes,
        dict_size,
        &options,
        FinalizeOptions::default(),
        &space,
        selection::Keep::Content,
    )?;
    Ok(content)
}

/// Finalize raw dictionary content into a dictionary of at most `dict_size`
/// bytes: magic, id, entropy tables, repeat offsets and the content (the
/// reference's `ZDICT_finalizeDictionary`).
///
/// The entropy tables are measured, not guessed: the first block of each
/// sample is compressed with `raw_content` as a raw dictionary at
/// [`FinalizeOptions::level`], and the tables describe the literals and
/// sequences those blocks produced. `samples` is every sample back to back and
/// `sample_sizes` their lengths. Content that does not fit after the header is
/// cut from the front: trainers place their best segments last.
///
/// # Errors
///
/// `InvalidInput` when `raw_content` is empty, when `dict_size` is below
/// [`MIN_TRAINED_DICT_SIZE`] or leaves less than eight bytes of content after
/// the header, when the id is zero, or when `sample_sizes` does not add up to
/// `samples.len()`; a size too small carries
/// [`TrainingError::DictionaryTooSmall`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{FinalizeOptions, finalize_raw_dict};
///
/// let mut samples = Vec::new();
/// let mut sizes = Vec::new();
/// for i in 0..50u32 {
///     let line = format!("tenant=demo table=orders key={i} status=shipped\n");
///     sizes.push(line.len());
///     samples.extend_from_slice(line.as_bytes());
/// }
/// let content = b"tenant=demo table=orders key= status=shipped\n";
/// let dict = finalize_raw_dict(content, &samples, &sizes, 1024, FinalizeOptions::default())
///     .unwrap();
/// assert!(dict.ends_with(content));
/// ```
pub fn finalize_raw_dict(
    raw_content: &[u8],
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: FinalizeOptions,
) -> io::Result<Vec<u8>> {
    check_finalize_dict_size(dict_size)?;
    let set = samples::SampleSet::new(samples, sample_sizes)?;
    finalize::finalize(raw_content, &set, set.count(), dict_size, options)
}

/// Refuse a `dict_size` [`finalize_raw_dict`] cannot fill, from the size
/// alone: below [`MIN_TRAINED_DICT_SIZE`]. Finalizing checks it first, before
/// the samples; a caller holding the samples somewhere costly to walk can run
/// it before that.
///
/// # Errors
///
/// `InvalidInput` carrying [`TrainingError::DictionaryTooSmall`].
///
/// # Examples
///
/// ```
/// use structured_zstd::dictionary::{
///     MIN_TRAINED_DICT_SIZE, TrainingError, check_finalize_dict_size,
/// };
///
/// assert!(check_finalize_dict_size(MIN_TRAINED_DICT_SIZE).is_ok());
/// let err = check_finalize_dict_size(MIN_TRAINED_DICT_SIZE - 1).unwrap_err();
/// assert_eq!(TrainingError::of(&err), Some(TrainingError::DictionaryTooSmall));
/// ```
pub fn check_finalize_dict_size(dict_size: usize) -> io::Result<()> {
    finalize::check_size(dict_size)
}

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
/// [`MIN_TRAINED_DICT_SIZE`], where it reports
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
    if dict_size < MIN_TRAINED_DICT_SIZE {
        return Err(refuse(
            TrainingError::DictionaryTooSmall,
            &format!("a dictionary must be at least {MIN_TRAINED_DICT_SIZE} bytes"),
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
/// `dict_size`, when `dict_size` is under [`MIN_TRAINED_DICT_SIZE`], when there
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
    let mut evaluator =
        selection::Evaluator::new(&set, split.train, split.test.clone(), dict_size, finalize);
    let mut best = selection::Best::new(selection::Keep::Dictionary {
        shrink: options.shrink,
    });
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
                    best.offer(Err(err), &[], chosen);
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
        // Ranked at full size; `finish` shrinks only the winner, as
        // `CoverOptions::shrink` documents. Upstream's optimizer never applies
        // its shrink at all, and cutting every candidate would multiply the
        // search by the number of sizes tried.
        best.offer(evaluator.score(content), content, chosen);
    }
    best.finish(&mut evaluator)
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
    let keep = selection::Keep::Dictionary {
        shrink: options.cover.shrink,
    };
    let (dict, _) = run_fastcover(
        samples,
        sample_sizes,
        dict_size,
        options,
        finalize,
        &space,
        keep,
    )?;
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
    let keep = selection::Keep::Dictionary {
        shrink: options.cover.shrink,
    };
    run_fastcover(
        samples,
        sample_sizes,
        dict_size,
        options,
        finalize,
        &space,
        keep,
    )
}

fn run_fastcover(
    samples: &[u8],
    sample_sizes: &[usize],
    dict_size: usize,
    options: &FastCoverOptions,
    finalize: FinalizeOptions,
    space: &SearchSpace,
    keep: selection::Keep,
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
    let mut best = selection::Best::new(keep);
    // `None` beside a `d` is a dmer size these samples cannot count.
    let mut context: Option<(usize, Option<fastcover::FastCoverContext<'_>>)> = None;
    let mut evaluator = selection::Evaluator::new(
        &set,
        fastcover::finalize_samples(split.train, accel),
        split.test.clone(),
        dict_size,
        finalize,
    );
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
                    best.offer(Err(err), &[], chosen);
                    None
                }
            };
            context = Some((d, built));
        }
        let Some(ctx) = context.as_ref().and_then(|(_, built)| built.as_ref()) else {
            continue;
        };
        let content = ctx.build(&mut freqs, &mut window, &mut content_scratch, dict_size, k)?;
        if !scored {
            return match keep {
                selection::Keep::Content => Ok((content.to_vec(), chosen)),
                selection::Keep::Dictionary { .. } => Ok((evaluator.finalize(content)?, chosen)),
            };
        }
        // Ranked at full size, the winner alone shrunk, as in the COVER search.
        best.offer(evaluator.score(content), content, chosen);
    }
    best.finish(&mut evaluator)
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
    let finalized = finalize_raw_dict(
        content.as_slice(),
        samples,
        sample_sizes,
        dict_size,
        finalize,
    )?;
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
