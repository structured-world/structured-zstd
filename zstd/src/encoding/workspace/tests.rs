use super::*;

/// A trailing part as large as the block it is sized for.
fn one_byte_per_block_byte(block: usize) -> usize {
    block
}

/// Lays `ws` out anew with `leading` bytes for tables and `trailing` for the
/// context's buffers, the window leaving the block ceiling as it is.
fn lay_out(ws: &mut Workspace, leading: usize, trailing: usize) {
    if trailing == 0 {
        ws.begin_layout(0, no_trailing, IngestPlan::Stream);
    } else {
        ws.begin_layout(trailing, one_byte_per_block_byte, IngestPlan::Stream);
    }
    ws.open(leading, usize::MAX);
}

/// An open layout of exactly `bytes`, all of them leading.
fn opened(bytes: usize) -> Workspace {
    opened_with(bytes, 0)
}

/// An open layout of `leading` bytes for tables and `trailing` for buffers.
fn opened_with(leading: usize, trailing: usize) -> Workspace {
    let mut ws = Workspace::new();
    lay_out(&mut ws, leading, trailing);
    ws
}

// Every region begins on the workspace alignment, from either end.
#[test]
fn regions_start_aligned_from_both_ends() {
    let mut ws = opened_with(
        region_bytes::<u32>(10),
        region_bytes::<u8>(3) + region_bytes::<u16>(7),
    );
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
    let mut ws = opened_with(2 * region_bytes::<u32>(20), region_bytes::<u8>(100));
    let mut first = ws.table::<u32>(20, 7);
    let second = ws.table::<u32>(20, 9);
    let mut tail = ws.buffer::<u8>(100);
    first.as_mut_slice().fill(u32::MAX);
    for _ in 0..100 {
        tail.push(0xAB);
    }
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

// A table of zeros is left unwritten only in the layout that made the (zeroed)
// allocation. A later layout carving it over bytes another holder wrote still
// empties it: the zeros it relies on are gone by then.
#[test]
fn a_zero_table_over_written_bytes_still_starts_empty() {
    let sparse = |ws: &mut Workspace| {
        ws.begin_layout(0, no_trailing, IngestPlan::Stream);
        let table = region_bytes::<u32>(64);
        ws.open_for_match_finder(table, table, usize::MAX, 0);
    };
    let mut ws = Workspace::new();
    sparse(&mut ws);
    let fresh = ws.table::<u32>(64, 0);
    assert!(fresh.as_slice().iter().all(|&v| v == 0));
    let mut other: Table<u32> = Table::empty();
    lay_out(&mut ws, region_bytes::<u32>(64), 0);
    other.bind(&mut ws, 64, 7);
    assert!(other.iter().all(|&v| v == 7));
    sparse(&mut ws);
    let later = ws.table::<u32>(64, 0);
    assert!(later.as_slice().iter().all(|&v| v == 0));
}

// A zeroed allocation is worth taking only when the zero tables are most of it.
// An allocator that serves it from memory it has handed out before zeroes the
// whole of it, the buffers behind the tables included, so a small frame whose
// per-block buffers outweigh its tables would pay several times the fill it
// saves on every fresh context.
#[test]
fn only_tables_that_fill_the_workspace_take_it_zeroed() {
    fn buffers_thrice_the_table(_block: usize) -> usize {
        3 * region_bytes::<u32>(64)
    }
    let table = region_bytes::<u32>(64);

    let mut ws = Workspace::new();
    ws.begin_layout(
        1 << 17,
        buffers_thrice_the_table,
        IngestPlan::Slice(1 << 17),
    );
    ws.open_for_match_finder(table, table, 1 << 14, 0);
    assert!(
        !ws.zeroed,
        "tables a quarter of the workspace must be filled, not the workspace zeroed",
    );

    // Just over half, the share a 10 KiB frame's tables have at level 1:
    // zeroing the rest still costs more than the fill saves.
    fn buffers_four_fifths_of_the_table(_block: usize) -> usize {
        4 * region_bytes::<u32>(64) / 5
    }
    let mut ws = Workspace::new();
    ws.begin_layout(
        1 << 17,
        buffers_four_fifths_of_the_table,
        IngestPlan::Slice(1 << 17),
    );
    ws.open_for_match_finder(table, table, 1 << 14, 0);
    assert!(!ws.zeroed, "tables just over half the workspace are filled");

    let mut ws = Workspace::new();
    ws.begin_layout(1 << 17, no_trailing, IngestPlan::Slice(1 << 17));
    ws.open_for_match_finder(table, table, 1 << 14, 0);
    assert!(
        ws.zeroed,
        "tables that are the whole workspace take it zeroed"
    );
    let fresh = ws.table::<u32>(64, 0);
    assert!(fresh.as_slice().iter().all(|&v| v == 0));
}

// Heap accounting reports what was allocated, not what can be carved: the
// allocation carries up to ALIGN - 1 bytes of padding ahead of the aligned start.
#[test]
fn heap_bytes_count_the_alignment_padding() {
    let ws = opened(region_bytes::<u32>(64));
    assert_eq!(ws.heap_bytes(), ws.capacity() + ALIGN - 1);
    assert_eq!(Workspace::new().heap_bytes(), 0);
}

// From the size the system allocator always serves with fresh pages, the
// workspace is taken zeroed however small its tables: zeroing costs nothing
// there, and filling the tables would fault in pages the frame never indexes.
#[test]
fn a_workspace_on_fresh_pages_is_taken_zeroed() {
    fn buffers_up_to_fresh_pages(_block: usize) -> usize {
        FRESH_PAGES_FROM
    }
    let table = region_bytes::<u32>(64);
    let mut ws = Workspace::new();
    ws.begin_layout(
        1 << 17,
        buffers_up_to_fresh_pages,
        IngestPlan::Slice(1 << 17),
    );
    // The tables are not even sparse: the input is expected to fill them.
    ws.open_for_match_finder(table, table, 1 << 14, usize::MAX);
    assert!(ws.zeroed, "a workspace past the fresh-page size is zeroed");
    assert!(ws.on_fresh_pages());
    let zeros = ws.table::<u32>(64, 0);
    assert!(zeros.as_slice().iter().all(|&v| v == 0));
    // The next frame lays out on the same allocation: its pages are warm.
    ws.begin_layout(
        1 << 17,
        buffers_up_to_fresh_pages,
        IngestPlan::Slice(1 << 17),
    );
    ws.open_for_match_finder(table, table, 1 << 14, usize::MAX);
    assert!(!ws.on_fresh_pages());
}

// A region whose byte size fits but whose alignment padding does not is refused
// loudly, as the multiplication overflow is, rather than wrapping to a small size.
#[test]
#[should_panic(expected = "workspace region size overflows usize")]
fn a_region_that_overflows_with_its_padding_panics() {
    let _ = region_bytes::<u8>(usize::MAX);
}

// A context that lays out nothing for longer than the give-back limit returns
// its whole allocation, as one left three times too large does.
#[test]
fn a_workspace_left_unused_is_given_back() {
    let mut ws = opened(region_bytes::<u32>(1024));
    for _ in 0..TOO_LARGE_MAX_LAYOUTS {
        lay_out(&mut ws, 0, 0);
        assert!(ws.heap_bytes() > 0, "kept while under the limit");
    }
    lay_out(&mut ws, 0, 0);
    assert_eq!(ws.capacity(), 0, "replaced by nothing past the limit");
    // The replaced allocation is kept until the next layout, for a history to
    // carry its bytes out of, and freed there.
    lay_out(&mut ws, 0, 0);
    assert_eq!(ws.heap_bytes(), 0, "given back past the limit");
}

// Restoring a table from one of another length replaces it with an owned copy;
// only a same-length restore can copy in place.
#[test]
fn a_table_restored_from_another_length_takes_a_copy() {
    let mut ws = opened(region_bytes::<u32>(64));
    let mut table: Table<u32> = Table::empty();
    table.bind(&mut ws, 64, 0);
    let source = Table::owned(alloc::vec![7u32; 16]);
    table.clone_from(&source);
    assert_eq!(table.len(), 16);
    assert!(table.iter().all(|&v| v == 7));
    assert_eq!(table.owned_bytes(), 16 * core::mem::size_of::<u32>());
}

// The public history buffer reports emptiness and formats its length and
// room, not its bytes, which may be a whole window of input.
#[test]
fn a_history_buffer_reports_its_length_and_room() {
    let mut history = HistoryBuf::new();
    assert!(history.is_empty());
    history.extend_from_slice(b"abc");
    assert!(!history.is_empty());
    let shown = alloc::format!("{history:?}");
    assert!(shown.starts_with("HistoryBuf"));
    assert!(shown.contains("len: 3"));
    assert!(
        !shown.contains("97"),
        "the bytes themselves stay out: {shown}"
    );
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

// Every new allocation starts a new generation: an allocator may hand the same
// address back, and only the generation then tells a holder that the bytes
// under its old pointer are not its table any more. Giving an oversized
// allocation back is a new allocation too.
#[test]
fn a_new_allocation_starts_a_new_generation() {
    let mut ws = opened(region_bytes::<u32>(16));
    let generation = ws.generation;
    lay_out(&mut ws, region_bytes::<u32>(1024), 0);
    assert_ne!(ws.generation, generation, "a grown allocation");
    let generation = ws.generation;
    for _ in 0..=TOO_LARGE_MAX_LAYOUTS {
        lay_out(&mut ws, region_bytes::<u32>(16), 0);
    }
    assert_ne!(ws.generation, generation, "an allocation given back");
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

// A buffer behaves like a Vec up to its capacity: push, clear and a manual
// length all agree with the slice it exposes.
#[test]
fn a_buffer_tracks_its_length() {
    let mut ws = opened_with(0, region_bytes::<u32>(8));
    let mut buf = ws.buffer::<u32>(8);
    assert_eq!(buf.capacity(), 8);
    assert!(buf.is_empty());
    for v in [1, 2, 3, 4] {
        buf.push(v);
    }
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
    let mut ws = opened_with(0, region_bytes::<u8>(ALIGN));
    let mut buf = ws.buffer::<u8>(ALIGN);
    for _ in 0..ALIGN {
        buf.push(0);
    }
    buf.push(1);
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
    ws.begin_layout(64, one_byte_per_block_byte, IngestPlan::Stream);
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
    ws.begin_layout(128 * 1024, one_byte_per_block_byte, IngestPlan::Stream);
    ws.open(0, 4096);
    assert_eq!(ws.block_capacity(), 4096);
    assert_eq!(ws.capacity(), 4096);
}

// A context that ran one large frame and then only small ones must not hold the
// large allocation forever: after the limit it is allocated again at what the
// frames need, as upstream gives back a workspace left three times too large.
#[test]
fn a_workspace_far_larger_than_its_frames_is_given_back_after_the_limit() {
    let mut ws = Workspace::new();
    lay_out(&mut ws, 4096, 0);
    for _ in 0..TOO_LARGE_MAX_LAYOUTS {
        lay_out(&mut ws, 1024, 0);
        assert_eq!(ws.capacity(), 4096, "given back before the limit");
    }
    lay_out(&mut ws, 1024, 0);
    assert_eq!(ws.capacity(), 1024);
}

// Only a run of layouts that all found the workspace too large gives it back,
// as upstream counts the oversized duration rather than the allocation's age:
// a context that meets one small frame among many large ones keeps its
// allocation for the next large frame.
#[test]
fn one_small_frame_after_many_large_ones_keeps_the_workspace() {
    let mut ws = Workspace::new();
    for _ in 0..(2 * TOO_LARGE_MAX_LAYOUTS) {
        lay_out(&mut ws, 4096, 0);
    }
    lay_out(&mut ws, 1024, 0);
    assert_eq!(ws.capacity(), 4096);
}

// A workspace that is larger than the frames need, but by less than the factor,
// is kept however long it is used: reallocating it would only move the pages.
#[test]
fn a_workspace_moderately_larger_than_its_frames_is_kept() {
    let mut ws = Workspace::new();
    lay_out(&mut ws, 4096, 0);
    for _ in 0..(2 * TOO_LARGE_MAX_LAYOUTS) {
        lay_out(&mut ws, 1500, 0);
    }
    assert_eq!(ws.capacity(), 4096);
}

/// Binds `history` at `capacity` and then a `table_len` table, in a fresh
/// layout sized for exactly the two: the order a match finder lays itself out
/// in.
fn lay_out_history_and_table(
    ws: &mut Workspace,
    history: &mut HistoryBuf,
    capacity: usize,
    table: &mut Table<u32>,
    table_len: usize,
) {
    lay_out(
        ws,
        region_bytes::<u32>(table_len) + history.workspace_bytes(capacity),
        0,
    );
    history.bind(ws, capacity);
    table.bind(ws, table_len, 0xFFFF_FFFF);
}

// A history laid out again keeps what it held, in place, however small the
// next frame's need: that is how a dictionary left at its head survives to
// the next frame. It sits after the tables, never under them.
#[test]
fn a_history_laid_out_again_keeps_its_bytes() {
    let mut ws = Workspace::new();
    let mut table: Table<u32> = Table::empty();
    let mut history = HistoryBuf::new();
    lay_out_history_and_table(&mut ws, &mut history, 256, &mut table, 16);
    history.extend_from_slice(b"dictionary bytes");
    let before = history.as_ptr();
    assert_eq!(
        before as usize,
        table.as_slice().as_ptr_range().end as usize,
        "the history follows the table"
    );

    lay_out_history_and_table(&mut ws, &mut history, 8, &mut table, 16);
    assert_eq!(&history[..], b"dictionary bytes");
    assert_eq!(history.as_ptr(), before, "kept in place");
    assert!(history.capacity() >= history.len());
}

// Tables that change size move the history within the same allocation; its
// bytes move with it, ahead of the tables that are then filled over where they
// used to be.
#[test]
fn a_history_moved_by_its_tables_keeps_its_bytes() {
    let mut ws = Workspace::new();
    let mut table: Table<u32> = Table::empty();
    let mut history = HistoryBuf::new();
    lay_out_history_and_table(&mut ws, &mut history, 2048, &mut table, 16);
    history.extend_from_slice(&[0x5A; 1000]);
    let capacity = ws.capacity();

    lay_out_history_and_table(&mut ws, &mut history, 64, &mut table, 64);
    assert_eq!(ws.capacity(), capacity, "same allocation");
    assert!(history.iter().all(|&b| b == 0x5A));
    assert_eq!(history.len(), 1000);
}

// A history laid out in a new allocation carries its bytes out of the one the
// workspace replaced, which lives until then.
#[test]
fn a_history_in_a_new_allocation_keeps_its_bytes() {
    let mut ws = Workspace::new();
    let mut table: Table<u32> = Table::empty();
    let mut history = HistoryBuf::new();
    lay_out_history_and_table(&mut ws, &mut history, 64, &mut table, 16);
    history.extend_from_slice(b"kept across a reallocation");
    let generation = ws.generation;

    lay_out_history_and_table(&mut ws, &mut history, 4096, &mut table, 1024);
    assert_ne!(ws.generation, generation, "fixture: the workspace grew");
    assert_eq!(&history[..], b"kept across a reallocation");
    assert!(
        ws.retired.is_none(),
        "the old allocation is freed once bound"
    );
}

// A matcher leaving its context takes its history and tables into rooms of
// their own, so they stay readable after the context's workspace is gone.
#[test]
fn a_history_and_table_leaving_the_workspace_outlive_it() {
    let mut ws = Workspace::new();
    let mut table: Table<u32> = Table::empty();
    let mut history = HistoryBuf::new();
    lay_out_history_and_table(&mut ws, &mut history, 64, &mut table, 16);
    history.extend_from_slice(b"still here");
    table.as_mut_slice()[3] = 7;

    history.leave_workspace();
    table.leave_workspace();
    drop(ws);
    assert_eq!(&history[..], b"still here");
    assert_eq!(table[3], 7);
    assert!(history.owned_bytes() >= history.len());
    assert_eq!(table.owned_bytes(), 16 * size_of::<u32>());
}

// A frame that brings more than its history was laid out for (a size hint that
// undercounted) moves the bytes into an allocation of the history's own, keeps
// them, and brings them back into the workspace at the next layout.
#[test]
fn a_history_outgrowing_its_room_keeps_its_bytes() {
    let mut ws = opened(region_bytes::<u8>(8));
    let mut history = HistoryBuf::new();
    history.bind(&mut ws, 8);
    history.extend_from_slice(b"12345678");
    assert_eq!(history.owned_bytes(), 0);
    history.extend_from_slice(b"9");
    assert!(history.owned_bytes() >= 9, "moved out of the workspace");
    assert_eq!(&history[..], b"123456789");

    lay_out(&mut ws, region_bytes::<u8>(64), 0);
    history.bind(&mut ws, 64);
    assert_eq!(history.owned_bytes(), 0, "back in the workspace");
    assert_eq!(&history[..], b"123456789");
}

// Dropping the front moves the rest down, as `Vec::drain(..n)` does; resize and
// truncate move the length, and a restore copies into the room it already has.
#[test]
fn a_history_behaves_like_the_vec_it_replaces() {
    let mut ws = opened(region_bytes::<u8>(64));
    let mut history = HistoryBuf::new();
    history.bind(&mut ws, 64);
    history.extend_from_slice(b"abcdef");
    history.drain_front(2);
    assert_eq!(&history[..], b"cdef");
    history.resize(6, b'z');
    assert_eq!(&history[..], b"cdefzz");
    history.truncate(3);
    assert_eq!(&history[..], b"cde");
    history.push(b'!');
    assert_eq!(&history[..], b"cde!");

    let snapshot = history.clone();
    history.clear();
    history.clone_from(&snapshot);
    assert_eq!(&history[..], b"cde!");
    assert_eq!(history.owned_bytes(), 0, "restored in place");
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
