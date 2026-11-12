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
//!    `ZSTD_overlapCopy8` spreading then wildcopy
//!    (`overlap_src_before_dst`, 8-byte loop while diff < 16,
//!    16-byte once diff catches up).
//!
//! Two helper implementations with an identical byte-level contract, each
//! compiled only on the targets it serves (so neither is linked from here):
//! `x86` uses SSE2 intrinsics (`_mm_loadu/storeu_si128`, the x86_64
//! baseline); `portable` uses unaligned `u128`/`u64` moves that the
//! backend lowers to its widest store (NEON `ldr q`/`str q` on aarch64,
//! plain movs on i686/riscv/wasm). The backend's `exec_sequence_inline`
//! arm picks one by `cfg(target_arch)`. Backends gate the whole path on
//! `SUPPORTS_INLINE_SEQUENCE_EXEC` (`true` for `UserSliceBackend` /
//! `FlatBuf` on every target, `false` for `RingBuffer`, which stays on
//! the `extend` + `repeat` fallback for wrap-aware multi-segment frames).
//! See the `portable` module doc for how the inline path is reached
//! per target.

/// Most bytes a wildcopy body may write past the sequence it was asked for: its
/// widest stride less one. A destination with at least this much room after the
/// write can take the overshooting copiers; anything tighter takes the exact one.
///
/// Gated to the tier that reads it (the AVX2 sequence loop, which keeps the
/// output end less this value in its cursor), so the builds without that tier
/// do not carry an unused constant.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) const MAX_WILDCOPY_OVERSHOOT: usize = 31;

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
            // Overlapping LZ copy. The match repeats a period of `offset`
            // bytes, so copy that period once and then double what is already
            // written: every step is a non-overlapping block copy, and the
            // block doubles each time, so even a period of one byte fills the
            // match in block moves rather than one byte at a time. Upstream
            // reaches the same place differently (`ZSTD_overlapCopy8` spreads
            // the period to eight and its wildcopy takes it from there,
            // zstd_decompress_block.c:804-824); what matters is that neither
            // walks the match byte by byte, which on a long match with a short
            // offset costs a factor.
            let period = offset.min(match_length);
            core::ptr::copy_nonoverlapping(match_src, op_match, period);
            let mut filled = period;
            while filled < match_length {
                let chunk = filled.min(match_length - filled);
                core::ptr::copy_nonoverlapping(op_match.cast_const(), op_match.add(filled), chunk);
                filled += chunk;
            }
        }
    }
}

/// Textual expansion of the AVX2 `ZSTD_execSequence` body at the call site,
/// fusing the match-copy into a per-tier sequence monolith, addressed by an
/// explicit cursor rather than by asking the backend where it is.
///
/// A `#[target_feature(avx2)]` function cannot be `#[inline(always)]`
/// (rust#145574), so the
/// [`BufferBackend::exec_sequence_inline_avx2`](super::buffer_backend::BufferBackend::exec_sequence_inline_avx2) trait
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
    ($base:expr, $tail:expr, $cap:expr, $cap_w:expr, $lit_src:expr, $lit_length:expr, $offset:expr, $match_length:expr) => {{
        use crate::decoding::buffer_backend::sequence_output_fits;
        use crate::decoding::exec_sequence_inline::x86::{
            copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride,
        };
        let lit_length_v: usize = $lit_length;
        let offset_v: usize = $offset;
        let match_length_v: usize = $match_length;
        let lit_src_v: *const u8 = $lit_src;
        let base: *mut u8 = $base;
        let cap: usize = $cap;
        let cap_w: usize = $cap_w;
        let tail: usize = $tail;
        // One branch decides the hot path, as upstream's `oMatchEnd > oend_w`:
        // `cap_w` is the end with the overshoot already taken off, computed
        // once per block. Both lengths are bounded by a block's FSE expansion,
        // so the sum cannot overflow. Everything else, including whether the
        // write fits at all, belongs to the branch that is almost never taken,
        // A long match is decided after it, on the path that has room for the
        // overshoot, so this gate stays one comparison.
        let total = lit_length_v + match_length_v;
        if tail + total > cap_w {
            sequence_output_fits(lit_length_v, match_length_v, tail, cap, 0).map(|total| {
                // Tight tail: the write fits but the overshoot would not.
                // SAFETY: `sequence_output_fits` holds the sequence within
                // `cap`, `cap_w` is this cursor's overshoot end, and
                // `offset_v` bytes of output precede the match.
                unsafe {
                    $crate::decoding::exec_sequence_inline::x86::exec_tight_tail_avx2(
                        base,
                        tail,
                        cap_w,
                        lit_src_v,
                        lit_length_v,
                        offset_v,
                        match_length_v,
                    );
                }
                total
            })
        } else {
            // SAFETY: the enclosing fn carries
            // `#[target_feature(enable = "...,bmi2,avx2")]`, the caller vouches
            // for `base`, and the comparison above established room for the
            // write and for every byte the wildcopy may overshoot.
            unsafe {
                let op_lit = base.add(tail);
                let op_match = base.add(tail + lit_length_v);
                let match_src = base.cast_const().add(tail + lit_length_v - offset_v);
                copy16(op_lit, lit_src_v);
                if lit_length_v > 16 {
                    wildcopy_no_overlap(op_lit.add(16), lit_src_v.add(16), lit_length_v - 16);
                }
                // Split at `WILDCOPY_VECLEN`, as upstream's `ZSTD_execSequence`
                // (zstd_decompress_block.c:1077), not at the 32-byte vector
                // width: a 32-byte arm would add a data-dependent branch to
                // every match.
                if match_length_v >= 256 {
                    // A long match: a copy from a fixed short distance would read
                    // its own recent stores, so it goes from doubling distances
                    // instead. The gate above leaves room for its 31-byte
                    // overshoot, and `offset_v` bytes of output precede it.
                    $crate::decoding::exec_sequence_inline::x86::match_copy_doubling_avx2(
                        op_match,
                        offset_v,
                        match_length_v,
                    );
                } else if offset_v >= 16 {
                    wildcopy_no_overlap(op_match, match_src, match_length_v);
                } else {
                    // Sixteen bytes of written output before the match, which
                    // the shuffle reads its period from; only the first few
                    // bytes of a buffer lack them.
                    #[cfg(any(feature = "std", target_feature = "ssse3"))]
                    let shuffle = tail + lit_length_v >= 16;
                    // Without std this tier runs only on a build with AVX2
                    // enabled at compile time, so a build without SSSE3 never
                    // reaches it; there the shuffle is left out rather than
                    // compiled for a target that cannot lower it (soft-float).
                    #[cfg(not(any(feature = "std", target_feature = "ssse3")))]
                    let shuffle = false;
                    if shuffle {
                        #[cfg(any(feature = "std", target_feature = "ssse3"))]
                        $crate::decoding::exec_sequence_inline::overlap_match_shuffle!(
                            op_match,
                            offset_v,
                            match_length_v
                        );
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
    }};
}
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) use exec_sequence_avx2_inline_at;

/// A match whose `offset` is below 16 overlaps its own output: its bytes
/// repeat with period `offset`. One byte shuffle spreads that period over a
/// vector and a second rotates it for each further chunk, so every step writes
/// 16 bytes, where the eight-byte copies after upstream's `ZSTD_overlapCopy8`
/// write eight. The period is read from the 16 bytes that END at `$dst`, all of
/// them output already written, so no byte the match has not produced yet is
/// read. May write up to 15 bytes past `$dst + $length`.
///
/// A macro, not a function: `pshufb` needs SSSE3, which only the enclosing
/// `target_feature` function provides, and a standalone body would be compiled
/// without it.
///
/// # Safety
/// `1 <= offset < 16`, the 16 bytes before `dst` are initialized output (so
/// `offset` of them are the match's source), `dst` is writable for `length`
/// rounded up to 16, and the enclosing function enables SSSE3 or a superset.
#[cfg(all(
    target_arch = "x86_64",
    any(feature = "kernel-avx2", test),
    any(feature = "std", target_feature = "ssse3")
))]
macro_rules! overlap_match_shuffle {
    ($dst:expr, $offset:expr, $length:expr) => {{
        use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_shuffle_epi8, _mm_storeu_si128};
        use $crate::decoding::exec_sequence_inline::x86::{PERIOD, ROTATE};
        let dst: *mut u8 = $dst;
        let offset: usize = $offset;
        let length: usize = $length;
        debug_assert!((1..16).contains(&offset));
        debug_assert!(length > 0);
        let source = _mm_loadu_si128(dst.cast_const().sub(16) as *const __m128i);
        let period_mask = _mm_loadu_si128(PERIOD[offset].as_ptr() as *const __m128i);
        let rotate = _mm_loadu_si128(ROTATE[offset].as_ptr() as *const __m128i);
        let mut period = _mm_shuffle_epi8(source, period_mask);
        let mut off = 0usize;
        loop {
            _mm_storeu_si128(dst.add(off) as *mut __m128i, period);
            off += 16;
            if off >= length {
                break;
            }
            period = _mm_shuffle_epi8(period, rotate);
        }
    }};
}
#[cfg(all(
    target_arch = "x86_64",
    any(feature = "kernel-avx2", test),
    any(feature = "std", target_feature = "ssse3")
))]
pub(crate) use overlap_match_shuffle;

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
        const MAX_WILDCOPY_OVERSHOOT: usize = 31;
        let backend = $buffer.buffer_mut();
        let cap = backend.cap();
        let tail = backend.tail();
        // SAFETY: a linear backend (the caller's gate), so its base addresses
        // the output from `tail` on.
        let base = unsafe { backend.inline_exec_base_ptr() };
        // The overshoot end is the backend's own capacity less the stride.
        // Saturating is the meaning: a capacity below the overshoot leaves no
        // room for an overshooting write, and an end of zero sends the
        // sequence to the exact copy.
        let r = $crate::decoding::exec_sequence_inline::exec_sequence_avx2_dict_inline_at!(
            base,
            tail,
            cap,
            cap.saturating_sub(MAX_WILDCOPY_OVERSHOOT),
            $lit_src,
            $lit_length,
            $dict_src,
            $match_length
        );
        r.map(|total| {
            // SAFETY: the copy wrote `[tail, tail + total)`, within `cap`.
            unsafe { backend.inline_exec_commit(tail + total) };
        })
    }};
}
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) use exec_sequence_avx2_dict_inline;

/// [`exec_sequence_avx2_dict_inline!`]'s body, addressed by an explicit cursor
/// as [`exec_sequence_avx2_inline_at!`] is, so the AVX2 sequence loop runs a
/// dictionary match on the position it keeps in locals, without handing it to
/// the buffer and taking it back. Returns the bytes written.
///
/// # Safety
/// The enclosing function must carry `target_feature(avx2)` or the CPU must
/// have it; `base` is the start of a linear output valid for writes through
/// `cap`, and through `cap_w + 31` for an overshooting copy; `lit_src` is
/// readable for the literal length rounded up to 16; `$dict_src` holds the
/// match at its front.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
macro_rules! exec_sequence_avx2_dict_inline_at {
    ($base:expr, $tail:expr, $cap:expr, $cap_w:expr, $lit_src:expr, $lit_length:expr, $dict_src:expr, $match_length:expr) => {{
        use crate::decoding::buffer_backend::sequence_output_fits;
        use crate::decoding::exec_sequence_inline::x86::{
            copy16, wildcopy_no_overlap, wildcopy_no_overlap_avx2,
        };
        let lit_length_v: usize = $lit_length;
        let match_length_v: usize = $match_length;
        let lit_src_v: *const u8 = $lit_src;
        let dict_src_v: &[u8] = $dict_src;
        let base: *mut u8 = $base;
        let tail: usize = $tail;
        debug_assert!(dict_src_v.len() >= match_length_v);
        let total = lit_length_v + match_length_v;
        let dict_ptr = dict_src_v.as_ptr();
        if tail + total > $cap_w {
            sequence_output_fits(lit_length_v, match_length_v, tail, $cap, 0).map(|total| {
                // Tight tail: an overshoot would run past the output. Copy both
                // halves exactly; the dictionary source cannot overlap the
                // destination.
                // SAFETY: `sequence_output_fits` validated `tail + total <= cap`;
                // the dictionary slice holds the match.
                unsafe {
                    let op = base.add(tail);
                    core::ptr::copy_nonoverlapping(lit_src_v, op, lit_length_v);
                    core::ptr::copy_nonoverlapping(dict_ptr, op.add(lit_length_v), match_length_v);
                }
                total
            })
        } else {
            // SAFETY: `tail + total <= cap_w` leaves the write and its overshoot
            // inside the output; the literal source carries the copiers' slack,
            // and every dictionary read is bounded by the slice's length.
            unsafe {
                let room = dict_src_v.len();
                let op_lit = base.add(tail);
                let op_match = base.add(tail + lit_length_v);
                copy16(op_lit, lit_src_v);
                if lit_length_v > 16 {
                    wildcopy_no_overlap(op_lit.add(16), lit_src_v.add(16), lit_length_v - 16);
                }
                // A wildcopy reads up to the match length rounded up to its
                // stride; past the dictionary's end there is nothing to read,
                // so that case copies exactly.
                if room >= match_length_v.next_multiple_of(32) {
                    wildcopy_no_overlap_avx2(op_match, dict_ptr, match_length_v);
                } else if room >= match_length_v.next_multiple_of(16) {
                    wildcopy_no_overlap(op_match, dict_ptr, match_length_v);
                } else {
                    core::ptr::copy_nonoverlapping(dict_ptr, op_match, match_length_v);
                }
            }
            Ok(total)
        }
    }};
}
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) use exec_sequence_avx2_dict_inline_at;

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

    /// `PERIOD[offset][lane] = 16 - offset + lane % offset`: a byte shuffle
    /// with this mask spreads the LAST `offset` bytes of a vector over all
    /// sixteen lanes. `static`, not `const`, so a debug build reads it in place
    /// instead of copying it onto the stack at every use.
    #[cfg(all(
        any(feature = "kernel-avx2", test),
        any(feature = "std", target_feature = "ssse3")
    ))]
    pub(crate) static PERIOD: [[u8; 16]; 16] = {
        let mut table = [[0u8; 16]; 16];
        let mut offset = 1;
        while offset < 16 {
            let mut lane = 0;
            while lane < 16 {
                table[offset][lane] = (16 - offset + lane % offset) as u8;
                lane += 1;
            }
            offset += 1;
        }
        table
    };

    /// `ROTATE[offset][lane] = (lane + 16) % offset`: turns one 16-byte chunk
    /// of a period-`offset` run into the next one.
    #[cfg(all(
        any(feature = "kernel-avx2", test),
        any(feature = "std", target_feature = "ssse3")
    ))]
    pub(crate) static ROTATE: [[u8; 16]; 16] = {
        let mut table = [[0u8; 16]; 16];
        let mut offset = 1;
        while offset < 16 {
            let mut lane = 0;
            while lane < 16 {
                table[offset][lane] = ((lane + 16) % offset) as u8;
                lane += 1;
            }
            offset += 1;
        }
        table
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

    /// A match copied in pieces whose source lies ever farther back: `offset`
    /// bytes from `offset` back, then `2 * offset` bytes from `2 * offset`
    /// back, and so on; one no longer than its offset is a single plain piece.
    /// The output repeats with period `offset`, so any multiple of it is a
    /// valid source distance once that much is written. Read from a fixed
    /// short distance, every load takes
    /// bytes the copy stored a few cycles before, still in the store buffer,
    /// and one straddling two stores cannot be forwarded and waits for them;
    /// doubling the distance moves the source out of that window after the
    /// first piece, as libc memcpy over doubling pieces does. May overshoot up
    /// to 31 bytes past `dst + length`.
    ///
    /// Out of line: it runs on rare long matches, where a call costs nothing
    /// beside the copy, and keeps its loops out of the sequence loop's code.
    ///
    /// # Safety
    /// The CPU has AVX2; `offset >= 1` bytes of output are in place before
    /// `dst`; room for `length + 31` bytes at `dst`.
    // Gated to the tier that calls it, as `MAX_WILDCOPY_OVERSHOOT` is.
    #[cfg(feature = "kernel-avx2")]
    #[target_feature(enable = "avx2")]
    #[inline(never)]
    pub(crate) unsafe fn match_copy_doubling_avx2(dst: *mut u8, offset: usize, length: usize) {
        // Above the bytes a store buffer of a few dozen 32-byte entries holds.
        const DOUBLING_DISTANCE_LIMIT: usize = 2048;
        // A piece this long from a source this far back (a long-distance match,
        // whose source lies beyond the caches) goes, with fast string moves,
        // by `rep movsb`, as libc memcpy moves it (glibc's
        // `__x86_rep_movsb_threshold` for 32-byte vectors). A short-offset
        // match never gets there: its distance stops doubling below it, and
        // the vector loop over a near source outruns `rep movsb`.
        const REP_MOVSB_MIN: usize = 4096;
        let erms = fast_string_moves();
        unsafe {
            let mut written = 0usize;
            let mut distance = offset;
            // Below 32 bytes of distance a vector load would reach past the
            // bytes in place, so those pieces, 31 bytes at most in all, are
            // copied exactly.
            while written < length && distance < 32 {
                let piece = distance.min(length - written);
                let at = dst.add(written);
                copy_short_exact(at, at.sub(distance), piece);
                written += piece;
                distance *= 2;
            }
            while written < length {
                let piece = distance.min(length - written);
                let at = dst.add(written);
                if erms && distance >= REP_MOVSB_MIN && piece >= REP_MOVSB_MIN {
                    // As glibc does, the string move starts on a 64-byte
                    // boundary of the destination, where its microcode runs
                    // fastest; the head goes by two vector moves first.
                    let src = at.sub(distance);
                    copy32_avx2(at, src);
                    copy32_avx2(at.add(32), src.add(32));
                    let head = 64 - (at.addr() & 63);
                    rep_movsb(at.add(head), src.add(head), piece - head);
                } else {
                    copy_back_avx2(at, at.sub(distance), piece);
                }
                written += piece;
                // Past the store buffer's reach the distance stops growing, so
                // the source stays in cache lines the copy just wrote rather
                // than ones a long match left behind in a farther level; a
                // fixed distance stays valid, as every piece writes as much as
                // it reads. Below that, after this piece `written + offset`
                // bytes of the period run are in place, which is the next
                // distance.
                if distance < DOUBLING_DISTANCE_LIMIT {
                    distance *= 2;
                }
            }
        }
    }

    /// A sequence whose bytes end within the output but whose overshoot would
    /// not. When its literals end within `cap_w` and its match comes from 32
    /// bytes back or more, the match goes by the overshooting copies up to
    /// `cap_w`, whose overshoot stays inside the output's memory, and only the
    /// rest, under 32 bytes, by an exact copy, as upstream's `ZSTD_safecopy`
    /// wildcopies to `oend_w` and finishes byte by byte
    /// (zstd_decompress_block.c, ZSTD_execSequenceEnd). Anything else takes
    /// the exact copy whole. The last match of a frame filling its buffer
    /// exactly is the case: it would otherwise go by libc memcpy, a call per
    /// doubled piece.
    ///
    /// Out of line: it runs once at the end of such a frame.
    ///
    /// # Safety
    /// The CPU has AVX2; `base` is valid for writes through
    /// `tail + lit_length + match_length`, which ends past `cap_w`, and
    /// through `cap_w + 31`; `lit_src` is readable for the literal length
    /// rounded up to 16; `offset <= tail + lit_length`.
    #[cfg(feature = "kernel-avx2")]
    #[target_feature(enable = "avx2")]
    #[inline(never)]
    pub(crate) unsafe fn exec_tight_tail_avx2(
        base: *mut u8,
        tail: usize,
        cap_w: usize,
        lit_src: *const u8,
        lit_length: usize,
        offset: usize,
        match_length: usize,
    ) {
        unsafe {
            let match_at = tail + lit_length;
            // The match runs past `cap_w` (the caller's branch); what lies past
            // it is the rest, at most the 31 bytes between `cap_w` and the end
            // of an output whose memory reaches its write limit, and below the
            // offset, so its exact copy reads no byte it writes.
            if offset < 32 || match_at > cap_w || match_at + match_length - cap_w >= 32 {
                return super::exec_sequence_bounded_copy(
                    base,
                    tail,
                    lit_src,
                    lit_length,
                    offset,
                    match_length,
                );
            }
            let op_lit = base.add(tail);
            copy16(op_lit, lit_src);
            if lit_length > 16 {
                wildcopy_no_overlap(op_lit.add(16), lit_src.add(16), lit_length - 16);
            }
            let head = cap_w - match_at;
            let rest = match_length - head;
            let op_match = base.add(match_at);
            if head >= 256 {
                match_copy_doubling_avx2(op_match, offset, head);
            } else if head > 0 {
                wildcopy_no_overlap(op_match, op_match.sub(offset), head);
            }
            let at = op_match.add(head);
            copy_short_exact(at, at.sub(offset), rest);
        }
    }

    /// Whether the CPU has fast string moves (ERMS), detected once.
    #[cfg(all(feature = "kernel-avx2", feature = "std"))]
    #[inline]
    fn fast_string_moves() -> bool {
        use core::sync::atomic::{AtomicU8, Ordering};
        // 0 unknown, 1 absent, 2 present.
        static ERMS: AtomicU8 = AtomicU8::new(0);
        match ERMS.load(Ordering::Relaxed) {
            0 => {
                let present = std::arch::is_x86_feature_detected!("ermsb");
                ERMS.store(if present { 2 } else { 1 }, Ordering::Relaxed);
                present
            }
            state => state == 2,
        }
    }

    // no-std: a compile-time `target_feature = "ermsb"` would stand in for the
    // run-time check; without one the vector copy serves every piece.
    #[cfg(all(feature = "kernel-avx2", not(feature = "std")))]
    #[inline]
    fn fast_string_moves() -> bool {
        false
    }

    /// `n` bytes from `src` to `dst` by `rep movsb`.
    ///
    /// # Safety
    /// `src + n <= dst`; both valid for `n` bytes.
    #[cfg(feature = "kernel-avx2")]
    #[inline(always)]
    unsafe fn rep_movsb(dst: *mut u8, src: *const u8, n: usize) {
        // SAFETY: the caller's contract; the direction flag is clear at every
        // call boundary (System V and Windows x64), so the move runs forward,
        // and it touches no flags.
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") n => _,
                inout("rdi") dst => _,
                inout("rsi") src => _,
                options(nostack, preserves_flags),
            );
        }
    }

    /// `n < 32` bytes from `src` to `dst`, exactly: two overlapping moves of
    /// the widest size that fits, or bytes for the last three.
    ///
    /// # Safety
    /// `src + n <= dst`; both valid for `n` bytes.
    #[cfg(feature = "kernel-avx2")]
    #[inline(always)]
    unsafe fn copy_short_exact(dst: *mut u8, src: *const u8, n: usize) {
        debug_assert!(n < 32);
        unsafe {
            if n >= 16 {
                copy16(dst, src);
                copy16(dst.add(n - 16), src.add(n - 16));
            } else if n >= 8 {
                let head = src.cast::<u64>().read_unaligned();
                let tail = src.add(n - 8).cast::<u64>().read_unaligned();
                dst.cast::<u64>().write_unaligned(head);
                dst.add(n - 8).cast::<u64>().write_unaligned(tail);
            } else if n >= 4 {
                let head = src.cast::<u32>().read_unaligned();
                let tail = src.add(n - 4).cast::<u32>().read_unaligned();
                dst.cast::<u32>().write_unaligned(head);
                dst.add(n - 4).cast::<u32>().write_unaligned(tail);
            } else {
                for i in 0..n {
                    *dst.add(i) = *src.add(i);
                }
            }
        }
    }

    /// `length` bytes from `src` to `dst`, reading only `src..dst`, in libc
    /// memcpy's shape: past 128 bytes, 128 per pass with the stores on 32-byte
    /// boundaries and the last 128 bytes written to end exactly at
    /// `dst + length`, so the pass count stays short enough for the loop exit
    /// to be predicted; from 32 to 128 bytes, 32 at a time with the last move
    /// ending exactly at the end. Below 32 bytes one move, which the distance
    /// keeps within the bytes in place and which writes up to 31 bytes past
    /// the end, overshoot the caller overwrites or leaves.
    ///
    /// # Safety
    /// Caller in `target_feature(avx2)` scope; `src + 32 <= dst` and
    /// `src + length <= dst`; room for `length + 31` bytes at `dst`.
    #[cfg(feature = "kernel-avx2")]
    #[inline(always)]
    unsafe fn copy_back_avx2(dst: *mut u8, src: *const u8, length: usize) {
        unsafe {
            copy32_avx2(dst, src);
            if length <= 128 {
                if length > 32 {
                    let mut off = 32usize;
                    while off + 32 < length {
                        copy32_avx2(dst.add(off), src.add(off));
                        off += 32;
                    }
                    copy32_avx2(dst.add(length - 32), src.add(length - 32));
                }
                return;
            }
            let tail = length - 128;
            let mut off = 32 - (dst.addr() & 31);
            while off < tail {
                copy32_avx2(dst.add(off), src.add(off));
                copy32_avx2(dst.add(off + 32), src.add(off + 32));
                copy32_avx2(dst.add(off + 64), src.add(off + 64));
                copy32_avx2(dst.add(off + 96), src.add(off + 96));
                off += 128;
            }
            copy32_avx2(dst.add(tail), src.add(tail));
            copy32_avx2(dst.add(tail + 32), src.add(tail + 32));
            copy32_avx2(dst.add(tail + 64), src.add(tail + 64));
            copy32_avx2(dst.add(tail + 96), src.add(tail + 96));
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
/// to the `x86` module, expressed with `read_unaligned`/`write_unaligned` so any
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
/// the SSE2 `x86` module for production, so this module is gated out
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
            core::ptr::copy_nonoverlapping(src, dst, 16);
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
    /// port of `x86::overlap_copy8` (same dec32/dec64 tables and
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
