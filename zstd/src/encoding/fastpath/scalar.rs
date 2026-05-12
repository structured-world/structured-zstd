//! Scalar fastpath variant — portable baseline used on targets without SIMD
//! intrinsics or when feature detection picks the fallback. Also provides the
//! shared scalar tail used by the SIMD variants once the vector loop has
//! consumed all whole chunks.
//!
//! No `#[target_feature]` attributes here: every function uses portable Rust
//! and must compile on any supported target.

#![allow(dead_code)]

pub(crate) const KERNEL_TAG: &str = "scalar";

/// Position of the first mismatching byte inside an 8-byte XOR diff. On
/// little-endian targets the low byte corresponds to the lowest address, so
/// `trailing_zeros / 8` is the index of the first non-equal byte.
#[inline(always)]
#[cfg(target_endian = "little")]
pub(crate) const fn mismatch_byte_index(diff: usize) -> usize {
    diff.trailing_zeros() as usize / 8
}

#[inline(always)]
#[cfg(target_endian = "big")]
pub(crate) const fn mismatch_byte_index(diff: usize) -> usize {
    diff.leading_zeros() as usize / 8
}

/// Multiplicative hash constant (Knuth golden ratio for 64-bit). Shared across
/// every variant — donor parity matches the C zstd value.
pub(crate) const HASH_MIX_PRIME: u64 = 0x9E37_79B1_85EB_CA87;

/// Pure scalar `hash_mix_u64` — single multiplication. Used by
/// `FastpathKernel::Scalar` and as the fallback when no CRC instruction is
/// available.
#[inline(always)]
pub(crate) fn hash_mix_u64(value: u64) -> u64 {
    value.wrapping_mul(HASH_MIX_PRIME)
}

/// Scalar prefix-length scan starting from `off` until `max`, using
/// word-sized XOR chunks then a byte tail. Callable from any target_feature
/// context — no SIMD intrinsics involved.
///
/// # Safety
/// `lhs` and `rhs` must point to at least `max` initialized bytes each.
#[inline(always)]
pub(crate) unsafe fn common_prefix_len_scalar_ptr(
    lhs: *const u8,
    rhs: *const u8,
    mut off: usize,
    max: usize,
) -> usize {
    let chunk = core::mem::size_of::<usize>();
    while off + chunk <= max {
        let lhs_word = unsafe { core::ptr::read_unaligned(lhs.add(off) as *const usize) };
        let rhs_word = unsafe { core::ptr::read_unaligned(rhs.add(off) as *const usize) };
        let diff = lhs_word ^ rhs_word;
        if diff != 0 {
            return off + mismatch_byte_index(diff);
        }
        off += chunk;
    }
    while off < max {
        if unsafe { *lhs.add(off) != *rhs.add(off) } {
            break;
        }
        off += 1;
    }
    off
}

/// Scalar-only common-prefix-length probe used by `FastpathKernel::Scalar`.
///
/// # Safety
/// `lhs` and `rhs` must point to at least `max` initialized bytes each.
#[inline(always)]
pub(crate) unsafe fn common_prefix_len_ptr(lhs: *const u8, rhs: *const u8, max: usize) -> usize {
    unsafe { common_prefix_len_scalar_ptr(lhs, rhs, 0, max) }
}

/// `count_match_from_indices` mirror for the scalar variant — kept here so
/// the variant module is a complete unit and BT-walk callers (Week 3a) can
/// pick the per-kernel implementation by symbol resolution rather than by a
/// branching dispatcher inside the hot loop.
///
/// # Safety
/// Caller-side BT-walk invariants ensure
/// `candidate_idx + tail_limit ≤ concat.len()` and
/// `current_idx + tail_limit ≤ concat.len()`.
#[inline(always)]
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
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn scalar_prefix_len_matches_naive_reference() {
        let a = b"abcdef_long string here for the test 1234567890";
        let mut b: Vec<u8> = a.to_vec();
        b[20] = b'!';
        let max = a.len().min(b.len());
        let len = unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
        assert_eq!(len, 20);
    }

    #[test]
    fn scalar_prefix_len_empty_inputs() {
        let p = core::ptr::NonNull::<u8>::dangling().as_ptr();
        assert_eq!(unsafe { common_prefix_len_ptr(p, p, 0) }, 0);
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn mismatch_byte_index_finds_low_bit_byte_le() {
        // diff == 0x_FF_00 -> mismatch is byte 1 (bits 8..16) on LE.
        let diff: usize = 0xff00;
        assert_eq!(mismatch_byte_index(diff), 1);
    }

    #[test]
    #[cfg(target_endian = "big")]
    fn mismatch_byte_index_finds_low_bit_byte_be() {
        // Mirror of the LE case for the BE branch: the LSB byte sits at the
        // tail of the word, so the mismatch byte index is `bytes_per_usize-1`.
        let diff: usize = 0xff;
        assert_eq!(mismatch_byte_index(diff), core::mem::size_of::<usize>() - 1);
    }
}
