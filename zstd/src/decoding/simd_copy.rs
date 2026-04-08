#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use core::arch::is_x86_feature_detected;
#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m128i, __m256i, __m512i, _mm_loadu_si128, _mm_storeu_si128, _mm256_loadu_si256,
    _mm256_storeu_si256, _mm512_loadu_si512, _mm512_storeu_si512,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m128i, __m256i, __m512i, _mm_loadu_si128, _mm_storeu_si128, _mm256_loadu_si256,
    _mm256_storeu_si256, _mm512_loadu_si512, _mm512_storeu_si512,
};

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
use core::arch::aarch64::{uint8x16_t, vld1q_u8, vst1q_u8};

#[inline(always)]
pub(crate) unsafe fn copy_bytes_overshooting(
    src: (*const u8, usize),
    dst: (*mut u8, usize),
    copy_at_least: usize,
) {
    if copy_at_least == 0 {
        return;
    }

    let min_buffer_size = core::cmp::min(src.1, dst.1);
    let chunk = copy_chunk_size();
    let copy_multiple = copy_at_least.next_multiple_of(chunk);

    if min_buffer_size >= copy_multiple {
        unsafe { copy_chunks(src.0, dst.0, copy_multiple, chunk) };
    } else {
        unsafe { dst.0.copy_from_nonoverlapping(src.0, copy_at_least) };
    }
}

#[inline(always)]
fn copy_chunk_size() -> usize {
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if is_x86_feature_detected!("avx512f") {
            return 64;
        }
        if is_x86_feature_detected!("avx2") {
            return 32;
        }
        if is_x86_feature_detected!("sse2") {
            return 16;
        }
        return core::mem::size_of::<usize>();
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        16
    }

    #[cfg(not(any(
        all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")),
        all(target_arch = "aarch64", target_feature = "neon")
    )))]
    {
        core::mem::size_of::<usize>()
    }
}

#[inline(always)]
unsafe fn copy_chunks(mut src: *const u8, mut dst: *mut u8, len: usize, chunk: usize) {
    let end = unsafe { src.add(len) };

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    unsafe {
        if chunk == 64 && is_x86_feature_detected!("avx512f") {
            copy_avx512(src, dst, end);
            return;
        }
        if chunk == 32 && is_x86_feature_detected!("avx2") {
            copy_avx2(src, dst, end);
            return;
        }
        if chunk == 16 && is_x86_feature_detected!("sse2") {
            copy_sse2(src, dst, end);
            return;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    unsafe {
        if chunk == 16 {
            copy_neon(src, dst, end);
            return;
        }
    }

    while src < end {
        unsafe {
            dst.cast::<usize>()
                .write_unaligned(src.cast::<usize>().read_unaligned());
            src = src.add(core::mem::size_of::<usize>());
            dst = dst.add(core::mem::size_of::<usize>());
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn copy_sse2(mut src: *const u8, mut dst: *mut u8, end: *const u8) {
    while src < end {
        unsafe {
            let v: __m128i = _mm_loadu_si128(src.cast::<__m128i>());
            _mm_storeu_si128(dst.cast::<__m128i>(), v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn copy_avx2(mut src: *const u8, mut dst: *mut u8, end: *const u8) {
    while src < end {
        unsafe {
            let v: __m256i = _mm256_loadu_si256(src.cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), v);
            src = src.add(32);
            dst = dst.add(32);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
unsafe fn copy_avx512(mut src: *const u8, mut dst: *mut u8, end: *const u8) {
    while src < end {
        unsafe {
            let v: __m512i = _mm512_loadu_si512(src.cast::<__m512i>());
            _mm512_storeu_si512(dst.cast::<__m512i>(), v);
            src = src.add(64);
            dst = dst.add(64);
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
unsafe fn copy_neon(mut src: *const u8, mut dst: *mut u8, end: *const u8) {
    while src < end {
        unsafe {
            let v: uint8x16_t = vld1q_u8(src);
            vst1q_u8(dst, v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}
