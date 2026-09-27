use super::*;

/// A trailing part as large as the block it is sized for.
fn one_byte_per_block_byte(block: usize) -> usize {
    block
}

/// Lays `ws` out anew with `leading` bytes for tables and `trailing` for the
/// context's buffers, the window leaving the block ceiling as it is.
fn lay_out(ws: &mut Workspace, leading: usize, trailing: usize) {
    if trailing == 0 {
        ws.begin_layout(0, no_trailing);
    } else {
        ws.begin_layout(trailing, one_byte_per_block_byte);
    }
    ws.open(leading, usize::MAX);
}

/// An open layout of exactly `bytes`, all of them leading.
fn opened(bytes: usize) -> Workspace {
    let mut ws = Workspace::new();
    lay_out(&mut ws, bytes, 0);
    ws
}

// Every region begins on the workspace alignment, from either end.
#[test]
fn regions_start_aligned_from_both_ends() {
    let mut ws = opened(region_bytes::<u32>(10) + region_bytes::<u8>(3) + region_bytes::<u16>(7));
    let table = ws.table::<u32>(10, 0);
    let mut bytes = ws.buffer::<u8>(3);
    let mut halves = ws.buffer::<u16>(7);
    assert_eq!(table.as_ptr() as usize % ALIGN, 0);
    assert_eq!(bytes.as_mut_ptr() as usize % ALIGN, 0);
    assert_eq!(halves.as_mut_ptr() as usize % ALIGN, 0);
}

// Regions of one layout never share a byte: writing one leaves the others
// exactly as they were.
#[test]
fn regions_of_one_layout_are_disjoint() {
    let mut ws = opened(2 * region_bytes::<u32>(20) + region_bytes::<u8>(100));
    let mut first = ws.table::<u32>(20, 7);
    let second = ws.table::<u32>(20, 9);
    let mut tail = ws.buffer::<u8>(100);
    first.as_mut_slice().fill(u32::MAX);
    tail.extend_from_slice(&[0xAB; 100]);
    assert!(second.as_slice().iter().all(|&v| v == 9));
    assert!(first.as_slice().iter().all(|&v| v == u32::MAX));
    assert!(tail.iter().all(|&b| b == 0xAB));
}

// A table starts with every value set, so reading it before any write is
// defined and sees the fill value.
#[test]
fn a_new_table_starts_filled() {
    let mut ws = opened(region_bytes::<u64>(33));
    let table = ws.table::<u64>(33, 0x1234);
    assert_eq!(table.len(), 33);
    assert!(table.as_slice().iter().all(|&v| v == 0x1234));
}

// The next frame's layout puts a table of the same size back on the same bytes,
// and its holder keeps what it wrote: that is what lets a match finder carry a
// table across frames without clearing it.
#[test]
fn a_table_laid_out_again_keeps_its_values() {
    let mut ws = opened(region_bytes::<u32>(64) + region_bytes::<u8>(64));
    let mut table: Table<u32> = Table::empty();
    assert!(!table.bind(&mut ws, 64, 0));
    table.as_mut_slice()[5] = 99;
    lay_out(&mut ws, region_bytes::<u32>(64), region_bytes::<u8>(64));
    assert!(table.bind(&mut ws, 64, 0));
    assert_eq!(table[5], 99);
}

// A table that changes size, or whose allocation grew, starts over at its
// empty value: the bytes it lands on may belong to anything.
#[test]
fn a_table_that_moves_starts_over() {
    let mut ws = opened(region_bytes::<u32>(64));
    let mut table: Table<u32> = Table::empty();
    table.bind(&mut ws, 64, 0);
    table.as_mut_slice().fill(5);

    lay_out(&mut ws, region_bytes::<u32>(32), 0);
    assert!(
        !table.bind(&mut ws, 32, 7),
        "a resized table is not a continuation"
    );
    assert!(table.iter().all(|&v| v == 7));

    table.as_mut_slice().fill(5);
    lay_out(&mut ws, region_bytes::<u32>(4096), 0);
    assert!(
        !table.bind(&mut ws, 32, 7),
        "a grown allocation is not a continuation"
    );
    assert!(table.iter().all(|&v| v == 7));
}

// A table carved behind another that changed size lands elsewhere and must not
// read the bytes it finds there as its own.
#[test]
fn a_table_shifted_by_its_neighbour_starts_over() {
    let mut ws = opened(3 * region_bytes::<u32>(64));
    let mut first: Table<u32> = Table::empty();
    let mut second: Table<u32> = Table::empty();
    first.bind(&mut ws, 64, 0);
    second.bind(&mut ws, 64, 0);
    second.as_mut_slice().fill(3);

    lay_out(&mut ws, 3 * region_bytes::<u32>(64), 0);
    first.bind(&mut ws, 128, 0);
    assert!(!second.bind(&mut ws, 64, 1));
    assert!(second.iter().all(|&v| v == 1));
}

// A copy of a workspace table owns its values: writing the original afterwards
// leaves the copy alone, and restoring the copy into the workspace table puts
// the values back in place without moving it out of the workspace.
#[test]
fn a_table_copy_is_independent_and_restores_in_place() {
    let mut ws = opened(region_bytes::<u32>(16));
    let mut live: Table<u32> = Table::empty();
    live.bind(&mut ws, 16, 0);
    live.as_mut_slice()[3] = 42;
    let snapshot = live.clone();
    assert_eq!(snapshot.owned_bytes(), 16 * size_of::<u32>());
    live.as_mut_slice()[3] = 0;
    assert_eq!(snapshot[3], 42);
    let before = live.as_slice().as_ptr();
    live.clone_from(&snapshot);
    assert_eq!(live[3], 42);
    assert_eq!(live.as_slice().as_ptr(), before);
    assert_eq!(live.owned_bytes(), 0);
}

// A buffer behaves like a Vec up to its capacity: push, extend, clear and a
// manual length all agree with the slice it exposes.
#[test]
fn a_buffer_tracks_its_length() {
    let mut ws = opened(region_bytes::<u32>(8));
    let mut buf = ws.buffer::<u32>(8);
    assert_eq!(buf.capacity(), 8);
    assert!(buf.is_empty());
    buf.push(1);
    buf.extend_from_slice(&[2, 3, 4]);
    assert_eq!(&buf[..], &[1, 2, 3, 4]);
    unsafe {
        buf.as_mut_ptr().add(4).write(5);
        buf.set_len(5);
    }
    assert_eq!(&buf[..], &[1, 2, 3, 4, 5]);
    buf.clear();
    assert!(buf.is_empty());
}

// Pushing past the capacity is a sizing bug and must stop the program at the
// write rather than run past the region.
#[test]
#[should_panic(expected = "workspace buffer sized below")]
fn a_full_buffer_refuses_a_push() {
    let mut ws = opened(region_bytes::<u8>(ALIGN));
    let mut buf = ws.buffer::<u8>(ALIGN);
    buf.extend_from_slice(&[0; ALIGN]);
    buf.push(1);
}

// The same for a slice that would overrun it.
#[test]
#[should_panic(expected = "workspace buffer sized below")]
fn a_buffer_refuses_a_slice_that_overruns_it() {
    let mut ws = opened(region_bytes::<u8>(4));
    let mut buf = ws.buffer::<u8>(4);
    buf.extend_from_slice(&[0; 5]);
}

// A layout larger than the workspace is refused at the carve, not discovered
// as an overlap later.
#[test]
#[should_panic(expected = "workspace sized below its layout")]
fn carving_past_the_workspace_is_refused() {
    let mut ws = opened(region_bytes::<u32>(4));
    let _ = ws.table::<u32>(4, 0);
    let _ = ws.buffer::<u8>(1);
}

// Nothing is carved from a layout that has not been opened: the opening is
// what reserves the trailing part, and carving first would overlap it.
#[test]
#[should_panic(expected = "not open")]
fn carving_before_the_opening_is_refused() {
    let mut ws = Workspace::new();
    ws.begin_layout(64, one_byte_per_block_byte);
    let _ = ws.buffer::<u8>(1);
}

// The trailing part a layout begins with is reserved by the opening on top of
// the leading part, and the allocation grows only to fit both.
#[test]
fn opening_reserves_the_trailing_part_and_grows_only_when_short() {
    let mut ws = Workspace::new();
    lay_out(&mut ws, 64, 128);
    assert_eq!(ws.capacity(), 192);
    let _ = ws.table::<u8>(64, 0);
    let _ = ws.buffer::<u8>(128);

    lay_out(&mut ws, 64, 64);
    assert_eq!(ws.capacity(), 192, "a smaller layout keeps the allocation");
    lay_out(&mut ws, 64, 200);
    assert_eq!(ws.capacity(), 264);
}

// The trailing part is sized for the block the frame's window allows, not the
// ceiling: a small frame must not reserve a full-size block's buffers.
#[test]
fn the_window_caps_the_block_the_trailing_part_is_sized_for() {
    let mut ws = Workspace::new();
    ws.begin_layout(128 * 1024, one_byte_per_block_byte);
    ws.open(0, 4096);
    assert_eq!(ws.block_capacity(), 4096);
    assert_eq!(ws.capacity(), 4096);
}

// Region sizes round up to the alignment and refuse to overflow.
#[test]
fn region_bytes_rounds_to_the_alignment() {
    assert_eq!(region_bytes::<u8>(0), 0);
    assert_eq!(region_bytes::<u8>(1), ALIGN);
    assert_eq!(region_bytes::<u32>(16), 64);
    assert_eq!(region_bytes::<u32>(17), 128);
}

#[test]
#[should_panic(expected = "overflows usize")]
fn region_bytes_refuses_an_overflowing_count() {
    let _ = region_bytes::<u64>(usize::MAX / 4);
}
