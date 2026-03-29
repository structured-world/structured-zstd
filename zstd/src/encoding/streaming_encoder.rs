use alloc::vec::Vec;
use core::mem;

#[cfg(feature = "hash")]
use core::hash::Hasher;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

use crate::encoding::levels::compress_fastest;
use crate::encoding::{
    CompressionLevel, MatchGeneratorDriver, Matcher, block_header::BlockHeader,
    frame_compressor::CompressState, frame_compressor::FseTables, frame_header::FrameHeader,
};
#[cfg(not(feature = "std"))]
use crate::io::ErrorKind;
use crate::io::{Error, Write};

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
    frame_started: bool,
    frame_finished: bool,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
}

impl<W: Write> StreamingEncoder<W, MatchGeneratorDriver> {
    pub fn new(drain: W, compression_level: CompressionLevel) -> Self {
        Self {
            drain: Some(drain),
            compression_level,
            state: CompressState {
                matcher: MatchGeneratorDriver::new(1024 * 128, 1),
                last_huff_table: None,
                fse_tables: FseTables::new(),
                offset_hist: [1, 4, 8],
            },
            pending: Vec::new(),
            frame_started: false,
            frame_finished: false,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
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

    pub fn finish(&mut self) -> Result<W, Error> {
        self.ensure_open()?;
        self.ensure_frame_started()?;

        if self.pending.is_empty() {
            self.write_empty_last_block()?;
        } else {
            let block = mem::take(&mut self.pending);
            self.encode_block(block, true)?;
        }

        #[cfg(feature = "hash")]
        {
            let checksum = self.hasher.finish() as u32;
            self.drain_mut()?.write_all(&checksum.to_le_bytes())?;
        }

        self.frame_finished = true;
        self.drain_mut()?.flush()?;
        self.drain
            .take()
            .ok_or_else(|| other_error("streaming encoder drain already taken"))
    }

    fn ensure_open(&self) -> Result<(), Error> {
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
        self.drain_mut()?.write_all(&encoded_header)?;

        self.frame_started = true;
        Ok(())
    }

    fn block_capacity(&self) -> usize {
        self.state.matcher.window_size() as usize
    }

    fn encode_block(&mut self, uncompressed_data: Vec<u8>, last_block: bool) -> Result<(), Error> {
        #[cfg(feature = "hash")]
        self.hasher.write(&uncompressed_data);

        let mut encoded = Vec::new();
        if uncompressed_data.is_empty() {
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size: 0,
            };
            header.serialize(&mut encoded);
        } else {
            match self.compression_level {
                CompressionLevel::Uncompressed => {
                    let header = BlockHeader {
                        last_block,
                        block_type: crate::blocks::block::BlockType::Raw,
                        block_size: uncompressed_data.len() as u32,
                    };
                    header.serialize(&mut encoded);
                    encoded.extend_from_slice(&uncompressed_data);
                }
                CompressionLevel::Fastest | CompressionLevel::Default => {
                    compress_fastest(&mut self.state, last_block, uncompressed_data, &mut encoded);
                }
                _ => {
                    return Err(other_error(
                        "streaming encoder currently supports Uncompressed/Fastest/Default only",
                    ));
                }
            }
        }

        self.drain_mut()?.write_all(&encoded)
    }

    fn write_empty_last_block(&mut self) -> Result<(), Error> {
        self.encode_block(Vec::new(), true)
    }
}

impl<W: Write, M: Matcher> Write for StreamingEncoder<W, M> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        self.ensure_open()?;
        if buf.is_empty() {
            return Ok(0);
        }

        self.ensure_frame_started()?;
        self.pending.extend_from_slice(buf);
        let block_capacity = self.block_capacity();
        while self.pending.len() >= block_capacity {
            let block: Vec<u8> = self.pending.drain(..block_capacity).collect();
            self.encode_block(block, false)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.ensure_open()?;
        self.ensure_frame_started()?;
        if !self.pending.is_empty() {
            let block = mem::take(&mut self.pending);
            self.encode_block(block, false)?;
        }
        self.drain_mut()?.flush()
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
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::decoding::StreamingDecoder;
    use crate::encoding::{CompressionLevel, Matcher, Sequence, StreamingEncoder};
    use crate::io::{Read, Write};

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
        let _ = encoder.finish().unwrap();
    }

    #[test]
    fn write_after_finish_returns_error() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"abc").unwrap();
        let _ = encoder.finish().unwrap();
        assert!(encoder.write_all(b"def").is_err());
        assert!(encoder.flush().is_err());
    }

    #[test]
    fn finish_without_writes_emits_empty_frame() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
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
        encoder.write_all(b"payload").unwrap();
        assert!(encoder.finish().is_err());
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
