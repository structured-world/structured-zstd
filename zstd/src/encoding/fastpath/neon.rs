//! AArch64 NEON fastpath variant. Every hot-path function in this module is
//! marked `#[target_feature(enable = "neon")]` so that the standard-library
//! NEON intrinsics (which themselves carry that attribute) inline directly
//! into the call graph instead of going through the function-call ABI barrier.
//!
//! NEON is part of the AArch64 baseline ISA — the attribute is therefore
//! redundant for correctness but mandatory for inline behavior under Rust's
//! ABI rules.

#![cfg(all(target_arch = "aarch64", target_endian = "little"))]

use core::arch::aarch64::{
    uint8x16_t, vandq_u8, vceqq_u8, vget_lane_u64, vld1q_u8, vminvq_u8, vreinterpret_u64_u8,
    vreinterpretq_u16_u8, vshrn_n_u16,
};

use super::scalar;

/// Index of the first unequal byte of a 16-byte `vceqq_u8` result that has
/// one. NEON has no byte mask move: narrowing each 16-bit lane by four keeps a
/// nibble per byte, in order, so the first zero nibble is the first mismatch.
#[target_feature(enable = "neon")]
#[inline]
fn first_unequal(eq: uint8x16_t) -> usize {
    let nibbles = vget_lane_u64(
        vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(eq), 4)),
        0,
    );
    ((!nibbles).trailing_zeros() / 4) as usize
}

/// NEON vector prefix-length probe. Returns the number of leading equal bytes
/// that fit in whole 16-byte chunks; the caller (or the wrapper below) handles
/// the scalar tail.
///
/// Compares 32 bytes a step with one branch: the two equality masks are
/// joined and their minimum lane tested, and the mismatch is only located on
/// the step that has one.
///
/// # Safety
/// `lhs` / `rhs` must point to at least `max` initialized bytes. NEON must be
/// available — guaranteed on AArch64 baseline but enforced by the
/// `target_feature` attribute.
#[target_feature(enable = "neon")]
#[inline]
pub(crate) unsafe fn prefix_len_simd(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
    let mut off = 0usize;
    while off + 32 <= max {
        let (eq0, eq1) = unsafe {
            (
                vceqq_u8(vld1q_u8(lhs.add(off)), vld1q_u8(rhs.add(off))),
                vceqq_u8(vld1q_u8(lhs.add(off + 16)), vld1q_u8(rhs.add(off + 16))),
            )
        };
        if vminvq_u8(vandq_u8(eq0, eq1)) != u8::MAX {
            if vminvq_u8(eq0) != u8::MAX {
                return off + first_unequal(eq0);
            }
            return off + 16 + first_unequal(eq1);
        }
        off += 32;
    }
    if off + 16 <= max {
        let eq = unsafe { vceqq_u8(vld1q_u8(lhs.add(off)), vld1q_u8(rhs.add(off))) };
        if vminvq_u8(eq) != u8::MAX {
            return off + first_unequal(eq);
        }
        off += 16;
    }
    off
}

/// NEON variant of `common_prefix_len_ptr`: SIMD vector loop, then the shared
/// scalar tail. Marked `target_feature(enable = "neon")` so callers inside
/// the same umbrella inline this into their hot loop without an ABI barrier.
///
/// # Safety
/// `lhs` / `rhs` must point to at least `max` initialized bytes.
#[target_feature(enable = "neon")]
#[inline]
pub(crate) unsafe fn common_prefix_len_ptr(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
    // Leading scalar word probe (mirrors upstream C `ZSTD_count`'s first
    // `MEM_readST` check): a prefix that diverges within the first 8 bytes
    // returns on one 8-byte read + count-trailing-zeros, skipping the vector
    // load. Longer matches fall through to the vector loop.
    //
    // This probe, not the vector loop, carries the BT path: instrumented over
    // decodecorpus-z000033, 87.45% of calls return here at both L19 and L22,
    // and the 12.55% that reach the vector average only 1.6 (L19) to 4.6 (L22)
    // 16-byte iterations. Widening the vector therefore buys almost nothing on
    // this path (measured: a wasm tier wired to its own vector probe came out
    // within noise of the scalar one); the lever is cutting the number of
    // candidate compares, not the bytes-per-iteration of the survivors.
    let chunk = core::mem::size_of::<usize>();
    if chunk <= max {
        let lhs_word = unsafe { core::ptr::read_unaligned(lhs.cast::<usize>()) };
        let rhs_word = unsafe { core::ptr::read_unaligned(rhs.cast::<usize>()) };
        let diff = lhs_word ^ rhs_word;
        if diff != 0 {
            return scalar::mismatch_byte_index(diff);
        }
    }
    let off = unsafe { prefix_len_simd(lhs, rhs, max) };
    unsafe { scalar::common_prefix_len_scalar_ptr(lhs, rhs, off, max) }
}

/// NEON variant of `count_match_from_indices` — the BT-walk match-length
/// probe entry point. Same invariants as the scalar variant but with the
/// NEON umbrella attribute so callers in Week 3a can adopt
/// `target_feature(enable = "neon")` themselves and get straight-line inlines.
///
/// # Safety
/// BT walk invariants: `candidate_idx + tail_limit ≤ concat.len()` and
/// `current_idx + tail_limit ≤ concat.len()`.
#[target_feature(enable = "neon")]
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

#[cfg(test)]
mod tests;
