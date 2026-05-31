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

/// Diagnostic-only copy-shape histogram. Compiled out unless the
/// `copy_shape_stats` feature is on, so production / bench builds carry
/// zero cost. Buckets mirror the dispatch thresholds in
/// [`copy_bytes_overshooting`] so the captured distribution lines up with
/// which code path each call took. Counts are deterministic from the
/// compressed input (same on every CPU tier); only per-call timing is
/// architecture-specific.
#[cfg(feature = "copy_shape_stats")]
pub mod shape_stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    pub static CALLS_LE8: AtomicU64 = AtomicU64::new(0);
    pub static CALLS_9_16: AtomicU64 = AtomicU64::new(0);
    pub static CALLS_17_32: AtomicU64 = AtomicU64::new(0);
    pub static CALLS_GT32: AtomicU64 = AtomicU64::new(0);
    /// Sum of `copy_at_least` over the `>32` bucket (bytes the caller asked for).
    pub static REQ_BYTES_GT32: AtomicU64 = AtomicU64::new(0);
    /// Sum of `copy_at_least.next_multiple_of(32)` over the `>32` bucket
    /// (bytes the chunk kernel actually writes — request + overshoot).
    pub static WRITTEN_BYTES_GT32: AtomicU64 = AtomicU64::new(0);
    /// Largest single `copy_at_least` seen (peak match/literal copy length).
    pub static MAX_LEN: AtomicU64 = AtomicU64::new(0);

    // ── Match-repeat shape (recorded in `DecodeBuffer::repeat_inner`) ──
    // Counts the match-copy calls (NOT literal pushes) by offset bucket,
    // splitting overlapping (offset < match_length) from non-overlapping.
    // The overlapping buckets are the ones C single-passes via
    // `ZSTD_wildcopy` (WILDCOPY_VECLEN=16) but we chunk by `offset` in
    // `repeat_in_chunks`.
    pub static MATCH_NONOVERLAP: AtomicU64 = AtomicU64::new(0);
    pub static MATCH_NONOVERLAP_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Overlapping matches, offset < 8 (period-tiled `repeat_short_offset`).
    pub static MATCH_OVL_LT8: AtomicU64 = AtomicU64::new(0);
    pub static MATCH_OVL_LT8_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Overlapping, 8 <= offset < 16 (chunked, sse2-safe single-pass).
    pub static MATCH_OVL_8_15: AtomicU64 = AtomicU64::new(0);
    pub static MATCH_OVL_8_15_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Overlapping, 16 <= offset < 32 (chunked; C single-passes at VECLEN 16).
    pub static MATCH_OVL_16_31: AtomicU64 = AtomicU64::new(0);
    pub static MATCH_OVL_16_31_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Overlapping, 32 <= offset < 64 (chunked; 32B-vector single-pass safe).
    pub static MATCH_OVL_32_63: AtomicU64 = AtomicU64::new(0);
    pub static MATCH_OVL_32_63_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Overlapping, offset >= 64 (chunked; 64B-unroll single-pass safe).
    pub static MATCH_OVL_GE64: AtomicU64 = AtomicU64::new(0);
    pub static MATCH_OVL_GE64_BYTES: AtomicU64 = AtomicU64::new(0);

    /// Record one match-repeat call: `offset`, `match_length`, and whether
    /// the copy region overlaps the source (`offset < match_length`).
    #[inline]
    pub fn record_repeat(offset: usize, match_length: usize, overlapping: bool) {
        let mlen = match_length as u64;
        if !overlapping {
            MATCH_NONOVERLAP.fetch_add(1, Ordering::Relaxed);
            MATCH_NONOVERLAP_BYTES.fetch_add(mlen, Ordering::Relaxed);
            return;
        }
        let (n, b) = if offset < 8 {
            (&MATCH_OVL_LT8, &MATCH_OVL_LT8_BYTES)
        } else if offset < 16 {
            (&MATCH_OVL_8_15, &MATCH_OVL_8_15_BYTES)
        } else if offset < 32 {
            (&MATCH_OVL_16_31, &MATCH_OVL_16_31_BYTES)
        } else if offset < 64 {
            (&MATCH_OVL_32_63, &MATCH_OVL_32_63_BYTES)
        } else {
            (&MATCH_OVL_GE64, &MATCH_OVL_GE64_BYTES)
        };
        n.fetch_add(1, Ordering::Relaxed);
        b.fetch_add(mlen, Ordering::Relaxed);
    }

    /// Snapshot + reset the match-repeat buckets, returning pairs of
    /// `(count, bytes)` in order: nonoverlap, ovl<8, ovl8-15, ovl16-31,
    /// ovl32-63, ovl>=64.
    pub fn take_repeat() -> [(u64, u64); 6] {
        [
            (
                MATCH_NONOVERLAP.swap(0, Ordering::Relaxed),
                MATCH_NONOVERLAP_BYTES.swap(0, Ordering::Relaxed),
            ),
            (
                MATCH_OVL_LT8.swap(0, Ordering::Relaxed),
                MATCH_OVL_LT8_BYTES.swap(0, Ordering::Relaxed),
            ),
            (
                MATCH_OVL_8_15.swap(0, Ordering::Relaxed),
                MATCH_OVL_8_15_BYTES.swap(0, Ordering::Relaxed),
            ),
            (
                MATCH_OVL_16_31.swap(0, Ordering::Relaxed),
                MATCH_OVL_16_31_BYTES.swap(0, Ordering::Relaxed),
            ),
            (
                MATCH_OVL_32_63.swap(0, Ordering::Relaxed),
                MATCH_OVL_32_63_BYTES.swap(0, Ordering::Relaxed),
            ),
            (
                MATCH_OVL_GE64.swap(0, Ordering::Relaxed),
                MATCH_OVL_GE64_BYTES.swap(0, Ordering::Relaxed),
            ),
        ]
    }

    #[inline]
    pub(super) fn record(copy_at_least: usize) {
        let n = copy_at_least as u64;
        if copy_at_least <= 8 {
            CALLS_LE8.fetch_add(1, Ordering::Relaxed);
        } else if copy_at_least <= 16 {
            CALLS_9_16.fetch_add(1, Ordering::Relaxed);
        } else if copy_at_least <= 32 {
            CALLS_17_32.fetch_add(1, Ordering::Relaxed);
        } else {
            CALLS_GT32.fetch_add(1, Ordering::Relaxed);
            REQ_BYTES_GT32.fetch_add(n, Ordering::Relaxed);
            WRITTEN_BYTES_GT32.fetch_add(
                (copy_at_least.next_multiple_of(32)) as u64,
                Ordering::Relaxed,
            );
        }
        MAX_LEN.fetch_max(n, Ordering::Relaxed);
    }

    /// Snapshot + reset all counters, returning `(le8, 9_16, 17_32, gt32,
    /// req_gt32, written_gt32, max_len)`.
    pub fn take() -> [u64; 7] {
        [
            CALLS_LE8.swap(0, Ordering::Relaxed),
            CALLS_9_16.swap(0, Ordering::Relaxed),
            CALLS_17_32.swap(0, Ordering::Relaxed),
            CALLS_GT32.swap(0, Ordering::Relaxed),
            REQ_BYTES_GT32.swap(0, Ordering::Relaxed),
            WRITTEN_BYTES_GT32.swap(0, Ordering::Relaxed),
            MAX_LEN.swap(0, Ordering::Relaxed),
        ]
    }
}

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

    #[cfg(feature = "copy_shape_stats")]
    shape_stats::record(copy_at_least);

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
    // copies up to 32 bytes — these write EXACTLY `copy_at_least` bytes
    // without any overshoot, which is the contract the slack-less call
    // sites require. 32-byte cap covers the typical literal-push size
    // range (1..=24 bytes seen on the profiled corpus) and stays within
    // a single straight-line block on the I-cache.
    if copy_at_least <= 32 {
        // SAFETY: `copy_at_least <= min(src.1, dst.1)` by this function's
        // contract, so all branches below read/write strictly within the
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
            } else if copy_at_least <= 16 {
                // 9..=16 bytes via two overlapping unaligned u64 ops. The
                // overlap region is written twice with the same source
                // bytes, so the net effect is exactly `copy_at_least` bytes
                // copied — no overshoot past dst.0 + copy_at_least.
                let lo: u64 = src.0.cast::<u64>().read_unaligned();
                let hi_offset = copy_at_least - 8;
                let hi: u64 = src.0.add(hi_offset).cast::<u64>().read_unaligned();
                dst.0.cast::<u64>().write_unaligned(lo);
                dst.0.add(hi_offset).cast::<u64>().write_unaligned(hi);
            } else {
                // 17..=32 bytes: first 16 via two adjacent u64 stores, the
                // trailing 1..=16 via the same overlapping-pair trick.
                // Four loads + four stores total, all branch-free.
                let lo: u64 = src.0.cast::<u64>().read_unaligned();
                let hi: u64 = src.0.add(8).cast::<u64>().read_unaligned();
                dst.0.cast::<u64>().write_unaligned(lo);
                dst.0.add(8).cast::<u64>().write_unaligned(hi);
                let tail_off = copy_at_least - 16;
                let tail_lo: u64 = src.0.add(tail_off).cast::<u64>().read_unaligned();
                let tail_hi: u64 = src.0.add(copy_at_least - 8).cast::<u64>().read_unaligned();
                dst.0.add(tail_off).cast::<u64>().write_unaligned(tail_lo);
                dst.0
                    .add(copy_at_least - 8)
                    .cast::<u64>()
                    .write_unaligned(tail_hi);
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

/// AVX2-tier wildcopy variant: same shape as [`copy_bytes_overshooting`]
/// but the chunked-SIMD path goes DIRECT to `copy_avx2` (32-byte
/// chunks) without consulting `detect_x86_caps()`. Issue #279 round 3
/// Phase 4: when called from inside a target_feature(avx2,bmi2)-scoped
/// caller, the inlined `copy_avx2` body emits `_mm256_storeu_si256`
/// ymm stores directly; the runtime dispatch branch + cached-OnceLock
/// load that `copy_bytes_overshooting` paid per call is gone.
///
/// Small-request paths (≤16 fast, ≤32 exact-length) are identical to
/// the dispatcher version — they don't need a chunk kernel and stay
/// inline. The AVX-512 chunk path is omitted (this variant targets
/// the AVX2-tier scope, which is the strict subset).
///
/// # Safety
/// `src` and `dst` must each point to at least `src.1` / `dst.1`
/// readable / writable bytes, regions must not overlap, and the
/// caller MUST itself be in `target_feature(enable = "avx2,bmi2")`
/// scope.
///
/// # Status
/// Currently unused in production: the AVX2 match-copy inline path
/// in PR #285 routes through `BufferBackend::exec_sequence_inline_avx2`
/// which uses the 32-byte wildcopy helpers in
/// `exec_sequence_inline::x86` directly. This standalone variant is
/// the bottom-layer building block for the next iteration
/// (`perf/#279-r4-1c-avx2-layered-chain`) — it will be wired into the
/// per-tier `repeat_in_chunks_avx2` for the RingBuffer / FlatBuf
/// (non-inline) backend paths. Keep the function until that work
/// lands; remove if the layered-chain experiment ends up not retained.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[allow(dead_code)]
pub(crate) unsafe fn copy_bytes_overshooting_avx2(
    src: (*const u8, usize),
    dst: (*mut u8, usize),
    copy_at_least: usize,
) {
    if copy_at_least == 0 {
        return;
    }

    let min_buffer_size = core::cmp::min(src.1, dst.1);

    if copy_at_least <= 16 && min_buffer_size >= 16 {
        unsafe { single_op_copy_16(src.0, dst.0, copy_at_least) };
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    if copy_at_least <= 32 {
        unsafe {
            if copy_at_least <= 8 {
                let mut i = 0;
                while i < copy_at_least {
                    dst.0.add(i).write(src.0.add(i).read());
                    i += 1;
                }
            } else if copy_at_least <= 16 {
                let lo: u64 = src.0.cast::<u64>().read_unaligned();
                let hi_offset = copy_at_least - 8;
                let hi: u64 = src.0.add(hi_offset).cast::<u64>().read_unaligned();
                dst.0.cast::<u64>().write_unaligned(lo);
                dst.0.add(hi_offset).cast::<u64>().write_unaligned(hi);
            } else {
                let lo: u64 = src.0.cast::<u64>().read_unaligned();
                let hi: u64 = src.0.add(8).cast::<u64>().read_unaligned();
                dst.0.cast::<u64>().write_unaligned(lo);
                dst.0.add(8).cast::<u64>().write_unaligned(hi);
                let tail_off = copy_at_least - 16;
                let tail_lo: u64 = src.0.add(tail_off).cast::<u64>().read_unaligned();
                let tail_hi: u64 = src.0.add(copy_at_least - 8).cast::<u64>().read_unaligned();
                dst.0.add(tail_off).cast::<u64>().write_unaligned(tail_lo);
                dst.0
                    .add(copy_at_least - 8)
                    .cast::<u64>()
                    .write_unaligned(tail_hi);
            }
        }
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // Direct AVX2 chunk path: rounds up to 32-byte multiple, calls
    // copy_avx2 if slack permits. No dispatcher, no detect_x86_caps —
    // target_feature(avx2) on this fn guarantees the kernel is
    // callable.
    let rounded = copy_at_least.next_multiple_of(32);
    if min_buffer_size >= rounded {
        unsafe { copy_avx2(src.0, dst.0, rounded) };
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // Slack-less tail: scalar 8-byte chunk loop if alignment permits,
    // else exact byte copy. Same fallback as `copy_bytes_overshooting`.
    let scalar_chunk = core::mem::size_of::<usize>();
    let rounded_scalar = copy_at_least.next_multiple_of(scalar_chunk);
    if min_buffer_size >= rounded_scalar {
        unsafe { copy_scalar(src.0, dst.0, rounded_scalar) };
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
    //
    // Reachability matrix (kept here so any future arch arm slotted
    // between the existing arms knows it must terminate with `return`
    // or its code will be silently dead):
    //   • aarch64+neon                                 → arm above returns
    //   • std + x86/x86_64 + runtime-SSE2              → arm above returns
    //   • std + x86/x86_64 + NO runtime-SSE2           → reaches here
    //   • no-std + x86/x86_64 + target_feature=sse2    → arm above returns
    //   • no-std + x86/x86_64 + NO target_feature=sse2 → reaches here
    //   • any other arch (riscv64, wasm32, …)          → reaches here
    // Anything new MUST `return` from its own arm before this comment.
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
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "avx512f"
    ))]
    {
        return 64;
    }
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "avx2"
    ))]
    {
        return 32;
    }
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2"
    ))]
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

// `#[allow(dead_code)]` because in `--no-default-features` builds on x86
// without `RUSTFLAGS="-C target-feature=+avx2"` the dispatcher cfg-gates
// out every call site (runtime detection lives behind `feature = "std"`).
// In std builds and target_feature=+avx2 builds the function is live.
//
// Inner loop is unrolled to 2× 32-byte AVX2 vectors per iteration (64
// bytes / iter), with a single-vector tail handling the residual 32
// bytes when `len` is a non-multiple of 64. The dispatcher rounds
// `copy_at_least` up to a multiple of 32 before calling, so `len`
// here is always a multiple of 32 — the loop body handles
// `len & !63` bytes, the tail handles the remaining 0 or 32.
//
// The two independent load / store pairs per iteration expose more
// instruction-level parallelism to the out-of-order core and amortise
// the loop branch, shortening AVX2 wildcopy latency. Actual speed-up
// is workload-dependent — measured in `benches/wildcopy_candidates.rs`
// (criterion micro) and end-to-end via `benches/compare_ffi.rs`.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[allow(dead_code)]
unsafe fn copy_avx2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    debug_assert!(
        len.is_multiple_of(32),
        "copy_avx2 expects len to be a multiple of 32 (dispatcher rounds up)",
    );
    let end_unrolled = len & !63;
    let mut copied = 0usize;
    while copied < end_unrolled {
        unsafe {
            let v0: __m256i = _mm256_loadu_si256(src.cast::<__m256i>());
            let v1: __m256i = _mm256_loadu_si256(src.add(32).cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), v0);
            _mm256_storeu_si256(dst.add(32).cast::<__m256i>(), v1);
            src = src.add(64);
            dst = dst.add(64);
        }
        copied += 64;
    }
    // Residual 32-byte vector when `len` is 32 mod 64.
    if copied < len {
        unsafe {
            let v: __m256i = _mm256_loadu_si256(src.cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), v);
        }
    }
}

// Same `#[allow(dead_code)]` rationale as `copy_avx2`: cfg-gated out in
// no-std builds without `target_feature=+avx512f`, live elsewhere.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
#[allow(dead_code)]
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
        // Single 32-byte vector (no unrolled body, tail-only path).
        let src = [8_u8; 32];
        let mut dst = [0_u8; 32];
        unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 32) };
        assert_eq!(dst, src);
    }

    /// Exercises one full iteration of the 64-byte unrolled body
    /// (`v0` + `v1` load/store pair) with no residual tail.
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn copy_avx2_copies_full_unroll2_iteration() {
        use alloc::vec::Vec;
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let src: Vec<u8> = (0..64u8).collect();
        let mut dst = [0_u8; 64];
        unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 64) };
        assert_eq!(&dst[..], &src[..]);
    }

    /// Exercises ONE unrolled 64-byte iteration PLUS the single-
    /// vector 32-byte residual tail (96 = 64 + 32). Validates that
    /// the tail branch doesn't overwrite preceding bytes and copies
    /// the correct source offset.
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn copy_avx2_copies_unroll2_loop_plus_residual_tail() {
        use alloc::vec::Vec;
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        let src: Vec<u8> = (0..96u8).collect();
        let mut dst = [0_u8; 96];
        unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 96) };
        assert_eq!(&dst[..], &src[..]);
        // Spot-check tail boundary: bytes 60..68 span the unroll/tail seam.
        assert_eq!(&dst[60..68], &[60, 61, 62, 63, 64, 65, 66, 67]);
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
