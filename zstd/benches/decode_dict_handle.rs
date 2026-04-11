use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use structured_zstd::decoding::{Dictionary, DictionaryHandle, FrameDecoder};
use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

fn build_payload() -> Vec<u8> {
    let mut payload = Vec::with_capacity(512);
    for i in 0..512u16 {
        payload.push((i % 251) as u8);
    }
    payload
}

fn compress_with_dictionary(payload: &[u8], dict_raw: &[u8]) -> Vec<u8> {
    let dict = Dictionary::decode_dict(dict_raw).expect("dictionary should parse");
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor
        .set_dictionary(dict)
        .expect("dictionary should attach");
    compressor.set_source_size_hint(payload.len() as u64);
    compressor.set_source(payload);
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();
    compressed
}

fn bench_decode_dict_handle(c: &mut Criterion) {
    let dict_raw = include_bytes!("../dict_tests/dictionary");
    let payload = build_payload();
    let compressed = compress_with_dictionary(&payload, dict_raw.as_slice());
    let output_len = payload.len();

    let handle = DictionaryHandle::decode_dict(dict_raw).expect("dictionary should parse");

    let mut group = c.benchmark_group("decode/dict_handle");
    group.bench_with_input(
        BenchmarkId::new("prepared_handle", output_len),
        &output_len,
        |b, &len| {
            b.iter(|| {
                let mut decoder = FrameDecoder::new();
                let mut output = vec![0u8; len];
                decoder
                    .decode_all_with_dict_handle(compressed.as_slice(), &mut output, &handle)
                    .expect("decode should succeed");
                black_box(output);
            });
        },
    );

    group.bench_with_input(
        BenchmarkId::new("raw_dict_each_call", output_len),
        &output_len,
        |b, &len| {
            b.iter(|| {
                let mut decoder = FrameDecoder::new();
                let mut output = vec![0u8; len];
                decoder
                    .decode_all_with_dict_bytes(compressed.as_slice(), &mut output, dict_raw)
                    .expect("decode should succeed");
                black_box(output);
            });
        },
    );
    group.finish();
}

criterion_group!(benches, bench_decode_dict_handle);
criterion_main!(benches);
