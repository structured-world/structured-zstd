use super::MatchTable;

/// `bt_pair_index_for_abs` switched from `+` to `wrapping_add` so the
/// release-mode overflow branch and the debug-mode panic stay off
/// the hot path on rare 32-bit streams where `abs_pos + index_shift`
/// overflows `usize`. This test forces that overflow and verifies
/// the returned BT slot still equals what the modulo-ring identity
/// promises: `(abs_pos + index_shift) mod 2^bt_log`, doubled
/// because the table stores pointer pairs.
#[test]
fn bt_pair_index_matches_modulo_ring_after_wraparound() {
    let mut table = MatchTable::new(1 << 20);
    // Small BT ring (`bt_log = chain_log - 1 = 3` → mask = 0b0111) so
    // the modular identity is easy to read at a glance.
    table.chain_log = 4;
    // Use a wide `index_shift` so the addition wraps for many of
    // the `abs_pos` values we probe below.
    table.index_shift = usize::MAX - 5;

    let bt_mask = table.bt_mask();
    assert_eq!(bt_mask, 0b0111);

    for abs_pos in [0usize, 1, 4, 5, 6, 7, 8, 12, 17] {
        let got = table.bt_pair_index_for_abs(abs_pos);
        let expected = 2 * (abs_pos.wrapping_add(table.index_shift) & bt_mask);
        assert_eq!(
            got, expected,
            "abs_pos={abs_pos}: wrapping_add ring slot must match the masked sum"
        );
    }

    // Spot-check one value where overflow is certain (abs_pos > 5)
    // and the ring index has a stable closed form.
    // abs_pos=7, index_shift=usize::MAX-5: sum wraps to 1, mask -> 1, doubled -> 2.
    assert_eq!(table.bt_pair_index_for_abs(7), 2);
    // abs_pos=14: sum wraps to 8, masked -> 0, doubled -> 0.
    assert_eq!(table.bt_pair_index_for_abs(14), 0);
}

/// Sanity check the non-overflow path keeps the same identity so a
/// future refactor cannot regress the common case while leaving the
/// overflow case green.
#[test]
fn bt_pair_index_matches_modulo_ring_without_overflow() {
    let mut table = MatchTable::new(1 << 20);
    table.chain_log = 8; // bt_log = 7, mask = 0x7f
    table.index_shift = 17;

    let bt_mask = table.bt_mask();
    for abs_pos in [0usize, 1, 16, 32, 64, 127, 128, 255, 1 << 20] {
        let got = table.bt_pair_index_for_abs(abs_pos);
        let expected = 2 * ((abs_pos + table.index_shift) & bt_mask);
        assert_eq!(got, expected, "abs_pos={abs_pos}");
    }
}
