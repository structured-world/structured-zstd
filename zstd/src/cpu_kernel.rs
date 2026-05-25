//! Top-level CPU kernel dispatch — single detect+match per zstd call,
//! propagated through the entire pipeline as a generic parameter so
//! inner code (HUF burst, FSE state update, sequence executor,
//! match-copy, bit-reader) monomorphises against the chosen kernel.
//!
//! See issue #247 for the architecture rationale: per-subsystem dispatch
//! scatters the choice across HUF / FSE / SIMD-copy independently and
//! pays the cost N times per call. The lifted top-level dispatch
//! collapses to one detect at the FrameDecoder / FrameCompressor entry;
//! all inner leaf-hot-path ops route through `K::method` calls on the
//! chosen kernel zero-sized type.
//!
//! Structure code (block loop, FCS check, offset history, repeat
//! semantics) stays single-impl and only carries `K` as a phantom on
//! the outer function. Monomorphisation specialises ONLY the bodies
//! that actually differ per ISA — `mask_lower_bits`, `huf_burst`,
//! `copy_chunk`, etc.

#[cfg(feature = "std")]
use std::sync::OnceLock;

/// Trait covering the leaf hot-path operations whose bodies differ
/// per ISA. Implementations are ZSTs; the trait is `Copy` so it can
/// be `Default`-constructed at each call site without runtime cost.
///
/// New methods land here ONLY when their codegen genuinely differs
/// per kernel (BMI2 intrinsic vs scalar shift, AVX2 256-bit move vs
/// SSE2 128-bit move, etc.). Structure ops that have one canonical
/// implementation must NOT be on this trait — they stay on the
/// existing decoder / encoder types.
pub(crate) trait CpuKernel: Copy + 'static {
    /// Mask the low `n` bits of `value`, returning the remaining
    /// high bits zeroed. The FSE bitstream hot path fires this 3×
    /// per decoded sequence; on BMI2-capable hardware this maps to
    /// a single `_bzhi_u64` instruction, otherwise to a scalar
    /// `u64::MAX >> (64 - n)` shift + mask.
    ///
    /// Precondition: `n <= 64`. Behaviour for `n == 0` is "return 0";
    /// behaviour for `n > 64` is unspecified (debug-asserted by the
    /// caller through the wrapper in `bit_reader_reverse.rs`).
    fn mask_lower_bits(value: u64, n: u8) -> u64;
}

/// Scalar fallback — portable, no SIMD or BMI2 intrinsics. Selected
/// when no x86 or aarch64 feature is detected at runtime.
#[derive(Copy, Clone, Default)]
pub(crate) struct ScalarKernel;

impl CpuKernel for ScalarKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // `checked_shr` returns `None` for shift counts >= 64, which
        // happens exactly when `n == 0` (`64 - 0 = 64`). Mapping
        // both that case and the invalid `n > 64` underflow to 0
        // gives the mathematically-correct empty mask for n=0 and
        // a safe-ish fallback for the invalid range.
        let mask = u64::MAX
            .checked_shr(64u32.wrapping_sub(n as u32))
            .unwrap_or(0);
        value & mask
    }
}

/// x86_64 BMI2-only kernel: `_bzhi_u64` for mask_lower_bits. Selected
/// when the CPU has BMI2 but not the AVX2 SIMD width to upgrade to
/// the Avx2 kernel. Treated as a stepping stone between Scalar and
/// Avx2 on hardware that has BMI2 but not AVX2 (rare in practice but
/// matches donor's gating).
#[cfg(target_arch = "x86_64")]
#[derive(Copy, Clone, Default)]
pub(crate) struct Bmi2Kernel;

#[cfg(target_arch = "x86_64")]
impl CpuKernel for Bmi2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // SAFETY: this impl is only constructed via
        // `dispatch_cpu_kernel` after `detect_cpu_kernel` confirmed
        // BMI2 is available on the running CPU.
        unsafe { mask_lower_bits_bmi2_impl(value, n) }
    }
}

/// x86_64 AVX2 + BMI2 kernel (x86-64-v3 baseline). The common modern
/// x86 case — most CPUs released since 2013 (Haswell) have AVX2+BMI2.
/// Uses `_bzhi_u64` for mask ops; future trait methods will use AVX2
/// 256-bit moves for `copy_chunk` and pext for HUF burst.
#[cfg(target_arch = "x86_64")]
#[derive(Copy, Clone, Default)]
pub(crate) struct Avx2Kernel;

#[cfg(target_arch = "x86_64")]
impl CpuKernel for Avx2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // SAFETY: Avx2Kernel is selected only after runtime detect
        // confirmed both AVX2 and BMI2 — `_bzhi_u64` is callable.
        unsafe { mask_lower_bits_bmi2_impl(value, n) }
    }
}

/// x86_64 AVX-512 VBMI2 + AVX2 + BMI2 kernel. Selected when the CPU
/// has the AVX-512 VBMI2 family available — VBMI2 unlocks a faster
/// HUF burst inner loop (VPSHUFB-based table lookup); BMI2 mask_lower
/// bits stays identical to Avx2 kernel.
#[cfg(target_arch = "x86_64")]
#[derive(Copy, Clone, Default)]
pub(crate) struct Vbmi2Kernel;

#[cfg(target_arch = "x86_64")]
impl CpuKernel for Vbmi2Kernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // SAFETY: same precondition as Avx2Kernel — BMI2 confirmed
        // at runtime before this kernel is instantiated.
        unsafe { mask_lower_bits_bmi2_impl(value, n) }
    }
}

/// aarch64 NEON baseline kernel. Used on all aarch64 hardware that
/// exposes NEON (effectively universal on the supported targets).
#[cfg(target_arch = "aarch64")]
#[derive(Copy, Clone, Default)]
pub(crate) struct NeonKernel;

#[cfg(target_arch = "aarch64")]
impl CpuKernel for NeonKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        // aarch64 has no BMI2 equivalent that improves on the scalar
        // shift-and-mask sequence for this op; the codegen is
        // identical to the Scalar kernel here. Other trait methods
        // (huf_burst, copy_chunk) will diverge once they land.
        ScalarKernel::mask_lower_bits(value, n)
    }
}

/// aarch64 SVE kernel. Variable-vector-length SVE extends NEON for
/// HUF burst / SIMD copy on Graviton3 / Apple M-series with SVE
/// support. Mask op identical to NEON / Scalar.
#[cfg(target_arch = "aarch64")]
#[derive(Copy, Clone, Default)]
pub(crate) struct SveKernel;

#[cfg(target_arch = "aarch64")]
impl CpuKernel for SveKernel {
    #[inline(always)]
    fn mask_lower_bits(value: u64, n: u8) -> u64 {
        ScalarKernel::mask_lower_bits(value, n)
    }
}

/// Monomorphised BMI2 `_bzhi_u64` wrapper. Lifted to a free function
/// with `#[target_feature]` so every kernel impl that wraps it
/// resolves to the same shared inlined code; LLVM emits one
/// monomorphisation per call site.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
#[inline]
unsafe fn mask_lower_bits_bmi2_impl(value: u64, n: u8) -> u64 {
    // SAFETY: caller selected a kernel whose CpuKernelTag was
    // resolved after `is_x86_feature_detected!("bmi2")` returned
    // true. The intrinsic is callable in that context.
    unsafe { core::arch::x86_64::_bzhi_u64(value, n as u32) }
}

/// Cached runtime-detected kernel tag. The actual `CpuKernel` impl
/// is constructed from this at the FrameDecoder / FrameCompressor
/// entry via a `match` that branches into the appropriate generic
/// `*_impl<K>` specialisation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum CpuKernelTag {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Bmi2,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "x86_64")]
    Vbmi2,
    #[cfg(target_arch = "aarch64")]
    Neon,
    #[cfg(target_arch = "aarch64")]
    Sve,
}

/// Detect once and cache the best available CPU kernel for the
/// current process. Subsequent calls return the cached tag without
/// re-running CPU-feature detection. Std-only — no-std targets use
/// the compile-time variant below that resolves at build time.
#[cfg(feature = "std")]
pub(crate) fn detect_cpu_kernel() -> CpuKernelTag {
    static CACHED: OnceLock<CpuKernelTag> = OnceLock::new();
    *CACHED.get_or_init(detect_cpu_kernel_uncached)
}

#[cfg(feature = "std")]
fn detect_cpu_kernel_uncached() -> CpuKernelTag {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected;
        let has_avx512vbmi2 = is_x86_feature_detected!("avx512vbmi2");
        let has_avx512f = is_x86_feature_detected!("avx512f");
        let has_avx512vl = is_x86_feature_detected!("avx512vl");
        let has_avx512bw = is_x86_feature_detected!("avx512bw");
        let has_bmi2 = is_x86_feature_detected!("bmi2");
        let has_avx2 = is_x86_feature_detected!("avx2");
        if has_avx512vbmi2 && has_avx512f && has_avx512vl && has_avx512bw && has_bmi2 {
            return CpuKernelTag::Vbmi2;
        }
        if has_avx2 && has_bmi2 {
            return CpuKernelTag::Avx2;
        }
        if has_bmi2 {
            return CpuKernelTag::Bmi2;
        }
        return CpuKernelTag::Scalar;
    }
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::is_aarch64_feature_detected;
        if is_aarch64_feature_detected!("sve") {
            return CpuKernelTag::Sve;
        }
        if is_aarch64_feature_detected!("neon") {
            return CpuKernelTag::Neon;
        }
        return CpuKernelTag::Scalar;
    }
    #[allow(unreachable_code)]
    CpuKernelTag::Scalar
}

/// no-std variant: rely on compile-time `target_feature` flags
/// instead of runtime detection. Resolves to the most-capable kernel
/// that the build target supports.
#[cfg(not(feature = "std"))]
pub(crate) fn detect_cpu_kernel() -> CpuKernelTag {
    #[cfg(target_arch = "x86_64")]
    {
        #[cfg(all(
            target_feature = "avx512vbmi2",
            target_feature = "avx512f",
            target_feature = "avx512vl",
            target_feature = "avx512bw",
            target_feature = "bmi2"
        ))]
        {
            return CpuKernelTag::Vbmi2;
        }
        #[cfg(all(target_feature = "avx2", target_feature = "bmi2"))]
        {
            return CpuKernelTag::Avx2;
        }
        #[cfg(target_feature = "bmi2")]
        {
            return CpuKernelTag::Bmi2;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "sve")]
        {
            return CpuKernelTag::Sve;
        }
        #[cfg(target_feature = "neon")]
        {
            return CpuKernelTag::Neon;
        }
    }
    #[allow(unreachable_code)]
    CpuKernelTag::Scalar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_mask_lower_bits_zero_n_returns_zero() {
        assert_eq!(ScalarKernel::mask_lower_bits(0xDEADBEEF, 0), 0);
    }

    #[test]
    fn scalar_mask_lower_bits_full_64_returns_full_value() {
        assert_eq!(
            ScalarKernel::mask_lower_bits(0xFFFF_FFFF_FFFF_FFFF, 64),
            0xFFFF_FFFF_FFFF_FFFF
        );
    }

    #[test]
    fn scalar_mask_lower_bits_mid_keeps_low_n_bits() {
        // n=8: keep low 8 bits, zero the rest
        assert_eq!(ScalarKernel::mask_lower_bits(0xDEAD_BEEF, 8), 0xEF);
        assert_eq!(
            ScalarKernel::mask_lower_bits(0x0102_0304_0506_0708, 16),
            0x0708
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_mask_lower_bits_matches_scalar_on_bmi2_hw() {
        // Only run when BMI2 actually available — otherwise constructing
        // Avx2Kernel via dispatch wouldn't happen. Hardcode the run
        // when detected.
        #[cfg(feature = "std")]
        if !std::arch::is_x86_feature_detected!("bmi2") {
            return;
        }
        for n in 0..=64u8 {
            let v = 0x1234_5678_9ABC_DEF0u64;
            assert_eq!(
                Avx2Kernel::mask_lower_bits(v, n),
                ScalarKernel::mask_lower_bits(v, n),
                "mismatch at n={}",
                n
            );
        }
    }

    #[test]
    fn detect_returns_consistent_tag() {
        let first = detect_cpu_kernel();
        let second = detect_cpu_kernel();
        assert_eq!(
            first, second,
            "cached detect must return same tag on repeated calls"
        );
    }
}
