//! Level-22 probe on 1 MiB of random bytes: size and time against libzstd,
//! the block-type breakdown of each frame, and our time without a source-size
//! hint (the full window).
//!
//! Run: cargo run --release -p ffi-bench --example he_level22_ratio

use rand::{RngExt, SeedableRng, rngs::SmallRng};
use structured_zstd::encoding::{CompressionLevel, FrameCompressor, compress_to_vec};

#[path = "support/block_breakdown.rs"]
mod block_breakdown;

fn main() {
    let mut rng = SmallRng::seed_from_u64(0xC0FF_EE11);
    let mut data = vec![0u8; 1024 * 1024];
    rng.fill(&mut data[..]);

    // warm
    let _ = compress_to_vec(&data[..], CompressionLevel::Level(22));
    let _ = zstd::bulk::compress(&data[..], 22).unwrap();

    let n_iter = 3u32;

    let t0 = std::time::Instant::now();
    let mut compressed = Vec::new();
    for _ in 0..n_iter {
        compressed = compress_to_vec(&data[..], CompressionLevel::Level(22));
    }
    let our_avg = t0.elapsed() / n_iter;
    let our_size = compressed.len();

    let t0 = std::time::Instant::now();
    let mut c_compressed = Vec::new();
    for _ in 0..n_iter {
        c_compressed = zstd::bulk::compress(&data[..], 22).unwrap();
    }
    let c_avg = t0.elapsed() / n_iter;
    let c_size = c_compressed.len();

    println!("Input: {} bytes (random, seed 0xC0FFEE11)", data.len());
    println!("Level 22 compress (avg over {n_iter} iters):");
    println!("  Rust: {our_size} bytes in {our_avg:?}");
    println!("  C:    {c_size} bytes in {c_avg:?}");
    println!(
        "  Size ratio (ours/c): {:.4}",
        our_size as f64 / c_size as f64
    );
    println!(
        "  Speed ratio (ours/c): {:.2}x",
        our_avg.as_secs_f64() / c_avg.as_secs_f64()
    );

    block_breakdown::dump_block_breakdown("Rust", &compressed);
    block_breakdown::dump_block_breakdown("C   ", &c_compressed);

    // Without a source-size hint the window stays at its level default.
    let t0 = std::time::Instant::now();
    let mut no_hint_compressed = Vec::new();
    for _ in 0..n_iter {
        no_hint_compressed.clear();
        let mut comp = FrameCompressor::new(CompressionLevel::Level(22));
        comp.set_source(&data[..]);
        comp.set_drain(&mut no_hint_compressed);
        comp.compress();
    }
    let no_hint_avg = t0.elapsed() / n_iter;
    println!(
        "  Rust (no source-size hint): {} bytes in {:?}",
        no_hint_compressed.len(),
        no_hint_avg,
    );
    println!(
        "    against the hinted path: {:.1}x",
        no_hint_avg.as_secs_f64() / our_avg.as_secs_f64()
    );
}
