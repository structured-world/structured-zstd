//! Standalone encode-loop binary for clean perf-record profiles of the
//! ENCODER hot path. Reads a raw corpus, then loops `compress_to_vec` at
//! the given level for N iters. No criterion, no FFI side — the perf
//! samples land purely in our encoder (the `compare_ffi` compress bench
//! runs the donor in the same process, so its flamegraph mixes
//! `ZSTD_*` donor symbols with ours; this binary does not).
//!
//! Build: cargo build --profile flamegraph -p structured-zstd \
//!          --example encode_loop_z000033 --features dict_builder
//! Run:   cargo flamegraph --example encode_loop_z000033 --features dict_builder \
//!          --profile flamegraph -- <level> <iters> <corpus_path>

use std::env;

use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(-1);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let corpus_path: Option<&str> = args.get(3).map(|s| s.as_str());

    let src: Vec<u8> = if let Some(path) = corpus_path {
        std::fs::read(path).expect("read corpus file")
    } else {
        // Deterministic 1 MiB LCG synthetic fallback.
        let n = 1_048_576usize;
        let mut src = Vec::with_capacity(n);
        let mut state: u64 = 0x517cc1b727220a95;
        while src.len() < n {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            src.push((state >> 56) as u8);
        }
        src
    };

    let mut sink: usize = 0;
    for _ in 0..iters {
        let out = compress_to_vec(src.as_slice(), CompressionLevel::Level(level));
        // Defeat dead-code elimination of the compress call.
        sink = sink.wrapping_add(out.len());
        core::hint::black_box(&out);
    }

    eprintln!(
        "encoded {} bytes × {} iters at level {}; last-out-sum={}",
        src.len(),
        iters,
        level,
        sink
    );
}
