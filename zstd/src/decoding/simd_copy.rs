// SIMD-intrinsic imports are split per tier and gated on the matching
// `kernel_*` feature so a tier-trimmed build pulls in only the intrinsics
// its enabled helpers use (a `kernel-scalar`-only trim imports none).
#[cfg(all(target_arch = "x86", feature = "kernel-sse"))]
use core::arch::x86::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
#[cfg(all(target_arch = "x86", feature = "kernel-avx2"))]
use core::arch::x86::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};
#[cfg(all(target_arch = "x86", feature = "kernel-vbmi2"))]
use core::arch::x86::{__m512i, _mm512_loadu_si512, _mm512_storeu_si512};
#[cfg(all(target_arch = "x86_64", feature = "kernel-sse"))]
use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
use core::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};
#[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
use core::arch::x86_64::{__m512i, _mm512_loadu_si512, _mm512_storeu_si512};
// Only the 32-bit x86 `detect_x86_caps` body queries CPU features at
// runtime; the x86_64 body derives them from `detect_cpu_kernel()`.
#[cfg(all(feature = "std", feature = "kernel-sse", target_arch = "x86"))]
use std::arch::is_x86_feature_detected;
#[cfg(all(
    feature = "std",
    feature = "kernel-sse",
    any(target_arch = "x86", target_arch = "x86_64")
))]
use std::sync::OnceLock;

#[cfg(all(
    target_arch = "aarch64",
    target_feature = "neon",
    feature = "kernel-neon"
))]
use core::arch::aarch64::{uint8x16_t, vld1q_u8, vst1q_u8};

#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
use core::arch::wasm32::{v128, v128_load, v128_store};

/// Diagnostic-only copy-shape histogram. Compiled out unless the
/// `copy-shape-stats` feature is on, so production / bench builds carry
/// zero cost. Buckets mirror the dispatch thresholds in
/// [`copy_bytes_overshooting`] so the captured distribution lines up with
/// which code path each call took. Counts are deterministic from the
/// compressed input (same on every CPU tier); only per-call timing is
/// architecture-specific.
#[cfg(feature = "copy-shape-stats")]
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

/// Copy length at or above which [`copy_bytes_overshooting`] hands off to
/// `memcpy` (ERMS `rep movsb` on x86) instead of its chunked SIMD loop.
/// Below this the inline SIMD / overlapping-u64 paths win; above it the
/// copy is bandwidth-bound and `memcpy`'s microcoded bulk store is faster.
/// Picked to sit well above hot literal pushes (1..=32 B) and typical
/// match copies, squarely in raw-block / long-match territory.
const BULK_MEMCPY_THRESHOLD: usize = 2048;

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

    #[cfg(feature = "copy-shape-stats")]
    shape_stats::record(copy_at_least);

    let min_buffer_size = core::cmp::min(src.1, dst.1);

    // Single-op fast path: for any copy_at_least in 1..=16 with 16 bytes of
    // slack on both sides, one vector store covers the request. Match copies
    // with offset 8..15 funnel into repeat_in_chunks → here as 8..15-byte
    // calls, and the previous chunk-loop dispatcher paid a function-call +
    // loop-setup cost on every one of them. The single-op path collapses
    // that to one load + one store, which is the upstream zstd wildcopy pattern.
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
        // SAFETY: `1 <= copy_at_least <= min(src.1, dst.1)` by this
        // function's contract and the zero check above.
        unsafe { copy_exact_small(src.0, dst.0, copy_at_least) };
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // Bulk-copy path: large non-overlapping copies (raw-block payloads,
    // long non-overlapping matches) are bandwidth-bound, and on modern
    // x86 the microcoded `rep movsb` (ERMS) that `memcpy` lowers to beats
    // a hand-rolled 2×32B ymm loop — it issues wider internal stores with
    // no per-iteration loop overhead and better hardware prefetch. The
    // chunked-SIMD kernels below win only in the small/medium range where
    // the `memcpy` call + ERMS startup cost would dominate the few bytes
    // actually moved. Above this threshold, hand off to `memcpy`.
    if copy_at_least >= BULK_MEMCPY_THRESHOLD {
        // SAFETY: by contract `copy_at_least <= min(src.1, dst.1)`, and
        // the regions do not overlap, so this reads/writes exactly
        // `copy_at_least` bytes within both reported spans (no overshoot).
        unsafe { dst.0.copy_from_nonoverlapping(src.0, copy_at_least) };
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // Chunked SIMD fast paths for larger copies. Each branch consults the
    // appropriate feature-detection mechanism (cached runtime detect under
    // std, compile-time target_feature otherwise) and falls through on miss
    // so a single dispatcher covers every arch + feature combination.
    // May have no callers in some builds: every invocation site is behind an
    // arch + `kernel_*` cfg, so the macro is unused on non-x86/non-aarch64
    // targets (no arch-specific sites compile) and on x86/aarch64 builds that
    // trim the SIMD tiers (e.g. scalar-only). Hence `allow(unused_macros)`.
    #[allow(unused_macros)]
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
        // Bound only when at least the SSE2 tier is enabled (the lowest x86
        // SIMD kernel; every higher tier implies it). A `kernel-scalar` trim
        // drops the binding along with all three dispatch arms below.
        #[cfg(feature = "kernel-sse")]
        let caps = detect_x86_caps();
        // Each call site is gated on its `kernel_*` feature so it disappears
        // alongside the cfg-gated helper def in a tier-trimmed build. `caps.*`
        // is already false when the feature is off (see `detect_x86_caps`), so
        // this only prunes already-dead branches.
        #[cfg(feature = "kernel-vbmi2")]
        if caps.avx512f {
            try_chunk_kernel!(64, copy_avx512);
        }
        #[cfg(feature = "kernel-avx2")]
        if caps.avx2 {
            try_chunk_kernel!(32, copy_avx2);
        }
        #[cfg(feature = "kernel-sse")]
        if caps.sse2 {
            try_chunk_kernel!(16, copy_sse2);
        }
    }

    #[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
    {
        // Gate the 64-byte copy on `avx512vbmi2`, not bare `avx512f`, to
        // match the std tag ladder: `detect_x86_caps` sets `avx512f` (→ 64B)
        // only for the `Vbmi2` tag, so an `avx512f`-but-not-VBMI2 target
        // (e.g. `-C target-cpu=skylake-avx512`) is the `Avx2` tier and uses
        // the 32B copy. Using bare `avx512f` here would diverge — no_std
        // would emit 64B copies where std emits 32B on the same CPU.
        #[cfg(all(target_feature = "avx512vbmi2", feature = "kernel-vbmi2"))]
        try_chunk_kernel!(64, copy_avx512);
        #[cfg(all(target_feature = "avx2", feature = "kernel-avx2"))]
        try_chunk_kernel!(32, copy_avx2);
        #[cfg(all(target_feature = "sse2", feature = "kernel-sse"))]
        try_chunk_kernel!(16, copy_sse2);
    }

    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        feature = "kernel-neon"
    ))]
    try_chunk_kernel!(16, copy_neon);

    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    try_chunk_kernel!(16, copy_simd128);

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
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
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
        unsafe { copy_exact_small(src.0, dst.0, copy_at_least) };
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
    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        feature = "kernel-neon"
    ))]
    unsafe {
        let v: uint8x16_t = vld1q_u8(src);
        vst1q_u8(dst, v);
        return;
    }
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    unsafe {
        let v: v128 = v128_load(src.cast::<v128>());
        v128_store(dst.cast::<v128>(), v);
        return;
    }
    #[cfg(all(
        feature = "std",
        feature = "kernel-sse",
        any(target_arch = "x86", target_arch = "x86_64")
    ))]
    unsafe {
        if detect_x86_caps().sse2 {
            copy_sse2(src, dst, 16);
            return;
        }
    }
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2",
        feature = "kernel-sse"
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
    // or its code will be silently dead). The explicit-SIMD arms are
    // gated on the matching `kernel_*` feature so a `kernel-scalar` trim
    // falls through to the portable path (matching the chunked-copy
    // dispatch above):
    //   • aarch64+neon + kernel-neon                   → arm above returns
    //   • aarch64+neon, NO kernel-neon                 → reaches here
    //   • std + x86 + kernel-sse + runtime-SSE2 tag   → arm above returns
    //   • std + x86 + kernel-sse + Scalar tag         → reaches here
    //   • std + x86, NO kernel-sse                    → reaches here
    //   • no-std + x86 + target_feature sse2+kernel-sse → arm above returns
    //   • no-std + x86, kernel-sse off (or no sse2)   → reaches here
    //   • wasm32 + simd128 + kernel-simd128            → arm above returns
    //   • wasm32, NO simd128 (or kernel off)           → reaches here
    //   • any other arch (riscv64, …)                  → reaches here
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
#[cfg(feature = "bench-internals")]
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
    #[cfg(all(
        feature = "std",
        feature = "kernel-sse",
        any(target_arch = "x86", target_arch = "x86_64")
    ))]
    {
        let caps = detect_x86_caps();
        // Mirror the dispatcher: a tier is selectable only when BOTH its
        // kernel_* feature is on AND the CPU exposes it, so a tier-trimmed
        // build reports the chunk the dispatcher can actually pick.
        #[cfg(feature = "kernel-vbmi2")]
        if caps.avx512f {
            return 64;
        }
        #[cfg(feature = "kernel-avx2")]
        if caps.avx2 {
            return 32;
        }
        if caps.sse2 {
            return 16;
        }
    }
    // The no-std arms must mirror the dispatcher's compile-time chunk
    // selection EXACTLY (both `target_feature` AND the matching `kernel_*`
    // gate), otherwise a tier-trimmed build would size test scenarios for a
    // chunk the dispatcher can never select — masking tier-gating
    // regressions. The 64B arm keys off `avx512vbmi2` (not bare `avx512f`),
    // matching the dispatcher's `kernel-vbmi2` 64B copy.
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "avx512vbmi2",
        feature = "kernel-vbmi2"
    ))]
    {
        return 64;
    }
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "avx2",
        feature = "kernel-avx2"
    ))]
    {
        return 32;
    }
    #[cfg(all(
        not(feature = "std"),
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2",
        feature = "kernel-sse"
    ))]
    {
        return 16;
    }
    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        feature = "kernel-neon"
    ))]
    {
        return 16;
    }
    #[allow(unreachable_code)]
    {
        core::mem::size_of::<usize>()
    }
}

/// Copies `len` bytes, a multiple of `usize`, one `usize` at a time.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes; regions non-overlapping.
#[inline(always)]
pub(crate) unsafe fn copy_scalar(mut src: *const u8, mut dst: *mut u8, len: usize) {
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

#[cfg(all(
    feature = "std",
    feature = "kernel-sse",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[derive(Clone, Copy)]
// `avx512f` / `avx2` are unread in a `kernel-sse`-only trim (their dispatch
// arms are cfg-gated out), so the fields are intentionally dead there.
#[allow(dead_code)]
struct X86Caps {
    avx512f: bool,
    avx2: bool,
    sse2: bool,
}

/// SIMD-copy capability flags for the chunked wildcopy dispatcher.
///
/// On `x86_64` these are DERIVED from the unified `detect_cpu_kernel()`
/// tag rather than detected independently, so the whole crate has a
/// single CPU-capability source of truth (the copy path can never
/// disagree with the entropy/sequence path about the running CPU). The
/// mapping follows the kernel ladder: Vbmi2 → 64/32/16-byte copies,
/// Avx2 → 32/16, Bmi2 / Sse2 → 16, Scalar → none. One subtle
/// consequence: an `avx512f`-but-not-VBMI2 CPU (e.g. Skylake-X) is
/// tagged `Avx2`, so it uses the 32-byte `copy_avx2` chunk instead of
/// the 64-byte `copy_avx512`. That is a negligible difference on match
/// copies (which are short) and keeps the taxonomy single-axis; modern
/// AVX-512 parts (Ice Lake+) carry VBMI2 and still reach `copy_avx512`.
///
/// On 32-bit `x86` the kernel tag carries no SIMD tiers (those are
/// `x86_64`-gated), so this keeps its own runtime detection to preserve
/// the SSE2 / AVX2 copy path there.
#[cfg(all(
    feature = "std",
    feature = "kernel-sse",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[inline(always)]
fn detect_x86_caps() -> X86Caps {
    static CAPS: OnceLock<X86Caps> = OnceLock::new();
    *CAPS.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            use crate::cpu_kernel::{CpuKernelTag, detect_cpu_kernel};
            match detect_cpu_kernel() {
                #[cfg(feature = "kernel-vbmi2")]
                CpuKernelTag::Vbmi2 => X86Caps {
                    avx512f: true,
                    avx2: true,
                    sse2: true,
                },
                #[cfg(feature = "kernel-avx2")]
                CpuKernelTag::Avx2 => X86Caps {
                    avx512f: false,
                    avx2: true,
                    sse2: true,
                },
                #[cfg(feature = "kernel-bmi2")]
                CpuKernelTag::Bmi2 => X86Caps {
                    avx512f: false,
                    avx2: false,
                    sse2: true,
                },
                #[cfg(feature = "kernel-sse")]
                CpuKernelTag::Sse2 => X86Caps {
                    avx512f: false,
                    avx2: false,
                    sse2: true,
                },
                CpuKernelTag::Scalar => X86Caps {
                    avx512f: false,
                    avx2: false,
                    sse2: false,
                },
            }
        }
        #[cfg(target_arch = "x86")]
        {
            // Mirror the x86_64 tag ladder above: each tier is reported only
            // when BOTH its `kernel_*` feature is enabled AND the CPU exposes
            // it at runtime, so a tier-trimmed build (e.g.
            // `--features kernel-scalar`) never selects a SIMD chunk on 32-bit
            // x86. `avx512f` follows the `Vbmi2` tag exactly — it is set on
            // `avx512vbmi2` (not bare `avx512f`), matching the rule that an
            // AVX-512F-but-not-VBMI2 CPU stays on the 32B (AVX2) copy.
            use crate::cpu_kernel::{CpuLevel, cpu_allows};
            X86Caps {
                avx512f: cfg!(feature = "kernel-vbmi2")
                    && cpu_allows(CpuLevel::Avx512)
                    && is_x86_feature_detected!("avx512vbmi2"),
                avx2: cfg!(feature = "kernel-avx2")
                    && cpu_allows(CpuLevel::Avx2)
                    && is_x86_feature_detected!("avx2"),
                sse2: cfg!(feature = "kernel-sse")
                    && cpu_allows(CpuLevel::Sse2)
                    && is_x86_feature_detected!("sse2"),
            }
        }
    })
}

// Gated on `kernel-sse` so a `kernel-scalar`-only trim prunes the SSE2
// helper at the source level, not just via dead-code elimination.
// `#[allow(dead_code)]` is still required for one combo the feature gate
// can't express: std builds reach this through runtime `detect_x86_caps`,
// so the helper must compile even when no compile-time `target_feature =
// "sse2"` selects it — leaving it caller-less in a no-std-without-sse2
// build that still enables `kernel-sse`.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[target_feature(enable = "sse2")]
#[inline]
#[allow(dead_code)]
pub(crate) unsafe fn copy_sse2(mut src: *const u8, mut dst: *mut u8, len: usize) {
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
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[target_feature(enable = "avx2")]
#[inline]
#[allow(dead_code)]
pub(crate) unsafe fn copy_avx2(mut src: *const u8, mut dst: *mut u8, len: usize) {
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
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-vbmi2"
))]
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

#[cfg(all(
    target_arch = "aarch64",
    target_feature = "neon",
    feature = "kernel-neon"
))]
#[inline(always)]
pub(crate) unsafe fn copy_neon(mut src: *const u8, mut dst: *mut u8, len: usize) {
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

/// WebAssembly `simd128` 16-byte chunk copy: `v128_load` / `v128_store` per
/// 16 bytes, mirroring [`copy_neon`]. `len` is a multiple of 16 (the caller
/// rounds up via `try_chunk_kernel!`). Compiled only under
/// `target_feature = "simd128"`, so the intrinsics are available without a
/// `#[target_feature]` attribute (wasm SIMD is a compile-time decision, no
/// runtime detection); the loads/stores are `unsafe` raw-pointer ops.
#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
#[inline(always)]
pub(crate) unsafe fn copy_simd128(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            let v: v128 = v128_load(src.cast::<v128>());
            v128_store(dst.cast::<v128>(), v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}

/// Exact copy of `1..=32` bytes that reads and writes strictly `[0, len)`:
/// bytes for `1..=8`, two overlapping `u64` for `9..=16`, four for `17..=32`.
/// Branch on the size class only, never on the CPU.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes; regions non-overlapping;
/// `1 <= len <= 32`.
#[inline(always)]
pub(crate) unsafe fn copy_exact_small(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!((1..=32).contains(&len), "copy_exact_small takes 1..=32");
    unsafe {
        if len <= 8 {
            // The fixed-size loop unrolls into immediate-offset loads and
            // stores on every sane backend: a few cycles inline against the
            // call into libc memmove it replaces.
            let mut i = 0;
            while i < len {
                dst.add(i).write(src.add(i).read());
                i += 1;
            }
        } else if len <= 16 {
            // The overlap is written twice with the same source bytes, so the
            // net effect is exactly `len` bytes and nothing past them.
            let lo: u64 = src.cast::<u64>().read_unaligned();
            let hi_offset = len - 8;
            let hi: u64 = src.add(hi_offset).cast::<u64>().read_unaligned();
            dst.cast::<u64>().write_unaligned(lo);
            dst.add(hi_offset).cast::<u64>().write_unaligned(hi);
        } else {
            // First 16 via two adjacent u64, the trailing 1..=16 via the same
            // overlapping pair. Four loads and four stores, no branch.
            let lo: u64 = src.cast::<u64>().read_unaligned();
            let hi: u64 = src.add(8).cast::<u64>().read_unaligned();
            dst.cast::<u64>().write_unaligned(lo);
            dst.add(8).cast::<u64>().write_unaligned(hi);
            let tail_off = len - 16;
            let tail_lo: u64 = src.add(tail_off).cast::<u64>().read_unaligned();
            let tail_hi: u64 = src.add(len - 8).cast::<u64>().read_unaligned();
            dst.add(tail_off).cast::<u64>().write_unaligned(tail_lo);
            dst.add(len - 8).cast::<u64>().write_unaligned(tail_hi);
        }
    }
}

/// AVX2 exact copy for `len >= 33`: branchless size classes up to 128 bytes,
/// then a 2×32B-unrolled loop, an exact 32B cleanup and one overlapping 32B
/// tail. Reads and writes strictly `[0, len)`.
///
/// `#[target_feature]` so the kernel exists in a stock artifact; it inlines into
/// a caller compiled under the same feature, which is how the per-tier loops
/// that use it are built.
///
/// # Safety
/// The CPU has AVX2; `src` readable and `dst` writable for `len` bytes, the
/// regions non-overlapping; `len >= 33`.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2",
))]
#[target_feature(enable = "avx2")]
#[inline]
pub(crate) unsafe fn copy_exact_avx2(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(len >= 33, "copy_exact_avx2 requires len >= 33");
    unsafe {
        if len <= 64 {
            // Two overlapping 32B blocks. A loop here lost to glibc's tiny
            // path on its branch alone (+23% at len=40, the dominant medium
            // bucket); the straight-line form ties or beats it.
            let a = _mm256_loadu_si256(src.cast::<__m256i>());
            let b = _mm256_loadu_si256(src.add(len - 32).cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), a);
            _mm256_storeu_si256(dst.add(len - 32).cast::<__m256i>(), b);
        } else if len <= 128 {
            let a = _mm256_loadu_si256(src.cast::<__m256i>());
            let b = _mm256_loadu_si256(src.add(32).cast::<__m256i>());
            let c = _mm256_loadu_si256(src.add(len - 64).cast::<__m256i>());
            let d = _mm256_loadu_si256(src.add(len - 32).cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), a);
            _mm256_storeu_si256(dst.add(32).cast::<__m256i>(), b);
            _mm256_storeu_si256(dst.add(len - 64).cast::<__m256i>(), c);
            _mm256_storeu_si256(dst.add(len - 32).cast::<__m256i>(), d);
        } else {
            // The single 32B tail overlaps the block before it at most once. A
            // 2×32B tail overlapping the block the loop just wrote hits a
            // store-buffer partial-overlap penalty on Skylake every iteration
            // (+69% at len=800).
            let mut o = 0usize;
            while o + 64 <= len {
                let v0 = _mm256_loadu_si256(src.add(o).cast::<__m256i>());
                let v1 = _mm256_loadu_si256(src.add(o + 32).cast::<__m256i>());
                _mm256_storeu_si256(dst.add(o).cast::<__m256i>(), v0);
                _mm256_storeu_si256(dst.add(o + 32).cast::<__m256i>(), v1);
                o += 64;
            }
            while o + 32 <= len {
                let v = _mm256_loadu_si256(src.add(o).cast::<__m256i>());
                _mm256_storeu_si256(dst.add(o).cast::<__m256i>(), v);
                o += 32;
            }
            if o < len {
                let t = len - 32;
                let v = _mm256_loadu_si256(src.add(t).cast::<__m256i>());
                _mm256_storeu_si256(dst.add(t).cast::<__m256i>(), v);
            }
        }
    }
}

/// SSE2 exact copy for `len >= 33`: branchless size class for `len <= 64`,
/// then a 2×16B-unrolled loop, an exact 16B cleanup and one overlapping 16B
/// tail. Reads and writes strictly `[0, len)`.
///
/// # Safety
/// The CPU has SSE2; `src` readable and `dst` writable for `len` bytes, the
/// regions non-overlapping; `len >= 33`.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse",
))]
#[target_feature(enable = "sse2")]
#[inline]
pub(crate) unsafe fn copy_exact_sse2(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(len >= 33, "copy_exact_sse2 requires len >= 33");
    unsafe {
        if len <= 64 {
            let a = _mm_loadu_si128(src.cast::<__m128i>());
            let b = _mm_loadu_si128(src.add(16).cast::<__m128i>());
            let c = _mm_loadu_si128(src.add(len - 32).cast::<__m128i>());
            let d = _mm_loadu_si128(src.add(len - 16).cast::<__m128i>());
            _mm_storeu_si128(dst.cast::<__m128i>(), a);
            _mm_storeu_si128(dst.add(16).cast::<__m128i>(), b);
            _mm_storeu_si128(dst.add(len - 32).cast::<__m128i>(), c);
            _mm_storeu_si128(dst.add(len - 16).cast::<__m128i>(), d);
        } else {
            let mut o = 0usize;
            while o + 32 <= len {
                let v0 = _mm_loadu_si128(src.add(o).cast::<__m128i>());
                let v1 = _mm_loadu_si128(src.add(o + 16).cast::<__m128i>());
                _mm_storeu_si128(dst.add(o).cast::<__m128i>(), v0);
                _mm_storeu_si128(dst.add(o + 16).cast::<__m128i>(), v1);
                o += 32;
            }
            while o + 16 <= len {
                _mm_storeu_si128(
                    dst.add(o).cast::<__m128i>(),
                    _mm_loadu_si128(src.add(o).cast::<__m128i>()),
                );
                o += 16;
            }
            if o < len {
                let t = len - 16;
                _mm_storeu_si128(
                    dst.add(t).cast::<__m128i>(),
                    _mm_loadu_si128(src.add(t).cast::<__m128i>()),
                );
            }
        }
    }
}

/// NEON exact copy for `len >= 33`, unrolled 2×16B (32 B/iter) with one
/// overlapping 16B tail. NEON is the aarch64 baseline, so the body inlines
/// with no boundary.
#[cfg(all(
    target_arch = "aarch64",
    target_feature = "neon",
    feature = "kernel-neon"
))]
#[inline]
pub(crate) unsafe fn copy_exact_neon(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(len >= 33, "copy_exact_neon requires len >= 33");
    let mut o = 0usize;
    unsafe {
        while o + 32 <= len {
            let v0 = vld1q_u8(src.add(o));
            let v1 = vld1q_u8(src.add(o + 16));
            vst1q_u8(dst.add(o), v0);
            vst1q_u8(dst.add(o + 16), v1);
            o += 32;
        }
        while o + 16 <= len {
            vst1q_u8(dst.add(o), vld1q_u8(src.add(o)));
            o += 16;
        }
        if o < len {
            let t = len - 16;
            vst1q_u8(dst.add(t), vld1q_u8(src.add(t)));
        }
    }
}

/// WebAssembly `simd128` exact copy for `len >= 33`, the shape of
/// [`copy_exact_neon`]: 2×16B per iteration and one overlapping 16B tail.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes, the regions
/// non-overlapping; `len >= 33`.
#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
#[inline]
pub(crate) unsafe fn copy_exact_simd128(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(len >= 33, "copy_exact_simd128 requires len >= 33");
    let mut o = 0usize;
    unsafe {
        while o + 32 <= len {
            let v0 = v128_load(src.add(o).cast::<v128>());
            let v1 = v128_load(src.add(o + 16).cast::<v128>());
            v128_store(dst.add(o).cast::<v128>(), v0);
            v128_store(dst.add(o + 16).cast::<v128>(), v1);
            o += 32;
        }
        while o + 16 <= len {
            v128_store(
                dst.add(o).cast::<v128>(),
                v128_load(src.add(o).cast::<v128>()),
            );
            o += 16;
        }
        if o < len {
            let t = len - 16;
            v128_store(
                dst.add(t).cast::<v128>(),
                v128_load(src.add(t).cast::<v128>()),
            );
        }
    }
}

/// `u64` exact copy for `len >= 33`: 8-byte strides and one overlapping 8-byte
/// tail. Reads and writes strictly `[0, len)`. The scalar tier's kernel.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes, the regions
/// non-overlapping; `len >= 33`.
#[inline]
pub(crate) unsafe fn copy_exact_u64(src: *const u8, dst: *mut u8, len: usize) {
    debug_assert!(len >= 33, "copy_exact_u64 requires len >= 33");
    unsafe {
        let mut o = 0usize;
        while o + 8 <= len {
            let v: u64 = src.add(o).cast::<u64>().read_unaligned();
            dst.add(o).cast::<u64>().write_unaligned(v);
            o += 8;
        }
        if o < len {
            let t = len - 8;
            let v: u64 = src.add(t).cast::<u64>().read_unaligned();
            dst.add(t).cast::<u64>().write_unaligned(v);
        }
    }
}

#[cfg(test)]
mod tests;
