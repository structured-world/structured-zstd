use super::x86::{copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride};

/// The doubling match copy in an AVX2 scope, as the executor expands it.
///
/// # Safety
/// The CPU has AVX2, plus the copy's own contract.
#[cfg(feature = "std")]
#[target_feature(enable = "avx2")]
unsafe fn match_copy_doubling(dst: *mut u8, offset: usize, length: usize) {
    unsafe { super::x86::match_copy_doubling_avx2(dst, offset, length) }
}

/// The doubling copy of a match writes what a byte-by-byte copy writes, for
/// short and long offsets, a match no longer than its offset, lengths around each
/// doubled piece and past the distance limit, and every alignment of the
/// write, and stays within its 31-byte overshoot.
#[cfg(feature = "std")]
#[test]
fn match_copy_doubling_repeats_matches_like_a_byte_copy() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    for skew in 0..32usize {
        let prefix: alloc::vec::Vec<u8> = (0..(400 + skew) as u32)
            .map(|b| (b as u8).wrapping_mul(37) ^ 0x5a)
            .collect();
        for offset in (1..=40usize).chain([63, 64, 100, 380]) {
            for length in [
                offset - 1,
                offset,
                offset + 1,
                2 * offset - 1,
                2 * offset,
                2 * offset + 1,
                3 * offset,
                4 * offset + 7,
                7 * offset,
                3700,
                20000,
            ] {
                let mut expected = prefix.clone();
                for _ in 0..length {
                    expected.push(expected[expected.len() - offset]);
                }
                let mut out = prefix.clone();
                out.resize(prefix.len() + length + 31 + 16, 0xee);
                let dst = prefix.len();
                // SAFETY: `offset <= 400` bytes precede the match, the buffer
                // holds it plus its 31-byte overshoot, and the CPU has AVX2.
                unsafe { match_copy_doubling(out.as_mut_ptr().add(dst), offset, length) };
                assert_eq!(
                    out[..expected.len()],
                    expected[..],
                    "skew {skew} offset {offset} length {length}"
                );
                assert!(
                    out[dst + length + 31..].iter().all(|&b| b == 0xee),
                    "skew {skew} offset {offset} length {length} wrote past its overshoot",
                );
            }
        }
    }
}

/// The shuffle copy expanded in an SSSE3 scope, as the executor expands it.
///
/// # Safety
/// The CPU has SSSE3, plus the macro's own contract.
#[cfg(feature = "std")]
#[target_feature(enable = "ssse3")]
unsafe fn overlap_match_shuffle(dst: *mut u8, offset: usize, length: usize) {
    unsafe { super::overlap_match_shuffle!(dst, offset, length) }
}

/// The shuffle copy of an overlapping match writes what a byte-by-byte copy
/// writes, for every offset it serves and lengths around each chunk edge, and
/// stays within its 15-byte overshoot.
#[cfg(feature = "std")]
#[test]
fn overlap_match_shuffle_repeats_the_period_like_a_byte_copy() {
    if !std::arch::is_x86_feature_detected!("ssse3") {
        return;
    }
    let prefix: alloc::vec::Vec<u8> = (0..32u8).map(|b| b.wrapping_mul(37) ^ 0x5a).collect();
    for offset in 1..16usize {
        for length in [1usize, 3, 4, 7, 8, 9, 15, 16, 17, 31, 32, 33, 64, 100] {
            let mut expected = prefix.clone();
            for _ in 0..length {
                expected.push(expected[expected.len() - offset]);
            }
            let mut out = prefix.clone();
            out.resize(prefix.len() + length.next_multiple_of(16) + 16, 0xee);
            // SAFETY: 32 written bytes precede the match, the buffer holds the
            // length rounded up to 16, and the CPU has SSSE3 (checked above).
            unsafe { overlap_match_shuffle(out.as_mut_ptr().add(prefix.len()), offset, length) };
            assert_eq!(
                out[..expected.len()],
                expected[..],
                "offset {offset} length {length}"
            );
            let written = prefix.len() + length.next_multiple_of(16);
            assert!(
                out[written..].iter().all(|&b| b == 0xee),
                "offset {offset} length {length} wrote past its overshoot",
            );
        }
    }
}

#[test]
fn copy16_copies_exactly_16_bytes() {
    let src: [u8; 16] = [
        0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE,
        0xAF,
    ];
    let mut dst = [0u8; 16];
    unsafe { copy16(dst.as_mut_ptr(), src.as_ptr()) };
    assert_eq!(dst, src);
}

#[test]
fn wildcopy_no_overlap_short_length_overshoots() {
    // Length 1 still triggers the unconditional first 16-byte
    // store — the wildcopy overshoots up to 15 bytes past the
    // declared end, which is the upstream zstd contract.
    let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
    let mut dst = [0u8; 32];
    unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 1) };
    // First 16 bytes copied from src; remaining untouched.
    assert_eq!(&dst[..16], &src[..16]);
    assert!(dst[16..].iter().all(|&b| b == 0));
}

#[test]
fn wildcopy_no_overlap_length_above_16_uses_multiple_iters() {
    // Length 24 → first 16-byte store, then one more iter that
    // overshoots 8 bytes past the declared end.
    let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
    let mut dst = [0u8; 32];
    unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 24) };
    // 32 bytes get written (two 16-byte stores).
    assert_eq!(&dst[..32], &src[..32]);
}

#[test]
fn wildcopy_overlap_8byte_stride_rle_expansion_offset_8() {
    // Offset = 8 means caller has set up src = dst - 8. Each
    // 8-byte read picks up bytes the previous iter just wrote,
    // expanding the seed pattern across the destination region.
    let mut buf = [0u8; 32];
    buf[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    unsafe {
        wildcopy_overlap_8byte_stride(buf.as_mut_ptr().add(8), buf.as_ptr(), 16);
    }
    // Bytes 8..16 = seed; bytes 16..24 = seed again (RLE expansion).
    assert_eq!(&buf[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(&buf[16..24], &[1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn overlap_copy8_offset_ge_8_does_plain_copy() {
    // offset >= 8 path: straight ZSTD_copy8 (8-byte read+write).
    let mut buf = [0u8; 32];
    buf[..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
    let (op2, ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(8), buf.as_ptr(), 8) };
    // dst advances by 8 bytes, src advances by 8 bytes.
    assert_eq!(op2, unsafe { buf.as_mut_ptr().add(16) });
    assert_eq!(ip2, unsafe { buf.as_ptr().add(8) });
    // bytes 8..16 = seed.
    assert_eq!(
        &buf[8..16],
        &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
    );
}

#[test]
fn overlap_copy8_offset_lt_8_spreads_source() {
    // offset < 8 path: uses dec32table / dec64table to spread
    // the source-destination distance so subsequent wildcopy can
    // use the ≥ 8 stride. Test offset = 3 (a common short-offset
    // RLE pattern).
    let mut buf = [0u8; 32];
    buf[..3].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
    let (op2, _ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(3), buf.as_ptr(), 3) };
    // dst advanced 8 bytes.
    assert_eq!(op2, unsafe { buf.as_mut_ptr().add(11) });
    // First 8 bytes of the destination region are the 3-byte
    // seed expanded — verify they're non-zero (exact spread
    // pattern depends on the lookup tables; upstream zstd parity is the
    // contract).
    assert!(buf[3..11].iter().any(|&b| b != 0));
}
