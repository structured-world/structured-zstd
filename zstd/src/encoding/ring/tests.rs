use super::*;

/// A one-segment history drains a whole window where twice the window fits
/// one allocation, and a quarter of it where it would not (a 2^30 window on a
/// 32-bit target): the room it reserves then stays within `isize::MAX` bytes,
/// the most one allocation can hold.
#[test]
fn a_one_segment_history_fits_one_allocation() {
    let block = MAX_BLOCK_SIZE as usize;
    let widest = 1usize << crate::common::MAX_WINDOW_LOG;
    let room = one_segment_room(widest, block);
    assert!(
        room <= isize::MAX as usize,
        "{room} bytes for a {widest}-byte window"
    );

    let fits = (isize::MAX as usize - 2 * block) / 2;
    assert_eq!(one_segment_drain(fits), fits);
    assert_eq!(one_segment_drain(fits + 1), (fits + 1) / 4);
    assert!(one_segment_room(fits, block) <= isize::MAX as usize);
    assert!(one_segment_room(fits + 1, block) <= isize::MAX as usize);
}

/// The largest block a window commits is the window itself below the format's
/// maximum block, and that maximum above it.
#[test]
fn the_largest_block_is_held_to_the_window() {
    let block = MAX_BLOCK_SIZE as usize;
    assert_eq!(largest_block(1024), 1024);
    assert_eq!(largest_block(block), block);
    assert_eq!(largest_block(1 << 20), block);
}
