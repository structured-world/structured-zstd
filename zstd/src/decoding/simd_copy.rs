#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m128i, __m256i, __m512i, _mm_loadu_si128, _mm_storeu_si128, _mm256_loadu_si256,
    _mm256_storeu_si256, _mm512_loadu_si512, _mm512_storeu_si512,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m128i, __m256i, __m512i, _mm_loadu_si128, _mm_storeu_si128, _mm256_loadu_si256,
    _mm256_storeu_si256, _mm512_loadu_si512, _mm512_storeu_si512,
};
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use std::arch::is_x86_feature_detected;
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use std::sync::OnceLock;

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
use core::arch::aarch64::{uint8x16_t, vld1q_u8, vst1q_u8};

/// Copies at least `copy_at_least` bytes from `src` to `dst`.
///
/// This helper may over-copy up to the chunk size of the chosen SIMD/scalar
/// kernel (16, 32, or 64 bytes — at most chunk_size - 1 extra bytes), mirroring
/// zstd wildcopy semantics for faster inner loops.
///
/// # Safety
/// Caller must guarantee:
/// - `src.0` points to at least `src.1` readable bytes.
/// - `dst.0` points to at least `dst.1` writable bytes.
/// - `copy_at_least <= src.1` and `copy_at_least <= dst.1`.
/// - `src.1` and `dst.1` are large enough for the selected kernel:
///   if `min(src.1, dst.1) >= copy_at_least` rounded up to the chunk size,
///   the SIMD/scalar chunk loop may copy that rounded-up amount.
///   Otherwise the function copies exactly `copy_at_least` bytes.
/// - Source and destination regions do not overlap.
#[inline(always)]
pub(crate) unsafe fn copy_bytes_overshooting(
    src: (*const u8, usize),
    dst: (*mut u8, usize),
    copy_at_least: usize,
) {
    if copy_at_least == 0 {
        return;
    }

    let min_buffer_size = core::cmp::min(src.1, dst.1);

    // Single-op fast path: for any copy_at_least in 1..=16 with 16 bytes of
    // slack on both sides, one vector store covers the request. Match copies
    // with offset 8..15 funnel into repeat_in_chunks → here as 8..15-byte
    // calls, and the previous chunk-loop dispatcher paid a function-call +
    // loop-setup cost on every one of them. The single-op path collapses
    // that to one load + one store, which is the donor wildcopy pattern.
    if copy_at_least <= 16 && min_buffer_size >= 16 {
        unsafe { single_op_copy_16(src.0, dst.0, copy_at_least) };
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // Exact-length tail path: when the caller has no WILDCOPY_OVERLENGTH
    // slack (e.g. RingBuffer call sites where dst.1 ends at `head`), the
    // single-op fast path above falls through and the chunked SIMD kernels
    // below also bail (`rounded > min_buffer_size`), leaving libc memmove
    // as the only option. memmove was 24% of decode CPU on the profiled
    // scenario. Replace it with inline byte / overlapping-u64 ops for
    // copies up to 16 bytes — these write EXACTLY `copy_at_least` bytes
    // without any overshoot, which is the contract the slack-less call
    // sites require.
    if copy_at_least <= 16 {
        // SAFETY: `copy_at_least <= min(src.1, dst.1)` by this function's
        // contract, so both branches read/write strictly within the
        // caller's reported readable / writable spans.
        unsafe {
            if copy_at_least <= 8 {
                // Byte-by-byte for 1..=8 bytes. The fixed-size loop unrolls
                // into a sequence of immediate-offset loads/stores on every
                // sane backend, so for the common 1..=8 case this is
                // typically 2-3 cycles inline vs the ~10+ cycle call into
                // libc memmove the previous fallback paid.
                let mut i = 0;
                while i < copy_at_least {
                    dst.0.add(i).write(src.0.add(i).read());
                    i += 1;
                }
            } else {
                // 9..=16 bytes via two overlapping unaligned u64 ops. The
                // overlap region is written twice with the same source
                // bytes, so the net effect is exactly `copy_at_least` bytes
                // copied — no overshoot past dst.0 + copy_at_least.
                let lo: u64 = src.0.cast::<u64>().read_unaligned();
                let hi_offset = copy_at_least - 8;
                let hi: u64 = src.0.add(hi_offset).cast::<u64>().read_unaligned();
                dst.0.cast::<u64>().write_unaligned(lo);
                dst.0.add(hi_offset).cast::<u64>().write_unaligned(hi);
            }
        }
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // Chunked SIMD fast paths for larger copies. Each branch consults the
    // appropriate feature-detection mechanism (cached runtime detect under
    // std, compile-time target_feature otherwise) and falls through on miss
    // so a single dispatcher covers every arch + feature combination.
    macro_rules! try_chunk_kernel {
        ($chunk:expr, $kernel:ident) => {{
            if copy_at_least >= $chunk {
                let rounded = copy_at_least.next_multiple_of($chunk);
                if min_buffer_size >= rounded {
                    unsafe { $kernel(src.0, dst.0, rounded) };
                    debug_assert_eq_copy(src, dst, copy_at_least);
                    return;
                }
            }
        }};
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        let caps = detect_x86_caps();
        if caps.avx512f {
            try_chunk_kernel!(64, copy_avx512);
        }
        if caps.avx2 {
            try_chunk_kernel!(32, copy_avx2);
        }
        if caps.sse2 {
            try_chunk_kernel!(16, copy_sse2);
        }
    }

    #[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
    {
        #[cfg(target_feature = "avx512f")]
        try_chunk_kernel!(64, copy_avx512);
        #[cfg(target_feature = "avx2")]
        try_chunk_kernel!(32, copy_avx2);
        #[cfg(target_feature = "sse2")]
        try_chunk_kernel!(16, copy_sse2);
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    try_chunk_kernel!(16, copy_neon);

    // Final fallback: scalar 8-byte chunk loop if alignment permits, else
    // an exact byte copy. Inlined directly to avoid the per-call dispatcher
    // overhead the previous CopyFn function-pointer abstraction imposed.
    let scalar_chunk = core::mem::size_of::<usize>();
    let rounded = copy_at_least.next_multiple_of(scalar_chunk);
    if min_buffer_size >= rounded {
        unsafe { copy_scalar(src.0, dst.0, rounded) };
    } else {
        unsafe { dst.0.copy_from_nonoverlapping(src.0, copy_at_least) };
    }
    debug_assert_eq_copy(src, dst, copy_at_least);
}

/// Single 16-byte transfer covering any 1..=16 byte request. The caller
/// guarantees 16 bytes of readable / writable slack on both sides so a full
/// vector store is safe even when only the first `len` bytes are required —
/// trailing bytes are written but the caller treats them as wildcopy overshoot.
///
/// # Safety
/// `src` and `dst` must each point to at least 16 readable / writable bytes;
/// regions must not overlap.
#[inline(always)]
unsafe fn single_op_copy_16(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(len <= 16);
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        let v: uint8x16_t = vld1q_u8(src);
        vst1q_u8(dst, v);
        return;
    }
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    unsafe {
        if detect_x86_caps().sse2 {
            copy_sse2(src, dst, 16);
            return;
        }
    }
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2"
    ))]
    unsafe {
        copy_sse2(src, dst, 16);
        return;
    }
    // Portable fallback: two overlapping unaligned u64 writes cover 1..=16
    // bytes. Still cheaper than the scalar-strategy loop + indirect call the
    // previous dispatcher imposed on every small copy.
    #[allow(unreachable_code)]
    unsafe {
        let lo: u64 = src.cast::<u64>().read_unaligned();
        let hi_offset = len.saturating_sub(8);
        let hi: u64 = src.add(hi_offset).cast::<u64>().read_unaligned();
        dst.cast::<u64>().write_unaligned(lo);
        dst.add(hi_offset).cast::<u64>().write_unaligned(hi);
    }
}

#[inline(always)]
fn debug_assert_eq_copy(_src: (*const u8, usize), _dst: (*mut u8, usize), _len: usize) {
    #[cfg(debug_assertions)]
    unsafe {
        let s = core::slice::from_raw_parts(_src.0, _len);
        let d = core::slice::from_raw_parts(_dst.0, _len);
        debug_assert_eq!(s, d);
    }
}

/// Bench-only entrypoint for evaluating alternative copy kernels against the
/// production overshooting wildcopy implementation.
///
/// # Safety
/// Caller must satisfy the same requirements as [`copy_bytes_overshooting`]:
/// source and destination pointers must be valid for reads/writes of at least
/// `copy_at_least` bytes, support any rounded-up overshoot implied by the
/// active copy strategy when capacities permit it, and must not overlap.
#[cfg(feature = "bench_internals")]
#[inline(always)]
pub(crate) unsafe fn copy_bytes_overshooting_for_bench(
    src: (*const u8, usize),
    dst: (*mut u8, usize),
    copy_at_least: usize,
) {
    // Keep an explicit unsafe block here because the crate enforces
    // `unsafe_op_in_unsafe_fn` under `-D warnings`.
    unsafe { copy_bytes_overshooting(src, dst, copy_at_least) };
}

/// Active chunk size for the chunk-loop dispatcher on this build. Used by
/// `RingBuffer` tests to size scenarios that exercise single-chunk,
/// multi-chunk, and capacity-tight (`chunk + 1`) copy shapes — keeping the
/// tests architecture-agnostic.
#[cfg(test)]
#[inline]
pub(crate) fn active_chunk_size_for_tests() -> usize {
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        let caps = detect_x86_caps();
        if caps.avx512f {
            return 64;
        }
        if caps.avx2 {
            return 32;
        }
        if caps.sse2 {
            return 16;
        }
    }
    #[cfg(all(not(feature = "std"), target_feature = "avx512f"))]
    {
        return 64;
    }
    #[cfg(all(not(feature = "std"), target_feature = "avx2"))]
    {
        return 32;
    }
    #[cfg(all(not(feature = "std"), target_feature = "sse2"))]
    {
        return 16;
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        return 16;
    }
    #[allow(unreachable_code)]
    {
        core::mem::size_of::<usize>()
    }
}

#[inline(always)]
unsafe fn copy_scalar(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            dst.cast::<usize>()
                .write_unaligned(src.cast::<usize>().read_unaligned());
            src = src.add(core::mem::size_of::<usize>());
            dst = dst.add(core::mem::size_of::<usize>());
        }
    }
}

#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
#[derive(Clone, Copy)]
struct X86Caps {
    avx512f: bool,
    avx2: bool,
    sse2: bool,
}

#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
fn detect_x86_caps() -> X86Caps {
    static CAPS: OnceLock<X86Caps> = OnceLock::new();
    *CAPS.get_or_init(|| X86Caps {
        avx512f: is_x86_feature_detected!("avx512f"),
        avx2: is_x86_feature_detected!("avx2"),
        sse2: is_x86_feature_detected!("sse2"),
    })
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn copy_sse2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            let v: __m128i = _mm_loadu_si128(src.cast::<__m128i>());
            _mm_storeu_si128(dst.cast::<__m128i>(), v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn copy_avx2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            let v: __m256i = _mm256_loadu_si256(src.cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), v);
            src = src.add(32);
            dst = dst.add(32);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
unsafe fn copy_avx512(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            let v: __m512i = _mm512_loadu_si512(src.cast::<__m512i>());
            _mm512_storeu_si512(dst.cast::<__m512i>(), v);
            src = src.add(64);
            dst = dst.add(64);
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
#[inline(always)]
unsafe fn copy_neon(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            let v: uint8x16_t = vld1q_u8(src);
            vst1q_u8(dst, v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn copy_bytes_overshooting_zero_len_is_noop() {
        let src = [1_u8, 2, 3, 4];
        let mut dst = [9_u8, 9, 9, 9];
        unsafe {
            copy_bytes_overshooting((src.as_ptr(), src.len()), (dst.as_mut_ptr(), dst.len()), 0);
        }
        assert_eq!(dst, [9_u8, 9, 9, 9]);
    }

    #[test]
    fn copy_bytes_overshooting_fallback_exact_copy_when_caps_are_tight() {
        // Pick a size that exceeds the single-op fast path threshold (16)
        // and the next chunk size on every supported arch, so the fallback
        // path is exercised regardless of which kernel a given build picks.
        let len = 65; // > AVX-512 chunk
        let src = vec![5_u8; len];
        let mut dst = vec![0_u8; len];

        unsafe {
            copy_bytes_overshooting((src.as_ptr(), len), (dst.as_mut_ptr(), len), len);
        }

        assert_eq!(dst, src);
    }

    #[test]
    fn copy_bytes_overshooting_single_op_small() {
        // Sub-16 copy with full 16-byte slack on both sides: single-op fast
        // path covers it via one SIMD store (or two overlapping u64 stores
        // on archs without 128-bit SIMD).
        for len in 1..=16 {
            let mut src = [0u8; 32];
            for (i, b) in src.iter_mut().enumerate() {
                *b = i as u8;
            }
            let mut dst = [0u8; 32];
            unsafe {
                copy_bytes_overshooting((src.as_ptr(), 32), (dst.as_mut_ptr(), 32), len);
            }
            assert_eq!(&dst[..len], &src[..len], "len={len}");
        }
    }

    #[test]
    fn copy_scalar_copies_requested_bytes() {
        let src = [11_u8, 12, 13, 14, 15, 16, 17, 18];
        let mut dst = [0_u8; 8];
        unsafe { copy_scalar(src.as_ptr(), dst.as_mut_ptr(), src.len()) };
        assert_eq!(dst, src);
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn copy_sse2_copies_full_chunk_when_available() {
        if !std::arch::is_x86_feature_detected!("sse2") {
            return;
        }
        let src = [7_u8; 16];
        let mut dst = [0_u8; 16];
        unsafe { copy_sse2(src.as_ptr(), dst.as_mut_ptr(), 16) };
        assert_eq!(dst, src);
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn copy_avx2_copies_full_chunk_when_available() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let src = [8_u8; 32];
        let mut dst = [0_u8; 32];
        unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 32) };
        assert_eq!(dst, src);
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn copy_avx512_copies_full_chunk_when_available() {
        if !std::arch::is_x86_feature_detected!("avx512f") {
            return;
        }
        let src = [9_u8; 64];
        let mut dst = [0_u8; 64];
        unsafe { copy_avx512(src.as_ptr(), dst.as_mut_ptr(), 64) };
        assert_eq!(dst, src);
    }
}
