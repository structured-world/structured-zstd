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
use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

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

fn run(name: &str, raw: &[u8], iters: u32) {
    let compressed = compress_slice_to_vec(raw, CompressionLevel::Level(3));
    let expected = raw.len();
    let flat = time_flat(&compressed, expected, iters);
    let ring = time_ring(&compressed, expected, iters);
    let c = time_c_oneshot(&compressed, expected, iters);
    println!(
        "{name:>20}  raw={:>8}  comp={:>8}  flat={flat:7.3}ms  ring={ring:7.3}ms  c={c:7.3}ms  ring/flat={:.2}x  ring/c={:.2}x  flat/c={:.2}x",
        expected,
        compressed.len(),
        ring / flat,
        ring / c,
        flat / c,
    );
}

fn main() {
    let mb = 1024 * 1024;
    let iters = 50;
    println!("== RingBuffer (stream) vs FlatBuf (slice) vs C one-shot, dfast level 3 ==");
    run("low-entropy-1m", &repeated_pattern_bytes(mb), iters);
    run("low-entropy-4m", &repeated_pattern_bytes(4 * mb), iters);
    run("large-log-1m", &repeated_log_lines(mb), iters);
    run("large-log-4m", &repeated_log_lines(4 * mb), iters);
}
