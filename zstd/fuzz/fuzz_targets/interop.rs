#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use std::io::Read;
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

fn decode_szstd(data: &mut dyn std::io::Read) -> Vec<u8> {
    let mut decoder = structured_zstd::decoding::StreamingDecoder::new(data).unwrap();
    let mut result: Vec<u8> = Vec::new();
    decoder.read_to_end(&mut result).expect("Decoding failed");
    result
}

fn decode_szstd_writer(mut data: impl Read) -> Vec<u8> {
    let mut decoder = structured_zstd::decoding::FrameDecoder::new();
    decoder.reset(&mut data).unwrap();
    let mut result = vec![];
    while !decoder.is_finished() || decoder.can_collect() > 0 {
        decoder
            .decode_blocks(
                &mut data,
                structured_zstd::decoding::BlockDecodingStrategy::UptoBytes(1024 * 1024),
            )
            .unwrap();
        decoder.collect_to_writer(&mut result).unwrap();
    }
    result
}

fn encode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    zstd::stream::encode_all(std::io::Cursor::new(data), 3)
}

fn encode_szstd_uncompressed(data: &mut dyn std::io::Read) -> Vec<u8> {
    let mut input = Vec::new();
    data.read_to_end(&mut input).unwrap();
    compress_to_vec(data, CompressionLevel::Uncompressed)
}

fn encode_szstd_compressed(data: &mut dyn std::io::Read) -> Vec<u8> {
    let mut input = Vec::new();
    data.read_to_end(&mut input).unwrap();
    compress_to_vec(data, CompressionLevel::Uncompressed)
}

fn decode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut output = Vec::new();
    zstd::stream::copy_decode(data, &mut output)?;
    Ok(output)
}

fuzz_target!(|data: &[u8]| {
    // Decoding
    let compressed = encode_zstd(data).unwrap();
    let decoded = decode_szstd(&mut compressed.as_slice());
    let decoded2 = decode_szstd_writer(&mut compressed.as_slice());
    assert!(
        decoded == data,
        "Decoded data did not match the original input during decompression"
    );
    assert_eq!(
        decoded2, data,
        "Decoded data did not match the original input during decompression"
    );

    // Encoding
    // Uncompressed encoding
    let mut input = data;
    let compressed = encode_szstd_uncompressed(&mut input);
    let decoded = decode_zstd(&compressed).unwrap();
    assert_eq!(
        decoded, data,
        "Decoded data did not match the original input during compression"
    );
    // Compressed encoding
    let mut input = data;
    let compressed = encode_szstd_compressed(&mut input);
    let decoded = decode_zstd(&compressed).unwrap();
    assert_eq!(
        decoded, data,
        "Decoded data did not match the original input during compression"
    );
});
