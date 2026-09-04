//! One-shot encode loop: a fresh context per frame, the shape upstream's
//! `ZSTD_compress` has.
//!
//! The existing fresh-compressor loops drive the STREAMING entry point
//! (`set_source` / `compress`), which stages the input into the matcher's
//! history by design. Upstream's one-shot entry creates a context, compresses
//! straight from the caller's buffer and frees the context
//! (`zstd_compress.c:5497`), so comparing that against our streaming path
//! measures two different contracts. This drives
//! [`compress_slice_to_vec`], which is the same contract: fresh context,
//! input borrowed, output returned.
//!
//! Run: `cargo run --release -p ffi-bench --example oneshot_loop
//!        -- <level> <iters> <corpus path>`
//!
//! Compare against `ffi_encode_loop_z000033` with the same arguments.

use std::env;

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let path = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("zstd/decodecorpus_files/z000033");
    let src =
        std::fs::read(path).unwrap_or_else(|e| panic!("oneshot_loop: cannot read {path}: {e}"));

    let compressor_level = CompressionLevel::from_level(level);
    let mut sink: usize = 0;
    for _ in 0..iters {
        let out = compress_slice_to_vec(&src[..], compressor_level);
        sink = sink.wrapping_add(out.len());
        core::hint::black_box(&out);
    }
    eprintln!(
        "encoded {} bytes × {iters} iters at level {level} (one-shot); out-sum={sink}",
        src.len(),
    );
}
