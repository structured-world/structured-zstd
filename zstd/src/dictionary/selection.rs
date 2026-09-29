//! Scoring a dictionary by what it saves: finalize the content, compress the
//! scoring samples with it, and count the bytes (upstream zstd `cover.c`,
//! `COVER_selectDict` and `COVER_checkTotalCompressedSize`).

use super::FinalizeOptions;
use super::finalize::{Analysis, finalize_into};
use super::samples::SampleSet;
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
    finalize: FinalizeOptions,
    compressor: Option<FrameCompressor>,
    frame: Vec<u8>,
    /// The candidate being priced, and a shrunk one being tried against it:
    /// finalized over the same allocations for every candidate, and copied out
    /// only by a candidate that wins.
    dict: Vec<u8>,
    shrunk: Vec<u8>,
    /// The entropy analysis every candidate is finalized through.
    analysis: Analysis<'s>,
}

impl<'s> Evaluator<'s> {
    pub(super) fn new(
        samples: &'s SampleSet<'s>,
        finalize_samples: usize,
        scoring: Range<usize>,
        capacity: usize,
        finalize: FinalizeOptions,
    ) -> Self {
        Self {
            samples,
            finalize_samples,
            scoring,
            capacity,
            finalize,
            compressor: None,
            frame: Vec::new(),
            dict: Vec::new(),
            shrunk: Vec::new(),
            analysis: Analysis::default(),
        }
    }

    /// `content` finalized into a dictionary of at most the capacity.
    pub(super) fn finalize(&mut self, content: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.finalize_into(content, &mut out)?;
        Ok(out)
    }

    /// [`Self::finalize`] over `out`, keeping its allocation.
    fn finalize_into(&mut self, content: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        finalize_into(
            out,
            &mut self.analysis,
            content,
            self.samples,
            self.finalize_samples,
            self.capacity,
            self.finalize,
        )
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
        // Scored at the level the dictionary is finalized for, as upstream uses
        // its one `compressionLevel` for both.
        let level = self.finalize.level;
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

    /// Finalize `content` and price it, in the evaluator's candidate buffer.
    pub(super) fn score(&mut self, content: &[u8]) -> io::Result<Priced<'_>> {
        let mut dict = core::mem::take(&mut self.dict);
        let priced = self.finalize_and_price(content, &mut dict);
        self.dict = dict;
        Ok(Priced {
            total: priced?,
            dict: &self.dict,
        })
    }

    /// Try the last 256, 512, ... bytes of `content` and keep the first that
    /// costs at most `regression` percent more than `full`, the whole
    /// content's score: a smaller dictionary that does nearly as well.
    ///
    /// Upstream zstd runs the same search for every candidate of its
    /// parameter search, but forces it off in the only path that reaches it,
    /// so its `shrink` has no effect. Here it runs once, on the winner: the
    /// size a dictionary is cut to is a separate choice from the `k` and `d`
    /// that built it, and searching it per candidate multiplies the cost of
    /// the whole search by the number of sizes tried.
    pub(super) fn shrink(
        &mut self,
        content: &[u8],
        full: Scored,
        regression: u32,
    ) -> io::Result<Scored> {
        let mut size = SHRINK_START;
        while size < content.len() {
            let mut shrunk = core::mem::take(&mut self.shrunk);
            let priced = self.finalize_and_price(&content[content.len() - size..], &mut shrunk);
            let total = priced?;
            if within_regression(total, full.total, regression) {
                return Ok(Scored {
                    dict: shrunk,
                    total,
                });
            }
            self.shrunk = shrunk;
            size *= 2;
        }
        Ok(full)
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

/// A candidate [`Evaluator::score`] priced, still in the evaluator's buffer.
pub(super) struct Priced<'e> {
    pub(super) dict: &'e [u8],
    pub(super) total: usize,
}

/// What a search hands back of its winner.
#[derive(Clone, Copy)]
pub(super) enum Keep {
    /// The finalized dictionary, cut down with `shrink` when given.
    Dictionary { shrink: Option<u32> },
    /// The raw content the dictionary was finalized from.
    Content,
}

impl Keep {
    /// Whether the winner's content has to be kept through the search.
    fn content(self) -> bool {
        !matches!(self, Self::Dictionary { shrink: None })
    }

    /// Whether the winner's finalized dictionary is what is handed back.
    fn dictionary(self) -> bool {
        matches!(self, Self::Dictionary { .. })
    }
}

/// The cheapest candidate seen, the parameters that built it, and what
/// [`Keep`] asks for of it: its dictionary, its content, or both. A tie keeps
/// the earlier one, as upstream's strict comparison does.
///
/// Candidates are ranked at full size and only the winner is shrunk: the size
/// a dictionary is cut to is a separate choice from the `k` and `d` that built
/// it (see [`Evaluator::shrink`]).
pub(super) struct Best<P> {
    found: Option<(Scored, Vec<u8>, P)>,
    last_error: Option<io::Error>,
    keep: Keep,
}

impl<P> Best<P> {
    /// A search handing back what `keep` names of its winner.
    pub(super) fn new(keep: Keep) -> Self {
        Self {
            found: None,
            last_error: None,
            keep,
        }
    }

    /// Keep `candidate` if it is the cheapest so far. What [`Keep`] asks for
    /// of it is copied only then, into the buffers the previous winner held;
    /// a candidate that loses is not copied at all.
    pub(super) fn offer(&mut self, candidate: io::Result<Priced<'_>>, content: &[u8], params: P) {
        let content = if self.keep.content() { content } else { &[] };
        let keep_dictionary = self.keep.dictionary();
        match candidate {
            Ok(priced) => {
                let dict = if keep_dictionary { priced.dict } else { &[] };
                match &mut self.found {
                    Some((best, _, _)) if priced.total >= best.total => {}
                    Some((best, kept, kept_params)) => {
                        best.total = priced.total;
                        best.dict.clear();
                        best.dict.extend_from_slice(dict);
                        kept.clear();
                        kept.extend_from_slice(content);
                        *kept_params = params;
                    }
                    None => {
                        let best = Scored {
                            dict: dict.to_vec(),
                            total: priced.total,
                        };
                        self.found = Some((best, content.to_vec(), params));
                    }
                }
            }
            Err(err) => self.last_error = Some(err),
        }
    }

    /// What [`Keep`] asked for of the winner, and its parameters; or why no
    /// candidate could be priced.
    pub(super) fn finish(self, evaluator: &mut Evaluator<'_>) -> io::Result<(Vec<u8>, P)> {
        let keep = self.keep;
        let (full, content, params) = match (self.found, self.last_error) {
            (Some(found), _) => found,
            (None, Some(err)) => return Err(err),
            (None, None) => {
                return Err(super::samples::refuse(
                    super::TrainingError::Parameter,
                    "no parameter combination is valid for this dictionary size",
                ));
            }
        };
        match keep {
            Keep::Content => Ok((content, params)),
            Keep::Dictionary { shrink: None } => Ok((full.dict, params)),
            Keep::Dictionary {
                shrink: Some(regression),
            } => Ok((evaluator.shrink(&content, full, regression)?.dict, params)),
        }
    }
}
