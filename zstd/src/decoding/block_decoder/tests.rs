//! Coverage for `decode_block_content_from_slice` error branches —
//! truncated / empty source on each block type, plus the
//! `DecoderState::Failed` / `ReadyToDecodeNextHeader` entry-state
//! guards. The happy path is exercised indirectly via the
//! roundtrip tests on `decode_all`; these tests pin the
//! fail-fast behaviour for malformed input.
use super::*;
use crate::blocks::block::{BlockHeader, BlockType};
use crate::decoding::ringbuffer::RingBuffer;
use crate::decoding::scratch::DecoderScratch;

fn header(block_type: BlockType, decompressed_size: u32, content_size: u32) -> BlockHeader {
    BlockHeader {
        last_block: true,
        block_type,
        decompressed_size,
        content_size,
    }
}

fn fresh_workspace() -> DecoderScratch<RingBuffer> {
    DecoderScratch::<RingBuffer>::new(1 << 20)
}

fn primed_decoder() -> BlockDecoder {
    let mut d = new();
    d.internal_state = DecoderState::ReadyToDecodeNextBody;
    d
}

#[test]
fn rejects_when_internal_state_expects_header() {
    // Default state is ReadyToDecodeNextHeader -> calling
    // decode_block_content_from_slice on a body must error,
    // not silently decode garbage.
    let mut d = new();
    let mut ws = fresh_workspace();
    let mut src: &[u8] = &[];
    let h = header(BlockType::RLE, 4, 1);
    let err = d
        .decode_block_content_from_slice(&h, &mut ws, None, &mut src)
        .expect_err("must err on body before header");
    assert!(matches!(
        err,
        DecodeBlockContentError::ExpectedHeaderOfPreviousBlock
    ));
}

#[test]
fn rejects_when_internal_state_failed() {
    let mut d = new();
    d.internal_state = DecoderState::Failed;
    let mut ws = fresh_workspace();
    let mut src: &[u8] = &[0x42];
    let h = header(BlockType::RLE, 4, 1);
    let err = d
        .decode_block_content_from_slice(&h, &mut ws, None, &mut src)
        .expect_err("must err on Failed state");
    assert!(matches!(err, DecodeBlockContentError::DecoderStateIsFailed));
}

#[test]
fn rle_empty_source_errors_not_panics() {
    // RLE block needs at least 1 fill byte in source. Empty
    // source must return ReadError, not panic on source[0].
    let mut d = primed_decoder();
    let mut ws = fresh_workspace();
    let mut src: &[u8] = &[];
    let h = header(BlockType::RLE, 4, 1);
    let err = d
        .decode_block_content_from_slice(&h, &mut ws, None, &mut src)
        .expect_err("must err on empty RLE source");
    match &err {
        DecodeBlockContentError::ReadError { step, source } => {
            assert_eq!(*step, BlockType::RLE);
            assert_eq!(
                source.kind(),
                crate::io::ErrorKind::UnexpectedEof,
                "slice-source truncation must report UnexpectedEof to match the streaming path's Read::read_exact behaviour"
            );
        }
        other => panic!("expected ReadError, got {other:?}"),
    }
}

#[test]
fn raw_truncated_source_errors_not_panics() {
    // Raw block header claims 10 decompressed bytes but only
    // 3 are available -> ReadError. The pre-split bounds check
    // catches this before split_at would panic.
    let mut d = primed_decoder();
    let mut ws = fresh_workspace();
    let mut src: &[u8] = &[1, 2, 3];
    let h = header(BlockType::Raw, 10, 10);
    let err = d
        .decode_block_content_from_slice(&h, &mut ws, None, &mut src)
        .expect_err("must err on truncated raw source");
    match &err {
        DecodeBlockContentError::ReadError { step, source } => {
            assert_eq!(*step, BlockType::Raw);
            assert_eq!(source.kind(), crate::io::ErrorKind::UnexpectedEof);
        }
        other => panic!("expected ReadError, got {other:?}"),
    }
}

#[test]
fn compressed_truncated_source_errors_not_panics() {
    // Compressed block header claims 100 compressed bytes but
    // only 8 are available -> ReadError. Pre-split bound check.
    let mut d = primed_decoder();
    let mut ws = fresh_workspace();
    let mut src: &[u8] = &[0u8; 8];
    let h = header(BlockType::Compressed, 0, 100);
    let err = d
        .decode_block_content_from_slice(&h, &mut ws, None, &mut src)
        .expect_err("must err on truncated compressed source");
    match &err {
        DecodeBlockContentError::ReadError { step, source } => {
            assert_eq!(*step, BlockType::Compressed);
            assert_eq!(source.kind(), crate::io::ErrorKind::UnexpectedEof);
        }
        other => panic!("expected ReadError, got {other:?}"),
    }
}

/// Exercise the BackendOverflow -> DecodeBlockContentError mapping
/// on the direct-decode path. Constructs a fixed-capacity
/// `UserSliceBackend` over a 4-byte slice and feeds it an RLE
/// block whose `decompressed_size` (10) exceeds the slice; the
/// `try_extend_and_fill` failure must surface as
/// `BackendOverflow { step: RLE }`, never panic.
#[test]
fn rle_oversized_against_user_slice_backend_returns_backend_overflow() {
    use crate::decoding::decode_buffer::DecodeBuffer;
    use crate::decoding::scratch::{DirectScratch, FSEScratch, HuffmanScratch};
    use crate::decoding::user_slice_buf::UserSliceBackend;

    let mut output = [0u8; 4];
    let backend = UserSliceBackend::from_slice(&mut output);
    let buffer = DecodeBuffer::from_backend(backend, 1 << 20);
    let mut huf = HuffmanScratch::new();
    let mut fse = FSEScratch::new();
    let mut offset_hist = [1u32, 4, 8];
    let mut literals_buffer = alloc::vec::Vec::new();
    let mut block_content_buffer = alloc::vec::Vec::new();
    let mut direct = DirectScratch {
        huf: &mut huf,
        fse: &mut fse,
        offset_hist: &mut offset_hist,
        literals_buffer: &mut literals_buffer,
        block_content_buffer: &mut block_content_buffer,
        buffer,
    };

    let mut d = primed_decoder();
    let payload = [0xCDu8];
    let mut src: &[u8] = &payload;
    let h = header(BlockType::RLE, 10, 1);
    let err = d
        .decode_block_content_from_slice(&h, &mut direct, None, &mut src)
        .expect_err("RLE 10 bytes into 4-byte slice must error");
    match err {
        DecodeBlockContentError::BackendOverflow { step } => {
            assert_eq!(step, BlockType::RLE);
        }
        other => panic!("expected BackendOverflow, got {other:?}"),
    }
    assert_eq!(direct.buffer.len(), 0, "no bytes written on overflow");
}

/// Regression test: on BackendOverflow error from the RLE
/// fallible write, the input `*source` must NOT have been
/// advanced. Otherwise `FrameDecoder::bytes_read_counter`
/// accounting is off by one byte on the error path: the caller
/// exits early and the 1-byte advance never gets reflected in
/// the read counter, but the next call would skip past the RLE
/// byte.
#[test]
fn rle_overflow_leaves_source_unadvanced() {
    use crate::decoding::decode_buffer::DecodeBuffer;
    use crate::decoding::scratch::{DirectScratch, FSEScratch, HuffmanScratch};
    use crate::decoding::user_slice_buf::UserSliceBackend;

    let mut output = [0u8; 4];
    let backend = UserSliceBackend::from_slice(&mut output);
    let buffer = DecodeBuffer::from_backend(backend, 1 << 20);
    let mut huf = HuffmanScratch::new();
    let mut fse = FSEScratch::new();
    let mut offset_hist = [1u32, 4, 8];
    let mut literals_buffer = alloc::vec::Vec::new();
    let mut block_content_buffer = alloc::vec::Vec::new();
    let mut direct = DirectScratch {
        huf: &mut huf,
        fse: &mut fse,
        offset_hist: &mut offset_hist,
        literals_buffer: &mut literals_buffer,
        block_content_buffer: &mut block_content_buffer,
        buffer,
    };

    let mut d = primed_decoder();
    let payload = [0xCDu8, 0xEE, 0xFF];
    let mut src: &[u8] = &payload;
    let h = header(BlockType::RLE, 10, 1);
    let _ = d
        .decode_block_content_from_slice(&h, &mut direct, None, &mut src)
        .expect_err("RLE 10 bytes into 4-byte slice must error");
    assert_eq!(
        src.as_ptr(),
        payload.as_ptr(),
        "source advanced despite write failure"
    );
    assert_eq!(
        src.len(),
        payload.len(),
        "source length changed on error path"
    );
}

#[test]
fn rle_advances_source_by_one_byte_and_extends_buffer() {
    // Happy path on a freshly primed decoder: 1 byte consumed
    // from source, N bytes filled into buffer.
    let mut d = primed_decoder();
    let mut ws = fresh_workspace();
    let payload = [0xCD, 0xFF, 0xAA];
    let mut src: &[u8] = &payload;
    let h = header(BlockType::RLE, 7, 1);
    let consumed = d
        .decode_block_content_from_slice(&h, &mut ws, None, &mut src)
        .expect("RLE happy path");
    assert_eq!(consumed, 1);
    assert_eq!(src, &payload[1..], "1 byte consumed from source");
    assert_eq!(ws.buffer.len(), 7, "buffer extended by decompressed_size");
}
