//! The [StreamingDecoder] wraps a [FrameDecoder] and provides a Read impl that decodes data when necessary

use core::borrow::BorrowMut;

use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::FrameDecoderError;
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
/// ## Caveat
/// [StreamingDecoder] expects the underlying stream to only contain a single frame,
/// yet the specification states that a single archive may contain multiple frames.
///
/// To decode all the frames in a finite stream, the calling code needs to recreate
/// the instance of the decoder and handle
/// [crate::decoding::errors::ReadFrameHeaderError::SkipFrame]
/// errors by skipping forward the `length` amount of bytes, see <https://github.com/KillingSpark/zstd-rs/issues/57>
///
/// ```no_run
/// // `read_to_end` is not implemented by the no_std implementation.
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
}

impl<READ: Read, DEC: BorrowMut<FrameDecoder>> StreamingDecoder<READ, DEC> {
    pub fn new_with_decoder(
        mut source: READ,
        mut decoder: DEC,
    ) -> Result<StreamingDecoder<READ, DEC>, FrameDecoderError> {
        decoder.borrow_mut().init(&mut source)?;
        Ok(StreamingDecoder { decoder, source })
    }
}

impl<READ: Read> StreamingDecoder<READ, FrameDecoder> {
    pub fn new(
        mut source: READ,
    ) -> Result<StreamingDecoder<READ, FrameDecoder>, FrameDecoderError> {
        let mut decoder = FrameDecoder::new();
        decoder.init(&mut source)?;
        Ok(StreamingDecoder { decoder, source })
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
        mut source: READ,
        dict: &DictionaryHandle,
    ) -> Result<StreamingDecoder<READ, FrameDecoder>, FrameDecoderError> {
        let mut decoder = FrameDecoder::new();
        decoder.init_with_dict_handle(&mut source, dict)?;
        Ok(StreamingDecoder { decoder, source })
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
}

impl<READ: Read, DEC: BorrowMut<FrameDecoder>> Read for StreamingDecoder<READ, DEC> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let decoder = self.decoder.borrow_mut();
        if decoder.is_finished() && decoder.can_collect() == 0 {
            // Frame fully decoded and fully drained: the running XXH64 digest
            // is final, so a `Verify`-mode decoder validates the content
            // checksum at this finish point. No-op in other modes.
            #[cfg(feature = "hash")]
            if let Err(e) = decoder.verify_content_checksum() {
                #[cfg(feature = "std")]
                return Err(Error::other(e));
                #[cfg(not(feature = "std"))]
                return Err(Error::new(ErrorKind::Other, alloc::boxed::Box::new(e)));
            }
            //No more bytes can ever be decoded
            return Ok(0);
        }

        // Interleave bounded decode with draining so the decode window
        // (`RingBuffer`) stays near `window_size` instead of accumulating the
        // whole request before a single end-of-call drain. `read_to_end` hands
        // ever-larger buffers; decoding `buf.len()` worth into the ring up
        // front grew it far past the window (repeated `reserve_amortized`
        // alloc+copy). Decode at most one block worth per step, then drain
        // what is now collectable into `buf`, mirroring the donor's
        // window-bounded flush loop.
        let mut written = 0;
        while written < buf.len() {
            // Drain whatever is collectable now (retaining `window_size` until
            // the frame finishes). Reclaims the ring promptly so the next
            // decode step reuses the same capacity.
            written += decoder.read(&mut buf[written..])?;
            if written == buf.len() || decoder.is_finished() {
                break;
            }
            // Decode one bounded chunk. `UptoBytes` may overshoot a little but
            // is capped to one block, so the ring's live region stays within
            // `window_size + MAX_BLOCK_SIZE`.
            let step = (buf.len() - written).min(MAX_BLOCK_SIZE as usize);
            if let Err(e) =
                decoder.decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBytes(step))
            {
                #[cfg(feature = "std")]
                {
                    return Err(Error::other(e));
                }
                #[cfg(not(feature = "std"))]
                {
                    return Err(Error::new(ErrorKind::Other, alloc::boxed::Box::new(e)));
                }
            }
        }

        // The loop can finish AND fully drain a frame within this same call
        // (decode last block, then drain it into `buf`). A caller that reads
        // exactly the frame's length and never calls `read` again would then
        // never hit the top early-return's verify, so validate here too when
        // the frame is finished and nothing is left to collect. Idempotent with
        // the top check for callers that do read again.
        #[cfg(feature = "hash")]
        if decoder.is_finished()
            && decoder.can_collect() == 0
            && let Err(e) = decoder.verify_content_checksum()
        {
            #[cfg(feature = "std")]
            return Err(Error::other(e));
            #[cfg(not(feature = "std"))]
            return Err(Error::new(ErrorKind::Other, alloc::boxed::Box::new(e)));
        }

        Ok(written)
    }
}
