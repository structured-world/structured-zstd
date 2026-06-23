use super::portable::{copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride};

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
    // Length 1 still triggers the unconditional first 16-byte store
    // — the wildcopy overshoots up to 15 bytes past the declared
    // end, the upstream zstd contract.
    let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
    let mut dst = [0u8; 32];
    unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 1) };
    assert_eq!(&dst[..16], &src[..16]);
    assert!(dst[16..].iter().all(|&b| b == 0));
}

#[test]
fn wildcopy_no_overlap_length_above_16_uses_multiple_iters() {
    // Length 24 → first 16-byte store, then one more iter
    // overshooting 8 bytes past the declared end.
    let src: [u8; 32] = core::array::from_fn(|i| (i + 1) as u8);
    let mut dst = [0u8; 32];
    unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), 24) };
    assert_eq!(&dst[..32], &src[..32]);
}

#[test]
fn wildcopy_overlap_8byte_stride_rle_expansion_offset_8() {
    // Offset = 8: src = dst - 8. Each 8-byte read picks up bytes the
    // previous iter just wrote, expanding the seed (RLE).
    let mut buf = [0u8; 32];
    buf[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    unsafe {
        wildcopy_overlap_8byte_stride(buf.as_mut_ptr().add(8), buf.as_ptr(), 16);
    }
    assert_eq!(&buf[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(&buf[16..24], &[1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn overlap_copy8_offset_ge_8_does_plain_copy() {
    // offset >= 8: straight ZSTD_copy8 (8-byte read+write).
    let mut buf = [0u8; 32];
    buf[..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
    let (op2, ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(8), buf.as_ptr(), 8) };
    assert_eq!(op2, unsafe { buf.as_mut_ptr().add(16) });
    assert_eq!(ip2, unsafe { buf.as_ptr().add(8) });
    assert_eq!(
        &buf[8..16],
        &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
    );
}

#[test]
fn overlap_copy8_offset_lt_8_spreads_source() {
    // offset < 8: dec32table / dec64table spread step. offset = 3.
    let mut buf = [0u8; 32];
    buf[..3].copy_from_slice(&[0xAA, 0xBB, 0xCC]);
    let (op2, _ip2) = unsafe { overlap_copy8(buf.as_mut_ptr().add(3), buf.as_ptr(), 3) };
    assert_eq!(op2, unsafe { buf.as_mut_ptr().add(11) });
    assert!(buf[3..11].iter().any(|&b| b != 0));
}

/// Cross-check: the portable helpers must produce byte-identical
/// output to a straightforward scalar reference for the no-overlap
/// copy across a range of lengths. Guards against a divergence
/// between the u128/u64 unaligned-move lowering and plain copies.
#[test]
fn wildcopy_no_overlap_matches_scalar_reference() {
    for len in 1usize..=48 {
        let src: [u8; 64] = core::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(1));
        let mut dst = [0u8; 64];
        unsafe { wildcopy_no_overlap(dst.as_mut_ptr(), src.as_ptr(), len) };
        // Only the first `len` bytes are contractually defined; the
        // overshoot tail is allowed to differ. Assert the defined
        // region matches.
        assert_eq!(&dst[..len], &src[..len], "len={len}");
    }
}
