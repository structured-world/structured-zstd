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

/// A repeat shifted off any grid must still be recognised.
///
/// Sampling positions by their offset sees a duplicate only at distances that
/// happen to be a multiple of the step; content that repeats after a couple of
/// inserted bytes then reads as fresh noise and the block goes out unsearched,
/// throwing away an almost block-sized match. Anchoring on the content itself
/// is what makes the answer independent of where the bytes landed.
#[test]
fn the_content_grid_reports_a_repeat_that_is_shifted() {
    let base = deterministic_bytes(0xBEEF, 128 * 1024);
    let mut shifted = alloc::vec![0xAAu8, 0x55];
    shifted.extend_from_slice(&base[..base.len() - 2]);
    const WIDE: usize = 8 * 1024 * 1024;
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(!grid.record_and_report_repeat(&base, WIDE));
    assert!(
        grid.record_and_report_repeat(&shifted, WIDE),
        "a two-byte shift must not hide a block-sized repeat",
    );
}

/// A block carrying a copy of its own earlier content is answered where a run
/// begins inside the copy, and missed where none does.
///
/// Such a block reads as incompressible to every sample of it, and the copy is a
/// block-sized match the search would have found, so the first half is what the
/// midpoint run is for. The second half pins the bound at the placement two
/// measurements chose (see `PROBE_RUNS_PER_BLOCK`): a change that starts
/// answering it has changed the run placement and owes its own numbers.
#[test]
fn the_content_grid_answers_a_block_that_copies_itself() {
    const BLOCK: usize = 128 * 1024;
    const WIDE: usize = 8 * 1024 * 1024;

    let mut halves = deterministic_bytes(0xBEEF, BLOCK);
    halves.copy_within(0..BLOCK / 2, BLOCK / 2);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(
        grid.record_and_report_repeat(&halves, WIDE),
        "a block of two identical halves is half a block of match",
    );

    // Away from both runs: the documented bound.
    let mut offset = deterministic_bytes(0xBEEF, BLOCK);
    let span = BLOCK - 76 * 1024;
    offset.copy_within(8 * 1024..8 * 1024 + span, 76 * 1024);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(
        !grid.record_and_report_repeat(&offset, WIDE),
        "a copy away from both runs is now answered, so the placement changed \
         and its cost on incompressible input has to be re-measured",
    );
}

/// A frame long enough to exhaust the step index has to keep the records the
/// window still reaches.
///
/// The index is rebased at that point, and retiring the table wholesale there
/// throws away the last window of records — so the block right after the rebase
/// finds nothing on the grid and goes out raw although the matcher still holds
/// and has indexed its original. It is one stretch of a two-tebibyte frame, and
/// it is a whole window's worth of blocks.
#[test]
fn the_content_grid_keeps_what_the_window_reaches_across_a_rebase() {
    const BLOCK: usize = 128 * 1024;
    const WIDE: usize = 8 * 1024 * 1024;
    let limit = (u64::from(u32::MAX) + 1) * SeenContentGrid::RECORD_STEP as u64;

    let block = deterministic_bytes(0x51DE, BLOCK);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    // One block short of the limit, so the first call stays under it and the
    // second crosses.
    grid.frame_offset = limit - BLOCK as u64;
    assert!(
        !grid.record_and_report_repeat(&block, WIDE),
        "nothing is recorded yet for this one to repeat",
    );
    assert!(
        grid.record_and_report_repeat(&block, WIDE),
        "the block right behind this one is exactly what the matcher would find",
    );
}

/// Content still inside the window must keep reading as a repeat, and content
/// the window has passed must stop.
///
/// The window is the matcher's reach: a match against content it can still see
/// is worth searching for, one against content it cannot is not. With a
/// block-sized window an immediately repeated block is exactly reachable — the
/// case a table cleared wholesale on the window boundary would forget.
#[test]
fn the_content_grid_expires_a_sample_with_the_window_not_before() {
    let block = deterministic_bytes(0xBEEF, 128 * 1024);
    let other = deterministic_bytes(0xF00D, 128 * 1024);
    let window = block.len();
    let mut grid = SeenContentGrid::default();

    grid.reset_for_frame();
    assert!(!grid.record_and_report_repeat(&block, window));
    assert!(
        grid.record_and_report_repeat(&block, window),
        "the block right behind is still within a block-sized window",
    );

    grid.reset_for_frame();
    assert!(!grid.record_and_report_repeat(&block, window));
    assert!(!grid.record_and_report_repeat(&other, window));
    assert!(
        !grid.record_and_report_repeat(&block, window),
        "two blocks back is past a block-sized window, so not reachable",
    );
}

/// Recording must not depend on what bytes the content happens to contain.
///
/// The scheme this replaced keyed on the positions carrying one chosen byte
/// value, so a block containing none of it recorded nothing at all and its
/// exact copy went out raw with a block-sized match sitting right there. The
/// grid is fixed stream offsets now, which no content can be missing.
#[test]
fn the_content_grid_records_a_block_whatever_bytes_it_holds() {
    let mut block = deterministic_bytes(0xC0DE, 64 * 1024);
    // One byte value removed entirely, the case that broke the old scheme.
    for byte in &mut block {
        if *byte == 0x9E {
            *byte = 0x9F;
        }
    }
    assert!(!block.contains(&0x9E));
    const WIDE: usize = 8 * 1024 * 1024;
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(!grid.record_and_report_repeat(&block, WIDE), "the first");
    assert!(
        grid.record_and_report_repeat(&block, WIDE),
        "an exact copy must be recognised whatever bytes the block is made of",
    );
}

/// A run of the same block must keep reporting, not every other one.
///
/// A hit has to refresh the slot it hit: leaving the recorded offset at the
/// FIRST occurrence makes the third one measure its distance from there, which
/// with a window of one block reads as out of reach even though the block right
/// behind it is exactly what the matcher would find. Every other block of a
/// repeating run would then go out raw.
#[test]
fn the_content_grid_keeps_reporting_a_run_of_the_same_block() {
    let block = deterministic_bytes(0xBEEF, 128 * 1024);
    let window = block.len();
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(!grid.record_and_report_repeat(&block, window), "the first");
    assert!(
        grid.record_and_report_repeat(&block, window),
        "the second repeats the first",
    );
    // The answer above can survive a stale slot by luck — some anchors of the
    // second block miss and are written fresh — so check the state itself: what
    // the second block matched must now be dated to the second block, or the
    // third will measure its distance from the first and read as out of reach.
    let stale = grid
        .slots
        .iter()
        .map(|word| SeenSample::unpack(*word))
        .filter(|slot| {
            slot.fingerprint != 0
                && u64::from(slot.at_step) * (SeenContentGrid::RECORD_STEP as u64)
                    < block.len() as u64
        })
        .count();
    assert_eq!(
        stale, 0,
        "{stale} slots still carry the first block's offset after the second matched them",
    );
    assert!(
        grid.record_and_report_repeat(&block, window),
        "the third repeats the second, which is still within a block-sized window",
    );
}

/// A block shorter than one key must be answered, not indexed — the last block
/// of a frame is routinely a handful of bytes, and reading a key out of it
/// would run off the end.
#[test]
fn the_content_grid_answers_a_block_shorter_than_its_key() {
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    for len in 0..SeenContentGrid::KEY_LEN {
        assert!(!grid.record_and_report_repeat(&vec![0xC3; len], 8 * 1024 * 1024));
    }
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
    // The named levels read the same ceiling. Their preset window is well under
    // it, but a public `window_log` override moves the window without moving the
    // level, and a named level must not then be allowed a reach a numeric level
    // asking for the same thing is refused.
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
    ] {
        assert!(compression_level_allows_raw_fast_path(
            level,
            RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES
        ));
        assert!(
            !compression_level_allows_raw_fast_path(level, RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES + 1),
            "{level:?} with an overridden window past the ceiling",
        );
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

/// A repeat the grid reports must be a repeat the matcher can then FIND.
///
/// The two halves of that contract live apart: the grid decides a block is worth
/// searching, and the backend has to have indexed the earlier block for the
/// search to land on anything. A window small enough to put the Row backend on
/// its hash chain took a path that indexed nothing at all when a block was
/// skipped, so the search ran over an empty chain and both copies went out raw.
#[test]
fn a_skipped_block_is_indexed_for_the_chain_finder_too() {
    use crate::encoding::{CompressionParameters, compress_with_parameters};

    // 16 KiB window puts Row on the chain finder rather than rows.
    const WINDOW_LOG: u32 = 14;
    const BLOCK: usize = 128 * 1024;
    // Two 128 KiB segments of source, which the window cuts into 16 KiB blocks:
    // the second segment opens with the first's tail, so the repeat is inside a
    // 16 KiB window and a search would code most of it as one match.
    let first = deterministic_bytes(0x51DE, BLOCK);
    let mut input = first.clone();
    input.extend_from_slice(&first[BLOCK - 8 * 1024..]);
    input.extend_from_slice(&deterministic_bytes(0xF00D, BLOCK - 8 * 1024));

    let params = CompressionParameters::builder(CompressionLevel::Level(5))
        .window_log(WINDOW_LOG)
        .build()
        .expect("level 5 with a 16 KiB window is a valid configuration");
    let out = compress_with_parameters(&input, &params);

    // The repeated 8 KiB has to come back as a match, not as 8 KiB of literals.
    assert!(
        out.len() < input.len() - 6 * 1024,
        "{} bytes from {}: the repeated tail was not found",
        out.len(),
        input.len(),
    );
}
