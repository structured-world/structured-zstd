//! Microbenchmark for [`BitReaderReversed`] hot-path operations.
//!
//! Measures the throughput of the core bit-reading primitives that underlie
//! all entropy decoding (Huffman + FSE).

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

/// Build a test buffer: 1 KiB of pseudo-random bytes seeded deterministically.
fn make_test_data() -> Vec<u8> {
    let mut data = vec![0u8; 1024];
    // Simple LCG for reproducibility without pulling in `rand`
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for byte in data.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        *byte = (state >> 33) as u8;
    }
    data
}

fn bench_get_bits(c: &mut Criterion) {
    let data = make_test_data();
    let total_bits = data.len() * 8;

    let mut group = c.benchmark_group("bitstream/get_bits");

    let reads_9 = (total_bits / 9) as u64;
    let reads_11 = (total_bits / 11) as u64;

    group.throughput(Throughput::Elements(reads_9));
    group.bench_function("sequential_9bit", |b| {
        b.iter(|| {
            let mut br = structured_zstd::testing::BitReaderReversed::new(black_box(&data));
            let mut sum = 0u64;
            for _ in 0..reads_9 {
                sum = sum.wrapping_add(br.get_bits(9));
            }
            black_box(sum)
        })
    });

    group.throughput(Throughput::Elements(reads_11));
    group.bench_function("sequential_11bit", |b| {
        b.iter(|| {
            let mut br = structured_zstd::testing::BitReaderReversed::new(black_box(&data));
            let mut sum = 0u64;
            for _ in 0..reads_11 {
                sum = sum.wrapping_add(br.get_bits(11));
            }
            black_box(sum)
        })
    });

    group.finish();
}

fn bench_get_bits_triple(c: &mut Criterion) {
    let data = make_test_data();
    let total_bits = data.len() * 8;

    let mut group = c.benchmark_group("bitstream/get_bits_triple");
    let reads_triple = (total_bits / 26) as u64;
    group.throughput(Throughput::Elements(reads_triple));

    // Simulates FSE sequence decode: offset(8) + match(9) + literal(9) = 26 bits
    group.bench_function("fse_pattern_8_9_9", |b| {
        b.iter(|| {
            let mut br = structured_zstd::testing::BitReaderReversed::new(black_box(&data));
            let mut sum = 0u64;
            for _ in 0..reads_triple {
                let (a, b_val, c_val) = br.get_bits_triple(8, 9, 9);
                sum = sum.wrapping_add(a).wrapping_add(b_val).wrapping_add(c_val);
            }
            black_box(sum)
        })
    });

    group.finish();
}

fn bench_ensure_and_unchecked(c: &mut Criterion) {
    let data = make_test_data();
    let total_bits = data.len() * 8;

    let mut group = c.benchmark_group("bitstream/ensure_unchecked");
    let reads_unchecked = (total_bits / 26) as u64;
    group.throughput(Throughput::Elements(reads_unchecked));

    // Simulates interleaved FSE: one ensure_bits(26) then 3 unchecked reads
    group.bench_function("ensure26_3x_unchecked", |b| {
        b.iter(|| {
            let mut br = structured_zstd::testing::BitReaderReversed::new(black_box(&data));
            let mut sum = 0u64;
            for _ in 0..reads_unchecked {
                br.ensure_bits(26);
                sum = sum.wrapping_add(br.get_bits_unchecked(8));
                sum = sum.wrapping_add(br.get_bits_unchecked(9));
                sum = sum.wrapping_add(br.get_bits_unchecked(9));
            }
            black_box(sum)
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_get_bits,
    bench_get_bits_triple,
    bench_ensure_and_unchecked
);
criterion_main!(benches);
