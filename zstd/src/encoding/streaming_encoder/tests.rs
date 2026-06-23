use crate::decoding::StreamingDecoder;
use crate::encoding::{CompressionLevel, Matcher, Sequence, StreamingEncoder};
use crate::io::{Error, ErrorKind, Read, Write};
use alloc::vec;
use alloc::vec::Vec;

struct TinyMatcher {
    last_space: Vec<u8>,
    window_size: u64,
}

impl TinyMatcher {
    fn new(window_size: u64) -> Self {
        Self {
            last_space: Vec::new(),
            window_size,
        }
    }
}

impl Matcher for TinyMatcher {
    fn get_next_space(&mut self) -> Vec<u8> {
        vec![0; self.window_size as usize]
    }

    fn get_last_space(&mut self) -> &[u8] {
        self.last_space.as_slice()
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        self.last_space = space;
    }

    fn skip_matching(&mut self) {}

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        handle_sequence(Sequence::Literals {
            literals: self.last_space.as_slice(),
        });
    }

    fn reset(&mut self, _level: CompressionLevel) {
        self.last_space.clear();
    }

    fn window_size(&self) -> u64 {
        self.window_size
    }
}

struct FailingWriteOnce {
    writes: usize,
    fail_on_write_number: usize,
    sink: Vec<u8>,
}

impl FailingWriteOnce {
    fn new(fail_on_write_number: usize) -> Self {
        Self {
            writes: 0,
            fail_on_write_number,
            sink: Vec::new(),
        }
    }
}

impl Write for FailingWriteOnce {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.writes += 1;
        if self.writes == self.fail_on_write_number {
            return Err(super::other_error("injected write failure"));
        }
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

struct FailingWithKind {
    writes: usize,
    fail_on_write_number: usize,
    kind: ErrorKind,
}

impl FailingWithKind {
    fn new(fail_on_write_number: usize, kind: ErrorKind) -> Self {
        Self {
            writes: 0,
            fail_on_write_number,
            kind,
        }
    }
}

impl Write for FailingWithKind {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.writes += 1;
        if self.writes == self.fail_on_write_number {
            return Err(Error::from(self.kind));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

struct PartialThenFailWriter {
    writes: usize,
    fail_on_write_number: usize,
    partial_prefix_len: usize,
    terminal_failure: bool,
    sink: Vec<u8>,
}

impl PartialThenFailWriter {
    fn new(fail_on_write_number: usize, partial_prefix_len: usize) -> Self {
        Self {
            writes: 0,
            fail_on_write_number,
            partial_prefix_len,
            terminal_failure: false,
            sink: Vec::new(),
        }
    }
}

impl Write for PartialThenFailWriter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        if self.terminal_failure {
            return Err(super::other_error("injected terminal write failure"));
        }

        self.writes += 1;
        if self.writes == self.fail_on_write_number {
            let written = core::cmp::min(self.partial_prefix_len, buf.len());
            if written > 0 {
                self.sink.extend_from_slice(&buf[..written]);
                self.terminal_failure = true;
                return Ok(written);
            }
            return Err(super::other_error("injected terminal write failure"));
        }

        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Pre-write `set_magicless(true)` → emitted frame omits the
/// magic prefix AND round-trips through a magicless-aware
/// decoder.
#[test]
fn streaming_encoder_set_magicless_before_write_omits_magic_and_roundtrips() {
    use crate::common::MAGIC_NUM;
    let payload = b"streaming-magicless-roundtrip-".repeat(64);

    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder
        .set_magicless(true)
        .expect("set_magicless pre-write");
    encoder.write_all(&payload).unwrap();
    let compressed = encoder.finish().unwrap();

    assert!(
        !compressed.starts_with(&MAGIC_NUM.to_le_bytes()),
        "magicless frame must omit the 4-byte magic prefix",
    );

    let mut decoder = crate::decoding::FrameDecoder::new();
    decoder.set_magicless(true);
    let mut cursor: &[u8] = compressed.as_slice();
    decoder.init(&mut cursor).expect("magicless init");
    decoder
        .decode_blocks(&mut cursor, crate::decoding::BlockDecodingStrategy::All)
        .expect("decode_blocks");
    let mut decoded: Vec<u8> = Vec::new();
    decoder
        .collect_to_writer(&mut decoded)
        .expect("collect_to_writer");
    assert_eq!(decoded, payload);
}

/// `set_magicless` after the first write MUST return an error
/// (the frame header has already been emitted, flipping the flag
/// can't affect the current frame). Mirrors
/// `set_pledged_content_size` / `set_source_size_hint` semantics.
#[test]
fn streaming_encoder_set_magicless_after_first_write_errors() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.write_all(b"first-block").unwrap();
    let err = encoder
        .set_magicless(true)
        .expect_err("set_magicless after first write must error");
    assert_eq!(
        err.kind(),
        crate::io::ErrorKind::InvalidInput,
        "expected InvalidInput when setting magicless after frame_started, got {err:?}",
    );
}

#[test]
fn streaming_encoder_roundtrip_multiple_writes() {
    let payload = b"streaming-encoder-roundtrip-".repeat(1024);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    for chunk in payload.chunks(313) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn flush_emits_nonempty_partial_output() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.write_all(b"partial-block").unwrap();
    encoder.flush().unwrap();
    let flushed_len = encoder.get_ref().len();
    assert!(
        flushed_len > 0,
        "flush should emit header+partial block bytes"
    );
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, b"partial-block");
}

#[test]
fn flush_without_writes_does_not_emit_frame_header() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.flush().unwrap();
    assert!(encoder.get_ref().is_empty());
}

#[test]
fn block_boundary_write_emits_block_in_same_call() {
    let mut boundary = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(4),
        Vec::new(),
        CompressionLevel::Uncompressed,
    );
    let mut below = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(4),
        Vec::new(),
        CompressionLevel::Uncompressed,
    );

    boundary.write_all(b"ABCD").unwrap();
    below.write_all(b"ABC").unwrap();

    let boundary_len = boundary.get_ref().len();
    let below_len = below.get_ref().len();
    assert!(
        boundary_len > below_len,
        "full block should be emitted immediately at block boundary"
    );
}

#[test]
fn finish_consumes_encoder_and_emits_frame() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.write_all(b"abc").unwrap();
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, b"abc");
}

#[test]
fn finish_without_writes_emits_empty_frame() {
    let encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert!(decoded.is_empty());
}

#[test]
fn write_empty_buffer_returns_zero() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    assert_eq!(encoder.write(&[]).unwrap(), 0);
    let _ = encoder.finish().unwrap();
}

#[test]
fn uncompressed_level_roundtrip() {
    let payload = b"uncompressed-streaming-roundtrip".repeat(64);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Uncompressed);
    for chunk in payload.chunks(41) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn better_level_streaming_roundtrip() {
    let payload = b"better-level-streaming-test".repeat(256);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Better);
    for chunk in payload.chunks(53) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn zero_window_matcher_returns_invalid_input_error() {
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(0),
        Vec::new(),
        CompressionLevel::Fastest,
    );
    let err = encoder.write_all(b"payload").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn best_level_streaming_roundtrip() {
    // 200 KiB payload crosses the 128 KiB block boundary, exercising
    // multi-block emission and matcher state carry-over for Best.
    let payload = b"best-level-streaming-test".repeat(8 * 1024);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Best);
    for chunk in payload.chunks(53) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn write_failure_poisoning_is_sticky() {
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(4),
        FailingWriteOnce::new(1),
        CompressionLevel::Uncompressed,
    );

    assert!(encoder.write_all(b"ABCD").is_err());
    assert!(encoder.flush().is_err());
    assert!(encoder.write_all(b"EFGH").is_err());
    assert_eq!(encoder.get_ref().sink.len(), 0);
    assert!(encoder.finish().is_err());
}

#[test]
fn poisoned_encoder_returns_original_error_kind() {
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(4),
        FailingWithKind::new(1, ErrorKind::BrokenPipe),
        CompressionLevel::Uncompressed,
    );

    let first_error = encoder.write_all(b"ABCD").unwrap_err();
    assert_eq!(first_error.kind(), ErrorKind::BrokenPipe);

    let second_error = encoder.write_all(b"EFGH").unwrap_err();
    assert_eq!(second_error.kind(), ErrorKind::BrokenPipe);
}

#[test]
fn write_reports_progress_but_poisoning_is_sticky_after_later_block_failure() {
    let payload = b"ABCDEFGHIJKL";
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(4),
        FailingWriteOnce::new(3),
        CompressionLevel::Uncompressed,
    );

    let first_write = encoder.write(payload).unwrap();
    assert_eq!(first_write, 8);
    assert!(encoder.write(&payload[first_write..]).is_err());
    assert!(encoder.flush().is_err());
    assert!(encoder.write_all(b"EFGH").is_err());
}

#[test]
fn partial_write_failure_after_progress_poisons_encoder() {
    let payload = b"ABCDEFGHIJKL";
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(4),
        PartialThenFailWriter::new(3, 1),
        CompressionLevel::Uncompressed,
    );

    let first_write = encoder.write(payload).unwrap();
    assert_eq!(first_write, 8);

    let second_write = encoder.write(&payload[first_write..]);
    assert!(second_write.is_err());
    assert!(encoder.flush().is_err());
    assert!(encoder.write_all(b"MNOP").is_err());
}

#[test]
fn new_with_matcher_and_get_mut_work() {
    let matcher = TinyMatcher::new(128 * 1024);
    let mut encoder =
        StreamingEncoder::new_with_matcher(matcher, Vec::new(), CompressionLevel::Fastest);
    encoder.get_mut().extend_from_slice(b"");
    encoder.write_all(b"custom-matcher").unwrap();
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, b"custom-matcher");
}

#[test]
fn pledged_content_size_written_in_header() {
    let payload = b"hello world, pledged size test";
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder
        .set_pledged_content_size(payload.len() as u64)
        .unwrap();
    encoder.write_all(payload).unwrap();
    let compressed = encoder.finish().unwrap();

    // Verify FCS is present and correct
    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    assert_eq!(header.frame_content_size(), payload.len() as u64);

    // Verify roundtrip
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn pledged_content_size_mismatch_returns_error() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.set_pledged_content_size(100).unwrap();
    encoder.write_all(b"short payload").unwrap(); // 13 bytes != 100 pledged
    let err = encoder.finish().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn write_exceeding_pledge_returns_error() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.set_pledged_content_size(5).unwrap();
    let err = encoder.write_all(b"exceeds five bytes").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn write_straddling_pledge_reports_partial_progress() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.set_pledged_content_size(5).unwrap();
    // write() should accept exactly 5 bytes (partial progress)
    assert_eq!(encoder.write(b"abcdef").unwrap(), 5);
    // Next write should fail — pledge exhausted
    let err = encoder.write(b"g").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn encoded_scratch_capacity_is_reused_across_blocks() {
    let payload = vec![0xAB; 64 * 3];
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(64),
        Vec::new(),
        CompressionLevel::Uncompressed,
    );

    encoder.write_all(&payload[..64]).unwrap();
    let first_capacity = encoder.encoded_scratch.capacity();
    assert!(
        first_capacity >= 67,
        "expected encoded scratch to keep block header + payload capacity",
    );

    encoder.write_all(&payload[64..128]).unwrap();
    let second_capacity = encoder.encoded_scratch.capacity();
    assert!(
        second_capacity >= first_capacity,
        "encoded scratch capacity should be reused across block emits",
    );

    encoder.write_all(&payload[128..]).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn pledged_content_size_after_write_returns_error() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.write_all(b"already writing").unwrap();
    let err = encoder.set_pledged_content_size(15).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn source_size_hint_directly_reduces_window_header() {
    let payload = b"streaming-source-size-hint".repeat(64);

    let mut no_hint = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(11));
    no_hint.write_all(payload.as_slice()).unwrap();
    let no_hint_frame = no_hint.finish().unwrap();
    let no_hint_header = crate::decoding::frame::read_frame_header(no_hint_frame.as_slice())
        .unwrap()
        .0;
    let no_hint_window = no_hint_header.window_size().unwrap();

    let mut with_hint = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(11));
    with_hint
        .set_source_size_hint(payload.len() as u64)
        .unwrap();
    with_hint.write_all(payload.as_slice()).unwrap();
    let late_hint_err = with_hint
        .set_source_size_hint(payload.len() as u64)
        .unwrap_err();
    assert_eq!(late_hint_err.kind(), ErrorKind::InvalidInput);
    let with_hint_frame = with_hint.finish().unwrap();
    let with_hint_header = crate::decoding::frame::read_frame_header(with_hint_frame.as_slice())
        .unwrap()
        .0;
    let with_hint_window = with_hint_header.window_size().unwrap();

    assert!(
        with_hint_window <= no_hint_window,
        "source size hint should not increase advertised window"
    );

    let mut decoder = StreamingDecoder::new(with_hint_frame.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn single_segment_requires_pledged_to_fit_matcher_window() {
    let payload = b"streaming-window-gate-".repeat(60); // 1320 bytes
    let mut encoder = StreamingEncoder::new_with_matcher(
        TinyMatcher::new(1024),
        Vec::new(),
        CompressionLevel::Fastest,
    );
    encoder
        .set_pledged_content_size(payload.len() as u64)
        .unwrap();
    encoder.write_all(payload.as_slice()).unwrap();
    let compressed = encoder.finish().unwrap();

    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    assert_eq!(header.frame_content_size(), payload.len() as u64);
    assert!(
        !header.descriptor.single_segment_flag(),
        "single-segment must stay off when pledged content size exceeds matcher window"
    );
    assert!(
        header.window_size().unwrap() >= 1024,
        "window descriptor should be present when single-segment is disabled"
    );
}

#[test]
fn ensure_frame_started_refreshes_stale_strategy_tag_at_reset() {
    // The literal-compression gates (`min_literals_to_compress`,
    // `min_gain`) read `state.strategy_tag`. Regression: every
    // reset site MUST refresh that tag from the active compression
    // level — relying on construction-time initialization alone is
    // not enough, because later mutations or reuse patterns can
    // leave the tag stale.
    //
    // To exercise the RESET-time refresh (not just the
    // construction-time init that `StreamingEncoder::new` does for
    // free), this test deliberately corrupts `state.strategy_tag`
    // to a value that does NOT match the active level, then
    // triggers `ensure_frame_started` and asserts the reset path
    // wrote the correct tag back. If the sync line in
    // `ensure_frame_started` were deleted, the corrupted value
    // would survive the write and fail the assertion.
    use crate::encoding::strategy::StrategyTag;
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
    ] {
        let expected = StrategyTag::for_compression_level(level);
        let mut encoder = StreamingEncoder::new(Vec::new(), level);
        // Pick a sentinel that differs from the legitimate tag so
        // a missing reset-time sync is observable. BtUltra2 is the
        // most-aggressive variant; the four levels above resolve
        // to Fast/Dfast/Lazy/Lazy respectively, none equal to it.
        let sentinel = StrategyTag::BtUltra2;
        assert_ne!(
            expected, sentinel,
            "sentinel must differ from the legitimate tag at level {level:?}",
        );
        encoder.state.strategy_tag = sentinel;
        encoder.write_all(b"x").unwrap();
        assert_eq!(
            encoder.state.strategy_tag, expected,
            "reset-time strategy_tag sync missing at level {level:?}: \
                 sentinel survived `ensure_frame_started`",
        );
        let _ = encoder.finish().unwrap();
    }
}

/// Level 22 advertises the largest default window (`window_log 27` =
/// 128 MiB). Because streaming omits FCS, that window is written verbatim
/// into the frame header — so the encoder's max window MUST NOT exceed the
/// decoder's [`crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE`], or our own
/// decoder rejects our own frame with `WindowSizeTooBig`. Regression for
/// the encoder↔decoder window-cap mismatch: streaming L22 must round-trip
/// through `StreamingDecoder` (and, implicitly, any stock zstd decoder,
/// which accepts up to the same 128 MiB default).
#[test]
fn level_22_streaming_window_roundtrips_in_our_decoder() {
    let payload = b"level-22-streaming-window-cap-".repeat(512);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(22));
    for chunk in payload.chunks(101) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();

    // The advertised window equals the L22 default (128 MiB) and must sit
    // at or below the decoder cap — otherwise the round-trip below fails.
    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    let window = header.window_size().unwrap();
    assert!(
        window <= crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE,
        "L22 advertised window {window} exceeds decoder cap {}",
        crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE,
    );

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

/// `set_content_checksum(false)` before the first write must clear the
/// frame header's `Content_Checksum_flag` and the frame must still
/// round-trip through the decoder.
#[test]
fn streaming_encoder_set_content_checksum_false_clears_header_flag() {
    let payload = b"streaming-checksum-toggle-".repeat(64);
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder
        .set_content_checksum(false)
        .expect("set_content_checksum pre-write");
    encoder.write_all(&payload).unwrap();
    let compressed = encoder.finish().unwrap();

    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    assert!(
        !header.descriptor.content_checksum_flag(),
        "content_checksum(false) must clear the frame header flag",
    );

    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

/// With the `hash` feature, disabling the checksum must drop exactly the
/// 4-byte XXH64 trailer: the same payload encoded with the checksum on is
/// 4 bytes longer and its header flag is set.
#[cfg(feature = "hash")]
#[test]
fn streaming_encoder_set_content_checksum_false_omits_trailer() {
    let payload = b"streaming-checksum-trailer-".repeat(64);

    let mut with = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    // Explicit: the encoder default is off (upstream library parity).
    with.set_content_checksum(true)
        .expect("set_content_checksum pre-write");
    with.write_all(&payload).unwrap();
    let with_checksum = with.finish().unwrap();

    let mut without = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    without
        .set_content_checksum(false)
        .expect("set_content_checksum pre-write");
    without.write_all(&payload).unwrap();
    let without_checksum = without.finish().unwrap();

    assert!(
        crate::decoding::frame::read_frame_header(with_checksum.as_slice())
            .unwrap()
            .0
            .descriptor
            .content_checksum_flag(),
        "default checksum-on frame must set the header flag",
    );
    assert_eq!(
        with_checksum.len(),
        without_checksum.len() + 4,
        "checksum-on frame must carry exactly the 4-byte XXH64 trailer",
    );
}

/// `set_content_checksum` after the first write must error: the frame
/// header (and its checksum flag) is already emitted, so a late flip would
/// desync the header flag from the emitted trailer. Mirrors
/// `set_magicless` / `set_pledged_content_size` semantics.
#[test]
fn streaming_encoder_set_content_checksum_after_first_write_errors() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.write_all(b"first-block").unwrap();
    let err = encoder
        .set_content_checksum(false)
        .expect_err("set_content_checksum after first write must error");
    assert_eq!(
        err.kind(),
        ErrorKind::InvalidInput,
        "expected InvalidInput when setting content checksum after frame_started, got {err:?}",
    );
}

#[test]
fn no_pledged_size_omits_fcs_from_header() {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
    encoder.write_all(b"no pledged size").unwrap();
    let compressed = encoder.finish().unwrap();

    // FCS should be omitted from the header; the decoder reports absent FCS as 0.
    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    assert_eq!(header.frame_content_size(), 0);
    // Verify the descriptor confirms FCS field is truly absent (0 bytes),
    // not just FCS present with value 0.
    assert_eq!(header.descriptor.frame_content_size_bytes().unwrap(), 0);
}

#[test]
fn streaming_encoder_with_dictionary_roundtrips_and_carries_dict_id() {
    use alloc::format;
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let dict_id = crate::decoding::Dictionary::decode_dict(dict_raw)
        .unwrap()
        .id;

    // Dictionary-resembling payload (the dict was trained on similar lines),
    // fed in many small writes so the dict + cross-block matching are both
    // exercised by the streaming path.
    let mut payload = Vec::new();
    for i in 0..400u32 {
        payload.extend_from_slice(
            format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n")
                .as_bytes(),
        );
    }

    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(19));
    encoder
        .set_dictionary_from_bytes(dict_raw)
        .expect("attach dictionary");
    for chunk in payload.chunks(777) {
        encoder.write_all(chunk).unwrap();
    }
    let compressed = encoder.finish().unwrap();

    // The frame header advertises the dictionary ID (single-segment is
    // disabled for dictionary frames, so an explicit window is present).
    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    assert_eq!(header.dictionary_id(), Some(dict_id));

    // Round-trip through a decoder primed with the SAME dictionary.
    let mut decoder =
        StreamingDecoder::new_with_dictionary_bytes(compressed.as_slice(), dict_raw).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);

    // The dictionary is actually used: the dict frame is no larger than the
    // no-dictionary frame on this dict-resembling payload (a dict that was
    // ignored could only ever make the frame the same size or bigger).
    let mut nodict = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(19));
    for chunk in payload.chunks(777) {
        nodict.write_all(chunk).unwrap();
    }
    let nodict_frame = nodict.finish().unwrap();
    assert!(
        compressed.len() <= nodict_frame.len(),
        "dict frame {} should not exceed no-dict frame {}",
        compressed.len(),
        nodict_frame.len()
    );
}

#[test]
fn streaming_encoder_strategy_override_survives_frame_start() {
    // A `.strategy(...)` override must drive BOTH the matcher and the
    // literal-compression gates (`state.strategy_tag`) once the frame
    // starts. `ensure_frame_started` re-syncs the tag, so without persisting
    // the override it would silently fall back to the level's strategy and
    // diverge from `FrameCompressor` for the same parameters.
    use crate::encoding::{CompressionParameters, Strategy};
    let level = CompressionLevel::Fastest;
    let level_tag = crate::encoding::strategy::StrategyTag::for_compression_level(level);
    let override_tag = Strategy::Greedy.tag();
    assert_ne!(
        level_tag, override_tag,
        "test needs an override that changes the derived tag"
    );

    let params = CompressionParameters::builder(level)
        .strategy(Strategy::Greedy)
        .build()
        .unwrap();
    let payload = b"override must outlive the frame header";
    let mut encoder = StreamingEncoder::new(Vec::new(), level);
    encoder.set_parameters(&params).unwrap();
    encoder.write_all(payload).unwrap();
    assert_eq!(
        encoder.state.strategy_tag, override_tag,
        "strategy override was discarded when the frame started"
    );

    let compressed = encoder.finish().unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn streaming_encoder_uncompressed_with_dictionary_omits_dict_id() {
    // At `Uncompressed` the matcher cannot prime a dictionary, so an
    // attached dictionary must NOT be reflected in the frame: advertising a
    // `Dictionary_ID` would force a dictionary at decode time for a frame
    // that does not actually depend on one. Mirrors `FrameCompressor`'s
    // `use_dictionary_state` gate.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let payload = b"tenant=demo table=orders region=eu payload=aaaaabbbbbccccc";
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Uncompressed);
    encoder
        .set_dictionary_from_bytes(dict_raw)
        .expect("attach dictionary");
    encoder.write_all(payload).unwrap();
    let compressed = encoder.finish().unwrap();

    let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
        .unwrap()
        .0;
    assert_eq!(
        header.dictionary_id(),
        None,
        "uncompressed frame must not require a dictionary at decode time"
    );

    // Decodes WITHOUT any dictionary.
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    assert_eq!(decoded, payload);
}
