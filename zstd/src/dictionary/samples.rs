//! Training samples: every sample back to back in one buffer, and where each
//! one ends.

use core::ops::Range;
use std::{io, string::String, vec::Vec};

/// Fewest samples the segment trainers build from (upstream zstd `cover.c`,
/// `COVER_ctx_init`): a dmer's frequency is the number of samples holding it,
/// which says nothing when there are only a handful.
pub(super) const MIN_TRAIN_SAMPLES: usize = 5;

/// Samples a trainer reads: sample `i` is `data[offsets[i]..offsets[i + 1]]`.
pub(super) struct SampleSet<'a> {
    data: &'a [u8],
    offsets: Vec<usize>,
}

impl<'a> SampleSet<'a> {
    /// `data` cut into consecutive samples of `sizes` bytes, which must cover it
    /// exactly.
    pub(super) fn new(data: &'a [u8], sizes: &[usize]) -> io::Result<Self> {
        let mut offsets = Vec::with_capacity(sizes.len() + 1);
        offsets.push(0);
        let mut end = 0usize;
        for &size in sizes {
            end = end
                .checked_add(size)
                .filter(|&end| end <= data.len())
                .ok_or_else(|| {
                    refuse(
                        TrainingError::Samples,
                        "the sample sizes add up to more than the samples",
                    )
                })?;
            offsets.push(end);
        }
        if end != data.len() {
            return Err(refuse(
                TrainingError::Samples,
                "the sample sizes do not add up to the samples",
            ));
        }
        Ok(Self { data, offsets })
    }

    pub(super) fn count(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Start of every sample, then the end of the last.
    pub(super) fn offsets(&self) -> &[usize] {
        &self.offsets
    }

    pub(super) fn sample(&self, index: usize) -> &'a [u8] {
        &self.data[self.offsets[index]..self.offsets[index + 1]]
    }

    /// The first `count` samples, back to back.
    pub(super) fn leading(&self, count: usize) -> &'a [u8] {
        &self.data[..self.offsets[count]]
    }

    /// Refuse the first `train` samples when none of them is `span` bytes
    /// long: a trainer counts a dmer only inside one sample, so none would be
    /// counted however long the samples run together. Known from the sizes,
    /// so it is refused before the dmers are indexed.
    pub(super) fn check_holds_dmer(&self, train: usize, span: usize) -> io::Result<()> {
        if self.offsets[..=train]
            .windows(2)
            .any(|bounds| bounds[1] - bounds[0] >= span)
        {
            return Ok(());
        }
        Err(refuse(
            TrainingError::Samples,
            &std::format!("no training sample is {span} bytes long, so none holds a dmer to count"),
        ))
    }

    /// Divide the samples into those a dictionary is built from and those it
    /// is scored on (upstream zstd `COVER_ctx_init`): below 1 the leading
    /// `split_point` share builds and the rest scores; at 1 every sample does
    /// both.
    pub(super) fn split(&self, split_point: f64) -> io::Result<Split> {
        split_count(self.count(), split_point)
    }
}

/// [`SampleSet::split`] of `count` samples, which needs only their number.
pub(super) fn split_count(count: usize, split_point: f64) -> io::Result<Split> {
    let split = if split_point < 1.0 {
        let train = (count as f64 * split_point) as usize;
        Split {
            train,
            test: train..count,
        }
    } else {
        Split {
            train: count,
            test: 0..count,
        }
    };
    if split.train < MIN_TRAIN_SAMPLES {
        return Err(refuse(
            TrainingError::Samples,
            &std::format!(
                "{} training sample(s) is too few; at least {MIN_TRAIN_SAMPLES} are needed",
                split.train
            ),
        ));
    }
    if split.test.is_empty() {
        return Err(refuse(
            TrainingError::Samples,
            "the split leaves no sample to score the dictionary on",
        ));
    }
    Ok(split)
}

/// How the samples divide between building a dictionary and scoring it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Split {
    /// The leading samples a dictionary is built from.
    pub(super) train: usize,
    /// The samples a dictionary is scored on.
    pub(super) test: Range<usize>,
}

/// Why a segment trainer or the finalizer refused its input. It rides inside
/// the `InvalidInput` error returned; [`TrainingError::of`] reads it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrainingError {
    /// A training parameter lies outside its range.
    Parameter,
    /// The samples cannot be trained on: none, too few, or too few or too many
    /// bytes.
    Samples,
    /// The dictionary asked for is smaller than
    /// [`MIN_TRAINED_DICT_SIZE`](super::MIN_TRAINED_DICT_SIZE).
    DictionaryTooSmall,
}

impl TrainingError {
    /// The cause carried by an error a trainer returned, or `None` for one it
    /// did not raise itself (a failed read, write or allocation).
    ///
    /// # Examples
    ///
    /// ```
    /// use structured_zstd::dictionary::{
    ///     CoverOptions, FinalizeOptions, TrainingError, train_cover_dict,
    /// };
    ///
    /// let options = CoverOptions { k: 64, d: 8, ..CoverOptions::default() };
    /// let err = train_cover_dict(&[0; 64], &[64], 100, &options, FinalizeOptions::default())
    ///     .unwrap_err();
    /// assert_eq!(TrainingError::of(&err), Some(TrainingError::DictionaryTooSmall));
    /// ```
    pub fn of(error: &io::Error) -> Option<Self> {
        error
            .get_ref()?
            .downcast_ref::<Refusal>()
            .map(|refusal| refusal.cause)
    }
}

/// A trainer's refusal: its cause, and the reason shown to a person.
#[derive(Debug)]
struct Refusal {
    cause: TrainingError,
    reason: String,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Refusal {}

/// An `InvalidInput` error carrying `cause`.
pub(super) fn refuse(cause: TrainingError, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        Refusal {
            cause,
            reason: reason.into(),
        },
    )
}
