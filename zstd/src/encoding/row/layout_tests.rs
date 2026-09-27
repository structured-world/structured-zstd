//! Laying the shared table buffer out for a finder.

use super::{LazyFinder, ROW_EMPTY_SLOT, RowMatchGenerator};

/// The chain and tree finders take a buffer of the same length at the same
/// widths but mark an empty slot differently, so switching between them keeps
/// the allocation and rewrites every slot with the new finder's empty value: a
/// chain's empty marker read by the tree walker is a link to position
/// `ROW_EMPTY_SLOT`, and a stale link either way is a candidate that was never
/// inserted.
#[test]
fn switching_between_finders_of_one_length_refills_the_buffer() {
    let mut matcher = RowMatchGenerator::new(1 << 20);
    matcher.set_hash_bits(16);
    matcher.finder = LazyFinder::Chain;
    matcher.ensure_tables();
    let len = matcher.tables_len();
    assert!(
        matcher.tables.iter().all(|&s| s == ROW_EMPTY_SLOT),
        "a chain layout starts empty"
    );
    matcher.tables.iter_mut().for_each(|s| *s = 77);

    matcher.finder = LazyFinder::Tree;
    matcher.ensure_tables();
    assert_eq!(
        matcher.tables_len(),
        len,
        "the same length for both finders"
    );
    assert!(
        matcher.tables.iter().all(|&s| s == 0),
        "the tree layout starts empty on its own sentinel"
    );

    matcher.tables.iter_mut().for_each(|s| *s = 77);
    matcher.ensure_tables();
    assert!(
        matcher.tables.iter().all(|&s| s == 77),
        "a layout already in place is kept"
    );
}
