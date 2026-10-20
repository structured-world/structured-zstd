//! v0.5-v0.7 frames through the C ABI, as libzstd 1.5.7 reads them by
//! default: the frame queries, the one-shot decoders and the stream, on the
//! frames the v0.5.1, v0.6.2 and v0.7.5 command lines wrote
//! (`zstd/legacy_frames`).

use std::path::{Path, PathBuf};

use crate::attach::ZSTD_decompress_usingDict;
use crate::cdict::{ZSTD_createDDict, ZSTD_freeDDict};
use crate::context::{ZSTD_createDCtx, ZSTD_freeDCtx};
use crate::error::{ZSTD_ErrorCode, ZSTD_getErrorCode, ZSTD_isError};
use crate::frame::{ZSTD_decompressBound, ZSTD_frameHeaderSize, ZSTD_isFrame};
use crate::simple::{ZSTD_decompress, ZSTD_findFrameCompressedSize, ZSTD_getFrameContentSize};
use crate::streaming::{
    ZSTD_createDStream, ZSTD_decompressStream, ZSTD_freeDStream, ZSTD_inBuffer,
    ZSTD_initDStream_usingDDict, ZSTD_outBuffer,
};

struct Fixture {
    name: String,
    frame: Vec<u8>,
    content: Vec<u8>,
    dict: Option<Vec<u8>>,
}

fn codec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../zstd")
}

/// Every fixture, with the file it decodes to and the dictionary it needs.
fn fixtures() -> Vec<Fixture> {
    let dir = codec_dir().join("legacy_frames");
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
            let (stem, tag) = ["v05", "v06", "v07"]
                .iter()
                .find_map(|tag| {
                    name.split_once(&format!(".{tag}."))
                        .map(|(stem, _)| (stem.to_string(), *tag))
                })
                .expect("fixture name carries its version");
            let is_dict = name.contains(".dict");
            // `<file>x<N>` is the file N times over.
            let (file, times) = stem
                .rsplit_once('x')
                .and_then(|(file, n)| Some((file, n.parse::<usize>().ok()?)))
                .unwrap_or((stem.as_str(), 1));
            let source = if is_dict {
                codec_dir().join("dict_tests/files").join(file)
            } else {
                codec_dir().join("decodecorpus_files").join(file)
            };
            Fixture {
                frame: std::fs::read(dir.join(&name)).unwrap(),
                content: std::fs::read(source).unwrap().repeat(times),
                dict: is_dict
                    .then(|| std::fs::read(dir.join(format!("dictionary.{tag}"))).unwrap()),
                name,
            }
        })
        .collect()
}

/// The frame queries: a frame, its length through the end block, a bound of
/// whole blocks, and the declared size or unknown.
#[test]
fn frame_queries_read_these_frames() {
    for fixture in fixtures() {
        let frame = &fixture.frame;
        assert_eq!(
            unsafe { ZSTD_isFrame(frame.as_ptr(), frame.len()) },
            1,
            "{}",
            fixture.name
        );
        assert_eq!(
            unsafe { ZSTD_findFrameCompressedSize(frame.as_ptr(), frame.len()) },
            frame.len(),
            "{}",
            fixture.name
        );
        // A frame `ZSTD_isFrame` accepts has a header to measure.
        let header = unsafe { ZSTD_frameHeaderSize(frame.as_ptr(), frame.len()) };
        assert_eq!(ZSTD_isError(header), 0, "{}", fixture.name);
        assert!((5..=18).contains(&header), "{}: {header}", fixture.name);
        let bound = unsafe { ZSTD_decompressBound(frame.as_ptr(), frame.len()) };
        assert!(bound >= fixture.content.len() as u64, "{}", fixture.name);
        assert_eq!(bound % (128 * 1024), 0, "{}", fixture.name);
        // Cut short, the frame's length cannot be told.
        let cut = unsafe { ZSTD_findFrameCompressedSize(frame.as_ptr(), frame.len() - 1) };
        assert_eq!(
            ZSTD_getErrorCode(cut),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong,
            "{}",
            fixture.name
        );
    }
    let size = |name: &str| {
        let fixture = fixtures().into_iter().find(|f| f.name == name).unwrap();
        unsafe { ZSTD_getFrameContentSize(fixture.frame.as_ptr(), fixture.frame.len()) }
    };
    // v0.5 declares no size.
    assert_eq!(size("abc.txt.v05.l1.zst"), u64::MAX);
    // The v0.6.2 command line declares its first 128 KiB chunk.
    assert_eq!(size("z000000.v06.l1.zst"), 128 * 1024);
}

/// `ZSTD_decompress` decodes a frame that needs no dictionary;
/// `ZSTD_decompress_usingDict` one that does, with the dictionary file as
/// that version's command line trained it; one byte short is too small.
#[test]
fn one_shot_decoders_decode_these_frames() {
    let dctx = ZSTD_createDCtx();
    for fixture in fixtures() {
        let mut out = vec![0u8; fixture.content.len()];
        let written = match &fixture.dict {
            None => unsafe {
                ZSTD_decompress(
                    out.as_mut_ptr(),
                    out.len(),
                    fixture.frame.as_ptr(),
                    fixture.frame.len(),
                )
            },
            Some(dict) => unsafe {
                ZSTD_decompress_usingDict(
                    dctx,
                    out.as_mut_ptr(),
                    out.len(),
                    fixture.frame.as_ptr(),
                    fixture.frame.len(),
                    dict.as_ptr(),
                    dict.len(),
                )
            },
        };
        assert_eq!(
            ZSTD_isError(written),
            0,
            "{}: {:?}",
            fixture.name,
            ZSTD_getErrorCode(written)
        );
        assert_eq!(written, fixture.content.len(), "{}", fixture.name);
        assert!(out == fixture.content, "{}", fixture.name);
        if fixture.dict.is_none() && !out.is_empty() {
            let short = unsafe {
                ZSTD_decompress(
                    out.as_mut_ptr(),
                    out.len() - 1,
                    fixture.frame.as_ptr(),
                    fixture.frame.len(),
                )
            };
            assert_eq!(
                ZSTD_getErrorCode(short),
                ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall,
                "{}",
                fixture.name
            );
        }
    }
    unsafe { ZSTD_freeDCtx(dctx) };
}

/// A v0.7 frame naming a dictionary is refused without it, as the wrong
/// dictionary.
#[test]
fn a_v07_frame_without_its_dictionary_is_refused() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name.ends_with(".v07.dict.zst"))
        .unwrap();
    let mut out = vec![0u8; fixture.content.len()];
    let rc = unsafe {
        ZSTD_decompress(
            out.as_mut_ptr(),
            out.len(),
            fixture.frame.as_ptr(),
            fixture.frame.len(),
        )
    };
    assert_eq!(
        ZSTD_getErrorCode(rc),
        ZSTD_ErrorCode::ZSTD_error_dictionary_wrong
    );
}

/// Decode `input` through a stream referencing `dict`, giving it `step`
/// bytes of input and `room` bytes of output per call. Returns the output
/// and the last call's result.
fn stream(input: &[u8], dict: Option<&[u8]>, step: usize, room: usize) -> (Vec<u8>, usize) {
    let zds = ZSTD_createDStream();
    let ddict = dict.map(|dict| unsafe { ZSTD_createDDict(dict.as_ptr().cast(), dict.len()) });
    let rc = unsafe { ZSTD_initDStream_usingDDict(zds, ddict.unwrap_or(core::ptr::null_mut())) };
    assert_eq!(ZSTD_isError(rc), 0);
    let mut out = Vec::new();
    let mut buf = vec![0u8; room];
    let mut at = 0;
    let mut last = 0;
    let mut idle = 0;
    while at < input.len() || last != 0 {
        let end = (at + step).min(input.len());
        let mut inb = ZSTD_inBuffer {
            src: input[at..end].as_ptr().cast(),
            size: end - at,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: buf.as_mut_ptr().cast(),
            size: buf.len(),
            pos: 0,
        };
        last = unsafe { ZSTD_decompressStream(zds, &mut outb, &mut inb) };
        if ZSTD_isError(last) != 0 {
            break;
        }
        at += inb.pos;
        out.extend_from_slice(&buf[..outb.pos]);
        // Input all given and nothing moving: the stream wants no more.
        if inb.pos == 0 && outb.pos == 0 {
            idle += 1;
            if idle > 2 {
                break;
            }
        } else {
            idle = 0;
        }
    }
    if let Some(ddict) = ddict {
        unsafe { ZSTD_freeDDict(ddict) };
    }
    unsafe { ZSTD_freeDStream(zds) };
    (out, last)
}

/// The stream decodes each frame from input in small pieces into small
/// output, its dictionary referenced as a DDict, and finishes with 0.
#[test]
fn the_stream_decodes_these_frames_in_pieces() {
    for fixture in fixtures().into_iter().filter(|f| f.frame.len() < 64 * 1024) {
        let (out, last) = stream(&fixture.frame, fixture.dict.as_deref(), 7, 1000);
        assert_eq!(last, 0, "{}: {:?}", fixture.name, ZSTD_getErrorCode(last));
        assert!(out == fixture.content, "{}", fixture.name);
    }
}

/// One stream holding RFC 8878 frames and these, in turn, decodes them all.
#[test]
fn the_stream_decodes_these_frames_among_others() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.name == "z000001.v06.l19.zst")
        .unwrap();
    let rfc = codec::encoding::compress_to_vec(
        &b"between"[..],
        codec::encoding::CompressionLevel::Fastest,
    );
    let mut input = rfc.clone();
    input.extend_from_slice(&fixture.frame);
    input.extend_from_slice(&rfc);
    let mut expected = b"between".to_vec();
    expected.extend_from_slice(&fixture.content);
    expected.extend_from_slice(b"between");
    let (out, last) = stream(&input, None, 5, 333);
    assert_eq!(last, 0, "{:?}", ZSTD_getErrorCode(last));
    assert!(out == expected);
}
