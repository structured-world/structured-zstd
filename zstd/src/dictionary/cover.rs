//! COVER: dictionary content made of the segments that cover the dmers most
//! samples share (upstream zstd `cover.c`, after Liao, Petri, Moffat and Wirth,
//! "Effective Construction of Relative Lempel-Ziv Dictionaries").
//!
//! A dmer is the `d` bytes starting at a position of the training samples; its
//! frequency is the number of samples it occurs in, because a dictionary only
//! saves the first reference to it in each sample. The corpus is cut into
//! epochs, and each visit to an epoch takes the `k`-byte segment whose distinct
//! dmers are worth the most, then zeroes their frequencies so later segments
//! are valued for new coverage only.

use super::samples::{SampleSet, TrainingError, refuse};
use std::{io, vec, vec::Vec};

/// A segment of the training bytes, in dmer positions.
#[derive(Clone, Copy, Default)]
struct Segment {
    begin: usize,
    end: usize,
    score: u64,
}

/// How the dmers are divided into epochs: `num` epochs of `size` dmers each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Epochs {
    pub(super) num: usize,
    pub(super) size: usize,
}

impl Epochs {
    /// The dmers epoch `epoch` spans. The last one runs to the end of the
    /// corpus: `num * size` can fall short of it by up to one epoch, which
    /// upstream zstd never scans. Stretching the last epoch covers that tail
    /// at the cost of one longer epoch per cycle; widening every epoch to share
    /// it scans more on every visit and measured about a tenth more work.
    #[inline]
    pub(super) fn bounds(self, epoch: usize, nb_dmers: usize) -> (usize, usize) {
        let begin = epoch * self.size;
        let end = if epoch + 1 == self.num {
            nb_dmers
        } else {
            begin + self.size
        };
        (begin, end)
    }
}

/// Upstream zstd `COVER_computeEpochs`: aim for `passes` selections per epoch
/// over a dictionary of `max_dict_size`, but keep an epoch at least ten
/// segments long so it can hold a useful one. `nb_dmers` is at least one.
pub(super) fn compute_epochs(
    max_dict_size: usize,
    nb_dmers: usize,
    k: usize,
    passes: usize,
) -> Epochs {
    debug_assert!(nb_dmers > 0 && k > 0 && passes > 0);
    // Ten segments, or the whole corpus when that is shorter; asked by
    // division so a `k` near the top of `usize` needs no product.
    let min_epoch_size = if k > nb_dmers / 10 { nb_dmers } else { k * 10 };
    let num = (max_dict_size / k / passes).max(1);
    let size = nb_dmers / num;
    if size >= min_epoch_size {
        return Epochs { num, size };
    }
    Epochs {
        num: nb_dmers / min_epoch_size,
        size: min_epoch_size,
    }
}

/// A dmer's standing while dictionaries are built: how many samples hold it
/// that no chosen segment covers yet, and how often it occurs in the candidate
/// window. Kept side by side so the window touches one slot per position.
#[derive(Clone, Copy, Default)]
pub(super) struct DmerState {
    freq: u32,
    active: u32,
}

/// Every position of the training samples mapped to its dmer, and each dmer's
/// frequency. Depends on `d` alone, so one context serves every `k`.
pub(super) struct CoverContext<'s> {
    data: &'s [u8],
    /// The dmer id starting at each position.
    dmer_at: Vec<u32>,
    /// Frequency of each dmer id, window counts all zero.
    initial: Vec<DmerState>,
    d: usize,
}

impl<'s> CoverContext<'s> {
    /// Index the dmers of the first `train` samples.
    pub(super) fn new(samples: &SampleSet<'s>, train: usize, d: usize) -> io::Result<Self> {
        debug_assert!(d > 0);
        let data = samples.leading(train);
        // Short dmers are compared as one 8-byte read, so every position needs
        // eight readable bytes (upstream zstd `COVER_ctx_init`).
        let read_len = d.max(8);
        let Some(nb_dmers) = data.len().checked_sub(read_len).map(|n| n + 1) else {
            return Err(refuse(
                TrainingError::Samples,
                &std::format!(
                    "the training samples total {} bytes; COVER needs at least {read_len}",
                    data.len()
                ),
            ));
        };
        // Ids and positions are held as `u32`, as upstream's are.
        if u32::try_from(nb_dmers).is_err() {
            return Err(refuse(
                TrainingError::Samples,
                "the training samples are too large for COVER (4 GiB at most)",
            ));
        }
        samples.check_holds_dmer(train, d)?;
        let (dmer_at, initial) = if d <= 8 {
            index_dmers::<false>(data, nb_dmers, d, samples.offsets())
        } else {
            index_dmers::<true>(data, nb_dmers, d, samples.offsets())
        };
        Ok(Self {
            data,
            dmer_at,
            initial,
            d,
        })
    }

    /// Build content of at most `capacity` bytes from segments of `k` bytes
    /// (upstream zstd `COVER_buildDictionary`). `state` is scratch the build
    /// refills with the frequencies and spends, so one buffer serves every
    /// build; `out` is scratch the content is written into, and the returned
    /// slice borrows it. The best segments are chosen first and placed last,
    /// where the offsets that reach them are smallest.
    pub(super) fn build<'o>(
        &self,
        state: &mut Vec<DmerState>,
        out: &'o mut Vec<u8>,
        capacity: usize,
        k: usize,
    ) -> &'o [u8] {
        state.clear();
        state.extend_from_slice(&self.initial);
        let d = self.d;
        debug_assert!(d <= k);
        let nb_dmers = self.dmer_at.len();
        let epochs = compute_epochs(capacity, nb_dmers, k, 4);
        let max_zero_score_run = (epochs.num >> 3).clamp(10, 100);
        let dmers_in_k = k - d + 1;
        // Only bytes past the final `tail` are returned, so what an earlier
        // build left before it needs no clearing.
        out.resize(capacity, 0);
        let mut tail = capacity;
        let mut zero_score_run = 0usize;
        let mut epoch = 0usize;
        while tail > 0 {
            let (begin, end) = epochs.bounds(epoch, nb_dmers);
            epoch = (epoch + 1) % epochs.num;
            let segment = select_segment(&self.dmer_at, state, begin, end, dmers_in_k);
            if segment.score == 0 {
                // This epoch is spent; others may not be yet.
                zero_score_run += 1;
                if zero_score_run >= max_zero_score_run {
                    break;
                }
                continue;
            }
            zero_score_run = 0;
            let segment_size = (segment.end - segment.begin + d - 1).min(tail);
            if segment_size < d {
                break;
            }
            tail -= segment_size;
            out[tail..tail + segment_size]
                .copy_from_slice(&self.data[segment.begin..segment.begin + segment_size]);
        }
        &out[tail..]
    }
}

/// Slide a window of `dmers_in_k` dmers across `begin..end` and take the one
/// whose distinct dmers carry the most frequency (upstream zstd
/// `COVER_selectSegment`), trimmed of dmers worth nothing at either end. The
/// chosen dmers' frequencies are zeroed; window counts are left at zero.
fn select_segment(
    dmer_at: &[u32],
    state: &mut [DmerState],
    begin: usize,
    end: usize,
    dmers_in_k: usize,
) -> Segment {
    let mut best = Segment::default();
    let mut active_begin = begin;
    let mut active_end = begin;
    let mut score = 0u64;
    while active_end < end {
        let dmer = &mut state[dmer_at[active_end] as usize];
        if dmer.active == 0 {
            score += u64::from(dmer.freq);
        }
        dmer.active += 1;
        active_end += 1;
        if active_end - active_begin == dmers_in_k + 1 {
            let dmer = &mut state[dmer_at[active_begin] as usize];
            active_begin += 1;
            dmer.active -= 1;
            if dmer.active == 0 {
                score -= u64::from(dmer.freq);
            }
        }
        if score > best.score {
            best = Segment {
                begin: active_begin,
                end: active_end,
                score,
            };
        }
    }
    for &id in &dmer_at[active_begin..active_end] {
        state[id as usize].active = 0;
    }
    // Trim the zero-frequency head and tail.
    let mut trimmed_begin = best.end;
    let mut trimmed_end = best.begin;
    for pos in best.begin..best.end {
        if state[dmer_at[pos] as usize].freq != 0 {
            trimmed_begin = trimmed_begin.min(pos);
            trimmed_end = pos + 1;
        }
    }
    best.begin = trimmed_begin;
    best.end = trimmed_end.max(trimmed_begin);
    for &id in &dmer_at[best.begin..best.end] {
        state[id as usize].freq = 0;
    }
    best
}

const EMPTY: u32 = u32::MAX;
const HASH_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

/// The first `d` bytes at `pos` as a little-endian key; `d <= 8` and eight
/// bytes are readable there.
#[inline]
fn short_key(data: &[u8], pos: usize, mask: u64) -> u64 {
    let word: [u8; 8] = data[pos..pos + 8].try_into().expect("eight readable bytes");
    u64::from_le_bytes(word) & mask
}

/// A 64-bit digest of the `d` bytes at `pos`, for dmers longer than one word;
/// equal digests are confirmed against the bytes.
fn long_digest(data: &[u8], pos: usize, d: usize) -> u64 {
    let mut digest = d as u64;
    for chunk in data[pos..pos + d].chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        digest = (digest ^ u64::from_le_bytes(word))
            .wrapping_mul(HASH_MULTIPLIER)
            .rotate_left(29);
    }
    digest
}

/// Give every distinct dmer of `data` a dense id and count the samples it
/// occurs in: returns the id at each position and each id's frequency, with
/// window counts zero.
///
/// Upstream zstd groups positions by sorting them on their dmer; ids here come
/// from a hash index in one pass instead. Which label a dmer carries does not
/// change a single selection, and the pass does no comparison sort. A dmer is
/// counted once per sample it lies wholly inside.
///
/// Memory stays within a few words per position however many distinct dmers
/// there are: a slot holds only the first position of its dmer (its id is
/// `dmer_at` there, and its key is read back from `data`), and the sample a
/// dmer was last counted in lives in the window-count field it returns zeroed.
fn index_dmers<const LONG: bool>(
    data: &[u8],
    nb_dmers: usize,
    d: usize,
    offsets: &[usize],
) -> (Vec<u32>, Vec<DmerState>) {
    let mask = if d >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * d)) - 1
    };
    let tag_at = |pos: usize| {
        if LONG {
            long_digest(data, pos, d)
        } else {
            short_key(data, pos, mask)
        }
    };
    let same_dmer = |a: usize, b: usize| {
        if LONG {
            data[a..a + d] == data[b..b + d]
        } else {
            short_key(data, a, mask) == short_key(data, b, mask)
        }
    };
    // Slots hold the first position of a dmer; positions fit `u32` below
    // `EMPTY`, which the caller's size check guarantees.
    let mut slots: Vec<u32> = vec![EMPTY; 1 << 12];
    let mut shift = 64 - 12;
    // `active` holds the last sample a dmer was counted in until the end.
    let mut dmers: Vec<DmerState> = Vec::new();
    let mut dmer_at: Vec<u32> = Vec::with_capacity(nb_dmers);
    let mut sample = 0usize;
    for pos in 0..nb_dmers {
        while offsets[sample + 1] <= pos {
            sample += 1;
        }
        let mask = slots.len() - 1;
        let mut slot = (tag_at(pos).wrapping_mul(HASH_MULTIPLIER) >> shift) as usize;
        let id = loop {
            let first = slots[slot];
            if first == EMPTY {
                let id = dmers.len() as u32;
                slots[slot] = pos as u32;
                dmers.push(DmerState {
                    freq: 0,
                    active: EMPTY,
                });
                break id;
            }
            if same_dmer(first as usize, pos) {
                break dmer_at[first as usize];
            }
            slot = (slot + 1) & mask;
        };
        // A dmer spilling into the next sample exists only in the
        // concatenation, so it earns nothing.
        let dmer = &mut dmers[id as usize];
        if pos + d <= offsets[sample + 1] && dmer.active != sample as u32 {
            dmer.active = sample as u32;
            dmer.freq += 1;
        }
        dmer_at.push(id);
        // Keep the load at or under a half. First occurrences appear in
        // `dmer_at` in id order, so the new table is filled from it without
        // keeping the old one.
        if dmers.len() * 2 > slots.len() {
            let len = slots.len() * 2;
            shift -= 1;
            slots.clear();
            slots.resize(len, EMPTY);
            let mut next = 0u32;
            for (first, &id) in dmer_at.iter().enumerate() {
                if id != next {
                    continue;
                }
                next += 1;
                let mut slot = (tag_at(first).wrapping_mul(HASH_MULTIPLIER) >> shift) as usize;
                while slots[slot] != EMPTY {
                    slot = (slot + 1) & (len - 1);
                }
                slots[slot] = first as u32;
                if next as usize == dmers.len() {
                    break;
                }
            }
        }
    }
    for dmer in &mut dmers {
        dmer.active = 0;
    }
    (dmer_at, dmers)
}

#[cfg(test)]
mod tests;
