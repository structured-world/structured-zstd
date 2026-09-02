use super::{FastpathKernel, detect_kernel_uncached, select_kernel};

#[test]
fn select_kernel_returns_supported_variant() {
    let k = select_kernel();
    // Cached and direct calls must agree.
    assert_eq!(k, detect_kernel_uncached());
    // Whatever the kernel is, it must be one of the variants compiled in
    // for this target.
    match k {
        FastpathKernel::Scalar => {}
        #[cfg(all(
            target_arch = "aarch64",
            target_endian = "little",
            feature = "kernel-neon"
        ))]
        FastpathKernel::Neon => {}
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-sse"
        ))]
        FastpathKernel::Sse2 => {}
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-sse"
        ))]
        FastpathKernel::Sse42 => {}
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-avx2"
        ))]
        FastpathKernel::Avx2Bmi2 => {}
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ))]
        FastpathKernel::Simd128 => {}
    }
}

/// NEON is the AArch64 baseline and the tier uses nothing beyond it, so the
/// dispatcher must pick it unconditionally. It used to also require the
/// optional `crc` extension, for a hash mix that no longer exists; a CPU
/// without `crc` dropping to the scalar kernel would be a regression.
#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
#[test]
fn aarch64_picks_neon_without_requiring_crc() {
    assert_eq!(detect_kernel_uncached(), FastpathKernel::Neon);
}

/// With the NEON tier compiled out there is nothing else on AArch64, so the
/// dispatcher must fall back to the scalar kernel rather than name a variant
/// that no longer exists.
#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    not(feature = "kernel-neon")
))]
#[test]
fn aarch64_falls_back_to_scalar_without_the_neon_feature() {
    assert_eq!(detect_kernel_uncached(), FastpathKernel::Scalar);
}
