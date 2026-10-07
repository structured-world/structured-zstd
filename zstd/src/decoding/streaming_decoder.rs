//! The [StreamingDecoder] wraps a [FrameDecoder] and provides a Read impl that decodes data when necessary

use core::borrow::BorrowMut;

use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use crate::decoding::{BlockDecodingStrategy, DictionaryHandle, FrameDecoder};
#[cfg(not(feature = "std"))]
use crate::io::ErrorKind;
use crate::io::{Error, Read};

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
    /// What a frame boundary has read so far, kept across a non-blocking
    /// source's `WouldBlock` so that retrying resumes rather than restarts.
    boundary: Boundary,
}

/// The longest frame header (RFC 8878 3.1.1.1): magic, descriptor, window
/// descriptor, a 4-byte dictionary id and an 8-byte content size.
const MAX_FRAME_HEADER: usize = 4 + 1 + 1 + 4 + 8;

/// A frame boundary in progress: the bytes of the next header read so far, and
/// the content of a skippable frame still to step over.
#[derive(Default)]
struct Boundary {
    header: [u8; MAX_FRAME_HEADER],
    have: usize,
    skip_left: u32,
}

impl Boundary {
    /// The length of the header whose first `self.have` bytes are in hand, or
    /// of as much of it as those bytes can tell: the magic decides a
    /// skippable frame's 8 bytes, a frame's descriptor decides the rest.
    fn header_len(&self) -> usize {
        if self.have < 4 {
            return 4;
        }
        let magic = u32::from_le_bytes([
            self.header[0],
            self.header[1],
            self.header[2],
            self.header[3],
        ]);
        if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            return 8;
        }
        if magic != crate::common::MAGIC_NUM {
            return 4;
        }
        if self.have < 5 {
            return 5;
        }
        let descriptor = crate::decoding::frame::FrameDescriptor(self.header[4]);
        let window = usize::from(!descriptor.single_segment_flag());
        let dict = descriptor.dictionary_id_bytes().map_or(0, usize::from);
        let fcs = descriptor.frame_content_size_bytes().map_or(0, usize::from);
        5 + window + dict + fcs
    }
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
            boundary: Boundary::default(),
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
            boundary: Boundary::default(),
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
            boundary: Boundary::default(),
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

    /// Start the frame that follows the finished one, skipping skippable
    /// frames, with the constructor's dictionary if it had one. `false` when
    /// the source ends cleanly at the frame boundary; anything else that is
    /// not a frame is an error.
    ///
    /// The source's own errors go back as it reported them, so a non-blocking
    /// source's `WouldBlock` stays one, and what the boundary had read is kept
    /// in [`Boundary`]: the header is parsed only once it is whole, so a retry
    /// resumes where the source stopped.
    fn start_next_frame(&mut self) -> Result<bool, Error> {
        let how = if self.forced_dictionary {
            FrameStart::HeldDictionary
        } else {
            FrameStart::Plain
        };
        loop {
            while self.boundary.skip_left > 0 {
                let mut scratch = [0u8; 512];
                // In `u32`, as the skippable length is: at most 512 after.
                let take = self.boundary.skip_left.min(scratch.len() as u32) as usize;
                match self.source.read(&mut scratch[..take]) {
                    Ok(0) => return Err(frame_error(FrameDecoderError::FailedToSkipFrame)),
                    // `n <= take <= skip_left`, so it fits the `u32` it is
                    // taken from.
                    Ok(n) => self.boundary.skip_left -= n as u32,
                    Err(e) if e.kind() == crate::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            loop {
                let want = self.boundary.header_len();
                if self.boundary.have >= want {
                    break;
                }
                let have = self.boundary.have;
                match self.source.read(&mut self.boundary.header[have..want]) {
                    Ok(0) if have == 0 => return Ok(false),
                    // The source ended inside a header: let the parser report
                    // it as it reports any header cut short.
                    Ok(0) => break,
                    Ok(n) => self.boundary.have += n,
                    Err(e) if e.kind() == crate::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            let have = core::mem::take(&mut self.boundary.have);
            let started = how.start(self.decoder.borrow_mut(), &self.boundary.header[..have]);
            match started {
                Ok(()) => return Ok(true),
                Err(e) => match skippable_frame_length(&e) {
                    Some(length) => self.boundary.skip_left = length,
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
    let mut scratch = [0u8; 512];
    while left > 0 {
        // The minimum is taken in `u32`, then fits `usize` as at most 512.
        let take = left.min(scratch.len() as u32) as usize;
        match source.read(&mut scratch[..take]) {
            Ok(0) => return Err(FrameDecoderError::FailedToSkipFrame),
            // `n <= take <= left`, so it fits the `u32` it is taken from.
            Ok(n) => left -= n as u32,
            Err(e) if e.kind() == crate::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(FrameDecoderError::FailedToSkipFrame),
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
        loop {
            let decoder = self.decoder.borrow_mut();
            if decoder.is_finished() && decoder.can_collect() == 0 {
                // Frame fully decoded and fully drained: its length and running
                // digest are final, so a frame shorter or longer than it
                // declared, or with a bad checksum in `Verify` mode, fails
                // here. Only reached with nothing written in this call, so the
                // error never follows delivered bytes, which the `Read`
                // contract forbids.
                verify_finished_frame(decoder)?;
                if !self.start_next_frame()? {
                    self.exhausted = true;
                    return Ok(0);
                }
                continue;
            }

            // Interleave bounded decode with draining so the decode window
            // (`RingBuffer`) stays near `window_size` instead of accumulating
            // the whole request before a single end-of-call drain.
            // `read_to_end` hands ever-larger buffers; decoding `buf.len()`
            // worth into the ring up front grew it far past the window
            // (repeated `reserve_amortized` alloc+copy). Decode at most one
            // block worth per step, then drain what is now collectable into
            // `buf`, mirroring upstream zstd's window-bounded flush loop.
            let mut written = 0;
            while written < buf.len() {
                // Drain whatever is collectable now (retaining `window_size`
                // until the frame finishes). Reclaims the ring promptly so the
                // next decode step reuses the same capacity.
                written += decoder.read(&mut buf[written..])?;
                if written == buf.len() || decoder.is_finished() {
                    break;
                }
                // Decode one bounded chunk. `UptoBytes` may overshoot a little
                // but is capped to one block, so the ring's live region stays
                // within `window_size + MAX_BLOCK_SIZE`.
                let step = (buf.len() - written).min(MAX_BLOCK_SIZE as usize);
                decoder
                    .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBytes(step))
                    .map_err(frame_error)?;
            }
            // Bytes in hand go back now: the next frame is only started on a
            // call that has nothing to deliver, so a reader is never held
            // waiting on a source that has not sent the next frame yet.
            if written > 0 {
                return Ok(written);
            }
            // Nothing written: the frame finished and drained empty, so loop
            // to the finish point above to check it and move on.
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
        // the decoder sits at the start of that frame with nothing decoded yet.
        let at_start = {
            let d = self.decoder.borrow_mut();
            d.is_at_frame_start() && d.can_collect() == 0
        };
        // A forced dictionary is the one the decoder already holds; following
        // frames are re-initialised with it in place, touching no reference
        // count.
        let keep_dictionary = self.forced_dictionary;
        if at_start {
            let mut compressed = alloc::vec::Vec::new();
            self.source.read_to_end(&mut compressed)?;
            self.decoder
                .borrow_mut()
                .decode_current_frame_to_vec(&compressed, output, keep_dictionary)
                .map_err(Error::other)?;
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
        let at_start = {
            let d = self.decoder.borrow_mut();
            d.is_at_frame_start() && d.can_collect() == 0
        };
        // As in the std path: the decoder's own dictionary, reused in place.
        let keep_dictionary = self.forced_dictionary;
        if at_start {
            let mut compressed = alloc::vec::Vec::new();
            self.source.read_to_end(&mut compressed)?;
            self.decoder
                .borrow_mut()
                .decode_current_frame_to_vec(&compressed, output, keep_dictionary)
                .map_err(|e| Error::new(ErrorKind::Other, alloc::boxed::Box::new(e)))?;
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
