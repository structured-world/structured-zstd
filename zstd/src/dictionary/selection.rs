//! Scoring a dictionary by what it saves: finalize the content, compress the
//! scoring samples with it, and count the bytes (upstream zstd `cover.c`,
//! `COVER_selectDict` and `COVER_checkTotalCompressedSize`).

use super::samples::SampleSet;
use super::{FinalizeOptions, assemble_dict, finalize_raw_dict, sample_entropy_tables};
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
/// frame buffer is reused across samples. The compressor is built at the
/// first pricing, so a run that only finalizes never pays for it.
pub(super) struct Evaluator<'s> {
    samples: &'s SampleSet<'s>,
    /// Samples the entropy tables are drawn from.
    finalize_samples: usize,
    /// Samples a dictionary is scored on.
    scoring: Range<usize>,
    capacity: usize,
    level: i32,
    finalize: FinalizeOptions,
    /// The entropy tables, built once: they come from the finalize samples,
    /// the same for every candidate. `None` where the samples are too thin
    /// and each candidate's content decides them.
    tables: Option<Vec<u8>>,
    compressor: Option<FrameCompressor>,
    frame: Vec<u8>,
    /// The candidate being priced, and a shrunk one being tried against it:
    /// finalized over the same allocations for every candidate, and copied out
    /// only by a candidate that wins.
    dict: Vec<u8>,
    shrunk: Vec<u8>,
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
            level,
            finalize,
            tables: sample_entropy_tables(samples.leading(finalize_samples)),
            compressor: None,
            frame: Vec::new(),
            dict: Vec::new(),
            shrunk: Vec::new(),
        }
    }

    /// `content` finalized into a dictionary of at most the capacity.
    pub(super) fn finalize(&self, content: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.finalize_into(content, &mut out)?;
        Ok(out)
    }

    /// [`Self::finalize`] over `out`, keeping its allocation.
    fn finalize_into(&self, content: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        if let Some(tables) = &self.tables {
            if content.is_empty() {
                // The full path owns that refusal and its wording.
                *out = finalize_raw_dict(content, &[], self.capacity, self.finalize)?;
                return Ok(());
            }
            return assemble_dict(out, content, tables, self.capacity, self.finalize);
        }
        // Samples too thin to decide the tables alone: the content decides
        // them, a path a search takes only on corpora of a handful of bytes.
        *out = finalize_raw_dict(
            content,
            self.samples.leading(self.finalize_samples),
            self.capacity,
            self.finalize,
        )?;
        Ok(())
    }

    /// `content` finalized over `buffer` and priced.
    fn finalize_and_price(&mut self, content: &[u8], buffer: &mut Vec<u8>) -> io::Result<usize> {
        self.finalize_into(content, buffer)?;
        self.price(buffer)
    }

    /// The dictionary's size plus every scoring sample compressed with it.
    fn price(&mut self, dict: &[u8]) -> io::Result<usize> {
        // Each candidate is a new dictionary, prepared by copy as upstream zstd
        // prepares one per candidate (`ZSTD_createCDict` in
        // `COVER_checkTotalCompressedSize`). The copy and parse are 0.016% of a
        // COVER search under callgrind; borrowing would put a lifetime on the
        // encoder dictionary for none of it. The cost is in re-priming the
        // matcher for it, which belongs to the compressor, not here.
        let prepared = EncoderDictionary::from_bytes(dict)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let level = self.level;
        let compressor = self
            .compressor
            .get_or_insert_with(|| FrameCompressor::new(CompressionLevel::from_level(level)));
        compressor
            .set_encoder_dictionary(prepared)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut total = dict.len();
        for index in self.scoring.clone() {
            compressor.compress_independent_frame_into(self.samples.sample(index), &mut self.frame);
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
    pub(super) fn select(&mut self, content: &[u8], shrink: Option<u32>) -> io::Result<Priced<'_>> {
        let mut dict = core::mem::take(&mut self.dict);
        let priced = self.finalize_and_price(content, &mut dict);
        self.dict = dict;
        let total = priced?;
        let Some(regression) = shrink else {
            return Ok(Priced {
                dict: &self.dict,
                total,
            });
        };
        let mut size = SHRINK_START;
        while size < content.len() {
            let mut shrunk = core::mem::take(&mut self.shrunk);
            let priced = self.finalize_and_price(&content[content.len() - size..], &mut shrunk);
            self.shrunk = shrunk;
            let candidate_total = priced?;
            if within_regression(candidate_total, total, regression) {
                return Ok(Priced {
                    dict: &self.shrunk,
                    total: candidate_total,
                });
            }
            size *= 2;
        }
        Ok(Priced {
            dict: &self.dict,
            total,
        })
    }
}

/// Whether `total` costs at most `regression` percent more than `full`,
/// compared in integers: upstream zstd's `COVER_selectDict` multiplies by a
/// `double` tolerance, which can land just under an exact bound and reject a
/// size that meets it. Every product fits `u128` (a `usize` times at most
/// `u32::MAX + 100`).
pub(super) fn within_regression(total: usize, full: usize, regression: u32) -> bool {
    total as u128 * 100 <= full as u128 * (u128::from(regression) + 100)
}

/// A candidate [`Evaluator::select`] priced, still in the evaluator's buffer.
pub(super) struct Priced<'e> {
    pub(super) dict: &'e [u8],
    pub(super) total: usize,
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

    /// Keep `candidate` if it beats the best so far, copying its bytes into the
    /// buffer the previous winner held; a candidate that loses is not copied.
    pub(super) fn offer(&mut self, candidate: io::Result<Priced<'_>>, params: P) {
        match candidate {
            Ok(priced) => {
                if self
                    .found
                    .as_ref()
                    .is_none_or(|(best, _)| priced.total < best.total)
                {
                    let mut dict = self
                        .found
                        .take()
                        .map_or_else(Vec::new, |(previous, _)| previous.dict);
                    dict.clear();
                    dict.extend_from_slice(priced.dict);
                    self.found = Some((
                        Scored {
                            dict,
                            total: priced.total,
                        },
                        params,
                    ));
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
            (None, None) => Err(super::samples::refuse(
                super::TrainingError::Parameter,
                "no parameter combination is valid for this dictionary size",
            )),
        }
    }
}
