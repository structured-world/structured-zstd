extern crate alloc;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

#[test]
fn extend_writes_at_tail() {
    let mut buf = vec![0u8; 32];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2, 3, 4]);
    assert_eq!(b.len(), 4);
    assert_eq!(b.tail(), 4);
    b.extend(&[5, 6]);
    let (s, t) = b.as_slices();
    assert_eq!(s, &[1, 2, 3, 4, 5, 6]);
    assert!(t.is_empty());
}

#[test]
fn extend_and_fill_repeats_byte() {
    let mut buf = vec![0u8; 16];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[0xAA]);
    b.extend_and_fill(0xBB, 4);
    let (s, _) = b.as_slices();
    assert_eq!(s, &[0xAA, 0xBB, 0xBB, 0xBB, 0xBB]);
}

#[test]
fn extend_from_within_unchecked_copies_non_overlapping() {
    let mut buf = vec![0u8; 32];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[10, 20, 30, 40, 50]);
    // SAFETY: 0+3 <= 5 = len; cap 32 covers 5+3.
    unsafe { b.extend_from_within_unchecked(0, 3) };
    let (s, _) = b.as_slices();
    assert_eq!(s, &[10, 20, 30, 40, 50, 10, 20, 30]);
}

#[test]
fn drop_first_n_advances_head_keeps_history() {
    let mut buf = vec![0u8; 32];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2, 3, 4, 5]);
    b.drop_first_n(2);
    assert_eq!(b.len(), 3);
    let (s, _) = b.as_slices();
    assert_eq!(s, &[3, 4, 5]);
    // After drop, drained bytes remain physically present and can
    // back a match copy via `start` indexed from the post-drop head.
    unsafe { b.extend_from_within_unchecked(0, 3) };
    let (s, _) = b.as_slices();
    assert_eq!(s, &[3, 4, 5, 3, 4, 5]);
}

#[test]
fn set_tail_rollback() {
    let mut buf = vec![0u8; 32];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2, 3]);
    let saved = b.tail();
    b.extend(&[4, 5, 6, 7]);
    assert_eq!(b.len(), 7);
    unsafe { b.set_tail(saved) };
    assert_eq!(b.len(), 3);
    let (s, _) = b.as_slices();
    assert_eq!(s, &[1, 2, 3]);
}

#[test]
fn clear_resets_cursors() {
    let mut buf = vec![0u8; 32];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2, 3]);
    b.drop_first_n(1);
    b.clear();
    assert_eq!(b.len(), 0);
    assert_eq!(b.tail(), 0);
}

#[test]
fn extend_from_reader_into_slice() {
    let mut buf = vec![0u8; 16];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    let src = [9u8, 8, 7, 6, 5];
    b.extend_from_reader(&src[..], 5).unwrap();
    let (s, _) = b.as_slices();
    assert_eq!(s, &[9, 8, 7, 6, 5]);
}

#[test]
fn extend_from_reader_over_capacity_errors() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    let src = [9u8, 8, 7, 6, 5];
    // 5 bytes requested, only 4 cap -> error, tail unchanged.
    assert!(b.extend_from_reader(&src[..], 5).is_err());
    assert_eq!(b.tail(), 0);
}

// Coverage for the fallible try_* surface. Exercises:
//   - happy paths (exact-fit + room to spare),
//   - capacity-overflow paths (returns Err with diagnostic fields),
//   - integer-overflow wrap-guards (checked_add ok_or branch).

use super::super::buffer_backend::BufferBackend;

#[test]
fn try_extend_exact_fit_succeeds_and_advances_tail() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    assert!(b.try_extend(&[1, 2, 3, 4]).is_ok());
    assert_eq!(b.tail(), 4);
    let (s, _) = b.as_slices();
    assert_eq!(s, &[1, 2, 3, 4]);
}

#[test]
fn try_extend_over_capacity_returns_overflow_and_keeps_tail() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    let err = b.try_extend(&[1, 2, 3, 4, 5]).unwrap_err();
    assert_eq!(err.tail, 0);
    assert_eq!(err.requested, 5);
    assert_eq!(err.capacity, 4);
    assert_eq!(b.tail(), 0);
}

#[test]
fn try_extend_partially_full_overshoot_reports_current_tail() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2]);
    // 3 more bytes would need 5 total, capacity is 4.
    let err = b.try_extend(&[3, 4, 5]).unwrap_err();
    assert_eq!(err.tail, 2);
    assert_eq!(err.requested, 3);
    assert_eq!(err.capacity, 4);
    assert_eq!(b.tail(), 2);
}

#[test]
fn try_extend_zero_length_succeeds_and_leaves_tail_unchanged() {
    // The `checked_add(tail, len)` wrap branch in `try_extend` is
    // a defense-in-depth guard for corrupted input that names
    // `regenerated_size` near `usize::MAX`. Constructing such a
    // `&[u8]` from safe Rust is not expressible — `from_raw_parts`
    // with a forged length is UB. The wrap branch is exercised
    // only from the fuzz harness under `feature = "fuzz_exports"`
    // (which routes a controlled `len` through `try_*`) and from
    // the real malformed-frame decode path that the harness
    // emulates.
    //
    // This test covers the adjacent normal case — a zero-length
    // `try_extend` MUST succeed regardless of current `tail`
    // (the new_tail = tail + 0 = tail comparison both passes
    // checked_add and the upper-bound check). Without this case
    // the early-return on `len == 0` could regress silently.
    let mut buf = vec![0u8; 8];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    assert!(b.try_extend(&[]).is_ok());
    assert_eq!(b.tail(), 0);
    b.extend(&[1, 2, 3]);
    assert!(b.try_extend(&[]).is_ok());
    assert_eq!(b.tail(), 3);
}

#[test]
fn try_extend_and_fill_exact_fit_writes_pattern() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    assert!(b.try_extend_and_fill(0xAB, 4).is_ok());
    assert_eq!(b.tail(), 4);
    let (s, _) = b.as_slices();
    assert_eq!(s, &[0xAB, 0xAB, 0xAB, 0xAB]);
}

#[test]
fn try_extend_and_fill_over_capacity_returns_overflow() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2]);
    let err = b.try_extend_and_fill(0xCD, 5).unwrap_err();
    assert_eq!(err.tail, 2);
    assert_eq!(err.requested, 5);
    assert_eq!(err.capacity, 4);
    assert_eq!(b.tail(), 2);
}

#[test]
fn try_extend_from_within_within_bounds_repeats_history() {
    let mut buf = vec![0u8; 8];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2, 3]);
    // Repeat the first 3 bytes from history into the next 3 slots.
    assert!(b.try_extend_from_within(0, 3).is_ok());
    let (s, _) = b.as_slices();
    assert_eq!(s, &[1, 2, 3, 1, 2, 3]);
    assert_eq!(b.tail(), 6);
}

#[test]
fn try_extend_from_within_source_past_tail_returns_overflow() {
    let mut buf = vec![0u8; 8];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2]);
    // start=0, len=5 — source range needs bytes 0..5 but tail=2.
    let err = b.try_extend_from_within(0, 5).unwrap_err();
    assert_eq!(err.tail, 2);
    assert_eq!(err.requested, 5);
    assert_eq!(b.tail(), 2);
}

#[test]
fn try_extend_from_within_destination_overflow_returns_err() {
    let mut buf = vec![0u8; 4];
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.extend(&[1, 2, 3]);
    // Source 0..2 valid, but writing 2 more bytes would push tail
    // from 3 to 5, past capacity 4.
    let err = b.try_extend_from_within(0, 2).unwrap_err();
    assert_eq!(err.tail, 3);
    assert_eq!(err.requested, 2);
    assert_eq!(err.capacity, 4);
    assert_eq!(b.tail(), 3);
}

/// Direct tests for `exec_sequence_inline` — exercise the
/// x86_64 inline body so coverage attributes its 40 lines to
/// these tests, not through the deep `decode_all`
/// pipeline where `cargo llvm-cov` sometimes loses the inlined
/// callee. Tests cover: short-literal + short-match, long
/// literal (wildcopy tail), short-offset match (overlapCopy8 +
/// 8-byte stride), long-offset match (wildcopy_no_overlap).
#[test]
fn exec_sequence_inline_overflow_returns_output_buffer_overflow() {
    // #246: the upstream zstd-inline sequence executor must return
    // `ExecuteSequencesError::OutputBufferOverflow` (NOT panic, NOT
    // an out-of-bounds unsafe write) when a sequence's literal+match
    // copy plus wildcopy overshoot would land past the fixed slice.
    // This is the per-sequence guard that keeps the unsafe write
    // surface inside the user slice on the way to the post-block FCS
    // check; the higher-level acceptance test
    // (`decode_all_compressed_block_fcs_overflow_returns_structured_error`)
    // proves the whole chain folds into `FrameContentSizeMismatch`.
    use super::super::errors::ExecuteSequencesError;
    // Tiny slice: 16-byte capacity, tail already at 8. A sequence
    // writing 8 literals + 8 match (16 bytes) plus the 15-byte
    // wildcopy overshoot cannot fit in `16 - 8 = 8` remaining bytes.
    let mut buf = vec![0u8; 16];
    for (i, slot) in buf.iter_mut().take(8).enumerate() {
        *slot = i as u8;
    }
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.tail = 8;
    let lits = [0xAAu8; 16];
    // SAFETY: `lits` is a 16-byte parent buffer (upstream zstd 16-byte read
    // slack satisfied); the call must return Err before any write
    // past the slice end.
    let err = unsafe { b.exec_sequence_inline(lits.as_ptr(), 8, 4, 8) }
        .expect_err("overshoot must return OutputBufferOverflow");
    assert!(
        matches!(err, ExecuteSequencesError::OutputBufferOverflow { .. }),
        "expected OutputBufferOverflow, got {err:?}"
    );
    // Backend left untouched on Err — tail not advanced.
    assert_eq!(b.tail, 8, "tail must not advance on overflow");
}

#[test]
fn exec_sequence_inline_short_literal_plus_long_offset_match() {
    // Layout: pre-fill `tail = 8` with a "history" region so
    // a match copy at offset 16 reaches inside the slice. Then
    // bump tail past that history and exercise upstream zstd_exec.
    // Buffer sized with WILDCOPY_OVERLENGTH slack at the end.
    const WILDCOPY: usize = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut buf = vec![0u8; 256 + WILDCOPY];
    // Seed history: bytes 0..32 = ascending values, so a later
    // match at offset 16 picks up bytes 16..32.
    for (i, slot) in buf.iter_mut().take(32).enumerate() {
        *slot = i as u8;
    }
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.tail = 32; // Pretend 32 history bytes are already written.

    // 8-byte literals to write (upstream zstd's litLength <= 16 fast
    // path — no wildcopy tail). Match length 8 at offset 16.
    let lits: [u8; 16] = [
        0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22, 0xA1, 0xB1, 0xC1, 0xD1, 0xE1, 0xF1, 0x10,
        0x20,
    ];
    unsafe {
        b.exec_sequence_inline(lits.as_ptr(), 8, 16, 8).unwrap();
    }
    // tail advanced by 8 lit + 8 match = 16.
    assert_eq!(b.tail, 48);
    // Literals landed at tail = 32..40.
    assert_eq!(&buf[32..40], &lits[..8]);
    // Match: at output position 40..48, source = tail + lit_len -
    // offset = 32 + 8 - 16 = 24. So buf[40..48] == buf[24..32]
    // (history bytes 24..32 = [24, 25, 26, 27, 28, 29, 30, 31]).
    assert_eq!(&buf[40..48], &[24u8, 25, 26, 27, 28, 29, 30, 31]);
}

#[test]
fn exec_sequence_inline_long_literal_uses_wildcopy_tail() {
    // litLength > 16 path: unconditional copy16 + wildcopy tail
    // for the remaining literal bytes.
    const WILDCOPY: usize = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut buf = vec![0u8; 256 + WILDCOPY];
    for (i, slot) in buf.iter_mut().take(32).enumerate() {
        *slot = i as u8;
    }
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.tail = 32;

    // 40-byte literals (forces wildcopy tail) — needs 40-byte
    // source buffer with extra read slack for the final
    // 16-byte load.
    let lits: Vec<u8> = (0..40u8 + 16).map(|i| 0x80 + i).collect();
    unsafe {
        b.exec_sequence_inline(lits.as_ptr(), 40, 16, 8).unwrap();
    }
    assert_eq!(b.tail, 80);
    assert_eq!(&buf[32..72], &lits[..40]);
    // Match at offset 16: src = 32 + 40 - 16 = 56. buf[72..80]
    // == buf[56..64] (which we just wrote = lits[24..32]).
    assert_eq!(&buf[72..80], &lits[24..32]);
}

#[test]
fn exec_sequence_inline_short_offset_match_uses_overlap_copy() {
    // offset < 16 takes the overlapCopy8 + 8-byte stride path
    // (vs. the offset >= 16 wildcopy_no_overlap path).
    //
    // What we actually assert here:
    //   1. `tail` advances by `lit_length + match_length`.
    //   2. The literal payload lands at `buf[tail..tail+ll]`.
    //   3. The FIRST 4 match-output bytes match seed[4..8] —
    //      that prefix of the match copy reads source bytes
    //      that the literal `copy16` overshoot did NOT
    //      overwrite (the upstream zstd `copy16` writes 16 bytes at
    //      `tail`, so source bytes BEFORE `tail` survive).
    //
    // We do NOT cross-validate against the legacy `extend` +
    // `repeat_in_chunks` chain: those paths don't perform the
    // 16-byte literal overshoot, so they produce a different
    // output for the same logical sequence (different bytes in
    // positions where the upstream zstd's overshoot is consumed by the
    // match copy). End-to-end parity is covered by the higher
    // level `roundtrip_integrity::*` tests in lib.rs which
    // decode whole frames and compare to the encoder input.
    const WILDCOPY: usize = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut buf = vec![0u8; 256 + WILDCOPY];
    // Seed last 8 bytes of history with a recognisable pattern.
    let seed = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7];
    buf[24..32].copy_from_slice(&seed);
    let mut b = UserSliceBackend::from_slice(&mut buf);
    b.tail = 32;

    let lits: [u8; 16] = [0xFF; 16];
    // litLength=4, offset=8, matchLength=12. offset<16 → short
    // path (overlapCopy8 + 8-byte stride).
    unsafe {
        b.exec_sequence_inline(lits.as_ptr(), 4, 8, 12).unwrap();
    }
    // tail = 32 + 4 + 12 = 48.
    assert_eq!(b.tail, 48);
    // Literal copy: bytes 32..36 are the literal payload.
    assert_eq!(&buf[32..36], &lits[..4]);
    // Match copy: the first 4 output bytes (positions 36..40)
    // are the FIRST 4 source bytes (positions 28..32), which the
    // literal copy16 has NOT overwritten (it wrote at 32..48, so
    // 28..32 remain as the seed tail). Verify those.
    assert_eq!(&buf[36..40], &seed[4..8]);
    // The remaining 8 match bytes (40..48) get fed by the
    // 8-byte-stride wildcopy reading from positions inside the
    // match-destination region, which the literal copy16 already
    // overwrote with 0xFF. That's the upstream zstd invariant — the
    // overshoot is consumed correctly. We don't pin the exact
    // bytes (they're a function of overlap_copy8's spread
    // tables) but the output length must be right.
}

/// `exec_sequence_inline_avx2` correctness across the
/// match-copy offset cases. Issue #279 round 4 regression:
/// the AVX2 32-byte ymm wildcopy threshold MUST be `offset >= 32`
/// (not >= 16). At offset 16..31 a 32-byte load from `match_src`
/// reads bytes inside the destination region BEFORE the first
/// store has written them; SSE2 16-byte fallback covers those
/// offsets safely. Test exercises:
/// - offset 20 (mid-range, must route to SSE2 fallback)
/// - offset 32 (boundary, AVX2 path)
/// - offset 64 (deep AVX2 path)
///
/// against a byte-by-byte reference of the same repeat semantics.
// `std` feature gates the test: `is_x86_feature_detected!` is `std`-only
// (runtime CPU detection), unavailable in the crate's `#![no_std]` build.
#[cfg(all(target_arch = "x86_64", feature = "std"))]
#[test]
fn exec_sequence_inline_avx2_offset_boundary_correctness() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    const WILDCOPY: usize = super::super::buffer_backend::WILDCOPY_OVERLENGTH;

    for offset in [20usize, 32, 64] {
        let mut buf = vec![0u8; 512 + WILDCOPY];
        // Seed bytes 0..200 with a deterministic, byte-position-derived
        // pattern so the reference match-copy is unambiguous.
        for (i, slot) in buf.iter_mut().take(200).enumerate() {
            *slot = ((i * 31 + 7) & 0xFF) as u8;
        }
        // Compute reference: byte-by-byte repeat from offset 100..(100+offset)
        // up to match_length bytes, written at position 200.
        let match_length = 96usize;
        let mut reference = buf.clone();
        for i in 0..match_length {
            reference[200 + i] = reference[200 + i - offset];
        }

        // Now run the AVX2 inline executor on the actual buffer.
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.tail = 200;
        let dummy_lits: [u8; 16] = [0xAA; 16];
        unsafe {
            b.exec_sequence_inline_avx2(dummy_lits.as_ptr(), 0, offset, match_length)
                .unwrap();
        }
        assert_eq!(b.tail, 200 + match_length);

        // Compare match-copy region against the reference.
        // Bytes 200..(200+match_length) must match the byte-by-byte
        // repeat. The overshoot region past match_length is not pinned.
        for i in 0..match_length {
            assert_eq!(
                buf[200 + i],
                reference[200 + i],
                "exec_sequence_inline_avx2 offset={offset} byte {i}: \
                     got {:#x}, expected {:#x} (regression: AVX2 wildcopy \
                     reading past first-store boundary)",
                buf[200 + i],
                reference[200 + i],
            );
        }
    }
}
