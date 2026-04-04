//! Roundtrip integrity tests: compress → decompress → verify data unchanged.
//!
//! Tests run 1000 iterations with random data of varying sizes and patterns
//! to ensure no data corruption in the compress/decompress pipeline.

extern crate std;

#[allow(unused_imports)]
use alloc::vec;
use alloc::vec::Vec;

use crate::decoding::StreamingDecoder;
use crate::encoding::{CompressionLevel, FrameCompressor, compress_to_vec};
use crate::io::Read;

/// Generate deterministic pseudo-random data using a simple LCG.
fn generate_data(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut data = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push((state >> 33) as u8);
    }
    data
}

/// Generate highly compressible data (repeating patterns).
fn generate_compressible(seed: u64, len: usize) -> Vec<u8> {
    let pattern_len = ((seed % 16) + 1) as usize;
    let pattern = generate_data(seed, pattern_len);
    let mut data = Vec::with_capacity(len);
    for i in 0..len {
        data.push(pattern[i % pattern_len]);
    }
    data
}

/// Roundtrip using compress_to_vec at the given level.
fn roundtrip_at_level(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    let compressed = compress_to_vec(data, level);
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
}

fn roundtrip_simple(data: &[u8]) -> Vec<u8> {
    roundtrip_at_level(data, CompressionLevel::Fastest)
}

fn compress_streaming(data: &[u8]) -> Vec<u8> {
    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor.set_source(data);
    compressor.set_drain(&mut compressed);
    compressor.compress();
    compressed
}

/// Roundtrip using FrameCompressor at the given level.
fn roundtrip_streaming_at_level(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(level);
    compressor.set_source(data);
    compressor.set_drain(&mut compressed);
    compressor.compress();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
}

fn roundtrip_streaming(data: &[u8]) -> Vec<u8> {
    roundtrip_streaming_at_level(data, CompressionLevel::Fastest)
}

fn roundtrip_default(data: &[u8]) -> Vec<u8> {
    roundtrip_at_level(data, CompressionLevel::Default)
}

fn roundtrip_better(data: &[u8]) -> Vec<u8> {
    roundtrip_at_level(data, CompressionLevel::Better)
}

fn roundtrip_better_streaming(data: &[u8]) -> Vec<u8> {
    roundtrip_streaming_at_level(data, CompressionLevel::Better)
}

fn roundtrip_best(data: &[u8]) -> Vec<u8> {
    roundtrip_at_level(data, CompressionLevel::Best)
}

fn roundtrip_best_streaming(data: &[u8]) -> Vec<u8> {
    roundtrip_streaming_at_level(data, CompressionLevel::Best)
}

/// Generate data with limited alphabet for better Huffman compressibility
/// but enough variety to avoid RLE path.
fn generate_huffman_friendly(seed: u64, len: usize, alphabet_size: u8) -> Vec<u8> {
    assert!(alphabet_size > 0, "alphabet_size must be non-zero");
    let mut state = seed;
    let mut data = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push(((state >> 33) as u8) % alphabet_size);
    }
    data
}

fn repeat_offset_fixture(pattern: &[u8], chunks: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(chunks * (pattern.len() + 2));
    for i in 0..chunks {
        data.extend_from_slice(pattern);
        data.extend_from_slice(&(i as u16).to_le_bytes());
    }
    data
}

// Cross-validation tests (pure Rust ↔ C FFI) are in tests/cross_validation.rs
// because dev-dependencies (zstd) aren't available in library test modules.

#[test]
fn roundtrip_random_data_1000_iterations() {
    for i in 0..1000u64 {
        // Vary data sizes: 0 to ~64KB
        let len = (i * 67 % 65536) as usize;
        let data = generate_data(i, len);

        let result = roundtrip_simple(&data);
        assert_eq!(
            data, result,
            "Simple API roundtrip failed at iteration {i}, len={len}"
        );
    }
}

#[test]
fn roundtrip_compressible_data_1000_iterations() {
    for i in 0..1000u64 {
        let len = (i * 131 % 65536) as usize;
        let data = generate_compressible(i, len);

        let result = roundtrip_simple(&data);
        assert_eq!(
            data, result,
            "Compressible roundtrip failed at iteration {i}, len={len}"
        );
    }
}

#[test]
fn roundtrip_streaming_api_1000_iterations() {
    for i in 0..1000u64 {
        let len = (i * 97 % 32768) as usize;
        let data = generate_data(i.wrapping_add(0xDEAD), len);

        let result = roundtrip_streaming(&data);
        assert_eq!(
            data, result,
            "Streaming API roundtrip failed at iteration {i}, len={len}"
        );
    }
}

#[test]
fn roundtrip_edge_cases() {
    // Empty data
    assert_eq!(roundtrip_simple(&[]), Vec::<u8>::new());

    // Single byte
    assert_eq!(roundtrip_simple(&[0x42]), vec![0x42]);

    // All zeros (maximally compressible)
    let zeros = vec![0u8; 100_000];
    assert_eq!(roundtrip_simple(&zeros), zeros);

    // All 0xFF
    let ones = vec![0xFFu8; 100_000];
    assert_eq!(roundtrip_simple(&ones), ones);

    // Ascending bytes (moderately compressible)
    let ascending: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    assert_eq!(roundtrip_simple(&ascending), ascending);

    // 1 byte repeated (RLE-like)
    let rle = vec![0xABu8; 1_000_000];
    assert_eq!(roundtrip_simple(&rle), rle);
}

/// Roundtrip tests with large inputs that produce large literal sections.
///
/// The encoder uses `compress_literals` (Huffman) for literals > 1024 bytes,
/// so these inputs exercise the 14-bit (0b10) and 18-bit (0b11) size formats.
/// The exact literals size depends on how many matches the encoder finds,
/// so we verify roundtrip correctness rather than specific format selection.
#[test]
fn roundtrip_large_literals() {
    // ~1KB input — just above the raw→Huffman threshold.
    let data_1025 = generate_huffman_friendly(42, 1025, 16);
    assert_eq!(roundtrip_simple(&data_1025), data_1025);
    assert_eq!(roundtrip_streaming(&data_1025), data_1025);

    // ~16KB input — near the 14-bit/18-bit boundary.
    let data_16383 = generate_huffman_friendly(43, 16383, 32);
    assert_eq!(roundtrip_simple(&data_16383), data_16383);

    let data_16384 = generate_huffman_friendly(44, 16384, 32);
    assert_eq!(roundtrip_simple(&data_16384), data_16384);
    assert_eq!(roundtrip_streaming(&data_16384), data_16384);

    // 64KB input — well within the 18-bit range.
    let data_64k = generate_huffman_friendly(45, 65536, 64);
    assert_eq!(roundtrip_simple(&data_64k), data_64k);

    // 128KB input — MAX_BLOCK_SIZE, the largest single block.
    let data_128k = generate_huffman_friendly(46, 128 * 1024, 64);
    assert_eq!(roundtrip_simple(&data_128k), data_128k);
    assert_eq!(roundtrip_streaming(&data_128k), data_128k);
}

/// Multi-block data larger than MAX_BLOCK_SIZE that exercises the 4-stream
/// Huffman encoding across multiple blocks, each with large literal sections.
#[test]
fn roundtrip_multi_block_large_literals() {
    // 512KB of Huffman-friendly data — will be split into multiple 128KB blocks,
    // each exercising the 18-bit (0b11) size format with 4-stream encoding.
    let data = generate_huffman_friendly(100, 512 * 1024, 48);
    assert_eq!(roundtrip_simple(&data), data);
    assert_eq!(roundtrip_streaming(&data), data);
}

/// Repeat offset encoding: data with many repeated match offsets should compress
/// better than data where every offset is unique, and must roundtrip correctly.
#[test]
fn roundtrip_repeat_offsets() {
    // Break each repeated chunk with a changing 2-byte sentinel so the matcher
    // has to re-emit the same offset instead of collapsing everything into one
    // maximal match.
    let data = repeat_offset_fixture(b"ABCDE12345", 10_000);
    let result = roundtrip_simple(&data);
    assert_eq!(data, result, "Repeat offset roundtrip failed");

    // Also verify with streaming API
    let result = roundtrip_streaming(&data);
    assert_eq!(data, result, "Repeat offset streaming roundtrip failed");
}

/// Verify that highly repetitive data compresses significantly better than random data.
#[test]
fn repetitive_data_compresses_better_than_random() {
    // Repetitive data: fixed-offset matches separated by a changing sentinel.
    let repetitive = repeat_offset_fixture(b"ABCDE12345", 5_000);
    let compressed_repetitive = compress_to_vec(&repetitive[..], CompressionLevel::Fastest);

    // Random data of same size (incompressible)
    let random = generate_data(999, repetitive.len());
    let compressed_random = compress_to_vec(&random[..], CompressionLevel::Fastest);

    // Repetitive data should still beat random data, without pinning an exact
    // ratio that may drift as encoder heuristics evolve.
    assert!(
        compressed_repetitive.len() < compressed_random.len(),
        "Repetitive input should compress better than random input. \
         repetitive={} bytes, random={} bytes",
        compressed_repetitive.len(),
        compressed_random.len()
    );
}

/// Multi-block data exercises FSE table reuse across blocks and offset history
/// persistence across block boundaries.
#[test]
fn roundtrip_multi_block_repeat_offsets() {
    // 512KB of data with fixed-offset repeats broken by a changing sentinel —
    // spans multiple 128KB blocks, so offset history and FSE tables must
    // persist correctly across block boundaries.
    let mut data = repeat_offset_fixture(b"HelloWorld", (512 * 1024) / 12 + 1);
    data.truncate(512 * 1024);

    let result = roundtrip_simple(&data);
    assert_eq!(data, result, "Multi-block repeat offset roundtrip failed");

    let result = roundtrip_streaming(&data);
    assert_eq!(
        data, result,
        "Multi-block repeat offset streaming roundtrip failed"
    );

    let whole_frame = compress_streaming(&data);
    let frame_overhead = compress_to_vec(&[][..], CompressionLevel::Fastest).len();
    let independent_chunks: usize = data
        .chunks(128 * 1024)
        .map(|chunk| {
            compress_to_vec(chunk, CompressionLevel::Fastest)
                .len()
                .saturating_sub(frame_overhead)
        })
        .sum::<usize>()
        .saturating_add(frame_overhead);
    assert!(
        whole_frame.len() < independent_chunks,
        "Cross-block reuse should beat per-block resets. whole={} bytes, split={} bytes",
        whole_frame.len(),
        independent_chunks
    );
}

/// Zero literal length sequences (back-to-back matches with no literals between them)
/// exercise the shifted repeat-offset remap path instead of only generic new offsets.
#[test]
fn roundtrip_zero_literal_length_sequences() {
    // Alternate a base prefix with a one-byte-shifted version so the encoder
    // sees back-to-back zero-literal matches that must use a shifted repeat
    // remap path instead of only generic new offsets.
    let mut data = Vec::with_capacity(10_000);
    // Initial unique segment
    for i in 0..100u8 {
        data.push(i);
    }
    // Repeat the first 50 bytes, then alternate with a shifted 50-byte window.
    let prefix = data[..50].to_vec();
    let shifted_prefix = data[1..51].to_vec();
    data.extend_from_slice(&prefix);
    for _ in 0..100 {
        data.extend_from_slice(&shifted_prefix);
        data.extend_from_slice(&prefix);
    }

    let result = roundtrip_simple(&data);
    assert_eq!(data, result, "Zero ll sequence roundtrip failed");
}

/// Reusing the same `FrameCompressor` across frames must reset per-frame FSE repeat tables.
#[test]
fn roundtrip_reused_frame_compressor_across_frames() {
    let first = generate_huffman_friendly(700, 512 * 1024, 48);
    let second = generate_huffman_friendly(701, 512 * 1024, 48);

    let mut first_compressed = Vec::new();
    let mut second_compressed = Vec::new();
    {
        let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
        compressor.set_source(first.as_slice());
        compressor.set_drain(&mut first_compressed);
        compressor.compress();

        compressor.set_source(second.as_slice());
        compressor.set_drain(&mut second_compressed);
        compressor.compress();
    }

    let mut decoder = StreamingDecoder::new(first_compressed.as_slice()).unwrap();
    let mut first_roundtrip = Vec::new();
    decoder.read_to_end(&mut first_roundtrip).unwrap();
    assert_eq!(
        first, first_roundtrip,
        "First reused-frame roundtrip failed"
    );

    let mut decoder = StreamingDecoder::new(second_compressed.as_slice()).unwrap();
    let mut second_roundtrip = Vec::new();
    decoder.read_to_end(&mut second_roundtrip).unwrap();
    assert_eq!(
        second, second_roundtrip,
        "Second reused-frame roundtrip failed"
    );
}

#[test]
fn roundtrip_default_level_regression() {
    let data = generate_compressible(777, 64 * 1024);
    assert_eq!(roundtrip_default(&data), data);
}

#[test]
fn roundtrip_default_level_multi_block_regression() {
    let data = generate_compressible(1337, 512 * 1024);
    assert_eq!(roundtrip_default(&data), data);
}

/// Standard roundtrip test suite for a compression level. Generates 7 tests
/// covering compressible, random, multi-block, streaming, edge-case,
/// repeat-offset, and large-literal inputs inside a named module.
macro_rules! level_roundtrip_suite {
    (mod $mod_name:ident, $level:expr, $seed_base:expr) => {
        mod $mod_name {
            use super::*;

            fn rt(data: &[u8]) -> Vec<u8> {
                roundtrip_at_level(data, $level)
            }
            fn rt_stream(data: &[u8]) -> Vec<u8> {
                roundtrip_streaming_at_level(data, $level)
            }

            #[test]
            fn compressible() {
                let data = generate_compressible($seed_base, 64 * 1024);
                assert_eq!(rt(&data), data);
            }
            #[test]
            fn random() {
                let data = generate_data($seed_base + 111, 64 * 1024);
                assert_eq!(rt(&data), data);
            }
            #[test]
            fn multi_block() {
                let data = generate_compressible($seed_base + 222, 512 * 1024);
                assert_eq!(rt(&data), data);
            }
            #[test]
            fn streaming() {
                let data = generate_compressible($seed_base + 333, 64 * 1024);
                assert_eq!(rt_stream(&data), data);
            }
            #[test]
            fn edge_cases() {
                assert_eq!(rt(&[]), Vec::<u8>::new());
                assert_eq!(rt(&[0x42]), vec![0x42]);
                let zeros = vec![0u8; 100_000];
                assert_eq!(rt(&zeros), zeros);
                let ascending: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
                assert_eq!(rt(&ascending), ascending);
            }
            #[test]
            fn repeat_offsets() {
                let data = repeat_offset_fixture(b"ABCDE12345", 10_000);
                assert_eq!(rt(&data), data);
            }
            #[test]
            fn large_literals() {
                let data = generate_huffman_friendly($seed_base + 444, 128 * 1024, 64);
                assert_eq!(rt(&data), data);
            }
        }
    };
}

level_roundtrip_suite!(mod better_level, CompressionLevel::Better, 888);
level_roundtrip_suite!(mod best_level, CompressionLevel::Best, 1111);

/// Better (lazy2) should compress close to or better than Default (lazy) on
/// structured, compressible data. Lazy2 may be marginally worse on some inputs
/// due to skipping otherwise-adequate matches while looking further ahead.
#[test]
fn better_level_compresses_close_to_default() {
    let data = repeat_offset_fixture(b"HelloWorld", (256 * 1024) / 12 + 1);
    let compressed_default = compress_to_vec(&data[..], CompressionLevel::Default);
    let compressed_better = compress_to_vec(&data[..], CompressionLevel::Better);
    // Allow up to 1% regression; lazy2 optimizes for broader data patterns.
    assert!(
        (compressed_better.len() as u64) * 100 <= (compressed_default.len() as u64) * 101,
        "Better level should stay within 1% of Default. \
         better={} bytes, default={} bytes",
        compressed_better.len(),
        compressed_default.len(),
    );
}

/// Exercise the 8 MiB window: place a repeated pattern beyond Default's
/// 4 MiB window so only Better (8 MiB) can match it.
#[test]
fn roundtrip_better_level_large_window() {
    // Two identical 256 KiB regions separated by a 4.5 MiB compressible gap.
    // The gap uses a different seed so it doesn't share patterns with the
    // regions, but being compressible means hash chains aren't fully
    // destroyed by random noise. Better's 8 MiB window can still reach the
    // first region; Default's 4 MiB window cannot.
    let region = generate_compressible(42, 256 * 1024);
    let gap = generate_compressible(9999, 4 * 1024 * 1024 + 512 * 1024);
    let mut data = Vec::with_capacity(region.len() + gap.len() + region.len());
    data.extend_from_slice(&region);
    data.extend_from_slice(&gap);
    data.extend_from_slice(&region);

    assert_eq!(roundtrip_better(&data), data);

    // Better should compress the duplicated region; Default cannot reach it.
    let compressed_better = compress_to_vec(&data[..], CompressionLevel::Better);
    let compressed_default = compress_to_vec(&data[..], CompressionLevel::Default);
    assert!(
        compressed_better.len() < compressed_default.len(),
        "Better (8 MiB window) should beat Default (4 MiB) across 4.5 MiB gap. \
         better={} default={}",
        compressed_better.len(),
        compressed_default.len(),
    );
}

/// Best must not regress vs Better on this repetitive fixture. Equal
/// output is expected here (HC finds identical matches at any depth);
/// the strict Best < Better check lives in cross_validation.rs on the
/// more diverse decodecorpus sample.
#[test]
fn best_level_does_not_regress_vs_better() {
    let data = repeat_offset_fixture(b"HelloWorld", (256 * 1024) / 12 + 1);
    let compressed_better = compress_to_vec(&data[..], CompressionLevel::Better);
    let compressed_best = compress_to_vec(&data[..], CompressionLevel::Best);
    assert!(
        compressed_best.len() <= compressed_better.len(),
        "Best must not regress vs Better. best={} bytes, better={} bytes",
        compressed_best.len(),
        compressed_better.len(),
    );
}

/// Exercise the 16 MiB window: place a repeated pattern beyond Better's
/// 8 MiB window so only Best (16 MiB) can match it.
#[test]
fn roundtrip_best_level_large_window() {
    // Two identical 256 KiB high-entropy regions separated by a 9 MiB
    // compressible gap. The region is random so the only way to compress
    // the second copy is via long-distance matching (window reach).
    // Best's 16 MiB window can still reach the first region;
    // Better's 8 MiB window cannot.
    let region = generate_data(42, 256 * 1024);
    let gap = generate_compressible(7777, 9 * 1024 * 1024);
    let mut data = Vec::with_capacity(region.len() + gap.len() + region.len());
    data.extend_from_slice(&region);
    data.extend_from_slice(&gap);
    data.extend_from_slice(&region);

    assert_eq!(roundtrip_best(&data), data);

    // Best should compress the duplicated region; Better cannot reach it.
    let compressed_best = compress_to_vec(&data[..], CompressionLevel::Best);
    let compressed_better = compress_to_vec(&data[..], CompressionLevel::Better);
    assert!(
        compressed_best.len() < compressed_better.len(),
        "Best (16 MiB window) should beat Better (8 MiB) across 9 MiB gap. \
         best={} better={}",
        compressed_best.len(),
        compressed_better.len(),
    );
}

/// Best level streaming should produce identical decompressed output.
#[test]
fn roundtrip_best_level_streaming_multi_block() {
    let data = generate_compressible(5555, 512 * 1024);
    assert_eq!(roundtrip_best_streaming(&data), data);
}

// ─── Numeric compression levels (CompressionLevel::Level) ─────────

/// `from_level(3)` must be equivalent to `Default` — same compressed output.
#[test]
fn numeric_level_3_matches_default() {
    let data = generate_compressible(9000, 64 * 1024);
    let default = compress_to_vec(&data[..], CompressionLevel::Default);
    let level_3 = compress_to_vec(&data[..], CompressionLevel::from_level(3));
    assert_eq!(
        default, level_3,
        "Level(3) output must be identical to Default"
    );
}

/// `from_level(1)` must be equivalent to `Fastest`.
#[test]
fn numeric_level_1_matches_fastest() {
    let data = generate_compressible(9001, 64 * 1024);
    let fastest = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let level_1 = compress_to_vec(&data[..], CompressionLevel::from_level(1));
    assert_eq!(
        fastest, level_1,
        "Level(1) output must be identical to Fastest"
    );
}

/// `from_level(7)` must be equivalent to `Better`.
#[test]
fn numeric_level_7_matches_better() {
    let data = generate_compressible(9002, 64 * 1024);
    let better = compress_to_vec(&data[..], CompressionLevel::Better);
    let level_7 = compress_to_vec(&data[..], CompressionLevel::from_level(7));
    assert_eq!(
        better, level_7,
        "Level(7) output must be identical to Better"
    );
}

/// `from_level(11)` must be equivalent to `Best`.
#[test]
fn numeric_level_11_matches_best() {
    let data = generate_compressible(9003, 64 * 1024);
    let best = compress_to_vec(&data[..], CompressionLevel::Best);
    let level_11 = compress_to_vec(&data[..], CompressionLevel::from_level(11));
    assert_eq!(best, level_11, "Level(11) output must be identical to Best");
}

/// `from_level(0)` maps to default compression (level 3), matching C zstd.
#[test]
fn numeric_level_0_is_default_compression() {
    let data = generate_compressible(9004, 64 * 1024);
    let level_0 = compress_to_vec(&data[..], CompressionLevel::from_level(0));
    let level_3 = compress_to_vec(&data[..], CompressionLevel::from_level(3));
    assert_eq!(level_0, level_3, "Level(0) should map to default (level 3)");
}

/// All 22 positive levels produce valid output that round-trips correctly.
#[test]
fn all_22_levels_roundtrip() {
    let data = generate_compressible(9100, 32 * 1024);
    for level in 1..=22 {
        let result = roundtrip_at_level(&data, CompressionLevel::from_level(level));
        assert_eq!(data, result, "Roundtrip failed for Level({level})");
    }
}

/// Negative levels produce valid compressed output (ultra-fast mode).
#[test]
fn negative_levels_roundtrip() {
    let data = generate_compressible(9200, 32 * 1024);
    for level in [-1, -2, -3, -5] {
        let result = roundtrip_at_level(&data, CompressionLevel::from_level(level));
        assert_eq!(data, result, "Roundtrip failed for Level({level})");
    }
}

/// For this reasonably compressible fixture, the sampled higher levels are
/// expected not to produce larger output than the lower sampled levels.
#[test]
fn levels_monotonic_compression_ratio() {
    let data = generate_compressible(9300, 64 * 1024);
    let mut prev_size = usize::MAX;
    for level in [1, 3, 7, 11] {
        let compressed = compress_to_vec(&data[..], CompressionLevel::from_level(level));
        assert!(
            compressed.len() <= prev_size,
            "Level {level} produced larger output ({}) than a lower level ({prev_size})",
            compressed.len(),
        );
        prev_size = compressed.len();
    }
}

/// Numeric levels work with the streaming encoder.
#[test]
fn numeric_level_streaming_roundtrip() {
    use crate::encoding::StreamingEncoder;
    use crate::io::Write;

    let data = generate_compressible(9400, 200 * 1024);
    for level in [1, 3, 5, 7, 9, 11, -1] {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(level));
        for chunk in data.chunks(4096) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            data, result,
            "Streaming roundtrip failed for Level({level})"
        );
    }
}

/// Values beyond MAX_LEVEL are clamped — they must still produce valid output.
#[test]
fn out_of_range_level_clamped() {
    let data = generate_compressible(9500, 16 * 1024);
    let result = roundtrip_at_level(&data, CompressionLevel::from_level(100));
    assert_eq!(data, result, "Clamped Level(100) must still roundtrip");
    let result = roundtrip_at_level(&data, CompressionLevel::from_level(-200000));
    assert_eq!(data, result, "Clamped Level(-200000) must still roundtrip");
    let result = roundtrip_at_level(&data, CompressionLevel::from_level(i32::MIN));
    assert_eq!(data, result, "Clamped Level(i32::MIN) must still roundtrip");
}

// ─── Source-size-aware selection ───────────────────────────────────

/// Small input with source size hint should produce valid output.
#[test]
fn source_size_hint_small_input_roundtrip() {
    let data = generate_compressible(9600, 4 * 1024); // 4 KiB
    let compressed = {
        let mut compressor = FrameCompressor::new(CompressionLevel::from_level(7));
        compressor.set_source_size_hint(data.len() as u64);
        compressor.set_source(data.as_slice());
        let mut out = Vec::new();
        compressor.set_drain(&mut out);
        compressor.compress();
        out
    };
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "Small input with size hint must roundtrip");
}

/// Source size hint should reduce compressed output overhead for small inputs
/// by avoiding oversized windows/tables.
#[test]
fn source_size_hint_reduces_window_for_small_input() {
    let data = generate_compressible(9601, 1024); // 1 KiB
    // Without hint: uses full level-11 window (16 MiB)
    let no_hint = compress_to_vec(&data[..], CompressionLevel::from_level(11));
    let no_hint_header = crate::decoding::frame::read_frame_header(no_hint.as_slice())
        .unwrap()
        .0
        .window_size()
        .unwrap();
    // With hint: should use smaller window
    let with_hint = {
        let mut compressor = FrameCompressor::new(CompressionLevel::from_level(11));
        compressor.set_source_size_hint(data.len() as u64);
        compressor.set_source(data.as_slice());
        let mut out = Vec::new();
        compressor.set_drain(&mut out);
        compressor.compress();
        out
    };
    let with_hint_header = crate::decoding::frame::read_frame_header(with_hint.as_slice())
        .unwrap()
        .0
        .window_size()
        .unwrap();
    // Both must decompress correctly
    let mut decoder = StreamingDecoder::new(no_hint.as_slice()).unwrap();
    let mut r = Vec::new();
    decoder.read_to_end(&mut r).unwrap();
    assert_eq!(data, r);

    let mut decoder = StreamingDecoder::new(with_hint.as_slice()).unwrap();
    let mut r = Vec::new();
    decoder.read_to_end(&mut r).unwrap();
    assert_eq!(data, r);

    assert!(
        with_hint_header <= no_hint_header,
        "size hint should not increase frame window size: hint={} no_hint={}",
        with_hint_header,
        no_hint_header
    );
    assert!(
        with_hint_header < (16 * 1024 * 1024),
        "hinted level-11 frame should advertise smaller-than-default window, got {}",
        with_hint_header
    );
}

/// Streaming encoder with pledged content size automatically uses source size hint.
#[test]
fn streaming_pledged_size_uses_source_hint() {
    use crate::encoding::StreamingEncoder;
    use crate::io::Write;

    let data = generate_compressible(9602, 2 * 1024); // 2 KiB
    let no_hint = compress_to_vec(&data[..], CompressionLevel::from_level(11));
    let no_hint_header = crate::decoding::frame::read_frame_header(no_hint.as_slice())
        .unwrap()
        .0
        .window_size()
        .unwrap();

    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(11));
    encoder.set_pledged_content_size(data.len() as u64).unwrap();
    encoder.write_all(&data).unwrap();
    let compressed = encoder.finish().unwrap();
    let hinted_header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0
        .window_size()
        .unwrap();

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "Pledged-size streaming must roundtrip");
    assert!(
        hinted_header <= no_hint_header,
        "pledged source hint should not increase window size: hinted={} no_hint={}",
        hinted_header,
        no_hint_header
    );
    assert!(
        hinted_header < (16 * 1024 * 1024),
        "pledged source hint should reduce level-11 advertised window, got {}",
        hinted_header
    );
}

/// All 22 levels produce valid output for a tiny (256 byte) input with size hint.
#[test]
fn all_levels_tiny_input_with_hint() {
    let data = generate_compressible(9603, 256);
    for level in 1..=22 {
        let compressed = {
            let mut compressor = FrameCompressor::new(CompressionLevel::from_level(level));
            compressor.set_source_size_hint(data.len() as u64);
            compressor.set_source(data.as_slice());
            let mut out = Vec::new();
            compressor.set_drain(&mut out);
            compressor.compress();
            out
        };
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            data, result,
            "Tiny input with hint failed for Level({level})"
        );
    }
}
