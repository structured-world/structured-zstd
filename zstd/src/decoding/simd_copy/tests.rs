use super::*;
use alloc::vec;

#[test]
fn copy_exact_medium_matches_memcpy_all_sizes() {
    // Exact byte-for-byte equivalence across the medium range, incl.
    // non-multiples of every tier width (16/32) so the overlapping
    // tail is exercised.
    let src: vec::Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    for len in 33..2048usize {
        let mut got = vec![0u8; len];
        unsafe { copy_exact_medium(src.as_ptr(), got.as_mut_ptr(), len) };
        assert_eq!(
            &got[..],
            &src[..len],
            "copy_exact_medium mismatch at len={len}"
        );
    }
}

#[test]
fn copy_bytes_overshooting_zero_len_is_noop() {
    let src = [1_u8, 2, 3, 4];
    let mut dst = [9_u8, 9, 9, 9];
    unsafe {
        copy_bytes_overshooting((src.as_ptr(), src.len()), (dst.as_mut_ptr(), dst.len()), 0);
    }
    assert_eq!(dst, [9_u8, 9, 9, 9]);
}

#[test]
fn copy_bytes_overshooting_fallback_exact_copy_when_caps_are_tight() {
    // Pick a size that exceeds the single-op fast path threshold (16)
    // and the next chunk size on every supported arch, so the fallback
    // path is exercised regardless of which kernel a given build picks.
    let len = 65; // > AVX-512 chunk
    let src = vec![5_u8; len];
    let mut dst = vec![0_u8; len];

    unsafe {
        copy_bytes_overshooting((src.as_ptr(), len), (dst.as_mut_ptr(), len), len);
    }

    assert_eq!(dst, src);
}

#[test]
fn copy_bytes_overshooting_single_op_small() {
    // Sub-16 copy with full 16-byte slack on both sides: single-op fast
    // path covers it via one SIMD store (or two overlapping u64 stores
    // on archs without 128-bit SIMD).
    for len in 1..=16 {
        let mut src = [0u8; 32];
        for (i, b) in src.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut dst = [0u8; 32];
        unsafe {
            copy_bytes_overshooting((src.as_ptr(), 32), (dst.as_mut_ptr(), 32), len);
        }
        assert_eq!(&dst[..len], &src[..len], "len={len}");
    }
}

#[test]
fn copy_scalar_copies_requested_bytes() {
    let src = [11_u8, 12, 13, 14, 15, 16, 17, 18];
    let mut dst = [0_u8; 8];
    unsafe { copy_scalar(src.as_ptr(), dst.as_mut_ptr(), src.len()) };
    assert_eq!(dst, src);
}

#[cfg(all(
    feature = "std",
    feature = "kernel_sse2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[test]
fn copy_sse2_copies_full_chunk_when_available() {
    if !std::arch::is_x86_feature_detected!("sse2") {
        return;
    }
    let src = [7_u8; 16];
    let mut dst = [0_u8; 16];
    unsafe { copy_sse2(src.as_ptr(), dst.as_mut_ptr(), 16) };
    assert_eq!(dst, src);
}

#[cfg(all(
    feature = "std",
    feature = "kernel_avx2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[test]
fn copy_avx2_copies_full_chunk_when_available() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    // Single 32-byte vector (no unrolled body, tail-only path).
    let src = [8_u8; 32];
    let mut dst = [0_u8; 32];
    unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 32) };
    assert_eq!(dst, src);
}

/// Exercises one full iteration of the 64-byte unrolled body
/// (`v0` + `v1` load/store pair) with no residual tail.
#[cfg(all(
    feature = "std",
    feature = "kernel_avx2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[test]
fn copy_avx2_copies_full_unroll2_iteration() {
    use alloc::vec::Vec;
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    let src: Vec<u8> = (0..64u8).collect();
    let mut dst = [0_u8; 64];
    unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 64) };
    assert_eq!(&dst[..], &src[..]);
}

/// Exercises ONE unrolled 64-byte iteration PLUS the single-
/// vector 32-byte residual tail (96 = 64 + 32). Validates that
/// the tail branch doesn't overwrite preceding bytes and copies
/// the correct source offset.
#[cfg(all(
    feature = "std",
    feature = "kernel_avx2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[test]
fn copy_avx2_copies_unroll2_loop_plus_residual_tail() {
    use alloc::vec::Vec;
    if !std::arch::is_x86_feature_detected!("avx2") {
        return;
    }
    let src: Vec<u8> = (0..96u8).collect();
    let mut dst = [0_u8; 96];
    unsafe { copy_avx2(src.as_ptr(), dst.as_mut_ptr(), 96) };
    assert_eq!(&dst[..], &src[..]);
    // Spot-check tail boundary: bytes 60..68 span the unroll/tail seam.
    assert_eq!(&dst[60..68], &[60, 61, 62, 63, 64, 65, 66, 67]);
}

#[cfg(all(
    feature = "std",
    feature = "kernel_vbmi2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
#[test]
fn copy_avx512_copies_full_chunk_when_available() {
    if !std::arch::is_x86_feature_detected!("avx512f") {
        return;
    }
    let src = [9_u8; 64];
    let mut dst = [0_u8; 64];
    unsafe { copy_avx512(src.as_ptr(), dst.as_mut_ptr(), 64) };
    assert_eq!(dst, src);
}
