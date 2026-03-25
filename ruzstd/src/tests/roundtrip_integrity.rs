//! Roundtrip integrity tests: compress → decompress → verify data unchanged.
//!
//! Tests run 1000 iterations with random data of varying sizes and patterns
//! to ensure no data corruption in the compress/decompress pipeline.

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use crate::decoding::StreamingDecoder;
use crate::encoding::{compress_to_vec, CompressionLevel, FrameCompressor};
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

/// Roundtrip using FrameCompressor (streaming API).
fn roundtrip_streaming(data: &[u8]) -> Vec<u8> {
    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor.set_source(data);
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    result
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
    assert_eq!(roundtrip_simple(&[]), vec![]);

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
