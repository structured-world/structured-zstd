use rand::{RngExt, SeedableRng, rngs::SmallRng};
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

mod common;

#[test]
#[ignore = "manual perf probe — run with --ignored"]
fn high_entropy_level22_ratio_and_speed() {
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

    // Block-type breakdown — shared parser in `tests/common`.
    common::dump_block_breakdown("Rust", &compressed);
    common::dump_block_breakdown("C   ", &c_compressed);

    // Hypothesis: our raw-fast-path is gated on
    // `compression_level_allows_raw_fast_path(level, window_size)`
    // which for `Level(22)` requires `window_size <= 8 MiB`. On a
    // 1 MiB source-size hint, level 22 shrinks to window_log=20 ->
    // 1 MiB window ≤ 8 MiB, so the path is allowed and
    // `block_looks_incompressible(random)` short-circuits the matcher.
    // Without the hint, window stays at 128 MiB → fast path disabled
    // → full DP runs → expect ~C-zstd-equivalent timing.
    use structured_zstd::encoding::FrameCompressor;

    let t0 = std::time::Instant::now();
    let mut no_hint_compressed = Vec::new();
    for _ in 0..n_iter {
        no_hint_compressed.clear();
        let mut comp = FrameCompressor::new(CompressionLevel::Level(22));
        // intentionally NO set_source_size_hint
        comp.set_source(&data[..]);
        comp.set_drain(&mut no_hint_compressed);
        comp.compress();
    }
    let no_hint_avg = t0.elapsed() / n_iter;
    println!(
        "  Rust (no source-size hint, window=128 MiB → fast path disabled): {} bytes in {:?}",
        no_hint_compressed.len(),
        no_hint_avg,
    );
    println!(
        "    delta vs hinted path: {:.1}x slower",
        no_hint_avg.as_secs_f64() / our_avg.as_secs_f64()
    );
}
