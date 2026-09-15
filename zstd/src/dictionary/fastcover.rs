use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy)]
pub struct FastCoverParams {
    pub k: usize,
    pub d: usize,
    pub f: u32,
    pub accel: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct FastCoverTuned {
    pub k: usize,
    pub d: usize,
    pub f: u32,
    pub accel: usize,
    pub score: usize,
}

pub const DEFAULT_K_CANDIDATES: &[usize] = &[64, 128, 256, 512, 1024, 2048];
pub const DEFAULT_D_CANDIDATES: &[usize] = &[6, 8, 12, 16];
pub const DEFAULT_F_CANDIDATES: &[u32] = &[16, 18, 20];

// Upstream zstd multiplicative hash primes (`ZSTD_hashXPtr` family,
// `zstd/lib/common/zstd_internal.h`): one unaligned read + one multiply per
// dmer instead of a per-byte FNV loop.
const PRIME_4_BYTES: u32 = 2_654_435_761;
const PRIME_5_BYTES: u64 = 889_523_592_379;
const PRIME_6_BYTES: u64 = 227_718_039_650_203;
const PRIME_7_BYTES: u64 = 58_295_818_150_454_627;
const PRIME_8_BYTES: u64 = 0xCF1B_BCDC_B7A5_6463;

/// Bytes a dmer hash reads at a position: the hash covers the first
/// `min(d, 8)` bytes but the wide read is always 8 (upstream zstd
/// `readLength = MAX(d, 8)`), except the pure 4-byte hash.
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

/// The frequency-table width, in the range upstream zstd's FastCOVER takes
/// (`FASTCOVER_MAX_F`, fastcover.c). A width outside it is brought to its
/// nearest end rather than refused; inside it, the width is used as given, so
/// memory grows as `2^f` exactly as the caller chose.
fn clamp_table_bits(f: u32) -> u32 {
    f.clamp(1, 31)
}

/// A count table that does not fit in memory: larger than this target can lay
/// out, or refused by the allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableTooLarge {
    pub(crate) entries: usize,
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

pub(crate) fn normalize_fastcover_params(mut params: FastCoverParams) -> FastCoverParams {
    params.d = params.d.clamp(4, 32);
    params.k = params.k.max(params.d).max(16);
    params.f = clamp_table_bits(params.f);
    params.accel = params.accel.clamp(1, 10);
    params
}

fn build_frequency_table(
    sample: &[u8],
    d: usize,
    f: u32,
    accel: usize,
) -> Result<Vec<u32>, TableTooLarge> {
    let bits = clamp_table_bits(f);
    let size = 1usize << bits;
    // Upstream zstd accel table: `skip = accel - 1` dmers between counted dmers
    // (`FASTCOVER_defaultAccelParameters`), i.e. a stride of `accel`.
    let step = accel.max(1);
    let mut table = zeroed_counts::<u32>(size)?;

    let read_len = dmer_read_len(d);
    if sample.len() < read_len {
        return Ok(table);
    }

    let mut i = 0usize;
    while i + read_len <= sample.len() {
        // A count is bounded by the dmer count (`sample.len()`), far below
        // `u32::MAX` for any trainable corpus — plain increment.
        table[hash_dmer_index(sample, i, bits, d)] += 1;
        i += step;
    }
    Ok(table)
}

fn build_raw_dict(
    sample: &[u8],
    dict_size: usize,
    params: FastCoverParams,
) -> Result<Vec<u8>, TableTooLarge> {
    if sample.is_empty() || dict_size == 0 {
        return Ok(Vec::new());
    }

    let params = normalize_fastcover_params(params);
    let k = params.k;
    let d = params.d;
    let f = clamp_table_bits(params.f);
    let read_len = dmer_read_len(d);
    if sample.len() < read_len {
        // Too short for even one wide-read dmer: no trainable content.
        // Callers treat an empty raw dict as "sample too small".
        return Ok(Vec::new());
    }

    // Upstream zstd `FASTCOVER_buildDictionary` epoch model: split the corpus into
    // epochs of dmers and round-robin them, taking the best k-byte segment
    // per visit. A segment's score is the sum of frequencies of its DISTINCT
    // dmers, maintained incrementally while the candidate window slides
    // (O(1) per position via the `segment_freqs` occurrence counts), and a
    // chosen segment's dmer frequencies are zeroed so later picks value only
    // new coverage. This replaced a global greedy set-cover with a per-
    // segment inverted index (`BTreeMap` per segment + slot lists): that
    // shape allocated millions of map nodes on a 1 MiB corpus and ran an
    // order of magnitude slower than the reference trainer at equal
    // coverage quality.
    let nb_dmers = sample.len() - read_len + 1;
    let mut freqs = build_frequency_table(sample, d, f, params.accel)?;
    let dmers_in_k = k - d + 1; // `normalize` guarantees k >= d

    // Upstream zstd `COVER_computeEpochs` (passes = 1): target one selection per
    // epoch, with a floor so epochs stay large enough to contain useful
    // segments.
    // The floor only matters up to the corpus it is capped at, so a product
    // past `usize` (a `k` near the top of it) is that cap, not an overflow.
    let min_epoch_size = k
        .checked_mul(10)
        .map_or(nb_dmers, |floor| floor.min(nb_dmers));
    let mut epoch_count = (dict_size / k).max(1);
    let mut epoch_size = nb_dmers / epoch_count;
    if epoch_size < min_epoch_size {
        epoch_size = min_epoch_size;
        epoch_count = (nb_dmers / epoch_size).max(1);
    }

    let layout = EpochLayout {
        dmers_in_k,
        epoch_size,
        epoch_count,
    };
    // A window holds at most `dmers_in_k + 1` occurrences of one index (one
    // past the segment before the oldest leaves). Upstream zstd keeps them in
    // `u16` for any `k`; a longer segment than that counts in `u32`.
    if dmers_in_k < usize::from(u16::MAX) {
        select_segments::<u16>(sample, dict_size, f, d, &mut freqs, layout)
    } else {
        select_segments::<u32>(sample, dict_size, f, d, &mut freqs, layout)
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

/// Pick a segment per epoch visit until `dict_size` bytes are filled, and
/// return them as the dictionary.
fn select_segments<C: WindowCount>(
    sample: &[u8],
    dict_size: usize,
    f: u32,
    d: usize,
    freqs: &mut [u32],
    layout: EpochLayout,
) -> Result<Vec<u8>, TableTooLarge> {
    let EpochLayout {
        dmers_in_k,
        epoch_size,
        epoch_count,
    } = layout;
    let zero = C::from(0);
    let one = C::from(1);
    // Per-window dmer occurrence counts (upstream zstd `segmentFreqs`).
    let mut segment_freqs = zeroed_counts::<C>(1usize << f)?;
    // Fill from the back (upstream zstd layout) so the best segments sit at the end
    // of the dictionary and get referenced with the smallest offsets.
    let mut out = vec![0u8; dict_size];
    let mut tail = dict_size;
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
    Ok(out)
}

fn coverage_score(dict: &[u8], eval: &[u8], d: usize, accel: usize) -> usize {
    let read_len = dmer_read_len(d);
    if dict.len() < read_len || eval.len() < read_len || d == 0 {
        return 0;
    }
    const COVERAGE_F: u32 = 20;
    let mut seen = BTreeSet::new();
    for i in 0..=(dict.len() - read_len) {
        seen.insert(hash_dmer_index(dict, i, COVERAGE_F, d));
    }

    let mut hits = 0usize;
    let step = accel.max(1);
    let mut i = 0usize;
    while i + read_len <= eval.len() {
        if seen.contains(&hash_dmer_index(eval, i, COVERAGE_F, d)) {
            hits += 1;
        }
        i += step;
    }
    hits
}

pub fn train_fastcover_raw(
    sample: &[u8],
    dict_size: usize,
    params: FastCoverParams,
) -> Result<Vec<u8>, TableTooLarge> {
    build_raw_dict(sample, dict_size, params)
}

pub fn optimize_fastcover_raw(
    sample: &[u8],
    dict_size: usize,
    split_point: f64,
    accel: usize,
    d_candidates: &[usize],
    f_candidates: &[u32],
    k_values: &[usize],
) -> Result<(Vec<u8>, FastCoverTuned), TableTooLarge> {
    let d_values = if d_candidates.is_empty() {
        DEFAULT_D_CANDIDATES
    } else {
        d_candidates
    };
    let f_values = if f_candidates.is_empty() {
        DEFAULT_F_CANDIDATES
    } else {
        f_candidates
    };
    let k_candidates = if k_values.is_empty() {
        DEFAULT_K_CANDIDATES
    } else {
        k_values
    };

    if sample.len() < 2 {
        let params = normalize_fastcover_params(FastCoverParams {
            k: k_candidates[0],
            d: d_values[0],
            f: f_values[0],
            accel,
        });
        let mut dict = build_raw_dict(sample, dict_size, params)?;
        if dict.is_empty() && dict_size > 0 {
            let take = sample.len().min(dict_size);
            dict.extend_from_slice(&sample[..take]);
        }
        return Ok((
            dict,
            FastCoverTuned {
                k: params.k,
                d: params.d,
                f: params.f,
                accel: params.accel,
                score: 0,
            },
        ));
    }

    // Upstream's split (fastcover.c, `FASTCOVER_ctx_init`): below 1 the corpus
    // trains on its leading share and is scored on the rest; at 1 it trains
    // and scores on all of it. A split that is not positive keeps the 0.75
    // default. The index stays inside the corpus so neither half is empty.
    let (train, eval) = if split_point >= 1.0 {
        (sample, sample)
    } else {
        let split = if split_point > 0.0 { split_point } else { 0.75 };
        let split_idx = ((sample.len() as f64) * split) as usize;
        sample.split_at(split_idx.clamp(1, sample.len() - 1))
    };

    let mut best_dict = Vec::new();
    let mut best = FastCoverTuned {
        k: 0,
        d: 0,
        f: 0,
        accel: accel.clamp(1, 10),
        score: 0,
    };

    for &f in f_values {
        for &d in d_values {
            for &k in k_candidates {
                let params = normalize_fastcover_params(FastCoverParams { k, d, f, accel });
                let dict = build_raw_dict(train, dict_size, params)?;
                let score = coverage_score(dict.as_slice(), eval, params.d, params.accel);
                if best_dict.is_empty() || score > best.score {
                    best.score = score;
                    best.k = params.k;
                    best.d = params.d;
                    best.f = params.f;
                    best.accel = params.accel;
                    best_dict = dict;
                }
            }
        }
    }

    Ok((best_dict, best))
}

#[cfg(test)]
mod tests;
