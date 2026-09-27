//! Fresh-context one-shot encode loop: the shape `compare_ffi` times.
//!
//! Every frame builds a new `FrameCompressor` and compresses the slice with
//! `compress_independent_frame_into` into one reused output buffer, as the
//! C arm does with a fresh `ZSTD_CCtx` and a caller-owned `dst`
//! (`ffi_encode_loop_z000033`). The context's own construction and teardown
//! are part of what is measured; the output buffer is not.
//!
//! Run: `perf stat -e task-clock,page-faults ./encode_loop_fresh_z000033 <level> <iters> <corpus>`

use std::env;
use std::fs;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

fn main() {
    let level: i32 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: usize = env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let corpus_path = env::args()
        .nth(3)
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());

    let bytes = fs::read(&corpus_path).expect("read corpus");
    let mut out: Vec<u8> = Vec::new();
    let compression_level = CompressionLevel::from_level(level);

    let mut sum = 0usize;
    for _ in 0..iters {
        let mut enc: FrameCompressor = FrameCompressor::new(compression_level);
        enc.compress_independent_frame_into(&bytes[..], &mut out);
        sum += out.len();
    }

    println!(
        "encoded {} bytes x {} iters at level {}; last-out-sum={}",
        bytes.len(),
        iters,
        level,
        sum
    );
}
