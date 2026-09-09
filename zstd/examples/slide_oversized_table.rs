//! Window slides with a hash table far larger than the window.
//!
//! `hashLog` is capped at `windowLog + 1` for a frame with no dictionary, so
//! the table is at most twice the window there. The cap is lifted when a
//! dictionary is attached, because the main and dictionary tables share one
//! `hashLog` — which leaves a configuration the advanced API can ask for and
//! the level presets never produce: a table of a million entries over a window
//! of a kilobyte, sliding once per kilobyte of input.
//!
//! That is the shape where sliding the table's indices and rebuilding it from
//! the retained bytes cost very different amounts, so it is the fixture for
//! deciding between them.
//!
//! Build: cargo build --profile bench -p structured-zstd
//!          --example slide_oversized_table --features hash,std,dict-builder
//! Run:   ./target/release/examples/slide_oversized_table
//!          <window_log> <hash_log> <frame_bytes> <iters> <dict_path>

use std::env;

use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, FrameCompressor, Strategy,
};

/// Compressible but not degenerate: repeated lines with a rotating field, so
/// the matcher finds real matches across the window without the whole frame
/// collapsing to one repeat.
fn body(len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    let mut counter = 0u32;
    while bytes.len() < len {
        let line = format!(
            "ts=2026-03-26T21:39:{:02}Z level=INFO msg=\"flush memtable\" seq={counter} \
             tenant=demo table=orders region=eu-west\n",
            counter % 60,
        );
        let remaining = len - bytes.len();
        bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
        counter += 1;
    }
    bytes
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let window_log: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let hash_log: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let frame_bytes: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 * 1024 * 1024);
    let iters: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4);
    let dict_path: Option<&str> = args.get(5).map(|s| s.as_str());

    let src = body(frame_bytes);

    let params = CompressionParameters::builder(CompressionLevel::Level(1))
        .strategy(Strategy::Fast)
        .window_log(window_log)
        .hash_log(hash_log)
        .build()
        .expect("parameters within bounds");

    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(1));
    cctx.set_parameters(&params);
    if let Some(path) = dict_path {
        let dict = std::fs::read(path).expect("read dict file");
        cctx.set_dictionary_from_bytes(&dict)
            .expect("dictionary should attach");
    }

    let mut out: Vec<u8> = Vec::new();
    let mut sink: usize = 0;
    for _ in 0..iters {
        cctx.compress_independent_frame_into(&src, &mut out);
        sink = sink.wrapping_add(out.len());
        core::hint::black_box(&out);
    }

    eprintln!(
        "windowLog={window_log} hashLog={hash_log} frame={frame_bytes} iters={iters} \
         dict={} out={} sum={sink} heap={}",
        dict_path.unwrap_or("none"),
        out.len(),
        cctx.heap_size(),
    );
}
