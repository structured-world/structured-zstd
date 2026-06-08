//! Measure the one-shot encode content-checksum offload: hashing the input on
//! a scoped worker concurrent with compression vs hashing inline. Run on the
//! i9: `cargo run --release --example encode_offload`.

use std::hint::black_box;
use std::time::Instant;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

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

fn time(raw: &[u8], level: i32, offload: bool, iters: u32) -> f64 {
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
    enc.set_offload_checksum(offload);
    let mut out = Vec::new();
    // warm
    enc.compress_independent_frame_into(raw, &mut out);
    let start = Instant::now();
    for _ in 0..iters {
        enc.compress_independent_frame_into(black_box(raw), &mut out);
        black_box(&out[..]);
    }
    start.elapsed().as_secs_f64() * 1e3 / f64::from(iters)
}

fn run(name: &str, raw: &[u8], level: i32, iters: u32) {
    // Verify byte-identical output before timing.
    let mut a: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
    let inline_bytes = a.compress_independent_frame(raw);
    let mut b: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
    b.set_offload_checksum(true);
    let off_bytes = b.compress_independent_frame(raw);
    assert_eq!(
        inline_bytes, off_bytes,
        "offload output must be byte-identical"
    );

    let inline = time(raw, level, false, iters);
    let off = time(raw, level, true, iters);
    println!(
        "{name:>16} L{level:>2}  raw={:>9}  inline={inline:8.3}ms  offload={off:8.3}ms  offload/inline={:.3}x",
        raw.len(),
        off / inline,
    );
}

fn main() {
    let mb = 1024 * 1024;
    let data = repeated_log_lines(8 * mb);
    println!("== one-shot encode: checksum inline vs scoped-offload ==");
    for level in [3, 8, 12, 19] {
        let iters = if level >= 19 { 6 } else { 20 };
        run("large-log-8m", &data, level, iters);
    }
}
