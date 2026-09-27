//! Streaming encode loop: one reused `CompressionContext` writes the corpus as
//! a frame per iteration, in chunks of a fixed size, the way a caller feeding a
//! `Write` sink does. Prints a digest of the last frame so two builds can be
//! compared byte for byte.
//!
//! Run: `encode_loop_stream_z000033 <level> <iters> <corpus_path> [chunk_bytes]`

use std::env;

use structured_zstd::encoding::{CompressionContext, CompressionLevel};

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let path = args.get(3).expect("corpus path");
    let chunk: usize = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64 * 1024);
    assert!(chunk > 0, "chunk size must be positive");
    let src = std::fs::read(path).expect("read corpus file");

    let mut context = CompressionContext::new(CompressionLevel::from_level(level));
    let mut out: Vec<u8> = Vec::with_capacity(src.len() + (src.len() >> 3) + 4096);
    for _ in 0..iters {
        out.clear();
        context
            .set_pledged_content_size(src.len() as u64)
            .expect("pledge before the first write");
        for piece in src.chunks(chunk) {
            let mut piece = piece;
            while !piece.is_empty() {
                let taken = context.write(&mut out, piece).expect("write");
                piece = &piece[taken..];
            }
        }
        context.finish_frame(&mut out).expect("finish frame");
        core::hint::black_box(&out);
    }

    // FNV-1a over the last frame: a byte change anywhere moves it.
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in &out {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x1000_0000_01b3);
    }
    eprintln!(
        "level {level}: {} bytes -> {} bytes, fnv={digest:016x}",
        src.len(),
        out.len()
    );
}
