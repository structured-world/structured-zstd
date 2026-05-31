//! Standalone encode-loop binary for clean perf-record profiles of the
//! ENCODER hot path. Reads a raw corpus, then loops a `FrameCompressor`
//! over a contiguous `&[u8]` source at the given level for N iters. No
//! criterion, no FFI side — the perf samples land purely in our encoder
//! (the `compare_ffi` compress bench runs the donor in the same process,
//! so its flamegraph mixes `ZSTD_*` donor symbols with ours; this binary
//! does not).
//!
//! The output buffer is allocated ONCE and `clear()`-reused every
//! iteration, so steady-state iters do zero output-buffer allocation —
//! the flamegraph stays on the encoder hot path instead of per-iter
//! `Vec` growth + first-touch page faults. A fresh `FrameCompressor` per
//! iter mirrors a real per-frame encode: each frame in production is
//! compressed by its own `FrameCompressor`, and the matcher-table
//! allocation that `new()` performs is part of that real per-frame cost.
//! `FrameCompressor::compress()` does reset the matcher + offset history
//! per call, so a single instance could be reused; this binary keeps the
//! fresh-per-iter shape on purpose so the profile includes the matcher
//! allocation, unlike the pure-noise output realloc which we elide.
//!
//! Build: `cargo build --profile flamegraph -p structured-zstd
//!          --example encode_loop_z000033 --features dict_builder`
//! Run:   `cargo flamegraph --example encode_loop_z000033 --features dict_builder
//!          --profile flamegraph -- <level> <iters> <corpus_path>`

use std::env;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

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

    // Output buffer reused across iterations. Generous capacity
    // (src + 1/8 + 4 KiB) exceeds any frame's compressed size — even the
    // incompressible worst case (raw blocks + frame/block headers stays
    // well under src * 1.125) — so no iteration ever reallocates. We
    // can't call the crate-internal `compress_bound` from an example, so
    // this closed-form bound stands in for it.
    let cap = src.len() + (src.len() >> 3) + 4096;
    let mut out: Vec<u8> = Vec::with_capacity(cap);

    let mut sink: usize = 0;
    for _ in 0..iters {
        // Reuse the buffer: `clear()` resets len to 0 but keeps the
        // capacity, so the drain writes into already-faulted-in pages.
        // Drive the low-level `FrameCompressor` directly (the input is
        // already a contiguous `&[u8]`) instead of `compress_to_vec`,
        // which takes `impl Read` and re-buffers via `read_to_end` into
        // a fresh `Vec` every iteration.
        out.clear();
        let mut frame_enc = FrameCompressor::new(CompressionLevel::Level(level));
        frame_enc.set_source_size_hint(src.len() as u64);
        frame_enc.set_source(src.as_slice());
        frame_enc.set_drain(&mut out);
        frame_enc.compress();
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
