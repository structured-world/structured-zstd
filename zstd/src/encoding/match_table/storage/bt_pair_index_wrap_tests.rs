use super::MatchTable;

/// A node's pair sits at its stored index's low `bt_log` bits, doubled, so
/// any `bt_mask + 1` consecutive stored indices (the tree's reach behind a
/// position) occupy distinct pairs, whatever the coordinates. A slot formula
/// that let two of them share a pair would overwrite a live node's children.
#[test]
fn bt_pair_index_gives_a_window_of_stored_indices_distinct_pairs() {
    let mut table = MatchTable::new(1 << 20);
    // `bt_log = chain_log - 1 = 3`, mask 0b0111: eight pairs.
    table.chain_log = 4;
    // Coordinates that must not enter the slot.
    table.position_base = 12_345;
    table.index_shift = 678;
    let bt_mask = table.bt_mask();
    assert_eq!(bt_mask, 0b0111);

    for first in [1usize, 5, 8, 1 << 20, u32::MAX as usize - 8] {
        let mut seen = [false; 8];
        for stored in first..=first + bt_mask {
            let pair = table.bt_pair_index_for_stored(stored);
            assert_eq!(pair, 2 * (stored & bt_mask), "stored={stored}");
            assert!(
                pair + 1 < 2 << table.bt_log(),
                "stored={stored}: pair in range"
            );
            assert!(
                !seen[pair / 2],
                "stored={stored}: pair already taken in this window"
            );
            seen[pair / 2] = true;
        }
    }
}
