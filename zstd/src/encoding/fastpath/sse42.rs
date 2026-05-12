//! x86/x86_64 SSE4.2 fastpath variant. Hot-path functions are marked
//! `#[target_feature(enable = "sse4.2")]` so intrinsics like `_mm_crc32_*` and
//! 128-bit SSE2 vector ops inline freely inside this module.
//!
//! Selected at runtime by [`super::detect_kernel_uncached`] when the running
//! CPU lacks AVX2/BMI2 (Avx2Bmi2 takes precedence when available).

#![cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#![allow(dead_code)]

pub(crate) const KERNEL_TAG: &str = "sse42";
