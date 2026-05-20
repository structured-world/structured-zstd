//! Forward match-length counter — direct port of donor's `ZSTD_count`
//! from `lib/compress/zstd_compress_internal.h`. Compares `pIn` against
//! `pMatch` in 8-byte chunks via XOR, falls back to a u32 / u16 / u8
//! tail. The first mismatching byte is located via `trailing_zeros()/8`
//! on the XOR difference, matching donor's `ZSTD_NbCommonBytes`.

/// Count the number of bytes that match starting at `ip` against the
/// reference at `match_ptr`, up to (but not including) `iend`. Returns
/// the match length in bytes — `0` if `*ip != *match_ptr`.
///
/// # Safety
///
/// - `ip` MUST point to `ip_len = (iend as usize) - (ip as usize)`
///   readable bytes.
/// - `match_ptr` MUST point to at least `ip_len + 7` readable bytes (the
///   8-byte chunked-load body reads past `ip_len` whenever the match
///   extends to the limit). The caller's prefix bookkeeping in the
///   donor encoder ensures this — `prefixStart` is always ≥ 8 bytes
///   before the first valid match position, and the trailing 7 bytes
///   of any frame are tagged as literals before this routine is
///   invoked (the `ilimit = iend - HASH_READ_SIZE` cap upstream).
/// - Neither pointer's range may overlap the destination of a
///   concurrent write — the kernel runs single-threaded over a
///   block-local input slice so this holds by construction.
///
/// # Equivalence to donor
///
/// Donor (`ZSTD_count` in `zstd_compress_internal.h`):
/// ```c
/// const BYTE* const pStart = pIn;
/// const BYTE* const pInLoopLimit = pInLimit - (sizeof(size_t)-1);
/// if (pIn < pInLoopLimit) {
///   { size_t const diff = MEM_readST(pMatch) ^ MEM_readST(pIn);
///     if (diff) return ZSTD_NbCommonBytes(diff); }
///   pIn += sizeof(size_t); pMatch += sizeof(size_t);
///   while (pIn < pInLoopLimit) { ... }
/// }
/// if (MEM_64bits() && pIn < pInLimit-3 && MEM_read32(pMatch) == MEM_read32(pIn)) { pIn+=4; pMatch+=4; }
/// if (pIn < pInLimit-1 && MEM_read16(pMatch) == MEM_read16(pIn)) { pIn+=2; pMatch+=2; }
/// if (pIn < pInLimit && *pMatch == *pIn) pIn++;
/// return (size_t)(pIn - pStart);
/// ```
///
/// The Rust port preserves the exact same chunk progression so a
/// future cross-check against the C reference can be byte-identical.
#[inline(always)]
pub(crate) unsafe fn count_forward(ip: *const u8, match_ptr: *const u8, iend: *const u8) -> usize {
    let p_start = ip;
    let mut ip = ip;
    let mut m = match_ptr;

    // 8-byte chunk loop. `loop_limit = iend - 7` ensures every chunked
    // read stays inside the caller's `[ip, iend)` source range.
    // SAFETY: iend ≥ ip + 7 in the only branch that enters the loop
    // (checked by the `(ip as usize) + 8 <= iend as usize` guard
    // before the read).
    while (ip as usize) + 8 <= (iend as usize) {
        // SAFETY: 8 readable bytes at both pointers per the function
        // contract; pointers are not const-aligned, so `read_unaligned`.
        let a = unsafe { core::ptr::read_unaligned(ip.cast::<u64>()) };
        let b = unsafe { core::ptr::read_unaligned(m.cast::<u64>()) };
        let diff = a ^ b;
        if diff != 0 {
            // Native-endian XOR — the byte ordering cancels out when
            // we ask "how many low-order bytes are equal", since both
            // operands were loaded with the same endianness.
            let common = (diff.trailing_zeros() / 8) as usize;
            // SAFETY: `common < 8` (otherwise `diff == 0`), and the
            // caller's source range covers ≥ `common` more bytes (we
            // already verified the 8-byte chunk is in range).
            return unsafe { ip.add(common).offset_from(p_start) as usize };
        }
        // SAFETY: pointer arithmetic stays within `[p_start, iend)`
        // because we just consumed an 8-byte chunk that fit in range.
        unsafe {
            ip = ip.add(8);
            m = m.add(8);
        }
    }

    // 4-byte tail.
    // SAFETY: bounds check `+ 4 <= iend` before the read; both ptrs
    // have at least `iend - ip` readable bytes by contract.
    if (ip as usize) + 4 <= (iend as usize) {
        let a = unsafe { core::ptr::read_unaligned(ip.cast::<u32>()) };
        let b = unsafe { core::ptr::read_unaligned(m.cast::<u32>()) };
        if a == b {
            // SAFETY: just verified 4 readable bytes; pointer add by 4
            // keeps the pointer ≤ iend.
            unsafe {
                ip = ip.add(4);
                m = m.add(4);
            }
        }
    }

    // 2-byte tail.
    if (ip as usize) + 2 <= (iend as usize) {
        let a = unsafe { core::ptr::read_unaligned(ip.cast::<u16>()) };
        let b = unsafe { core::ptr::read_unaligned(m.cast::<u16>()) };
        if a == b {
            // SAFETY: 2 readable bytes verified.
            unsafe {
                ip = ip.add(2);
                m = m.add(2);
            }
        }
    }

    // 1-byte tail.
    if (ip as usize) < (iend as usize) {
        // SAFETY: 1 readable byte verified.
        let a = unsafe { *ip };
        let b = unsafe { *m };
        if a == b {
            // SAFETY: 1 readable byte verified; pointer add by 1 keeps
            // ip ≤ iend.
            unsafe {
                ip = ip.add(1);
            }
        }
    }

    // SAFETY: ip is bounded by [p_start, iend], so the difference is
    // a non-negative isize that fits in usize.
    unsafe { ip.offset_from(p_start) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(a: &[u8], b: &[u8]) -> usize {
        let min_len = a.len().min(b.len());
        // SAFETY: both slices have at least `min_len` readable bytes,
        // `iend = a.as_ptr() + min_len` stays in range.
        unsafe { count_forward(a.as_ptr(), b.as_ptr(), a.as_ptr().add(min_len)) }
    }

    #[test]
    fn empty_inputs_return_zero() {
        // Empty range → loop body never executes.
        let a: [u8; 0] = [];
        let b: [u8; 0] = [];
        // SAFETY: iend == ip, the function never dereferences.
        let n = unsafe { count_forward(a.as_ptr(), b.as_ptr(), a.as_ptr()) };
        assert_eq!(n, 0);
    }

    #[test]
    fn full_match_inside_8_byte_chunk() {
        let a = [1, 2, 3, 4, 5, 6, 7, 8];
        let b = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(count(&a, &b), 8);
    }

    #[test]
    fn diff_at_byte_3_in_first_chunk() {
        let a = [1, 2, 3, 9, 5, 6, 7, 8];
        let b = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(count(&a, &b), 3);
    }

    #[test]
    fn match_spanning_two_chunks() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        for i in 0..16 {
            a[i] = i as u8;
            b[i] = i as u8;
        }
        a[13] = 99;
        assert_eq!(count(&a, &b), 13);
    }

    #[test]
    fn match_terminates_at_iend_within_tail() {
        // 11 bytes: 1×8-chunk + 3 tail bytes (u16 + u8 fall-through).
        let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        assert_eq!(count(&a, &b), 11);
    }

    #[test]
    fn diff_in_u32_tail() {
        // 12 bytes: 1×8-chunk match, then 4-byte tail diverges at
        // BYTE INDEX 9 (`99` vs `10`). After the 8-chunk advances
        // ip/m by 8, the donor's u32 tail check compares
        // a[8..12]=[9,99,11,12] vs b[8..12]=[9,10,11,12] → unequal,
        // so the u32 advance is skipped. Same for u16
        // (a[8..10]=[9,99] vs b[8..10]=[9,10] → unequal). The single
        // byte cmp THEN sees a[8]=9 == b[8]=9 and advances ip by 1.
        // Final match length: 8 + 1 = 9.
        let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 99, 11, 12];
        let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        assert_eq!(count(&a, &b), 9);
    }

    #[test]
    fn diff_in_u16_tail_after_u32_match() {
        // 14 bytes total, first 12 match, then u16 differs at byte 12.
        let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 99, 14];
        let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
        // 8 chunk + 4 u32 = 12 matched. u16 cmp on (99,14) vs (13,14)
        // says unequal → 0 more. Single byte cmp on 99 vs 13 → 0 more.
        assert_eq!(count(&a, &b), 12);
    }

    #[test]
    fn diff_in_single_byte_tail() {
        // 13 bytes, first 12 match, then single byte differs.
        let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 99];
        let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        // 8 chunk + 4 u32 = 12; u16 cmp not entered (only 1 byte left).
        // Single byte cmp 99 != 13 → 0 more.
        assert_eq!(count(&a, &b), 12);
    }

    #[test]
    fn long_match_thousand_bytes() {
        let a = [0x5Au8; 1024];
        let b = [0x5Au8; 1024];
        assert_eq!(count(&a, &b), 1024);
    }

    #[test]
    fn no_match_first_byte() {
        let a = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let b = [9u8, 1, 2, 3, 4, 5, 6, 7];
        assert_eq!(count(&a, &b), 0);
    }
}
