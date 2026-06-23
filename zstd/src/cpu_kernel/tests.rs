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

// Gated on `std` AND `kernel_avx2`: the `is_x86_feature_detected!`
// guard below is a no-op under `--no-default-features` (no std,
// no runtime feature detection), so the test body would call
// `Avx2Kernel::mask_lower_bits` unconditionally and SIGILL on any
// non-BMI2 CPU — hence `feature = "std"`. `Avx2Kernel` itself is
// `#[cfg(feature = "kernel_avx2")]`, so the test must also require
// that feature or a `std`-only trimmed build (`kernel_avx2` off)
// fails to compile against the undefined type.
#[cfg(all(target_arch = "x86_64", feature = "std", feature = "kernel_avx2"))]
#[test]
fn avx2_mask_lower_bits_matches_scalar_on_bmi2_hw() {
    // Only run when BMI2 actually available — otherwise constructing
    // Avx2Kernel via dispatch wouldn't happen.
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

/// Regression: a CPU advertising AVX-512 VBMI2 but NOT AVX2 (the
/// AMD64 baseline allows this combination at the spec level) was
/// previously selected as `Vbmi2`, which would SIGILL on the
/// first AVX2-mixed VBMI2 kernel invocation. The selection must
/// fall through to Scalar (or a non-AVX tier) in that case.
#[cfg(all(target_arch = "x86_64", feature = "kernel_vbmi2"))]
#[test]
fn select_x86_kernel_vbmi2_without_avx2_does_not_pick_vbmi2() {
    let tag = select_x86_kernel(
        /* avx512vbmi2 */ true, /* avx512f */ true, /* avx512vl */ true,
        /* avx512bw */ true, /* bmi2 */ true, /* avx2 */ false,
        /* sse2 */ true,
    );
    assert_ne!(
        tag,
        CpuKernelTag::Vbmi2,
        "selecting Vbmi2 without AVX2 would call AVX2 instructions and SIGILL"
    );
}

/// Sanity: when every flag is present the selector returns Vbmi2.
#[cfg(all(target_arch = "x86_64", feature = "kernel_vbmi2"))]
#[test]
fn select_x86_kernel_full_x86_v4_picks_vbmi2() {
    let tag = select_x86_kernel(true, true, true, true, true, true, true);
    assert_eq!(tag, CpuKernelTag::Vbmi2);
}

/// Sanity: AVX2 + BMI2 without AVX-512 → Avx2.
#[cfg(all(target_arch = "x86_64", feature = "kernel_avx2"))]
#[test]
fn select_x86_kernel_avx2_baseline_picks_avx2() {
    let tag = select_x86_kernel(false, false, false, false, true, true, true);
    assert_eq!(tag, CpuKernelTag::Avx2);
}

/// SSE2-only (no BMI2/AVX2) → Sse2, the x86_64 floor above Scalar.
#[cfg(all(target_arch = "x86_64", feature = "kernel_sse2"))]
#[test]
fn select_x86_kernel_sse2_only_picks_sse2() {
    let tag = select_x86_kernel(false, false, false, false, false, false, true);
    assert_eq!(tag, CpuKernelTag::Sse2);
}

/// No SIMD flags at all → Scalar (off-x86_64 / pre-SSE2 x86).
#[cfg(target_arch = "x86_64")]
#[test]
fn select_x86_kernel_no_features_picks_scalar() {
    let tag = select_x86_kernel(false, false, false, false, false, false, false);
    assert_eq!(tag, CpuKernelTag::Scalar);
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

#[test]
fn active_kernel_name_is_known_lowercase_tier() {
    // The diagnostic name must be one of the stable lowercase tier
    // strings the dashboard parses, and must match whatever tier
    // detection resolves to on this host (no `unknown` / empty leak).
    const KNOWN: &[&str] = &["scalar", "sse2", "bmi2", "avx2", "vbmi2", "neon", "sve"];
    let name = active_cpu_kernel_name();
    assert!(
        KNOWN.contains(&name),
        "active kernel name {name:?} is not a recognised tier"
    );
    assert_eq!(
        name,
        name.to_ascii_lowercase(),
        "tier name must be lowercase for stable dashboard parsing"
    );
}

#[test]
fn every_kernel_tag_maps_to_its_lowercase_name() {
    // `active_cpu_kernel_name` only exercises whichever arm the running
    // CPU resolves to, so map each constructible tag directly to cover
    // every branch on this build's feature set.
    assert_eq!(CpuKernelTag::Scalar.name(), "scalar");
    #[cfg(all(target_arch = "x86_64", feature = "kernel_sse2"))]
    assert_eq!(CpuKernelTag::Sse2.name(), "sse2");
    #[cfg(all(target_arch = "x86_64", feature = "kernel_bmi2"))]
    assert_eq!(CpuKernelTag::Bmi2.name(), "bmi2");
    #[cfg(all(target_arch = "x86_64", feature = "kernel_avx2"))]
    assert_eq!(CpuKernelTag::Avx2.name(), "avx2");
    #[cfg(all(target_arch = "x86_64", feature = "kernel_vbmi2"))]
    assert_eq!(CpuKernelTag::Vbmi2.name(), "vbmi2");
    #[cfg(all(target_arch = "aarch64", feature = "kernel_neon"))]
    assert_eq!(CpuKernelTag::Neon.name(), "neon");
    #[cfg(all(
        target_arch = "aarch64",
        feature = "kernel_sve",
        any(feature = "std", target_feature = "sve"),
    ))]
    assert_eq!(CpuKernelTag::Sve.name(), "sve");
}

#[test]
fn active_kernel_name_is_stable_across_calls() {
    // Backed by the cached `detect_cpu_kernel`, so repeated calls must
    // return the identical static string.
    assert_eq!(active_cpu_kernel_name(), active_cpu_kernel_name());
}
