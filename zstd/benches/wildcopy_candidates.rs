use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use std::hint::black_box;
use structured_zstd::testing::copy_bytes_overshooting_for_bench;

#[inline(always)]
unsafe fn copy_candidate_unroll2_avx2(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::arch::is_x86_feature_detected!("avx2") && len >= 64 {
            unsafe { copy_unroll2_avx2_impl(src, dst, len) };
            return;
        }
    }

    unsafe { dst.copy_from_nonoverlapping(src, len) };
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
        unsafe { dst.copy_from_nonoverlapping(src, tail) };
    }
}

fn bench_wildcopy_candidates(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(0x57A7_1DC0_0EED_1234);
    let mut group = c.benchmark_group("decode_wildcopy_candidates");
    let lengths = [64usize, 256, 1024, 4096, 16 * 1024, 64 * 1024];

    for len in lengths {
        let mut src = vec![0u8; len + 64];
        rng.fill(&mut src);
        let mut dst_baseline = vec![0u8; len + 64];
        let mut dst_candidate = vec![0u8; len + 64];

        unsafe {
            copy_bytes_overshooting_for_bench(
                (src.as_ptr(), src.len()),
                (dst_baseline.as_mut_ptr(), dst_baseline.len()),
                len,
            );
            copy_candidate_unroll2_avx2(src.as_ptr(), dst_candidate.as_mut_ptr(), len);
        }
        assert_eq!(
            &dst_baseline[..len],
            &src[..len],
            "baseline wildcopy must preserve bytes for len={len}"
        );
        assert_eq!(
            &dst_candidate[..len],
            &src[..len],
            "candidate wildcopy must preserve bytes for len={len}"
        );

        group.throughput(Throughput::Bytes(len as u64));

        group.bench_with_input(BenchmarkId::new("baseline", len), &len, |b, &copy_len| {
            b.iter(|| unsafe {
                copy_bytes_overshooting_for_bench(
                    (src.as_ptr(), src.len()),
                    (dst_baseline.as_mut_ptr(), dst_baseline.len()),
                    copy_len,
                );
                black_box(dst_baseline[copy_len - 1]);
            });
        });

        group.bench_with_input(
            BenchmarkId::new("candidate_avx2_unroll2", len),
            &len,
            |b, &copy_len| {
                b.iter(|| unsafe {
                    copy_candidate_unroll2_avx2(src.as_ptr(), dst_candidate.as_mut_ptr(), copy_len);
                    black_box(dst_candidate[copy_len - 1]);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_wildcopy_candidates);
criterion_main!(benches);
