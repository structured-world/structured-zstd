use super::*;

// Every region begins on the workspace alignment, from either end.
#[test]
fn regions_start_aligned_from_both_ends() {
    let mut ws = Workspace::new();
    ws.ensure(region_bytes::<u32>(10) + region_bytes::<u8>(3) + region_bytes::<u16>(7));
    let mut carver = ws.carver();
    let table = carver.table::<u32>(10, 0);
    let mut bytes = carver.buffer::<u8>(3);
    let mut halves = carver.buffer::<u16>(7);
    assert_eq!(table.as_ptr() as usize % ALIGN, 0);
    assert_eq!(bytes.as_mut_ptr() as usize % ALIGN, 0);
    assert_eq!(halves.as_mut_ptr() as usize % ALIGN, 0);
    assert_eq!(carver.remaining(), 0);
}

// Regions of one layout never share a byte: writing one leaves the others
// exactly as they were.
#[test]
fn regions_of_one_layout_are_disjoint() {
    let mut ws = Workspace::new();
    ws.ensure(2 * region_bytes::<u32>(20) + region_bytes::<u8>(100));
    let mut carver = ws.carver();
    let mut first = carver.table::<u32>(20, 7);
    let second = carver.table::<u32>(20, 9);
    let mut tail = carver.buffer::<u8>(100);
    first.as_mut_slice().fill(u32::MAX);
    tail.extend_from_slice(&[0xAB; 100]);
    assert!(second.as_slice().iter().all(|&v| v == 9));
    assert!(first.as_slice().iter().all(|&v| v == u32::MAX));
    assert!(tail.iter().all(|&b| b == 0xAB));
}

// A table starts with every value set, so reading it before any write is
// defined and sees the fill value.
#[test]
fn a_table_starts_filled() {
    let mut ws = Workspace::new();
    ws.ensure(region_bytes::<u64>(33));
    let mut carver = ws.carver();
    let table = carver.table::<u64>(33, 0x1234);
    assert_eq!(table.len(), 33);
    assert!(table.as_slice().iter().all(|&v| v == 0x1234));
}

// A buffer behaves like a Vec up to its capacity: push, extend, truncate,
// clear and a manual length all agree with the slice it exposes.
#[test]
fn a_buffer_tracks_its_length() {
    let mut ws = Workspace::new();
    ws.ensure(region_bytes::<u32>(8));
    let mut carver = ws.carver();
    let mut buf = carver.buffer::<u32>(8);
    assert_eq!(buf.capacity(), 8);
    assert!(buf.is_empty());
    buf.push(1);
    buf.extend_from_slice(&[2, 3, 4]);
    assert_eq!(&buf[..], &[1, 2, 3, 4]);
    buf.truncate(2);
    assert_eq!(&buf[..], &[1, 2]);
    unsafe {
        buf.as_mut_ptr().add(2).write(5);
        buf.set_len(3);
    }
    assert_eq!(&buf[..], &[1, 2, 5]);
    buf.clear();
    assert!(buf.is_empty());
}

// Pushing past the capacity is a sizing bug and must stop the program at the
// write rather than run past the region.
#[test]
#[should_panic(expected = "workspace buffer sized below")]
fn a_full_buffer_refuses_a_push() {
    let mut ws = Workspace::new();
    ws.ensure(region_bytes::<u8>(ALIGN));
    let mut carver = ws.carver();
    let mut buf = carver.buffer::<u8>(ALIGN);
    buf.extend_from_slice(&[0; ALIGN]);
    buf.push(1);
}

// The same for a slice that would overrun it.
#[test]
#[should_panic(expected = "workspace buffer sized below")]
fn a_buffer_refuses_a_slice_that_overruns_it() {
    let mut ws = Workspace::new();
    ws.ensure(region_bytes::<u8>(4));
    let mut carver = ws.carver();
    let mut buf = carver.buffer::<u8>(4);
    buf.extend_from_slice(&[0; 5]);
}

// A layout larger than the workspace is refused at the carve, not discovered
// as an overlap later.
#[test]
#[should_panic(expected = "workspace sized below its layout")]
fn carving_past_the_workspace_is_refused() {
    let mut ws = Workspace::new();
    ws.ensure(region_bytes::<u32>(4));
    let mut carver = ws.carver();
    let _ = carver.table::<u32>(4, 0);
    let _ = carver.buffer::<u8>(1);
}

// The workspace grows only when asked for more than it holds, and keeps its
// allocation otherwise: that is what keeps a context's pages its own.
#[test]
fn ensure_reallocates_only_to_grow() {
    let mut ws = Workspace::new();
    assert!(ws.ensure(1000));
    let first = ws.carver().table::<u8>(1, 0).as_ptr();
    assert!(!ws.ensure(1000));
    assert!(!ws.ensure(10));
    assert_eq!(ws.carver().table::<u8>(1, 0).as_ptr(), first);
    assert!(ws.ensure(1001));
    assert_eq!(ws.capacity(), 1001);
}

// An empty workspace allocates nothing and carves only empty regions.
#[test]
fn an_empty_workspace_carves_empty_regions() {
    let mut ws = Workspace::new();
    assert_eq!(ws.capacity(), 0);
    let mut carver = ws.carver();
    let table = carver.table::<u32>(0, 0);
    let buf = carver.buffer::<u8>(0);
    assert_eq!(table.len(), 0);
    assert_eq!(buf.capacity(), 0);
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
