//! FastCOVER: COVER with dmers counted in a hashed frequency table of `2^f`
//! entries instead of indexed exactly, and optionally only every
//! `accel`-th position counted (upstream zstd `fastcover.c`).

use super::cover::{Epochs, compute_epochs};
use super::samples::{SampleSet, TrainingError, refuse};
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

/// Upstream zstd `FASTCOVER_hashPtrToIndex`: hash the first `H` bytes of the
/// dmer at `ptr` (`H = min(d, 8)`) into an `f`-bit table index.
///
/// # Safety
///
/// `ptr` is readable for eight bytes, which every dmer position is: the
/// positions stop [`dmer_read_len`] bytes before the end.
#[inline(always)]
unsafe fn hash_at<const H: usize>(ptr: *const u8, f: u32) -> usize {
    // SAFETY: the caller's eight readable bytes.
    let v = u64::from_le(unsafe { ptr.cast::<u64>().read_unaligned() });
    match H {
        4 => ((v as u32).wrapping_mul(PRIME_4_BYTES) >> (32 - f)) as usize,
        5 => ((v << 24).wrapping_mul(PRIME_5_BYTES) >> (64 - f)) as usize,
        6 => ((v << 16).wrapping_mul(PRIME_6_BYTES) >> (64 - f)) as usize,
        7 => ((v << 8).wrapping_mul(PRIME_7_BYTES) >> (64 - f)) as usize,
        _ => (v.wrapping_mul(PRIME_8_BYTES) >> (64 - f)) as usize,
    }
}

/// Run `$body` with `$h` bound to the number of bytes a dmer of `$d` hashes,
/// as a constant: the counting and selecting loops are monomorphised on it
/// once, rather than branching on `d` at every position.
macro_rules! with_hashed_bytes {
    ($d:expr, $h:ident => $body:expr) => {
        match $d.min(8) {
            4 => {
                const $h: usize = 4;
                $body
            }
            5 => {
                const $h: usize = 5;
                $body
            }
            6 => {
                const $h: usize = 6;
                $body
            }
            7 => {
                const $h: usize = 7;
                $body
            }
            _ => {
                const $h: usize = 8;
                $body
            }
        }
    };
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

/// Share of the training samples the entropy tables are drawn from, per
/// acceleration (upstream zstd `FASTCOVER_defaultAccelParameters`; the dmers
/// skipped between counted ones are `accel - 1`).
const ACCEL_FINALIZE_PERCENT: [usize; MAX_ACCEL as usize + 1] =
    [100, 100, 50, 34, 25, 20, 17, 14, 13, 11, 10];

/// How many of `train` samples the entropy tables are drawn from at `accel`.
pub(super) fn finalize_samples(train: usize, accel: u32) -> usize {
    train * ACCEL_FINALIZE_PERCENT[accel as usize] / 100
}

/// Every trainable position of the training samples hashed into a frequency
/// table. Depends on `d`, `f` and `accel`, so one context serves every `k`.
pub(super) struct FastCoverContext<'s> {
    data: &'s [u8],
    nb_dmers: usize,
    freqs: Vec<u32>,
    d: usize,
    f: u32,
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
            return Err(refuse(
                TrainingError::Samples,
                &std::format!(
                    "the training samples total {} bytes; FastCOVER needs at least {read_len}",
                    data.len()
                ),
            ));
        };
        // Counts are `u32`, as upstream's are (`FASTCOVER_MAX_SAMPLES_SIZE`),
        // and no bucket counts more positions than there are dmers.
        if u32::try_from(nb_dmers).is_err() {
            return Err(refuse(
                TrainingError::Samples,
                "the training samples are too large for FastCOVER (4 GiB at most)",
            ));
        }
        samples.check_holds_dmer(train, read_len)?;
        let mut freqs = zeroed_counts::<u32>(1usize << f)?;
        let offsets = &samples.offsets()[..=train];
        with_hashed_bytes!(d, H => count_dmers::<H>(data, offsets, read_len, f, accel as usize, &mut freqs));
        Ok(Self {
            data,
            nb_dmers,
            freqs,
            d,
            f,
        })
    }

    /// Build content of at most `capacity` bytes from segments of `k` bytes
    /// (upstream zstd `FASTCOVER_buildDictionary`). `freqs` is scratch the
    /// build refills with the counted frequencies and spends, so one table
    /// serves every build; `out` is scratch the content is written into, and
    /// the returned slice borrows it.
    pub(super) fn build<'o>(
        &self,
        freqs: &mut Vec<u32>,
        window: &mut WindowCounts,
        out: &'o mut Vec<u8>,
        capacity: usize,
        k: usize,
    ) -> Result<&'o [u8], TableTooLarge> {
        debug_assert!(self.d <= k);
        freqs.clear();
        freqs
            .try_reserve_exact(self.freqs.len())
            .map_err(|_| TableTooLarge {
                entries: self.freqs.len(),
            })?;
        freqs.extend_from_slice(&self.freqs);
        let layout = EpochLayout {
            dmers_in_k: k - self.d + 1,
            epochs: compute_epochs(capacity, self.nb_dmers, k, 1),
        };
        // Only bytes past the final `tail` are returned, so what an earlier
        // build left before it needs no clearing.
        out.resize(capacity, 0);
        // A window holds at most `dmers_in_k + 1` occurrences of one index (one
        // past the segment before the oldest leaves). Upstream zstd keeps them in
        // `u16` for any `k`; a longer segment than that counts in `u32`.
        let WindowCounts { narrow, wide, ring } = window;
        let tail = if layout.dmers_in_k < usize::from(u16::MAX) {
            let counts = narrow.get_or_insert_with_result(self.f)?;
            with_hashed_bytes!(self.d, H => self.select_segments::<H, u16>(out, freqs, counts, ring, layout))
        } else {
            let counts = wide.get_or_insert_with_result(self.f)?;
            with_hashed_bytes!(self.d, H => self.select_segments::<H, u32>(out, freqs, counts, ring, layout))
        };
        Ok(&out[tail..])
    }

    /// Pick a segment per epoch visit until `out` is filled from its end, and
    /// return where the content starts in it. `H` is the number of bytes a
    /// dmer hashes.
    fn select_segments<const H: usize, C: WindowCount>(
        &self,
        out: &mut [u8],
        freqs: &mut [u32],
        segment_freqs: &mut [C],
        ring: &mut Vec<u32>,
        layout: EpochLayout,
    ) -> usize {
        let EpochLayout { dmers_in_k, epochs } = layout;
        let (sample, f, d) = (self.data, self.f, self.d);
        let zero = C::from(0);
        let one = C::from(1);
        debug_assert!(freqs.len() == 1 << f && segment_freqs.len() == 1 << f);
        debug_assert!(epochs.num * epochs.size <= self.nb_dmers);
        // Every position below `nb_dmers` has eight readable bytes and hashes
        // below `2^f`, the length of both tables, so the loops below read and
        // index without checks.
        let base = sample.as_ptr();
        let freq_ptr = freqs.as_mut_ptr();
        let window_ptr = segment_freqs.as_mut_ptr();
        // A window reaching this many positions sheds its oldest.
        let window_limit = dmers_in_k + 1;
        // The window's dmer indices in arrival order, so each position is
        // hashed once as it enters and read back as it leaves. Upstream hashes
        // it again on the way out. A window never spans more than its epoch,
        // so a segment longer than the longest epoch, the last one, needs no
        // more room than that.
        let longest_epoch = self.nb_dmers - (epochs.num - 1) * epochs.size;
        let ring_len = window_limit.min(longest_epoch);
        ring.clear();
        ring.resize(ring_len, 0);
        let ring_ptr = ring.as_mut_ptr();
        // Fill from the back (upstream zstd layout) so the best segments sit at
        // the end of the dictionary and get referenced with the smallest offsets.
        let mut tail = out.len();
        // Upstream zstd's patience (`maxZeroScoreRun`), but never more than one
        // pass over the epochs: counts only fall to zero, so a run of empty
        // epochs as long as there are epochs means none can score again.
        let max_zero_score_run = epochs.num.min(10);
        let mut zero_score_run = 0usize;
        let mut epoch = 0usize;

        while tail > 0 {
            let (epoch_begin, epoch_end) = epochs.bounds(epoch, self.nb_dmers);
            epoch = (epoch + 1) % epochs.num;

            // Slide the candidate window across the epoch, tracking the best
            // segment (upstream zstd `FASTCOVER_selectSegment`).
            let mut best_begin = 0usize;
            let mut best_end = 0usize;
            let mut best_score = 0u64;
            let mut active_begin = epoch_begin;
            let mut active_end = epoch_begin;
            let mut active_score = 0u64;
            let mut ring_head = 0usize;
            let mut ring_tail = 0usize;
            while active_end < epoch_end {
                // SAFETY: `active_end < epoch_end <= nb_dmers`; see above.
                // `ring_tail < ring_len`, the ring's length.
                let idx = unsafe { hash_at::<H>(base.add(active_end), f) };
                unsafe { *ring_ptr.add(ring_tail) = idx as u32 };
                ring_tail += 1;
                if ring_tail == ring_len {
                    ring_tail = 0;
                }
                // SAFETY: `idx < 2^f`, both tables' length.
                let count = unsafe { &mut *window_ptr.add(idx) };
                if *count == zero {
                    active_score += u64::from(unsafe { *freq_ptr.add(idx) });
                }
                *count += one;
                active_end += 1;
                if active_end - active_begin == window_limit {
                    // SAFETY: `ring_head < ring_len`; the index it holds was
                    // hashed from a position, so it is below `2^f`.
                    let del = unsafe { *ring_ptr.add(ring_head) } as usize;
                    ring_head += 1;
                    if ring_head == ring_len {
                        ring_head = 0;
                    }
                    let count = unsafe { &mut *window_ptr.add(del) };
                    *count -= one;
                    if *count == zero {
                        active_score -= u64::from(unsafe { *freq_ptr.add(del) });
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
                // SAFETY: as for the leaving position above.
                let del = unsafe { *ring_ptr.add(ring_head) } as usize;
                ring_head += 1;
                if ring_head == ring_len {
                    ring_head = 0;
                }
                unsafe { *window_ptr.add(del) -= one };
                active_begin += 1;
            }
            // Drop the segment's leading dmers that earlier segments already
            // cover: they score nothing and would only spend dictionary bytes.
            // The tail needs no trim, the best segment is recorded as a dmer
            // that scores enters it.
            // SAFETY (both reads): `best_begin < best_end <= epoch_end <= nb_dmers`.
            while best_begin < best_end
                && unsafe { *freq_ptr.add(hash_at::<H>(base.add(best_begin), f)) } == 0
            {
                best_begin += 1;
            }
            debug_assert!(
                best_score == 0
                    || unsafe { *freq_ptr.add(hash_at::<H>(base.add(best_end - 1), f)) } != 0,
                "the best segment ends on a dmer that scores"
            );
            // Zero the chosen segment's frequencies: its dmers are covered.
            for pos in best_begin..best_end {
                // SAFETY: `pos < best_end <= epoch_end <= nb_dmers`.
                unsafe { *freq_ptr.add(hash_at::<H>(base.add(pos), f)) = 0 };
            }

            if best_score == 0 {
                // This epoch has no uncovered content left; other epochs may.
                // Give up after a run of empty epochs (upstream zstd `maxZeroScoreRun`).
                zero_score_run += 1;
                if zero_score_run >= max_zero_score_run {
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
        tail
    }
}

/// Count every `step`-th dmer lying wholly inside its sample (upstream zstd
/// `FASTCOVER_computeFrequency`). `offsets` bounds the samples of `data`.
fn count_dmers<const H: usize>(
    data: &[u8],
    offsets: &[usize],
    read_len: usize,
    f: u32,
    step: usize,
    freqs: &mut [u32],
) {
    debug_assert_eq!(freqs.len(), 1 << f);
    debug_assert_eq!(offsets.last(), Some(&data.len()));
    let base = data.as_ptr();
    let counts = freqs.as_mut_ptr();
    for bounds in offsets.windows(2) {
        let end = bounds[1];
        let mut start = bounds[0];
        while start + read_len <= end {
            // SAFETY: `start + read_len <= end <= data.len()` and `read_len`
            // is at least eight; the index is below `2^f`, the table's length.
            // A count is bounded by the dmer count, far below `u32::MAX`.
            unsafe { *counts.add(hash_at::<H>(base.add(start), f)) += 1 };
            start += step;
        }
    }
}

/// How the corpus is walked: the dmers a segment spans, and the epochs it is
/// split into.
#[derive(Clone, Copy)]
struct EpochLayout {
    dmers_in_k: usize,
    epochs: Epochs,
}

/// A dmer's occurrence count in the candidate window.
trait WindowCount: Copy + PartialEq + core::ops::AddAssign + core::ops::SubAssign + From<u8> {}
impl WindowCount for u16 {}
impl WindowCount for u32 {}

/// Per-window occurrence counts (upstream zstd `segmentFreqs`), kept across
/// builds: every build leaves them zero, so one table serves every `k`. With
/// them, the ring of the window's dmer indices.
#[derive(Default)]
pub(super) struct WindowCounts {
    narrow: LazyCounts<u16>,
    wide: LazyCounts<u32>,
    ring: Vec<u32>,
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
