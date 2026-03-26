//! Cross-validation: structured-zstd ↔ C FFI zstd roundtrip integrity.
//!
//! Tests 1000 iterations in both directions:
//! - Pure Rust compress → C FFI decompress
//! - C FFI compress → Pure Rust decompress

use structured_zstd::decoding::StreamingDecoder;
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};
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

/// Cross-validate large inputs (1KB–512KB) that produce large literal sections,
/// verifying C zstd can decompress what our encoder produces.
#[test]
fn cross_rust_compress_ffi_decompress_large_blocks() {
    let sizes = [1025, 16384, 65536, 128 * 1024];
    for (i, &size) in sizes.iter().enumerate() {
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

/// Cross-validate Rust compress (seed=100, 512KB) → C FFI decompress for the
/// same Huffman-heavy multi-block input used in roundtrip_multi_block_large_literals.
#[test]
fn cross_rust_compress_ffi_decompress_huffman_seed100() {
    let data = generate_huffman_friendly(100, 512 * 1024, 48);
    let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust→ffi seed=100 512KB roundtrip failed");
}

/// Cross-validate the same Huffman-heavy 512KB input in the opposite direction:
/// C FFI compress (seed=100) → Rust decompress.
#[test]
fn cross_ffi_compress_rust_decompress_huffman_seed100() {
    let data = generate_huffman_friendly(100, 512 * 1024, 48);
    let compressed = zstd::encode_all(&data[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "ffi→rust seed=100 512KB roundtrip failed");
}

/// Cross-validate repeat offset encoding: Rust compress → C FFI decompress.
/// Exercises repeat offset codes (1/2/3) and offset history across blocks.
#[test]
fn cross_rust_compress_ffi_decompress_repeat_offsets() {
    // Single-block: repeating pattern at fixed offset
    let pattern = b"ABCDE12345";
    let mut data = Vec::with_capacity(50_000);
    for _ in 0..5_000 {
        data.extend_from_slice(pattern);
    }
    let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust→ffi repeat offset roundtrip failed");

    // Multi-block: 512KB with repeating patterns spanning block boundaries
    let mut multi_block = Vec::with_capacity(512 * 1024);
    while multi_block.len() < 512 * 1024 {
        multi_block.extend_from_slice(pattern);
    }
    multi_block.truncate(512 * 1024);
    let compressed = compress_to_vec(&multi_block[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(
        multi_block, result,
        "rust→ffi multi-block repeat offset roundtrip failed"
    );
}

/// Cross-validate repeat-offset-friendly inputs in the opposite direction:
/// C FFI compress → Rust decompress.
#[test]
fn cross_ffi_compress_rust_decompress_repeat_offsets() {
    let pattern = b"ABCDE12345";

    let mut data = Vec::with_capacity(50_000);
    for _ in 0..5_000 {
        data.extend_from_slice(pattern);
    }
    let compressed = zstd::encode_all(&data[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "ffi→rust repeat offset roundtrip failed");

    let mut multi_block = Vec::with_capacity(512 * 1024);
    while multi_block.len() < 512 * 1024 {
        multi_block.extend_from_slice(pattern);
    }
    multi_block.truncate(512 * 1024);
    let compressed = zstd::encode_all(&multi_block[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(
        multi_block, result,
        "ffi→rust multi-block repeat offset roundtrip failed"
    );
}

#[test]
fn cross_rust_default_compress_ffi_decompress_regression() {
    let data = generate_huffman_friendly(900, 64 * 1024, 32);
    let compressed = compress_to_vec(&data[..], CompressionLevel::Default);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust default→ffi roundtrip failed");
}

#[test]
fn default_level_beats_fastest_on_corpus_proxy() {
    // Keep this strict: issue #5 requires Default to be a real step up from Fastest,
    // not just an alias that happens to roundtrip.
    let data = include_bytes!("../decodecorpus_files/z000033");
    let fastest = compress_to_vec(data.as_slice(), CompressionLevel::Fastest);
    let default = compress_to_vec(data.as_slice(), CompressionLevel::Default);

    assert!(
        default.len() < fastest.len(),
        "Default should compress better than Fastest on corpus proxy. default={} fastest={}",
        default.len(),
        fastest.len()
    );
}

#[test]
fn default_level_stays_within_ten_percent_of_ffi_level3_on_corpus_proxy() {
    // This corpus-proxy check is the in-repo acceptance test for the issue's
    // level-3 ratio target until the full Silesia corpus is vendored.
    let data = include_bytes!("../decodecorpus_files/z000033");
    let default = compress_to_vec(data.as_slice(), CompressionLevel::Default);
    let ffi_level3 = zstd::encode_all(data.as_slice(), 3).unwrap();

    assert!(
        (default.len() as u64) * 10 <= (ffi_level3.len() as u64) * 11,
        "Default should stay within 10% of zstd level 3 on corpus proxy. default={} ffi_l3={}",
        default.len(),
        ffi_level3.len()
    );
}
