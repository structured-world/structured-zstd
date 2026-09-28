//! FastCOVER: COVER with dmers counted in a hashed frequency table of `2^f`
//! entries instead of indexed exactly, and optionally only every
//! `accel`-th position counted (upstream zstd `fastcover.c`).

use super::cover::compute_epochs;
use super::samples::{SampleSet, invalid};
use alloc::vec;
use alloc::vec::Vec;
use std::io;

// Upstream zstd multiplicative hash primes (`ZSTD_hashXPtr` family,
// `zstd/lib/common/zstd_internal.h`): one unaligned read + one multiply per
// dmer instead of a per-byte loop.
const PRIME_4_BYTES: u32 = 2_654_435_761;
const PRIME_5_BYTES: u64 = 889_523_592_379;
const PRIME_6_BYTES: u64 = 227_718_039_650_203;
const PRIME_7_BYTES: u64 = 58_295_818_150_454_627;
const PRIME_8_BYTES: u64 = 0xCF1B_BCDC_B7A5_6463;

/// Widest frequency table, in bits (upstream zstd `FASTCOVER_MAX_F`).
pub(super) const MAX_F: u32 = 31;
/// Largest acceleration (upstream zstd `FASTCOVER_MAX_ACCEL`).
pub(super) const MAX_ACCEL: u32 = 10;

/// Bytes a dmer hash reads at a position: the hash covers the first
/// `min(d, 8)` bytes but the wide read is always 8 (upstream zstd
/// `readLength = MAX(d, 8)`).
#[inline]
fn dmer_read_len(d: usize) -> usize {
    d.max(8)
}

/// Upstream zstd `FASTCOVER_hashPtrToIndex`: hash the first `min(d, 8)` bytes of the
/// dmer at `pos` into an `f`-bit table index. Caller guarantees
/// `pos + dmer_read_len(d) <= sample.len()`.
#[inline]
fn hash_dmer_index(sample: &[u8], pos: usize, f: u32, d: usize) -> usize {
    if d.min(8) == 4 {
        let v = u32::from_le_bytes(sample[pos..pos + 4].try_into().unwrap());
        return (v.wrapping_mul(PRIME_4_BYTES) >> (32 - f)) as usize;
    }
    let v = u64::from_le_bytes(sample[pos..pos + 8].try_into().unwrap());
    let h = match d.min(8) {
        5 => (v << 24).wrapping_mul(PRIME_5_BYTES),
        6 => (v << 16).wrapping_mul(PRIME_6_BYTES),
        7 => (v << 8).wrapping_mul(PRIME_7_BYTES),
        _ => v.wrapping_mul(PRIME_8_BYTES),
    };
    (h >> (64 - f)) as usize
}

/// A count table that does not fit in memory: larger than this target can lay
/// out, or refused by the allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableTooLarge {
    pub(crate) entries: usize,
}

impl From<TableTooLarge> for io::Error {
    fn from(table: TableTooLarge) -> Self {
        io::Error::new(
            io::ErrorKind::OutOfMemory,
            std::format!(
                "a FastCOVER table of {} entries does not fit in memory; use a smaller f",
                table.entries
            ),
        )
    }
}

/// `len` zeroed counts, allocated as `vec![0; len]` is (zero pages the
/// allocator hands out lazily, so a wide table costs only what is touched),
/// but reporting a table that does not fit rather than panicking on the layout
/// or aborting on a refused allocation.
fn zeroed_counts<C: WindowCount>(len: usize) -> Result<Vec<C>, TableTooLarge> {
    let too_large = TableTooLarge { entries: len };
    let layout = core::alloc::Layout::array::<C>(len).map_err(|_| too_large)?;
    if layout.size() == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the layout has a non-zero size, checked above.
    let pointer = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if pointer.is_null() {
        return Err(too_large);
    }
    // SAFETY: `pointer` comes from the global allocator with the layout of
    // `[C; len]`, which is what `Vec<C>` with capacity `len` frees it with, and
    // every element is initialised: all-zero bytes are the value 0 of the
    // integer counts `WindowCount` is implemented for (`u16`, `u32`).
    Ok(unsafe { Vec::from_raw_parts(pointer.cast::<C>(), len, len) })
}

/// Share of the training samples the entropy tables are drawn from, and dmers
/// skipped between counted ones, per acceleration (upstream zstd
/// `FASTCOVER_defaultAccelParameters`).
const ACCEL_FINALIZE_PERCENT: [usize; MAX_ACCEL as usize + 1] =
    [100, 100, 50, 34, 25, 20, 17, 14, 13, 11, 10];

/// Every trainable position of the training samples hashed into a frequency
/// table. Depends on `d`, `f` and `accel`, so one context serves every `k`.
pub(super) struct FastCoverContext<'s> {
    data: &'s [u8],
    nb_dmers: usize,
    freqs: Vec<u32>,
    d: usize,
    f: u32,
    finalize_percent: usize,
}

impl<'s> FastCoverContext<'s> {
    /// Count the dmers of the first `train` samples (upstream zstd
    /// `FASTCOVER_ctx_init`). A dmer is counted where it lies wholly inside
    /// one sample; `accel` counts every `accel`-th position.
    pub(super) fn new(
        samples: &SampleSet<'s>,
        train: usize,
        d: usize,
        f: u32,
        accel: u32,
    ) -> io::Result<Self> {
        debug_assert!((4..).contains(&d) && (1..=MAX_F).contains(&f));
        debug_assert!((1..=MAX_ACCEL).contains(&accel));
        let data = samples.leading(train);
        let read_len = dmer_read_len(d);
        let Some(nb_dmers) = data.len().checked_sub(read_len).map(|n| n + 1) else {
            return Err(invalid(&std::format!(
                "the training samples total {} bytes; FastCOVER needs at least {read_len}",
                data.len()
            )));
        };
        let mut freqs = zeroed_counts::<u32>(1usize << f)?;
        let step = accel as usize;
        let offsets = samples.offsets();
        for sample in 0..train {
            let end = offsets[sample + 1];
            let mut start = offsets[sample];
            while start + read_len <= end {
                // A count is bounded by the dmer count, far below `u32::MAX`
                // for any corpus `SampleSet` accepts here.
                freqs[hash_dmer_index(data, start, f, d)] += 1;
                start += step;
            }
        }
        Ok(Self {
            data,
            nb_dmers,
            freqs,
            d,
            f,
            finalize_percent: ACCEL_FINALIZE_PERCENT[accel as usize],
        })
    }

    /// How many of `train` samples the entropy tables are drawn from.
    pub(super) fn finalize_samples(&self, train: usize) -> usize {
        train * self.finalize_percent / 100
    }

    /// A fresh copy of the frequencies, for one dictionary build to spend.
    pub(super) fn fresh_freqs(&self) -> Vec<u32> {
        self.freqs.clone()
    }

    /// Build content of at most `capacity` bytes from segments of `k` bytes
    /// (upstream zstd `FASTCOVER_buildDictionary`), spending `freqs`.
    pub(super) fn build(
        &self,
        freqs: &mut [u32],
        window: &mut WindowCounts,
        capacity: usize,
        k: usize,
    ) -> Result<Vec<u8>, TableTooLarge> {
        debug_assert!(self.d <= k);
        let epochs = compute_epochs(capacity, self.nb_dmers, k, 1);
        let layout = EpochLayout {
            dmers_in_k: k - self.d + 1,
            epoch_size: epochs.size,
            epoch_count: epochs.num,
        };
        // A window holds at most `dmers_in_k + 1` occurrences of one index (one
        // past the segment before the oldest leaves). Upstream zstd keeps them in
        // `u16` for any `k`; a longer segment than that counts in `u32`.
        if layout.dmers_in_k < usize::from(u16::MAX) {
            let counts = window.narrow.get_or_insert_with_result(self.f)?;
            Ok(self.select_segments(capacity, freqs, counts, layout))
        } else {
            let counts = window.wide.get_or_insert_with_result(self.f)?;
            Ok(self.select_segments(capacity, freqs, counts, layout))
        }
    }

    /// Pick a segment per epoch visit until `capacity` bytes are filled, and
    /// return them as the dictionary content.
    fn select_segments<C: WindowCount>(
        &self,
        capacity: usize,
        freqs: &mut [u32],
        segment_freqs: &mut [C],
        layout: EpochLayout,
    ) -> Vec<u8> {
        let EpochLayout {
            dmers_in_k,
            epoch_size,
            epoch_count,
        } = layout;
        let (sample, f, d) = (self.data, self.f, self.d);
        let zero = C::from(0);
        let one = C::from(1);
        // Fill from the back (upstream zstd layout) so the best segments sit at
        // the end of the dictionary and get referenced with the smallest offsets.
        let mut out = vec![0u8; capacity];
        let mut tail = capacity;
        const MAX_ZERO_SCORE_RUN: usize = 10;
        let mut zero_score_run = 0usize;
        let mut epoch = 0usize;

        while tail > 0 {
            let epoch_begin = epoch * epoch_size;
            let epoch_end = epoch_begin + epoch_size;
            epoch = (epoch + 1) % epoch_count;

            // Slide the candidate window across the epoch, tracking the best
            // segment (upstream zstd `FASTCOVER_selectSegment`).
            let mut best_begin = 0usize;
            let mut best_end = 0usize;
            let mut best_score = 0u64;
            let mut active_begin = epoch_begin;
            let mut active_end = epoch_begin;
            let mut active_score = 0u64;
            while active_end < epoch_end {
                let idx = hash_dmer_index(sample, active_end, f, d);
                if segment_freqs[idx] == zero {
                    active_score += u64::from(freqs[idx]);
                }
                active_end += 1;
                segment_freqs[idx] += one;
                if active_end - active_begin == dmers_in_k + 1 {
                    let del = hash_dmer_index(sample, active_begin, f, d);
                    segment_freqs[del] -= one;
                    if segment_freqs[del] == zero {
                        active_score -= u64::from(freqs[del]);
                    }
                    active_begin += 1;
                }
                if active_score > best_score {
                    best_begin = active_begin;
                    best_end = active_end;
                    best_score = active_score;
                }
            }
            // Reset the window counts for the next epoch.
            while active_begin < epoch_end {
                let del = hash_dmer_index(sample, active_begin, f, d);
                segment_freqs[del] -= one;
                active_begin += 1;
            }
            // Zero the chosen segment's frequencies: its dmers are covered.
            for pos in best_begin..best_end {
                freqs[hash_dmer_index(sample, pos, f, d)] = 0;
            }

            if best_score == 0 {
                // This epoch has no uncovered content left; other epochs may.
                // Give up after a run of empty epochs (upstream zstd `maxZeroScoreRun`).
                zero_score_run += 1;
                if zero_score_run >= MAX_ZERO_SCORE_RUN {
                    break;
                }
                continue;
            }
            zero_score_run = 0;

            let segment_size = (best_end - best_begin + d - 1).min(tail);
            if segment_size < d {
                break;
            }
            tail -= segment_size;
            out[tail..tail + segment_size]
                .copy_from_slice(&sample[best_begin..best_begin + segment_size]);
        }

        out.drain(..tail);
        out
    }
}

/// How the corpus is walked: the dmers a segment spans, and the epochs it is
/// split into.
#[derive(Clone, Copy)]
struct EpochLayout {
    dmers_in_k: usize,
    epoch_size: usize,
    epoch_count: usize,
}

/// A dmer's occurrence count in the candidate window.
trait WindowCount: Copy + PartialEq + core::ops::AddAssign + core::ops::SubAssign + From<u8> {}
impl WindowCount for u16 {}
impl WindowCount for u32 {}

/// Per-window occurrence counts (upstream zstd `segmentFreqs`), kept across
/// builds: every build leaves them zero, so one table serves every `k`.
#[derive(Default)]
pub(super) struct WindowCounts {
    narrow: LazyCounts<u16>,
    wide: LazyCounts<u32>,
}

struct LazyCounts<C>(Option<Vec<C>>);

impl<C> Default for LazyCounts<C> {
    fn default() -> Self {
        Self(None)
    }
}

impl<C: WindowCount> LazyCounts<C> {
    /// The zeroed table of `2^f` counts, allocated on first use.
    fn get_or_insert_with_result(&mut self, f: u32) -> Result<&mut [C], TableTooLarge> {
        let len = 1usize << f;
        if self.0.as_ref().is_none_or(|counts| counts.len() != len) {
            self.0 = Some(zeroed_counts::<C>(len)?);
        }
        Ok(self.0.as_mut().expect("allocated above"))
    }
}

#[cfg(test)]
mod tests;
