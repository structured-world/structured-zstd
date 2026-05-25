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
//! (`UserSliceBackend::SUPPORTS_INLINE_DONOR_EXEC` returns `false`
//! on those targets, so the dispatch site dead-eliminates this code
//! at compile time per backend monomorphisation).

// x86_64 only: SSE2 is the architectural baseline there (every x86_64
// CPU has SSE2 by definition). 32-bit `x86` is excluded because the
// SSE2 intrinsics here are emitted without a `#[target_feature]`
// gate, and 32-bit i386 / i486 / i586 targets do not always have
// SSE2 in their baseline. The dispatch site
// (`UserSliceBackend::SUPPORTS_INLINE_DONOR_EXEC`) mirrors this cfg
// so the legacy chain handles non-x86_64 targets.
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86 {
    use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_storeu_si128};

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
    /// loop. Each iter reads `src + off + 8` which may be in the
    /// just-written destination region — correct for RLE expansion
    /// once the source/dest gap is ≥ 8.
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
