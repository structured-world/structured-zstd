//! Decode one v0.5-v0.7 frame from a file in a tight loop, for timing the
//! legacy decoders on a frame an old release wrote.
//!
//! Build: cargo build --profile bench -p structured-zstd --example legacy_decode_loop
//! Run:   target/release/examples/legacy_decode_loop <frame> <iters> [whole|stream]

use std::env;
use std::io::Read as _;

use structured_zstd::decoding::{FrameDecoder, StreamingDecoder, frame_decompressed_bound};

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(1)
        .expect("usage: legacy_decode_loop <frame> <iters> [whole|stream]");
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let mode = args.get(3).map(String::as_str).unwrap_or("whole");
    let frame = std::fs::read(path).expect("read frame");
    let bound = frame_decompressed_bound(&frame).expect("a v0.5-v0.7 frame") as usize;
    let mut target = vec![0u8; bound];
    let mut sink = Vec::with_capacity(bound);
    let mut decoder = FrameDecoder::new();
    let start = std::time::Instant::now();
    let mut total = 0usize;
    for _ in 0..iters {
        total += match mode {
            "stream" => {
                sink.clear();
                StreamingDecoder::new(std::hint::black_box(&frame[..]))
                    .read_to_end(&mut sink)
                    .expect("stream decode")
            }
            _ => decoder
                .decode_all(std::hint::black_box(&frame), &mut target)
                .expect("decode_all"),
        };
    }
    let elapsed = start.elapsed();
    eprintln!(
        "{} iters, {} bytes, {:.0} ns/iter ({mode})",
        iters,
        total,
        elapsed.as_nanos() as f64 / f64::from(iters)
    );
}
