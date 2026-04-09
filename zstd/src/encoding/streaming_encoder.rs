use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem;

use crate::common::MAX_BLOCK_SIZE;
#[cfg(feature = "hash")]
use core::hash::Hasher;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

use crate::encoding::levels::compress_block_encoded;
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
    encoded_scratch: Vec<u8>,
    errored: bool,
    last_error_kind: Option<ErrorKind>,
    last_error_message: Option<String>,
    frame_started: bool,
    pledged_content_size: Option<u64>,
    bytes_consumed: u64,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
}

impl<W: Write> StreamingEncoder<W, MatchGeneratorDriver> {
    /// Creates a streaming encoder backed by the default match generator.
    ///
    /// The encoder writes compressed bytes into `drain` and applies `compression_level`
    /// to all subsequently written blocks.
    pub fn new(drain: W, compression_level: CompressionLevel) -> Self {
        Self::new_with_matcher(
            MatchGeneratorDriver::new(MAX_BLOCK_SIZE as usize, 1),
            drain,
            compression_level,
        )
    }
}

impl<W: Write, M: Matcher> StreamingEncoder<W, M> {
    /// Creates a streaming encoder with an explicitly provided matcher implementation.
    ///
    /// This constructor is primarily intended for tests and advanced callers that need
    /// custom match-window behavior.
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
            encoded_scratch: Vec::new(),
            errored: false,
            last_error_kind: None,
            last_error_message: None,
            frame_started: false,
            pledged_content_size: None,
            bytes_consumed: 0,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
    }

    /// Pledge the total uncompressed content size for this frame.
    ///
    /// When set, the frame header will include a `Frame_Content_Size` field.
    /// This enables decoders to pre-allocate output buffers.
    /// The pledged size is also forwarded as a source-size hint to the
    /// matcher so small inputs can use smaller matching tables.
    ///
    /// Must be called **before** the first [`write`](Write::write) call;
    /// calling it after the frame header has already been emitted returns an
    /// error.
    pub fn set_pledged_content_size(&mut self, size: u64) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "pledged content size must be set before the first write",
            ));
        }
        self.pledged_content_size = Some(size);
        // Also use pledged size as source-size hint so the matcher
        // can select smaller tables for small inputs.
        self.state.matcher.set_source_size_hint(size);
        Ok(())
    }

    /// Provide a hint about the total uncompressed size for the next frame.
    ///
    /// Unlike [`set_pledged_content_size`](Self::set_pledged_content_size),
    /// this does **not** enforce that exactly `size` bytes are written; it
    /// may reduce matcher tables, advertised frame window, and block sizing
    /// for small inputs. Must be called before the first
    /// [`write`](Write::write).
    pub fn set_source_size_hint(&mut self, size: u64) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "source size hint must be set before the first write",
            ));
        }
        self.state.matcher.set_source_size_hint(size);
        Ok(())
    }

    /// Returns an immutable reference to the wrapped output drain.
    ///
    /// The drain remains available for the encoder lifetime; [`finish`](Self::finish)
    /// consumes the encoder and returns ownership of the drain.
    pub fn get_ref(&self) -> &W {
        self.drain
            .as_ref()
            .expect("streaming encoder drain is present until finish consumes self")
    }

    /// Returns a mutable reference to the wrapped output drain.
    ///
    /// It is inadvisable to directly write to the underlying writer, as doing
    /// so would corrupt the zstd frame being assembled by the encoder.
    ///
    /// The drain remains available for the encoder lifetime; [`finish`](Self::finish)
    /// consumes the encoder and returns ownership of the drain.
    pub fn get_mut(&mut self) -> &mut W {
        self.drain
            .as_mut()
            .expect("streaming encoder drain is present until finish consumes self")
    }

    /// Finalizes the current zstd frame and returns the wrapped output drain.
    ///
    /// If no payload was written yet, this still emits a valid empty frame.
    /// Calling this method consumes the encoder.
    pub fn finish(mut self) -> Result<W, Error> {
        self.ensure_open()?;

        // Validate the pledge before finalizing the frame. If finish() is
        // called before any writes, this also avoids emitting a header with
        // an incorrect FCS into the drain on mismatch.
        if let Some(pledged) = self.pledged_content_size
            && self.bytes_consumed != pledged
        {
            return Err(invalid_input_error(
                "pledged content size does not match bytes consumed",
            ));
        }

        self.ensure_frame_started()?;

        if self.pending.is_empty() {
            self.write_empty_last_block()
                .map_err(|err| self.fail(err))?;
        } else {
            self.emit_pending_block(true)?;
        }

        let mut drain = self
            .drain
            .take()
            .expect("streaming encoder drain must be present when finishing");

        #[cfg(feature = "hash")]
        {
            let checksum = self.hasher.finish() as u32;
            drain
                .write_all(&checksum.to_le_bytes())
                .map_err(|err| self.fail(err))?;
        }

        drain.flush().map_err(|err| self.fail(err))?;
        Ok(drain)
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.errored {
            return Err(self.sticky_error());
        }
        Ok(())
    }

    // Cold path (only reached after poisoning). The format!() calls still allocate
    // in no_std even though error_with_kind_message/other_error_owned drop the
    // message; this is acceptable on an error recovery path to keep match arms simple.
    fn sticky_error(&self) -> Error {
        match (self.last_error_kind, self.last_error_message.as_deref()) {
            (Some(kind), Some(message)) => error_with_kind_message(
                kind,
                format!(
                    "streaming encoder is in an errored state due to previous {kind:?} failure: {message}"
                ),
            ),
            (Some(kind), None) => error_from_kind(kind),
            (None, Some(message)) => other_error_owned(format!(
                "streaming encoder is in an errored state: {message}"
            )),
            (None, None) => other_error("streaming encoder is in an errored state"),
        }
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

        let window_size = self.state.matcher.window_size();
        if window_size == 0 {
            return Err(invalid_input_error(
                "matcher reported window_size == 0, which is invalid",
            ));
        }

        let header = FrameHeader {
            frame_content_size: self.pledged_content_size,
            single_segment: false,
            content_checksum: cfg!(feature = "hash"),
            dictionary_id: None,
            window_size: Some(window_size),
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
            CompressionLevel::Fastest
            | CompressionLevel::Default
            | CompressionLevel::Better
            | CompressionLevel::Best
            | CompressionLevel::Level(_) => self.state.matcher.get_next_space(),
            CompressionLevel::Uncompressed => Vec::new(),
        };
        space.clear();
        if space.capacity() > block_capacity {
            space.shrink_to(block_capacity);
        }
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

    // Exhaustive match kept intentionally: adding a new CompressionLevel
    // variant will produce a compile error here, forcing the developer to
    // decide whether the streaming encoder supports it before shipping.
    fn ensure_level_supported(&self) -> Result<(), Error> {
        match self.compression_level {
            CompressionLevel::Uncompressed
            | CompressionLevel::Fastest
            | CompressionLevel::Default
            | CompressionLevel::Better
            | CompressionLevel::Best
            | CompressionLevel::Level(_) => Ok(()),
        }
    }

    fn encode_block(
        &mut self,
        uncompressed_data: Vec<u8>,
        last_block: bool,
    ) -> Result<(), (Error, Vec<u8>)> {
        let mut raw_block = Some(uncompressed_data);
        let mut encoded = Vec::new();
        mem::swap(&mut encoded, &mut self.encoded_scratch);
        encoded.clear();
        let needed_capacity = self.block_capacity() + 3;
        if encoded.capacity() < needed_capacity {
            encoded.reserve(needed_capacity.saturating_sub(encoded.len()));
        }
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
                CompressionLevel::Fastest
                | CompressionLevel::Default
                | CompressionLevel::Better
                | CompressionLevel::Best
                | CompressionLevel::Level(_) => {
                    let block = raw_block.take().expect("raw block missing");
                    debug_assert!(!block.is_empty(), "empty blocks handled above");
                    compress_block_encoded(
                        &mut self.state,
                        self.compression_level,
                        last_block,
                        block,
                        &mut encoded,
                    );
                    moved_into_matcher = true;
                }
            }
        }

        if let Err(err) = self.drain_mut().and_then(|drain| drain.write_all(&encoded)) {
            encoded.clear();
            mem::swap(&mut encoded, &mut self.encoded_scratch);
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
        } else {
            self.hash_block(raw_block.as_deref().unwrap_or(&[]));
        }
        encoded.clear();
        mem::swap(&mut encoded, &mut self.encoded_scratch);
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
        if self.last_error_message.is_none() {
            self.last_error_message = Some(err.to_string());
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

        // Check pledge before emitting the frame header so that a misuse
        // like set_pledged_content_size(0) + write(non_empty) doesn't leave
        // a partially-written header in the drain.
        if let Some(pledged) = self.pledged_content_size
            && self.bytes_consumed >= pledged
        {
            return Err(invalid_input_error(
                "write would exceed pledged content size",
            ));
        }

        self.ensure_frame_started()?;

        // Enforce pledged upper bound: truncate the accepted slice to the
        // remaining allowance so that partial-write semantics are honored
        // (return Ok(n) with n < buf.len()) instead of failing the full call.
        let buf = if let Some(pledged) = self.pledged_content_size {
            let remaining_allowed = pledged
                .checked_sub(self.bytes_consumed)
                .ok_or_else(|| invalid_input_error("bytes consumed exceed pledged content size"))?;
            if remaining_allowed == 0 {
                return Err(invalid_input_error(
                    "write would exceed pledged content size",
                ));
            }
            let accepted = core::cmp::min(
                buf.len(),
                usize::try_from(remaining_allowed).unwrap_or(usize::MAX),
            );
            &buf[..accepted]
        } else {
            buf
        };

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
                if let Ok(n) = &result {
                    self.bytes_consumed += *n as u64;
                }
                return result;
            }
        }
        self.bytes_consumed += consumed as u64;
        Ok(consumed)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.ensure_open()?;
        if self.pending.is_empty() {
            return self
                .drain_mut()
                .and_then(|drain| drain.flush())
                .map_err(|err| self.fail(err));
        }
        self.ensure_frame_started()?;
        self.emit_pending_block(false)?;
        self.drain_mut()
            .and_then(|drain| drain.flush())
            .map_err(|err| self.fail(err))
    }
}

fn error_from_kind(kind: ErrorKind) -> Error {
    Error::from(kind)
}

fn error_with_kind_message(kind: ErrorKind, message: String) -> Error {
    #[cfg(feature = "std")]
    {
        Error::new(kind, message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(kind, alloc::boxed::Box::new(message))
    }
}

fn invalid_input_error(message: &str) -> Error {
    #[cfg(feature = "std")]
    {
        Error::new(ErrorKind::InvalidInput, message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(
            ErrorKind::Other,
            alloc::boxed::Box::new(alloc::string::String::from(message)),
        )
    }
}

fn other_error_owned(message: String) -> Error {
    #[cfg(feature = "std")]
    {
        Error::other(message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(ErrorKind::Other, alloc::boxed::Box::new(message))
    }
}

fn other_error(message: &str) -> Error {
    #[cfg(feature = "std")]
    {
        Error::other(message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(
            ErrorKind::Other,
            alloc::boxed::Box::new(alloc::string::String::from(message)),
        )
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

        fn skip_matching(&mut self, _incompressible_hint: Option<bool>) {}

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
        let with_hint_header =
            crate::decoding::frame::read_frame_header(with_hint_frame.as_slice())
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

    #[cfg(feature = "std")]
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

        // FCS should be written
        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(header.frame_content_size(), payload.len() as u64);

        // C zstd should decompress successfully
        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, payload);
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
}
