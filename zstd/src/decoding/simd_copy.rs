// SIMD-intrinsic imports are split per tier and gated on the matching
// `kernel_*` feature so a tier-trimmed build pulls in only the intrinsics
// its enabled helpers use (a `kernel-scalar`-only trim imports none).
#[cfg(all(target_arch = "x86", feature = "kernel-sse"))]
use core::arch::x86::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
#[cfg(all(target_arch = "x86", feature = "kernel-avx2"))]
use core::arch::x86::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};
#[cfg(all(target_arch = "x86_64", feature = "kernel-sse"))]
use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
use core::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};

use crate::cpu_kernel::CpuKernel;

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

/// Chunk width of [`copy_chunks_baseline`]: the vector the build's baseline
/// guarantees on every CPU it runs on (SSE2 on x86 targets that carry it, NEON
/// on aarch64, simd128 on wasm), or a machine word where there is none. Wider
/// x86 tiers are a property of the running CPU and come from its kernel.
pub(crate) const BASELINE_COPY_CHUNK: usize = if cfg!(any(
    all(
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2",
        feature = "kernel-sse"
    ),
    all(
        target_arch = "aarch64",
        target_feature = "neon",
        feature = "kernel-neon"
    ),
    all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ),
)) {
    16
} else {
    SCALAR_COPY_CHUNK
};

/// Copies `len` bytes, a multiple of [`BASELINE_COPY_CHUNK`], in whole chunks
/// of the build's baseline width.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes; regions non-overlapping.
#[inline(always)]
pub(crate) unsafe fn copy_chunks_baseline(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        target_feature = "sse2",
        feature = "kernel-sse"
    ))]
    // SAFETY: SSE2 is in the build's baseline, so every CPU it runs on has it.
    unsafe {
        copy_sse2(src, dst, len)
    }
    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        feature = "kernel-neon"
    ))]
    unsafe {
        copy_neon(src, dst, len)
    }
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    unsafe {
        copy_simd128(src, dst, len)
    }
    #[cfg(not(any(
        all(
            any(target_arch = "x86", target_arch = "x86_64"),
            target_feature = "sse2",
            feature = "kernel-sse"
        ),
        all(
            target_arch = "aarch64",
            target_feature = "neon",
            feature = "kernel-neon"
        ),
        all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ),
    )))]
    unsafe {
        copy_scalar(src, dst, len)
    }
}

/// Copies exactly 16 bytes with the build's baseline vector, or two machine
/// loads and stores where there is none.
///
/// # Safety
/// `src` readable and `dst` writable for 16 bytes; regions non-overlapping.
#[inline(always)]
pub(crate) unsafe fn copy16_baseline(src: *const u8, dst: *mut u8) {
    #[cfg(any(
        all(
            any(target_arch = "x86", target_arch = "x86_64"),
            target_feature = "sse2",
            feature = "kernel-sse"
        ),
        all(
            target_arch = "aarch64",
            target_feature = "neon",
            feature = "kernel-neon"
        ),
        all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ),
    ))]
    unsafe {
        copy_chunks_baseline(src, dst, 16)
    }
    #[cfg(not(any(
        all(
            any(target_arch = "x86", target_arch = "x86_64"),
            target_feature = "sse2",
            feature = "kernel-sse"
        ),
        all(
            target_arch = "aarch64",
            target_feature = "neon",
            feature = "kernel-neon"
        ),
        all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ),
    )))]
    unsafe {
        let lo: u64 = src.cast::<u64>().read_unaligned();
        let hi: u64 = src.add(8).cast::<u64>().read_unaligned();
        dst.cast::<u64>().write_unaligned(lo);
        dst.add(8).cast::<u64>().write_unaligned(hi);
    }
}

/// Chunk width of [`copy_scalar`]: a 64-bit word on every target. A 32-bit
/// target's pointer-sized word would halve the stride of every copy the
/// portable paths make.
pub(crate) const SCALAR_COPY_CHUNK: usize = 8;

/// Chunk width of [`copy_chunks_portable`]: a 64-bit word, or the `simd128`
/// vector on wasm, where a build's SIMD is fixed at compile time and there is
/// no run-time tier to choose it.
pub(crate) const PORTABLE_COPY_CHUNK: usize = if cfg!(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
)) {
    16
} else {
    SCALAR_COPY_CHUNK
};

/// Copies `len` bytes, a multiple of [`PORTABLE_COPY_CHUNK`], in whole chunks
/// of portable code: the scalar tier's copies.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes; regions non-overlapping.
#[inline(always)]
pub(crate) unsafe fn copy_chunks_portable(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    unsafe {
        copy_simd128(src, dst, len)
    }
    #[cfg(not(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    )))]
    unsafe {
        copy_scalar(src, dst, len)
    }
}

/// Copies exactly 16 bytes with portable code: two machine loads and stores,
/// or one `simd128` transfer on wasm.
///
/// # Safety
/// `src` readable and `dst` writable for 16 bytes; regions non-overlapping.
#[inline(always)]
pub(crate) unsafe fn copy16_portable(src: *const u8, dst: *mut u8) {
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    unsafe {
        copy_simd128(src, dst, 16)
    }
    #[cfg(not(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    )))]
    unsafe {
        let lo: u64 = src.cast::<u64>().read_unaligned();
        let hi: u64 = src.add(8).cast::<u64>().read_unaligned();
        dst.cast::<u64>().write_unaligned(lo);
        dst.add(8).cast::<u64>().write_unaligned(hi);
    }
}

/// Copies at least `copy_at_least` bytes from `src` to `dst` with the copy
/// kernels of `K`, the CPU tier the caller was monomorphised for.
///
/// This helper may over-copy up to `K::COPY_CHUNK - 1` bytes (or 15 on the
/// single-store path), mirroring zstd wildcopy semantics for faster inner
/// loops. Nothing here asks the CPU anything: the tier is the type.
///
/// # Safety
/// Caller must guarantee:
/// - `src.0` points to at least `src.1` readable bytes.
/// - `dst.0` points to at least `dst.1` writable bytes.
/// - `copy_at_least <= src.1` and `copy_at_least <= dst.1`.
/// - `src.1` and `dst.1` are large enough for the overshoot: if
///   `min(src.1, dst.1) >= copy_at_least` rounded up to the chunk size, the
///   chunk loop may copy that rounded-up amount. Otherwise the function
///   copies exactly `copy_at_least` bytes.
/// - Source and destination regions do not overlap.
/// - The running CPU supports `K`'s tier.
#[inline(always)]
pub(crate) unsafe fn copy_bytes_overshooting<K: CpuKernel>(
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
        // SAFETY: 16 bytes of room on both sides (just checked); the tier is
        // the caller's contract.
        unsafe { K::copy16(src.0, dst.0) };
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

    // Chunked path in the tier's width, when the rounded-up copy fits.
    let rounded = copy_at_least.next_multiple_of(K::COPY_CHUNK);
    if min_buffer_size >= rounded {
        // SAFETY: `rounded` bytes fit both spans (just checked); the tier is
        // the caller's contract.
        unsafe { K::copy_chunks(src.0, dst.0, rounded) };
        debug_assert_eq_copy(src, dst, copy_at_least);
        return;
    }

    // A tier with a narrower step steps down to it before the machine word:
    // near the end of a buffer the slack often fits 16 but not 32. The
    // comparison is between constants and folds away.
    if K::STEP_CHUNK < K::COPY_CHUNK {
        let rounded = copy_at_least.next_multiple_of(K::STEP_CHUNK);
        if min_buffer_size >= rounded {
            // SAFETY: `rounded` bytes fit both spans (just checked); the tier
            // is the caller's contract.
            unsafe { K::copy_step(src.0, dst.0, rounded) };
            debug_assert_eq_copy(src, dst, copy_at_least);
            return;
        }
    }

    // Final fallback: 64-bit chunks if the slack permits, else an exact byte
    // copy.
    let rounded = copy_at_least.next_multiple_of(SCALAR_COPY_CHUNK);
    if min_buffer_size >= rounded {
        unsafe { copy_scalar(src.0, dst.0, rounded) };
    } else {
        unsafe { dst.0.copy_from_nonoverlapping(src.0, copy_at_least) };
    }
    debug_assert_eq_copy(src, dst, copy_at_least);
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
    // A standalone entry, so the tier is resolved here, on the way in, the way
    // the decoder resolves it once per block, and to the kernel the sequence
    // executor copies with for that tier: every arm mirrors its dispatch.
    use crate::cpu_kernel::{CpuKernelTag, ScalarKernel, detect_cpu_kernel};
    // SAFETY (every arm): the caller's contract covers the spans, and
    // `detect_cpu_kernel` names only a tier this CPU runs.
    match detect_cpu_kernel() {
        CpuKernelTag::Scalar => unsafe {
            copy_bytes_overshooting::<ScalarKernel>(src, dst, copy_at_least)
        },
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-sse"
        ))]
        CpuKernelTag::Sse2 => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::Sse2Kernel>(src, dst, copy_at_least)
        },
        // 32-bit x86 at the BMI2 tier runs the SSE2 walk.
        #[cfg(all(target_arch = "x86", feature = "kernel-bmi2"))]
        CpuKernelTag::Bmi2 => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::Sse2Kernel>(src, dst, copy_at_least)
        },
        #[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
        CpuKernelTag::Bmi2 => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::Bmi2Kernel>(src, dst, copy_at_least)
        },
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-avx2"
        ))]
        CpuKernelTag::Avx2 => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::Avx2Kernel>(src, dst, copy_at_least)
        },
        #[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
        CpuKernelTag::Vbmi2 => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::Vbmi2Kernel>(src, dst, copy_at_least)
        },
        #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
        CpuKernelTag::Neon => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::NeonKernel>(src, dst, copy_at_least)
        },
        #[cfg(all(
            target_arch = "aarch64",
            feature = "kernel-sve",
            any(feature = "std", target_feature = "sve"),
        ))]
        CpuKernelTag::Sve => unsafe {
            copy_bytes_overshooting::<crate::cpu_kernel::SveKernel>(src, dst, copy_at_least)
        },
    }
}

/// Chunk width of the kernel the backend tests copy with ([`ScalarKernel`],
/// the build's baseline). Used by `RingBuffer` tests to size scenarios that
/// exercise single-chunk, multi-chunk, and capacity-tight (`chunk + 1`) copy
/// shapes on every architecture.
///
/// [`ScalarKernel`]: crate::cpu_kernel::ScalarKernel
#[cfg(test)]
#[inline]
pub(crate) fn active_chunk_size_for_tests() -> usize {
    <crate::cpu_kernel::ScalarKernel as CpuKernel>::COPY_CHUNK
}

/// Copies `len` bytes, a multiple of [`SCALAR_COPY_CHUNK`], one `u64` at a
/// time.
///
/// # Safety
/// `src` readable and `dst` writable for `len` bytes; regions non-overlapping.
#[inline(always)]
pub(crate) unsafe fn copy_scalar(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            dst.cast::<u64>()
                .write_unaligned(src.cast::<u64>().read_unaligned());
            src = src.add(SCALAR_COPY_CHUNK);
            dst = dst.add(SCALAR_COPY_CHUNK);
        }
    }
}

/// Copies `len` bytes, a multiple of 16, in 16-byte SSE2 chunks. Gated on
/// `kernel-sse` so a `kernel-scalar`-only trim prunes it at the source level.
///
/// # Safety
/// The CPU has SSE2; `src` readable and `dst` writable for `len` bytes; the
/// regions do not overlap.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[target_feature(enable = "sse2")]
#[inline]
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

/// Copies `len` bytes, a multiple of 32, in 32-byte AVX2 chunks.
///
/// The loop is unrolled to two 32-byte vectors per iteration (64 bytes),
/// with one more vector for the residual 32 when `len` is not a multiple of
/// 64. The two independent load / store pairs expose instruction-level
/// parallelism and amortise the loop branch.
///
/// # Safety
/// The CPU has AVX2; `src` readable and `dst` writable for `len` bytes; the
/// regions do not overlap.
#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[target_feature(enable = "avx2")]
#[inline]
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
