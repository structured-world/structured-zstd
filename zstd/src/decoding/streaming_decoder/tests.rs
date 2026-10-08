use super::StreamingDecoder;
use crate::io::Read;

/// A frame with an 8-byte `Frame_Content_Size` of `fcs` followed by one raw
/// last block carrying `payload`. Multi-segment frames take a 1 KiB window
/// descriptor; single-segment ones size their window by the FCS.
fn frame_declaring(fcs: u64, single_segment: bool, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut f = alloc::vec![0x28, 0xB5, 0x2F, 0xFD];
    if single_segment {
        f.push(0xE0);
    } else {
        f.extend_from_slice(&[0xC0, 0x00]);
    }
    f.extend_from_slice(&fcs.to_le_bytes());
    let header = ((payload.len() as u32) << 3) | 1;
    f.extend_from_slice(&header.to_le_bytes()[..3]);
    f.extend_from_slice(payload);
    f
}

/// Drain `decoder` through plain `read` calls the way `io::copy` does, up to
/// the first `Ok(0)` or error.
fn drain_with_read(
    mut decoder: StreamingDecoder<&[u8], crate::decoding::FrameDecoder>,
) -> Result<alloc::vec::Vec<u8>, crate::io::Error> {
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// One compressed frame of `payload`.
fn frame_of(payload: &[u8]) -> alloc::vec::Vec<u8> {
    crate::encoding::compress_to_vec(payload, crate::encoding::CompressionLevel::Fastest)
}

/// A skippable frame (RFC 8878 3.1.2) carrying `payload`.
fn skippable_frame(payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut f = alloc::vec![0x50, 0x2A, 0x4D, 0x18];
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// RFC 8878 3: a stream is one or more frames. Plain `read`, the path
/// `io::copy` takes, must deliver every frame's content, not stop at the end
/// of the first one.
#[test]
fn read_continues_into_following_frames() {
    let mut stream = frame_of(b"first frame, ");
    stream.extend(frame_of(b"second frame, "));
    stream.extend(frame_of(b"third"));
    let decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    assert_eq!(
        drain_with_read(decoder).unwrap(),
        b"first frame, second frame, third"
    );
}

/// Skippable frames carry no content and are skipped wherever they stand,
/// first in the stream included; a skippable frame last ends the stream
/// cleanly.
#[test]
fn read_skips_skippable_frames_anywhere_in_the_stream() {
    let mut stream = skippable_frame(b"leading metadata");
    stream.extend(frame_of(b"one "));
    stream.extend(skippable_frame(b""));
    stream.extend(frame_of(b"two"));
    stream.extend(skippable_frame(b"trailer"));
    let decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    assert_eq!(drain_with_read(decoder).unwrap(), b"one two");
}

/// A stream of skippable frames alone holds no content: it decodes to
/// nothing, through `read` and through `read_to_end`, as upstream decodes it.
#[test]
fn a_stream_of_only_skippable_frames_decodes_to_nothing() {
    let mut stream = skippable_frame(b"first");
    stream.extend(skippable_frame(b"second"));
    let decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    assert!(drain_with_read(decoder).unwrap().is_empty());
    let mut decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    let mut out = alloc::vec::Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
}

/// Bytes after the last frame that do not form a frame are an error, not a
/// clean end of stream.
#[test]
fn read_rejects_bytes_after_the_last_frame_that_are_not_a_frame() {
    let mut stream = frame_of(b"payload");
    stream.extend_from_slice(b"not a frame");
    let decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    assert!(drain_with_read(decoder).is_err());
}

/// A decoder built with a dictionary applies it to every frame, not just the
/// first, as `read_to_end` already does.
#[test]
fn read_applies_the_constructor_dictionary_to_following_frames() {
    use crate::decoding::{Dictionary, DictionaryHandle};
    use crate::encoding::{CompressionLevel, FrameCompressor};
    let content = b"shared words the frames reuse again and again".to_vec();
    let handle = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(7, content.clone()).unwrap(),
    );
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    compressor
        .set_dictionary(Dictionary::from_raw_content(7, content).unwrap())
        .unwrap();
    let mut stream = compressor.compress_independent_frame(b"the frames reuse words");
    stream.extend(compressor.compress_independent_frame(b" and again the shared words"));
    let decoder = StreamingDecoder::new_with_dictionary_handle(stream.as_slice(), &handle).unwrap();
    assert_eq!(
        drain_with_read(decoder).unwrap(),
        b"the frames reuse words and again the shared words"
    );
}

/// A source that hands out its bytes and then reports that nothing more is
/// available yet, as an open pipe or socket would.
struct OpenPipe<'a> {
    data: &'a [u8],
}

impl Read for OpenPipe<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
        if self.data.is_empty() {
            return Err(crate::io::Error::from(crate::io::ErrorKind::WouldBlock));
        }
        self.data.read(buf)
    }
}

/// A non-blocking source that hands out one byte per call and reports
/// `WouldBlock` on every other call, so every frame header, skippable frame
/// and block arrives in pieces.
struct Trickle<'a> {
    data: &'a [u8],
    block_next: bool,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
        self.block_next = !self.block_next;
        if !self.block_next {
            return Err(crate::io::Error::from(crate::io::ErrorKind::WouldBlock));
        }
        let n = buf.len().min(1).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// A `WouldBlock` anywhere after the first frame's header, inside a block,
/// a block header, a checksum, a skippable frame or the next frame's header,
/// loses nothing: retrying the read picks up where the source stopped. The
/// constructor reads the first header synchronously, so that alone arrives
/// whole; every byte after it trickles in between `WouldBlock`s.
#[test]
fn read_resumes_wherever_a_would_block_interrupts_it() {
    let payload: alloc::vec::Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut compressor =
        crate::encoding::FrameCompressor::new(crate::encoding::CompressionLevel::Fastest);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut stream = alloc::vec::Vec::new();
    compressor.set_drain(&mut stream);
    compressor.compress();
    stream.extend(skippable_frame(b"metadata between frames"));
    stream.extend(frame_of(b"second"));
    let descriptor = crate::decoding::frame::FrameDescriptor(stream[4]);
    let first_header = 5
        + usize::from(!descriptor.single_segment_flag())
        + usize::from(descriptor.dictionary_id_bytes().unwrap())
        + usize::from(descriptor.frame_content_size_bytes().unwrap());

    struct Staged<'a> {
        head: &'a [u8],
        tail: Trickle<'a>,
    }
    impl Read for Staged<'_> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
            if self.head.is_empty() {
                self.tail.read(buf)
            } else {
                self.head.read(buf)
            }
        }
    }

    let staged = Staged {
        head: &stream[..first_header],
        tail: Trickle {
            data: &stream[first_header..],
            block_next: false,
        },
    };
    let mut decoder = StreamingDecoder::new(staged).unwrap();
    #[cfg(feature = "hash")]
    decoder
        .decoder_mut()
        .set_content_checksum(crate::decoding::ContentChecksum::Verify);
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == crate::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("the stream must survive WouldBlock: {e:?}"),
        }
    }
    let mut expected = payload;
    expected.extend_from_slice(b"second");
    assert!(out == expected, "decoded {} bytes", out.len());
}

/// Magicless frames carry no magic number, so the next frame's header is as
/// long as its descriptor says from its first byte. A ten-byte magicless
/// header (window descriptor and an eight-byte content size) handed over a
/// byte at a time between `WouldBlock`s must still start the frame.
#[test]
fn read_continues_into_following_magicless_frames() {
    let first = frame_declaring(5, false, b"first");
    let second = frame_declaring(6, false, b"second");
    let mut stream = first[4..].to_vec();
    let first_len = stream.len();
    stream.extend_from_slice(&second[4..]);

    struct Staged<'a> {
        head: &'a [u8],
        tail: Trickle<'a>,
    }
    impl Read for Staged<'_> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
            if self.head.is_empty() {
                self.tail.read(buf)
            } else {
                self.head.read(buf)
            }
        }
    }

    let mut frame_decoder = crate::decoding::FrameDecoder::new();
    frame_decoder.set_magicless(true);
    let staged = Staged {
        head: &stream[..first_len],
        tail: Trickle {
            data: &stream[first_len..],
            block_next: false,
        },
    };
    let mut decoder = StreamingDecoder::new_with_decoder(staged, frame_decoder).unwrap();
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 64];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == crate::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("magicless frames must decode in turn: {e:?}"),
        }
    }
    assert_eq!(out, b"firstsecond");
}

/// The reader takes from its source only what the next step of the frame
/// needs, as upstream's `ZSTD_decompressStream` loads only the next step's
/// input. Once a frame's content is delivered the source stands right after
/// that frame, so `into_inner` hands back the rest of the stream untouched.
#[test]
fn into_inner_after_a_frame_returns_the_rest_of_the_source() {
    let payload = b"one frame of content";
    let mut stream = frame_of(payload);
    stream.extend_from_slice(b"bytes after the frame");
    let mut decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    let mut out = [0u8; 20];
    decoder.read_exact(&mut out).unwrap();
    assert_eq!(&out, payload);
    assert_eq!(decoder.into_inner(), b"bytes after the frame");
}

/// A checksummed frame read into a buffer of exactly its content size takes
/// its four-byte checksum with its last block, so the source stands right
/// after the frame, not at the checksum.
#[test]
fn into_inner_after_a_checksummed_frame_returns_the_rest_of_the_source() {
    let payload = b"one frame of content";
    let mut compressor =
        crate::encoding::FrameCompressor::new(crate::encoding::CompressionLevel::Fastest);
    compressor.set_content_checksum(true);
    compressor.set_source(&payload[..]);
    let mut stream = alloc::vec::Vec::new();
    compressor.set_drain(&mut stream);
    compressor.compress();
    stream.extend_from_slice(b"bytes after the frame");
    let mut decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    let mut out = [0u8; 20];
    decoder.read_exact(&mut out).unwrap();
    assert_eq!(&out, payload);
    assert_eq!(decoder.into_inner(), b"bytes after the frame");
}

/// A small frame is read into a buffer the size of its block, not one sized
/// for the largest block any frame may hold.
#[test]
fn a_small_frame_buffers_no_more_than_its_block() {
    let payload: alloc::vec::Vec<u8> = (0..1000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let frame = frame_of(&payload);
    let mut decoder = StreamingDecoder::new(frame.as_slice()).unwrap();
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = decoder.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, payload);
    assert!(
        decoder.input.buf.len() <= frame.len(),
        "a {}-byte frame used a {}-byte input buffer",
        frame.len(),
        decoder.input.buf.len()
    );
}

/// A source that reports `Interrupted` before every read it serves, so each
/// read path's retry runs: the header reads, the skip of a skippable frame,
/// the boundary probe and the input fill.
struct Interrupting<'a> {
    data: &'a [u8],
    interrupt_next: bool,
}

impl Read for Interrupting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
        self.interrupt_next = !self.interrupt_next;
        if self.interrupt_next {
            return Err(crate::io::Error::from(crate::io::ErrorKind::Interrupted));
        }
        self.data.read(buf)
    }
}

/// `Interrupted` is not an error (the `Read` contract): every read path
/// retries it, whether the source is at a frame header, inside a skippable
/// frame, at a frame boundary or between blocks.
#[test]
fn read_retries_an_interrupted_source_everywhere() {
    let mut stream = skippable_frame(b"leading");
    stream.extend(frame_of(b"one "));
    stream.extend(skippable_frame(b"between"));
    stream.extend(frame_of(b"two"));
    let source = Interrupting {
        data: &stream,
        interrupt_next: false,
    };
    let mut decoder = StreamingDecoder::new(source).unwrap();
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = decoder.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, b"one two");
}

/// A skippable frame that ends before its declared length is a truncated
/// stream, wherever it stands: in front of the first frame, where the
/// constructor skips it, and after a frame, where `read` does.
#[test]
fn a_truncated_skippable_frame_is_an_error() {
    let mut leading = skippable_frame(b"metadata");
    leading.truncate(leading.len() - 3);
    let err = StreamingDecoder::new(leading.as_slice())
        .err()
        .expect("a leading skippable frame cut short must not start the stream");
    assert!(
        matches!(
            err,
            crate::decoding::errors::FrameDecoderError::FailedToSkipFrame
        ),
        "{err:?}"
    );

    let mut trailing = frame_of(b"payload");
    let mut skippable = skippable_frame(b"metadata");
    skippable.truncate(skippable.len() - 3);
    trailing.extend(skippable);
    let decoder = StreamingDecoder::new(trailing.as_slice()).unwrap();
    let err =
        drain_with_read(decoder).expect_err("a skippable frame cut short must not end cleanly");
    assert!(
        alloc::format!("{err:?}").contains("FailedToSkipFrame"),
        "{err:?}"
    );
}

/// A source that fails part-way through a leading skippable frame fails the
/// constructor with the skip error, and one that fails at the boundary after
/// it fails with the magic-number read error, not a clean empty stream.
#[test]
fn a_source_error_around_a_leading_skippable_frame_fails_the_constructor() {
    use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};

    /// Serves `data`, then fails every read with `Other`.
    struct FailsAfter<'a> {
        data: &'a [u8],
    }
    impl Read for FailsAfter<'_> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
            if self.data.is_empty() {
                return Err(crate::io::Error::from(crate::io::ErrorKind::Other));
            }
            self.data.read(buf)
        }
    }

    let skippable = skippable_frame(b"metadata");
    let inside = FailsAfter {
        data: &skippable[..skippable.len() - 3],
    };
    // The source's own error is kept: a failing device is not a skippable
    // frame cut short.
    let err = StreamingDecoder::new(inside)
        .err()
        .expect("a source error inside a skippable frame must fail");
    let shown = alloc::format!("{err:?}");
    assert!(
        shown.contains("FailedToReadSkippableFrame") && shown.contains("Other"),
        "{shown}"
    );

    let at_boundary = FailsAfter { data: &skippable };
    let err = StreamingDecoder::new(at_boundary)
        .err()
        .expect("a source error at the next frame boundary must fail");
    assert!(
        matches!(
            err,
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::MagicNumberReadError(_))
        ),
        "{err:?}"
    );
}

/// The source ending part-way through the next frame's header is a truncated
/// stream, not its clean end.
#[test]
fn read_rejects_a_stream_cut_inside_the_next_frame_header() {
    let mut stream = frame_of(b"payload");
    // Magic and descriptor only: a frame header is longer than five bytes,
    // since a single-segment frame always carries its content size.
    let next = frame_of(b"next");
    stream.extend_from_slice(&next[..5]);
    let decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    assert!(drain_with_read(decoder).is_err());
}

/// A block whose content does not decode fails the read that reaches it.
#[test]
fn read_rejects_a_block_that_does_not_decode() {
    // Single-segment frame of 16 bytes holding one last compressed block of
    // four bytes that are no valid literals section.
    let frame = [
        0x28, 0xB5, 0x2F, 0xFD, 0x20, 16, 0x25, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    let decoder = StreamingDecoder::new(&frame[..]).unwrap();
    assert!(drain_with_read(decoder).is_err());
}

/// A block header of the reserved type (RFC 8878 3.1.1.2.2) is invalid on its
/// own: the reader reports it from the header's three bytes instead of waiting
/// on the source for a body the block does not have.
#[test]
fn read_rejects_a_reserved_block_without_waiting_for_its_body() {
    // Single-segment frame of 16 bytes, then a last block of type 3 stating
    // 10 bytes of content, within the frame's block maximum, that never follow.
    let header = (10u32 << 3) | (3 << 1) | 1;
    let mut frame = alloc::vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, 16];
    frame.extend_from_slice(&header.to_le_bytes()[..3]);
    let mut decoder = StreamingDecoder::new(OpenPipe { data: &frame }).unwrap();
    let mut buf = [0u8; 64];
    let err = decoder
        .read(&mut buf)
        .expect_err("a reserved block type must fail");
    assert_ne!(err.kind(), crate::io::ErrorKind::WouldBlock, "{err:?}");
}

/// An RLE block's size field is its repeat count (RFC 8878 3.1.1.2.3): one
/// over the frame's block maximum is invalid from the header alone, so the
/// reader reports it without waiting for the block's one byte of content.
#[test]
fn read_rejects_an_oversized_rle_block_without_waiting_for_its_byte() {
    // Single-segment frame of 16 bytes, then a last RLE block repeating its
    // byte 100 times, past the frame's 16-byte block maximum.
    let header = (100u32 << 3) | (1 << 1) | 1;
    let mut frame = alloc::vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, 16];
    frame.extend_from_slice(&header.to_le_bytes()[..3]);
    let mut decoder = StreamingDecoder::new(OpenPipe { data: &frame }).unwrap();
    let mut buf = [0u8; 64];
    let err = decoder
        .read(&mut buf)
        .expect_err("an RLE block over the block maximum must fail");
    assert_ne!(err.kind(), crate::io::ErrorKind::WouldBlock, "{err:?}");
}

/// `read_to_end` gathers the rest of the stream in one buffer; it must not
/// stay allocated once the stream is decoded, or a decoder kept alive pins a
/// whole compressed archive.
#[cfg(feature = "std")]
#[test]
fn read_to_end_releases_the_compressed_stream() {
    let payload: alloc::vec::Vec<u8> = (0..50_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let stream = frame_of(&payload);
    let mut decoder = StreamingDecoder::new(stream.as_slice()).unwrap();
    let mut out = alloc::vec::Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert_eq!(out, payload);
    assert_eq!(decoder.input.buf.capacity(), 0);
}

/// A source that counts the reads it serves.
struct Counting<'a> {
    data: &'a [u8],
    reads: usize,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
        self.reads += 1;
        self.data.read(buf)
    }
}

/// A large skippable frame in front of the first frame is stepped over in
/// chunks of `SKIP_CHUNK`, as the ones after it are, not 512 bytes at a time.
#[test]
fn a_leading_skippable_frame_is_skipped_in_large_chunks() {
    let mut stream = skippable_frame(&alloc::vec![0u8; 64 * 1024]);
    stream.extend(frame_of(b"after the metadata"));
    let mut source = Counting {
        data: &stream,
        reads: 0,
    };
    StreamingDecoder::new(&mut source).unwrap();
    let chunks = 64 * 1024 / super::SKIP_CHUNK as usize;
    assert!(
        source.reads <= chunks + 8,
        "{} reads for a 64 KiB skippable frame",
        source.reads
    );
}

/// `len` bytes no compressor shrinks, so frames of them hold Raw blocks.
fn incompressible(len: usize) -> alloc::vec::Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

/// Raw blocks are read straight into the decode buffer, read by read: a
/// source that hands them over a byte at a time between `WouldBlock`s, with
/// the content checksum verified, still decodes every byte.
#[test]
fn read_resumes_raw_blocks_wherever_a_would_block_interrupts_them() {
    let payload = incompressible(300_000);
    let mut compressor =
        crate::encoding::FrameCompressor::new(crate::encoding::CompressionLevel::Fastest);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut stream = alloc::vec::Vec::new();
    compressor.set_drain(&mut stream);
    compressor.compress();
    assert!(
        stream.len() > payload.len(),
        "the frame must hold Raw blocks"
    );
    stream.extend(frame_of(b"next"));
    let descriptor = crate::decoding::frame::FrameDescriptor(stream[4]);
    let first_header = 5
        + usize::from(!descriptor.single_segment_flag())
        + usize::from(descriptor.dictionary_id_bytes().unwrap())
        + usize::from(descriptor.frame_content_size_bytes().unwrap());
    // Every byte after the first frame header arrives on its own, and every
    // other read would block.
    let stops: alloc::vec::Vec<usize> = (first_header..stream.len()).collect();
    let source = Arrivals {
        data: &stream,
        served: 0,
        stops: &stops,
    };
    let mut decoder = StreamingDecoder::new(source).unwrap();
    #[cfg(feature = "hash")]
    decoder
        .decoder_mut()
        .set_content_checksum(crate::decoding::ContentChecksum::Verify);
    let mut out = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == crate::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("Raw blocks must survive WouldBlock: {e:?}"),
        }
    }
    let mut expected = payload;
    expected.extend_from_slice(b"next");
    assert!(out == expected, "decoded {} bytes", out.len());
}

/// `read_to_end` after a `read` that stopped inside the first Raw block must
/// carry on from the decode buffer, not decode the frame again from the
/// source as if nothing had been read.
#[cfg(feature = "std")]
#[test]
fn read_to_end_after_a_part_read_raw_block_is_complete() {
    let payload = incompressible(20_000);
    let frame = frame_of(&payload);
    let descriptor = crate::decoding::frame::FrameDescriptor(frame[4]);
    let header = 5
        + usize::from(!descriptor.single_segment_flag())
        + usize::from(descriptor.dictionary_id_bytes().unwrap())
        + usize::from(descriptor.frame_content_size_bytes().unwrap());
    let stops = [header + 3 + 100];
    let source = Arrivals {
        data: &frame,
        served: 0,
        stops: &stops,
    };
    let mut decoder = StreamingDecoder::new(source).unwrap();
    let mut buf = [0u8; 64];
    // The Raw block's first 100 bytes are taken, then the source blocks.
    loop {
        match decoder.read(&mut buf) {
            Err(e) if e.kind() == crate::io::ErrorKind::WouldBlock => break,
            Ok(n) => assert_eq!(n, 0, "a frame's window holds its output until it ends"),
            Err(e) => panic!("{e:?}"),
        }
    }
    let mut out = alloc::vec::Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert!(out == payload, "decoded {} bytes", out.len());
}

/// A source that ends inside a Raw block is a truncated block body.
#[test]
fn read_rejects_a_frame_cut_inside_a_raw_block() {
    let payload = incompressible(20_000);
    let mut frame = frame_of(&payload);
    frame.truncate(frame.len() - 100);
    let decoder = StreamingDecoder::new(frame.as_slice()).unwrap();
    let err = drain_with_read(decoder).expect_err("a cut Raw block must fail");
    assert!(
        alloc::format!("{err:?}").contains("UnexpectedEof"),
        "{err:?}"
    );
}

/// Per-block checksums asked for on the decoder are taken for every block a
/// `read` decodes, compressed and Raw alike, in block order: the same digests
/// `decode_all` reports for the frame.
#[cfg(all(feature = "lsm", feature = "hash"))]
#[test]
fn read_records_per_block_checksums_for_every_block() {
    let mut payload = b"compressible line of text, again and again\n".repeat(8_000);
    payload.extend(incompressible(300_000));
    let frame = crate::encoding::compress_to_vec(
        payload.as_slice(),
        crate::encoding::CompressionLevel::Fastest,
    );

    let mut whole = crate::decoding::FrameDecoder::new();
    whole.enable_per_block_checksums();
    let mut out = alloc::vec![0u8; payload.len()];
    whole.decode_all(&frame, &mut out).unwrap();
    let expected = whole.computed_block_checksums().to_vec();
    assert!(expected.len() > 2, "the frame must hold several blocks");

    let mut frame_decoder = crate::decoding::FrameDecoder::new();
    frame_decoder.enable_per_block_checksums();
    let mut decoder = StreamingDecoder::new_with_decoder(frame.as_slice(), frame_decoder).unwrap();
    let mut decoded = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = decoder.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        decoded.extend_from_slice(&buf[..n]);
    }
    assert!(decoded == payload);
    assert_eq!(
        decoder.decoder.computed_block_checksums(),
        expected.as_slice()
    );
}

/// A source that serves `data` up to each of `stops` in turn, reporting
/// `WouldBlock` once at each, as a socket does between arrivals.
struct Arrivals<'a> {
    data: &'a [u8],
    served: usize,
    stops: &'a [usize],
}

impl Read for Arrivals<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
        let limit = match self.stops.first() {
            Some(&stop) if stop == self.served => {
                self.stops = &self.stops[1..];
                return Err(crate::io::Error::from(crate::io::ErrorKind::WouldBlock));
            }
            Some(&stop) => stop,
            None => self.data.len(),
        };
        let n = buf.len().min(limit - self.served);
        buf[..n].copy_from_slice(&self.data[self.served..self.served + n]);
        self.served += n;
        Ok(n)
    }
}

/// Input a `read` gathered before a `WouldBlock`, and whatever `read_to_end`
/// takes from the source before it stops again, both stay with the decoder: a
/// retried `read_to_end` decodes the whole frame.
#[cfg(feature = "std")]
#[test]
fn read_to_end_resumes_after_the_source_would_block() {
    let payload: alloc::vec::Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let frame = frame_of(&payload);
    let descriptor = crate::decoding::frame::FrameDescriptor(frame[4]);
    let header = 5
        + usize::from(!descriptor.single_segment_flag())
        + usize::from(descriptor.dictionary_id_bytes().unwrap())
        + usize::from(descriptor.frame_content_size_bytes().unwrap());
    let stops = [header + 10, header + 50];
    let source = Arrivals {
        data: &frame,
        served: 0,
        stops: &stops,
    };
    let mut decoder = StreamingDecoder::new(source).unwrap();
    let mut buf = [0u8; 64];
    let err = decoder
        .read(&mut buf)
        .expect_err("the first block is not all here");
    assert_eq!(err.kind(), crate::io::ErrorKind::WouldBlock);
    let mut out = alloc::vec::Vec::new();
    let err = decoder
        .read_to_end(&mut out)
        .expect_err("the source stops again");
    assert_eq!(err.kind(), crate::io::ErrorKind::WouldBlock);
    out.clear();
    decoder.read_to_end(&mut out).unwrap();
    assert!(out == payload, "decoded {} bytes", out.len());
}

/// `read_to_end` hands the source's own error back as it reported it.
#[cfg(feature = "std")]
#[test]
fn read_to_end_returns_the_source_error() {
    let frame = frame_of(b"complete frame");
    let mut decoder = StreamingDecoder::new(OpenPipe { data: &frame }).unwrap();
    let mut out = alloc::vec::Vec::new();
    let err = decoder
        .read_to_end(&mut out)
        .expect_err("a source that would block cannot be read to its end");
    assert_eq!(err.kind(), crate::io::ErrorKind::WouldBlock);
}

/// A frame cut inside its trailing checksum has every block decoded, and
/// still must not end as if it were whole.
#[test]
fn read_rejects_a_frame_cut_inside_its_checksum() {
    let mut compressor =
        crate::encoding::FrameCompressor::new(crate::encoding::CompressionLevel::Fastest);
    compressor.set_content_checksum(true);
    compressor.set_source(&b"checksummed payload"[..]);
    let mut frame = alloc::vec::Vec::new();
    compressor.set_drain(&mut frame);
    compressor.compress();
    frame.truncate(frame.len() - 2);
    let decoder = StreamingDecoder::new(frame.as_slice()).unwrap();
    // The cut is reported where it is, in the checksum, not as a block header
    // read out of the checksum's first bytes.
    let err = drain_with_read(decoder).expect_err("a frame missing checksum bytes must fail");
    assert!(
        alloc::format!("{err:?}").contains("FailedToReadChecksum"),
        "{err:?}"
    );
}

/// A dictionary constructor still rejects a source that is not a frame.
#[test]
fn a_dictionary_constructor_rejects_bytes_that_are_not_a_frame() {
    use crate::decoding::{Dictionary, DictionaryHandle};
    let handle = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(7, b"dictionary content".to_vec()).unwrap(),
    );
    assert!(StreamingDecoder::new_with_dictionary_handle(&b"not a frame"[..], &handle).is_err());
}

/// `read` stays a stream: a finished frame's bytes are delivered without
/// first waiting for the next frame's header from a source that is still
/// open.
#[test]
fn read_delivers_a_finished_frame_before_the_next_frame_arrives() {
    let frame = frame_of(b"complete frame");
    let mut decoder = StreamingDecoder::new(OpenPipe { data: &frame }).unwrap();
    let mut buf = [0u8; 64];
    let n = decoder.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"complete frame");
}

/// RFC 8878 3.1.1.1.4: when `Frame_Content_Size` is present the decompressed
/// data must be exactly that long. A frame that declares 64 MiB and produces
/// nothing must end the stream with an error, not a clean `Ok(0)` EOF that
/// lets `io::copy` report an empty file as decoded. Both window layouts.
#[test]
fn read_rejects_a_frame_shorter_than_its_declared_size() {
    for single_segment in [false, true] {
        let frame = frame_declaring(64 << 20, single_segment, &[]);
        let decoder = StreamingDecoder::new(frame.as_slice()).unwrap();
        let err = drain_with_read(decoder)
            .expect_err("a frame that produced less than it declared must not end cleanly");
        assert!(
            alloc::format!("{err:?}").contains("FrameContentSizeMismatch"),
            "single_segment={single_segment}: {err:?}"
        );
    }
}

/// `read_to_end` decodes a sized frame on the direct path, straight into the
/// caller's vector, outside the buffer whose counter the size check reads. A
/// `read` after it must still see the frame as complete and valid and return
/// a clean `Ok(0)`, not a size mismatch against a counter left at zero.
#[cfg(feature = "std")]
#[test]
fn read_after_a_direct_read_to_end_ends_cleanly() {
    use crate::encoding::{CompressionLevel, compress_to_vec};
    let payload: alloc::vec::Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let frame = compress_to_vec(payload.as_slice(), CompressionLevel::Fastest);
    let mut decoder = StreamingDecoder::new(frame.as_slice()).unwrap();
    let mut out = alloc::vec::Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert_eq!(out, payload);
    let mut buf = [0u8; 16];
    assert_eq!(decoder.read(&mut buf).unwrap(), 0);
}

/// The other direction: a frame that produces more than it declares. Its
/// bytes may be delivered, but the stream must not end as if it were valid.
#[test]
fn read_rejects_a_frame_longer_than_its_declared_size() {
    let frame = frame_declaring(1, false, b"abcd");
    let decoder = StreamingDecoder::new(frame.as_slice()).unwrap();
    let err = drain_with_read(decoder)
        .expect_err("a frame that produced more than it declared must not end cleanly");
    assert!(
        alloc::format!("{err:?}").contains("FrameContentSizeMismatch"),
        "{err:?}"
    );
}

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
