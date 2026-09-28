//! Training samples: every sample back to back in one buffer, and where each
//! one ends.

use core::ops::Range;
use std::{io, vec::Vec};

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
                .ok_or_else(|| invalid("the sample sizes add up to more than the samples"))?;
            offsets.push(end);
        }
        if end != data.len() {
            return Err(invalid("the sample sizes do not add up to the samples"));
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

    /// Divide the samples into those a dictionary is built from and those it
    /// is scored on (upstream zstd `COVER_ctx_init`): below 1 the leading
    /// `split_point` share builds and the rest scores; at 1 every sample does
    /// both.
    pub(super) fn split(&self, split_point: f64) -> io::Result<Split> {
        let count = self.count();
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
            return Err(invalid(&std::format!(
                "{} training sample(s) is too few; at least {MIN_TRAIN_SAMPLES} are needed",
                split.train
            )));
        }
        if split.test.is_empty() {
            return Err(invalid(
                "the split leaves no sample to score the dictionary on",
            ));
        }
        Ok(split)
    }
}

/// How the samples divide between building a dictionary and scoring it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Split {
    /// The leading samples a dictionary is built from.
    pub(super) train: usize,
    /// The samples a dictionary is scored on.
    pub(super) test: Range<usize>,
}

pub(super) fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}
