//! Verbatim port of upstream zstd zstd's `ZSTD_execSequence` body
//! (lib/decompress/zstd_decompress_block.c:1008-1105) for the inline
//! direct-write decode path (`UserSliceBackend` and `FlatBuf`). Bypasses
//! the `DecodeBuffer::push` + `repeat` abstraction chain in favour of
//! upstream zstd's straight-line shape:
//!
//! 1. Literal copy: unconditional 16-byte SIMD store + wildcopy tail
//!    if `litLength > 16`. Mirrors upstream zstd's "split out litLength <= 16
//!    since it is nearly always true" comment.
//! 2. Match copy fast path: `offset >= 16` → single wildcopy
//!    (`no_overlap` semantics, 16-byte SIMD loop).
//! 3. Match copy short-offset: `offset < 16` →
//!    [`ZSTD_overlapCopy8`] spreading then wildcopy
//!    (`overlap_src_before_dst`, 8-byte loop while diff < 16,
//!    16-byte once diff catches up).
//!
//! Two helper implementations with an identical byte-level contract:
//! [`x86`] uses SSE2 intrinsics (`_mm_loadu/storeu_si128`, the x86_64
//! baseline); [`portable`] uses unaligned `u128`/`u64` moves that the
//! backend lowers to its widest store (NEON `ldr q`/`str q` on aarch64,
//! plain movs on i686/riscv/wasm). The backend's `exec_sequence_inline`
//! arm picks one by `cfg(target_arch)`. Backends gate the whole path on
//! `SUPPORTS_INLINE_SEQUENCE_EXEC` (`true` for `UserSliceBackend` /
//! `FlatBuf` on every target, `false` for `RingBuffer`, which stays on
//! the `extend` + `repeat` fallback for wrap-aware multi-segment frames).
//! See the [`portable`] module doc for how the inline path is reached
//! per target.

/// Exact, non-overshooting literal+match copy of one sequence at
/// `base[tail..]` — the cold-path twin of the SIMD wildcopy bodies. Every
/// inline-exec site (the per-kernel macros below and
/// [`super::user_slice_buf::UserSliceBackend::exec_sequence_bounded`])
/// routes its tight-tail branch here: the trailing sequence(s) of an
/// exact-fit output slice, where the wildcopy overshoot would run past the
/// buffer end. Portable (`core::ptr` only), so it is shared across all
/// kernel tiers; the per-tier divergence lives only in the fast path.
///
/// # Safety
/// - `base` is valid for writes over `[tail, tail + lit_length + match_length)`.
/// - `lit_src` is valid for reads of exactly `lit_length` bytes.
/// - `offset >= 1` and `offset <= tail + lit_length` (match source stays
///   inside the already-written region).
#[inline]
pub(crate) unsafe fn exec_sequence_bounded_copy(
    base: *mut u8,
    tail: usize,
    lit_src: *const u8,
    lit_length: usize,
    offset: usize,
    match_length: usize,
) {
    unsafe {
        let op_lit = base.add(tail);
        core::ptr::copy_nonoverlapping(lit_src, op_lit, lit_length);
        let op_match = base.add(tail + lit_length);
        let match_src = base.cast_const().add(tail + lit_length - offset);
        if offset >= match_length {
            // No overlap: source range ends before destination starts.
            core::ptr::copy_nonoverlapping(match_src, op_match, match_length);
        } else {
            // Overlapping LZ copy: forward byte-by-byte replicates the
            // `offset`-periodic pattern (upstream zstd `ZSTD_overlapCopy`, scalar form).
            let mut i = 0usize;
            while i < match_length {
                *op_match.add(i) = *match_src.add(i);
                i += 1;
            }
        }
    }
}

/// Textual expansion of the AVX2 `ZSTD_execSequence` body at the call site,
/// fusing the match-copy into a per-tier sequence monolith, addressed by an
/// explicit cursor rather than by asking the backend where it is.
///
/// A `#[target_feature(avx2)]` function cannot be `#[inline(always)]`
/// (rust#145574), so the [`BufferBackend::exec_sequence_inline_avx2`] trait
/// method stays a real CALL on the hot path; expanding the body via a macro
/// removes that boundary (the reference `decompressSequences_bmi2` is one
/// inlined monolith). The trait method remains the unit-tested reference spec
/// for this body. 32-byte ymm match-copy for `offset >= 32`; usable from any
/// tier whose enclosing fn carries `target_feature(avx2,bmi2)` (AVX2 and VBMI2).
///
/// Taking the cursor as arguments is what lets the AVX2 tier keep its write
/// position in locals for a whole block; the VBMI2 tier reads one per sequence
/// and passes it here, so both share this body. Returns the bytes written, so
/// the caller can advance whichever of the two it holds.
//
// Gated on `kernel-avx2` (implied by `kernel-vbmi2`) so the macro is absent
// when its only consumers (`seq_decoder_avx2` / `seq_decoder_vbmi2`) are
// compiled out — otherwise the `--no-default-features` build sees an unused
// macro and trips `-D warnings`.
///
/// # Safety
/// The enclosing function must carry `target_feature(bmi2,avx2)`, `base` must be
/// the start of a linear output valid for writes through `cap`, and `lit_src`
/// must be readable for the literal length rounded up to 16.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
macro_rules! exec_sequence_avx2_inline_at {
    ($base:expr, $tail:expr, $cap:expr, $lit_src:expr, $lit_length:expr, $offset:expr, $match_length:expr) => {{
        use crate::decoding::buffer_backend::sequence_output_fits;
        use crate::decoding::exec_sequence_inline::x86::{
            copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_no_overlap_avx2,
            wildcopy_overlap_8byte_stride,
        };
        const MAX_WILDCOPY_OVERSHOOT: usize = 31;
        let lit_length_v: usize = $lit_length;
        let offset_v: usize = $offset;
        let match_length_v: usize = $match_length;
        let lit_src_v: *const u8 = $lit_src;
        let base: *mut u8 = $base;
        let cap: usize = $cap;
        let tail: usize = $tail;
        // Hard guard with `overshoot = 0`; the <=31-byte wildcopy slack is
        // handled by the tight-tail branch below so an exact-fit output
        // slice (no `WILDCOPY_OVERLENGTH` trailing room) stays correct.
        match sequence_output_fits(lit_length_v, match_length_v, tail, cap, 0) {
            Err(e) => Err(e),
            Ok(total) => {
                // SAFETY: the enclosing fn carries
                // `#[target_feature(enable = "...,bmi2,avx2")]` and the caller
                // vouches for `base`; `sequence_output_fits` validated
                // `tail + total <= cap`.
                unsafe {
                    if total + MAX_WILDCOPY_OVERSHOOT > cap - tail {
                        // Tight tail: literal+match fit exactly but the
                        // wildcopy overshoot would write past `cap`. Shared
                        // exact, non-overshooting copy.
                        $crate::decoding::exec_sequence_inline::exec_sequence_bounded_copy(
                            base,
                            tail,
                            lit_src_v,
                            lit_length_v,
                            offset_v,
                            match_length_v,
                        );
                    } else {
                        let op_lit = base.add(tail);
                        let op_match = base.add(tail + lit_length_v);
                        let match_src = base.cast_const().add(tail + lit_length_v - offset_v);
                        copy16(op_lit, lit_src_v);
                        if lit_length_v > 16 {
                            wildcopy_no_overlap(
                                op_lit.add(16),
                                lit_src_v.add(16),
                                lit_length_v - 16,
                            );
                        }
                        if offset_v >= 32 {
                            wildcopy_no_overlap_avx2(op_match, match_src, match_length_v);
                        } else if offset_v >= 16 {
                            wildcopy_no_overlap(op_match, match_src, match_length_v);
                        } else {
                            let (op2, ip2) = overlap_copy8(op_match, match_src, offset_v);
                            if match_length_v > 8 {
                                wildcopy_overlap_8byte_stride(op2, ip2, match_length_v - 8);
                            }
                        }
                    }
                }
                Ok(total)
            }
        }
    }};
}
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) use exec_sequence_avx2_inline_at;

/// AVX2-tier body for a sequence whose match lies wholly inside dictionary
/// content, as selected by
/// [`crate::decoding::decode_buffer::DecodeBuffer::dict_match_source`].
/// Upstream copies this case inside `ZSTD_execSequence` too
/// (`zstd_decompress_block.c` 1052-1058): rebase onto the dictionary and, when
/// the match ends before `dictEnd`, one copy and done.
///
/// `$dict_src` is the dictionary from the match's first byte to the end of the
/// dictionary. The match is its first `$match_length` bytes; the rest is the
/// room a wildcopy may overshoot the SOURCE into. There is no overlap to
/// handle — the dictionary is a separate allocation from the output — so the
/// offset plays no part in choosing the copy, only that room does.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
macro_rules! exec_sequence_avx2_dict_inline {
    ($buffer:expr, $lit_src:expr, $lit_length:expr, $dict_src:expr, $match_length:expr) => {{
        use crate::decoding::buffer_backend::sequence_output_fits;
        use crate::decoding::exec_sequence_inline::x86::{
            copy16, wildcopy_no_overlap, wildcopy_no_overlap_avx2,
        };
        const MAX_WILDCOPY_OVERSHOOT: usize = 31;
        let lit_length_v: usize = $lit_length;
        let match_length_v: usize = $match_length;
        let lit_src_v: *const u8 = $lit_src;
        let dict_src_v: &[u8] = $dict_src;
        debug_assert!(dict_src_v.len() >= match_length_v);
        let backend = $buffer.buffer_mut();
        let cap = backend.cap();
        let tail = backend.tail();
        match sequence_output_fits(lit_length_v, match_length_v, tail, cap, 0) {
            Err(e) => Err(e),
            Ok(total) => {
                // SAFETY: as for `exec_sequence_avx2_inline` on the destination
                // side — linear backend, `sequence_output_fits` validated
                // `tail + total <= cap`. The source side is the dictionary
                // slice, valid for reads of its whole length, and every read
                // below is bounded by that length.
                unsafe {
                    let base = backend.inline_exec_base_ptr();
                    let dict_ptr = dict_src_v.as_ptr();
                    let room = dict_src_v.len();
                    if total + MAX_WILDCOPY_OVERSHOOT > cap - tail {
                        // Tight tail: an overshoot would run past the output.
                        // Copy both halves exactly; the dictionary source
                        // cannot overlap the destination.
                        let op = base.add(tail);
                        core::ptr::copy_nonoverlapping(lit_src_v, op, lit_length_v);
                        core::ptr::copy_nonoverlapping(
                            dict_ptr,
                            op.add(lit_length_v),
                            match_length_v,
                        );
                    } else {
                        let op_lit = base.add(tail);
                        let op_match = base.add(tail + lit_length_v);
                        copy16(op_lit, lit_src_v);
                        if lit_length_v > 16 {
                            wildcopy_no_overlap(
                                op_lit.add(16),
                                lit_src_v.add(16),
                                lit_length_v - 16,
                            );
                        }
                        // A wildcopy reads up to the match length rounded up to
                        // its stride; past the dictionary's end there is nothing
                        // to read, so that case copies exactly.
                        if room >= match_length_v.next_multiple_of(32) {
                            wildcopy_no_overlap_avx2(op_match, dict_ptr, match_length_v);
                        } else if room >= match_length_v.next_multiple_of(16) {
                            wildcopy_no_overlap(op_match, dict_ptr, match_length_v);
                        } else {
                            core::ptr::copy_nonoverlapping(dict_ptr, op_match, match_length_v);
                        }
                    }
                    backend.inline_exec_commit(tail + total);
                }
                Ok(())
            }
        }
    }};
}
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) use exec_sequence_avx2_dict_inline;

/// SSE2 twin of [`exec_sequence_avx2_inline`] for the BMI2 tier (which has
/// no AVX2): 16-byte xmm match-copy only (`offset >= 16`), so the WILDCOPY
/// destination overshoot stays 15 bytes (vs 31 for the ymm path). Mirrors
/// the [`BufferBackend::exec_sequence_inline`] trait method body, which
/// remains the unit-tested reference spec. Usable from any fn carrying
/// `target_feature(bmi2)`; baseline SSE2 needs no feature gate on x86_64.
//
// Gated on `kernel-bmi2` so the macro is absent when its only consumer
// (`seq_decoder_bmi2`) is compiled out, keeping `--no-default-features`
// (`-D warnings`) free of an unused-macro error.
#[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
macro_rules! exec_sequence_sse2_inline {
    ($buffer:expr, $lit_src:expr, $lit_length:expr, $offset:expr, $match_length:expr) => {{
        use crate::decoding::buffer_backend::sequence_output_fits;
        use crate::decoding::exec_sequence_inline::x86::{
            copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride,
        };
        const MAX_WILDCOPY_OVERSHOOT: usize = 15;
        let lit_length_v: usize = $lit_length;
        let offset_v: usize = $offset;
        let match_length_v: usize = $match_length;
        let lit_src_v: *const u8 = $lit_src;
        let backend = $buffer.buffer_mut();
        let cap = backend.cap();
        let tail = backend.tail();
        // Hard guard with `overshoot = 0`; the <=15-byte wildcopy slack is
        // handled by the tight-tail branch below so an exact-fit output
        // slice stays correct (see the AVX2 twin for rationale).
        match sequence_output_fits(lit_length_v, match_length_v, tail, cap, 0) {
            Err(e) => Err(e),
            Ok(total) => {
                // SAFETY: inline path gated on `B::SUPPORTS_INLINE_SEQUENCE_EXEC`
                // (linear backend, overrides the accessors);
                // `sequence_output_fits` validated `tail + total <= cap`.
                // All copy primitives are SSE2 baseline (no target_feature).
                unsafe {
                    let base = backend.inline_exec_base_ptr();
                    if total + MAX_WILDCOPY_OVERSHOOT > cap - tail {
                        // Tight tail: shared exact, non-overshooting copy.
                        $crate::decoding::exec_sequence_inline::exec_sequence_bounded_copy(
                            base,
                            tail,
                            lit_src_v,
                            lit_length_v,
                            offset_v,
                            match_length_v,
                        );
                    } else {
                        let op_lit = base.add(tail);
                        let op_match = base.add(tail + lit_length_v);
                        let match_src = base.cast_const().add(tail + lit_length_v - offset_v);
                        copy16(op_lit, lit_src_v);
                        if lit_length_v > 16 {
                            wildcopy_no_overlap(
                                op_lit.add(16),
                                lit_src_v.add(16),
                                lit_length_v - 16,
                            );
                        }
                        if offset_v >= 16 {
                            wildcopy_no_overlap(op_match, match_src, match_length_v);
                        } else {
                            let (op2, ip2) = overlap_copy8(op_match, match_src, offset_v);
                            if match_length_v > 8 {
                                wildcopy_overlap_8byte_stride(op2, ip2, match_length_v - 8);
                            }
                        }
                    }
                    backend.inline_exec_commit(tail + total);
                }
                Ok(())
            }
        }
    }};
}
#[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
pub(crate) use exec_sequence_sse2_inline;

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

    /// Upstream zstd's `ZSTD_copy16`: one unaligned 16-byte SIMD store.
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

    /// Upstream zstd's `ZSTD_wildcopy(..., ZSTD_no_overlap)`: 16-byte SIMD
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

    /// Upstream zstd's `ZSTD_wildcopy(..., ZSTD_overlap_src_before_dst)` for
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

    /// Upstream zstd's `ZSTD_overlapCopy8`
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
        // dec32table / dec64table — upstream zstd's two precomputed lookup
        // tables for the offset < 8 spread step.
        const DEC32_TABLE: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        const DEC64_TABLE: [i32; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        unsafe {
            if offset < 8 {
                // Read 4 bytes, advance src by dec32, read 4 more bytes,
                // then back-advance by dec64 — see upstream zstd source.
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
                // (upstream zstd's literal C transcription) produces an
                // intermediate pointer below the allocation base
                // when `dec32 < sub2` — true for every offset ∈ 1..=7
                // in upstream zstd's tables — which is UB under Rust's
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
/// target can use them. On aarch64 LLVM lowers the 16-byte `u128`
/// load/store to a single NEON `ldr q`/`str q`; elsewhere it picks the
/// widest available move. The `cfg(not(x86_64))` arms of
/// `FlatBuf`/`UserSliceBackend::exec_sequence_inline` use these to get
/// the upstream zstd `ZSTD_execSequence` shape the x86 path already has, instead
/// of the slow `try_push` + `repeat` chain.
///
/// How the inline path is reached per target:
/// - aarch64: `detect_cpu_kernel` -> `Neon` -> the generic pipelined
///   executor (`execute_one_sequence_pipelined`), which calls
///   `exec_sequence_inline` when the backend opts in.
/// - i686 / riscv / wasm: `detect_cpu_kernel` -> `Scalar` ->
///   `seq_decoder_scalar`, whose execute body routes through that same
///   `execute_one_sequence_pipelined`, so the inline path is reached in
///   scalar-tier production dispatch too.
///
/// Both `FlatBuf` and `UserSliceBackend` set
/// `SUPPORTS_INLINE_SEQUENCE_EXEC = true` on every target; `RingBuffer`
/// keeps it `false` and stays on the wrap-aware fallback. x86_64 uses
/// the SSE2 [`x86`] module for production, so this module is gated out
/// there in non-test builds to avoid two definitions; it is still
/// compiled under `cfg(test)` on x86_64 so the architecture-independent
/// helpers are exercised on the main x86 CI lane, not only the i686
/// shard.
#[cfg(any(not(target_arch = "x86_64"), test))]
pub(crate) mod portable {
    /// Upstream zstd `ZSTD_copy16`: one unaligned 16-byte move.
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

    /// Upstream zstd `ZSTD_wildcopy(..., ZSTD_no_overlap)`: 16-byte loop until at
    /// least `length` bytes written. May overshoot up to 15 bytes past
    /// `dst + length`; caller's `WILDCOPY_OVERLENGTH` slack accommodates.
    ///
    /// # Safety
    /// `dst` writable for `length + 15`; `src` readable for `length + 15`;
    /// no-overlap (`dst` and `src` regions disjoint, upstream zstd semantics).
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

    /// Upstream zstd `ZSTD_wildcopy(..., ZSTD_overlap_src_before_dst)` 8-byte arm:
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

    /// Upstream zstd `ZSTD_overlapCopy8`: copies 8 bytes and, for `offset < 8`,
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
mod inline_helper_tests;

// Parallel coverage for the portable helpers (non-x86 targets). Mirrors
// `inline_helper_tests` exactly: the portable module is the unsafe
// pointer-copy backend the non-x86 `exec_sequence_inline` arms rely on,
// so it must carry the same exact-copy / overshoot / short-offset-spread
// assertions as the SSE2 helpers it byte-for-byte mirrors. On the host
// CI matrix this runs under the i686-unknown-linux-gnu test job.
// Runs on ALL targets (the `portable` module is compiled under
// `cfg(test)` on x86_64 too), so the architecture-independent helpers are
// covered on the main x86 CI lane as well as the i686 job.
#[cfg(test)]
mod portable_helper_tests;

// The dictionary copier, on both of the copy shapes it chooses between and on
// every tier the host can run, each checked against the bytes the sequence
// defines rather than against another tier alone.
#[cfg(test)]
mod dict_copy_tests;
