//! Fresh-per-call allocation audit for the encoder, matching the
//! `compare_ffi` bench shape (a brand-new `FrameCompressor` per frame, the
//! way `ZSTD_createCCtx`-per-iter is measured on the C side). Run with
//! `--features dhat-heap` to record every per-call allocation site + count;
//! the totals divided by `iters` give the per-frame malloc shape that the
//! single-arena work targets.
//!
//! Build: `cargo build --profile flamegraph -p structured-zstd
//!          --example alloc_audit_fresh --features dhat-heap`
//! Run:   `cargo run --release -p structured-zstd
//!          --example alloc_audit_fresh --features dhat-heap -- <level> <iters> logs<N>`

use std::env;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

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

fn resolve_input(spec: &str) -> Vec<u8> {
    if let Some(rest) = spec.strip_prefix("logs") {
        let n: usize = rest.parse().expect("logs<N>: N must be a byte count");
        repeated_log_lines(n)
    } else {
        std::fs::read(spec).expect("read input file")
    }
}

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let input_spec: &str = args.get(3).map(|s| s.as_str()).unwrap_or("logs4096");

    let src = resolve_input(input_spec);

    let mut sink: usize = 0;
    for _ in 0..iters {
        // FRESH compressor every iteration = the compare_ffi bench shape.
        let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(level));
        let mut out: Vec<u8> = Vec::new();
        cctx.compress_independent_frame_into(&src, &mut out);
        sink = sink.wrapping_add(out.len());
        core::hint::black_box(&out);
    }

    eprintln!(
        "fresh-encoded {} bytes × {} iters at level {}; last-out-sum={}",
        src.len(),
        iters,
        level,
        sink
    );
}
