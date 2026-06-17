//! Fuzz-artifact interop replay: for every saved interop fuzz input, check
//! that C-compressed bytes decode through our decoder and our-compressed bytes
//! decode through the C decoder. Gated on the corpus directory existing (the
//! `fuzz/artifacts/interop` outputs are not checked in), so it is a no-op
//! unless a developer has run the interop fuzzer. Moved out of the library
//! crate so it never links the C bindings; the corpus path is resolved
//! relative to the `zstd` crate regardless of the test runner's cwd.

use std::io::Read;

use structured_zstd::decoding::{BlockDecodingStrategy, FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

fn decode_szstd(data: &mut dyn Read) -> Vec<u8> {
    let mut decoder = StreamingDecoder::new(data).unwrap();
    let mut result: Vec<u8> = Vec::new();
    decoder.read_to_end(&mut result).expect("Decoding failed");
    result
}

fn decode_szstd_writer(mut data: impl Read) -> Vec<u8> {
    let mut decoder = FrameDecoder::new();
    decoder.reset(&mut data).unwrap();
    let mut result = vec![];
    while !decoder.is_finished() || decoder.can_collect() > 0 {
        decoder
            .decode_blocks(&mut data, BlockDecodingStrategy::UptoBytes(1024 * 1024))
            .unwrap();
        decoder.collect_to_writer(&mut result).unwrap();
    }
    result
}

fn encode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    zstd::stream::encode_all(std::io::Cursor::new(data), 3)
}

fn encode_szstd_uncompressed(data: &mut dyn Read) -> Vec<u8> {
    let mut input = Vec::new();
    data.read_to_end(&mut input).unwrap();
    compress_to_vec(input.as_slice(), CompressionLevel::Uncompressed)
}

fn encode_szstd_compressed(data: &mut dyn Read) -> Vec<u8> {
    let mut input = Vec::new();
    data.read_to_end(&mut input).unwrap();
    compress_to_vec(input.as_slice(), CompressionLevel::Fastest)
}

fn decode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut output = Vec::new();
    zstd::stream::copy_decode(data, &mut output)?;
    Ok(output)
}

#[test]
fn fuzz_targets() {
    // The fuzz outputs live in the `zstd` crate; resolve relative to this
    // crate's manifest so the test finds them no matter the runner cwd.
    let corpus = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../zstd/fuzz/artifacts/interop"
    );
    if std::fs::exists(corpus).unwrap_or(false) {
        for file in std::fs::read_dir(corpus).unwrap() {
            if file.as_ref().unwrap().file_type().unwrap().is_file() {
                let data = std::fs::read(file.unwrap().path()).unwrap();
                let data = data.as_slice();
                // Decoding: C-compressed input must decode through our decoder.
                let compressed = encode_zstd(data).unwrap();
                let decoded = decode_szstd(&mut compressed.as_slice());
                let decoded2 = decode_szstd_writer(&mut compressed.as_slice());
                assert!(decoded == data, "decode mismatch (streaming)");
                assert_eq!(decoded2, data, "decode mismatch (writer)");

                // Encoding: our output must decode through the C decoder.
                let mut input = data;
                let compressed = encode_szstd_uncompressed(&mut input);
                let decoded = decode_zstd(&compressed).unwrap();
                assert_eq!(decoded, data, "C decode mismatch (uncompressed encode)");

                let mut input = data;
                let compressed = encode_szstd_compressed(&mut input);
                let decoded = decode_zstd(&compressed).unwrap();
                assert_eq!(decoded, data, "C decode mismatch (compressed encode)");
            }
        }
    }
}
