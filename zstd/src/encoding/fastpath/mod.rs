//! Encoder fastpath: hot encode functions duplicated per CPU feature set so the
//! whole hot loop stays inside one `#[target_feature]` umbrella and SIMD/BMI2
//! intrinsics inline natively (no ABI barrier).
//!
//! All kernel functions are `unsafe fn`; the explicit inner `unsafe { }` blocks
//! around intrinsic calls are kept for safety documentation (this matches the
//! Rust 2024 recommended style enforced by `unsafe_op_in_unsafe_fn`). The
//! `unused_unsafe` lint sees them as redundant inside an `unsafe fn` body, so
//! we silence it at the module level rather than removing the documentation.
#![allow(unused_unsafe)]
//!
//! # Background
//!
//! In Rust, `#[target_feature(enable = "...")]` creates an ABI boundary: a
//! caller without the same feature set must call the function non-inline. In
//! C, the equivalent intrinsics inline via macros without restriction. That ABI
//! barrier is the dominant structural reason our encoder cannot match the
//! C zstd upstream zstd on per-block latency — every hot-path SIMD call becomes a
//! function call (~100 cycles overhead per BT walk iter, ~32-512 iters per
//! position, thousands of positions per block).
//!
//! # Strategy
//!
//! Each architecture-specific submodule (`neon`, `avx2_bmi2`, `sse42`,
//! `scalar`) holds a duplicate of the hot encode path, with every function in
//! the chain marked with the same `#[target_feature]`. Inside the module
//! everything inlines freely. The single ABI boundary is the dispatcher entry
//! point in this `mod.rs`, called once per encoder invocation.
//!
//! # Variant matrix
//!
//! - `scalar`: portable baseline, no SIMD assumptions. Used on unsupported
//!   targets and as fallback.
//! - `neon` (aarch64 only): NEON is part of the AArch64 baseline ISA but Rust
//!   still flags intrinsics like `vld1q_u8` with `#[target_feature(enable =
//!   "neon")]`, so we still need the umbrella attribute to let them inline.
//! - `sse42` (x86_64): 128-bit SSE2 vector ops, the x86_64 baseline.
//! - `avx2_bmi2` (x86_64): adds AVX2 (32-byte vectors) and BMI2 (`pext`,
//!   `pdep`, `bzhi`) — common on Haswell+ (2013+).
//!
//! # Dispatcher
//!
//! [`select_kernel`] picks the best supported variant once per process via a
//! `OnceLock`. Encoder entry points call through the cached function pointer.
//! The single indirect call is amortized over the entire compression call,
//! and once inside the variant module the call graph is straight-line inlined.
//!
//! # Roadmap inside this module
//!
//! Week 1 (this commit): module scaffold + dispatcher skeleton.
//! Week 2a: match-length / common-prefix-len + `count_match_from_indices`.
//! Week 3a: BT walk (`bt_insert_step_no_rebase`,
//!   `bt_insert_and_collect_matches`) + HC chain walk.
//! Week 3b: optimal parser DP (`build_optimal_plan_impl` + price helpers).
//! Week 4: entropy encoders (FSE `encode_interleaved`, Huff0 `encode_stream`).
//! Week 5-6: bench vs `perf/pre-intrinsics-refactor-baseline` tag, profile,
//!   finalize.
//!
//! Refactor history and working rules for the multi-week PR #110 effort are
//! captured in the corresponding pull-request description.

pub(crate) mod scalar;

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
pub(crate) mod neon;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) mod sse42;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) mod avx2_bmi2;

#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel_simd128"
))]
pub(crate) mod simd128;

/// Runtime-selected variant tag. Picked once per process by [`select_kernel`].
///
/// Each variant corresponds to one of the submodules above and dictates which
/// implementation of the hot encoder path the dispatcher will call into.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum FastpathKernel {
    Scalar,
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    Neon,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Sse42,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Avx2Bmi2,
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel_simd128"
    ))]
    Simd128,
}

/// Select the best supported variant for the running CPU. Cached after first
/// call; intended to be invoked once at the entry point of each encoder call
/// so the rest of the call graph can keep working with the resolved kernel
/// value as a const-foldable input.
#[inline]
pub(crate) fn select_kernel() -> FastpathKernel {
    #[cfg(feature = "std")]
    {
        use std::sync::OnceLock;
        static CACHE: OnceLock<FastpathKernel> = OnceLock::new();
        *CACHE.get_or_init(detect_kernel_uncached)
    }
    #[cfg(not(feature = "std"))]
    {
        detect_kernel_uncached()
    }
}

#[inline]
// On wasm32+simd128 the tier is resolved unconditionally to `Simd128` (no
// runtime CPUID), so the trailing `Scalar` fallback is statically unreachable
// there; it stays the reachable fallback on every other target.
#[cfg_attr(
    all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel_simd128"
    ),
    allow(unreachable_code)
)]
fn detect_kernel_uncached() -> FastpathKernel {
    // Every kernel here uses only the vector ops named by its own tier:
    // 256-bit `_mm256_*` plus BMI2 bit-manipulation for the AVX2 tier,
    // 128-bit `_mm_*` for the SSE2 tier, and the NEON baseline on AArch64.
    // No tier reaches for an ISA extension outside that umbrella, so the
    // probes below test exactly what the selected kernel executes.
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("bmi2") {
            return FastpathKernel::Avx2Bmi2;
        }
        if std::is_x86_feature_detected!("sse2") {
            return FastpathKernel::Sse42;
        }
    }
    #[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
    {
        if std::arch::is_aarch64_feature_detected!("neon") {
            return FastpathKernel::Neon;
        }
    }

    #[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if cfg!(target_feature = "avx2")
            && cfg!(target_feature = "bmi2")
            && cfg!(target_feature = "sse4.2")
        {
            return FastpathKernel::Avx2Bmi2;
        }
        if cfg!(target_feature = "sse4.2") {
            return FastpathKernel::Sse42;
        }
    }
    #[cfg(all(
        not(feature = "std"),
        target_arch = "aarch64",
        target_endian = "little"
    ))]
    {
        if cfg!(target_feature = "neon") && cfg!(target_feature = "crc") {
            return FastpathKernel::Neon;
        }
    }

    // wasm SIMD is a compile-time feature (no runtime detection), so the
    // `+simd128` payload selects the SIMD kernel and the scalar payload never
    // compiles the variant.
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel_simd128"
    ))]
    {
        return FastpathKernel::Simd128;
    }

    FastpathKernel::Scalar
}

/// Public entry point for raw-pointer prefix-length scans (BT byte compare,
/// repcode extend, etc.): resolves the tier via [`select_kernel`] on every
/// call, so hot loops should cache the kernel and call
/// [`dispatch_common_prefix_len_ptr_with_kernel`] instead.
///
/// # Safety
/// `lhs` / `rhs` must each point to at least `max` initialized bytes.
#[inline]
pub(crate) unsafe fn dispatch_common_prefix_len_ptr(
    lhs: *const u8,
    rhs: *const u8,
    max: usize,
) -> usize {
    // Cold-path shim: resolves the kernel via `select_kernel()` on every call.
    // Hot match-finder loops resolve the kernel once per block and call
    // [`dispatch_common_prefix_len_ptr_with_kernel`] directly.
    unsafe { dispatch_common_prefix_len_ptr_with_kernel(select_kernel(), lhs, rhs, max) }
}

/// Prefix-length scan against an already-resolved [`FastpathKernel`], so a hot
/// loop pays the kernel-select once per block (caller-cached) instead of the
/// `OnceLock` atomic + branch on every byte-compare.
///
/// # Safety
/// `lhs` / `rhs` must each point to at least `max` initialized bytes.
#[inline(always)]
pub(crate) unsafe fn dispatch_common_prefix_len_ptr_with_kernel(
    kernel: FastpathKernel,
    lhs: *const u8,
    rhs: *const u8,
    max: usize,
) -> usize {
    match kernel {
        FastpathKernel::Scalar => unsafe { scalar::common_prefix_len_ptr(lhs, rhs, max) },
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        FastpathKernel::Neon => unsafe { neon::common_prefix_len_ptr(lhs, rhs, max) },
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        FastpathKernel::Sse42 => unsafe { sse42::common_prefix_len_ptr(lhs, rhs, max) },
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        FastpathKernel::Avx2Bmi2 => unsafe { avx2_bmi2::common_prefix_len_ptr(lhs, rhs, max) },
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel_simd128"
        ))]
        FastpathKernel::Simd128 => unsafe { simd128::common_prefix_len_ptr(lhs, rhs, max) },
    }
}

#[cfg(test)]
mod tests;
