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
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        FastpathKernel::Neon => {}
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        FastpathKernel::Sse42 => {}
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        FastpathKernel::Avx2Bmi2 => {}
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel_simd128"
        ))]
        FastpathKernel::Simd128 => {}
    }
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[test]
fn aarch64_picks_neon_when_crc_available() {
    // The dispatcher gates the NEON kernel on both `neon` (baseline)
    // and the optional `crc` extension. Mirror that runtime/compile-time
    // gate so the test stays accurate on AArch64 CPUs (or CI runners)
    // where `crc` is not reported.
    #[cfg(feature = "std")]
    let crc_available = std::arch::is_aarch64_feature_detected!("crc");
    #[cfg(not(feature = "std"))]
    let crc_available = cfg!(target_feature = "crc");

    let expected = if crc_available {
        FastpathKernel::Neon
    } else {
        FastpathKernel::Scalar
    };
    assert_eq!(detect_kernel_uncached(), expected);
}
