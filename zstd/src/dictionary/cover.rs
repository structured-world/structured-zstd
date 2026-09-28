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

use super::samples::{SampleSet, invalid};
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
            return Err(invalid(&std::format!(
                "the training samples total {} bytes; COVER needs at least {read_len}",
                data.len()
            )));
        };
        // Ids and positions are held as `u32`, as upstream's are.
        if u32::try_from(nb_dmers).is_err() {
            return Err(invalid(
                "the training samples are too large for COVER (4 GiB at most)",
            ));
        }
        let (dmer_at, freqs) = if d <= 8 {
            index_dmers::<false>(data, nb_dmers, d, samples.offsets())
        } else {
            index_dmers::<true>(data, nb_dmers, d, samples.offsets())
        };
        let initial = freqs
            .into_iter()
            .map(|freq| DmerState { freq, active: 0 })
            .collect();
        Ok(Self {
            data,
            dmer_at,
            initial,
            d,
        })
    }

    /// A fresh copy of the frequencies, for one dictionary build to spend.
    pub(super) fn fresh_state(&self) -> Vec<DmerState> {
        self.initial.clone()
    }

    /// Build content of at most `capacity` bytes from segments of `k` bytes
    /// (upstream zstd `COVER_buildDictionary`), spending `state`. The best
    /// segments are chosen first and placed last, where the offsets that reach
    /// them are smallest.
    pub(super) fn build(&self, state: &mut [DmerState], capacity: usize, k: usize) -> Vec<u8> {
        let d = self.d;
        debug_assert!(d <= k);
        let nb_dmers = self.dmer_at.len();
        let epochs = compute_epochs(capacity, nb_dmers, k, 4);
        let max_zero_score_run = (epochs.num >> 3).clamp(10, 100);
        let dmers_in_k = k - d + 1;
        let mut dict = vec![0u8; capacity];
        let mut tail = capacity;
        let mut zero_score_run = 0usize;
        let mut epoch = 0usize;
        while tail > 0 {
            let begin = epoch * epochs.size;
            epoch = (epoch + 1) % epochs.num;
            let segment =
                select_segment(&self.dmer_at, state, begin, begin + epochs.size, dmers_in_k);
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
            dict[tail..tail + segment_size]
                .copy_from_slice(&self.data[segment.begin..segment.begin + segment_size]);
        }
        dict.drain(..tail);
        dict
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
/// occurs in: returns the id at each position and the frequency of each id.
///
/// Upstream zstd groups positions by sorting them on their dmer; ids here come
/// from a hash index in one pass instead. Which label a dmer carries does not
/// change a single selection, and the pass does no comparison sort. A dmer is
/// counted once per sample it starts in.
fn index_dmers<const LONG: bool>(
    data: &[u8],
    nb_dmers: usize,
    d: usize,
    offsets: &[usize],
) -> (Vec<u32>, Vec<u32>) {
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
    // Slots hold (tag, id); a short dmer's tag is the dmer itself.
    let mut slots: Vec<(u64, u32)> = vec![(0, EMPTY); 1 << 12];
    let mut shift = 64 - 12;
    let mut first_pos: Vec<u32> = Vec::new();
    let mut freqs: Vec<u32> = Vec::new();
    let mut last_sample: Vec<u32> = Vec::new();
    let mut dmer_at = Vec::with_capacity(nb_dmers);
    let mut sample = 0usize;
    for pos in 0..nb_dmers {
        while offsets[sample + 1] <= pos {
            sample += 1;
        }
        let tag = tag_at(pos);
        let mask = slots.len() - 1;
        let mut slot = (tag.wrapping_mul(HASH_MULTIPLIER) >> shift) as usize;
        let id = loop {
            let (slot_tag, slot_id) = slots[slot];
            if slot_id == EMPTY {
                let id = first_pos.len() as u32;
                slots[slot] = (tag, id);
                first_pos.push(pos as u32);
                freqs.push(0);
                last_sample.push(EMPTY);
                break id;
            }
            if slot_tag == tag {
                let first = first_pos[slot_id as usize] as usize;
                if !LONG || data[first..first + d] == data[pos..pos + d] {
                    break slot_id;
                }
            }
            slot = (slot + 1) & mask;
        };
        if last_sample[id as usize] != sample as u32 {
            last_sample[id as usize] = sample as u32;
            freqs[id as usize] += 1;
        }
        dmer_at.push(id);
        // Keep the load at or under a half.
        if first_pos.len() * 2 > slots.len() {
            let len = slots.len() * 2;
            shift -= 1;
            slots.clear();
            slots.resize(len, (0, EMPTY));
            for (id, &first) in first_pos.iter().enumerate() {
                let tag = tag_at(first as usize);
                let mut slot = (tag.wrapping_mul(HASH_MULTIPLIER) >> shift) as usize;
                while slots[slot].1 != EMPTY {
                    slot = (slot + 1) & (len - 1);
                }
                slots[slot] = (tag, id as u32);
            }
        }
    }
    (dmer_at, freqs)
}

#[cfg(test)]
mod tests;
