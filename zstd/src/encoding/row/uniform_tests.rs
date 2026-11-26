//! Indexing a range of one repeated byte into the rows.

use super::RowMatchGenerator;

/// Inserting a block of one repeated byte turns its row's head by the inserts
/// it skips and makes only the last ones, and leaves the row index exactly as
/// inserting every position does: the others land in the same row with the
/// same tag and are overwritten as the head comes round. After mixed content,
/// so the seam positions read other bytes, at every row width.
#[test]
fn a_repeated_byte_block_inserts_to_the_dense_rows() {
    let prefix: alloc::vec::Vec<u8> = (0..3000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    // Every key width: past four bytes the last positions hash four bytes
    // instead of eight, a key of their own.
    for (search_log, mls) in [4usize, 5, 6]
        .into_iter()
        .flat_map(|l| [(l, 4u32), (l, 5), (l, 6)])
    {
        for byte in [0u8, 0x41] {
            let build = || {
                let mut m = RowMatchGenerator::new(1 << 20);
                m.row_log = search_log;
                m.row_hash_mls = mls;
                m.set_hash_bits(16);
                m.ensure_tables();
                m.commit_input(&prefix);
                m.commit_input(alloc::vec![byte; 5000]);
                m
            };
            let mut range = build();
            let mut each = build();
            // The seam positions read the prefix too: indexed one by one on
            // both, so the comparison is the block's own range.
            let seam = range.history_abs_start + prefix.len() - 7;
            let start = seam + 7;
            let end = start + 5000;
            match search_log {
                4 => {
                    for m in [&mut range, &mut each] {
                        (seam..start).for_each(|pos| m.insert_position::<4>(pos));
                    }
                    range.insert_positions::<4>(start, end);
                    (start..end).for_each(|pos| each.insert_position::<4>(pos));
                }
                5 => {
                    for m in [&mut range, &mut each] {
                        (seam..start).for_each(|pos| m.insert_position::<5>(pos));
                    }
                    range.insert_positions::<5>(start, end);
                    (start..end).for_each(|pos| each.insert_position::<5>(pos));
                }
                _ => {
                    for m in [&mut range, &mut each] {
                        (seam..start).for_each(|pos| m.insert_position::<6>(pos));
                    }
                    range.insert_positions::<6>(start, end);
                    (start..end).for_each(|pos| each.insert_position::<6>(pos));
                }
            }
            assert_eq!(
                range.row_positions().to_vec(),
                each.row_positions().to_vec(),
                "row log {search_log}, mls {mls}, byte {byte:#x}: positions"
            );
            assert_eq!(
                range.row_tags().to_vec(),
                each.row_tags().to_vec(),
                "row log {search_log}, mls {mls}, byte {byte:#x}: heads and tags"
            );
        }
    }
}
