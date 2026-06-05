use super::{BETTER_WINDOW_LOG, CompressionLevel};

pub(crate) const RAW_FAST_PATH_MIN_BLOCK_LEN: usize = 512;
pub(crate) const RAW_FAST_PATH_MAX_SAMPLE_LEN: usize = 4096;
pub(crate) const RAW_FAST_PATH_MIN_SAMPLE_LEN: usize = 32;
const BETTER_WINDOW_SIZE_BYTES: u64 = 1u64 << BETTER_WINDOW_LOG;

// Keep classifier scratch modest for no_std/small-stack targets: 1024 slots
// cuts per-call stack for repeat tracking from ~8 KiB to ~4 KiB.
const INCOMPRESSIBLE_REPEAT_TABLE_BITS: usize = 10;
const INCOMPRESSIBLE_REPEAT_TABLE_LEN: usize = 1 << INCOMPRESSIBLE_REPEAT_TABLE_BITS;
const INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS: usize = INCOMPRESSIBLE_REPEAT_TABLE_LEN / 64;
const INCOMPRESSIBLE_REPEAT_HASH_MULT: u32 = 0x9E37_79B1;
const INCOMPRESSIBLE_MIN_DISTINCT_BYTES: usize = 200;
// Allow at most ~4.2% concentration for the most frequent symbol in sampled data.
// This guards against low-entropy text-like inputs being misclassified as random.
const INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR: usize = 24;
// Allow limited 4-byte hash-bucket repeats before treating the sample as structured.
const INCOMPRESSIBLE_REPEAT_DIVISOR: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StrictProbeSelection {
    probe_len: usize,
    tail_start: Option<usize>,
    mid_start: Option<usize>,
}

impl StrictProbeSelection {
    #[inline]
    const fn reuses_full_block_classification(self) -> bool {
        self.tail_start.is_none()
    }
}

#[inline]
fn select_strict_probes(block_len: usize) -> StrictProbeSelection {
    let probe_len = RAW_FAST_PATH_MIN_BLOCK_LEN.min(block_len);
    if probe_len == block_len {
        StrictProbeSelection {
            probe_len,
            tail_start: None,
            mid_start: None,
        }
    } else {
        let tail_start = block_len - probe_len;
        if tail_start < probe_len {
            // For [probe_len + 1, 2 * probe_len), head/tail would heavily overlap.
            // Reuse the full-block classification computed by the caller.
            StrictProbeSelection {
                probe_len,
                tail_start: None,
                mid_start: None,
            }
        } else if tail_start < 2 * probe_len {
            // For [2 * probe_len, 3 * probe_len), head/tail are separable but a
            // distinct non-overlapping middle probe is not.
            StrictProbeSelection {
                probe_len,
                tail_start: Some(tail_start),
                mid_start: None,
            }
        } else {
            // Once we can separate all windows, use head/mid/tail probing.
            StrictProbeSelection {
                probe_len,
                tail_start: Some(tail_start),
                mid_start: Some(tail_start / 2),
            }
        }
    }
}

#[inline]
pub(crate) fn compression_level_allows_raw_fast_path(
    level: CompressionLevel,
    window_size: u64,
) -> bool {
    match level {
        CompressionLevel::Fastest | CompressionLevel::Default | CompressionLevel::Better => true,
        CompressionLevel::Best => window_size <= BETTER_WINDOW_SIZE_BYTES,
        CompressionLevel::Level(_) => window_size <= BETTER_WINDOW_SIZE_BYTES,
        CompressionLevel::Uncompressed => false,
    }
}

/// Accumulate byte counts and 4-byte repeat hits for one sample region in a
/// single pass.
///
/// Returns `true` as soon as the running repeat count passes `repeat_guard`.
/// That guard is the FINAL threshold (fixed before any region is scanned)
/// and the repeat count only grows, so an early `true` is exactly the
/// verdict (`false` = compressible) that a full scan would have produced —
/// the `repeats <= repeat_guard` term of the final verdict is already
/// settled. On repetitive data (structured text, a single long match) the
/// quad repeats pass the guard within the first few hundred bytes; on random
/// data the guard is never reached and the whole region is counted, with no
/// per-byte branch in the hot path (the byte counts and the quad hashing
/// share one pass instead of the previous two).
#[inline]
fn scan_sample_region(
    sample: &[u8],
    counts: &mut [u16; 256],
    repeat_table: &mut [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN],
    repeat_occupied: &mut [u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS],
    repeats: &mut usize,
    repeat_guard: usize,
) -> bool {
    let mut idx = 0usize;
    let len = sample.len();
    while idx + 4 <= len {
        counts[sample[idx] as usize] += 1;
        counts[sample[idx + 1] as usize] += 1;
        counts[sample[idx + 2] as usize] += 1;
        counts[sample[idx + 3] as usize] += 1;
        let quad = u32::from_le_bytes([
            sample[idx],
            sample[idx + 1],
            sample[idx + 2],
            sample[idx + 3],
        ]);
        // Top `INCOMPRESSIBLE_REPEAT_TABLE_BITS` bits of the 32-bit hash give
        // the slot directly: the `as usize` value is `< 2^32`, so the shift
        // by `32 - BITS` already yields an index in `0..TABLE_LEN`. No mask
        // needed (donor `ZSTD_hashPtr` shape).
        let slot = (quad.wrapping_mul(INCOMPRESSIBLE_REPEAT_HASH_MULT) as usize)
            >> (32 - INCOMPRESSIBLE_REPEAT_TABLE_BITS);
        let word = slot / 64;
        let bit = 1_u64 << (slot % 64);
        let occupied = (repeat_occupied[word] & bit) != 0;
        if occupied && repeat_table[slot] == quad {
            *repeats += 1;
            if *repeats > repeat_guard {
                return true;
            }
        } else {
            repeat_table[slot] = quad;
            repeat_occupied[word] |= bit;
        }
        idx += 4;
    }
    // Tail bytes that don't form a full quad still count toward the symbol
    // histogram used by the final distinct / max-frequency verdict.
    while idx < len {
        counts[sample[idx] as usize] += 1;
        idx += 1;
    }
    false
}

#[inline]
pub(crate) fn block_looks_incompressible(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    sample_looks_incompressible(block)
}

#[inline]
pub(crate) fn block_looks_incompressible_strict(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    if !sample_looks_incompressible(block) {
        return false;
    }
    // Best level should only early-exit on strongly random data. Probe head,
    // middle, and tail so mixed-entropy blocks do not get misclassified.
    let selection = select_strict_probes(block.len());
    if selection.reuses_full_block_classification() {
        // The full-block sample above already classified this input. For
        // minimum and near-min blocks, split probes would overlap too heavily.
        return true;
    }
    let probe_len = selection.probe_len;
    let tail_start = selection
        .tail_start
        .expect("strict probe tail_start should be present for split probes");
    let head = &block[..probe_len];
    let tail = &block[tail_start..tail_start + probe_len];
    if let Some(mid_start) = selection.mid_start {
        let mid = &block[mid_start..mid_start + probe_len];
        sample_looks_incompressible(head)
            && sample_looks_incompressible(mid)
            && sample_looks_incompressible(tail)
    } else {
        sample_looks_incompressible(head) && sample_looks_incompressible(tail)
    }
}

#[inline]
fn sample_looks_incompressible(block: &[u8]) -> bool {
    let sample_len = block.len().min(RAW_FAST_PATH_MAX_SAMPLE_LEN);
    if sample_len < RAW_FAST_PATH_MIN_SAMPLE_LEN {
        return false;
    }

    // Select the sampled regions: the whole block when it fits the cap, or
    // head / middle / tail probes so capped samples still reject
    // mixed-entropy blocks whose center is compressible.
    let mut regions: [&[u8]; 3] = [&[], &[], &[]];
    let region_count;
    if sample_len == block.len() {
        regions[0] = block;
        region_count = 1;
    } else {
        let head_len = sample_len / 3;
        let mid_len = sample_len / 3;
        let tail_len = sample_len - head_len - mid_len;
        let mid_start = (block.len() - mid_len) / 2;
        regions[0] = &block[..head_len];
        regions[1] = &block[mid_start..mid_start + mid_len];
        regions[2] = &block[block.len() - tail_len..];
        region_count = 3;
    }

    // `repeat_guard` is the FINAL verdict threshold, fixed before scanning.
    // It needs the total 4-byte-quad count up front (one quad per 4 bytes of
    // each region) so `scan_sample_region` can bail the moment the running
    // repeat count passes it.
    let max_symbol_guard = sample_len / INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR;
    let total_quads: usize = regions[..region_count].iter().map(|r| r.len() / 4).sum();
    let repeat_guard = total_quads / INCOMPRESSIBLE_REPEAT_DIVISOR + 1;

    let mut counts = [0u16; 256];
    let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    // Bitset occupancy keeps this path no_std-friendly while avoiding the
    // larger per-slot bool map (and extra matcher-level scratch state).
    let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
    let mut repeats = 0usize;

    for region in &regions[..region_count] {
        if scan_sample_region(
            region,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            repeat_guard,
        ) {
            // The repeat guard was passed — the block is compressible. This
            // is exactly the verdict a full scan would have produced.
            return false;
        }
    }

    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    distinct >= INCOMPRESSIBLE_MIN_DISTINCT_BYTES
        && max_freq <= max_symbol_guard
        && repeats <= repeat_guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::CompressionLevel;
    use alloc::vec;
    use alloc::vec::Vec;

    fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        let mut out = vec![0u8; len];
        for byte in &mut out {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        out
    }

    #[test]
    fn sample_metrics_do_not_count_first_u32_max_as_repeat() {
        let sample = [0xFF_u8; 4];
        let mut counts = [0u16; 256];
        let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
        let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
        let mut repeats = 0usize;

        // Guard set high so the early-exit never fires: this exercises the
        // repeat-table init, where `0xFFFFFFFF` matches the `u32::MAX`
        // sentinel but the occupancy bit is still clear, so the first quad
        // must NOT be counted as a repeat.
        let bailed = scan_sample_region(
            &sample,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            usize::MAX,
        );

        assert!(!bailed, "high guards must not trigger an early exit");
        assert_eq!(repeats, 0, "first quad must not be miscounted as a repeat");
    }

    #[test]
    fn scan_sample_region_early_exits_on_repetitive_input() {
        // 32 identical 4-byte quads: the repeat count climbs past any small
        // guard, exercising the early-exit `true` path directly.
        let sample = [0xAB_u8; 128];
        let mut counts = [0u16; 256];
        let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
        let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
        let mut repeats = 0usize;

        // Guard of 1: the first quad seeds the table, the second is the first
        // counted repeat (repeats == 1), the third pushes repeats past the
        // guard and returns `true`.
        let bailed = scan_sample_region(
            &sample,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            1,
        );

        assert!(bailed, "repetitive input must trigger the early exit");
        assert!(repeats > 1, "repeat count must have exceeded the guard");
    }

    #[test]
    fn best_raw_fast_path_requires_better_sized_window() {
        assert!(compression_level_allows_raw_fast_path(
            CompressionLevel::Best,
            BETTER_WINDOW_SIZE_BYTES
        ));
        assert!(!compression_level_allows_raw_fast_path(
            CompressionLevel::Best,
            BETTER_WINDOW_SIZE_BYTES + 1
        ));
    }

    #[test]
    fn level4_row_raw_fast_path_allowed_with_better_window_reach() {
        assert!(compression_level_allows_raw_fast_path(
            CompressionLevel::Level(4),
            BETTER_WINDOW_SIZE_BYTES
        ));
    }

    #[test]
    fn strict_incompressible_reuses_full_block_classification_for_min_block() {
        let block = vec![0xA5; RAW_FAST_PATH_MIN_BLOCK_LEN];
        let probes = select_strict_probes(block.len());
        assert_eq!(
            probes.tail_start, None,
            "minimum-size strict blocks must reuse the full-block sample"
        );
        assert_eq!(
            block_looks_incompressible_strict(&block),
            sample_looks_incompressible(&block),
            "strict path should not re-score identical probes for minimum-size blocks"
        );
    }

    #[test]
    fn strict_probe_selector_avoids_overlap_on_small_non_min_blocks() {
        let near_min = select_strict_probes(RAW_FAST_PATH_MIN_BLOCK_LEN + 1);
        assert_eq!(near_min.tail_start, None);
        assert_eq!(near_min.mid_start, None);

        let two_probe = select_strict_probes(RAW_FAST_PATH_MIN_BLOCK_LEN * 2);
        assert_eq!(two_probe.tail_start, Some(RAW_FAST_PATH_MIN_BLOCK_LEN));
        assert_eq!(two_probe.mid_start, None);

        let three_probe = select_strict_probes(RAW_FAST_PATH_MIN_BLOCK_LEN * 3);
        assert_eq!(
            three_probe.tail_start,
            Some(RAW_FAST_PATH_MIN_BLOCK_LEN * 2)
        );
        assert_eq!(three_probe.mid_start, Some(RAW_FAST_PATH_MIN_BLOCK_LEN));
    }

    #[test]
    fn capped_sample_probes_middle_and_blocks_raw_fast_path_for_mixed_entropy() {
        let mut block =
            deterministic_bytes(0x9E37_79B9_7F4A_7C15, RAW_FAST_PATH_MAX_SAMPLE_LEN * 2);
        let mid_start = block.len() / 3;
        let mid_end = block.len() - (block.len() / 3);
        for byte in &mut block[mid_start..mid_end] {
            *byte = 0;
        }

        assert!(
            !sample_looks_incompressible(&block),
            "capped sampling must account for middle-region compressibility"
        );
        assert!(
            !block_looks_incompressible(&block),
            "mixed-entropy block should not look incompressible for default fast-path gate"
        );
    }
}
