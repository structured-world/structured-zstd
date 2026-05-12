//! x86/x86_64 AVX2 + BMI2 fastpath variant. Functions are marked
//! `#[target_feature(enable = "avx2,bmi2")]` so 256-bit vector intrinsics
//! (`_mm256_*`), BMI2 bit-manipulation (`_pext_u64`, `_bzhi_u64`), and SSE2/4.2
//! intrinsics all inline natively inside this module's hot loop.
//!
//! Selected at runtime when both feature sets are present (Haswell and newer
//! x86 CPUs, ~2013+).

#![cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#![allow(dead_code)]

#[cfg(target_arch = "x86")]
use core::arch::x86::{__m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{__m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};

use super::scalar;

pub(crate) const KERNEL_TAG: &str = "avx2_bmi2";

/// AVX2+BMI2 variant of `hash_mix_u64`. AVX2 itself doesn't change the hash
/// mix algorithm vs SSE4.2 (the CRC32 unit is the same), but having it on the
/// AVX2+BMI2 kernel lets callers in this module call into it without crossing
/// the SSE4.2-vs-AVX2 feature boundary.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2")]
#[inline]
pub(crate) unsafe fn hash_mix_u64(value: u64) -> u64 {
    // SAFETY: AVX2 implies SSE4.2 → CRC32 instruction available.
    let crc = unsafe {
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::_mm_crc32_u64;
        _mm_crc32_u64(0, value)
    };
    ((crc << 32) ^ value.rotate_left(13)).wrapping_mul(scalar::HASH_MIX_PRIME)
}

#[cfg(target_arch = "x86")]
#[target_feature(enable = "avx2,bmi2")]
#[inline]
pub(crate) unsafe fn hash_mix_u64(value: u64) -> u64 {
    scalar::hash_mix_u64(value)
}

/// 32-byte AVX2 vector prefix-length probe.
///
/// # Safety
/// `lhs` / `rhs` must point to at least `max` initialized bytes. AVX2 is
/// required and enforced by the `target_feature` attribute.
#[target_feature(enable = "avx2,bmi2")]
#[inline]
pub(crate) unsafe fn prefix_len_simd(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
    let mut off = 0usize;
    while off + 32 <= max {
        let a: __m256i = unsafe { _mm256_loadu_si256(lhs.add(off).cast::<__m256i>()) };
        let b: __m256i = unsafe { _mm256_loadu_si256(rhs.add(off).cast::<__m256i>()) };
        let eq = unsafe { _mm256_cmpeq_epi8(a, b) };
        let mask = unsafe { _mm256_movemask_epi8(eq) } as u32;
        if mask != u32::MAX {
            return off + (!mask).trailing_zeros() as usize;
        }
        off += 32;
    }
    off
}

/// AVX2+BMI2 variant of `common_prefix_len_ptr`. Vector loop then the shared
/// scalar tail.
///
/// # Safety
/// `lhs` / `rhs` must point to at least `max` initialized bytes.
#[target_feature(enable = "avx2,bmi2")]
#[inline]
pub(crate) unsafe fn common_prefix_len_ptr(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
    let off = unsafe { prefix_len_simd(lhs, rhs, max) };
    unsafe { scalar::common_prefix_len_scalar_ptr(lhs, rhs, off, max) }
}

/// AVX2+BMI2 variant of `count_match_from_indices`. Same invariants as the
/// scalar variant.
///
/// # Safety
/// BT walk invariants: `candidate_idx + tail_limit ≤ concat.len()` and
/// `current_idx + tail_limit ≤ concat.len()`.
#[target_feature(enable = "avx2,bmi2")]
#[inline]
pub(crate) unsafe fn count_match_from_indices(
    concat: &[u8],
    current_idx: usize,
    candidate_idx: usize,
    tail_limit: usize,
    seed_len: usize,
) -> usize {
    let seed = seed_len.min(tail_limit);
    if seed == tail_limit {
        return seed;
    }
    let remaining = tail_limit - seed;
    let base = concat.as_ptr();
    let lhs = unsafe { base.add(candidate_idx + seed) };
    let rhs = unsafe { base.add(current_idx + seed) };
    let extra = unsafe { common_prefix_len_ptr(lhs, rhs, remaining) };
    seed + extra
}
