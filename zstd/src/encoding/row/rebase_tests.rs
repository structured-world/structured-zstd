//! Rebasing the row index must touch only the positions.
//!
//! The full path that reaches [`RowMatchGenerator::rebase_positions`] needs
//! about four gibibytes through one matcher, which no unit test can afford, so
//! these drive the step itself.

use super::{ROW_EMPTY_SLOT, RowMatchGenerator};

/// The positions, the slot cursors and the hash tags share one buffer, and only
/// the positions hold absolute cursors. Rebasing the whole buffer reads the
/// cursor and tag bytes as positions and writes the empty sentinel over them,
/// which leaves every cursor at 255 — past the end of any row — and every tag
/// at `0xFF`, so the index no longer describes the rows it indexes.
#[test]
fn rebasing_leaves_the_cursors_and_tags_alone() {
    let mut matcher = RowMatchGenerator::new(1 << 20);
    matcher.set_hash_bits(16);
    matcher.ensure_tables();

    // Positions above the floor survive the rebase; the tail is written with
    // values the sentinel would visibly destroy.
    let floor = 1_000usize;
    for (i, slot) in matcher.row_positions_mut().iter_mut().enumerate() {
        *slot = (floor + i) as u32;
    }
    matcher.row_heads_mut().fill(3);
    matcher.row_tags_mut().fill(0x5A);
    matcher.history_abs_start = floor;

    matcher.rebase_positions();

    assert!(
        matcher.row_heads().iter().all(|&h| h == 3),
        "the slot cursors are not positions and must survive the rebase"
    );
    assert!(
        matcher.row_tags().iter().all(|&t| t == 0x5A),
        "the hash tags are not positions and must survive the rebase"
    );
    assert_eq!(
        matcher.row_positions()[0],
        0,
        "the first position sat exactly on the floor, so it rebases to zero"
    );
    assert_eq!(
        matcher.row_positions()[1],
        1,
        "and the next one to one behind it"
    );
}

/// The same buffer in chain / tree mode holds no tail, so rebasing still walks
/// all of it.
#[test]
fn rebasing_a_chain_layout_still_walks_the_whole_buffer() {
    let mut matcher = RowMatchGenerator::new(1 << 20);
    matcher.set_hash_bits(16);
    matcher.finder = super::LazyFinder::Chain;
    matcher.ensure_tables();
    assert_eq!(
        matcher.row_positions().len(),
        0,
        "chain mode lays out no rows"
    );

    let floor = 500usize;
    for slot in matcher.tables.iter_mut() {
        *slot = floor as u32;
    }
    matcher.history_abs_start = floor;
    matcher.rebase_positions();

    assert!(
        matcher
            .tables
            .iter()
            .all(|&s| s == 0 || s == ROW_EMPTY_SLOT),
        "every slot sat on the floor, so each rebases to zero"
    );
}
