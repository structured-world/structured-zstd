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
    const fn is_full_block(self) -> bool {
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

#[inline]
fn update_sample_metrics(
    sample: &[u8],
    counts: &mut [u16; 256],
    repeat_table: &mut [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN],
    repeat_occupied: &mut [u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS],
    repeats: &mut usize,
    sampled_quads: &mut usize,
) {
    for &byte in sample {
        counts[byte as usize] += 1;
    }
    let mut idx = 0usize;
    while idx + 4 <= sample.len() {
        let quad = u32::from_le_bytes([
            sample[idx],
            sample[idx + 1],
            sample[idx + 2],
            sample[idx + 3],
        ]);
        let slot = ((quad.wrapping_mul(INCOMPRESSIBLE_REPEAT_HASH_MULT) as usize)
            >> (32 - INCOMPRESSIBLE_REPEAT_TABLE_BITS))
            & (INCOMPRESSIBLE_REPEAT_TABLE_LEN - 1);
        let word = slot / 64;
        let bit = 1_u64 << (slot % 64);
        let occupied = (repeat_occupied[word] & bit) != 0;
        *sampled_quads += 1;
        if occupied && repeat_table[slot] == quad {
            *repeats += 1;
        } else {
            repeat_table[slot] = quad;
            repeat_occupied[word] |= bit;
        }
        idx += 4;
    }
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
    if selection.is_full_block() {
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

    let mut counts = [0u16; 256];
    let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    // Bitset occupancy keeps this path no_std-friendly while avoiding the
    // larger per-slot bool map (and extra matcher-level scratch state).
    let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
    let mut repeats = 0usize;
    let mut sampled_quads = 0usize;

    if sample_len == block.len() {
        update_sample_metrics(
            block,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );
    } else {
        // Probe head, middle, and tail so capped samples can still reject
        // mixed-entropy blocks whose center is compressible.
        let head_len = sample_len / 3;
        let mid_len = sample_len / 3;
        let tail_len = sample_len - head_len - mid_len;
        let head = &block[..head_len];
        let mid_start = (block.len() - mid_len) / 2;
        let mid = &block[mid_start..mid_start + mid_len];
        let tail = &block[block.len() - tail_len..];
        update_sample_metrics(
            head,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );
        update_sample_metrics(
            mid,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );
        update_sample_metrics(
            tail,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );
    }

    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    let max_symbol_guard = sample_len / INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR;
    let repeat_guard = sampled_quads / INCOMPRESSIBLE_REPEAT_DIVISOR + 1;
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
        let mut sampled_quads = 0usize;

        update_sample_metrics(
            &sample,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );

        assert_eq!(sampled_quads, 1);
        assert_eq!(repeats, 0, "first quad must not be miscounted as a repeat");
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
