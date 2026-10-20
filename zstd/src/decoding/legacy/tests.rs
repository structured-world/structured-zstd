//! Frames written by the v0.5, v0.6 and v0.7 release command lines
//! (`scripts/legacy-fixtures.sh`) decode to the files they were made from.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

#[cfg(feature = "std")]
mod fixtures {
    use super::*;
    use alloc::format;
    use alloc::string::{String, ToString};
    use std::path::{Path, PathBuf};

    pub(super) struct Fixture {
        pub(super) name: String,
        pub(super) frame: Vec<u8>,
        pub(super) content: Vec<u8>,
        pub(super) dict: Option<Vec<u8>>,
        /// `dict` as the dictionary parser reads it.
        pub(super) parsed: Option<crate::decoding::Dictionary>,
        pub(super) version: LegacyVersion,
    }

    impl Fixture {
        /// The fixture's dictionary as the decoders take it.
        pub(super) fn legacy_dict(&self) -> LegacyDict<'_> {
            self.parsed
                .as_ref()
                .map_or(LegacyDict::None, LegacyDict::Parsed)
        }
    }

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// Every fixture, with the file it decodes to and the dictionary it needs.
    pub(super) fn all() -> Vec<Fixture> {
        let dir = root().join("legacy_frames");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .expect("legacy_frames directory")
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".zst"))
            .collect();
        names.sort();
        assert!(names.len() >= 50, "fixtures missing from {}", dir.display());
        names
            .into_iter()
            .map(|name| {
                let (stem, rest) = ["v05", "v06", "v07"]
                    .iter()
                    .find_map(|tag| {
                        name.split_once(&format!(".{tag}."))
                            .map(|split| (split, *tag))
                    })
                    .map(|((stem, _), tag)| (stem.to_string(), tag))
                    .expect("fixture name carries its version");
                let version = match rest {
                    "v05" => LegacyVersion::V05,
                    "v06" => LegacyVersion::V06,
                    _ => LegacyVersion::V07,
                };
                let is_dict = name.contains(".dict");
                // `<file>x<N>` is the file N times over.
                let (file, times) = stem
                    .rsplit_once('x')
                    .and_then(|(file, n)| Some((file, n.parse::<usize>().ok()?)))
                    .unwrap_or((stem.as_str(), 1));
                let source: PathBuf = if is_dict {
                    root().join("dict_tests/files").join(file)
                } else {
                    root().join("decodecorpus_files").join(file)
                };
                let dict =
                    is_dict.then(|| std::fs::read(dir.join(format!("dictionary.{rest}"))).unwrap());
                Fixture {
                    frame: std::fs::read(dir.join(&name)).unwrap(),
                    content: std::fs::read(&source).unwrap().repeat(times),
                    parsed: dict.as_deref().map(|raw| {
                        crate::decoding::Dictionary::from_serialized_or_raw_content(raw).unwrap()
                    }),
                    dict,
                    version,
                    name,
                }
            })
            .collect()
    }
}

#[cfg(feature = "std")]
use fixtures::all as fixtures;

fn decode_to_vec(frame: &[u8], dict: LegacyDict<'_>) -> Result<(Vec<u8>, usize), Error> {
    decode_to_vec_in(
        frame,
        dict,
        FrameOptions::default(),
        &mut Workspace::default(),
    )
}

/// [`decode_to_vec`] refusing a frame whose content checksum does not match.
fn decode_verified(frame: &[u8], dict: LegacyDict<'_>) -> Result<(Vec<u8>, usize), Error> {
    let options = FrameOptions {
        #[cfg(feature = "hash")]
        checksum: crate::decoding::ContentChecksum::Verify,
        ..FrameOptions::default()
    };
    decode_to_vec_in(frame, dict, options, &mut Workspace::default())
}

/// [`decode_to_vec`] with `options`, in `workspace`, the storage an earlier
/// frame left.
fn decode_to_vec_in(
    frame: &[u8],
    dict: LegacyDict<'_>,
    options: FrameOptions,
    workspace: &mut Workspace,
) -> Result<(Vec<u8>, usize), Error> {
    let mut buf = Vec::new();
    let mut out = VecOutput {
        buf: &mut buf,
        start: 0,
    };
    let (consumed, _) = decode_frame(frame, dict, options, workspace, &mut out)?;
    Ok((buf, consumed))
}

/// Each fixture names its own format and decodes byte-exact, taking exactly
/// its own bytes: in a fresh decoder, and in one every fixture before it
/// left its tables in, whatever its format and dictionary.
#[cfg(feature = "std")]
#[test]
fn fixtures_decode_to_their_content() {
    let mut reused = Workspace::default();
    for fixture in fixtures() {
        assert_eq!(
            LegacyVersion::of(&fixture.frame),
            Some(fixture.version),
            "{}",
            fixture.name
        );
        for workspace in [&mut Workspace::default(), &mut reused] {
            let (decoded, consumed) = decode_to_vec_in(
                &fixture.frame,
                fixture.legacy_dict(),
                FrameOptions::default(),
                workspace,
            )
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
            assert_eq!(consumed, fixture.frame.len(), "{}", fixture.name);
            assert!(
                decoded == fixture.content,
                "{} decodes to other bytes",
                fixture.name
            );
        }
    }
}

/// Decoding into a caller's slice gives the same bytes, and a slice one byte
/// short is refused as too small rather than written past.
#[cfg(feature = "std")]
#[test]
fn fixtures_decode_into_a_slice() {
    for fixture in fixtures() {
        let dict = fixture.legacy_dict();
        let mut buf = vec![0u8; fixture.content.len()];
        let mut out = SliceOutput {
            buf: &mut buf,
            pos: 0,
        };
        decode_frame(
            &fixture.frame,
            dict,
            FrameOptions::default(),
            &mut Workspace::default(),
            &mut out,
        )
        .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
        assert_eq!(out.pos, fixture.content.len(), "{}", fixture.name);
        assert!(buf == fixture.content, "{}", fixture.name);

        if let Some(short) = fixture.content.len().checked_sub(1) {
            let mut buf = vec![0u8; short];
            let mut out = SliceOutput {
                buf: &mut buf,
                pos: 0,
            };
            assert!(
                matches!(
                    decode_frame(
                        &fixture.frame,
                        dict,
                        FrameOptions::default(),
                        &mut Workspace::default(),
                        &mut out
                    ),
                    Err(Error::OutputTooSmall)
                ),
                "{}",
                fixture.name
            );
        }
    }
}

/// The size walk ends where the decoder does, and bounds the content by
/// whole blocks.
#[cfg(feature = "std")]
#[test]
fn frame_size_matches_the_decoded_frame() {
    for fixture in fixtures() {
        let (size, bound) = frame_size(fixture.version, &fixture.frame).unwrap();
        assert_eq!(size, fixture.frame.len(), "{}", fixture.name);
        assert!(bound >= fixture.content.len() as u64, "{}", fixture.name);
        assert_eq!(bound % BLOCK_SIZE_MAX as u64, 0, "{}", fixture.name);
    }
}

/// A v0.7 frame naming a dictionary by ID is refused without it, and with a
/// dictionary of another ID, saying which it needs. (v0.5 and v0.6 frames
/// carry no ID; one whose blocks never reach into the dictionary decodes
/// without it.)
#[cfg(feature = "std")]
#[test]
fn a_v07_frame_naming_a_dictionary_needs_it() {
    for fixture in fixtures()
        .into_iter()
        .filter(|f| f.name.ends_with(".v07.dict.zst"))
    {
        assert_eq!(
            decode_to_vec(&fixture.frame, LegacyDict::None).unwrap_err(),
            Error::DictionaryMismatch {
                expected: 7,
                provided: 0
            },
            "{}",
            fixture.name
        );
        let mut other = fixture.dict.clone().unwrap();
        other[4] ^= 1;
        let other = crate::decoding::Dictionary::decode_dict(&other).unwrap();
        assert_eq!(
            decode_to_vec(&fixture.frame, LegacyDict::Parsed(&other)).unwrap_err(),
            Error::DictionaryMismatch {
                expected: 7,
                provided: 6
            },
            "{}",
            fixture.name
        );
    }
}

/// A v0.7 dictionary read by the RFC 8878 parser primes the decoder as its
/// raw bytes do, and a raw-content one gives its content and its ID.
#[cfg(feature = "std")]
#[test]
fn a_parsed_dictionary_decodes_like_its_bytes() {
    for fixture in fixtures().into_iter().filter(|f| f.dict.is_some()) {
        let raw = fixture.dict.as_deref().unwrap();
        let parsed = crate::decoding::Dictionary::from_serialized_or_raw_content(raw).unwrap();
        let (decoded, _) = decode_to_vec(&fixture.frame, LegacyDict::Parsed(&parsed))
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
        assert!(decoded == fixture.content, "{}", fixture.name);
    }
    // A raw-content dictionary answers to the ID it was given.
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name.ends_with(".v07.dict.zst"))
        .unwrap();
    let raw = crate::decoding::Dictionary::from_raw_content(6, vec![0; 64]).unwrap();
    assert_eq!(
        decode_to_vec(&fixture.frame, LegacyDict::Parsed(&raw)).unwrap_err(),
        Error::DictionaryMismatch {
            expected: 7,
            provided: 6
        }
    );
}

/// `FrameDecoder::decode_all` decodes each fixture, its dictionary given as a
/// handle or registered under the ID a v0.7 frame names, and a frame between
/// RFC 8878 frames; a slice one byte short is too small.
#[cfg(feature = "std")]
#[test]
fn decode_all_decodes_the_fixtures() {
    use crate::decoding::{DictionaryHandle, FrameDecoder, errors::FrameDecoderError};
    for fixture in fixtures() {
        let mut out = vec![0u8; fixture.content.len()];
        let mut decoder = FrameDecoder::new();
        let written = match &fixture.dict {
            None => decoder.decode_all(&fixture.frame, &mut out),
            Some(raw) => {
                let dict = DictionaryHandle::from_serialized_or_raw_content(raw).unwrap();
                decoder.decode_all_with_dict_handle(&fixture.frame, &mut out, &dict)
            }
        }
        .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
        assert_eq!(written, fixture.content.len(), "{}", fixture.name);
        assert!(out == fixture.content, "{}", fixture.name);
        if let Some(short) = fixture.content.len().checked_sub(1)
            && fixture.dict.is_none()
        {
            assert!(matches!(
                FrameDecoder::new().decode_all(&fixture.frame, &mut out[..short]),
                Err(FrameDecoderError::TargetTooSmall)
            ));
        }
    }

    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name.ends_with(".v07.dict.zst"))
        .unwrap();
    let mut out = vec![0u8; fixture.content.len()];
    assert!(matches!(
        FrameDecoder::new().decode_all(&fixture.frame, &mut out),
        Err(FrameDecoderError::DictNotProvided { dict_id: 7 })
    ));
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict_from_bytes(fixture.dict.as_deref().unwrap())
        .unwrap();
    decoder.decode_all(&fixture.frame, &mut out).unwrap();
    assert!(out == fixture.content);

    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name == "abc.txt.v06.l1.zst")
        .unwrap();
    let rfc = crate::encoding::compress_to_vec(
        &b"between"[..],
        crate::encoding::CompressionLevel::Fastest,
    );
    let mut input = rfc.clone();
    input.extend_from_slice(&fixture.frame);
    input.extend_from_slice(&rfc);
    let mut expected = b"between".to_vec();
    expected.extend_from_slice(&fixture.content);
    expected.extend_from_slice(b"between");
    let mut out = vec![0u8; expected.len()];
    assert_eq!(
        FrameDecoder::new().decode_all(&input, &mut out).unwrap(),
        expected.len()
    );
    assert!(out == expected);
}

/// A source that hands out one byte per read.
#[cfg(feature = "std")]
struct Trickle<'a>(&'a [u8]);

#[cfg(feature = "std")]
impl std::io::Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some((&first, rest)) = self.0.split_first() else {
            return Ok(0);
        };
        if buf.is_empty() {
            return Ok(0);
        }
        buf[0] = first;
        self.0 = rest;
        Ok(1)
    }
}

/// `StreamingDecoder` decodes each fixture whole, and from a source that
/// gives one byte at a time into a small buffer, which drives the stream a
/// step at a time and keeps only a window of history; a stream mixing these
/// frames with RFC 8878 ones decodes in order.
#[cfg(feature = "std")]
#[test]
fn streaming_decoder_decodes_the_fixtures() {
    use crate::decoding::{DictionaryHandle, StreamingDecoder};
    use std::io::Read;
    for fixture in fixtures() {
        let dict = fixture
            .dict
            .as_deref()
            .map(|raw| DictionaryHandle::from_serialized_or_raw_content(raw).unwrap());
        let open = |source| match &dict {
            None => StreamingDecoder::new(source),
            Some(dict) => StreamingDecoder::new_with_dictionary_handle(source, dict).unwrap(),
        };
        let mut whole = Vec::new();
        open(&fixture.frame[..])
            .read_to_end(&mut whole)
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
        assert!(whole == fixture.content, "{}", fixture.name);

        if fixture.frame.len() < 64 * 1024 {
            let mut decoder = match &dict {
                None => StreamingDecoder::new(Trickle(&fixture.frame)),
                Some(dict) => {
                    StreamingDecoder::new_with_dictionary_handle(Trickle(&fixture.frame), dict)
                        .unwrap()
                }
            };
            let mut pieces = Vec::new();
            let mut buf = [0u8; 1000];
            loop {
                let n = decoder
                    .read(&mut buf)
                    .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
                if n == 0 {
                    break;
                }
                pieces.extend_from_slice(&buf[..n]);
            }
            assert!(pieces == fixture.content, "{} in pieces", fixture.name);
        }
    }

    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name == "z000001.v07.l19.zst")
        .unwrap();
    let rfc = crate::encoding::compress_to_vec(
        &b"between"[..],
        crate::encoding::CompressionLevel::Fastest,
    );
    let mut input = rfc.clone();
    input.extend_from_slice(&fixture.frame);
    input.extend_from_slice(&rfc);
    input.extend_from_slice(&fixture.frame);
    let mut expected = b"between".to_vec();
    expected.extend_from_slice(&fixture.content);
    expected.extend_from_slice(b"between");
    expected.extend_from_slice(&fixture.content);
    let mut out = Vec::new();
    StreamingDecoder::new(&input[..])
        .read_to_end(&mut out)
        .unwrap();
    assert!(out == expected);
    let mut out = Vec::new();
    let mut decoder = StreamingDecoder::new(&input[..]);
    let mut buf = [0u8; 4096];
    loop {
        let n = decoder.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert!(out == expected);
}

/// The block-level interface drives these frames too: `reset` starts one,
/// `decode_blocks` decodes it a block at a time and `collect` hands it out;
/// `decode_from_to` decodes it from input given in small pieces.
#[cfg(feature = "std")]
#[test]
fn frame_decoder_steps_through_the_fixtures() {
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    for fixture in fixtures().into_iter().filter(|f| f.dict.is_none()) {
        let mut source = &fixture.frame[..];
        let mut decoder = FrameDecoder::new();
        decoder.reset(&mut source).unwrap();
        let mut out = Vec::new();
        while !decoder
            .decode_blocks(&mut source, BlockDecodingStrategy::UptoBlocks(1))
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.name))
        {
            out.extend(decoder.collect().unwrap());
        }
        out.extend(decoder.collect().unwrap());
        assert!(source.is_empty(), "{}", fixture.name);
        assert!(out == fixture.content, "{}", fixture.name);
        assert_eq!(
            decoder.bytes_read_from_source(),
            fixture.frame.len() as u64,
            "{}",
            fixture.name
        );

        // `decode_from_to` with what the decoder asks for, and a target
        // smaller than a block.
        let mut decoder = FrameDecoder::new();
        let mut source = &fixture.frame[..];
        decoder.reset(&mut source).unwrap();
        let mut out = Vec::new();
        let mut target = [0u8; 777];
        while !(decoder.is_finished() && decoder.can_collect() == 0) {
            let need = decoder.input_needed(source).min(source.len());
            let (read, written) = decoder
                .decode_from_to(&source[..need], &mut target)
                .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
            source = &source[read..];
            out.extend_from_slice(&target[..written]);
            assert!(read > 0 || written > 0, "{} stalled", fixture.name);
        }
        assert!(out == fixture.content, "{}", fixture.name);
    }
}

/// The frame queries answer for these frames as libzstd does: a frame, its
/// length through the end block, its declared size or unknown, and a bound
/// of 128 KiB per block; a cut frame is truncated.
#[cfg(feature = "std")]
#[test]
fn frame_queries_answer_for_the_fixtures() {
    use crate::decoding::{
        FrameContentSize, FrameSizeError, find_decompressed_size, find_frame_compressed_size,
        frame_decompressed_bound, is_frame, read_frame_content_size,
    };
    for fixture in fixtures() {
        let frame = &fixture.frame;
        assert!(is_frame(frame), "{}", fixture.name);
        assert_eq!(
            find_frame_compressed_size(frame).unwrap(),
            frame.len(),
            "{}",
            fixture.name
        );
        let bound = frame_decompressed_bound(frame).unwrap();
        assert!(bound >= fixture.content.len() as u64, "{}", fixture.name);
        let declared = declared_content_size(frame);
        let expected = declared.map_or(FrameContentSize::Unknown, FrameContentSize::Known);
        assert_eq!(
            read_frame_content_size(frame).unwrap(),
            expected,
            "{}",
            fixture.name
        );
        assert_eq!(
            find_decompressed_size(frame).unwrap(),
            expected,
            "{}",
            fixture.name
        );
        assert!(matches!(
            find_frame_compressed_size(&frame[..frame.len() - 1]),
            Err(FrameSizeError::Truncated)
        ));
    }
    // v0.5 declares no size.
    let v05 = fixtures()
        .into_iter()
        .find(|f| f.name == "abc.txt.v05.l1.zst")
        .unwrap();
    assert_eq!(
        read_frame_content_size(&v05.frame).unwrap(),
        FrameContentSize::Unknown
    );
}

/// Every truncation of every fixture is refused or decodes to a prefix, and
/// none panics.
#[cfg(feature = "std")]
#[test]
fn truncated_frames_are_refused() {
    for fixture in fixtures().into_iter().filter(|f| f.frame.len() < 4096) {
        let dict = fixture.legacy_dict();
        for len in 0..fixture.frame.len() {
            assert!(
                decode_to_vec(&fixture.frame[..len], dict).is_err(),
                "{} cut to {len} bytes decoded",
                fixture.name
            );
        }
    }
}

/// Flipping any one bit of a frame is refused or decodes to some output, but
/// never panics; a v0.7 frame with a checksum, verified, decodes to other
/// content only when that content's checksum is the one the frame carries,
/// which with 22 bits of it does happen.
#[cfg(feature = "std")]
#[test]
fn damaged_frames_never_panic() {
    for fixture in fixtures().into_iter().filter(|f| f.frame.len() < 4096) {
        let dict = fixture.legacy_dict();
        let mut frame = fixture.frame.clone();
        for byte in 0..frame.len() {
            for bit in 0..8 {
                frame[byte] ^= 1 << bit;
                let decoded = decode_verified(&frame, dict);
                #[cfg(feature = "hash")]
                if let Ok((content, end)) = &decoded
                    && *content != fixture.content
                    && read_header(&frame).is_ok_and(|header| header.checksum)
                {
                    let carried = u64::from(frame[end - 1])
                        | u64::from(frame[end - 2]) << 8
                        | u64::from(frame[end - 3] & 0x3F) << 16;
                    let digest = twox_hash::XxHash64::oneshot(0, content) >> 11 & 0x3F_FFFF;
                    assert_eq!(
                        digest, carried,
                        "{}: bit {bit} of byte {byte} changed the content unchecked",
                        fixture.name
                    );
                }
                drop(decoded);
                frame[byte] ^= 1 << bit;
            }
        }
    }
}

/// A v0.7 frame whose content checksum does not match is refused with the
/// checksum error when verified, and decodes when not.
#[cfg(all(feature = "std", feature = "hash"))]
#[test]
fn a_wrong_v07_checksum_is_refused() {
    let fixture = fixture("abc.txt.v07.l1.zst");
    let mut frame = fixture.frame.clone();
    // The end block's header holds the checksum in its low 22 bits.
    let last = frame.len() - 1;
    frame[last] ^= 1;
    assert!(matches!(
        decode_verified(&frame, LegacyDict::None),
        Err(Error::ChecksumMismatch { .. })
    ));
    assert!(decode_to_vec(&frame, LegacyDict::None).unwrap().0 == fixture.content);
}

/// The headers of the three formats: version, size, window and content size.
#[test]
fn headers_read_their_fields() {
    // v0.5: magic, then a window descriptor of log 11 + 1.
    let v05 = read_header(&[0x25, 0xB5, 0x2F, 0xFD, 0x01]).unwrap();
    assert_eq!(
        (v05.version, v05.size, v05.window, v05.content_size),
        (LegacyVersion::V05, 5, 1 << 12, None)
    );
    // v0.5 reserves the descriptor's high nibble.
    assert_eq!(
        read_header(&[0x25, 0xB5, 0x2F, 0xFD, 0x10]).unwrap_err(),
        Error::UnsupportedParameter
    );
    // v0.6: a one-byte content size of 200, window log 12 + 3.
    let v06 = read_header(&[0x26, 0xB5, 0x2F, 0xFD, 0x43, 200]).unwrap();
    assert_eq!(
        (v06.version, v06.size, v06.window, v06.content_size),
        (LegacyVersion::V06, 6, 1 << 15, Some(200))
    );
    // A two-byte content size counts from 256.
    let v06 = read_header(&[0x26, 0xB5, 0x2F, 0xFD, 0x80, 1, 0]).unwrap();
    assert_eq!(v06.content_size, Some(257));
    // v0.6 reserves bit 5.
    assert_eq!(
        read_header(&[0x26, 0xB5, 0x2F, 0xFD, 0x20]).unwrap_err(),
        Error::UnsupportedParameter
    );
    // v0.7: checksum, a one-byte dictionary ID, single segment with a
    // one-byte content size, which is also the window.
    let v07 = read_header(&[0x27, 0xB5, 0x2F, 0xFD, 0x25, 9, 100]).unwrap();
    assert_eq!(
        (
            v07.size,
            v07.window,
            v07.content_size,
            v07.dict_id,
            v07.checksum
        ),
        (7, 100, Some(100), 9, true)
    );
    // v0.7 reserves bit 3, and a header cut short is truncated.
    assert_eq!(
        read_header(&[0x27, 0xB5, 0x2F, 0xFD, 0x08, 0]).unwrap_err(),
        Error::UnsupportedParameter
    );
    assert_eq!(
        read_header(&[0x27, 0xB5, 0x2F, 0xFD, 0x25, 9]).unwrap_err(),
        Error::Truncated
    );
    // A window past the format's largest is refused.
    assert_eq!(
        read_header(&[0x27, 0xB5, 0x2F, 0xFD, 0x00, 0xF8]).unwrap_err(),
        Error::UnsupportedParameter
    );
}

/// An RFC 8878 frame and a frame of another magic number are not legacy.
#[test]
fn other_magic_numbers_are_not_legacy() {
    assert_eq!(LegacyVersion::of(&[0x28, 0xB5, 0x2F, 0xFD]), None);
    assert_eq!(LegacyVersion::of(&[0x24, 0xB5, 0x2F, 0xFD]), None);
    assert_eq!(LegacyVersion::of(&[0x25, 0xB5, 0x2F]), None);
    assert_eq!(declared_content_size(&[0x26, 0xB5, 0x2F, 0xFD, 0x43]), None);
}

/// v0.5 and v0.6 end a frame at any block whose body is empty, v0.7 only at
/// its end block: a raw block of no bytes is an empty block there.
#[test]
fn an_empty_block_ends_only_older_frames() {
    let empty_raw = [0x40, 0, 0];
    let end = [0xC0, 0, 0];
    let mut v06 = vec![0x26, 0xB5, 0x2F, 0xFD, 0x00];
    v06.extend_from_slice(&empty_raw);
    assert_eq!(frame_size(LegacyVersion::V06, &v06), Some((8, 0)));
    let mut v07 = vec![0x27, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    v07.extend_from_slice(&empty_raw);
    assert_eq!(frame_size(LegacyVersion::V07, &v07), None);
    v07.extend_from_slice(&end);
    assert_eq!(
        frame_size(LegacyVersion::V07, &v07),
        Some((12, BLOCK_SIZE_MAX as u64))
    );
    assert_eq!(
        decode_to_vec(&v07, LegacyDict::None).unwrap(),
        (Vec::new(), 12)
    );
}

/// The size walk measures a header from its size fields alone, as upstream's
/// does: a reserved bit the decoder refuses does not stop the walk.
#[test]
fn frame_size_reads_only_the_size_fields() {
    let end = [0xC0, 0, 0];
    // v0.6 with the reserved bit and a one-byte content size.
    let mut v06 = vec![0x26, 0xB5, 0x2F, 0xFD, 0x60, 9];
    v06.extend_from_slice(&end);
    assert_eq!(frame_size(LegacyVersion::V06, &v06), Some((9, 0)));
    // v0.7 with the reserved bit, a two-byte dictionary ID and a window byte.
    let mut v07 = vec![0x27, 0xB5, 0x2F, 0xFD, 0x0A, 0, 1, 2];
    v07.extend_from_slice(&end);
    assert_eq!(frame_size(LegacyVersion::V07, &v07), Some((11, 0)));
    // v0.7 single segment without a size field still carries one byte of it.
    let mut v07 = vec![0x27, 0xB5, 0x2F, 0xFD, 0x20, 7];
    v07.extend_from_slice(&end);
    assert_eq!(frame_size(LegacyVersion::V07, &v07), Some((9, 0)));
    // v0.5 headers are five bytes whatever the descriptor says.
    let mut v05 = vec![0x25, 0xB5, 0x2F, 0xFD, 0xF0];
    v05.extend_from_slice(&end);
    assert_eq!(frame_size(LegacyVersion::V05, &v05), Some((8, 0)));
    assert_eq!(frame_size(LegacyVersion::V05, &v05[..4]), None);
}

/// A stream keeps its window and a block of history, not the whole frame:
/// a frame of a 1 KiB window and 1 MiB of blocks decodes in order while the
/// buffer stays within a few blocks.
#[test]
fn a_stream_holds_its_window_not_the_frame() {
    // v0.7, window byte 0: a 1 KiB window.
    let mut frame = vec![0x27, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    let mut expected = Vec::new();
    for n in 0..8u8 {
        let size = 128 * 1024 - 1;
        // A raw block of `size` bytes of `n`.
        frame.extend_from_slice(&[0x40 | (size >> 16) as u8, (size >> 8) as u8, size as u8]);
        frame.extend(core::iter::repeat_n(n, size));
        expected.extend(core::iter::repeat_n(n, size));
    }
    frame.extend_from_slice(&[0xC0, 0, 0]);
    let header = read_header(&frame).unwrap();
    let mut stream = LegacyStream::new();
    stream.start(header, None, FrameOptions::default()).unwrap();
    let mut src = &frame[header.size..];
    let mut out = Vec::new();
    let mut target = vec![0u8; 50_000];
    let mut largest = 0;
    while !(stream.is_finished() && stream.undelivered() == 0) {
        let (read, written) = stream.decode(src, &mut target).unwrap();
        src = &src[read..];
        out.extend_from_slice(&target[..written]);
        largest = largest.max(stream.heap_bytes());
        assert!(read > 0 || written > 0);
    }
    assert!(src.is_empty());
    assert!(out == expected);
    assert_eq!(stream.produced(), expected.len() as u64);
    assert_eq!(stream.consumed(), frame.len() as u64);
    assert!(largest <= 4 * BLOCK_SIZE_MAX, "held {largest} bytes");
}

/// A frame several windows long decodes through the stream with its matches
/// reaching into the segment before the current one, the buffer never past
/// the window and a block.
#[cfg(feature = "std")]
#[test]
fn a_stream_reads_matches_from_the_segment_before() {
    for fixture in fixtures()
        .into_iter()
        .filter(|f| f.name.starts_with("z000000x8."))
    {
        let header = read_header(&fixture.frame).unwrap();
        let capacity = header.window as usize + BLOCK_SIZE_MAX;
        assert!(
            fixture.content.len() > 2 * capacity,
            "{}: the frame must outrun the buffer",
            fixture.name
        );
        let mut stream = LegacyStream::new();
        stream.start(header, None, FrameOptions::default()).unwrap();
        let mut src = &fixture.frame[header.size..];
        let mut out = Vec::new();
        let mut target = vec![0u8; 4096];
        while !(stream.is_finished() && stream.undelivered() == 0) {
            let (read, written) = stream
                .decode(src, &mut target)
                .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
            assert!(read > 0 || written > 0, "{} stalled", fixture.name);
            src = &src[read..];
            out.extend_from_slice(&target[..written]);
        }
        assert!(out == fixture.content, "{}", fixture.name);
        assert!(
            stream.buf.capacity() <= capacity,
            "{}: held {} bytes for a window of {}",
            fixture.name,
            stream.buf.capacity(),
            header.window
        );
    }
}

/// The fixture of this name.
#[cfg(feature = "std")]
fn fixture(name: &str) -> fixtures::Fixture {
    fixtures()
        .into_iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no fixture {name}"))
}

/// A fresh decoder handed one of these frames by `decode_from_to` starts it
/// itself, as it starts an RFC 8878 frame, and decodes it to its content.
#[cfg(feature = "std")]
#[test]
fn decode_from_to_starts_these_frames_on_a_fresh_decoder() {
    use crate::decoding::FrameDecoder;
    for name in [
        "abc.txt.v05.l1.zst",
        "z000001.v06.l19.zst",
        "z000001.v07.l19.zst",
    ] {
        let fixture = fixture(name);
        let mut decoder = FrameDecoder::new();
        let mut src = &fixture.frame[..];
        let mut out = Vec::new();
        let mut target = [0u8; 500];
        loop {
            let (read, written) = decoder
                .decode_from_to(src, &mut target)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            src = &src[read..];
            out.extend_from_slice(&target[..written]);
            if decoder.is_finished() && decoder.can_collect() == 0 {
                break;
            }
            assert!(read > 0 || written > 0, "{name} stalled");
        }
        assert!(src.is_empty(), "{name}");
        assert!(out == fixture.content, "{name}");
    }
}

/// The checksum mode applies to a v0.7 frame's 22-bit checksum as to an
/// RFC 8878 frame's: `Verify` refuses a frame whose content does not match
/// it, `EmitOnly` decodes it and exposes both values, `None` computes
/// nothing.
#[cfg(all(feature = "std", feature = "hash"))]
#[test]
fn the_checksum_mode_applies_to_v07_frames() {
    use crate::decoding::errors::FrameDecoderError;
    use crate::decoding::{BlockDecodingStrategy, ContentChecksum, FrameDecoder};
    let fixture = fixture("abc.txt.v07.l1.zst");
    let mut frame = fixture.frame.clone();
    let last = frame.len() - 1;
    frame[last] ^= 1;
    let end = &frame[frame.len() - 3..];
    let stored = u32::from(end[2]) | u32::from(end[1]) << 8 | u32::from(end[0] & 0x3F) << 16;
    let run = |mode| {
        let mut decoder = FrameDecoder::new();
        decoder.set_content_checksum(mode);
        let mut src = &frame[..];
        decoder.reset(&mut src)?;
        decoder.decode_blocks(&mut src, BlockDecodingStrategy::All)?;
        let content = decoder.collect().unwrap();
        Ok::<_, FrameDecoderError>((
            content,
            decoder.get_checksum_from_data(),
            decoder.get_calculated_checksum(),
        ))
    };
    assert!(matches!(
        run(ContentChecksum::Verify),
        Err(FrameDecoderError::LegacyFrame(
            Error::ChecksumMismatch { .. }
        ))
    ));
    let (content, from_data, calculated) = run(ContentChecksum::EmitOnly).unwrap();
    assert!(content == fixture.content);
    assert_eq!(from_data, Some(stored));
    assert!(calculated.is_some_and(|calculated| calculated != stored));
    let (content, _, calculated) = run(ContentChecksum::None).unwrap();
    assert!(content == fixture.content);
    assert_eq!(calculated, None);

    // The same on the whole-buffer path.
    let mut out = vec![0u8; fixture.content.len()];
    let mut decoder = FrameDecoder::new();
    decoder.set_content_checksum(ContentChecksum::Verify);
    assert!(decoder.decode_all(&frame, &mut out).is_err());
    for mode in [ContentChecksum::EmitOnly, ContentChecksum::None] {
        decoder.set_content_checksum(mode);
        assert_eq!(
            decoder.decode_all(&frame, &mut out).unwrap(),
            fixture.content.len()
        );
    }
}

/// A block limit set on the decoder holds these frames' blocks to it, on
/// both paths.
#[cfg(feature = "std")]
#[test]
fn the_block_limit_applies_to_these_frames() {
    use crate::decoding::errors::FrameDecoderError;
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    for name in [
        "z000000.v05.l1.zst",
        "z000000.v06.l1.zst",
        "z000000.v07.l1.zst",
    ] {
        let fixture = fixture(name);
        let mut decoder = FrameDecoder::new();
        decoder.set_max_block_size(Some(1024));
        let mut out = vec![0u8; fixture.content.len()];
        assert!(
            matches!(
                decoder.decode_all(&fixture.frame, &mut out),
                Err(FrameDecoderError::LegacyFrame(_))
            ),
            "{name}"
        );
        let mut src = &fixture.frame[..];
        decoder.reset(&mut src).unwrap();
        assert!(
            matches!(
                decoder.decode_blocks(&mut src, BlockDecodingStrategy::All),
                Err(FrameDecoderError::LegacyFrame(_))
            ),
            "{name}"
        );
        decoder.set_max_block_size(None);
        assert_eq!(
            decoder.decode_all(&fixture.frame, &mut out).unwrap(),
            out.len(),
            "{name}"
        );
    }
}

/// A window ceiling lowered under a started frame's window refuses its next
/// step, as it refuses an RFC 8878 frame's.
#[cfg(feature = "std")]
#[test]
fn a_lowered_window_ceiling_binds_a_started_frame() {
    use crate::decoding::errors::FrameDecoderError;
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    let fixture = fixture("z000000.v07.l1.zst");
    let window = read_header(&fixture.frame).unwrap().window;
    let mut decoder = FrameDecoder::new();
    let mut src = &fixture.frame[..];
    decoder.reset(&mut src).unwrap();
    decoder.set_max_window_size(window / 2).unwrap();
    assert!(matches!(
        decoder.decode_blocks(&mut src, BlockDecodingStrategy::All),
        Err(FrameDecoderError::WindowSizeTooBig { requested, limit })
            if requested == window && limit == window / 2
    ));
    let mut target = [0u8; 64];
    assert!(matches!(
        decoder.decode_from_to(src, &mut target),
        Err(FrameDecoderError::WindowSizeTooBig { .. })
    ));
    // Raised again, the frame decodes on.
    decoder.set_max_window_size(window).unwrap();
    decoder
        .decode_blocks(&mut src, BlockDecodingStrategy::All)
        .unwrap();
    assert!(decoder.collect().unwrap() == fixture.content);

    // A ceiling below the window refuses the frame from its header.
    decoder.set_max_window_size(window / 2).unwrap();
    assert!(matches!(
        decoder.reset(&fixture.frame[..]),
        Err(FrameDecoderError::WindowSizeTooBig { requested, limit })
            if requested == window && limit == window / 2
    ));
}

/// The decoder's accessors describe a started frame of these formats: its
/// declared content size, and the blocks decoded so far, not what the frame
/// before left.
#[cfg(feature = "std")]
#[test]
fn the_accessors_describe_these_frames() {
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    let rfc = crate::encoding::compress_slice_to_vec(
        &[7u8; 500],
        crate::encoding::CompressionLevel::Fastest,
    );
    let mut decoder = FrameDecoder::new();
    let mut src = &rfc[..];
    decoder.reset(&mut src).unwrap();
    decoder
        .decode_blocks(&mut src, BlockDecodingStrategy::All)
        .unwrap();
    assert_eq!(decoder.content_size(), 500);

    // v0.6.2 declares its first chunk's size.
    let v06 = fixture("z000000.v06.l1.zst");
    let mut src = &v06.frame[..];
    decoder.reset(&mut src).unwrap();
    assert_eq!(decoder.content_size(), 128 * 1024);
    assert_eq!(decoder.blocks_decoded(), 0);
    decoder
        .decode_blocks(&mut src, BlockDecodingStrategy::All)
        .unwrap();
    let (_, bound) = frame_size(LegacyVersion::V06, &v06.frame).unwrap();
    assert_eq!(
        decoder.blocks_decoded() as u64,
        bound / BLOCK_SIZE_MAX as u64
    );
    // The stream's buffers, past a block of output, count toward the
    // decoder's footprint.
    assert!(decoder.workspace_size() > BLOCK_SIZE_MAX);

    // v0.5 declares none.
    let v05 = fixture("abc.txt.v05.l1.zst");
    let mut src = &v05.frame[..];
    decoder.reset(&mut src).unwrap();
    assert_eq!(decoder.content_size(), 0);
}

/// Each failure carries the code libzstd returns for it, and keeps that code,
/// its message and itself as the source when the frame decoder wraps it.
#[cfg(feature = "std")]
#[test]
fn errors_report_libzstd_codes() {
    use crate::decoding::errors::{ErrorCode, FrameDecoderError};
    use std::error::Error as _;
    use std::string::ToString;
    let cases = [
        (
            Error::Truncated,
            ErrorCode::CorruptionDetected,
            "ends inside",
        ),
        (
            Error::UnsupportedParameter,
            ErrorCode::FrameParameterUnsupported,
            "unsupported setting",
        ),
        (
            Error::Corrupted {
                reason: "bad table",
            },
            ErrorCode::CorruptionDetected,
            "bad table",
        ),
        (
            Error::OutputTooSmall,
            ErrorCode::DstSizeTooSmall,
            "too small",
        ),
        (
            Error::DictionaryCorrupted,
            ErrorCode::DictionaryCorrupted,
            "dictionary could not be read",
        ),
        (
            Error::DictionaryMismatch {
                expected: 0xAB,
                provided: 0,
            },
            ErrorCode::DictionaryWrong,
            "dictionary 0xAB",
        ),
        (
            Error::ChecksumMismatch {
                expected: 1,
                calculated: 2,
            },
            ErrorCode::ChecksumWrong,
            "0x000001",
        ),
    ];
    for (err, code, text) in cases {
        assert_eq!(err.code(), code, "{err:?}");
        let message = err.to_string();
        assert!(message.contains(text), "{message}");
        let wrapped = FrameDecoderError::from(err);
        assert_eq!(wrapped.code(), code);
        assert_eq!(wrapped.to_string(), message);
        assert!(
            wrapped
                .source()
                .is_some_and(|source| source.to_string() == message)
        );
    }
}

/// After `decode_all` the accessors describe the frame it decoded last, as
/// they do an RFC 8878 one, not the frame decoded before it.
#[cfg(feature = "std")]
#[test]
fn the_accessors_describe_a_frame_decoded_whole() {
    use crate::decoding::FrameDecoder;
    let rfc = crate::encoding::compress_slice_to_vec(
        &[7u8; 500],
        crate::encoding::CompressionLevel::Fastest,
    );
    let v06 = fixture("z000000.v06.l1.zst");
    let mut decoder = FrameDecoder::new();
    let mut out = vec![0u8; v06.content.len()];
    decoder.decode_all(&rfc, &mut out).unwrap();
    assert_eq!(decoder.content_size(), 500);

    assert_eq!(
        decoder.decode_all(&v06.frame, &mut out).unwrap(),
        v06.content.len()
    );
    let (_, bound) = frame_size(LegacyVersion::V06, &v06.frame).unwrap();
    assert_eq!(decoder.content_size(), 128 * 1024);
    assert_eq!(
        decoder.blocks_decoded() as u64,
        bound / BLOCK_SIZE_MAX as u64
    );
    assert_eq!(decoder.bytes_read_from_source(), v06.frame.len() as u64);
    assert!(decoder.is_finished());
    assert_eq!(decoder.can_collect(), 0);

    #[cfg(feature = "hash")]
    {
        let v07 = fixture("abc.txt.v07.l1.zst");
        let end = &v07.frame[v07.frame.len() - 3..];
        let stored = u32::from(end[2]) | u32::from(end[1]) << 8 | u32::from(end[0] & 0x3F) << 16;
        decoder.decode_all(&v07.frame, &mut out).unwrap();
        assert_eq!(decoder.get_checksum_from_data(), Some(stored));
        assert_eq!(decoder.get_calculated_checksum(), Some(stored));
    }
}

/// Per-block checksums cover these frames' blocks on every path, as they
/// cover an RFC 8878 frame's.
#[cfg(all(feature = "std", feature = "lsm", feature = "hash"))]
#[test]
fn per_block_checksums_cover_these_frames() {
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    // One data block: its digest is the content's.
    let one = fixture("abc.txt.v07.l1.zst");
    let digest = twox_hash::XxHash64::oneshot(0, &one.content) as u32;
    let many = fixture("z000000.v07.l1.zst");
    let (_, bound) = frame_size(LegacyVersion::V07, &many.frame).unwrap();
    let blocks = (bound / BLOCK_SIZE_MAX as u64) as usize;

    let mut decoder = FrameDecoder::new();
    decoder.enable_per_block_checksums();
    let mut out = vec![0u8; many.content.len()];
    decoder.decode_all(&one.frame, &mut out).unwrap();
    assert_eq!(decoder.computed_block_checksums(), [digest]);
    decoder.decode_all(&many.frame, &mut out).unwrap();
    assert_eq!(decoder.computed_block_checksums().len(), blocks);

    let mut src = &one.frame[..];
    decoder.reset(&mut src).unwrap();
    decoder
        .decode_blocks(&mut src, BlockDecodingStrategy::All)
        .unwrap();
    assert_eq!(decoder.computed_block_checksums(), [digest]);
}

/// A pinned dictionary ID or window descriptor holds these frames to it, on
/// both paths: v0.7 carries both fields, v0.5 and v0.6 neither.
#[cfg(all(feature = "std", feature = "lsm"))]
#[test]
fn pinned_header_fields_bind_these_frames() {
    use crate::decoding::FrameDecoder;
    use crate::decoding::errors::FrameDecoderError;
    let v07 = fixture("z000000x8.v07.l1.zst");
    // Not single-segment: the window byte follows the descriptor.
    assert_eq!(v07.frame[4] & 0x20, 0);
    let window = v07.frame[5];
    let v05 = fixture("abc.txt.v05.l1.zst");
    let mut out = vec![0u8; v07.content.len()];

    let mut decoder = FrameDecoder::new();
    decoder.expect_dict_id(Some(5));
    assert!(matches!(
        decoder.decode_all(&v07.frame, &mut out),
        Err(FrameDecoderError::UnexpectedDictId {
            expected: Some(5),
            found: None
        })
    ));
    assert!(matches!(
        decoder.reset(&v07.frame[..]),
        Err(FrameDecoderError::UnexpectedDictId { .. })
    ));
    decoder.expect_dict_id(Some(0));
    decoder.expect_window_descriptor(Some(window ^ 1));
    assert!(matches!(
        decoder.decode_all(&v07.frame, &mut out),
        Err(FrameDecoderError::UnexpectedWindowDescriptor { found: Some(w), .. }) if w == window
    ));
    assert!(matches!(
        decoder.reset(&v05.frame[..]),
        Err(FrameDecoderError::UnexpectedWindowDescriptor { found: None, .. })
    ));
    decoder.expect_window_descriptor(Some(window));
    assert_eq!(
        decoder.decode_all(&v07.frame, &mut out).unwrap(),
        v07.content.len()
    );
}

/// A v0.5-v0.7 magic number with nothing after it is a truncated frame to
/// the aggregate size query, as upstream's `ZSTD_findDecompressedSize`
/// refuses a tail shorter than a frame's first five bytes.
#[cfg(feature = "std")]
#[test]
fn a_bare_magic_number_is_a_truncated_frame() {
    use crate::decoding::find_decompressed_size;
    let magic = [0x27, 0xB5, 0x2F, 0xFD];
    assert!(find_decompressed_size(&magic).is_err());
    let mut stream = fixture("z000000.v07.l1.zst").frame;
    stream.extend_from_slice(&magic);
    assert!(find_decompressed_size(&stream).is_err());
}

/// A reader failing after the magic number hands its own error back, in the
/// header field it failed in, rather than calling the frame truncated.
#[cfg(feature = "std")]
#[test]
fn reader_errors_inside_the_header_are_kept() {
    use crate::decoding::FrameDecoder;
    use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    struct Stalls<'a>(&'a [u8]);
    impl std::io::Read for Stalls<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    let mut decoder = FrameDecoder::new();
    let err = decoder
        .reset(Stalls(&[0x27, 0xB5, 0x2F, 0xFD]))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::FrameDescriptorReadError(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "{err:?}"
    );
    // v0.7 with a window byte still to come.
    let err = decoder
        .reset(Stalls(&[0x27, 0xB5, 0x2F, 0xFD, 0x00]))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::WindowDescriptorReadError(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "{err:?}"
    );
    // v0.7 with its window byte read and a one-byte dictionary ID to come.
    let err = decoder
        .reset(Stalls(&[0x27, 0xB5, 0x2F, 0xFD, 0x01, 0x00]))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::DictionaryIdReadError(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "{err:?}"
    );
    // v0.6 with a two-byte content size still to come.
    let err = decoder
        .reset(Stalls(&[0x26, 0xB5, 0x2F, 0xFD, 0x80, 1]))
        .unwrap_err();
    assert!(
        matches!(
            &err,
            FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::FrameContentSizeReadError(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "{err:?}"
    );
}

/// The stream counts in its footprint the entropy tables and literal buffer
/// its frame holds, not only its output buffer.
#[cfg(feature = "std")]
#[test]
fn the_stream_footprint_counts_its_tables() {
    for name in [
        "z000000.v05.l1.zst",
        "z000000.v06.l1.zst",
        "z000000.v07.l1.zst",
    ] {
        let fixture = fixture(name);
        let header = read_header(&fixture.frame).unwrap();
        let mut stream = LegacyStream::new();
        stream.start(header, None, FrameOptions::default()).unwrap();
        let mut target = vec![0u8; fixture.content.len()];
        stream
            .decode(&fixture.frame[header.size..], &mut target)
            .unwrap();
        assert!(
            stream.heap_bytes() > stream.buf.capacity() + stream.block.capacity(),
            "{name}: {} bytes held, {} of them the output buffer",
            stream.heap_bytes(),
            stream.buf.capacity()
        );
    }
}

/// A block whose header declares more than the largest block is refused from
/// the header alone: the stream asks for no more than it.
#[test]
fn an_oversized_block_is_refused_from_its_header() {
    // v0.7 with a 1 KiB window.
    let frame = [0x27, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    let header = read_header(&frame).unwrap();
    let mut stream = LegacyStream::new();
    stream.start(header, None, FrameOptions::default()).unwrap();
    // Less than a block header asks for the header.
    assert_eq!(stream.input_needed(&[0x40]), 3);
    // A raw block of 0x3FFFF bytes, compressed and raw alike.
    for kind in [0x00u8, 0x40] {
        let block = [kind | 0x03, 0xFF, 0xFF];
        assert_eq!(stream.input_needed(&block), 3);
        assert!(stream.decode(&block, &mut [0u8; 16]).is_err());
    }
}

/// A v0.7 block whose sequence tables are each one symbol decodes, and one
/// naming a symbol past its field is refused.
#[test]
fn one_symbol_sequence_tables_decode_in_v07() {
    // Raw literals "a"; one sequence; every table one symbol: literal length
    // code 1, offset code 2 (two extra bits, here 0, so offset 1), match
    // length code 0 (3 bytes). The bitstream holds those two bits under its
    // end mark.
    let block = [0x81, b'a', 0x01, 0x54, 1, 2, 0, 0x04];
    let mut frame = vec![0x27, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x00, 0x00];
    frame.push(block.len() as u8);
    frame.extend_from_slice(&block);
    frame.extend_from_slice(&[0xC0, 0, 0]);
    assert_eq!(decode_to_vec(&frame, LegacyDict::None).unwrap().0, b"aaaa");
    // A match length symbol past the field's largest.
    let at = frame.len() - 4;
    frame[at - 1] = 200;
    assert!(matches!(
        decode_to_vec(&frame, LegacyDict::None),
        Err(Error::Corrupted { .. })
    ));
}

/// A sequence count takes one byte below 0x80, two up to 0x7EFF, and three
/// after a 0xFF where the format allows them.
#[test]
fn sequence_counts_take_their_forms() {
    assert_eq!(v07::sequence_count(&[0x7F], true).unwrap(), (0x7F, 1));
    assert_eq!(
        v07::sequence_count(&[0x81, 0x02], true).unwrap(),
        (0x102, 2)
    );
    assert_eq!(
        v07::sequence_count(&[0xFF, 0x01, 0x00], true).unwrap(),
        (0x7F01, 3)
    );
    assert_eq!(
        v07::sequence_count(&[0xFF, 0x01], false).unwrap(),
        (0x7F01, 2)
    );
    assert!(v07::sequence_count(&[], true).is_err());
    assert!(v07::sequence_count(&[0xFF, 0x01], true).is_err());
    assert!(v07::sequence_count(&[0x80], true).is_err());
}

/// The version queries: each format's minor version, and a header's length
/// from its size fields once the descriptor is in hand.
#[test]
fn versions_answer_their_queries() {
    assert_eq!(LegacyVersion::V05.minor(), 5);
    assert_eq!(LegacyVersion::V06.minor(), 6);
    assert_eq!(LegacyVersion::V07.minor(), 7);
    let v05 = [0x25, 0xB5, 0x2F, 0xFD, 0x01];
    assert_eq!(LegacyVersion::V05.header_size(&v05[..4]), None);
    assert_eq!(LegacyVersion::V05.header_size(&v05), Some(5));
    // v0.6 with a two-byte content size.
    assert_eq!(
        LegacyVersion::V06.header_size(&[0x26, 0xB5, 0x2F, 0xFD, 0x80]),
        Some(7)
    );
    // v0.7, single-segment with an eight-byte content size and a four-byte
    // dictionary ID.
    assert_eq!(
        LegacyVersion::V07.header_size(&[0x27, 0xB5, 0x2F, 0xFD, 0xE3]),
        Some(17)
    );
}

/// A literal section header the block cannot hold, or one past the largest
/// block, is refused before anything decodes.
#[test]
fn literal_headers_refuse_what_does_not_fit() {
    for header in [
        // Huffman literals with less than their five-byte minimum.
        &[0x00, 0, 0, 0][..],
        // Huffman literals declaring more than a block.
        &[0x3F, 0xFF, 0xFC, 0, 0, 0],
        // Huffman literals running past the block.
        &[0x00, 0x04, 0xFF, 0, 0],
        // Repeated-table literals in a form other than the small one.
        &[0x40, 0, 0],
        // Repeated-table literals running past the block.
        &[0x50, 0x03, 0xFF],
        // Raw literals running past the block.
        &[0x9F, 0, 0],
        // A run with a three-byte header and nothing after it.
        &[0xF0, 0, 0],
        // A run longer than a block.
        &[0xFF, 0xFF, 0xFF, 0],
        // Less than any literal section.
        &[0x80, 0],
    ] {
        assert!(
            matches!(Literals::read(header), Err(Error::Corrupted { .. })),
            "{header:02X?}"
        );
    }
}

/// A block's budget passes everything but its size check to the output it
/// guards.
#[cfg(feature = "hash")]
#[test]
fn a_budget_passes_through_to_its_output() {
    let mut buf = Vec::new();
    let mut inner = VecOutput {
        buf: &mut buf,
        start: 0,
    };
    let mut out = Budget {
        out: &mut inner,
        end: 8,
    };
    out.reserve(2).unwrap();
    out.extend(b"ab");
    out.repeat(b'c', 2);
    out.copy_back(4, 2);
    assert_eq!((out.produced(), out.held()), (6, 6));
    assert_eq!(out.since(2), b"ccab");
    assert!(out.reserve(3).is_err());
    out.copy_match(1, 2, &[]).unwrap();
    assert_eq!(buf, b"abccabbb");
}

/// The size walk refuses a frame cut inside a block's body.
#[cfg(feature = "std")]
#[test]
fn frame_size_refuses_a_cut_block() {
    let fixture = fixture("z000000.v07.l1.zst");
    let frame = &fixture.frame[..fixture.frame.len() - 10];
    assert_eq!(frame_size(LegacyVersion::V07, frame), None);
}

/// An RLE block decodes into a caller's slice and through a stream as it
/// does into a vector.
#[cfg(feature = "std")]
#[test]
fn rle_blocks_decode_on_every_path() {
    use crate::decoding::FrameDecoder;
    let frame = [
        0x27, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x80, 0, 5, b'z', 0xC0, 0, 0,
    ];
    let mut out = [0u8; 5];
    assert_eq!(FrameDecoder::new().decode_all(&frame, &mut out).unwrap(), 5);
    assert_eq!(&out, b"zzzzz");
    let mut decoder = FrameDecoder::new();
    let mut target = [0u8; 8];
    let (read, written) = decoder.decode_from_to(&frame, &mut target).unwrap();
    assert_eq!((read, &target[..written]), (frame.len(), &b"zzzzz"[..]));
}

/// A `Read` source failing inside a block hands back its own error, in the
/// block header or the body it failed in.
#[cfg(feature = "std")]
#[test]
fn reader_errors_inside_a_block_are_kept() {
    use crate::decoding::errors::FrameDecoderError;
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    let fixture = fixture("z000000.v07.l1.zst");
    let header = read_header(&fixture.frame).unwrap();
    for (cut, header_error) in [(header.size + 1, true), (header.size + 4, false)] {
        let mut decoder = FrameDecoder::new();
        let mut src = &fixture.frame[..cut];
        decoder.reset(&mut src).unwrap();
        let err = decoder
            .decode_blocks(&mut src, BlockDecodingStrategy::All)
            .unwrap_err();
        if header_error {
            assert!(
                matches!(err, FrameDecoderError::FailedToReadBlockHeader(_)),
                "{err:?}"
            );
        } else {
            assert!(
                matches!(err, FrameDecoderError::FailedToReadBlockBody(_)),
                "{err:?}"
            );
        }
    }
}

/// `read_to_end` over an RFC 8878 frame and then a v0.7 one decodes both
/// with the dictionary the reader was forced to, and hands out nothing of a
/// v0.7 frame that fails.
#[cfg(feature = "std")]
#[test]
fn read_to_end_keeps_a_forced_dictionary_for_these_frames() {
    use crate::decoding::{DictionaryHandle, StreamingDecoder};
    use std::io::Read;
    let v07 = fixture("ModemManager.service.v07.dict.zst");
    let dict =
        DictionaryHandle::from_serialized_or_raw_content(v07.dict.as_deref().unwrap()).unwrap();
    let leading = b"a leading frame";
    let mut input =
        crate::encoding::compress_slice_to_vec(leading, crate::encoding::CompressionLevel::Fastest);
    let rfc_len = input.len();
    input.extend_from_slice(&v07.frame);
    let mut out = Vec::new();
    StreamingDecoder::new_with_dictionary_handle(&input[..], &dict)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    let mut expected = leading.to_vec();
    expected.extend_from_slice(&v07.content);
    assert!(out == expected);

    // The v0.7 frame cut short: its bytes are not handed out.
    let cut = &input[..input.len() - 1];
    assert!(cut.len() > rfc_len);
    let mut out = Vec::new();
    assert!(
        StreamingDecoder::new_with_dictionary_handle(cut, &dict)
            .unwrap()
            .read_to_end(&mut out)
            .is_err()
    );
    assert_eq!(out, leading);
}

/// Output a caller does not collect stays in the buffer however far past the
/// window it grows, and all of it is handed out once collected.
#[cfg(feature = "std")]
#[test]
fn output_held_back_grows_the_buffer() {
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    let fixture = fixture("z000000x8.v07.l1.zst");
    let mut decoder = FrameDecoder::new();
    let mut src = &fixture.frame[..];
    decoder.reset(&mut src).unwrap();
    decoder
        .decode_blocks(&mut src, BlockDecodingStrategy::All)
        .unwrap();
    assert!(decoder.collect().unwrap() == fixture.content);
}

/// Damaged frames are refused or decode to some output, but never panic,
/// wherever the damage lands: random bytes overwritten, through every format
/// and both decoding paths.
#[cfg(feature = "std")]
#[test]
fn overwritten_bytes_never_panic() {
    use crate::decoding::FrameDecoder;
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for fixture in fixtures().into_iter().filter(|f| f.frame.len() < 16384) {
        let dict = fixture.legacy_dict();
        let header = read_header(&fixture.frame).unwrap().size;
        for _ in 0..64 {
            let mut frame = fixture.frame.clone();
            for _ in 0..(next() % 4) + 1 {
                let at = header + (next() as usize) % (frame.len() - header);
                frame[at] = next() as u8;
            }
            let _whole = decode_verified(&frame, dict).is_ok();
            let mut decoder = FrameDecoder::new();
            let mut target = vec![0u8; 4096];
            let mut src = &frame[..];
            while let Ok((read, written)) = decoder.decode_from_to(src, &mut target) {
                if read == 0 && written == 0 {
                    break;
                }
                src = &src[read..];
            }
        }
    }
}

/// An RLE block repeats its byte in v0.7; v0.5 and v0.6 reserve the type,
/// and their decoders refuse it (`ZSTDv0{5,6}_decompressFrame`).
#[test]
fn rle_blocks_repeat_their_byte_in_v07_only() {
    for magic in [0x25u8, 0x26, 0x27] {
        let mut frame = vec![magic, 0xB5, 0x2F, 0xFD, 0x00];
        if magic == 0x27 {
            frame.push(0x00);
        }
        frame.extend_from_slice(&[0x80, 0, 5, b'z', 0xC0, 0, 0]);
        let decoded = decode_to_vec(&frame, LegacyDict::None);
        if magic == 0x27 {
            let (decoded, consumed) = decoded.unwrap();
            assert_eq!(decoded, b"zzzzz");
            assert_eq!(consumed, frame.len());
        } else {
            assert!(
                matches!(decoded, Err(Error::Corrupted { .. })),
                "{magic:#x}: {decoded:?}"
            );
        }
    }
}

/// A block may not decode past the largest block size, whatever its header
/// claims.
#[test]
fn a_block_past_the_largest_size_is_refused() {
    let mut frame = vec![0x27, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    // An RLE block of 128 KiB + 1.
    let size = BLOCK_SIZE_MAX + 1;
    frame.extend_from_slice(&[
        0x80 | (size >> 16) as u8,
        (size >> 8) as u8,
        size as u8,
        b'z',
    ]);
    frame.extend_from_slice(&[0xC0, 0, 0]);
    assert!(matches!(
        decode_to_vec(&frame, LegacyDict::None),
        Err(Error::Corrupted { .. })
    ));
}

/// The declared content size does not bound the output: the v0.6.2 command
/// line declares its first chunk's size, and the blocks are what decode.
#[cfg(feature = "std")]
#[test]
fn the_declared_content_size_is_not_enforced() {
    // v0.6 with a one-byte content size of 4 and a raw block of 5.
    let frame = [
        0x26, 0xB5, 0x2F, 0xFD, 0x40, 4, 0x40, 0, 5, b'z', b'z', b'z', b'z', b'z', 0xC0, 0, 0,
    ];
    assert_eq!(declared_content_size(&frame), Some(4));
    assert_eq!(
        decode_to_vec(&frame, LegacyDict::None).unwrap(),
        (b"zzzzz".to_vec(), frame.len())
    );
    // A v0.6 archive from that command line, past 128 KiB, declares 128 KiB.
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name == "z000000.v06.l1.zst")
        .unwrap();
    assert_eq!(declared_content_size(&fixture.frame), Some(128 * 1024));
    assert_eq!(fixture.content.len(), 224_787);
}
