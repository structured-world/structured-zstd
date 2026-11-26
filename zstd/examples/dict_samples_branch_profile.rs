//! Branch outcome counts of the double-fast dictionary scan over the samples
//! FastCOVER training scores: the input cut as `train_loop_fastcover` cuts it,
//! every sample compressed `iters` times with the dictionary, then, for each
//! probed scan branch, how often it ran, was taken, and changed outcome.
//!
//! Run: `cargo run --release -p structured-zstd --features
//!        kernel-trace,dict-builder --example dict_samples_branch_profile --
//!        <level> <iters> <input> <dict>`

use structured_zstd::encoding::{CompressionLevel, FrameCompressor, branch_profile};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let src = std::fs::read(args.get(3).expect("input path")).expect("read input");
    let dict = std::fs::read(args.get(4).expect("dict path")).expect("read dict");
    let sample_size = src.len().div_ceil(16).clamp(256, 8192);
    let samples: Vec<&[u8]> = src
        .chunks(sample_size)
        .take(64)
        .filter(|chunk| chunk.len() >= 64)
        .collect();
    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(level));
    cctx.set_dictionary_from_bytes(&dict)
        .expect("dictionary should attach");
    let mut out = Vec::new();
    let mut total = 0usize;
    for _ in 0..iters {
        for sample in &samples {
            cctx.compress_independent_frame_into(sample, &mut out);
            total += out.len();
        }
    }
    print!("{}", branch_profile::report());
    eprintln!("{} samples x {iters}, {total} bytes", samples.len());
}
