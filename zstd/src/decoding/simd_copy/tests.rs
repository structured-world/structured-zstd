use super::*;
use alloc::vec;

/// Copies `len` bytes with `copy` into a buffer fenced by sentinel bytes on
/// both sides, and checks the copy is exact: every byte of the source, and not
/// one byte written before or after it.
type CopyKernel = unsafe fn(*const u8, *mut u8, usize);

fn assert_exact_copy(name: &str, len: usize, copy: CopyKernel) {
    const FENCE: usize = 64;
    let src: vec::Vec<u8> = (0..len as u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let mut dst = vec![0xEEu8; FENCE + len + FENCE];
    unsafe { copy(src.as_ptr(), dst.as_mut_ptr().add(FENCE), len) };
    assert_eq!(&dst[FENCE..FENCE + len], &src[..], "{name} len={len}");
    assert!(
        dst[..FENCE]
            .iter()
            .chain(&dst[FENCE + len..])
            .all(|&b| b == 0xEE),
        "{name} len={len} wrote outside [0, len)"
    );
}

#[test]
fn small_exact_copy_writes_exactly_the_run() {
    // Encoder literal runs have no slack after them: a store past `len` would
    // overwrite the next run or the end of the buffer.
    for len in 1..=32usize {
        assert_exact_copy("copy_exact_small", len, copy_exact_small);
    }
}

/// Every medium exact-copy kernel this CPU can run. Each is swept on its own:
/// a kernel only the tier dispatch reaches would otherwise be checked only on
/// the hosts that select it.
fn runnable_medium_kernels() -> vec::Vec<(&'static str, CopyKernel)> {
    let mut kernels: vec::Vec<(&'static str, CopyKernel)> =
        vec![("copy_exact_u64", copy_exact_u64)];
    #[cfg(all(
        feature = "std",
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    if std::arch::is_x86_feature_detected!("sse2") {
        kernels.push(("copy_exact_sse2", copy_exact_sse2));
    }
    #[cfg(all(
        feature = "std",
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    if std::arch::is_x86_feature_detected!("avx2") {
        kernels.push(("copy_exact_avx2", copy_exact_avx2));
    }
    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        feature = "kernel-neon"
    ))]
    kernels.push(("copy_exact_neon", copy_exact_neon));
    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    kernels.push(("copy_exact_simd128", copy_exact_simd128));
    kernels
}

#[test]
fn medium_exact_copies_write_exactly_the_run() {
    // Every length, so each non-multiple of the 8/16/32-byte strides exercises
    // the kernel's overlapping tail.
    for (name, kernel) in runnable_medium_kernels() {
        for len in 33..2048usize {
            assert_exact_copy(name, len, kernel);
        }
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
    feature = "kernel-sse",
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
    feature = "kernel-avx2",
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
    feature = "kernel-avx2",
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
    feature = "kernel-avx2",
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
    feature = "kernel-vbmi2",
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
