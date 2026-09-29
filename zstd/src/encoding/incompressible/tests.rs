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

/// A block carrying a copy of its own earlier content is answered wherever the
/// copy begins, once it is longer than a run spacing and a run.
///
/// Such a block reads as incompressible to every sample of it, and the copy is a
/// match the search would have found. Runs at fixed places (the start and the
/// middle) missed a copy that began between them; runs every
/// `PROBE_FRACTION` of the block bound what a copy has to be to hide, whatever
/// its offset.
#[test]
fn the_content_grid_answers_a_block_that_copies_itself() {
    const BLOCK: usize = 128 * 1024;
    const WIDE: usize = 8 * 1024 * 1024;
    let step = SeenContentGrid::RECORD_STEP;
    let spacing = BLOCK / SeenContentGrid::PROBE_FRACTION;
    let span = spacing + step + SeenContentGrid::KEY_LEN;

    let mut halves = deterministic_bytes(0xBEEF, BLOCK);
    halves.copy_within(0..BLOCK / 2, BLOCK / 2);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(
        grid.record_and_report_repeat(&halves, WIDE),
        "a block of two identical halves is half a block of match",
    );

    // Copies of the guaranteed length at offsets that avoid every run start,
    // including the one that sat between the old start and middle runs.
    // Every offset leaves the original `[0, span)` intact.
    for at in [span + 1, 76 * 1024, BLOCK - span - 777, BLOCK - span] {
        let mut block = deterministic_bytes(0xBEEF, BLOCK);
        block.copy_within(0..span, at);
        let mut grid = SeenContentGrid::default();
        grid.reset_for_frame();
        assert!(
            grid.record_and_report_repeat(&block, WIDE),
            "a {span}-byte copy at {at} must be answered",
        );
    }

    // A copy shorter than a spacing between two run starts can hide: the bound
    // the spacing buys. A change that answers it has changed the run spacing
    // and owes its cost on incompressible input.
    let short = spacing / 2;
    let mut block = deterministic_bytes(0xBEEF, BLOCK);
    block.copy_within(0..short, spacing + step + 64);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    assert!(
        !grid.record_and_report_repeat(&block, WIDE),
        "a copy between two runs is answered, so the spacing changed",
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

/// A frame whose compressible prefix runs past the step index must not carry an
/// origin the index cannot hold.
///
/// Blocks the grid is never asked about advance the offset without going through
/// the walk that moves the origin, so a long enough prefix — a couple of
/// tebibytes of anything the matcher codes well — leaves an origin that the
/// first probe after it then has to narrow by more than the index can express.
#[test]
fn the_content_grid_bounds_the_origin_across_an_unasked_prefix() {
    const BLOCK: usize = 128 * 1024;
    const WIDE: usize = 8 * 1024 * 1024;
    let limit = (u64::from(u32::MAX) + 1) * SeenContentGrid::RECORD_STEP as u64;

    let block = deterministic_bytes(0x51DE, BLOCK);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    // A prefix of blocks nobody asked about, walking the frame up to the index
    // and one block past it.
    grid.frame_offset = limit - BLOCK as u64 / 2;
    grid.record_searched(&block, WIDE);
    assert!(
        !grid.record_and_report_repeat(&block, WIDE),
        "nothing recorded before this can make it a repeat",
    );
    assert!(
        grid.record_and_report_repeat(&block, WIDE),
        "the block right behind this one is what the matcher would find",
    );
}

/// Crossing the index on blocks the grid records nothing for must carry the
/// records with the origin, not just move the origin.
///
/// A frame that has recorded something and then walks past the index on
/// sub-key blocks keeps every record dated in the old coordinates: the next
/// probe measures a distance from an offset AHEAD of itself, reads no repeat,
/// and a duplicate the matcher still holds goes out raw.
#[test]
fn the_content_grid_carries_its_records_when_a_skip_crosses_the_index() {
    const BLOCK: usize = 128 * 1024;
    const WIDE: usize = 8 * 1024 * 1024;
    let step = SeenContentGrid::RECORD_STEP as u64;
    let limit = (u64::from(u32::MAX) + 1) * step;

    let block = deterministic_bytes(0x51DE, BLOCK);
    let crumb = deterministic_bytes(0xF00D, SeenContentGrid::KEY_LEN - 1);
    let mut grid = SeenContentGrid::default();
    grid.reset_for_frame();
    // Just under the index, with a record in reach of what follows: after this
    // block the frame sits four bytes short of the boundary.
    grid.frame_offset = limit - BLOCK as u64 - 4;
    assert!(!grid.record_and_report_repeat(&block, WIDE));
    // A few bytes carry the frame across the boundary.
    for _ in 0..8 {
        grid.record_searched(&crumb, WIDE);
    }
    assert!(
        grid.record_and_report_repeat(&block, WIDE),
        "the block recorded a few bytes ago is what the matcher would find",
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

/// The grid's placement and both of its full mixes spread keys over the slots
/// and keep the tag independent of the slot.
///
/// A mix that correlates them fails in a way no single-key test sees: keys
/// crowding into fewer slots evict each other's records, and a repeat whose
/// record was evicted is missed outright. So the check is statistical, over the
/// key shapes the grid is fed: overlapping eight-byte windows of noise and of
/// structured text, and counters that differ only in their low bits. Every mix
/// lays the slot in its high half and the tag in bits 16 to 23.
#[test]
fn the_grid_mixes_spread_slots_and_keep_the_tag_independent() {
    const SLOT_BITS: u32 = 16;
    let noise = deterministic_bytes(0x5EED, 96 * 1024);
    let mut text = Vec::new();
    let mut line = 0u32;
    while text.len() < 96 * 1024 {
        text.extend_from_slice(
            alloc::format!("record {line}: value {}\n", line * 7 % 1000).as_bytes(),
        );
        line += 1;
    }
    let mut keys: Vec<u64> = Vec::new();
    for source in [&noise, &text] {
        keys.extend(
            source
                .windows(8)
                .map(|w| u64::from_le_bytes(w.try_into().unwrap())),
        );
    }
    keys.extend(0..64 * 1024u64);
    keys.sort_unstable();
    keys.dedup();

    type Mix = fn(u64) -> u64;
    // The full mixes are meant to look random, so a deviation either way is a
    // defect. The placement is a multiplicative hash, which spreads counters
    // more evenly than random — fewer empty slots and fewer shared tags than a
    // random mix — so for it only crowding, a deviation upward, is one.
    let mixes: [(&str, Mix, bool); 3] = [
        ("placement", SeenContentGrid::placement, false),
        ("wide", SeenContentGrid::avalanche_wide, true),
        ("narrow", SeenContentGrid::avalanche_narrow, true),
    ];
    for (name, mix, random) in mixes {
        let within = |ratio: f64| {
            if random {
                (0.85..1.15).contains(&ratio)
            } else {
                ratio < 1.15
            }
        };
        let mut fields: Vec<(u32, u8)> = keys
            .iter()
            .map(|&key| {
                let mixed = mix(key);
                (
                    ((mixed >> 32) as u32) & ((1 << SLOT_BITS) - 1),
                    (mixed >> 16) as u8 | 1,
                )
            })
            .collect();
        fields.sort_unstable();

        // Poisson occupancy: at a load of `lambda` keys per slot, a share
        // `e^-lambda` of the slots stays empty.
        let slots = 1usize << SLOT_BITS;
        let lambda = keys.len() as f64 / slots as f64;
        let mut used = 0usize;
        let mut slot_pairs = 0usize;
        let mut tag_pairs = 0usize;
        let mut i = 0;
        while i < fields.len() {
            let slot = fields[i].0;
            let mut j = i;
            while j < fields.len() && fields[j].0 == slot {
                j += 1;
            }
            used += 1;
            let n = j - i;
            slot_pairs += n * (n - 1) / 2;
            let mut k = i;
            while k < j {
                let mut m = k;
                while m < j && fields[m].1 == fields[k].1 {
                    m += 1;
                }
                tag_pairs += (m - k) * (m - k - 1) / 2;
                k = m;
            }
            i = j;
        }
        let empty = (slots - used) as f64;
        let expected_empty = slots as f64 * (-lambda).exp();
        assert!(
            within(empty / expected_empty),
            "{name}: {empty} empty slots against {expected_empty:.0} expected at load {lambda:.2}",
        );
        // The tag keeps seven free bits, so two keys sharing a slot share a tag
        // one time in 128 when the two are independent.
        let expected_tag_pairs = slot_pairs as f64 / 128.0;
        assert!(
            within(tag_pairs as f64 / expected_tag_pairs),
            "{name}: {tag_pairs} same-slot pairs share a tag against {expected_tag_pairs:.0} expected",
        );
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
fn a_quad_equal_to_an_empty_slot_is_not_a_repeat() {
    // 0 and 1 are the values the empty table holds, and u32::MAX the value an
    // all-ones fill would; each occurs once, so none may count as a repeat.
    let mut sample = Vec::new();
    for quad in [0_u32, 1, u32::MAX] {
        sample.extend_from_slice(&quad.to_le_bytes());
    }
    let mut repeat_table = empty_repeat_table();
    let mut repeats = 0usize;

    // Guard set high so the early exit never fires and every quad is scanned.
    let bailed = count_quad_repeats(&sample, &mut repeat_table, &mut repeats, usize::MAX);

    assert!(!bailed, "high guards must not trigger an early exit");
    assert_eq!(
        repeats, 0,
        "a first occurrence must not be counted as a repeat"
    );
}

#[test]
fn count_quad_repeats_early_exits_on_repetitive_input() {
    // 32 identical 4-byte quads: the repeat count climbs past any small
    // guard, exercising the early-exit `true` path directly.
    let sample = [0xAB_u8; 128];
    let mut repeat_table = empty_repeat_table();
    let mut repeats = 0usize;

    // Guard of 1: the first quad seeds the table, the second is the first
    // counted repeat (repeats == 1), the third pushes repeats past the
    // guard and returns `true`.
    let bailed = count_quad_repeats(&sample, &mut repeat_table, &mut repeats, 1);

    assert!(bailed, "repetitive input must trigger the early exit");
    assert!(repeats > 1, "repeat count must have exceeded the guard");
}

/// The classifier as it stood before its scan was narrowed to 32-bit words,
/// sixteen-bit counts and an uncleared table, kept verbatim as the reference the
/// rewrite must agree with.
fn reference_sample_looks_incompressible(block: &[u8]) -> bool {
    let sample_len = block.len().min(RAW_FAST_PATH_MAX_SAMPLE_LEN);
    if sample_len < RAW_FAST_PATH_MIN_SAMPLE_LEN {
        return false;
    }
    let mut regions: [&[u8]; 3] = [&[], &[], &[]];
    let region_count = if sample_len == block.len() {
        regions[0] = block;
        1
    } else {
        let head_len = sample_len / 3;
        let mid_len = sample_len / 3;
        let tail_len = sample_len - head_len - mid_len;
        let mid_start = (block.len() - mid_len) / 2;
        regions[0] = &block[..head_len];
        regions[1] = &block[mid_start..mid_start + mid_len];
        regions[2] = &block[block.len() - tail_len..];
        3
    };
    let max_symbol_guard = sample_len / INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR;
    let total_quads: usize = regions[..region_count].iter().map(|r| r.len() / 4).sum();
    let repeat_guard = total_quads / INCOMPRESSIBLE_REPEAT_DIVISOR + 1;
    let mut counts = [0u32; 256];
    let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_TABLE_LEN / 64];
    let mut repeats = 0usize;
    for sample in &regions[..region_count] {
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
            let slot = (quad.wrapping_mul(INCOMPRESSIBLE_REPEAT_HASH_MULT) as usize)
                >> (32 - INCOMPRESSIBLE_REPEAT_TABLE_BITS);
            let word = slot / 64;
            let bit = 1_u64 << (slot % 64);
            let occupied = (repeat_occupied[word] & bit) != 0;
            if occupied && repeat_table[slot] == quad {
                repeats += 1;
                if repeats > repeat_guard {
                    return false;
                }
            } else {
                repeat_table[slot] = quad;
                repeat_occupied[word] |= bit;
            }
            idx += 4;
        }
        while idx < len {
            counts[sample[idx] as usize] += 1;
            idx += 1;
        }
    }
    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    distinct >= INCOMPRESSIBLE_MIN_DISTINCT_BYTES
        && max_freq <= max_symbol_guard
        && repeats <= repeat_guard
}

/// The rewritten scan decides every block exactly as the one it replaced.
///
/// The verdict picks which blocks go out raw, so a scan that is faster but
/// disagrees on even one shape changes compressed output. The corpus straddles
/// every threshold the verdict reads: lengths around the sample cap, the
/// three-region split and the quad remainder; alphabets around the
/// distinct-byte floor; a skewed byte around the frequency ceiling; and repeat
/// densities around the quad-repeat guard. Both outcomes have to occur, or the
/// corpus is not testing the boundary.
#[test]
fn the_rewritten_scan_decides_every_block_as_before() {
    let lengths = [
        0,
        RAW_FAST_PATH_MIN_SAMPLE_LEN - 1,
        RAW_FAST_PATH_MIN_SAMPLE_LEN,
        RAW_FAST_PATH_MIN_BLOCK_LEN - 1,
        RAW_FAST_PATH_MIN_BLOCK_LEN,
        1000,
        1024,
        1027,
        PARALLEL_HISTOGRAM_MIN_LEN - 1,
        PARALLEL_HISTOGRAM_MIN_LEN,
        RAW_FAST_PATH_MAX_SAMPLE_LEN - 1,
        RAW_FAST_PATH_MAX_SAMPLE_LEN,
        RAW_FAST_PATH_MAX_SAMPLE_LEN + 1,
        RAW_FAST_PATH_MAX_SAMPLE_LEN + 3,
        10 * 1024,
        64 * 1024 + 5,
        128 * 1024,
    ];
    let mut outcomes = [0usize; 2];
    let mut seed = 0x1234_5678_9ABC_DEF1_u64;
    for &len in &lengths {
        for alphabet in [16usize, 180, 199, 200, 201, 230, 256] {
            for skew_per_mille in [0usize, 30, 42, 45, 60] {
                for repeat_per_mille in [0usize, 10, 15, 16, 20, 40, 200] {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let mut block = deterministic_bytes(seed | 1, len);
                    for (i, byte) in block.iter_mut().enumerate() {
                        let pick = (i as u64).wrapping_mul(seed | 1) >> 7;
                        *byte = if (pick % 1000) < skew_per_mille as u64 {
                            0x42
                        } else {
                            (*byte as usize % alphabet) as u8
                        };
                    }
                    // Copy a quad from four bytes back at the chosen density,
                    // which is what the repeat guard counts.
                    let mut i = 8;
                    while i + 4 <= block.len() {
                        let pick = (i as u64).wrapping_mul(seed.rotate_left(17) | 1) >> 11;
                        if (pick % 1000) < repeat_per_mille as u64 {
                            block.copy_within(i - 8..i - 4, i);
                        }
                        i += 4;
                    }
                    let expected = reference_sample_looks_incompressible(&block);
                    assert_eq!(
                        sample_looks_incompressible(&block),
                        expected,
                        "len {len}, alphabet {alphabet}, skew {skew_per_mille}, repeats {repeat_per_mille}",
                    );
                    if len >= RAW_FAST_PATH_MIN_BLOCK_LEN {
                        assert_eq!(block_looks_incompressible(&block), expected);
                    }
                    outcomes[usize::from(expected)] += 1;
                }
            }
        }
    }
    assert!(
        outcomes[0] > 0 && outcomes[1] > 0,
        "the corpus must reach both verdicts, got {outcomes:?}",
    );
}

/// A frame that stores its literals raw searches its 2-24 KiB blocks instead of
/// asking the classifier, where the search is the cheaper of the two; every
/// other block, every block of a frame that codes its literals, and every block
/// of a frame whose tables sit on freshly mapped pages still asks.
#[test]
fn only_small_blocks_of_raw_literal_frames_skip_the_classifier() {
    for len in [2 * 1024, 10 * 1024, 24 * 1024] {
        assert!(
            !raw_skip_worth_asking(true, false, len),
            "{len} bytes, raw literals"
        );
        assert!(
            raw_skip_worth_asking(false, false, len),
            "{len} bytes, coded literals"
        );
        assert!(
            raw_skip_worth_asking(true, true, len),
            "{len} bytes, fresh pages"
        );
    }
    for len in [
        RAW_FAST_PATH_MIN_BLOCK_LEN,
        2 * 1024 - 1,
        24 * 1024 + 1,
        128 * 1024,
    ] {
        assert!(
            raw_skip_worth_asking(true, false, len),
            "{len} bytes, raw literals"
        );
    }
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
