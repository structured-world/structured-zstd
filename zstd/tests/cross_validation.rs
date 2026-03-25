//! Cross-validation: structured-zstd ↔ C FFI zstd roundtrip integrity.
//!
//! Tests 1000 iterations in both directions:
//! - Pure Rust compress → C FFI decompress
//! - C FFI compress → Pure Rust decompress

use structured_zstd::decoding::StreamingDecoder;
use structured_zstd::encoding::{compress_to_vec, CompressionLevel};
use structured_zstd::io::Read;

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

/// Generate data with limited alphabet for Huffman-friendly compression.
fn generate_huffman_friendly(seed: u64, len: usize, alphabet_size: u8) -> Vec<u8> {
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

#[test]
fn cross_rust_compress_ffi_decompress_1000() {
    for i in 0..1000u64 {
        let len = (i * 89 % 16384) as usize;
        let data = generate_data(i, len);

        let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
        let result = zstd::decode_all(compressed.as_slice()).unwrap();
        assert_eq!(
            data, result,
            "rust→ffi roundtrip failed at iteration {i}, len={len}"
        );
    }
}

#[test]
fn cross_ffi_compress_rust_decompress_1000() {
    for i in 0..1000u64 {
        let len = (i * 89 % 16384) as usize;
        let data = generate_data(i.wrapping_add(0xBEEF), len);

        let compressed = zstd::encode_all(&data[..], 1).unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            data, result,
            "ffi→rust roundtrip failed at iteration {i}, len={len}"
        );
    }
}

/// Cross-validate large blocks (>16KB up to 128KB) that exercise the 18-bit
/// size format (0b11) with 4-stream Huffman encoding.
#[test]
fn cross_rust_compress_ffi_decompress_large_blocks() {
    // Sizes targeting each compressed literals size format boundary:
    // 1025 (14-bit), 16384 (18-bit), 65536 (18-bit), 128*1024 (18-bit max)
    let sizes = [1025, 16384, 65536, 128 * 1024];
    for (i, &size) in sizes.iter().enumerate() {
        // Huffman-friendly data ensures compressed path is taken (not raw fallback)
        let data = generate_huffman_friendly(i as u64 + 200, size, 48);

        let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
        let result = zstd::decode_all(compressed.as_slice()).unwrap();
        assert_eq!(
            data, result,
            "rust→ffi large block roundtrip failed at size={size}"
        );
    }

    // Multi-block: 512KB forces multiple blocks, each with large literals
    let data = generate_huffman_friendly(300, 512 * 1024, 48);
    let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust→ffi multi-block roundtrip failed");
}

/// Cross-validate C FFI compress → Rust decompress for large blocks.
#[test]
fn cross_ffi_compress_rust_decompress_large_blocks() {
    let sizes = [1025, 16384, 65536, 128 * 1024];
    for (i, &size) in sizes.iter().enumerate() {
        let data = generate_huffman_friendly(i as u64 + 400, size, 48);

        let compressed = zstd::encode_all(&data[..], 1).unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            data, result,
            "ffi→rust large block roundtrip failed at size={size}"
        );
    }

    // Multi-block: 512KB
    let data = generate_huffman_friendly(500, 512 * 1024, 48);
    let compressed = zstd::encode_all(&data[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "ffi→rust multi-block roundtrip failed");
}
