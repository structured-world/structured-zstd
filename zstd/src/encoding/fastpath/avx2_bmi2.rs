//! x86/x86_64 AVX2 + BMI2 fastpath variant. Functions are marked
//! `#[target_feature(enable = "avx2,bmi2")]` so 256-bit vector intrinsics
//! (`_mm256_*`), BMI2 bit-manipulation (`_pext_u64`, `_bzhi_u64`), and SSE2/4.2
//! intrinsics all inline natively inside this module's hot loop.
//!
//! Selected at runtime when both feature sets are present (Haswell and newer
//! x86 CPUs, ~2013+).

#![cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#![allow(dead_code)]

pub(crate) const KERNEL_TAG: &str = "avx2_bmi2";
