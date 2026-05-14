use rand::{RngExt, SeedableRng, rngs::SmallRng};
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

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

    // Block-type breakdown — frame header at the start, then series of
    // 3-byte block headers each carrying (last_block, block_type, size).
    fn dump_block_types(label: &str, frame: &[u8]) {
        // Frame magic: 4 bytes 0x28 0xB5 0x2F 0xFD. Frame header
        // descriptor is the next byte; full parsing is non-trivial,
        // so just scan known offsets by reading the descriptor and
        // skipping accordingly.
        if frame.len() < 6 || &frame[..4] != b"\x28\xb5\x2f\xfd" {
            println!("  {label}: not a zstd frame");
            return;
        }
        let fhd = frame[4];
        let single_segment = (fhd & 0x20) != 0;
        let fcs_flag = fhd >> 6;
        let dict_id_flag = fhd & 0x03;
        let dict_id_len = match dict_id_flag {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let window_descriptor_len = if single_segment { 0 } else { 1 };
        let fcs_len = match fcs_flag {
            0 => {
                if single_segment {
                    1
                } else {
                    0
                }
            }
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let mut off = 5 + window_descriptor_len + dict_id_len + fcs_len;
        let mut raw = 0usize;
        let mut rle = 0usize;
        let mut compressed = 0usize;
        let mut total_size = 0usize;
        for _ in 0..64 {
            if off + 3 > frame.len() {
                break;
            }
            let h = u32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], 0]);
            let last = h & 1;
            let block_type = (h >> 1) & 3;
            let block_size = (h >> 3) as usize;
            match block_type {
                0 => raw += 1,
                1 => rle += 1,
                2 => compressed += 1,
                _ => {}
            }
            total_size += block_size;
            off += 3 + if block_type == 1 { 1 } else { block_size };
            if last == 1 {
                break;
            }
        }
        println!(
            "  {label}: raw={raw}, rle={rle}, compressed={compressed}, total_block_bytes={total_size}"
        );
    }
    dump_block_types("Rust", &compressed);
    dump_block_types("C   ", &c_compressed);

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
