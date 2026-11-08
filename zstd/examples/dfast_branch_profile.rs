//! Branch outcome counts of the double-fast scan for one cell: compress the
//! input `iters` times with a reused compressor, then print, for each
//! probed scan branch, how often it ran, was taken, and changed outcome.
//!
//! Run: `cargo run --release -p structured-zstd --features
//!        kernel-trace,dict-builder --example dfast_branch_profile --
//!        <level> <iters> <input> [dict]`

use structured_zstd::encoding::{CompressionLevel, FrameCompressor, branch_profile};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let src = std::fs::read(args.get(3).expect("input path")).expect("read input");
    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(level));
    if let Some(path) = args.get(4) {
        let dict = std::fs::read(path).expect("read dict");
        cctx.set_dictionary_from_bytes(&dict)
            .expect("dictionary should attach");
    }
    let mut out = Vec::new();
    for _ in 0..iters {
        cctx.compress_independent_frame_into(&src, &mut out);
    }
    print!("{}", branch_profile::report());
    eprintln!("frames {iters} last frame {} bytes", out.len());
}
