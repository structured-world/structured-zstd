use super::*;
use alloc::vec;

/// Sanity-check the verbatim port of `ZSTD_ldm_gearTab`. If these
/// anchors regress every downstream LDM split point drifts
/// relative to upstream output.
///
/// Indices were cross-verified by parsing upstream zstd
/// `lib/compress/zstd_ldm_geartab.h` and computing each entry's
/// positional index (upstream zstd layout: 3 entries per source line
/// starting at line 18, except the final line which holds the
/// 256th element). The full 256-entry table was also
/// byte-compared against the parsed upstream zstd at build time during
/// the port (zero mismatches).
#[test]
fn gear_tab_anchor_entries_match_known_geartab_header() {
    // First entry (upstream zstd zstd_ldm_geartab.h:18).
    assert_eq!(GEAR_TAB[0], 0xf5b8f72c5f77775c, "GEAR_TAB[0]");
    // Mid-table anchor (upstream zstd row 14 col 0 → index 42).
    assert_eq!(GEAR_TAB[42], 0x869cb54a8749c161, "GEAR_TAB[42]");
    // Three-quarter anchor (upstream zstd row 26 col 2 → index 80).
    assert_eq!(GEAR_TAB[80], 0x3bed519cbcb4e1e1, "GEAR_TAB[80]");
    // Last entry: index 255 (upstream zstd zstd_ldm_geartab.h:103).
    assert_eq!(GEAR_TAB[255], 0x2b4da14f2613d8f4, "GEAR_TAB[255]");
}

/// `stop_mask` derivation under the *common* parameter set
/// (`min_match_length = 64`, `hash_rate_log = 7`): upstream zstd
/// `zstd_ldm.c:52-53` produces
/// `(((1 << 7) - 1)) << (64 - 7) = 0x7F << 57 = 0xFE00_0000_0000_0000`.
#[test]
fn stop_mask_default_params_use_high_bit_window() {
    let state = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    assert_eq!(
        state.stop_mask, 0xFE00_0000_0000_0000,
        "default stop_mask must put the 7 active bits at the \
             top of the 64-bit window (upstream zstd zstd_ldm.c:52-53)"
    );
    assert_eq!(state.rolling, GEAR_HASH_INIT);
}

/// Degenerate path: when `hash_rate_log > min_match_length`
/// upstream zstd `zstd_ldm.c:55-56` falls back to the low-bit mask
/// `(1 << hash_rate_log) - 1` without trying to bias the bits
/// upward. Guards against silent drift if the params clamp ever
/// gets reworked.
#[test]
fn stop_mask_degenerate_path_returns_low_bit_mask() {
    let state = GearHashState::new(4, 8); // hash_rate_log > min_match
    assert_eq!(
        state.stop_mask,
        (1u64 << 8) - 1,
        "fallback mask must equal (1 << hash_rate_log) - 1 \
             (upstream zstd zstd_ldm.c:56)"
    );
}

/// `hash_rate_log == 0` also lands on the degenerate branch
/// (`> 0` precondition fails) → mask becomes `0`. Every byte
/// then qualifies as a split point. Matches upstream zstd behaviour.
#[test]
fn stop_mask_hash_rate_log_zero_disables_filter() {
    let state = GearHashState::new(LDM_MIN_MATCH_LENGTH, 0);
    assert_eq!(state.stop_mask, 0);
}

/// Verify the rolling update against a hand-traced two-byte
/// stream. Upstream zstd recurrence is `hash = (hash << 1) +
/// GEAR_TAB[byte]`. Starting from `GEAR_HASH_INIT = 0xFFFF_FFFF`:
///
///   after byte 0x00:
///     hash = (0xFFFF_FFFF << 1) + GEAR_TAB[0]
///          = 0x0000_0001_FFFF_FFFE + 0xf5b8f72c5f77775c
///          = 0xf5b8_f72e_5f77_775a
///   after byte 0x01:
///     hash = (0xf5b8_f72e_5f77_775a << 1) + GEAR_TAB[1]
///          = 0xeb71_ee5c_beee_eeb4 + 0x84935f266b7ac412
///          = 0x7005_4d83_2a69_b2c6
#[test]
fn reset_two_byte_stream_matches_hand_traced_recurrence() {
    let mut state = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    reset(&mut state, &[0x00, 0x01]);
    let expected = (((GEAR_HASH_INIT.wrapping_shl(1)).wrapping_add(GEAR_TAB[0])) << 1)
        .wrapping_add(GEAR_TAB[1]);
    assert_eq!(
        state.rolling, expected,
        "rolling recurrence (hash << 1) + GEAR_TAB[byte] regressed"
    );
}

/// `reset` must be order-equivalent to the unrolled `feed` body
/// modulo the split detection — feeding the same bytes through
/// both APIs leaves the same `rolling` value. Guards against
/// drift in the 4-wide unrolled head.
#[test]
fn reset_and_feed_produce_identical_rolling_state() {
    let data: alloc::vec::Vec<u8> = (0u8..73).collect(); // crosses the 4-wide boundary
    let mut s_reset = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    reset(&mut s_reset, &data);

    let mut s_feed = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    // `feed` with a mask that never matches → no early return,
    // exercises exactly the same recurrence as `reset`. Use a
    // mask of `u64::MAX` so the all-zeros condition is
    // unreachable for the constructed stream.
    s_feed.stop_mask = u64::MAX;
    let mut splits = vec![0usize; LDM_BATCH_SIZE];
    let (consumed, num) = feed(&mut s_feed, &data, &mut splits);
    assert_eq!(consumed, data.len());
    assert_eq!(num, 0, "u64::MAX mask must never trigger a split");
    assert_eq!(
        s_feed.rolling, s_reset.rolling,
        "reset and feed must agree on the rolling state after \
             identical input — guards the 4-wide unroll"
    );
}

/// `feed` with a zero mask flags every byte as a split. The
/// number of recorded splits is therefore `min(data.len(),
/// LDM_BATCH_SIZE)`, the consumed-byte count matches, and each
/// `splits[i]` is `i + 1` (upstream zstd stores the 1-based post-byte
/// offset).
#[test]
fn feed_zero_mask_records_split_per_byte_up_to_batch_cap() {
    let data: alloc::vec::Vec<u8> = vec![0u8; LDM_BATCH_SIZE * 2];
    let mut state = GearHashState::new(LDM_MIN_MATCH_LENGTH, 0);
    // Defensive: `new` with hash_rate_log == 0 already sets
    // stop_mask = 0, but re-assert here so the intent is obvious.
    assert_eq!(state.stop_mask, 0);

    let mut splits = vec![0usize; LDM_BATCH_SIZE];
    let (consumed, num) = feed(&mut state, &data, &mut splits);
    assert_eq!(num, LDM_BATCH_SIZE, "batch cap must be honoured");
    assert_eq!(
        consumed, LDM_BATCH_SIZE,
        "consumed must equal LDM_BATCH_SIZE"
    );
    for (i, &s) in splits.iter().enumerate() {
        assert_eq!(
            s,
            i + 1,
            "upstream zstd records 1-based post-byte indices \
                 (zstd_ldm.c:111); splits[{i}] should be {}",
            i + 1
        );
    }
}

/// Feeding in two consecutive chunks must produce the same
/// rolling state as feeding the concatenation. Guards against
/// state corruption between calls (the upstream zstd allows the caller
/// to drain its `splits` buffer and resume mid-stream).
#[test]
fn feed_concatenation_invariant() {
    let part_a: alloc::vec::Vec<u8> = (0u8..30).collect();
    let part_b: alloc::vec::Vec<u8> = (30u8..73).collect();
    let mut joined = part_a.clone();
    joined.extend_from_slice(&part_b);

    let mut splits_a = vec![0usize; LDM_BATCH_SIZE];
    let mut splits_b = vec![0usize; LDM_BATCH_SIZE];
    let mut splits_joined = vec![0usize; LDM_BATCH_SIZE];

    let mut s_chunked = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    let _ = feed(&mut s_chunked, &part_a, &mut splits_a);
    let _ = feed(&mut s_chunked, &part_b, &mut splits_b);

    let mut s_joined = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    let _ = feed(&mut s_joined, &joined, &mut splits_joined);

    assert_eq!(
        s_chunked.rolling, s_joined.rolling,
        "chunked feed must leave the same rolling state as a single feed"
    );
}

/// `splits` buffer too small must panic — upstream zstd pre-condition
/// enforced. Without this assertion a short buffer would
/// silently truncate at runtime once the inner branches start
/// indexing past the end.
#[test]
#[should_panic(expected = "LDM_BATCH_SIZE")]
fn feed_panics_on_undersized_splits_buffer() {
    let data = [0u8; 8];
    let mut state = GearHashState::new(LDM_MIN_MATCH_LENGTH, LDM_HASH_RLOG);
    let mut splits = vec![0usize; LDM_BATCH_SIZE - 1];
    let _ = feed(&mut state, &data, &mut splits);
}
