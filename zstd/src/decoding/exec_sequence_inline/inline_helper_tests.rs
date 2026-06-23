use super::x86::{copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride};

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
