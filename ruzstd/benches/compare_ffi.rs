//! Comparison benchmark: structured-zstd (pure Rust) vs zstd (C FFI).

use criterion::{criterion_group, criterion_main, Criterion};

const CORPUS: &[u8] = include_bytes!("../decodecorpus_files/z000033.zst");

fn bench_pure_rust(c: &mut Criterion) {
    let mut fr = structured_zstd::decoding::FrameDecoder::new();
    let target = &mut vec![0u8; 1024 * 1024 * 200];

    c.bench_function("pure_rust_decode", |b| {
        b.iter(|| {
            fr.decode_all(CORPUS, target).unwrap();
        })
    });
}

fn bench_c_ffi(c: &mut Criterion) {
    // First decompress once to know the output size.
    let decompressed = zstd::decode_all(CORPUS).unwrap();
    let expected_len = decompressed.len();
    drop(decompressed);

    c.bench_function("c_ffi_decode", |b| {
        b.iter(|| {
            let out = zstd::decode_all(CORPUS).unwrap();
            assert_eq!(out.len(), expected_len);
        })
    });
}

criterion_group!(benches, bench_pure_rust, bench_c_ffi);
criterion_main!(benches);
