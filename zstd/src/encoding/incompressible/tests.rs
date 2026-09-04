use super::*;
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
