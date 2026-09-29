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
    /// The dmer count the epochs are sized by: upstream zstd's, which stops
    /// where an eight-byte read would run past the corpus. Dmers shorter than
    /// eight bytes past that point are indexed and fall in the last epoch.
    epoch_dmers: usize,
    d: usize,
}

impl<'s> CoverContext<'s> {
    /// Index the dmers of the first `train` samples.
    pub(super) fn new(samples: &SampleSet<'s>, train: usize, d: usize) -> io::Result<Self> {
        debug_assert!(d > 0);
        let data = samples.leading(train);
        // Every position a whole dmer starts at is indexed. Upstream zstd
        // (`COVER_ctx_init`) stops eight bytes short of the end, where its
        // one-word read of a short dmer would run past the corpus; a dmer of
        // fewer bytes starting there can be the only one inside its sample,
        // and is read through a bounded tail instead (`short_key`).
        let Some(nb_dmers) = data.len().checked_sub(d).map(|n| n + 1) else {
            return Err(refuse(
                TrainingError::Samples,
                &std::format!(
                    "the training samples total {} bytes; COVER needs at least {d}",
                    data.len()
                ),
            ));
        };
        let epoch_dmers = data.len().checked_sub(d.max(8)).map_or(nb_dmers, |n| n + 1);
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
            epoch_dmers,
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
        let epochs = compute_epochs(capacity, self.epoch_dmers, k, 4);
        // Upstream zstd's patience (`COVER_buildDictionary`), but never more
        // than one pass over the epochs: frequencies only fall to zero, so an
        // epoch that scored nothing never scores again, and once every epoch
        // in a row has, the rest of the passes can add nothing.
        let max_zero_score_run = (epochs.num >> 3).clamp(10, 100).min(epochs.num);
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

/// The first `d` bytes at `pos` as a little-endian key, `d <= 8`: one word
/// read, or near the end of the corpus the bytes left, zero-padded (the mask
/// keeps only the `d` that are always there).
#[inline]
fn short_key(data: &[u8], pos: usize, mask: u64) -> u64 {
    let word: [u8; 8] = match data.get(pos..pos + 8) {
        Some(word) => word.try_into().expect("eight bytes"),
        None => {
            let mut word = [0u8; 8];
            let tail = &data[pos..];
            word[..tail.len()].copy_from_slice(tail);
            word
        }
    };
    u64::from_le_bytes(word) & mask
}

/// Base of the rolling fingerprint: odd, so every power of it is too and no
/// byte's weight can vanish modulo 2^64.
const ROLL_BASE: u64 = 0x100_0000_01B3;

/// A polynomial fingerprint of the `d` bytes at one position, for dmers
/// longer than one word, moved to the next position in constant time: an
/// index scanning every position pays O(1) per position whatever `d` is.
/// Equal fingerprints are confirmed against the bytes.
#[derive(Default)]
struct Rolling {
    fingerprint: u64,
    /// The weight of the dmer's first byte, `ROLL_BASE^(d - 1)`.
    first_weight: u64,
}

impl Rolling {
    /// The fingerprint of the `d` bytes at `pos`.
    fn at(data: &[u8], pos: usize, d: usize) -> Self {
        let mut fingerprint = 0u64;
        for &byte in &data[pos..pos + d] {
            fingerprint = fingerprint
                .wrapping_mul(ROLL_BASE)
                .wrapping_add(u64::from(byte) + 1);
        }
        Self {
            fingerprint,
            first_weight: ROLL_BASE.wrapping_pow((d - 1) as u32),
        }
    }

    /// From the dmer at `pos` to the one at `pos + 1`; `data[pos + d]` must
    /// exist.
    fn advance(&mut self, data: &[u8], pos: usize, d: usize) {
        self.fingerprint = self
            .fingerprint
            .wrapping_sub((u64::from(data[pos]) + 1).wrapping_mul(self.first_weight))
            .wrapping_mul(ROLL_BASE)
            .wrapping_add(u64::from(data[pos + d]) + 1);
    }
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
/// there are: a slot holds the first position of its dmer (its id is
/// `dmer_at` there, and its bytes are read back from `data`), plus a long
/// dmer's fingerprint, and the sample a
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
    // A long dmer's tag is its rolling fingerprint, carried from one position
    // to the next; a short one's is its bytes.
    let start = || {
        if LONG {
            Rolling::at(data, 0, d)
        } else {
            Rolling::default()
        }
    };
    let tag_at = |rolling: &Rolling, pos: usize| {
        if LONG {
            rolling.fingerprint
        } else {
            short_key(data, pos, mask)
        }
    };
    // Slots hold the first position of a dmer; positions fit `u32` below
    // `EMPTY`, which the caller's size check guarantees. A long dmer's slot
    // also keeps its fingerprint beside it, so a probe compares bytes only
    // when the fingerprints agree and a rebuild never re-rolls one; a short
    // dmer's key is one word read back from `data`.
    let mut slots: Vec<u32> = vec![EMPTY; 1 << 12];
    let mut fingerprints: Vec<u64> = if LONG { vec![0; 1 << 12] } else { Vec::new() };
    // Whether the dmer in `slot`, first seen at `first`, is the one at `pos`,
    // whose tag is `tag`.
    let same_dmer = |fingerprints: &[u64], slot: usize, first: usize, pos: usize, tag: u64| {
        if LONG {
            fingerprints[slot] == tag && data[first..first + d] == data[pos..pos + d]
        } else {
            short_key(data, first, mask) == tag
        }
    };
    let mut shift = 64 - 12;
    // `active` holds the last sample a dmer was counted in until the end.
    let mut dmers: Vec<DmerState> = Vec::new();
    let mut dmer_at: Vec<u32> = Vec::with_capacity(nb_dmers);
    let mut sample = 0usize;
    let mut rolling = start();
    for pos in 0..nb_dmers {
        if LONG && pos > 0 {
            rolling.advance(data, pos - 1, d);
        }
        while offsets[sample + 1] <= pos {
            sample += 1;
        }
        let slot_mask = slots.len() - 1;
        let tag = tag_at(&rolling, pos);
        let mut slot = (tag.wrapping_mul(HASH_MULTIPLIER) >> shift) as usize;
        let id = loop {
            let first = slots[slot];
            if first == EMPTY {
                let id = dmers.len() as u32;
                slots[slot] = pos as u32;
                if LONG {
                    fingerprints[slot] = tag;
                }
                dmers.push(DmerState {
                    freq: 0,
                    active: EMPTY,
                });
                break id;
            }
            if same_dmer(&fingerprints, slot, first as usize, pos, tag) {
                break dmer_at[first as usize];
            }
            slot = (slot + 1) & slot_mask;
        };
        // A dmer spilling into the next sample exists only in the
        // concatenation, so it earns nothing.
        let dmer = &mut dmers[id as usize];
        if pos + d <= offsets[sample + 1] && dmer.active != sample as u32 {
            dmer.active = sample as u32;
            dmer.freq += 1;
        }
        dmer_at.push(id);
        // Keep the load at or under a half. The new table is filled from the
        // old one's first positions, so a rebuild costs the distinct dmers
        // and not every position scanned so far; which slot a dmer lands in
        // changes no id.
        if dmers.len() * 2 > slots.len() {
            let len = slots.len() * 2;
            shift -= 1;
            let old = core::mem::replace(&mut slots, vec![EMPTY; len]);
            let old_fingerprints = if LONG {
                core::mem::replace(&mut fingerprints, vec![0; len])
            } else {
                Vec::new()
            };
            for (at, &first) in old.iter().enumerate() {
                if first == EMPTY {
                    continue;
                }
                let tag = if LONG {
                    old_fingerprints[at]
                } else {
                    short_key(data, first as usize, mask)
                };
                let mut slot = (tag.wrapping_mul(HASH_MULTIPLIER) >> shift) as usize;
                while slots[slot] != EMPTY {
                    slot = (slot + 1) & (len - 1);
                }
                slots[slot] = first;
                if LONG {
                    fingerprints[slot] = tag;
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
