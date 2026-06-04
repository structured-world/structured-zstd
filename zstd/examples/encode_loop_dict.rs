//! Standalone encode-loop binary for CLEAN perf-record profiles of the
//! ENCODER hot path on small inputs, with or without a dictionary.
//!
//! Why this exists: the `compare_ffi` dictionary bench cannot be flamegraphed
//! for the timed compress. Its per-scenario setup trains dictionaries
//! (XXH64-heavy) and runs a validation decode for EVERY scenario before the
//! filtered group, so a compress-filtered flamegraph is polluted with
//! `ZDICT`/decoder/`twox_hash` frames that never run in the timed loop. This
//! binary does only: parse the dictionary ONCE, then loop
//! [`FrameCompressor::compress_independent_frame_into`] over a contiguous
//! `&[u8]` at the given level. No criterion, no FFI, no training, no decode —
//! the samples land purely on our per-frame encode hot path.
//!
//! The compressor and the output buffer are both reused across iterations
//! (the reusable-compression-context shape a real per-block-frame consumer
//! uses): the matcher tables, scratch, and any primed dictionary are
//! allocated/parsed once, and the dictionary is re-primed per frame inside
//! `prepare_frame`. So the steady-state profile isolates the genuine
//! per-frame cost (dictionary prime + optimal parse + entropy), not the
//! one-time setup the fresh-per-iter bench shape folds in.
//!
//! Build: `cargo build --profile flamegraph -p structured-zstd
//!          --example encode_loop_dict --features dict_builder`
//! Run:   `cargo flamegraph --example encode_loop_dict --features dict_builder
//!          --profile flamegraph -- <level> <iters> <input> [dict_path]`
//!
//! `<input>` is either `logs<N>` (N bytes of the bench `repeated_log_lines`
//! fixture, e.g. `logs4096` == the `small-4k-log-lines` scenario byte for
//! byte) or a path to a raw file. `dict_path` is optional; when given the
//! dictionary is attached once before the loop.

use std::env;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

/// Byte-for-byte the bench `repeated_log_lines(len)` fixture so a `logs<N>`
/// input reproduces the `*-log-lines` dashboard scenarios exactly.
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
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(22);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let input_spec: &str = args.get(3).map(|s| s.as_str()).unwrap_or("logs4096");
    let dict_path: Option<&str> = args.get(4).map(|s| s.as_str());

    let src = resolve_input(input_spec);

    // One reused compressor: matcher tables + any dictionary parse happen
    // once here, mirroring a consumer that reuses a context across N
    // independent per-block frames.
    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(level));
    if let Some(path) = dict_path {
        let dict = std::fs::read(path).expect("read dict file");
        cctx.set_dictionary_from_bytes(&dict)
            .expect("dictionary should attach");
    }

    // Output buffer reused across iterations (allocated once, replaced in
    // place by `compress_independent_frame_into`), so steady-state iters do
    // zero output allocation.
    let mut out: Vec<u8> = Vec::new();
    let mut sink: usize = 0;
    for _ in 0..iters {
        cctx.compress_independent_frame_into(&src, &mut out);
        sink = sink.wrapping_add(out.len());
        core::hint::black_box(&out);
    }

    eprintln!(
        "encoded {} bytes × {} iters at level {} dict={}; last-out-sum={}",
        src.len(),
        iters,
        level,
        dict_path.unwrap_or("none"),
        sink
    );
}
