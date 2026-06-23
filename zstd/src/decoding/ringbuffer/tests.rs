use alloc::vec;

use super::{RingBuffer, copy_with_checks, copy_with_nobranch_check};
use crate::decoding::simd_copy;

fn assert_buffers_equal(expected: &RingBuffer, actual: &RingBuffer) {
    assert_eq!(expected.len(), actual.len());
    assert_eq!(expected.as_slices(), actual.as_slices());
    assert_eq!(expected.head, actual.head);
    assert_eq!(expected.tail, actual.tail);
    assert_eq!(expected.cap, actual.cap);
}

fn assert_branchless_matches_checked(
    mut checked: RingBuffer,
    mut branchless: RingBuffer,
    start: usize,
    len: usize,
) {
    assert!(checked.free() >= len);
    assert!(branchless.free() >= len);

    unsafe {
        checked.extend_from_within_unchecked(start, len);
        branchless.extend_from_within_unchecked_branchless(start, len);
    }

    assert_buffers_equal(&checked, &branchless);
}

#[test]
fn inline_exec_ok_respects_block_output_ceiling() {
    // The inline sequence-exec path bypasses `try_reserve`, so it must
    // itself honour the per-block output ceiling (`max_capacity`, armed
    // by `set_block_output_ceiling(MAX_BLOCK_SIZE)`). Otherwise a reused
    // large-window ring with physical slack could emit past the ceiling
    // — weakening the streaming-path decompression-bomb guard.
    use super::super::buffer_backend::BufferBackend;
    let mut rb = RingBuffer::new();
    rb.reserve(64 * 1024); // plenty of physical slack
    rb.extend(&[0u8; 1000]); // head = 0, tail = 1000
    // Per-block ceiling with only 100 bytes of output budget remaining.
    rb.set_max_capacity(1000 + 100);
    // lit+match = 500 exceeds the 100-byte budget but fits physically
    // (1531 < cap): the inline gate must reject it.
    assert!(
        !rb.inline_exec_ok(500, 0, 1),
        "inline_exec_ok must reject a write past the per-block output ceiling"
    );
    // A write within the budget stays eligible for the inline path.
    assert!(
        rb.inline_exec_ok(50, 0, 1),
        "inline_exec_ok must allow a write within the per-block ceiling"
    );
}

#[test]
fn inline_exec_ok_admits_contiguous_wrapped_sequence() {
    // After the ring wraps (`head > tail`), the inline path stays
    // eligible for a sequence whose linear write fits in the free gap
    // before `head` AND whose match source is the contiguous lower live
    // segment (`offset <= tail + lit`). A far-back match (source across the
    // wrap) or a write that would reach `head` must still be vetoed.
    use super::super::buffer_backend::BufferBackend;
    let mut rb = RingBuffer::new();
    rb.reserve(4096);
    let cap = rb.cap;
    // Force a wrapped layout: head well ahead of a small tail.
    rb.head = cap - 64;
    rb.tail = 32;
    // Free gap is [32, cap-64): write 16 lit + 16 match + 31 overshoot = 63
    // ends at 95, far below head, and offset 8 <= tail+lit = 48 -> eligible.
    assert!(
        rb.inline_exec_ok(16, 16, 8),
        "wrapped ring with contiguous write + in-segment source must stay inline-eligible"
    );
    // Match source crosses the wrap (offset 100 > tail+lit = 48) -> veto.
    assert!(
        !rb.inline_exec_ok(16, 16, 100),
        "wrapped ring with match source across the wrap must veto the inline path"
    );
    // Write + overshoot would reach `head` (huge match) -> veto.
    let huge_match = cap; // tail + lit + huge_match overflows past head
    assert!(
        !rb.inline_exec_ok(16, huge_match, 8),
        "wrapped ring whose write would reach the upper live segment must veto"
    );
}

#[test]
fn smoke() {
    let mut rb = RingBuffer::new();

    rb.reserve(15);
    assert_eq!(17, rb.cap);

    rb.extend(b"0123456789");
    assert_eq!(rb.len(), 10);
    assert_eq!(rb.as_slices().0, b"0123456789");
    assert_eq!(rb.as_slices().1, b"");

    rb.drop_first_n(5);
    assert_eq!(rb.len(), 5);
    assert_eq!(rb.as_slices().0, b"56789");
    assert_eq!(rb.as_slices().1, b"");

    rb.extend_from_within(2, 3);
    assert_eq!(rb.len(), 8);
    assert_eq!(rb.as_slices().0, b"56789789");
    assert_eq!(rb.as_slices().1, b"");

    rb.extend_from_within(0, 3);
    assert_eq!(rb.len(), 11);
    assert_eq!(rb.as_slices().0, b"56789789567");
    assert_eq!(rb.as_slices().1, b"");

    rb.extend_from_within(0, 2);
    assert_eq!(rb.len(), 13);
    assert_eq!(rb.as_slices().0, b"567897895675");
    assert_eq!(rb.as_slices().1, b"6");

    rb.drop_first_n(11);
    assert_eq!(rb.len(), 2);
    assert_eq!(rb.as_slices().0, b"5");
    assert_eq!(rb.as_slices().1, b"6");

    rb.extend(b"0123456789");
    assert_eq!(rb.len(), 12);
    assert_eq!(rb.as_slices().0, b"5");
    assert_eq!(rb.as_slices().1, b"60123456789");

    rb.drop_first_n(11);
    assert_eq!(rb.len(), 1);
    assert_eq!(rb.as_slices().0, b"9");
    assert_eq!(rb.as_slices().1, b"");

    rb.extend(b"0123456789");
    assert_eq!(rb.len(), 11);
    assert_eq!(rb.as_slices().0, b"9012345");
    assert_eq!(rb.as_slices().1, b"6789");
}

#[test]
fn edge_cases() {
    // Fill exactly, then empty then fill again
    let mut rb = RingBuffer::new();
    rb.reserve(16);
    assert_eq!(17, rb.cap);
    rb.extend(b"0123456789012345");
    assert_eq!(17, rb.cap);
    assert_eq!(16, rb.len());
    assert_eq!(0, rb.free());
    rb.drop_first_n(16);
    assert_eq!(0, rb.len());
    assert_eq!(16, rb.free());
    rb.extend(b"0123456789012345");
    assert_eq!(16, rb.len());
    assert_eq!(0, rb.free());
    assert_eq!(17, rb.cap);
    assert_eq!(1, rb.as_slices().0.len());
    assert_eq!(15, rb.as_slices().1.len());

    rb.clear();

    // data in both slices and then reserve
    rb.extend(b"0123456789012345");
    rb.drop_first_n(8);
    rb.extend(b"67890123");
    assert_eq!(16, rb.len());
    assert_eq!(0, rb.free());
    assert_eq!(17, rb.cap);
    assert_eq!(9, rb.as_slices().0.len());
    assert_eq!(7, rb.as_slices().1.len());
    rb.reserve(1);
    assert_eq!(16, rb.len());
    assert_eq!(16, rb.free());
    assert_eq!(33, rb.cap);
    assert_eq!(16, rb.as_slices().0.len());
    assert_eq!(0, rb.as_slices().1.len());

    rb.clear();

    // fill exactly, then extend from within
    rb.extend(b"0123456789012345");
    rb.extend_from_within(0, 16);
    assert_eq!(32, rb.len());
    assert_eq!(0, rb.free());
    assert_eq!(33, rb.cap);
    assert_eq!(32, rb.as_slices().0.len());
    assert_eq!(0, rb.as_slices().1.len());

    // extend from within cases
    let mut rb = RingBuffer::new();
    rb.reserve(8);
    rb.extend(b"01234567");
    rb.drop_first_n(5);
    rb.extend_from_within(0, 3);
    assert_eq!(4, rb.as_slices().0.len());
    assert_eq!(2, rb.as_slices().1.len());

    rb.drop_first_n(2);
    assert_eq!(2, rb.as_slices().0.len());
    assert_eq!(2, rb.as_slices().1.len());
    rb.extend_from_within(0, 4);
    assert_eq!(2, rb.as_slices().0.len());
    assert_eq!(6, rb.as_slices().1.len());

    rb.drop_first_n(2);
    assert_eq!(6, rb.as_slices().0.len());
    assert_eq!(0, rb.as_slices().1.len());
    rb.drop_first_n(2);
    assert_eq!(4, rb.as_slices().0.len());
    assert_eq!(0, rb.as_slices().1.len());
    rb.extend_from_within(0, 4);
    assert_eq!(7, rb.as_slices().0.len());
    assert_eq!(1, rb.as_slices().1.len());

    let mut rb = RingBuffer::new();
    rb.reserve(8);
    rb.extend(b"11111111");
    rb.drop_first_n(7);
    rb.extend(b"111");
    assert_eq!(2, rb.as_slices().0.len());
    assert_eq!(2, rb.as_slices().1.len());
    rb.extend_from_within(0, 4);
    assert_eq!(b"11", rb.as_slices().0);
    assert_eq!(b"111111", rb.as_slices().1);
}

#[test]
fn extend_from_within_branchless_matches_checked_across_layouts() {
    let contiguous = || {
        let mut rb = RingBuffer::new();
        rb.reserve(16);
        rb.extend(b"0123456789");
        rb
    };
    assert_branchless_matches_checked(contiguous(), contiguous(), 2, 5);

    let wrapped_write = || {
        let mut rb = RingBuffer::new();
        rb.reserve(16);
        rb.extend(b"0123456789ABC");
        rb.drop_first_n(2);
        rb
    };
    assert_branchless_matches_checked(wrapped_write(), wrapped_write(), 1, 5);

    let wrapped_data = || {
        let mut rb = RingBuffer::new();
        rb.reserve(32);
        rb.extend(b"0123456789abcdefghijklmn");
        rb.drop_first_n(18);
        rb.extend(b"wxyz012345");
        rb
    };
    assert_branchless_matches_checked(wrapped_data(), wrapped_data(), 8, 2);
    assert_branchless_matches_checked(wrapped_data(), wrapped_data(), 2, 8);
}

#[test]
fn copy_with_nobranch_check_matches_checked_for_all_valid_case_masks() {
    let cases = [
        (0, 0, 0, 0),
        (1, 0, 0, 0),
        (0, 1, 0, 0),
        (0, 0, 1, 0),
        (0, 0, 0, 1),
        (1, 1, 0, 0),
        (1, 0, 1, 0),
        (1, 0, 0, 1),
        (0, 1, 0, 1),
        (0, 0, 1, 1),
        (1, 1, 0, 1),
        (1, 0, 1, 1),
    ];

    for (m1_in_f1, m2_in_f1, m1_in_f2, m2_in_f2) in cases {
        let m1 = [11_u8, 12, 13, 14];
        let m2 = [21_u8, 22, 23, 24];
        let mut expected = [0_u8; 8];
        let mut actual = [0_u8; 8];

        unsafe {
            copy_with_checks(
                m1.as_ptr(),
                m2.as_ptr(),
                expected.as_mut_ptr(),
                expected.as_mut_ptr().add(4),
                m1_in_f1,
                m2_in_f1,
                m1_in_f2,
                m2_in_f2,
            );
            copy_with_nobranch_check(
                m1.as_ptr(),
                m2.as_ptr(),
                actual.as_mut_ptr(),
                actual.as_mut_ptr().add(4),
                m1_in_f1,
                m2_in_f1,
                m1_in_f2,
                m2_in_f2,
            );
        }

        assert_eq!(
            expected, actual,
            "case=({}, {}, {}, {})",
            m1_in_f1, m2_in_f1, m1_in_f2, m2_in_f2
        );
    }
}

#[test]
fn copy_bytes_overshooting_preserves_prefix_for_runtime_chunk_lengths() {
    // Validate correctness for lengths derived from the active runtime chunk:
    // - single chunk (`chunk`)
    // - multi chunk (`2 * chunk`)
    // - fallback shape (`chunk + 1`)
    // This checks copy semantics across runtime-selected strategies.
    let chunk = simd_copy::active_chunk_size_for_tests();
    let single_len = chunk;
    let multi_len = chunk * 2;
    let fallback_len = chunk + 1;
    let overshoot_cap = chunk * 2;
    let cap = multi_len + chunk;

    let src_single = vec![1_u8; cap];
    let mut dst_single = vec![0_u8; cap];
    unsafe {
        simd_copy::copy_bytes_overshooting(
            (src_single.as_ptr(), single_len),
            (dst_single.as_mut_ptr(), single_len),
            single_len,
        );
    }
    assert_eq!(&dst_single[..single_len], &src_single[..single_len]);

    let src_multi = vec![2_u8; cap];
    let mut dst_multi = vec![0_u8; cap];
    unsafe {
        simd_copy::copy_bytes_overshooting(
            (src_multi.as_ptr(), multi_len),
            (dst_multi.as_mut_ptr(), multi_len),
            multi_len,
        );
    }
    assert_eq!(&dst_multi[..multi_len], &src_multi[..multi_len]);

    let src_fallback = vec![3_u8; cap];
    let mut dst_fallback = vec![0_u8; cap];
    unsafe {
        simd_copy::copy_bytes_overshooting(
            (src_fallback.as_ptr(), fallback_len),
            (dst_fallback.as_mut_ptr(), fallback_len),
            fallback_len,
        );
    }
    assert_eq!(&dst_fallback[..fallback_len], &src_fallback[..fallback_len]);

    let src_overshoot = vec![4_u8; cap + 1];
    let mut dst_overshoot = vec![0_u8; cap + 1];
    unsafe {
        simd_copy::copy_bytes_overshooting(
            (src_overshoot.as_ptr().add(1), overshoot_cap),
            (dst_overshoot.as_mut_ptr().add(1), overshoot_cap),
            fallback_len,
        );
    }
    assert_eq!(
        &dst_overshoot[1..1 + fallback_len],
        &src_overshoot[1..1 + fallback_len]
    );
}

/// Helper: drive the ringbuffer into a wrapped layout where the two
/// free-slice halves straddle the physical end of the backing buffer.
/// Returns a buffer whose `len() == fill_len` of `pre_byte` data and
/// whose free region wraps.
fn build_wrapped_buffer(cap: usize, fill_len: usize, pre_byte: u8) -> RingBuffer {
    let mut rb = RingBuffer::new();
    rb.reserve(cap);
    let actual_cap = rb.cap;
    // Push to near-end of the physical buffer.
    let pre_len = actual_cap - 2;
    let prefix = alloc::vec![pre_byte; pre_len];
    rb.extend(&prefix);
    // Drop those bytes so head advances past them; tail now sits near
    // the end of `cap`, head is in the middle. Subsequent inserts will
    // wrap across the physical end.
    rb.drop_first_n(pre_len - fill_len);
    assert_eq!(rb.len(), fill_len);
    assert!(rb.tail > rb.head, "tail should still trail tape end");
    rb
}

#[test]
fn extend_and_fill_contiguous_layout() {
    let mut rb = RingBuffer::new();
    rb.extend_and_fill(0xAB, 7);
    assert_eq!(rb.len(), 7);
    let (s1, s2) = rb.as_slices();
    let mut combined = alloc::vec::Vec::with_capacity(7);
    combined.extend_from_slice(s1);
    combined.extend_from_slice(s2);
    assert_eq!(combined, alloc::vec![0xAB; 7]);
}

/// Construct a RingBuffer in the wrapped state where `tail < head`.
/// Data occupies `[head, cap)` followed by `[0, tail)`, leaving the
/// free region as `[tail, head)` — i.e. the **first** free slice
/// returned by `free_slice_parts` is the inner gap, NOT a span that
/// ends at `cap`.
fn build_tail_before_head(cap_hint: usize, head_pos: usize, tail_pos: usize) -> RingBuffer {
    assert!(tail_pos < head_pos);
    let mut rb = RingBuffer::new();
    rb.reserve(cap_hint);
    let actual_cap = rb.cap;
    // Fill almost the whole buffer so we can carve out tail < head.
    let fill_len = actual_cap - 2;
    let prefix = alloc::vec![0xCD; fill_len];
    rb.extend(&prefix);
    // Drop bytes so `head` lands at the target. Tail stays at fill_len.
    rb.drop_first_n(head_pos);
    // Now extend just enough to wrap tail past `cap` and land it at
    // `tail_pos`. From the current state (head=head_pos, tail=fill_len),
    // we need `tail_pos = (fill_len + extra) % cap`, so
    // `extra = (tail_pos + cap - fill_len) % cap`.
    let extra = (tail_pos + actual_cap - fill_len) % actual_cap;
    rb.extend(&alloc::vec![0xCD; extra]);
    assert_eq!(rb.head, head_pos);
    assert_eq!(rb.tail, tail_pos);
    assert!(
        rb.tail < rb.head,
        "expected wrapped layout: tail={} head={}",
        rb.tail,
        rb.head
    );
    rb
}

#[test]
fn extend_wrapped_layout_preserves_bytes_past_head() {
    // Regression test for the WILDCOPY_OVERLENGTH inflation bug.
    // When `tail < head` the first free slice returned by
    // `free_slice_parts` is the inner `[tail, head)` gap — it does NOT
    // end at `cap`. A previous version of `RingBuffer::extend` always
    // added WILDCOPY_OVERLENGTH to the destination capacity passed to
    // `simd_copy::copy_bytes_overshooting`, which on this layout would
    // allow wildcopy overshoot writes to clobber bytes at `[head,
    // head+16)` — still-readable data the caller has not consumed yet.
    // The current code gates the inflation on `tail >= head`; this
    // test asserts that bytes at `head..head+16` survive an extend()
    // that fills the inner gap nearly to capacity.
    let mut rb = build_tail_before_head(128, 80, 10);
    let head_before = rb.head;
    let cap = rb.cap;
    // Sample a window of bytes immediately past `head` and confirm
    // they are the prefill (0xCD); these are the bytes any erroneous
    // wildcopy overshoot would corrupt.
    let mut sentinel = [0u8; 16];
    unsafe {
        for (i, slot) in sentinel.iter_mut().enumerate() {
            *slot = rb.buf.as_ptr().add((head_before + i) % cap).read();
        }
    }
    assert!(sentinel.iter().all(|&b| b == 0xCD), "pre-state sentinel");

    // Fill the inner free region with a recognisable pattern, leaving
    // exactly the 1-byte sentinel gap the RingBuffer always reserves.
    let free_before = rb.free();
    let payload = alloc::vec![0x42; free_before];
    rb.extend(&payload);

    // The bytes at [head, head+16) must still be the original 0xCD
    // prefill — any overshoot would have written 0x42 over them.
    unsafe {
        for i in 0..16usize {
            let actual = rb.buf.as_ptr().add((head_before + i) % cap).read();
            assert_eq!(
                actual,
                0xCD,
                "byte at head+{i} (raw idx {}) was clobbered: got {:#04x}",
                (head_before + i) % cap,
                actual
            );
        }
    }
}

#[test]
fn extend_and_fill_wrapped_layout() {
    // Pre-fill so the free region straddles the wrap boundary, then
    // verify both halves are written with the fill byte.
    let mut rb = build_wrapped_buffer(16, 2, 0x11);
    let extra = rb.cap - 2; // fills exactly to capacity, forcing a wrap
    rb.extend_and_fill(0x22, extra);
    assert_eq!(rb.len(), 2 + extra);
    let (s1, s2) = rb.as_slices();
    let mut combined = alloc::vec::Vec::with_capacity(rb.len());
    combined.extend_from_slice(s1);
    combined.extend_from_slice(s2);
    let mut expected = alloc::vec![0x11; 2];
    expected.extend(alloc::vec![0x22; extra]);
    assert_eq!(combined, expected);
}

#[test]
fn extend_from_reader_contiguous_layout() {
    let mut rb = RingBuffer::new();
    let src: [u8; 6] = [1, 2, 3, 4, 5, 6];
    rb.extend_from_reader(&src[..], 6).unwrap();
    assert_eq!(rb.len(), 6);
    let (s1, s2) = rb.as_slices();
    let mut combined = alloc::vec::Vec::with_capacity(6);
    combined.extend_from_slice(s1);
    combined.extend_from_slice(s2);
    assert_eq!(combined, src);
}

#[test]
fn extend_from_reader_wrapped_layout() {
    let mut rb = build_wrapped_buffer(16, 3, 0xAA);
    let extra = rb.cap - 3;
    let src: alloc::vec::Vec<u8> = (0..extra as u8).collect();
    rb.extend_from_reader(src.as_slice(), extra).unwrap();
    assert_eq!(rb.len(), 3 + extra);
    let (s1, s2) = rb.as_slices();
    let mut combined = alloc::vec::Vec::with_capacity(rb.len());
    combined.extend_from_slice(s1);
    combined.extend_from_slice(s2);
    let mut expected = alloc::vec![0xAA; 3];
    expected.extend_from_slice(&src);
    assert_eq!(combined, expected);
}

#[test]
fn extend_from_reader_eof_leaves_state_unchanged() {
    let mut rb = RingBuffer::new();
    rb.extend(b"prefix");
    let snapshot_len = rb.len();
    let snapshot_slices: (alloc::vec::Vec<u8>, alloc::vec::Vec<u8>) = {
        let (a, b) = rb.as_slices();
        (a.to_vec(), b.to_vec())
    };

    // Reader yields only 2 bytes but we ask for 10 → `read_exact` fails
    // on the second chunk, and `tail` must not advance.
    let short: [u8; 2] = [0xCC, 0xDD];
    let err = rb.extend_from_reader(&short[..], 10);
    assert!(err.is_err(), "short reader must propagate IO error");
    assert_eq!(rb.len(), snapshot_len, "len() must be unchanged on error");
    let (a, b) = rb.as_slices();
    assert_eq!(a, snapshot_slices.0.as_slice());
    assert_eq!(b, snapshot_slices.1.as_slice());
}
