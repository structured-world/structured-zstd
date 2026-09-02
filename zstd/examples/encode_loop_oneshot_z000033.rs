//! One-shot encode loop over a slice, matching what `compare_ffi` times and
//! what `ffi_encode_loop_z000033` does on the C side.
//!
//! `encode_loop_z000033` drives `FrameCompressor::compress` from a `Read`
//! source, which takes the owned block loop and copies the input into the
//! matcher's history. The benchmark instead calls
//! `compress_independent_frame_into`, which can take the borrowed path and
//! scan the caller's slice in place. Profiling the wrong one attributes
//! copy cost that the measured path may not pay, so keep a binary for each.
//!
//! Output buffer allocated once and reused, as in the sibling binaries.
//!
//! Build: `cargo build --release -p ffi-bench --example encode_loop_oneshot_z000033 --features dict-builder`
//! Run:   `perf stat -e cycles,instructions ./encode_loop_oneshot_z000033 <level> <iters> <corpus>`

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
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(level));

    let mut sum = 0usize;
    for _ in 0..iters {
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
