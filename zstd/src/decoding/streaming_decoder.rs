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
        // (decode last block, then drain it into `buf`). Validate here too when
        // the frame is finished and nothing is left to collect, but ONLY when
        // this call wrote no bytes: the `Read` contract forbids returning `Err`
        // after bytes were delivered, so when `written > 0` the verify is
        // deferred to the next call, where the top early-return runs it and
        // returns `Err` on the zero-byte path. Idempotent with that top check.
        #[cfg(feature = "hash")]
        if written == 0
            && decoder.is_finished()
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
    /// skippable frames skipped). To recover bytes that follow a single frame,
    /// use `read` plus the
    /// [`SkipFrame`](crate::decoding::errors::ReadFrameHeaderError::SkipFrame)
    /// recreate-the-decoder pattern instead.
    #[cfg(feature = "std")]
    fn read_to_end(&mut self, output: &mut alloc::vec::Vec<u8>) -> Result<usize, Error> {
        // `new()` already read the frame header, so the fast path applies when
        // the decoder sits at the start of that frame with nothing decoded yet.
        let at_start = {
            let d = self.decoder.borrow_mut();
            d.is_at_frame_start() && d.can_collect() == 0
        };
        if at_start {
            let mut compressed = alloc::vec::Vec::new();
            self.source.read_to_end(&mut compressed)?;
            let written = self
                .decoder
                .borrow_mut()
                .decode_current_frame_to_vec(&compressed, output)
                .map_err(Error::other)?;
            return Ok(written);
        }
        // Mid-frame fallback: grow `output` and drain through the generic path.
        let mut total = 0;
        loop {
            let start = output.len();
            output.resize(start + MAX_BLOCK_SIZE as usize, 0);
            let n = self.read(&mut output[start..])?;
            output.truncate(start + n);
            if n == 0 {
                break;
            }
            total += n;
        }
        Ok(total)
    }

    /// no_std counterpart of the decode-in-place `read_to_end` fast path above
    /// (the no_std `Read::read_to_end` returns `()` instead of the byte count).
    #[cfg(not(feature = "std"))]
    fn read_to_end(&mut self, output: &mut alloc::vec::Vec<u8>) -> Result<(), Error> {
        let at_start = {
            let d = self.decoder.borrow_mut();
            d.is_at_frame_start() && d.can_collect() == 0
        };
        if at_start {
            let mut compressed = alloc::vec::Vec::new();
            self.source.read_to_end(&mut compressed)?;
            self.decoder
                .borrow_mut()
                .decode_current_frame_to_vec(&compressed, output)
                .map_err(|e| Error::new(ErrorKind::Other, alloc::boxed::Box::new(e)))?;
            return Ok(());
        }
        loop {
            let start = output.len();
            output.resize(start + MAX_BLOCK_SIZE as usize, 0);
            let n = self.read(&mut output[start..])?;
            output.truncate(start + n);
            if n == 0 {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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

        let dict_raw = include_bytes!("../../dict_tests/dictionary");
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
}
