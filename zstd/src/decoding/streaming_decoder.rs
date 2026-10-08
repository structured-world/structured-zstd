//! The [StreamingDecoder] wraps a [FrameDecoder] and provides a Read impl that decodes data when necessary

use core::borrow::BorrowMut;

use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use crate::decoding::{BlockDecodingStrategy, DictionaryHandle, FrameDecoder};
use crate::io::{Error, ErrorKind, Read};

/// High level Zstandard frame decoder that can be used to decompress a given Zstandard frame.
///
/// This decoder implements `io::Read`, so you can interact with it by calling
/// `io::Read::read_to_end` / `io::Read::read_exact` or passing this to another library / module as a source for the decoded content
///
/// If you need more control over how decompression takes place, you can use
/// the lower level [FrameDecoder], which allows for greater control over how
/// decompression takes place but the implementor must call
/// [FrameDecoder::decode_blocks] repeatedly to decode the entire frame.
///
/// ## Multiple frames
/// A stream is one or more frames (RFC 8878 3). Every read path, plain `read`
/// and `io::copy` over it included, decodes them in turn, skips skippable
/// frames wherever they stand, and applies the constructor's dictionary to
/// each frame. The source ending at a frame boundary ends the stream; bytes
/// that do not form a frame are an error. `read` hands back a finished frame's
/// bytes before it reads the next frame's header, so a source that stays open
/// is not waited on while output is pending.
///
/// To decode a single frame and leave the bytes after it unread, drive a
/// [FrameDecoder] directly.
///
/// ```no_run
/// // `File` is std-only; `read_to_end` itself is available under no_std too.
/// #[cfg(feature = "std")]
/// {
///     use std::fs::File;
///     use std::io::Read;
///     use structured_zstd::decoding::StreamingDecoder;
///
///     // Read a Zstandard archive from the filesystem then decompress it into a vec.
///     let mut f: File = todo!("Read a .zstd archive from somewhere");
///     let mut decoder = StreamingDecoder::new(f).unwrap();
///     let mut result = Vec::new();
///     Read::read_to_end(&mut decoder, &mut result).unwrap();
/// }
/// ```
pub struct StreamingDecoder<READ: Read, DEC: BorrowMut<FrameDecoder>> {
    pub decoder: DEC,
    source: READ,
    /// Whether the decoder was constructed with a dictionary it applies to
    /// every frame. The `read_to_end` paths re-initialise FOLLOWING
    /// concatenated frames with it (a plain re-init resolves dictionaries by
    /// frame id only and would lose it for frames omitting the id); the
    /// decoder already holds its handle, so this one keeps none of its own.
    forced_dictionary: bool,
    /// The source has ended at a frame boundary (or held only skippable
    /// frames): every read from here on is the end of the stream, without
    /// asking the source again.
    exhausted: bool,
    /// Source bytes read ahead and not yet decoded.
    input: Input,
}

/// The most of a skippable frame's content read in one step.
const SKIP_CHUNK: u32 = 8 * 1024;

/// Source bytes of the decoder's next step, gathered the way upstream zstd's
/// `ZSTD_decompressStream` loads `nextSrcSizeToDecompress` bytes: exactly the
/// next frame header, block or checksum, and nothing past it. A step is decoded
/// only once it is all in hand, so a non-blocking source that returns
/// `WouldBlock` part-way through loses nothing, and the source never stands
/// beyond what the decoder has taken. A Raw block's content is the exception:
/// it is read straight into the decoder's buffer, see [`RawBlock`].
struct Input {
    /// Grown to the largest step seen; holds the pending bytes at
    /// `start..end`.
    buf: alloc::vec::Vec<u8>,
    start: usize,
    end: usize,
    /// Content of a skippable frame still to step over.
    skip_left: u32,
    /// The Raw block whose content is being read.
    raw: Option<RawBlock>,
}

/// A Raw block in progress. Its content is the output itself, so it is not
/// gathered whole: each read lands in a [`RAW_CHUNK`] of the input buffer,
/// still in cache, and goes on to the decode buffer at once.
struct RawBlock {
    size: u32,
    left: u32,
    last: bool,
    /// The running digest of the block, when per-block checksums are asked
    /// for: its content may be handed on before the block ends, so it cannot
    /// be hashed from the decode buffer at the end as other blocks are.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    digest: Option<twox_hash::XxHash64>,
}

/// The most of a Raw block read in one step. Every byte of the input buffer
/// is initialised once, when it first grows, so a source that hands content
/// over in small pieces costs a copy of each piece and nothing more.
const RAW_CHUNK: usize = 32 * 1024;

impl Input {
    const fn new() -> Self {
        Self {
            buf: alloc::vec::Vec::new(),
            start: 0,
            end: 0,
            skip_left: 0,
            raw: None,
        }
    }

    fn pending(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Read the rest of `source` behind the pending bytes. What it reads stays
    /// pending whatever it returns, so a source that stops part-way through
    /// is retried from where it stopped, with nothing lost.
    fn read_rest<R: Read>(&mut self, source: &mut R) -> Result<(), Error> {
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        self.buf.truncate(self.end);
        // Both `read_to_end`s keep what they appended when they fail.
        let result = source.read_to_end(&mut self.buf);
        self.end = self.buf.len();
        result.map(drop)
    }

    /// Read the next part of a Raw block, at most `left` bytes and at most
    /// [`RAW_CHUNK`], into the start of the buffer and return how many. Only
    /// called while nothing is pending. `Ok(0)` is the source's end;
    /// `Interrupted` is retried.
    fn read_raw<R: Read>(&mut self, source: &mut R, left: u32) -> Result<usize, Error> {
        debug_assert!(
            self.pending().is_empty(),
            "a Raw block is read with nothing pending"
        );
        // `RAW_CHUNK` fits a `u32`, so the minimum is taken in the wire's width.
        let chunk = left.min(RAW_CHUNK as u32) as usize;
        if self.buf.len() < chunk {
            self.buf.resize(chunk, 0);
        }
        loop {
            match source.read(&mut self.buf[..chunk]) {
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }

    /// Drop the input, buffer and all: after `read_rest` it holds a whole
    /// stream, which a decoder kept alive must not pin.
    fn release(&mut self) {
        self.buf = alloc::vec::Vec::new();
        self.start = 0;
        self.end = 0;
    }

    fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.end - self.start);
        self.start += n;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }

    /// Read more of `source` behind the pending bytes, up to `want` pending in
    /// all and no further. `Ok(0)` is its end; its errors come back as it
    /// reported them, `Interrupted` retried.
    fn fill<R: Read>(&mut self, source: &mut R, want: usize) -> Result<usize, Error> {
        // Pending bytes are an unfinished step, which reads stop at, so they
        // are rarely moved; when they are, they are less than one step.
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        debug_assert!(self.end < want, "a fill must ask for more than is pending");
        if self.buf.len() < want {
            // Doubling keeps a stream of growing blocks to a few allocations.
            let len = want.max(2 * self.buf.len());
            self.buf.resize(len, 0);
        }
        loop {
            match source.read(&mut self.buf[self.end..want]) {
                Ok(n) => {
                    self.end += n;
                    return Ok(n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// How much of a frame header `bytes` says there is to read: 4 for the magic,
/// 8 for a skippable frame's header, and for a frame its descriptor's fields.
/// A magic that is neither stops at 4, where the parser rejects it. A
/// magicless frame (`ZSTD_f_zstd1_magicless`) starts at its descriptor and
/// has no skippable form.
fn header_len(bytes: &[u8], magicless: bool) -> usize {
    let descriptor_at = if magicless {
        0
    } else {
        let Some(magic) = bytes.first_chunk::<4>() else {
            return 4;
        };
        let magic = u32::from_le_bytes(*magic);
        if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            return 8;
        }
        if magic != crate::common::MAGIC_NUM {
            return 4;
        }
        4
    };
    let Some(&descriptor) = bytes.get(descriptor_at) else {
        return descriptor_at + 1;
    };
    let descriptor = crate::decoding::frame::FrameDescriptor(descriptor);
    let window = usize::from(!descriptor.single_segment_flag());
    let dict = descriptor.dictionary_id_bytes().map_or(0, usize::from);
    let fcs = descriptor.frame_content_size_bytes().map_or(0, usize::from);
    descriptor_at + 1 + window + dict + fcs
}

impl<READ: Read, DEC: BorrowMut<FrameDecoder>> StreamingDecoder<READ, DEC> {
    pub fn new_with_decoder(
        mut source: READ,
        mut decoder: DEC,
    ) -> Result<StreamingDecoder<READ, DEC>, FrameDecoderError> {
        let started = start_first_frame(decoder.borrow_mut(), &mut source, FrameStart::Plain)?;
        Ok(StreamingDecoder {
            decoder,
            source,
            forced_dictionary: false,
            exhausted: !started,
            input: Input::new(),
        })
    }

    /// [`new_with_decoder`](Self::new_with_decoder) with `dict` applied to
    /// the frame even when its header omits the dictionary ID, as
    /// [`new_with_dictionary_handle`](StreamingDecoder::new_with_dictionary_handle)
    /// applies it (same warning). A decoder reused frame after frame keeps its
    /// buffers and, for the same dictionary, the handle it already holds, so
    /// after the first frame it allocates nothing and touches no reference
    /// count.
    ///
    /// # Examples
    /// ```
    /// use std::io::Read;
    /// use structured_zstd::decoding::{Dictionary, DictionaryHandle, FrameDecoder, StreamingDecoder};
    /// use structured_zstd::encoding::{FrameCompressor, CompressionLevel};
    ///
    /// let content = b"a dictionary of words the frames reuse".to_vec();
    /// let dictionary = DictionaryHandle::from_dictionary(
    ///     Dictionary::from_raw_content(1, content.clone()).unwrap(),
    /// );
    /// let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    /// compressor.set_dictionary(Dictionary::from_raw_content(1, content).unwrap()).unwrap();
    /// let frame = compressor.compress_independent_frame(b"the frames reuse words");
    ///
    /// let mut decoder = FrameDecoder::new();
    /// for _ in 0..2 {
    ///     let mut stream =
    ///         StreamingDecoder::new_with_decoder_and_dictionary_handle(&frame[..], &mut decoder, &dictionary)
    ///             .unwrap();
    ///     let mut decoded = Vec::new();
    ///     stream.read_to_end(&mut decoded).unwrap();
    ///     assert_eq!(decoded, b"the frames reuse words");
    /// }
    /// ```
    pub fn new_with_decoder_and_dictionary_handle(
        mut source: READ,
        mut decoder: DEC,
        dict: &DictionaryHandle,
    ) -> Result<StreamingDecoder<READ, DEC>, FrameDecoderError> {
        let started = start_first_frame(
            decoder.borrow_mut(),
            &mut source,
            FrameStart::Dictionary(dict),
        )?;
        Ok(StreamingDecoder {
            decoder,
            source,
            forced_dictionary: true,
            exhausted: !started,
            input: Input::new(),
        })
    }
}

impl<READ: Read> StreamingDecoder<READ, FrameDecoder> {
    pub fn new(
        mut source: READ,
    ) -> Result<StreamingDecoder<READ, FrameDecoder>, FrameDecoderError> {
        let mut decoder = FrameDecoder::new();
        let started = start_first_frame(&mut decoder, &mut source, FrameStart::Plain)?;
        Ok(StreamingDecoder {
            decoder,
            source,
            forced_dictionary: false,
            exhausted: !started,
            input: Input::new(),
        })
    }

    /// Create a streaming decoder using a pre-parsed dictionary handle.
    ///
    /// # Warning
    ///
    /// This constructor initializes the underlying [`FrameDecoder`] with
    /// `dict`, even if a frame header omits the optional dictionary ID.
    /// Callers must only use it when they already know the stream was encoded
    /// with this dictionary; otherwise decoded output can be silently
    /// corrupted.
    pub fn new_with_dictionary_handle(
        source: READ,
        dict: &DictionaryHandle,
    ) -> Result<StreamingDecoder<READ, FrameDecoder>, FrameDecoderError> {
        Self::new_with_decoder_and_dictionary_handle(source, FrameDecoder::new(), dict)
    }

    /// Create a streaming decoder using a serialized dictionary blob.
    ///
    /// # Warning
    ///
    /// This API forwards to [`StreamingDecoder::new_with_dictionary_handle`]
    /// and therefore applies the decoded dictionary to frames whose headers may
    /// omit the optional dictionary ID. Only use it when the stream is known to
    /// be encoded with that dictionary.
    pub fn new_with_dictionary_bytes(
        source: READ,
        raw_dictionary: &[u8],
    ) -> Result<StreamingDecoder<READ, FrameDecoder>, FrameDecoderError> {
        let dict = DictionaryHandle::decode_dict(raw_dictionary)?;
        Self::new_with_dictionary_handle(source, &dict)
    }
}

impl<READ: Read, DEC: BorrowMut<FrameDecoder>> StreamingDecoder<READ, DEC> {
    /// Gets a reference to the underlying reader.
    pub fn get_ref(&self) -> &READ {
        &self.source
    }

    /// Gets a mutable reference to the underlying reader.
    ///
    /// It is inadvisable to directly read from the underlying reader.
    pub fn get_mut(&mut self) -> &mut READ {
        &mut self.source
    }

    /// Gets a mutable reference to the frame decoder driving this stream.
    ///
    /// Exposed for settings that are read as decoding proceeds rather than at
    /// construction — [`FrameDecoder::set_content_checksum`] above all, which a
    /// caller that wants mismatches to fail (rather than merely be computed)
    /// has to reach after the constructor has chosen and initialised the
    /// decoder, including on the dictionary paths.
    pub fn decoder_mut(&mut self) -> &mut FrameDecoder {
        self.decoder.borrow_mut()
    }

    /// Destructures this object into the inner reader.
    ///
    /// The decoder reads only what its next step needs, so once a frame's
    /// content has been delivered the reader stands right after that frame.
    /// Bytes of a step a `WouldBlock` left unfinished stay with the decoder.
    pub fn into_inner(self) -> READ
    where
        READ: Sized,
    {
        self.source
    }

    /// Destructures this object into both the inner reader and [FrameDecoder].
    pub fn into_parts(self) -> (READ, DEC)
    where
        READ: Sized,
    {
        (self.source, self.decoder)
    }

    /// Destructures this object into the inner [FrameDecoder].
    pub fn into_frame_decoder(self) -> DEC {
        self.decoder
    }

    /// The error for a source that ended inside a frame. Cut inside the
    /// trailing checksum, that is what it reports; cut inside a block, the
    /// leftover input goes to the block decoder, which reports how it was cut
    /// short as it does for any truncated frame. Whole input never reaches
    /// here (it would have decoded), so that decode fails; were it to pass,
    /// the cut is still reported, as an early end of input.
    fn cut_short(&mut self) -> Error {
        let decoder = self.decoder.borrow_mut();
        if decoder.awaits_checksum() {
            return frame_error(FrameDecoderError::FailedToReadChecksum(Error::from(
                ErrorKind::UnexpectedEof,
            )));
        }
        let mut rest = self.input.pending();
        decoder
            .decode_blocks(&mut rest, BlockDecodingStrategy::All)
            .map_or_else(frame_error, |_| Error::from(ErrorKind::UnexpectedEof))
    }

    /// Start the frame that follows the finished one, skipping skippable
    /// frames, with the constructor's dictionary if it had one. `false` when
    /// the source ends cleanly at the frame boundary; anything else that is
    /// not a frame is an error.
    ///
    /// The source's own errors go back as it reported them, so a non-blocking
    /// source's `WouldBlock` stays one; what the boundary had read stays in
    /// [`Input`], and the header is parsed only once it is whole, so a retry
    /// resumes where the source stopped.
    fn start_next_frame(&mut self) -> Result<bool, Error> {
        let how = if self.forced_dictionary {
            FrameStart::HeldDictionary
        } else {
            FrameStart::Plain
        };
        let magicless = self.decoder.borrow_mut().is_magicless();
        loop {
            while self.input.skip_left > 0 {
                // Counted in the wire's `u32`; a step is at most `SKIP_CHUNK`,
                // which every `usize` holds.
                let step = self.input.skip_left.min(SKIP_CHUNK);
                if self.input.pending().is_empty()
                    && self.input.fill(&mut self.source, step as usize)? == 0
                {
                    return Err(frame_error(FrameDecoderError::FailedToSkipFrame));
                }
                // Reads stop at `step`, so what is pending is skippable content;
                // the minimum is taken in `u32`, the wire's width.
                let pending = u32::try_from(self.input.pending().len()).unwrap_or(u32::MAX);
                let take = self.input.skip_left.min(pending);
                self.input.consume(take as usize);
                self.input.skip_left -= take;
            }
            loop {
                let have = self.input.pending().len();
                let want = header_len(self.input.pending(), magicless);
                if have >= want {
                    break;
                }
                if self.input.fill(&mut self.source, want)? == 0 {
                    if have == 0 {
                        return Ok(false);
                    }
                    // The source ended inside a header: let the parser report
                    // it as it reports any header cut short.
                    break;
                }
            }
            let mut header = self.input.pending();
            let before = header.len();
            let started = how.start(self.decoder.borrow_mut(), &mut header);
            let used = before - header.len();
            self.input.consume(used);
            match started {
                Ok(()) => return Ok(true),
                Err(e) => match skippable_frame_length(&e) {
                    Some(length) => self.input.skip_left = length,
                    None => return Err(frame_error(e)),
                },
            }
        }
    }
}

/// `inner` with one byte already taken from it put back in front, so a frame
/// boundary can be probed for the end of the stream without losing the byte.
struct Prefixed<'a, R: Read> {
    first: Option<u8>,
    inner: &'a mut R,
}

impl<R: Read> Read for Prefixed<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        match (self.first.take(), buf.first_mut()) {
            (Some(byte), Some(slot)) => {
                *slot = byte;
                Ok(1)
            }
            (byte, _) => {
                self.first = byte;
                self.inner.read(buf)
            }
        }
    }
}

/// The length of the skippable frame (RFC 8878 3.1.2) whose header `e`
/// reports, if it is one.
fn skippable_frame_length(e: &FrameDecoderError) -> Option<u32> {
    match e {
        FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
            length, ..
        }) => Some(*length),
        _ => None,
    }
}

/// Read past a skippable frame's `length` content bytes.
fn skip_frame_content<R: Read>(source: &mut R, length: u32) -> Result<(), FrameDecoderError> {
    // Counted in the wire's `u32`: a `usize` is 16 bits on some targets, and a
    // truncated count would leave part of the content to be read as a header.
    let mut left = length;
    if left == 0 {
        return Ok(());
    }
    // Read in `SKIP_CHUNK`s, as skippable frames after the first frame are,
    // on the heap and only as large as the content: a stack array that size
    // would cost a no_std target 8 KiB of stack for every constructor call.
    let mut scratch = alloc::vec![0u8; left.min(SKIP_CHUNK) as usize];
    while left > 0 {
        // The minimum is taken in `u32`, then fits `usize` as at most
        // `SKIP_CHUNK`.
        let take = left.min(scratch.len() as u32) as usize;
        match source.read(&mut scratch[..take]) {
            Ok(0) => return Err(FrameDecoderError::FailedToSkipFrame),
            // `n <= take <= left`, so it fits the `u32` it is taken from.
            Ok(n) => left -= n as u32,
            Err(e) if e.kind() == crate::io::ErrorKind::Interrupted => {}
            // A source that fails is not a frame cut short: its error is kept.
            Err(e) => return Err(FrameDecoderError::FailedToReadSkippableFrame(e)),
        }
    }
    Ok(())
}

/// How a frame's decoder is initialised.
enum FrameStart<'d> {
    /// Dictionaries resolved by the frame's dictionary id.
    Plain,
    /// The supplied dictionary, applied whatever the frame header names.
    Dictionary(&'d DictionaryHandle),
    /// The dictionary the decoder already holds, applied the same way.
    HeldDictionary,
}

impl FrameStart<'_> {
    fn start(
        &self,
        decoder: &mut FrameDecoder,
        source: impl Read,
    ) -> Result<(), FrameDecoderError> {
        match self {
            Self::Plain => decoder.init(source),
            Self::Dictionary(dict) => decoder.init_with_dict_handle(source, dict),
            Self::HeldDictionary => decoder.reset_with_active_dict(source),
        }
    }
}

/// Start the first content frame of `source`, skipping skippable frames in
/// front of it. `false` when the source held only skippable frames, which
/// decode to nothing; an empty source is an error from the header read.
fn start_first_frame<R: Read>(
    decoder: &mut FrameDecoder,
    source: &mut R,
    how: FrameStart<'_>,
) -> Result<bool, FrameDecoderError> {
    let mut started = how.start(decoder, &mut *source);
    loop {
        let length = match started {
            Ok(()) => return Ok(true),
            Err(e) => skippable_frame_length(&e).ok_or(e)?,
        };
        skip_frame_content(source, length)?;
        let mut probe = [0u8; 1];
        let n = read_at_boundary(source, &mut probe).map_err(|e| {
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::MagicNumberReadError(e))
        })?;
        if n == 0 {
            return Ok(false);
        }
        started = how.start(
            decoder,
            Prefixed {
                first: Some(probe[0]),
                inner: source,
            },
        );
    }
}

/// One byte from `source` at a frame boundary, retried on interruption; `0`
/// is the clean end of the stream.
fn read_at_boundary<R: Read>(source: &mut R, probe: &mut [u8; 1]) -> Result<usize, Error> {
    loop {
        match source.read(probe) {
            Err(e) if e.kind() == crate::io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

/// Close a Raw block whose content is all in the decode buffer, recording its
/// digest when per-block checksums are on.
fn finish_raw_block(decoder: &mut FrameDecoder, raw: RawBlock) {
    #[cfg(all(feature = "lsm", feature = "hash"))]
    if let Some(digest) = raw.digest {
        decoder.record_block_checksum(core::hash::Hasher::finish(&digest) as u32);
    }
    decoder.finish_raw_block(raw.size, raw.last);
}

/// A frame decode error as the `Read` error it surfaces through.
fn frame_error(e: FrameDecoderError) -> Error {
    #[cfg(feature = "std")]
    return Error::other(e);
    #[cfg(not(feature = "std"))]
    return Error::new(ErrorKind::Other, alloc::boxed::Box::new(e));
}

/// The checks a frame can only pass once it is fully decoded and drained: the
/// declared content size, then the content checksum in `Verify` mode.
fn verify_finished_frame(decoder: &FrameDecoder) -> Result<(), Error> {
    decoder.verify_content_size().map_err(frame_error)?;
    #[cfg(feature = "hash")]
    decoder.verify_content_checksum().map_err(frame_error)?;
    Ok(())
}

impl<READ: Read, DEC: BorrowMut<FrameDecoder>> Read for StreamingDecoder<READ, DEC> {
    /// Decode the stream into `buf`, frame after frame (RFC 8878 3: a stream
    /// is one or more frames), as upstream zstd's `ZSTD_decompressStream`
    /// does. Skippable frames are skipped; the source ending at a frame
    /// boundary is the end of the stream, anything else is an error.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if buf.is_empty() || self.exhausted {
            return Ok(0);
        }
        let mut written = 0;
        loop {
            let decoder = self.decoder.borrow_mut();
            if decoder.is_finished() && decoder.can_collect() == 0 {
                // Bytes in hand go back first: the frame's checks and the next
                // frame's header wait for a call that has nothing to deliver,
                // so an error never follows delivered bytes (the `Read`
                // contract) and a source that has not sent the next frame
                // yet is not waited on.
                if written > 0 {
                    return Ok(written);
                }
                // Fully decoded and drained: the frame's length and running
                // digest are final, so a frame shorter or longer than it
                // declared, or with a bad checksum in `Verify` mode, fails here.
                verify_finished_frame(decoder)?;
                if !self.start_next_frame()? {
                    self.exhausted = true;
                    return Ok(0);
                }
                continue;
            }
            // A full `buf` never strands a checksum: a frame's last window of
            // output is collectable only once the frame is finished, its
            // checksum read, so the source already stands past the frame.
            if written == buf.len() {
                return Ok(written);
            }
            // A Raw block in progress reads on into the decode buffer once
            // the output already there has been handed over.
            if self.input.raw.is_some() && decoder.can_collect() == 0 {
                // Return what this call has rather than wait on the source.
                if written > 0 {
                    return Ok(written);
                }
                // The block stays recorded until the read has delivered: a
                // `WouldBlock` returns here with the block still open.
                let left = self.input.raw.as_ref().map_or(0, |raw| raw.left);
                let n = self.input.read_raw(&mut self.source, left)?;
                let mut raw = self.input.raw.take().expect("checked just above");
                if n == 0 {
                    return Err(frame_error(decoder.raw_block_cut_short(raw.size, raw.last)));
                }
                let content = &self.input.buf[..n];
                decoder.raw_block_push(content);
                #[cfg(all(feature = "lsm", feature = "hash"))]
                if let Some(digest) = raw.digest.as_mut() {
                    core::hash::Hasher::write(digest, content);
                }
                // `n <= left`, so it fits the `u32` it is taken from.
                raw.left -= n as u32;
                if raw.left == 0 {
                    finish_raw_block(decoder, raw);
                } else {
                    self.input.raw = Some(raw);
                }
                continue;
            }
            // Otherwise the decoder is entered only with its next step whole,
            // or with output to hand over: entering it with part of a block
            // only parses the block header to find the block incomplete.
            let want = decoder.input_needed(self.input.pending());
            if self.input.raw.is_none()
                && want > self.input.pending().len()
                && decoder.can_collect() == 0
            {
                // Return what this call has rather than wait on the source.
                if written > 0 {
                    return Ok(written);
                }
                // A Raw block's header alone is in hand: its content goes
                // straight to the decode buffer from here on.
                if let Some(&header) = self.input.pending().first_chunk::<3>()
                    && self.input.pending().len() == 3
                    && let Some((size, last)) = decoder
                        .start_raw_block(&header, true)
                        .map_err(frame_error)?
                {
                    self.input.consume(3);
                    let raw = RawBlock {
                        size,
                        left: size,
                        last,
                        #[cfg(all(feature = "lsm", feature = "hash"))]
                        digest: decoder
                            .per_block_checksums_enabled()
                            .then(|| twox_hash::XxHash64::with_seed(0)),
                    };
                    if size == 0 {
                        finish_raw_block(decoder, raw);
                    } else {
                        self.input.raw = Some(raw);
                    }
                    continue;
                }
                if self.input.fill(&mut self.source, want)? == 0 {
                    return Err(self.cut_short());
                }
                continue;
            }
            // Decode the step, handing output to `buf` block by block: the
            // decode window stays one window plus one block, as upstream's
            // flush loop keeps it. The buffer takes the whole window at the
            // first block whatever the frame declares, as upstream sizes a
            // stream's buffer from the window.
            let (consumed, produced) = decoder
                .decode_available(self.input.pending(), &mut buf[written..], true)
                .map_err(frame_error)?;
            self.input.consume(consumed);
            written += produced;
            if consumed > 0 || produced > 0 {
                continue;
            }
            if written > 0 {
                return Ok(written);
            }
            // A step whose input is all here yet decodes nothing (a block over
            // the frame's maximum, say) is a damaged frame: report it.
            return Err(self.cut_short());
        }
    }

    /// Decode-in-place fast path for whole-frame consumption. Instead of the
    /// generic `read` loop (decode block -> `RingBuffer` -> copy into the
    /// caller buffer), buffer the (compressed, hence small) source and decode
    /// STRAIGHT into `output`'s spare capacity via the single-copy direct path,
    /// pre-sized from the frame's declared content size. Only taken when the
    /// decoder is at a frame boundary (nothing partially decoded / undrained);
    /// otherwise it falls back to the generic grow-and-`read` loop so a caller
    /// that mixed `read` with `read_to_end` still gets correct output.
    ///
    /// Per the `Read::read_to_end` contract this consumes the source to EOF: if
    /// the stream holds several concatenated frames they are ALL decoded (and
    /// skippable frames skipped), as with `read`.
    #[cfg(feature = "std")]
    fn read_to_end(&mut self, output: &mut alloc::vec::Vec<u8>) -> Result<usize, Error> {
        let start_total = output.len();
        if self.exhausted {
            return Ok(0);
        }
        // `new()` already read the frame header, so the fast path applies when
        // the decoder sits at the start of that frame with nothing decoded yet;
        // a first Raw block part-read into the decode buffer is not that.
        let at_start = self.input.raw.is_none() && {
            let d = self.decoder.borrow_mut();
            d.is_at_frame_start() && d.can_collect() == 0
        };
        // A forced dictionary is the one the decoder already holds; following
        // frames are re-initialised with it in place, touching no reference
        // count.
        let keep_dictionary = self.forced_dictionary;
        if at_start {
            self.input.read_rest(&mut self.source)?;
            let decoded = self.decoder.borrow_mut().decode_current_frame_to_vec(
                self.input.pending(),
                output,
                keep_dictionary,
            );
            self.input.release();
            decoded.map_err(Error::other)?;
            return Ok(output.len() - start_total);
        }
        // Mid-frame fallback: drain through the generic path, which carries on
        // into the following frames, so the source is consumed to true EOF.
        loop {
            let start = output.len();
            output.resize(start + MAX_BLOCK_SIZE as usize, 0);
            // On error, drop the just-grown (zeroed) tail before propagating so
            // the caller never observes bytes that were never decoded.
            let n = match self.read(&mut output[start..]) {
                Ok(n) => n,
                Err(e) => {
                    output.truncate(start);
                    return Err(e);
                }
            };
            output.truncate(start + n);
            if n == 0 {
                break;
            }
        }
        Ok(output.len() - start_total)
    }

    /// no_std counterpart of the decode-in-place `read_to_end` fast path above
    /// (the no_std `Read::read_to_end` returns `()` instead of the byte count).
    #[cfg(not(feature = "std"))]
    fn read_to_end(&mut self, output: &mut alloc::vec::Vec<u8>) -> Result<(), Error> {
        if self.exhausted {
            return Ok(());
        }
        let at_start = self.input.raw.is_none() && {
            let d = self.decoder.borrow_mut();
            d.is_at_frame_start() && d.can_collect() == 0
        };
        // As in the std path: the decoder's own dictionary, reused in place.
        let keep_dictionary = self.forced_dictionary;
        if at_start {
            self.input.read_rest(&mut self.source)?;
            let decoded = self.decoder.borrow_mut().decode_current_frame_to_vec(
                self.input.pending(),
                output,
                keep_dictionary,
            );
            self.input.release();
            decoded.map_err(|e| Error::new(ErrorKind::Other, alloc::boxed::Box::new(e)))?;
            return Ok(());
        }
        // Mid-frame fallback: drain through the generic path, which carries on
        // into the following frames, so the source is consumed to true EOF.
        loop {
            let start = output.len();
            output.resize(start + MAX_BLOCK_SIZE as usize, 0);
            // On error, drop the just-grown (zeroed) tail before propagating so
            // the caller never observes bytes that were never decoded.
            let n = match self.read(&mut output[start..]) {
                Ok(n) => n,
                Err(e) => {
                    output.truncate(start);
                    return Err(e);
                }
            };
            output.truncate(start + n);
            if n == 0 {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
