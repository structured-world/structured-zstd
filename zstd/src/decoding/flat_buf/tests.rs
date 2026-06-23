use super::*;

#[test]
fn with_capacity_starts_empty() {
    let f = FlatBuf::with_capacity(1024);
    assert_eq!(f.len(), 0);
    assert_eq!(f.tail(), 0);
    assert!(f.cap() >= 1024 + WILDCOPY_OVERLENGTH);
}

#[test]
fn extend_appends_then_len_matches() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[1, 2, 3, 4]);
    assert_eq!(f.len(), 4);
    f.extend(&[5, 6]);
    assert_eq!(f.len(), 6);
    let (s1, s2) = f.as_slices();
    assert_eq!(s1, &[1, 2, 3, 4, 5, 6]);
    assert!(s2.is_empty(), "flat layout never wraps");
}

#[test]
fn extend_and_fill_appends_repeated_byte() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[0xAA]);
    f.extend_and_fill(0xBB, 5);
    let (s1, _) = f.as_slices();
    assert_eq!(s1, &[0xAA, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB]);
}

#[test]
fn extend_from_within_unchecked_copies_non_overlapping() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[10, 20, 30, 40, 50]);
    // SAFETY: start+len=3 <= len()=5; capacity covers 5+3.
    unsafe { f.extend_from_within_unchecked(0, 3) };
    let (s1, _) = f.as_slices();
    assert_eq!(s1, &[10, 20, 30, 40, 50, 10, 20, 30]);
}

#[test]
fn drop_first_n_advances_head() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[1, 2, 3, 4, 5]);
    f.drop_first_n(2);
    assert_eq!(f.len(), 3);
    let (s1, _) = f.as_slices();
    assert_eq!(s1, &[3, 4, 5]);
    // Drained bytes remain physically present and back match copies.
    // After head=2, logical start=0 maps to physical index 2.
    // SAFETY: start+len=3 <= len()=3.
    unsafe { f.extend_from_within_unchecked(0, 3) };
    let (s1, _) = f.as_slices();
    assert_eq!(s1, &[3, 4, 5, 3, 4, 5]);
}

#[test]
fn set_tail_rolls_back() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[1, 2, 3]);
    let saved_tail = f.tail();
    let saved_cap = f.cap();
    f.extend(&[4, 5, 6, 7]);
    assert_eq!(f.len(), 7);
    assert_eq!(f.cap(), saved_cap, "with_capacity sized to avoid realloc");
    // SAFETY: cap unchanged; new_tail came from prior tail() call.
    unsafe { f.set_tail(saved_tail) };
    assert_eq!(f.len(), 3);
    let (s1, _) = f.as_slices();
    assert_eq!(s1, &[1, 2, 3]);
}

#[test]
fn clear_resets() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[1, 2, 3]);
    f.drop_first_n(1);
    assert_eq!(f.len(), 2);
    f.clear();
    assert_eq!(f.len(), 0);
    assert_eq!(f.tail(), 0);
}

/// Inline executor — verify match-copy correctness against a
/// byte-by-byte reference. Exercises the non-overlap path
/// (offset >= 16), short-offset overlapCopy8 path (offset < 16),
/// and the literal copy16 + wildcopy tail. Runs on every target: on
/// x86_64 it drives the SSE2 `exec_sequence_inline` arm, elsewhere
/// the portable arm (both `cfg`-selected), giving the non-x86
/// backend method direct coverage.
#[test]
fn exec_sequence_inline_match_copy_correctness() {
    for offset in [4usize, 8, 12, 20, 48, 96] {
        let mut f = FlatBuf::with_capacity(512);
        // Seed bytes 0..256 with deterministic pattern.
        let seed: Vec<u8> = (0..256u32).map(|i| ((i * 31 + 7) & 0xFF) as u8).collect();
        f.extend(&seed);
        let base = f.len();
        let match_length = 96usize;
        // Reference: byte-by-byte repeat starting at base, sourced from base-offset.
        let mut reference = alloc::vec![0u8; base + match_length];
        reference[..base].copy_from_slice(&seed);
        for i in 0..match_length {
            reference[base + i] = reference[base + i - offset];
        }

        let lits = [0xAAu8; 16];
        // SAFETY: lit_length = 0 so lit_src is unused beyond a 16-byte
        // over-read into the literal scratch (in-bounds).
        unsafe {
            f.exec_sequence_inline(lits.as_ptr(), 0, offset, match_length)
                .unwrap();
        }
        assert_eq!(f.len(), base + match_length, "offset={offset}");
        let (s1, _) = f.as_slices();
        for i in 0..match_length {
            assert_eq!(
                s1[base + i],
                reference[base + i],
                "offset={offset} byte {i}: got {:#x}, expected {:#x}",
                s1[base + i],
                reference[base + i],
            );
        }
    }
}

/// AVX2 inline executor — verify match-copy correctness for
/// offsets across the SSE2/AVX2 threshold boundary
/// (offset 20 routes to SSE2 16-byte path, offset 32 to AVX2
/// 32-byte ymm path, offset 64 to deep AVX2 path).
// AVX2 override is x86_64-only; this test calls it directly. The `std`
// feature gate is required: `is_x86_feature_detected!` is `std`-only,
// unavailable in the crate's `#![no_std]` build.
#[cfg(all(target_arch = "x86_64", feature = "std"))]
#[test]
fn exec_sequence_inline_avx2_offset_boundary_correctness() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    for offset in [20usize, 32, 64] {
        let mut f = FlatBuf::with_capacity(512);
        let seed: Vec<u8> = (0..256u32).map(|i| ((i * 31 + 7) & 0xFF) as u8).collect();
        f.extend(&seed);
        let base = f.len();
        let match_length = 96usize;
        let mut reference = alloc::vec![0u8; base + match_length];
        reference[..base].copy_from_slice(&seed);
        for i in 0..match_length {
            reference[base + i] = reference[base + i - offset];
        }

        let lits = [0xAAu8; 16];
        // SAFETY: AVX2 detected via runtime feature check above;
        // lit_length = 0 → lit_src 16-byte over-read into scratch.
        unsafe {
            f.exec_sequence_inline_avx2(lits.as_ptr(), 0, offset, match_length)
                .unwrap();
        }
        assert_eq!(f.len(), base + match_length, "offset={offset}");
        let (s1, _) = f.as_slices();
        for i in 0..match_length {
            assert_eq!(
                s1[base + i],
                reference[base + i],
                "offset={offset} byte {i}: got {:#x}, expected {:#x} \
                     (regression: AVX2 wildcopy at offset < 32)",
                s1[base + i],
                reference[base + i],
            );
        }
    }
}

/// Fallible capacity guard — `exec_sequence_inline` MUST return
/// `OutputBufferOverflow` instead of writing past `Vec::capacity()`
/// when the requested write + 15-byte SSE2 overshoot would
/// overflow. Mirrors the contract on `UserSliceBackend`.
#[cfg(target_arch = "x86_64")]
#[test]
fn exec_sequence_inline_capacity_overflow_returns_err() {
    // Tiny capacity: 32 bytes + WILDCOPY_OVERLENGTH = 64 total.
    let mut f = FlatBuf::with_capacity(32);
    f.extend(&[0u8; 16]);
    // Request `lit_length + match_length + 15 = 17 + 100 + 15 = 132`
    // bytes past tail; well over the 64-byte allocation. The literal
    // buffer is `lit_length.next_multiple_of(16) = 32` bytes so the call
    // satisfies the inline read-slack precondition even if the capacity
    // guard later moves past the first literal read.
    let lits = [0xAAu8; 32];
    // SAFETY: error-returning path; no writes performed.
    let result = unsafe { f.exec_sequence_inline(lits.as_ptr(), 17, 8, 100) };
    assert!(
        matches!(
            result,
            Err(super::super::errors::ExecuteSequencesError::OutputBufferOverflow { .. })
        ),
        "expected OutputBufferOverflow, got {result:?}"
    );
}

/// AVX2 analogue of the capacity guard: the 32-byte-stride variant MUST
/// also return `OutputBufferOverflow` (not write past `Vec::capacity()`)
/// when the requested write plus the 31-byte overshoot exceeds the
/// remaining headroom. Guards the single-compare bounds check on the AVX2
/// hot path.
// `std` feature gate required: `is_x86_feature_detected!` is `std`-only.
#[cfg(all(target_arch = "x86_64", feature = "std"))]
#[test]
fn exec_sequence_inline_avx2_capacity_overflow_returns_err() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    let mut f = FlatBuf::with_capacity(32);
    f.extend(&[0u8; 16]);
    // 32-byte literal buffer = `lit_length.next_multiple_of(16)`, so the
    // call satisfies the inline read-slack precondition even if the guard
    // later moves past the first literal read.
    let lits = [0xAAu8; 32];
    // SAFETY: AVX2 detected above; error-returning path performs no writes.
    let result = unsafe { f.exec_sequence_inline_avx2(lits.as_ptr(), 17, 8, 100) };
    assert!(
        matches!(
            result,
            Err(super::super::errors::ExecuteSequencesError::OutputBufferOverflow { .. })
        ),
        "expected OutputBufferOverflow, got {result:?}"
    );
}

/// `FlatBuf` is growable, so the per-block decompression-bomb ceiling
/// (`set_max_capacity`) MUST be honoured on its `try_reserve` growth path —
/// the fallback `push`/`repeat` route a malformed single-segment block can
/// take when the inline slack gate fails. A reserve that would grow live
/// output past the ceiling must return `BackendOverflow` instead of growing
/// the `Vec` toward a decompression-bomb OOM.
#[test]
fn try_reserve_rejects_growth_past_block_ceiling() {
    let mut f = FlatBuf::with_capacity(64);
    f.extend(&[0u8; 32]);
    let ceiling = f.len() + 100; // 132
    f.set_max_capacity(ceiling);
    // Within the ceiling: succeeds (32 + 50 = 82 <= 132).
    assert!(f.try_reserve(50).is_ok());
    // Past the ceiling (32 + 200 = 232 > 132): rejected, no growth.
    assert!(
        matches!(
            f.try_reserve(200),
            Err(super::super::buffer_backend::BackendOverflow { .. })
        ),
        "reserve past the per-block ceiling must be rejected, not grown"
    );
}

/// The ceiling bounds this block's OUTPUT, so a reserve past it must be
/// rejected even when it FITS the existing allocation (no growth needed).
/// A large pre-reserved buffer (e.g. a known-FCS single-segment frame) has
/// spare capacity beyond `MAX_BLOCK_SIZE`; without checking the ceiling
/// ahead of the no-growth fast path, an over-producing block could write
/// into that spare and bypass the decompression-bomb guard.
#[test]
fn try_reserve_rejects_within_capacity_but_past_ceiling() {
    let mut f = FlatBuf::with_capacity(4096);
    f.extend(&[0u8; 32]);
    let ceiling = f.len() + 100; // 132, far below the 4 KiB allocation
    f.set_max_capacity(ceiling);
    // 32 + 500 = 532 <= 4096 capacity (no growth) but > 132 ceiling.
    assert!(
        matches!(
            f.try_reserve(500),
            Err(super::super::buffer_backend::BackendOverflow { .. })
        ),
        "a reserve past the ceiling must be rejected even when it fits the \
             current capacity without growth"
    );
}
