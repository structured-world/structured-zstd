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
        RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES
    ));
    assert!(!compression_level_allows_raw_fast_path(
        CompressionLevel::Best,
        RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES + 1
    ));
}

#[test]
fn level4_row_raw_fast_path_allowed_with_better_window_reach() {
    assert!(compression_level_allows_raw_fast_path(
        CompressionLevel::Level(4),
        RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES
    ));
    // Over-cap numeric level is rejected, same boundary as `Best`, so the
    // two branches can't drift apart.
    assert!(!compression_level_allows_raw_fast_path(
        CompressionLevel::Level(4),
        RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES + 1
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
    let mut block = deterministic_bytes(0x9E37_79B9_7F4A_7C15, RAW_FAST_PATH_MAX_SAMPLE_LEN * 2);
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
