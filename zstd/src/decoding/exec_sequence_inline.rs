//! Verbatim port of donor zstd's `ZSTD_execSequence` body
//! (lib/decompress/zstd_decompress_block.c:1008-1105) for the
//! `UserSliceBackend` direct-write decode path. Bypasses the
//! `DecodeBuffer::push` + `repeat` abstraction chain in favour of
//! donor's straight-line shape:
//!
//! 1. Literal copy: unconditional 16-byte SIMD store + wildcopy tail
//!    if `litLength > 16`. Mirrors donor's "split out litLength <= 16
//!    since it is nearly always true" comment.
//! 2. Match copy fast path: `offset >= 16` → single wildcopy
//!    (`no_overlap` semantics, 16-byte SIMD loop).
//! 3. Match copy short-offset: `offset < 16` →
//!    [`ZSTD_overlapCopy8`] spreading then wildcopy
//!    (`overlap_src_before_dst`, 8-byte loop while diff < 16,
//!    16-byte once diff catches up).
//!
//! Helpers are private SSE2-baseline x86_64 ops (all supported
//! x86_64 targets carry SSE2). Non-x86 paths fall back through the
//! existing `BufferBackend::extend` + `DecodeBuffer::repeat` chain
//! (`UserSliceBackend::SUPPORTS_INLINE_SEQUENCE_EXEC` returns `false`
//! on those targets, so the dispatch site dead-eliminates this code
//! at compile time per backend monomorphisation).

// x86_64 only: SSE2 is the architectural baseline there (every x86_64
// CPU has SSE2 by definition). 32-bit `x86` is excluded because the
// SSE2 intrinsics here are emitted without a `#[target_feature]`
// gate, and 32-bit i386 / i486 / i586 targets do not always have
// SSE2 in their baseline. The dispatch site
// (`UserSliceBackend::SUPPORTS_INLINE_SEQUENCE_EXEC`) mirrors this cfg
// so the legacy chain handles non-x86_64 targets.
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86 {
    use core::arch::x86_64::{
        __m128i, __m256i, _mm_loadu_si128, _mm_storeu_si128, _mm256_loadu_si256,
        _mm256_storeu_si256,
    };

    /// AVX2-tier `ZSTD_copy16`-equivalent: 32-byte ymm load/store. Used
    /// by the AVX2-scoped wildcopy variant below. Caller must be in
    /// target_feature(avx2) scope. Issue #279 round 3 Phase 4.
    ///
    /// # Safety
    /// `dst` and `src` must each be valid for 32 bytes; regions
    /// non-overlapping for the no-overlap caller; target_feature(avx2)
    /// scope on caller.
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) unsafe fn copy32_avx2(dst: *mut u8, src: *const u8) {
        unsafe {
            let v = _mm256_loadu_si256(src as *const __m256i);
            _mm256_storeu_si256(dst as *mut __m256i, v);
        }
    }

    /// AVX2-tier `ZSTD_wildcopy(..., ZSTD_no_overlap)`: 32-byte ymm
    /// loop until at least `length` bytes are written. May overshoot
    /// up to 31 bytes past `dst + length`. Same caller contract as
    /// [`wildcopy_no_overlap`] but doubled stride; AVX2 / WILDCOPY_OVERLENGTH
    /// slack must accommodate ≥ 31 byte tail overshoot at the
    /// destination. Issue #279 round 3 Phase 4.
    ///
    /// # Safety
    /// Same as [`wildcopy_no_overlap`] plus caller in
    /// target_feature(avx2) scope.
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) unsafe fn wildcopy_no_overlap_avx2(dst: *mut u8, src: *const u8, length: usize) {
        debug_assert!(length > 0);
        unsafe {
            let mut off = 0usize;
            loop {
                copy32_avx2(dst.add(off), src.add(off));
                off += 32;
                if off >= length {
                    break;
                }
            }
        }
    }

    /// Donor's `ZSTD_copy16`: one unaligned 16-byte SIMD store.
    /// SSE2 is the x86_64 baseline (and on x86 we gate via the
    /// module's `cfg(target_arch)`), so the intrinsics are always
    /// available without a per-call CPU feature check.
    #[inline(always)]
    pub(crate) unsafe fn copy16(dst: *mut u8, src: *const u8) {
        unsafe {
            let v = _mm_loadu_si128(src as *const __m128i);
            _mm_storeu_si128(dst as *mut __m128i, v);
        }
    }

    /// Donor's `ZSTD_wildcopy(..., ZSTD_no_overlap)`: 16-byte SIMD
    /// loop until at least `length` bytes are written. May overshoot
    /// up to 15 bytes past `dst + length`; caller's
    /// `WILDCOPY_OVERLENGTH` slack accommodates.
    #[inline(always)]
    pub(crate) unsafe fn wildcopy_no_overlap(dst: *mut u8, src: *const u8, length: usize) {
        debug_assert!(length > 0);
        unsafe {
            let mut off = 0usize;
            loop {
                copy16(dst.add(off), src.add(off));
                off += 16;
                if off >= length {
                    break;
                }
            }
        }
    }

    /// Donor's `ZSTD_wildcopy(..., ZSTD_overlap_src_before_dst)` for
    /// the `diff < WILDCOPY_VECLEN` (= < 16) arm: 8-byte unaligned
    /// loop. Each iter reads `src + off` (8 bytes) which may be in
    /// the just-written destination region — correct for RLE
    /// expansion once the source/dest gap is ≥ 8.
    #[inline(always)]
    pub(crate) unsafe fn wildcopy_overlap_8byte_stride(
        dst: *mut u8,
        src: *const u8,
        length: usize,
    ) {
        debug_assert!(length > 0);
        unsafe {
            let mut off = 0usize;
            loop {
                let v: u64 = src.add(off).cast::<u64>().read_unaligned();
                dst.add(off).cast::<u64>().write_unaligned(v);
                off += 8;
                if off >= length {
                    break;
                }
            }
        }
    }

    /// Donor's `ZSTD_overlapCopy8`
    /// (zstd_decompress_block.c:799-826). Copies 8 bytes from `src`
    /// to `dst` and, when `offset < 8`, "spreads" the source/dest
    /// distance so the following wildcopy can use the safe ≥ 8
    /// stride.
    ///
    /// Returns the updated `(dst, src)` pair (caller's old pointers
    /// are no longer valid).
    #[inline(always)]
    pub(crate) unsafe fn overlap_copy8(
        dst: *mut u8,
        src: *const u8,
        offset: usize,
    ) -> (*mut u8, *const u8) {
        // dec32table / dec64table — donor's two precomputed lookup
        // tables for the offset < 8 spread step.
        const DEC32_TABLE: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        const DEC64_TABLE: [i32; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        unsafe {
            if offset < 8 {
                // Read 4 bytes, advance src by dec32, read 4 more bytes,
                // then back-advance by dec64 — see donor source.
                let sub2 = DEC64_TABLE[offset];
                dst.add(0).write(src.add(0).read());
                dst.add(1).write(src.add(1).read());
                dst.add(2).write(src.add(2).read());
                dst.add(3).write(src.add(3).read());
                let dec32 = DEC32_TABLE[offset] as usize;
                let v: u32 = src.add(dec32).cast::<u32>().read_unaligned();
                dst.add(4).cast::<u32>().write_unaligned(v);
                // Post-call src position is `src + (dec32 - sub2 + 8)`.
                // Computing this as
                // `src.add(dec32).offset(-(sub2 as isize)).add(8)`
                // (donor's literal C transcription) produces an
                // intermediate pointer below the allocation base
                // when `dec32 < sub2` — true for every offset ∈ 1..=7
                // in donor's tables — which is UB under Rust's
                // `.offset()` provenance rules even when the final
                // pointer lands back in-bounds. Apply the net signed
                // offset once so no intermediate underflows.
                let net_offset = dec32 as isize - sub2 as isize + 8;
                debug_assert!(
                    net_offset >= 0,
                    "overlap_copy8 net offset is non-negative for all offset ∈ 1..=7"
                );
                let src_after = src.offset(net_offset);
                (dst.add(8), src_after)
            } else {
                // ZSTD_copy8 — straight 8-byte unaligned move.
                let v: u64 = src.cast::<u64>().read_unaligned();
                dst.cast::<u64>().write_unaligned(v);
                (dst.add(8), src.add(8))
            }
        }
    }
}

/// Portable (non-x86) wildcopy helpers: identical byte-level contract
/// to [`x86`], expressed with `read_unaligned`/`write_unaligned` so any
/// target can use them. On aarch64 LLVM lowers the 16-byte
/// `u128` load/store to a single NEON `ldr q`/`str q`; on other targets
/// it picks the widest available move. Used by the aarch64 (and any
/// `BufferBackend::exec_sequence_inline` arm to get the same donor
/// `ZSTD_execSequence` shape the x86 path already has, instead of
/// falling through to the slow `try_push` + `repeat` chain.
///
/// Available on every non-x86 target: both inline-exec backends
/// (`FlatBuf`, `UserSliceBackend`) set `SUPPORTS_INLINE_SEQUENCE_EXEC =
/// true` everywhere and carry a `cfg(not(x86_64))` arm built on these
/// helpers, so they are live on aarch64, i686, riscv, wasm, etc. x86_64
/// uses the SSE2 [`x86`] module instead, so this is gated out there to
/// avoid two definitions.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) mod portable {
    /// Donor `ZSTD_copy16`: one unaligned 16-byte move.
    ///
    /// # Safety
    /// `dst` / `src` valid for 16 bytes; regions non-overlapping.
    #[inline(always)]
    pub(crate) unsafe fn copy16(dst: *mut u8, src: *const u8) {
        unsafe {
            let v: u128 = src.cast::<u128>().read_unaligned();
            dst.cast::<u128>().write_unaligned(v);
        }
    }

    /// Donor `ZSTD_wildcopy(..., ZSTD_no_overlap)`: 16-byte loop until at
    /// least `length` bytes written. May overshoot up to 15 bytes past
    /// `dst + length`; caller's `WILDCOPY_OVERLENGTH` slack accommodates.
    ///
    /// # Safety
    /// `dst` writable for `length + 15`; `src` readable for `length + 15`;
    /// no-overlap (`dst` and `src` regions disjoint, donor semantics).
    #[inline(always)]
    pub(crate) unsafe fn wildcopy_no_overlap(dst: *mut u8, src: *const u8, length: usize) {
        debug_assert!(length > 0);
        unsafe {
            let mut off = 0usize;
            loop {
                copy16(dst.add(off), src.add(off));
                off += 16;
                if off >= length {
                    break;
                }
            }
        }
    }

    /// Donor `ZSTD_wildcopy(..., ZSTD_overlap_src_before_dst)` 8-byte arm:
    /// each iter reads `src + off` (may lie in the just-written
    /// destination), correct once the src/dst gap is ≥ 8.
    ///
    /// # Safety
    /// `dst` writable for `length + 7`; `src` readable for `length + 7`;
    /// the src/dst gap must be ≥ 8 (caller establishes via
    /// [`overlap_copy8`]).
    #[inline(always)]
    pub(crate) unsafe fn wildcopy_overlap_8byte_stride(
        dst: *mut u8,
        src: *const u8,
        length: usize,
    ) {
        debug_assert!(length > 0);
        unsafe {
            let mut off = 0usize;
            loop {
                let v: u64 = src.add(off).cast::<u64>().read_unaligned();
                dst.add(off).cast::<u64>().write_unaligned(v);
                off += 8;
                if off >= length {
                    break;
                }
            }
        }
    }

    /// Donor `ZSTD_overlapCopy8`: copies 8 bytes and, for `offset < 8`,
    /// spreads the src/dst distance so the following wildcopy can use the
    /// safe ≥ 8 stride. Returns the updated `(dst, src)` pair. Byte-exact
    /// port of [`super::x86::overlap_copy8`] (same dec32/dec64 tables and
    /// the same net-offset computation that avoids intermediate pointer
    /// underflow).
    ///
    /// # Safety
    /// `dst` writable for 8 bytes from the returned pointer's base; `src`
    /// readable for the spread reads; `offset >= 1`.
    #[inline(always)]
    pub(crate) unsafe fn overlap_copy8(
        dst: *mut u8,
        src: *const u8,
        offset: usize,
    ) -> (*mut u8, *const u8) {
        const DEC32_TABLE: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        const DEC64_TABLE: [i32; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        unsafe {
            if offset < 8 {
                let sub2 = DEC64_TABLE[offset];
                dst.add(0).write(src.add(0).read());
                dst.add(1).write(src.add(1).read());
                dst.add(2).write(src.add(2).read());
                dst.add(3).write(src.add(3).read());
                let dec32 = DEC32_TABLE[offset] as usize;
                let v: u32 = src.add(dec32).cast::<u32>().read_unaligned();
                dst.add(4).cast::<u32>().write_unaligned(v);
                let net_offset = dec32 as isize - sub2 as isize + 8;
                debug_assert!(
                    net_offset >= 0,
                    "overlap_copy8 net offset is non-negative for all offset ∈ 1..=7"
                );
                let src_after = src.offset(net_offset);
                (dst.add(8), src_after)
            } else {
                let v: u64 = src.cast::<u64>().read_unaligned();
                dst.cast::<u64>().write_unaligned(v);
                (dst.add(8), src.add(8))
            }
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod inline_helper_tests {
    use super::x86::{copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride};

    #[test]
    fn copy16_copies_exactly_16_bytes() {
        let src: [u8; 16] = [
            0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD,
            0xAE, 0xAF,
        ];
        let mut dst = [0u8; 16];
        unsafe { copy16(dst.as_mut_ptr(), src.as_ptr()) };
        assert_eq!(dst, src);
    }

    #[test]
    fn wildcopy_no_overlap_short_length_overshoots() {
        // Length 1 still triggers the unconditional first 16-byte
        // store — the wildcopy overshoots up to 15 bytes past the
        // declared end, which is the donor contract.
        let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
        let mut dst = [0u8; 32];
        unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 1) };
        // First 16 bytes copied from src; remaining untouched.
        assert_eq!(&dst[..16], &src[..16]);
        assert!(dst[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn wildcopy_no_overlap_length_above_16_uses_multiple_iters() {
        // Length 24 → first 16-byte store, then one more iter that
        // overshoots 8 bytes past the declared end.
        let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
        let mut dst = [0u8; 32];
        unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 24) };
        // 32 bytes get written (two 16-byte stores).
        assert_eq!(&dst[..32], &src[..32]);
    }

    #[test]
    fn wildcopy_overlap_8byte_stride_rle_expansion_offset_8() {
        // Offset = 8 means caller has set up src = dst - 8. Each
        // 8-byte read picks up bytes the previous iter just wrote,
        // expanding the seed pattern across the destination region.
        let mut buf = [0u8; 32];
        buf[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        unsafe {
            wildcopy_overlap_8byte_stride(buf.as_mut_ptr().add(8), buf.as_ptr(), 16);
        }
        // Bytes 8..16 = seed; bytes 16..24 = seed again (RLE expansion).
        assert_eq!(&buf[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&buf[16..24], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn overlap_copy8_offset_ge_8_does_plain_copy() {
        // offset >= 8 path: straight ZSTD_copy8 (8-byte read+write).
        let mut buf = [0u8; 32];
        buf[..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        let (op2, ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(8), buf.as_ptr(), 8) };
        // dst advances by 8 bytes, src advances by 8 bytes.
        assert_eq!(op2, unsafe { buf.as_mut_ptr().add(16) });
        assert_eq!(ip2, unsafe { buf.as_ptr().add(8) });
        // bytes 8..16 = seed.
        assert_eq!(
            &buf[8..16],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }

    #[test]
    fn overlap_copy8_offset_lt_8_spreads_source() {
        // offset < 8 path: uses dec32table / dec64table to spread
        // the source-destination distance so subsequent wildcopy can
        // use the ≥ 8 stride. Test offset = 3 (a common short-offset
        // RLE pattern).
        let mut buf = [0u8; 32];
        buf[..3].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
        let (op2, _ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(3), buf.as_ptr(), 3) };
        // dst advanced 8 bytes.
        assert_eq!(op2, unsafe { buf.as_mut_ptr().add(11) });
        // First 8 bytes of the destination region are the 3-byte
        // seed expanded — verify they're non-zero (exact spread
        // pattern depends on the lookup tables; donor parity is the
        // contract).
        assert!(buf[3..11].iter().any(|&b| b != 0));
    }
}

// Parallel coverage for the portable helpers (non-x86 targets). Mirrors
// `inline_helper_tests` exactly: the portable module is the unsafe
// pointer-copy backend the non-x86 `exec_sequence_inline` arms rely on,
// so it must carry the same exact-copy / overshoot / short-offset-spread
// assertions as the SSE2 helpers it byte-for-byte mirrors. On the host
// CI matrix this runs under the i686-unknown-linux-gnu test job.
#[cfg(all(test, not(target_arch = "x86_64")))]
mod portable_helper_tests {
    use super::portable::{
        copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride,
    };

    #[test]
    fn copy16_copies_exactly_16_bytes() {
        let src: [u8; 16] = [
            0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD,
            0xAE, 0xAF,
        ];
        let mut dst = [0u8; 16];
        unsafe { copy16(dst.as_mut_ptr(), src.as_ptr()) };
        assert_eq!(dst, src);
    }

    #[test]
    fn wildcopy_no_overlap_short_length_overshoots() {
        // Length 1 still triggers the unconditional first 16-byte store
        // — the wildcopy overshoots up to 15 bytes past the declared
        // end, the donor contract.
        let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
        let mut dst = [0u8; 32];
        unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 1) };
        assert_eq!(&dst[..16], &src[..16]);
        assert!(dst[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn wildcopy_no_overlap_length_above_16_uses_multiple_iters() {
        // Length 24 → first 16-byte store, then one more iter
        // overshooting 8 bytes past the declared end.
        let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
        let mut dst = [0u8; 32];
        unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 24) };
        assert_eq!(&dst[..32], &src[..32]);
    }

    #[test]
    fn wildcopy_overlap_8byte_stride_rle_expansion_offset_8() {
        // Offset = 8: src = dst - 8. Each 8-byte read picks up bytes the
        // previous iter just wrote, expanding the seed (RLE).
        let mut buf = [0u8; 32];
        buf[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        unsafe {
            wildcopy_overlap_8byte_stride(buf.as_mut_ptr().add(8), buf.as_ptr(), 16);
        }
        assert_eq!(&buf[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&buf[16..24], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn overlap_copy8_offset_ge_8_does_plain_copy() {
        // offset >= 8: straight ZSTD_copy8 (8-byte read+write).
        let mut buf = [0u8; 32];
        buf[..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        let (op2, ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(8), buf.as_ptr(), 8) };
        assert_eq!(op2, unsafe { buf.as_mut_ptr().add(16) });
        assert_eq!(ip2, unsafe { buf.as_ptr().add(8) });
        assert_eq!(
            &buf[8..16],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }

    #[test]
    fn overlap_copy8_offset_lt_8_spreads_source() {
        // offset < 8: dec32table / dec64table spread step. offset = 3.
        let mut buf = [0u8; 32];
        buf[..3].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
        let (op2, _ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(3), buf.as_ptr(), 3) };
        assert_eq!(op2, unsafe { buf.as_mut_ptr().add(11) });
        assert!(buf[3..11].iter().any(|&b| b != 0));
    }

    /// Cross-check: the portable helpers must produce byte-identical
    /// output to a straightforward scalar reference for the no-overlap
    /// copy across a range of lengths. Guards against a divergence
    /// between the u128/u64 unaligned-move lowering and plain copies.
    #[test]
    fn wildcopy_no_overlap_matches_scalar_reference() {
        for len in 1usize..=48 {
            let src: [u8; 64] = core::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(1));
            let mut dst = [0u8; 64];
            unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), len) };
            // Only the first `len` bytes are contractually defined; the
            // overshoot tail is allowed to differ. Assert the defined
            // region matches.
            assert_eq!(&dst[..len], &src[..len], "len={len}");
        }
    }
}
