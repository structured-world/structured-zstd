//! Scoring a dictionary by what it saves: finalize the content, compress the
//! scoring samples with it, and count the bytes (upstream zstd `cover.c`,
//! `COVER_selectDict` and `COVER_checkTotalCompressedSize`).

use super::samples::SampleSet;
use super::{FinalizeOptions, finalize_raw_dict};
use crate::encoding::{CompressionLevel, EncoderDictionary, FrameCompressor};
use core::ops::Range;
use std::{io, vec::Vec};

/// Smallest content the shrinking search tries (upstream zstd
/// `ZDICT_DICTSIZE_MIN`).
const SHRINK_START: usize = 256;

/// A finalized dictionary and the bytes it costs: its own size plus every
/// scoring sample compressed with it.
pub(super) struct Scored {
    pub(super) dict: Vec<u8>,
    pub(super) total: usize,
}

/// Finalizes candidate content and prices it against the scoring samples.
///
/// One compressor serves every candidate: each is attached in turn, and the
/// frame buffer is reused across samples.
pub(super) struct Evaluator<'s> {
    samples: &'s SampleSet<'s>,
    /// Samples the entropy tables are drawn from.
    finalize_samples: usize,
    /// Samples a dictionary is scored on.
    scoring: Range<usize>,
    capacity: usize,
    finalize: FinalizeOptions,
    compressor: FrameCompressor,
    frame: Vec<u8>,
}

impl<'s> Evaluator<'s> {
    pub(super) fn new(
        samples: &'s SampleSet<'s>,
        finalize_samples: usize,
        scoring: Range<usize>,
        capacity: usize,
        level: i32,
        finalize: FinalizeOptions,
    ) -> Self {
        Self {
            samples,
            finalize_samples,
            scoring,
            capacity,
            finalize,
            compressor: FrameCompressor::new(CompressionLevel::from_level(level)),
            frame: Vec::new(),
        }
    }

    /// `content` finalized into a dictionary of at most the capacity.
    pub(super) fn finalize(&self, content: &[u8]) -> io::Result<Vec<u8>> {
        finalize_raw_dict(
            content,
            self.samples.leading(self.finalize_samples),
            self.capacity,
            self.finalize,
        )
    }

    /// The dictionary's size plus every scoring sample compressed with it.
    fn price(&mut self, dict: &[u8]) -> io::Result<usize> {
        let prepared = EncoderDictionary::from_bytes(dict)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.compressor
            .set_encoder_dictionary(prepared)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut total = dict.len();
        for index in self.scoring.clone() {
            self.compressor
                .compress_independent_frame_into(self.samples.sample(index), &mut self.frame);
            total += self.frame.len();
        }
        Ok(total)
    }

    /// Finalize `content` and price it. With `shrink`, also try the content's
    /// last 256, 512, ... bytes and keep the first that costs at most
    /// `shrink` percent more than the whole: a smaller dictionary that does
    /// nearly as well.
    ///
    /// Upstream zstd runs the same search but forces it off in the only path
    /// that reaches it, so its `shrink` has no effect; here it takes effect.
    pub(super) fn select(&mut self, content: &[u8], shrink: Option<u32>) -> io::Result<Scored> {
        let dict = self.finalize(content)?;
        let total = self.price(&dict)?;
        let Some(regression) = shrink else {
            return Ok(Scored { dict, total });
        };
        let tolerance = 1.0 + f64::from(regression) / 100.0;
        let mut size = SHRINK_START;
        while size < content.len() {
            let candidate = self.finalize(&content[content.len() - size..])?;
            let candidate_total = self.price(&candidate)?;
            if candidate_total as f64 <= total as f64 * tolerance {
                return Ok(Scored {
                    dict: candidate,
                    total: candidate_total,
                });
            }
            size *= 2;
        }
        Ok(Scored { dict, total })
    }
}

/// The cheapest dictionary seen, and the parameters that built it. A tie keeps
/// the earlier one, as upstream's strict comparison does.
pub(super) struct Best<P> {
    found: Option<(Scored, P)>,
    last_error: Option<io::Error>,
}

impl<P> Best<P> {
    pub(super) fn new() -> Self {
        Self {
            found: None,
            last_error: None,
        }
    }

    pub(super) fn offer(&mut self, candidate: io::Result<Scored>, params: P) {
        match candidate {
            Ok(scored) => {
                if self
                    .found
                    .as_ref()
                    .is_none_or(|(best, _)| scored.total < best.total)
                {
                    self.found = Some((scored, params));
                }
            }
            Err(err) => self.last_error = Some(err),
        }
    }

    /// The winner, or why no candidate could be priced.
    pub(super) fn finish(self) -> io::Result<(Vec<u8>, P)> {
        match (self.found, self.last_error) {
            (Some((scored, params)), _) => Ok((scored.dict, params)),
            (None, Some(err)) => Err(err),
            (None, None) => Err(super::samples::invalid(
                "no parameter combination is valid for this dictionary size",
            )),
        }
    }
}
