//! Cross-implementation parity for the streaming encoder: output produced
//! incrementally through `StreamingEncoder` must decode through the C `zstd`
//! bindings. Moved out of the library crate so it never links the C side;
//! header introspection goes through the `structured_zstd::testing` facade.
#![cfg(feature = "bench-internals")]

use std::io::Write;

use structured_zstd::encoding::{CompressionLevel, StreamingEncoder};
use structured_zstd::testing::frame_header_info;

fn c_decode(compressed: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    zstd::stream::copy_decode(compressed, &mut decoded).expect("C zstd must decode our output");
    decoded
}

#[test]
fn streaming_encoder_output_decompresses_with_c_zstd() {
    let payload = b"tenant=demo op=put key=streaming value=abcdef\n".repeat(4096);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    for chunk in payload.chunks(1024) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();
    assert_eq!(c_decode(&compressed), payload);
}

#[test]
fn pledged_content_size_c_zstd_compatible() {
    let payload = b"tenant=demo op=put key=streaming value=abcdef\n".repeat(4096);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder
        .set_pledged_content_size(payload.len() as u64)
        .unwrap();
    for chunk in payload.chunks(1024) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();

    // FCS should be written, and C zstd should decompress successfully.
    let (_, fcs, _) = frame_header_info(&compressed);
    assert_eq!(fcs, payload.len() as u64);
    assert_eq!(c_decode(&compressed), payload);
}
