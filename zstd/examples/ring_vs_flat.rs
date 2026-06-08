//! Diagnostic: isolate the RingBuffer (streaming) decode cost vs the
//! FlatBuf/UserSlice (slice) decode cost on identical compressed bytes.
//!
//! The streaming production path (`StreamingDecoder` -> `RingBuffer`) keeps
//! at most `window_size` live bytes, so on inputs larger than the window the
//! ring is perpetually wrapped (`head > tail`). `RingBuffer::inline_exec_ok`
//! vetoes the donor inline match-copy on a wrapped ring, so the whole frame
//! tail runs the slow `push`/`repeat` fallback. The flat path never wraps and
//! always takes the fast inline exec. This harness measures the gap and gives
//! a C one-shot reference.
//!
//! Run on the i9 (x86 AVX2): `cargo run --release --example ring_vs_flat`.

use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

use structured_zstd::decoding::{FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, compress_with_parameters,
};

fn repeated_pattern_bytes(len: usize) -> Vec<u8> {
    let pattern = b"coordinode:segment:0001|tenant=demo|label=orders|";
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let remaining = len - bytes.len();
        bytes.extend_from_slice(&pattern[..pattern.len().min(remaining)]);
    }
    bytes
}

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

fn time_flat(compressed: &[u8], expected: usize, iters: u32) -> f64 {
    let mut target = vec![0u8; expected + structured_zstd::WILDCOPY_OVERLENGTH];
    let mut decoder = FrameDecoder::new();
    // warm
    let w = decoder.decode_all(compressed, &mut target).unwrap();
    assert_eq!(w, expected);
    let start = Instant::now();
    for _ in 0..iters {
        let written = decoder
            .decode_all(black_box(compressed), &mut target)
            .unwrap();
        black_box(&target[..written]);
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

fn time_ring(compressed: &[u8], expected: usize, iters: u32) -> f64 {
    let mut out = Vec::with_capacity(expected);
    // warm
    {
        let mut dec = StreamingDecoder::new(compressed).unwrap();
        out.clear();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), expected);
    }
    let start = Instant::now();
    for _ in 0..iters {
        let mut dec = StreamingDecoder::new(black_box(compressed)).unwrap();
        out.clear();
        dec.read_to_end(&mut out).unwrap();
        black_box(&out[..]);
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

fn time_c_oneshot(compressed: &[u8], expected: usize, iters: u32) -> f64 {
    // warm
    let w = zstd::stream::decode_all(compressed).unwrap();
    assert_eq!(w.len(), expected);
    let start = Instant::now();
    for _ in 0..iters {
        let v = zstd::stream::decode_all(black_box(compressed)).unwrap();
        black_box(&v[..]);
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

/// C streaming decode (ZSTD_decompressStream via zstd::stream::read::Decoder):
/// the apples-to-apples peer to our `StreamingDecoder` ring path — C also keeps
/// a window buffer and flushes (double-copies) into `out`.
fn time_c_stream(compressed: &[u8], expected: usize, iters: u32) -> f64 {
    let mut out = Vec::with_capacity(expected);
    {
        let mut dec = zstd::stream::read::Decoder::new(compressed).unwrap();
        out.clear();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), expected);
    }
    let start = Instant::now();
    for _ in 0..iters {
        let mut dec = zstd::stream::read::Decoder::new(black_box(compressed)).unwrap();
        out.clear();
        dec.read_to_end(&mut out).unwrap();
        black_box(&out[..]);
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

fn run(name: &str, raw: &[u8], window_log: u32, iters: u32) {
    // Compress with an explicit small window so the streaming RingBuffer
    // genuinely cycles (window << src), the realistic streaming case the
    // dashboard's large-log-stream/low-entropy-1m hit. With src == window the
    // ring is near-full in steady state (no gap) and the inline path can never
    // fire regardless of the gate, so that degenerate case measures nothing.
    let params = CompressionParameters::builder(CompressionLevel::Level(3))
        .window_log(window_log)
        .build()
        .unwrap();
    let compressed = compress_with_parameters(raw, &params);
    let expected = raw.len();
    let flat = time_flat(&compressed, expected, iters);
    let ring = time_ring(&compressed, expected, iters);
    let c = time_c_oneshot(&compressed, expected, iters);
    let cs = time_c_stream(&compressed, expected, iters);
    println!(
        "{name:>16} wlog={window_log}  raw={:>9}  flat={flat:7.3}  ring={ring:7.3}  c1={c:7.3}  cstream={cs:7.3} ms  ring/flat={:.2}x  ring/cstream={:.2}x  ring/c1={:.2}x",
        expected,
        ring / flat,
        ring / cs,
        ring / c,
    );
}

fn main() {
    let mb = 1024 * 1024;
    println!(
        "== RingBuffer (stream) vs FlatBuf (slice) vs C one-shot, dfast level 3, small window =="
    );
    // window_log 18 = 256 KiB window; src 4/16 MiB => the ring cycles many times.
    run("low-entropy-4m", &repeated_pattern_bytes(4 * mb), 18, 30);
    run("low-entropy-16m", &repeated_pattern_bytes(16 * mb), 18, 15);
    run("large-log-4m", &repeated_log_lines(4 * mb), 18, 30);
    run("large-log-16m", &repeated_log_lines(16 * mb), 18, 15);
}
