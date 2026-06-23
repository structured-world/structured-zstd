//! Stage D coverage for `MatchTable` entry points that the
//! end-to-end compression path doesn't naturally hit on CI:
//!  * `set_dictionary_limit_from_primed_bytes(0)` — the "clear"
//!    branch is only entered when a dictionary frame is reset.
//!  * BT-mode incompressible-block skip path on `skip_matching`.
//!  * `replay_history_for_rebase_bt` — exercised only when the
//!    BT cursor crosses the rolling-rebase threshold (~`u32::MAX`),
//!    so we drive it directly here.
use alloc::vec;

use super::*;

fn new_table(window: usize) -> MatchTable {
    let mut t = MatchTable::new(window);
    // window_size is driven by `push_test_chunk` (= sum of live chunk
    // lengths), so it is not preset here.
    t.hash_log = 8;
    t.chain_log = 8;
    t.hash3_log = 0;
    t
}

#[test]
fn set_dictionary_limit_from_primed_bytes_zero_clears_limit() {
    let mut t = new_table(64);
    t.history_abs_start = 100;
    t.dictionary_limit_abs = Some(123);
    t.set_dictionary_limit_from_primed_bytes(0);
    assert_eq!(t.dictionary_limit_abs, None);
}

#[test]
fn set_dictionary_limit_from_primed_bytes_offsets_from_history_start() {
    let mut t = new_table(64);
    t.history_abs_start = 100;
    t.set_dictionary_limit_from_primed_bytes(40);
    assert_eq!(t.dictionary_limit_abs, Some(140));
}

#[test]
fn dms_cache_rebuilds_across_hc_bt_layout_switch() {
    // A reused compressor that changes level across the HC↔BT boundary lands
    // back in prime_dms_* with the SAME (region, mls, hash_log) but the OTHER
    // builder. Without a layout discriminator in the cache key, the second
    // prime would reuse the first builder's tables verbatim (HC single-link
    // chain reinterpreted as a BT DUBT, or vice versa) — a silent corruption.
    // The cache key must include the layout so the switch forces a rebuild.
    let mut t = new_table(64);
    // dms_hash_log clamps to [10, hash_log]; keep hash_log above the floor.
    t.hash_log = 12;
    // Match HC's fixed mls (HC_MIN_MATCH_LEN == 4) so the BT prime resolves
    // the SAME (region, mls, hash_log) as the HC prime — then the LAYOUT field
    // is the only differing key, which is exactly what this test guards. With
    // search_mls != 4 the rebuild would happen via the mls mismatch and the
    // test would pass even without the layout discriminator.
    t.search_mls = 4;
    t.push_test_chunk(vec![7u8; 48]);
    t.ensure_tables();
    let region = 48;

    t.prime_dms_hc(region);
    assert_eq!(t.dms.table().unwrap().layout, DmsDictLayout::Hc);
    // HC chain has one `next` per dict position.
    assert_eq!(t.dms.table().unwrap().chain_table.len(), region);

    // Same region/mls/hash_log, but the BT builder must NOT reuse the HC
    // tables: it rebuilds to the BT layout (2 children per dict position).
    t.prime_dms_bt(region);
    assert_eq!(t.dms.table().unwrap().layout, DmsDictLayout::Bt);
    assert_eq!(t.dms.table().unwrap().chain_table.len(), 2 * region);

    // And back to HC rebuilds again.
    t.prime_dms_hc(region);
    assert_eq!(t.dms.table().unwrap().layout, DmsDictLayout::Hc);
    assert_eq!(t.dms.table().unwrap().chain_table.len(), region);
}

#[test]
fn skip_matching_bt_incompressible_routes_through_sparse_block() {
    let mut t = new_table(32);
    t.push_test_chunk(vec![0u8; 32]);
    t.ensure_tables();
    t.uses_bt = true;
    t.is_btultra2 = false;
    t.search_depth = 4;
    let before_skip_until = t.skip_insert_until_abs;
    t.skip_matching(Some(true));
    // BT + incompressible path must take the
    // `bt_insert_sparse_incompressible_block` branch and advance
    // `skip_insert_until_abs` to current_abs_end.
    assert!(t.skip_insert_until_abs >= t.window_size);
    assert!(t.skip_insert_until_abs > before_skip_until);
}

#[test]
fn skip_matching_bt_dense_routes_through_bt_update_tree() {
    let mut t = new_table(32);
    t.push_test_chunk(vec![1u8; 32]);
    t.ensure_tables();
    t.uses_bt = true;
    t.is_btultra2 = false;
    t.search_depth = 4;
    // `incompressible_hint = None` → dense bt_update_tree_until path
    t.skip_matching(None);
    assert_eq!(t.skip_insert_until_abs, t.history_abs_start + t.window_size);
}

#[test]
fn replay_history_for_rebase_bt_walks_inserted_prefix() {
    let mut t = new_table(64);
    // Construct a contiguous mirror long enough for the BT walker
    // (`bt_insert_step_no_rebase` reads 8-byte prefixes).
    t.history = vec![0u8; 64];
    for (i, slot) in t.history.iter_mut().enumerate() {
        *slot = (i % 17) as u8;
    }
    t.history_start = 0;
    t.history_abs_start = 0;
    t.window_size = 64;
    t.position_base = 0;
    t.search_depth = 4;
    t.uses_bt = true;
    t.ensure_tables();
    // Replay the first 32 positions; the BT walker writes entries
    // into the hash table (via `hash_table[hash] = stored`) so the
    // ground-truth observation is "some hash slots are no longer
    // HC_EMPTY".
    assert!(t.hash_table.iter().all(|&v| v == HC_EMPTY));
    t.replay_history_for_rebase_bt(0, 32);
    assert!(
        t.hash_table.iter().any(|&v| v != HC_EMPTY),
        "BT replay must populate hash table"
    );
}

#[test]
fn begin_rebase_clears_index_tables_and_resets_base() {
    let mut t = new_table(32);
    t.hash_table = vec![7; 16];
    t.chain_table = vec![9; 16];
    t.hash3_table = vec![5; 16];
    t.history_abs_start = 50;
    t.position_base = 0;
    t.index_shift = 4;
    t.allow_zero_relative_position = false;

    t.begin_rebase();

    assert_eq!(t.position_base, 50);
    assert_eq!(t.index_shift, 0);
    assert!(t.allow_zero_relative_position);
    assert!(t.hash_table.iter().all(|&v| v == HC_EMPTY));
    assert!(t.chain_table.iter().all(|&v| v == HC_EMPTY));
    assert!(t.hash3_table.iter().all(|&v| v == HC_EMPTY));
}

/// Regression: `rebase_positions_cold` must replay the HC3 side
/// table along with the main hash / chain replay. `begin_rebase`
/// zeroes `hash3_table`, so without an explicit refill every HC3
/// probe before `abs_pos` returns "empty" until the next encode
/// position falls due. On long-running btultra2 streams that
/// silently changes match selection (the btultra2 cascade leans
/// heavily on HC3 short matches).
#[test]
fn rebase_positions_cold_rebuilds_hash3_for_btultra2() {
    let mut t = new_table(64);
    t.history = b"abcdef_abcdef_abcdef_abcdef_abcdef_abcdef".to_vec();
    t.history_start = 0;
    t.history_abs_start = 0;
    t.window_size = t.history.len();
    // `history` is set directly above; just record it as one live chunk.
    t.chunk_lens.push_back(t.history.len());
    t.hash_log = 8;
    t.chain_log = 8;
    // btultra2-style: HC3 side table allocated.
    t.hash3_log = 6;
    t.is_btultra2 = true;
    t.search_depth = 4;
    t.ensure_tables();

    // Pre-fill the HC3 table the way the encoder would by walking
    // positions up to the would-be rebase point.
    t.update_hash3_until(20);
    assert!(
        t.hash3_table.iter().any(|&v| v != HC_EMPTY),
        "fixture precondition: hash3 must be non-empty before rebase"
    );

    t.rebase_positions_cold(20);

    assert!(
        t.hash3_table.iter().any(|&v| v != HC_EMPTY),
        "rebase must repopulate the HC3 side table — \
             btultra2 short-match selection depends on it"
    );
}

#[test]
fn insert_positions_with_step_zero_step_is_noop() {
    let mut t = new_table(32);
    t.history = vec![0u8; 32];
    t.push_test_chunk(vec![0u8; 32]);
    t.ensure_tables();
    let next_to_update3_before = t.next_to_update3;
    // step=0 must early-return without touching anything.
    t.insert_positions_with_step(0, 16, 0);
    assert!(t.hash_table.iter().all(|&v| v == HC_EMPTY));
    assert_eq!(t.next_to_update3, next_to_update3_before);
}

#[test]
fn insert_positions_with_step_saturating_step_breaks_loop() {
    // step = usize::MAX so first iteration overflows
    // `pos.saturating_add(step)` to usize::MAX, then the `next <= pos`
    // guard breaks out of the loop after one insert.
    let mut t = new_table(32);
    t.history = vec![1u8; 32];
    t.push_test_chunk(vec![1u8; 32]);
    t.ensure_tables();
    t.insert_positions_with_step(0, 16, usize::MAX);
    // Exactly one position should have been inserted before the
    // loop terminated — observe that only one slot is non-empty.
    let non_empty = t.hash_table.iter().filter(|&&v| v != HC_EMPTY).count();
    assert!(
        non_empty <= 1,
        "step=usize::MAX must break after the first insert"
    );
}

#[test]
fn apply_limited_update_after_long_match_hc_mode_is_noop() {
    // HC mode (`uses_bt = false`) — function must early-return
    // without mutating `skip_insert_until_abs`.
    let mut t = new_table(32);
    t.uses_bt = false;
    t.skip_insert_until_abs = 100;
    t.apply_limited_update_after_long_match(1000);
    assert_eq!(
        t.skip_insert_until_abs, 100,
        "HC mode must not adjust skip cursor"
    );
}

#[test]
fn apply_limited_update_after_long_match_bt_mode_caps_gap_at_384() {
    // BT mode with gap > 384 → cap the skip cursor so future
    // `bt_update_tree_until` doesn't walk an unbounded prefix.
    let mut t = new_table(32);
    t.uses_bt = true;
    t.skip_insert_until_abs = 0;
    // current_abs_start = 1000 → gap = 1000 → cap subtracts
    // (gap - 384).min(192) = 192, so result is 1000 - 192 = 808.
    t.apply_limited_update_after_long_match(1000);
    assert_eq!(t.skip_insert_until_abs, 808);
}

#[test]
fn apply_limited_update_after_long_match_small_gap_is_noop() {
    let mut t = new_table(32);
    t.uses_bt = true;
    t.skip_insert_until_abs = 800;
    // gap = 200 < 384 → no change.
    t.apply_limited_update_after_long_match(1000);
    assert_eq!(t.skip_insert_until_abs, 800);
}

#[test]
fn emit_optimal_plan_empty_plan_emits_full_literals() {
    let mut t = new_table(8);
    t.push_test_chunk(b"abcdefgh".to_vec());
    let mut emitted: Vec<u8> = Vec::new();
    t.emit_optimal_plan(8, &[], &mut |seq| {
        if let Sequence::Literals { literals } = seq {
            emitted.extend_from_slice(literals);
        }
    });
    assert_eq!(emitted, b"abcdefgh");
}

#[test]
fn emit_optimal_plan_skips_oversized_plan_item_and_emits_trailing_literals() {
    let mut t = new_table(8);
    t.push_test_chunk(b"abcdefgh".to_vec());
    // Plan item asks for `start + match_len > current_len` → skip.
    // The function must still emit the trailing literals at the end.
    let plan = [HcOptimalSequence {
        offset: 1,
        lit_len: 4,
        match_len: 99, // overflows the 8-byte window → continue
    }];
    let mut triples = 0usize;
    let mut trailing: Vec<u8> = Vec::new();
    t.emit_optimal_plan(8, &plan, &mut |seq| match seq {
        Sequence::Triple { .. } => triples += 1,
        Sequence::Literals { literals } => trailing.extend_from_slice(literals),
    });
    assert_eq!(triples, 0, "oversized plan item must be skipped");
    assert_eq!(
        trailing, b"abcdefgh",
        "trailing-literals path must emit the full window when plan skipped everything"
    );
}
