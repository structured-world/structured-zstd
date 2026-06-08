//! Streaming RingBuffer decode timing at fast levels, with a C-streaming
//! control arm. Decodes a compressible frame (window << input, so the ring
//! cycles) via `StreamingDecoder` fresh per iteration (the create-decode-drop
//! pattern). `ring/cstream` is the cross-run-stable metric; compare it between
//! `main` and this branch to see the bounded-ring fix.
//!
//! `cargo run --release --example ring_grow_probe`

use std::io::Read;

use structured_zstd::decoding::StreamingDecoder;
use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, compress_with_parameters,
};

fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        for line in LINES {
            if bytes.len() == len {
                break;
            }
            let remaining = len - bytes.len();
            bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
        }
    }
    bytes
}

fn time_ring(compressed: &[u8], expected: usize, iters: u32) -> f64 {
    let mut out = Vec::with_capacity(expected);
    {
        let mut d = StreamingDecoder::new(compressed).unwrap();
        out.clear();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), expected);
    }
    let t = std::time::Instant::now();
    for _ in 0..iters {
        let mut d = StreamingDecoder::new(std::hint::black_box(compressed)).unwrap();
        out.clear();
        d.read_to_end(&mut out).unwrap();
        std::hint::black_box(&out[..]);
    }
    t.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

fn time_cstream(compressed: &[u8], expected: usize, iters: u32) -> f64 {
    let mut out = Vec::with_capacity(expected);
    {
        let mut d = zstd::stream::read::Decoder::new(compressed).unwrap();
        out.clear();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), expected);
    }
    let t = std::time::Instant::now();
    for _ in 0..iters {
        let mut d = zstd::stream::read::Decoder::new(std::hint::black_box(compressed)).unwrap();
        out.clear();
        d.read_to_end(&mut out).unwrap();
        std::hint::black_box(&out[..]);
    }
    t.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

fn main() {
    let raw = repeated_log_lines(8 * 1024 * 1024);
    let expected = raw.len();
    let iters = 50u32;
    println!(
        "== streaming ring decode at fast levels (8MiB, window_log 18), fresh decoder per iter =="
    );
    // Fast strategy: negative + level 1/2; level 3 = dfast for reference.
    for level in [-5, -3, -1, 1, 2, 3] {
        let params = CompressionParameters::builder(CompressionLevel::Level(level))
            .window_log(18)
            .build()
            .unwrap();
        let compressed = compress_with_parameters(&raw, &params);
        let ring = time_ring(&compressed, expected, iters);
        let cstream = time_cstream(&compressed, expected, iters);
        println!(
            "L{level:>3}  comp={:>8}  ring={ring:7.3}ms  cstream={cstream:7.3}ms  ring/cstream={:.3}x",
            compressed.len(),
            ring / cstream,
        );
    }
}
