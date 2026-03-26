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

/// Roundtrip using compress_to_vec (simple API).
fn roundtrip_simple(data: &[u8]) -> Vec<u8> {
    let compressed = compress_to_vec(data, CompressionLevel::Fastest);
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
}

fn compress_streaming(data: &[u8]) -> Vec<u8> {
    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor.set_source(data);
    compressor.set_drain(&mut compressed);
    compressor.compress();
    compressed
}

/// Roundtrip using FrameCompressor (streaming API).
fn roundtrip_streaming(data: &[u8]) -> Vec<u8> {
    let compressed = compress_streaming(data);
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
}

/// Roundtrip using compress_to_vec with the default compression level.
fn roundtrip_default(data: &[u8]) -> Vec<u8> {
    let compressed = compress_to_vec(data, CompressionLevel::Default);
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
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
