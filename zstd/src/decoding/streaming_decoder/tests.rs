use super::StreamingDecoder;
use crate::io::Read;

/// `Read::read` must not return `Err` after it has already written bytes
/// into the caller's buffer (the trait mandates that an error implies no
/// bytes were read). When a single `read` call both drains the final bytes
/// of a `Verify`-mode frame AND finishes it, a checksum mismatch must be
/// deferred: those bytes are delivered as `Ok(n)` and the error surfaces on
/// the next (zero-byte) call, where returning `Err` violates no contract.
#[cfg(feature = "hash")]
#[test]
fn read_delivering_bytes_defers_checksum_error_to_next_call() {
    use crate::decoding::ContentChecksum;
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use crate::io::ErrorKind;
    use alloc::vec;
    use alloc::vec::Vec;

    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    // Checksum is the subject under test; the encoder default is off
    // (upstream library parity).
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    // Corrupt the trailing 4-byte content checksum: the body still decodes
    // to the right bytes, but the stored digest no longer matches.
    let last = compressed.len() - 1;
    compressed[last] ^= 0xFF;

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    decoder
        .decoder
        .set_content_checksum(ContentChecksum::Verify);

    // A buffer large enough to drain the whole frame in one call: this call
    // finishes the frame AND writes every payload byte. The mismatch must
    // NOT abort it (that would drop the delivered bytes).
    let mut buf = vec![0u8; payload.len() + 4096];
    let n = decoder
        .read(&mut buf)
        .expect("a read that delivered bytes must not return the checksum Err");
    assert_eq!(n, payload.len());
    assert_eq!(&buf[..n], payload.as_slice());

    // The deferred mismatch surfaces on the terminating zero-byte read.
    let err = decoder
        .read(&mut buf)
        .expect_err("deferred checksum mismatch must surface on the terminating read");
    assert_eq!(err.kind(), ErrorKind::Other);
}

/// A fresh `read_to_end` must take the single-copy decode-in-place path
/// (FCS-declared frame decoded straight into the output `Vec`, no ring
/// drain) AND reproduce the payload byte-for-byte.
#[cfg(feature = "std")]
#[test]
fn read_to_end_decode_in_place_matches_and_takes_direct_path() {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec::Vec;

    let payload: Vec<u8> = (0..20_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut out = Vec::new();
    let n = decoder.read_to_end(&mut out).unwrap();
    assert_eq!(n, payload.len());
    assert_eq!(out, payload);
    // FrameCompressor declares FCS, so the fresh fast path used the direct
    // (decode-in-place) route, not the ring drain.
    assert_eq!(decoder.decoder.direct_frames(), 1);
}

/// `read_to_end` after a partial `read` must still produce the full
/// payload. The decoder is mid-frame, so the fast path is skipped and the
/// generic grow-and-drain fallback runs (no direct frame).
#[cfg(feature = "std")]
#[test]
fn read_to_end_after_partial_read_is_complete() {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec;
    use alloc::vec::Vec;

    let payload: Vec<u8> = (0..20_000u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut head = vec![0u8; 4096];
    let got = decoder.read(&mut head).unwrap();
    assert!(got > 0 && got <= head.len());

    let mut out = Vec::new();
    out.extend_from_slice(&head[..got]);
    decoder.read_to_end(&mut out).unwrap();
    assert_eq!(out, payload);
    // Mid-frame entry → fallback path, never the direct route.
    assert_eq!(decoder.decoder.direct_frames(), 0);
}

/// `read_to_end` reads the WHOLE source to EOF: a stream of concatenated
/// frames must decode every frame, not just the first. (The fast path
/// buffers the whole source, so dropping the trailing frame would lose
/// data.)
#[cfg(feature = "std")]
#[test]
fn read_to_end_decodes_all_concatenated_frames() {
    use crate::encoding::{CompressionLevel, compress_slice_to_vec};
    use alloc::vec::Vec;

    let a: Vec<u8> = (0..5000u32).map(|i| (i & 0xFF) as u8).collect();
    let b: Vec<u8> = (0..3000u32)
        .map(|i| ((i.wrapping_mul(7)) & 0xFF) as u8)
        .collect();
    let mut stream = compress_slice_to_vec(&a, CompressionLevel::Level(3));
    stream.extend_from_slice(&compress_slice_to_vec(&b, CompressionLevel::Level(3)));

    let mut decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(out, expected);
    // Both FCS-declared frames took the direct path.
    assert_eq!(decoder.decoder.direct_frames(), 2);
}

/// `read_to_end` after a partial `read` must STILL consume the source to
/// EOF across concatenated frames, not stop at the current frame's end. The
/// partial read forces the mid-frame fallback path; with two concatenated
/// frames the fallback must finish frame 1, then advance through frame 2.
#[cfg(feature = "std")]
#[test]
fn read_to_end_after_partial_read_decodes_all_concatenated_frames() {
    use crate::encoding::{CompressionLevel, compress_slice_to_vec};
    use alloc::vec;
    use alloc::vec::Vec;

    let a: Vec<u8> = (0..6000u32).map(|i| (i & 0xFF) as u8).collect();
    let b: Vec<u8> = (0..4000u32)
        .map(|i| ((i.wrapping_mul(11)) & 0xFF) as u8)
        .collect();
    let mut stream = compress_slice_to_vec(&a, CompressionLevel::Level(3));
    stream.extend_from_slice(&compress_slice_to_vec(&b, CompressionLevel::Level(3)));

    let mut decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    // Partial read of frame 1 → mid-frame, so read_to_end takes the fallback.
    let mut head = vec![0u8; 2048];
    let got = decoder.read(&mut head).unwrap();
    assert!(got > 0 && got <= head.len());

    let mut out = Vec::new();
    out.extend_from_slice(&head[..got]);
    decoder.read_to_end(&mut out).unwrap();

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(
        out, expected,
        "fallback path must decode frame 2 too, not stop at frame 1 EOF"
    );
}

/// `read_to_end` on a stream of concatenated DICTIONARY frames must decode
/// every frame WITH the dictionary the decoder was constructed with. The
/// fast-path concatenated loop re-initialises following frames, and a plain
/// re-init resolves dictionaries by frame id only — losing the forced
/// dictionary for frames that omit (or can't resolve) the id.
#[cfg(feature = "std")]
#[test]
fn read_to_end_concatenated_dict_frames_decode_with_dictionary() {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec::Vec;

    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let compress_with_dict = |payload: &[u8]| -> Vec<u8> {
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dict load");
        compressor.set_source(payload);
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();
        compressed
    };

    let a = b"first dictionary-compressed frame payload".to_vec();
    let b = b"second dictionary-compressed frame payload".to_vec();
    let mut stream = compress_with_dict(&a);
    stream.extend_from_slice(&compress_with_dict(&b));

    let mut decoder =
        StreamingDecoder::new_with_dictionary_bytes(stream.as_slice(), dict_raw).unwrap();
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .expect("both dict frames must decode with the forced dictionary");

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(out, expected);
}

/// A direct-path decode error must NOT leave non-decoded bytes in `output`.
/// The fast path resizes `output` to the declared content size before
/// decoding; if decode fails, the enlarged (zeroed) tail must be truncated
/// away so callers never observe bytes that were never decoded.
#[cfg(feature = "std")]
#[test]
fn read_to_end_truncates_output_on_direct_decode_error() {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec::Vec;

    let payload: Vec<u8> = (0..5000u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();
    // Truncate the block bytes (the FCS-bearing header at the front stays
    // intact) so the header parses but the direct-path block decode hits a
    // premature end → error after `output` was already resized.
    compressed.truncate(compressed.len() - 40);

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut out = b"SENTINEL".to_vec();
    let result = decoder.read_to_end(&mut out);
    assert!(result.is_err(), "truncated block must fail the decode");
    assert_eq!(
        out, b"SENTINEL",
        "failed direct decode must not append non-decoded bytes to output"
    );
}

/// The mid-frame fallback grows `output` by `MAX_BLOCK_SIZE` before each
/// `self.read`. When that read errors (truncated current frame), the grown
/// zero-filled tail must be truncated away before the error propagates, so
/// the caller never observes `MAX_BLOCK_SIZE` worth of bytes that were never
/// decoded.
#[cfg(feature = "std")]
#[test]
fn read_to_end_truncates_output_on_midframe_fallback_error() {
    use crate::encoding::{CompressionLevel, CompressionParameters, FrameCompressor};
    use alloc::vec;
    use alloc::vec::Vec;

    // Incompressible payload with a window (128 KiB) SMALLER than the input,
    // so the frame holds several blocks and bytes become collectable while
    // the frame is still mid-decode. Without a sub-input window the decoder
    // retains the whole input until the frame finishes, and a partial read
    // could only ever finish or error, never leave a truncated remainder for
    // the fallback to trip on.
    let payload: Vec<u8> = (0..320_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let params = CompressionParameters::builder(CompressionLevel::Default)
        .window_log(17)
        .build()
        .expect("window_log within bounds");
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_parameters(&params);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();
    // Truncate the tail so the final block decode fails partway through.
    compressed.truncate(compressed.len() - 40);

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    // A partial `read` first: leaves the decoder mid-frame so `read_to_end`
    // takes the grow-and-drain fallback (not the decode-in-place fast path).
    let mut head = vec![0u8; 4096];
    let got = decoder.read(&mut head).unwrap();
    assert!(got > 0);

    let mut out = Vec::new();
    out.extend_from_slice(&head[..got]);
    let result = decoder.read_to_end(&mut out);
    assert!(
        result.is_err(),
        "truncated current frame must fail the decode"
    );
    assert!(
        out.len() <= payload.len(),
        "failed fallback read must not leave a zero-filled tail (len {} > payload {})",
        out.len(),
        payload.len()
    );
    assert_eq!(
        out.as_slice(),
        &payload[..out.len()],
        "decoded prefix must match the payload, with no appended non-decoded bytes"
    );
}

/// An empty (`Frame_Content_Size = 0`) frame decodes to nothing through the
/// `read_to_end` fast path — the declared-size validation accepts the valid
/// case (produced == 0) instead of erroring.
#[cfg(feature = "std")]
#[test]
fn read_to_end_empty_frame_decodes_to_empty() {
    use crate::encoding::{CompressionLevel, compress_slice_to_vec};
    use alloc::vec::Vec;

    let compressed = compress_slice_to_vec(&[], CompressionLevel::Level(3));
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
}
