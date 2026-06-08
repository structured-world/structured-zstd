//! Tight RingBuffer (streaming) decode loop for perf-record/flamegraph on the
//! i9. Decodes a 16 MiB low-entropy frame with a small (256 KiB) window via
//! `StreamingDecoder` (the wrapped-ring path) repeatedly, so the profile is
//! dominated by the ring decode hot path + drain, not setup.
//!
//! `cargo flamegraph --example decode_ring_loop --features dict_builder`
//! or `perf record -g -- target/release/examples/decode_ring_loop`.

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

fn main() {
    let raw = repeated_log_lines(16 * 1024 * 1024);
    let params = CompressionParameters::builder(CompressionLevel::Level(3))
        .window_log(18)
        .build()
        .unwrap();
    let compressed = compress_with_parameters(&raw, &params);
    let expected = raw.len();
    let mut out = Vec::with_capacity(expected);
    let iters: u32 = std::env::var("ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    for _ in 0..iters {
        let mut dec = StreamingDecoder::new(compressed.as_slice()).unwrap();
        out.clear();
        dec.read_to_end(&mut out).unwrap();
        std::hint::black_box(&out[..]);
    }
    assert_eq!(out.len(), expected);
}
