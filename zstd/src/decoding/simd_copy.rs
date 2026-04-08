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
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use std::arch::is_x86_feature_detected;
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use std::sync::OnceLock;

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
use core::arch::aarch64::{uint8x16_t, vld1q_u8, vst1q_u8};

type CopyFn = unsafe fn(*const u8, *mut u8, usize);

#[derive(Clone, Copy)]
struct CopyStrategy {
    chunk: usize,
    copy: CopyFn,
}

/// Copies at least `copy_at_least` bytes from `src` to `dst`.
///
/// This helper may over-copy up to
/// `copy_at_least.next_multiple_of(active_chunk)`, i.e. at most
/// `active_chunk - 1` extra bytes, mirroring zstd wildcopy semantics for
/// faster inner loops.
///
/// # Safety
/// Caller must guarantee:
/// - `src.0` points to at least `src.1` readable bytes.
/// - `dst.0` points to at least `dst.1` writable bytes.
/// - `copy_at_least <= src.1` and `copy_at_least <= dst.1`.
/// - `src.1` and `dst.1` are large enough for the selected strategy:
///   if `min(src.1, dst.1) >= next_multiple_of(copy_at_least, active_chunk)`,
///   the SIMD/scalar chunk loop may copy that rounded-up amount.
///   Otherwise the function copies exactly `copy_at_least` bytes.
/// - Source and destination regions do not overlap.
#[inline(always)]
pub(crate) unsafe fn copy_bytes_overshooting(
    src: (*const u8, usize),
    dst: (*mut u8, usize),
    copy_at_least: usize,
) {
    if copy_at_least == 0 {
        return;
    }

    let strategy = copy_strategy(copy_at_least);
    let min_buffer_size = core::cmp::min(src.1, dst.1);
    let copy_multiple = copy_at_least.next_multiple_of(strategy.chunk);

    if min_buffer_size >= copy_multiple {
        unsafe { (strategy.copy)(src.0, dst.0, copy_multiple) };
    } else {
        unsafe { dst.0.copy_from_nonoverlapping(src.0, copy_at_least) };
    }

    #[cfg(debug_assertions)]
    unsafe {
        let src_bytes = core::slice::from_raw_parts(src.0, copy_at_least);
        let dst_bytes = core::slice::from_raw_parts(dst.0, copy_at_least);
        debug_assert_eq!(dst_bytes, src_bytes);
    }
}

#[inline(always)]
fn scalar_strategy() -> CopyStrategy {
    CopyStrategy {
        chunk: core::mem::size_of::<usize>(),
        copy: copy_scalar,
    }
}

#[inline(always)]
fn copy_strategy(copy_at_least: usize) -> CopyStrategy {
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        let caps = detect_x86_caps();
        if caps.avx512f && copy_at_least >= 64 {
            return CopyStrategy {
                chunk: 64,
                copy: copy_avx512,
            };
        }
        if caps.avx2 && copy_at_least >= 32 {
            return CopyStrategy {
                chunk: 32,
                copy: copy_avx2,
            };
        }
        if caps.sse2 {
            return CopyStrategy {
                chunk: 16,
                copy: copy_sse2,
            };
        }
        scalar_strategy()
    }

    #[cfg(all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if cfg!(target_feature = "avx512f") && copy_at_least >= 64 {
            return CopyStrategy {
                chunk: 64,
                copy: copy_avx512,
            };
        }
        if cfg!(target_feature = "avx2") && copy_at_least >= 32 {
            return CopyStrategy {
                chunk: 32,
                copy: copy_avx2,
            };
        }
        if cfg!(target_feature = "sse2") {
            return CopyStrategy {
                chunk: 16,
                copy: copy_sse2,
            };
        }
        scalar_strategy()
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        if copy_at_least >= 16 {
            CopyStrategy {
                chunk: 16,
                copy: copy_neon,
            }
        } else {
            scalar_strategy()
        }
    }

    #[cfg(not(any(
        all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")),
        all(not(feature = "std"), any(target_arch = "x86", target_arch = "x86_64")),
        all(target_arch = "aarch64", target_feature = "neon")
    )))]
    {
        let _ = copy_at_least;
        scalar_strategy()
    }
}

#[cfg(test)]
#[inline]
pub(crate) fn active_chunk_size_for_tests() -> usize {
    copy_strategy(usize::MAX / 2).chunk
}

#[inline(always)]
unsafe fn copy_scalar(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            dst.cast::<usize>()
                .write_unaligned(src.cast::<usize>().read_unaligned());
            src = src.add(core::mem::size_of::<usize>());
            dst = dst.add(core::mem::size_of::<usize>());
        }
    }
}

#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
#[derive(Clone, Copy)]
struct X86Caps {
    avx512f: bool,
    avx2: bool,
    sse2: bool,
}

#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
fn detect_x86_caps() -> X86Caps {
    static CAPS: OnceLock<X86Caps> = OnceLock::new();
    *CAPS.get_or_init(|| X86Caps {
        avx512f: is_x86_feature_detected!("avx512f"),
        avx2: is_x86_feature_detected!("avx2"),
        sse2: is_x86_feature_detected!("sse2"),
    })
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn copy_sse2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
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
unsafe fn copy_avx2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
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
unsafe fn copy_avx512(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
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
unsafe fn copy_neon(mut src: *const u8, mut dst: *mut u8, len: usize) {
    let end = unsafe { src.add(len) };
    while src < end {
        unsafe {
            let v: uint8x16_t = vld1q_u8(src);
            vst1q_u8(dst, v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}
