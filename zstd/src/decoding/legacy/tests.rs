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
        pub(super) version: LegacyVersion,
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
                let source: PathBuf = if is_dict {
                    root().join("dict_tests/files").join(&stem)
                } else {
                    root().join("decodecorpus_files").join(&stem)
                };
                Fixture {
                    frame: std::fs::read(dir.join(&name)).unwrap(),
                    content: std::fs::read(&source).unwrap(),
                    dict: is_dict
                        .then(|| std::fs::read(dir.join(format!("dictionary.{rest}"))).unwrap()),
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
    let mut buf = Vec::new();
    let mut out = VecOutput {
        buf: &mut buf,
        start: 0,
        dropped: 0,
        limit: usize::MAX,
    };
    let consumed = decode_frame(frame, dict, &mut out)?;
    Ok((buf, consumed))
}

/// Each fixture names its own format and decodes byte-exact, taking exactly
/// its own bytes.
#[cfg(feature = "std")]
#[test]
fn fixtures_decode_to_their_content() {
    for fixture in fixtures() {
        assert_eq!(
            LegacyVersion::of(&fixture.frame),
            Some(fixture.version),
            "{}",
            fixture.name
        );
        let dict = fixture
            .dict
            .as_deref()
            .map_or(LegacyDict::None, LegacyDict::Raw);
        let (decoded, consumed) = decode_to_vec(&fixture.frame, dict)
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
        assert_eq!(consumed, fixture.frame.len(), "{}", fixture.name);
        assert!(
            decoded == fixture.content,
            "{} decodes to other bytes",
            fixture.name
        );
    }
}

/// Decoding into a caller's slice gives the same bytes, and a slice one byte
/// short is refused as too small rather than written past.
#[cfg(feature = "std")]
#[test]
fn fixtures_decode_into_a_slice() {
    for fixture in fixtures() {
        let dict = fixture
            .dict
            .as_deref()
            .map_or(LegacyDict::None, LegacyDict::Raw);
        let mut buf = vec![0u8; fixture.content.len()];
        let mut out = SliceOutput {
            buf: &mut buf,
            pos: 0,
        };
        decode_frame(&fixture.frame, dict, &mut out)
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.name));
        assert_eq!(out.pos, fixture.content.len(), "{}", fixture.name);
        assert!(buf == fixture.content, "{}", fixture.name);

        if let Some(short) = fixture.content.len().checked_sub(1) {
            let mut buf = vec![0u8; short];
            let mut out = SliceOutput {
                buf: &mut buf,
                pos: 0,
            };
            assert_eq!(
                decode_frame(&fixture.frame, dict, &mut out),
                Err(Error::OutputTooSmall),
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
        assert_eq!(
            decode_to_vec(&fixture.frame, LegacyDict::Raw(&other)).unwrap_err(),
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
        let dict = fixture
            .dict
            .as_deref()
            .map_or(LegacyDict::None, LegacyDict::Raw);
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
/// never panics; in a v0.7 frame with a checksum a change to the content is
/// caught.
#[cfg(feature = "std")]
#[test]
fn damaged_frames_never_panic() {
    for fixture in fixtures().into_iter().filter(|f| f.frame.len() < 4096) {
        let dict = fixture
            .dict
            .as_deref()
            .map_or(LegacyDict::None, LegacyDict::Raw);
        let mut frame = fixture.frame.clone();
        for byte in 0..frame.len() {
            for bit in 0..8 {
                frame[byte] ^= 1 << bit;
                let _ = decode_to_vec(&frame, dict);
                frame[byte] ^= 1 << bit;
            }
        }
    }
}

/// A v0.7 frame whose content checksum does not match is refused with the
/// checksum error.
#[cfg(all(feature = "std", feature = "hash"))]
#[test]
fn a_wrong_v07_checksum_is_refused() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name == "abc.txt.v07.l1.zst")
        .unwrap();
    let mut frame = fixture.frame.clone();
    // The end block's header holds the checksum in its low 22 bits.
    let last = frame.len() - 1;
    frame[last] ^= 1;
    assert!(matches!(
        decode_to_vec(&frame, LegacyDict::None),
        Err(Error::ChecksumMismatch { .. })
    ));
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

/// An RLE block repeats its byte, in every format.
#[test]
fn rle_blocks_repeat_their_byte() {
    for magic in [0x25u8, 0x26, 0x27] {
        let mut frame = vec![magic, 0xB5, 0x2F, 0xFD, 0x00];
        if magic == 0x27 {
            frame.push(0x00);
        }
        frame.extend_from_slice(&[0x80, 0, 5, b'z', 0xC0, 0, 0]);
        let (decoded, consumed) = decode_to_vec(&frame, LegacyDict::None).unwrap();
        assert_eq!(decoded, b"zzzzz");
        assert_eq!(consumed, frame.len());
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
    // v0.6 with a one-byte content size of 4 and an RLE block of 5.
    let frame = [
        0x26, 0xB5, 0x2F, 0xFD, 0x40, 4, 0x80, 0, 5, b'z', 0xC0, 0, 0,
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
