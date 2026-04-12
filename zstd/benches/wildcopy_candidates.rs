#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use rand::{RngExt, SeedableRng, rngs::StdRng};
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::hint::black_box;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use structured_zstd::testing::copy_bytes_overshooting_for_bench;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
type CopyKernel = unsafe fn(*const u8, *mut u8, usize);

#[derive(Clone, Copy)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
struct BenchPath {
    name: &'static str,
    chunk: usize,
    kernel: CopyKernel,
}

#[inline(always)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
unsafe fn copy_candidate_scalar(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: Benchmark callers provide valid non-overlapping buffers for `len`.
    unsafe {
        dst.copy_from_nonoverlapping(src, len);
    }
}

#[inline(always)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
unsafe fn copy_candidate_unroll2_avx2(src: *const u8, dst: *mut u8, len: usize) {
    if len >= 64 {
        // SAFETY: AVX2 support and copy bounds are checked by caller/path selection.
        unsafe {
            copy_unroll2_avx2_impl(src, dst, len);
        }
    } else {
        // SAFETY: Same preconditions as this function.
        unsafe {
            copy_candidate_scalar(src, dst, len);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn copy_unroll2_avx2_impl(mut src: *const u8, mut dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};

    let end_unrolled = len & !63;
    let mut copied = 0usize;
    while copied < end_unrolled {
        // SAFETY: Unrolled loop copies within caller-provided valid bounds.
        unsafe {
            let v0: __m256i = _mm256_loadu_si256(src.cast::<__m256i>());
            let v1: __m256i = _mm256_loadu_si256(src.add(32).cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), v0);
            _mm256_storeu_si256(dst.add(32).cast::<__m256i>(), v1);
            src = src.add(64);
            dst = dst.add(64);
        }
        copied += 64;
    }

    let tail = len - copied;
    if tail > 0 {
        // SAFETY: Remaining tail stays within the validated source/destination ranges.
        unsafe {
            dst.copy_from_nonoverlapping(src, tail);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn copy_baseline_avx2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};

    // SAFETY: `src..src+len` stays in caller-provided readable range.
    let end = unsafe { src.add(len) };
    while src < end {
        // SAFETY: Loop advances by vector width while staying inside valid ranges.
        unsafe {
            let v: __m256i = _mm256_loadu_si256(src.cast::<__m256i>());
            _mm256_storeu_si256(dst.cast::<__m256i>(), v);
            src = src.add(32);
            dst = dst.add(32);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn copy_baseline_sse2(mut src: *const u8, mut dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_storeu_si128};

    // SAFETY: `src..src+len` stays in caller-provided readable range.
    let end = unsafe { src.add(len) };
    while src < end {
        // SAFETY: Loop advances by vector width while staying inside valid ranges.
        unsafe {
            let v: __m128i = _mm_loadu_si128(src.cast::<__m128i>());
            _mm_storeu_si128(dst.cast::<__m128i>(), v);
            src = src.add(16);
            dst = dst.add(16);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
unsafe fn copy_baseline_avx512(mut src: *const u8, mut dst: *mut u8, len: usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{__m512i, _mm512_loadu_si512, _mm512_storeu_si512};
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{__m512i, _mm512_loadu_si512, _mm512_storeu_si512};

    // SAFETY: `src..src+len` stays in caller-provided readable range.
    let end = unsafe { src.add(len) };
    while src < end {
        // SAFETY: Loop advances by vector width while staying inside valid ranges.
        unsafe {
            let v: __m512i = _mm512_loadu_si512(src.cast::<__m512i>());
            _mm512_storeu_si512(dst.cast::<__m512i>(), v);
            src = src.add(64);
            dst = dst.add(64);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn select_candidate_copy_kernel(len: usize, baseline_path: BenchPath) -> Option<BenchPath> {
    if baseline_path.name == "baseline_avx512" {
        // Skip AVX2 candidate when production baseline is AVX-512.
        return None;
    }

    if std::arch::is_x86_feature_detected!("avx2") && len >= 64 {
        Some(BenchPath {
            name: "candidate_avx2_unroll2",
            chunk: baseline_path.chunk,
            kernel: copy_candidate_unroll2_avx2,
        })
    } else {
        Some(BenchPath {
            name: "candidate_scalar_fallback",
            chunk: baseline_path.chunk,
            kernel: copy_candidate_scalar,
        })
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn select_baseline_copy_kernel(len: usize) -> BenchPath {
    if std::arch::is_x86_feature_detected!("avx512f") && len >= 64 {
        BenchPath {
            name: "baseline_avx512",
            chunk: 64,
            kernel: copy_baseline_avx512,
        }
    } else if std::arch::is_x86_feature_detected!("avx2") && len >= 32 {
        BenchPath {
            name: "baseline_avx2",
            chunk: 32,
            kernel: copy_baseline_avx2,
        }
    } else if std::arch::is_x86_feature_detected!("sse2") && len >= 16 {
        BenchPath {
            name: "baseline_sse2",
            chunk: 16,
            kernel: copy_baseline_sse2,
        }
    } else {
        BenchPath {
            name: "baseline_scalar",
            chunk: core::mem::size_of::<usize>(),
            kernel: copy_candidate_scalar,
        }
    }
}

#[inline(always)]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
unsafe fn copy_with_overshoot_policy(
    src: (*const u8, usize),
    dst: (*mut u8, usize),
    copy_at_least: usize,
    path: BenchPath,
) {
    if copy_at_least == 0 {
        return;
    }

    let copy_multiple = copy_at_least.next_multiple_of(path.chunk);
    let min_capacity = core::cmp::min(src.1, dst.1);
    if min_capacity >= copy_multiple {
        // SAFETY: `copy_multiple` is bounded by both capacities and kernel contract.
        unsafe {
            (path.kernel)(src.0, dst.0, copy_multiple);
        }
    } else {
        // SAFETY: Fallback path copies exactly `copy_at_least` bytes within capacities.
        unsafe {
            dst.0.copy_from_nonoverlapping(src.0, copy_at_least);
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn bench_wildcopy_candidates(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(0x57A7_1DC0_0EED_1234);
    let mut group = c.benchmark_group("decode_wildcopy_candidates");
    let lengths = [
        17usize,
        33,
        63,
        64,
        65,
        256,
        1024,
        4096,
        16 * 1024,
        64 * 1024,
    ];

    for len in lengths {
        let baseline_path = select_baseline_copy_kernel(len);
        let candidate_path = select_candidate_copy_kernel(len, baseline_path);
        let mut src = vec![0u8; len + 64];
        rng.fill(&mut src);
        let mut dst_production = vec![0u8; len + 64];
        let mut dst_baseline = vec![0u8; len + 64];
        let mut dst_candidate = vec![0u8; len + 64];

        unsafe {
            copy_bytes_overshooting_for_bench(
                (src.as_ptr(), src.len()),
                (dst_production.as_mut_ptr(), dst_production.len()),
                len,
            );
            copy_with_overshoot_policy(
                (src.as_ptr(), src.len()),
                (dst_baseline.as_mut_ptr(), dst_baseline.len()),
                len,
                baseline_path,
            );
            if let Some(path) = candidate_path {
                copy_with_overshoot_policy(
                    (src.as_ptr(), src.len()),
                    (dst_candidate.as_mut_ptr(), dst_candidate.len()),
                    len,
                    path,
                );
            }
        }
        let check_len = len.next_multiple_of(baseline_path.chunk);
        assert_eq!(
            &dst_baseline[..check_len],
            &dst_production[..check_len],
            "baseline path must match production wildcopy for len={len}"
        );
        if candidate_path.is_none() {
            continue;
        }
        assert_eq!(
            &dst_candidate[..check_len],
            &dst_production[..check_len],
            "candidate path must match production wildcopy for len={len}"
        );

        group.throughput(Throughput::Bytes(len as u64));

        group.bench_with_input(
            BenchmarkId::new(baseline_path.name, len),
            &len,
            |b, &copy_len| {
                b.iter(|| unsafe {
                    let src_buf = black_box(src.as_slice());
                    let dst_buf = black_box(dst_baseline.as_mut_slice());
                    copy_with_overshoot_policy(
                        (black_box(src_buf.as_ptr()), src_buf.len()),
                        (black_box(dst_buf.as_mut_ptr()), dst_buf.len()),
                        black_box(copy_len),
                        baseline_path,
                    );
                    black_box(dst_buf[copy_len - 1]);
                });
            },
        );

        if let Some(candidate_path) = candidate_path {
            group.bench_with_input(
                BenchmarkId::new(candidate_path.name, len),
                &len,
                |b, &copy_len| {
                    b.iter(|| unsafe {
                        let src_buf = black_box(src.as_slice());
                        let dst_buf = black_box(dst_candidate.as_mut_slice());
                        copy_with_overshoot_policy(
                            (black_box(src_buf.as_ptr()), src_buf.len()),
                            (black_box(dst_buf.as_mut_ptr()), dst_buf.len()),
                            black_box(copy_len),
                            candidate_path,
                        );
                        black_box(dst_buf[copy_len - 1]);
                    });
                },
            );
        }
    }

    group.finish();
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
criterion_group!(benches, bench_wildcopy_candidates);
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
criterion_main!(benches);

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn main() {}
