//! Scalar fastpath variant — portable baseline used on targets without SIMD
//! intrinsics or when feature detection picks the fallback. Also provides the
//! shared scalar tail used by the SIMD variants once the vector loop has
//! consumed all whole chunks.
//!
//! No `#[target_feature]` attributes here: every function uses portable Rust
//! and must compile on any supported target.

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

/// `count_match_from_indices` mirror for the scalar variant, so BT-walk
/// callers pick the per-kernel implementation by symbol resolution rather
/// than through a branching dispatcher inside the hot loop.
///
/// Unused on little-endian aarch64 when the NEON tier is compiled in: the
/// scalar BT collect-matches walker is then compiled out itself
/// (`MatchTable::bt_insert_and_collect_matches_scalar` is gated on not having
/// that combination), since NEON is baseline there and the scalar walker can
/// never be selected. With `kernel-neon` off it becomes the live path again.
///
/// # Safety
/// Caller-side BT-walk invariants ensure
/// `candidate_idx + tail_limit ≤ concat.len()` and
/// `current_idx + tail_limit ≤ concat.len()`.
#[cfg_attr(
    all(
        target_arch = "aarch64",
        target_endian = "little",
        feature = "kernel-neon"
    ),
    allow(dead_code)
)]
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
mod tests;
