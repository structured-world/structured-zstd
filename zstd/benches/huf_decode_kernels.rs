use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use structured_zstd::decoding::FrameDecoder;
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

fn make_literals_heavy_payload(len: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(0xD3C0_DEC0_0BAD_5EED);
    let mut data = Vec::with_capacity(len);
    // Small alphabet forces strong Huffman skew in literals blocks.
    let alphabet: [u8; 16] = *b"ETAOINSHRDLCUMW?";
    let mut i = 0;
    while i < len {
        let run = 1 + (rng.random::<u8>() % 9) as usize;
        let sym = alphabet[(rng.random::<u8>() % alphabet.len() as u8) as usize];
        let mut j = 0;
        while j < run && i < len {
            data.push(sym);
            i += 1;
            j += 1;
        }
    }
    data
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_huf_kernels");

    let corpus_src = include_bytes!("../decodecorpus_files/z000033.zst").as_slice();
    let mut corpus_probe_decoder = FrameDecoder::new();
    let mut corpus_probe_target = vec![0u8; 1024 * 1024 * 200];
    let corpus_decoded = corpus_probe_decoder
        .decode_all(corpus_src, &mut corpus_probe_target)
        .unwrap();
    group.throughput(Throughput::Bytes(corpus_decoded as u64));
    let mut corpus_decoder = FrameDecoder::new();
    let mut corpus_target = vec![0u8; 1024 * 1024 * 200];
    group.bench_with_input(
        BenchmarkId::new("corpus_reference", "z000033.zst"),
        &corpus_src,
        |b, src| {
            b.iter(|| {
                corpus_decoder.decode_all(src, &mut corpus_target).unwrap();
            })
        },
    );

    let literals_heavy = make_literals_heavy_payload(16 * 1024 * 1024);
    let compressed = compress_to_vec(&literals_heavy[..], CompressionLevel::Default);
    group.throughput(Throughput::Bytes(literals_heavy.len() as u64));

    let mut heavy_decoder = FrameDecoder::new();
    let mut heavy_target = vec![0u8; literals_heavy.len()];
    group.bench_with_input(
        BenchmarkId::new("literals_heavy_default", "16MiB"),
        &compressed,
        |b, src| {
            b.iter(|| {
                heavy_decoder.decode_all(src, &mut heavy_target).unwrap();
            })
        },
    );

    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
