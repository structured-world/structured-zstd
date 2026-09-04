use super::*;
use crate::encoding::CompressionLevel;
use alloc::vec;
use alloc::vec::Vec;

/// The grid has to report a block that duplicates an earlier one, and stay
/// quiet on blocks that do not — that pair is the whole contract the raw-skip
/// leans on.
#[test]
fn the_content_grid_reports_a_duplicated_block_and_nothing_else() {
    let first = deterministic_bytes(0xBEEF, 128 * 1024);
    let second = deterministic_bytes(0xF00D, 128 * 1024);
    // A window wide enough to hold the whole fixture, so nothing expires
    // during the run; expiry has its own test below.
    const WIDE: usize = 8 * 1024 * 1024;
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(
        !grid.record_and_report_repeat(&first, WIDE),
        "the first block"
    );
    assert!(
        !grid.record_and_report_repeat(&second, WIDE),
        "unrelated content must not read as a repeat",
    );
    assert!(
        grid.record_and_report_repeat(&first, WIDE),
        "a block repeating the first must be reported",
    );
    // A new frame starts with no memory of the old one.
    grid.reset_for_frame();
    assert!(
        !grid.record_and_report_repeat(&first, WIDE),
        "the grid must not carry content across frames",
    );
}

/// Content the matcher can no longer reach must stop reading as a repeat, or a
/// stream cycling blocks at a period wider than the window holds the raw skip
/// off for the rest of the frame chasing matches it cannot encode.
#[test]
fn the_content_grid_forgets_what_the_window_has_passed() {
    let block = deterministic_bytes(0xBEEF, 128 * 1024);
    let other = deterministic_bytes(0xF00D, 128 * 1024);
    // One block wide: the second block already carries the window past the
    // first, so the third sees a cleared grid.
    let window = block.len();
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(!grid.record_and_report_repeat(&block, window));
    assert!(!grid.record_and_report_repeat(&other, window));
    assert!(
        !grid.record_and_report_repeat(&block, window),
        "a repeat of content the window has passed is not reachable, so not a repeat",
    );
}

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
    let mut counts = [0u32; 256];
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
    let mut counts = [0u32; 256];
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

/// The window, not the level, is what closes the skip: a match that may reach
/// further back is worth more than one written off unsearched.
#[test]
fn the_window_ceiling_is_what_closes_the_raw_fast_path() {
    for level in [
        CompressionLevel::Best,
        CompressionLevel::Level(1),
        CompressionLevel::Level(9),
        CompressionLevel::Level(22),
    ] {
        assert!(
            compression_level_allows_raw_fast_path(level, RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES),
            "{level:?} at the ceiling",
        );
        assert!(
            !compression_level_allows_raw_fast_path(level, RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES + 1),
            "{level:?} past the ceiling",
        );
    }
    // The three named levels below the ceiling never consult it.
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
    ] {
        assert!(compression_level_allows_raw_fast_path(
            level,
            RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES + 1
        ));
    }
    assert!(!compression_level_allows_raw_fast_path(
        CompressionLevel::Uncompressed,
        1
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
