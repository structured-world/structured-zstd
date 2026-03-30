use alloc::vec::Vec;
use core::mem;

use crate::common::MAX_BLOCK_SIZE;
#[cfg(feature = "hash")]
use core::hash::Hasher;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

use crate::encoding::levels::compress_fastest;
use crate::encoding::{
    CompressionLevel, MatchGeneratorDriver, Matcher, block_header::BlockHeader,
    frame_compressor::CompressState, frame_compressor::FseTables, frame_header::FrameHeader,
};
use crate::io::{Error, ErrorKind, Write};

/// Incremental frame encoder that implements [`Write`].
///
/// Data can be provided with multiple `write()` calls. Full blocks are compressed
/// automatically, `flush()` emits the currently buffered partial block as non-last,
/// and `finish()` closes the frame and returns the wrapped writer.
pub struct StreamingEncoder<W: Write, M: Matcher = MatchGeneratorDriver> {
    drain: Option<W>,
    compression_level: CompressionLevel,
    state: CompressState<M>,
    pending: Vec<u8>,
    errored: bool,
    last_error_kind: Option<ErrorKind>,
    frame_started: bool,
    frame_finished: bool,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
}

impl<W: Write> StreamingEncoder<W, MatchGeneratorDriver> {
    pub fn new(drain: W, compression_level: CompressionLevel) -> Self {
        Self::new_with_matcher(
            MatchGeneratorDriver::new(1024 * 128, 1),
            drain,
            compression_level,
        )
    }
}

impl<W: Write, M: Matcher> StreamingEncoder<W, M> {
    pub fn new_with_matcher(matcher: M, drain: W, compression_level: CompressionLevel) -> Self {
        Self {
            drain: Some(drain),
            compression_level,
            state: CompressState {
                matcher,
                last_huff_table: None,
                fse_tables: FseTables::new(),
                offset_hist: [1, 4, 8],
            },
            pending: Vec::new(),
            errored: false,
            last_error_kind: None,
            frame_started: false,
            frame_finished: false,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
    }

    pub fn get_ref(&self) -> Option<&W> {
        self.drain.as_ref()
    }

    pub fn get_mut(&mut self) -> Option<&mut W> {
        self.drain.as_mut()
    }

    pub fn finish(mut self) -> Result<W, Error> {
        self.ensure_open()?;
        self.ensure_frame_started()?;

        if self.pending.is_empty() {
            self.write_empty_last_block()
                .map_err(|err| self.fail(err))?;
        } else {
            self.emit_pending_block(true)?;
        }

        #[cfg(feature = "hash")]
        {
            let checksum = self.hasher.finish() as u32;
            self.drain_mut()
                .and_then(|drain| drain.write_all(&checksum.to_le_bytes()))
                .map_err(|err| self.fail(err))?;
        }

        self.frame_finished = true;
        self.drain_mut()
            .and_then(|drain| drain.flush())
            .map_err(|err| self.fail(err))?;
        self.drain
            .take()
            .ok_or_else(|| other_error("streaming encoder drain already taken"))
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.errored {
            return Err(error_from_kind(
                self.last_error_kind.unwrap_or(ErrorKind::Other),
            ));
        }
        if self.frame_finished {
            return Err(other_error(
                "cannot write to a finished streaming encoder frame",
            ));
        }
        Ok(())
    }

    fn drain_mut(&mut self) -> Result<&mut W, Error> {
        self.drain
            .as_mut()
            .ok_or_else(|| other_error("streaming encoder has no active drain"))
    }

    fn ensure_frame_started(&mut self) -> Result<(), Error> {
        if self.frame_started {
            return Ok(());
        }

        self.ensure_level_supported()?;
        self.state.matcher.reset(self.compression_level);
        self.state.offset_hist = [1, 4, 8];
        self.state.last_huff_table = None;
        self.state.fse_tables.ll_previous = None;
        self.state.fse_tables.ml_previous = None;
        self.state.fse_tables.of_previous = None;
        #[cfg(feature = "hash")]
        {
            self.hasher = XxHash64::with_seed(0);
        }

        let header = FrameHeader {
            frame_content_size: None,
            single_segment: false,
            content_checksum: cfg!(feature = "hash"),
            dictionary_id: None,
            window_size: Some(self.state.matcher.window_size()),
        };
        let mut encoded_header = Vec::new();
        header.serialize(&mut encoded_header);
        self.drain_mut()
            .and_then(|drain| drain.write_all(&encoded_header))
            .map_err(|err| self.fail(err))?;

        self.frame_started = true;
        Ok(())
    }

    fn block_capacity(&self) -> usize {
        let matcher_window = self.state.matcher.window_size() as usize;
        core::cmp::max(1, core::cmp::min(matcher_window, MAX_BLOCK_SIZE as usize))
    }

    fn allocate_pending_space(&mut self, block_capacity: usize) -> Vec<u8> {
        let mut space = match self.compression_level {
            CompressionLevel::Fastest | CompressionLevel::Default => {
                self.state.matcher.get_next_space()
            }
            _ => Vec::new(),
        };
        space.clear();
        if space.capacity() < block_capacity {
            space.reserve(block_capacity - space.capacity());
        }
        space
    }

    fn emit_full_pending_block(
        &mut self,
        block_capacity: usize,
        consumed: usize,
    ) -> Option<Result<usize, Error>> {
        if self.pending.len() != block_capacity {
            return None;
        }

        let new_pending = self.allocate_pending_space(block_capacity);
        let full_block = mem::replace(&mut self.pending, new_pending);
        if let Err((err, restored_block)) = self.encode_block(full_block, false) {
            self.pending = restored_block;
            let err = self.fail(err);
            if consumed > 0 {
                return Some(Ok(consumed));
            }
            return Some(Err(err));
        }
        None
    }

    fn emit_pending_block(&mut self, last_block: bool) -> Result<(), Error> {
        let block = mem::take(&mut self.pending);
        if let Err((err, restored_block)) = self.encode_block(block, last_block) {
            self.pending = restored_block;
            return Err(self.fail(err));
        }
        if !last_block {
            let block_capacity = self.block_capacity();
            self.pending = self.allocate_pending_space(block_capacity);
        }
        Ok(())
    }

    fn ensure_level_supported(&self) -> Result<(), Error> {
        match self.compression_level {
            CompressionLevel::Uncompressed
            | CompressionLevel::Fastest
            | CompressionLevel::Default => Ok(()),
            _ => Err(other_error(
                "streaming encoder currently supports Uncompressed/Fastest/Default only",
            )),
        }
    }

    fn encode_block(
        &mut self,
        uncompressed_data: Vec<u8>,
        last_block: bool,
    ) -> Result<(), (Error, Vec<u8>)> {
        let mut raw_block = Some(uncompressed_data);
        let mut encoded = Vec::with_capacity(self.block_capacity());
        let mut moved_into_matcher = false;
        if raw_block.as_ref().is_some_and(|block| block.is_empty()) {
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size: 0,
            };
            header.serialize(&mut encoded);
        } else {
            match self.compression_level {
                CompressionLevel::Uncompressed => {
                    let block = raw_block.as_ref().expect("raw block missing");
                    let header = BlockHeader {
                        last_block,
                        block_type: crate::blocks::block::BlockType::Raw,
                        block_size: block.len() as u32,
                    };
                    header.serialize(&mut encoded);
                    encoded.extend_from_slice(block);
                }
                CompressionLevel::Fastest | CompressionLevel::Default => {
                    let block = raw_block.take().expect("raw block missing");
                    compress_fastest(&mut self.state, last_block, block, &mut encoded);
                    moved_into_matcher = true;
                }
                _ => {
                    return Err((
                        other_error(
                            "streaming encoder currently supports Uncompressed/Fastest/Default only",
                        ),
                        raw_block.unwrap_or_default(),
                    ));
                }
            }
        }

        if let Err(err) = self.drain_mut().and_then(|drain| drain.write_all(&encoded)) {
            let restored = if moved_into_matcher {
                self.state.matcher.get_last_space().to_vec()
            } else {
                raw_block.unwrap_or_default()
            };
            return Err((err, restored));
        }

        if moved_into_matcher {
            #[cfg(feature = "hash")]
            {
                self.hasher.write(self.state.matcher.get_last_space());
            }
            #[cfg(not(feature = "hash"))]
            {
                self.hash_block(&[]);
            }
        } else {
            self.hash_block(raw_block.as_deref().unwrap_or(&[]));
        }
        Ok(())
    }

    fn write_empty_last_block(&mut self) -> Result<(), Error> {
        self.encode_block(Vec::new(), true).map_err(|(err, _)| err)
    }

    fn fail(&mut self, err: Error) -> Error {
        self.errored = true;
        if self.last_error_kind.is_none() {
            self.last_error_kind = Some(err.kind());
        }
        err
    }

    #[cfg(feature = "hash")]
    fn hash_block(&mut self, uncompressed_data: &[u8]) {
        self.hasher.write(uncompressed_data);
    }

    #[cfg(not(feature = "hash"))]
    fn hash_block(&mut self, _uncompressed_data: &[u8]) {}
}

impl<W: Write, M: Matcher> Write for StreamingEncoder<W, M> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.ensure_open()?;
        if buf.is_empty() {
            return Ok(0);
        }

        self.ensure_frame_started()?;
        let block_capacity = self.block_capacity();
        if self.pending.capacity() == 0 {
            self.pending = self.allocate_pending_space(block_capacity);
        }
        let mut remaining = buf;
        let mut consumed = 0usize;

        while !remaining.is_empty() {
            if let Some(result) = self.emit_full_pending_block(block_capacity, consumed) {
                return result;
            }

            let available = block_capacity - self.pending.len();
            let to_take = core::cmp::min(remaining.len(), available);
            if to_take == 0 {
                break;
            }
            self.pending.extend_from_slice(&remaining[..to_take]);
            remaining = &remaining[to_take..];
            consumed += to_take;

            if let Some(result) = self.emit_full_pending_block(block_capacity, consumed) {
                return result;
            }
        }
        Ok(consumed)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.ensure_open()?;
        self.ensure_frame_started()?;
        if !self.pending.is_empty() {
            self.emit_pending_block(false)?;
        }
        self.drain_mut()
            .and_then(|drain| drain.flush())
            .map_err(|err| self.fail(err))
    }
}

fn error_from_kind(kind: ErrorKind) -> Error {
    #[cfg(feature = "std")]
    {
        Error::from(kind)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::from(kind)
    }
}

fn other_error(message: &str) -> Error {
    #[cfg(feature = "std")]
    {
        Error::other(message)
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = message;
        Error::from(ErrorKind::Other)
    }
}

#[cfg(test)]
mod tests {
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
        let flushed_len = encoder.get_ref().unwrap().len();
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

        let boundary_len = boundary.get_ref().unwrap().len();
        let below_len = below.get_ref().unwrap().len();
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
    fn better_level_returns_unsupported_error() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Better);
        assert!(encoder.write_all(b"payload").is_err());
        assert!(encoder.finish().is_err());
    }

    #[test]
    fn unsupported_level_write_fails_before_emitting_frame_header() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Better);
        assert!(encoder.write_all(b"payload").is_err());
        assert_eq!(encoder.get_ref().unwrap().len(), 0);
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
        assert_eq!(encoder.get_ref().unwrap().sink.len(), 0);
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
        encoder.get_mut().unwrap().extend_from_slice(b"");
        encoder.write_all(b"custom-matcher").unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"custom-matcher");
    }

    #[cfg(feature = "std")]
    #[test]
    fn streaming_encoder_output_decompresses_with_c_zstd() {
        let payload = b"tenant=demo op=put key=streaming value=abcdef\n".repeat(4096);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        for chunk in payload.chunks(1024) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        let mut decoded = Vec::with_capacity(payload.len());
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }
}
