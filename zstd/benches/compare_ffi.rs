//! Comparison benchmark: structured-zstd (pure Rust) vs zstd (C FFI).
//!
//! Five variations: decompress (pure Rust/C FFI), compress (pure Rust/C FFI L1/L3).
//! Both decompress benchmarks allocate per-iteration for symmetric comparison.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

/// Compressed corpus for decompression benchmarks.
const COMPRESSED_CORPUS: &[u8] = include_bytes!("../decodecorpus_files/z000033.zst");

fn bench_decompress(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompress");

    // Pre-compute expected output length for assertions.
    let expected_len = zstd::decode_all(COMPRESSED_CORPUS).unwrap().len();

    // Pure Rust decompression — allocate per-iteration (symmetric with C FFI).
    group.bench_function("pure_rust", |b| {
        b.iter(|| {
            let mut fr = structured_zstd::decoding::FrameDecoder::new();
            let mut target = vec![0u8; expected_len];
            let written = fr.decode_all(COMPRESSED_CORPUS, &mut target).unwrap();
            assert_eq!(written, expected_len);
        })
    });

    // C FFI decompression — allocates per-iteration via decode_all.
    group.bench_function("c_ffi", |b| {
        b.iter(|| {
            let out = zstd::decode_all(COMPRESSED_CORPUS).unwrap();
            assert_eq!(out.len(), expected_len);
        })
    });

    group.finish();
}

fn bench_compress(c: &mut Criterion) {
    // Get raw data by decompressing the corpus.
    let raw_data = zstd::decode_all(COMPRESSED_CORPUS).unwrap();

    let mut group = c.benchmark_group("compress");

    // Pure Rust compression (Fastest level)
    group.bench_with_input(
        BenchmarkId::new("pure_rust", "fastest"),
        &raw_data,
        |b, data| {
            b.iter(|| {
                black_box(structured_zstd::encoding::compress_to_vec(
                    &data[..],
                    structured_zstd::encoding::CompressionLevel::Fastest,
                ))
            })
        },
    );

    // C FFI compression (level 1 ≈ fastest)
    group.bench_with_input(BenchmarkId::new("c_ffi", "level1"), &raw_data, |b, data| {
        b.iter(|| black_box(zstd::encode_all(&data[..], 1).unwrap()))
    });

    // C FFI compression (level 3 ≈ default)
    group.bench_with_input(BenchmarkId::new("c_ffi", "level3"), &raw_data, |b, data| {
        b.iter(|| black_box(zstd::encode_all(&data[..], 3).unwrap()))
    });

    group.finish();
}

criterion_group!(benches, bench_decompress, bench_compress);
criterion_main!(benches);
