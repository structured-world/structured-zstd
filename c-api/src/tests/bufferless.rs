//! The buffer-less and block-level API. Expected values marked "upstream"
//! were read from `libzstd` 1.5.7 driven the same way.

use super::experimental::code;
use crate::bufferless::{
    ZSTD_compressBegin, ZSTD_compressBegin_advanced, ZSTD_compressBegin_usingCDict,
    ZSTD_compressBegin_usingDict, ZSTD_compressBlock, ZSTD_compressContinue, ZSTD_compressEnd,
    ZSTD_copyCCtx, ZSTD_copyDCtx, ZSTD_decompressBegin, ZSTD_decompressBegin_usingDDict,
    ZSTD_decompressBegin_usingDict, ZSTD_decompressBlock, ZSTD_decompressContinue,
    ZSTD_getBlockSize, ZSTD_insertBlock, ZSTD_nextInputType, ZSTD_nextSrcSizeToDecompress,
};
use crate::cdict::{ZSTD_createCDict, ZSTD_createDDict, ZSTD_freeCDict, ZSTD_freeDDict};
use crate::context::{
    ZSTD_CCtx, ZSTD_DCtx, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_freeCCtx, ZSTD_freeDCtx,
};
use crate::error::{ZSTD_ErrorCode, ZSTD_isError};
use crate::params::{ZSTD_CCtx_setParameter, ZSTD_getParams};

const DICT: &[u8] = include_bytes!("../../../zstd/dict_tests/dictionary");

/// Text-like input, changing every thousand bytes, as the upstream block
/// fixture was made from.
fn fox(len: usize) -> Vec<u8> {
    let phrase = b"the quick brown fox jumps over the lazy dog ";
    (0..len)
        .map(|i| phrase[i % phrase.len()] ^ (i / 1000) as u8)
        .collect()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// One step of a buffer-less decode: the input type and size asked for, and
/// what the step returned.
fn step(dctx: *mut ZSTD_DCtx, src: &[u8], out: &mut [u8]) -> (u32, usize, usize) {
    unsafe {
        let kind = ZSTD_nextInputType(dctx);
        let want = ZSTD_nextSrcSizeToDecompress(dctx);
        let rc = ZSTD_decompressContinue(dctx, out.as_mut_ptr(), out.len(), src.as_ptr(), want);
        (kind, want, rc)
    }
}

/// Decode every frame of `src` the buffer-less way, beginning each with
/// `begin`, and return the output.
fn decode_bufferless(
    dctx: *mut ZSTD_DCtx,
    src: &[u8],
    begin: impl Fn(*mut ZSTD_DCtx) -> usize,
) -> Result<Vec<u8>, usize> {
    let mut output = Vec::new();
    let mut out = vec![0u8; 1 << 17];
    let mut pos = 0;
    while pos < src.len() {
        assert_eq!(begin(dctx), 0);
        loop {
            let want = unsafe { ZSTD_nextSrcSizeToDecompress(dctx) };
            if want == 0 {
                break;
            }
            let rc = unsafe {
                ZSTD_decompressContinue(
                    dctx,
                    out.as_mut_ptr(),
                    out.len(),
                    src[pos..].as_ptr(),
                    want,
                )
            };
            if ZSTD_isError(rc) != 0 {
                return Err(rc);
            }
            output.extend_from_slice(&out[..rc]);
            pos += want;
        }
    }
    Ok(output)
}

/// A context copied with a pledge of 0 takes input of any size and leaves
/// the content size out, as upstream reads 0 there and its own callers rely
/// on (tests/fuzzer.c:2450, 4885).
#[test]
fn copy_cctx_reads_zero_as_unknown() {
    let src = ZSTD_createCCtx();
    let dst = ZSTD_createCCtx();
    let mut out = [0u8; 64];
    unsafe {
        assert_eq!(ZSTD_compressBegin(src, 3), 0);
        assert_eq!(ZSTD_copyCCtx(dst, src, 0), 0);
        let n = ZSTD_compressEnd(dst, out.as_mut_ptr(), out.len(), b"bytes".as_ptr(), 5);
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        assert_eq!(
            crate::simple::ZSTD_getFrameContentSize(out.as_ptr(), n),
            crate::params::CONTENTSIZE_UNKNOWN
        );
        ZSTD_freeCCtx(dst);
        ZSTD_freeCCtx(src);
    }
}

/// A context copied onto itself begins its own frame again, and one copied
/// onto itself on the decoding side is left as it is.
#[test]
fn copy_onto_itself() {
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut out = [0u8; 64];
    unsafe {
        assert_eq!(ZSTD_compressBegin(cctx, 3), 0);
        assert_eq!(ZSTD_copyCCtx(cctx, cctx, 5), 0);
        let n = ZSTD_compressEnd(cctx, out.as_mut_ptr(), out.len(), b"bytes".as_ptr(), 5);
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        assert_eq!(crate::simple::ZSTD_getFrameContentSize(out.as_ptr(), n), 5);

        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        ZSTD_copyDCtx(dctx, dctx);
        let decoded = decode_bufferless(dctx, &out[..n], |d| ZSTD_decompressBegin(d)).unwrap();
        assert_eq!(decoded, b"bytes");
        ZSTD_freeDCtx(dctx);
        ZSTD_freeCCtx(cctx);
    }
}

/// A run of blocks has no frame window, so the streaming ceiling does not
/// bind it: a block may match an inserted one further back than the
/// ceiling, as upstream's block API allows.
#[test]
fn block_runs_ignore_the_streaming_window_ceiling() {
    // Sixteen symbols: compressible, with no long repeats inside the block,
    // so its copy matches the first one 3000 bytes back.
    let mut state = 0x2468_ACE1u32;
    let stored: Vec<u8> = (0..3000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            b'a' + (state % 16) as u8
        })
        .collect();
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut block = vec![0u8; 8192];
    let mut out = vec![0u8; 8192];
    unsafe {
        assert_eq!(ZSTD_compressBegin(cctx, 3), 0);
        // The decoder takes this first block as stored bytes: the same
        // history, through `ZSTD_insertBlock`.
        let n = ZSTD_compressBlock(
            cctx,
            block.as_mut_ptr(),
            block.len(),
            stored.as_ptr(),
            stored.len(),
        );
        assert_eq!(ZSTD_isError(n), 0);
        let n = ZSTD_compressBlock(
            cctx,
            block.as_mut_ptr(),
            block.len(),
            stored.as_ptr(),
            stored.len(),
        );
        assert!(n > 0 && ZSTD_isError(n) == 0, "{:?}", code(n));

        assert_eq!(crate::params::ZSTD_DCtx_setMaxWindowSize(dctx, 2048), 0);
        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        assert_eq!(
            ZSTD_insertBlock(dctx, stored.as_ptr(), stored.len()),
            stored.len()
        );
        let m = ZSTD_decompressBlock(dctx, out.as_mut_ptr(), out.len(), block.as_ptr(), n);
        assert_eq!(m, stored.len(), "{:?}", code(m));
        assert_eq!(&out[..m], &stored[..]);
        ZSTD_freeDCtx(dctx);
        ZSTD_freeCCtx(cctx);
    }
}

/// A block that compressed but did not fit `dst` is already history the next
/// block could match against; the session ends, so a retry cannot emit a
/// block that refers to one the caller never received.
#[test]
fn compress_block_into_too_small_dst_ends_the_session() {
    let block = fox(4096);
    let cctx = ZSTD_createCCtx();
    let mut small = [0u8; 8];
    let mut big = vec![0u8; 8192];
    unsafe {
        assert_eq!(ZSTD_compressBegin(cctx, 3), 0);
        let rc = ZSTD_compressBlock(
            cctx,
            small.as_mut_ptr(),
            small.len(),
            block.as_ptr(),
            block.len(),
        );
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
        let rc = ZSTD_compressBlock(
            cctx,
            big.as_mut_ptr(),
            big.len(),
            block.as_ptr(),
            block.len(),
        );
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_stage_wrong);
        ZSTD_freeCCtx(cctx);
    }
}

/// A context copied from one part-way through a frame starts a frame of its
/// own: the copy carries the settings and the dictionary, not a stage its
/// decoder never reached.
#[test]
fn copy_dctx_mid_frame_starts_afresh() {
    let frame: &[u8] = &[
        0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x18, 0, 0, b'a', b'b', b'c', 0x11, 0, 0, b'd', b'e',
    ];
    let src = ZSTD_createDCtx();
    let dst = ZSTD_createDCtx();
    let mut out = [0u8; 64];
    unsafe {
        assert_eq!(ZSTD_decompressBegin(src), 0);
        // Through the frame header: `src` now waits for a block header.
        let mut pos = 0;
        for _ in 0..2 {
            pos += step(src, &frame[pos..], &mut out).1;
        }
        assert_eq!(ZSTD_nextSrcSizeToDecompress(src), 3);
        ZSTD_copyDCtx(dst, src);
        assert_eq!(ZSTD_nextSrcSizeToDecompress(dst), 5);
        let mut decoded = Vec::new();
        let mut at = 0;
        loop {
            let want = ZSTD_nextSrcSizeToDecompress(dst);
            if want == 0 {
                break;
            }
            let rc = ZSTD_decompressContinue(
                dst,
                out.as_mut_ptr(),
                out.len(),
                frame[at..].as_ptr(),
                want,
            );
            assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
            decoded.extend_from_slice(&out[..rc]);
            at += want;
        }
        assert_eq!(decoded, b"abcde");
        ZSTD_freeDCtx(dst);
        ZSTD_freeDCtx(src);
    }
}

/// The decoder asks for the parts of a frame in upstream's order and sizes,
/// and hands each block's output back at once.
#[test]
fn decompress_continue_asks_like_upstream() {
    // Two raw blocks under a 1 KiB window, no checksum; then a skippable
    // frame of variant 2 with five bytes.
    let frame: &[u8] = &[
        0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x18, 0, 0, b'a', b'b', b'c', 0x11, 0, 0, b'd', b'e',
    ];
    let skippable: &[u8] = &[
        0x52, 0x2A, 0x4D, 0x18, 5, 0, 0, 0, b'v', b'w', b'x', b'y', b'z',
    ];
    let dctx = ZSTD_createDCtx();
    let mut out = [0u8; 64];
    unsafe {
        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        let mut pos = 0;
        // (input type, size asked, returned) as upstream reports them.
        for expected in [
            (0, 5, 0),
            (0, 1, 0),
            (1, 3, 0),
            (2, 3, 3),
            (1, 3, 0),
            (3, 2, 2),
        ] {
            let got = step(dctx, &frame[pos..], &mut out);
            assert_eq!(got, expected);
            if got.2 > 0 {
                let text: &[u8] = if got.2 == 3 { b"abc" } else { b"de" };
                assert_eq!(&out[..got.2], text);
            }
            pos += got.1;
        }
        assert_eq!(ZSTD_nextSrcSizeToDecompress(dctx), 0);

        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        let mut pos = 0;
        for expected in [(0, 5, 0), (5, 3, 0), (5, 5, 0)] {
            let got = step(dctx, &skippable[pos..], &mut out);
            assert_eq!(got, expected);
            pos += got.1;
        }
        assert_eq!(ZSTD_nextSrcSizeToDecompress(dctx), 0);

        // A size other than the one asked for is refused.
        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        let rc = ZSTD_decompressContinue(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), 4);
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
        ZSTD_freeDCtx(dctx);
    }
}

/// A checksummed frame decodes through the steps and its checksum is held to
/// the output (upstream: `checksum_wrong` on a bad one).
#[test]
fn decompress_continue_checks_the_checksum() {
    let input = fox(300_000);
    let cctx = ZSTD_createCCtx();
    let mut frame = vec![0u8; input.len() + 1024];
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 201, 1), 0);
        let n = crate::context::ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(n), 0);
        frame.truncate(n);
        ZSTD_freeCCtx(cctx);
    }
    let dctx = ZSTD_createDCtx();
    let begin = |dctx| unsafe { ZSTD_decompressBegin(dctx) };
    assert_eq!(decode_bufferless(dctx, &frame, begin).unwrap(), input);
    let last = frame.len() - 1;
    frame[last] ^= 1;
    assert_eq!(
        code(decode_bufferless(dctx, &frame, begin).unwrap_err()),
        ZSTD_ErrorCode::ZSTD_error_checksum_wrong
    );
    unsafe { ZSTD_freeDCtx(dctx) };
}

/// Feed `parts` through the buffer-less compression steps and return the frame.
fn compress_parts(cctx: *mut ZSTD_CCtx, parts: &[&[u8]]) -> Result<Vec<u8>, usize> {
    let mut frame = Vec::new();
    let mut out = vec![0u8; 1 << 18];
    for (i, part) in parts.iter().enumerate() {
        let rc = unsafe {
            if i + 1 == parts.len() {
                ZSTD_compressEnd(cctx, out.as_mut_ptr(), out.len(), part.as_ptr(), part.len())
            } else {
                ZSTD_compressContinue(cctx, out.as_mut_ptr(), out.len(), part.as_ptr(), part.len())
            }
        };
        if ZSTD_isError(rc) != 0 {
            return Err(rc);
        }
        frame.extend_from_slice(&out[..rc]);
    }
    Ok(frame)
}

fn decompress_all(frame: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let n = unsafe {
        crate::simple::ZSTD_decompress(out.as_mut_ptr(), out.len(), frame.as_ptr(), frame.len())
    };
    assert_eq!(n, len, "{:?}", code(n));
    out
}

/// A frame compressed in steps decodes to what went in, whatever the steps;
/// a pledge is held to and an unbegun context refused.
#[test]
fn compress_continue_builds_a_frame() {
    let input = fox(400_000);
    let cctx = ZSTD_createCCtx();
    unsafe {
        let rc = ZSTD_compressContinue(cctx, core::ptr::null_mut(), 0, core::ptr::null(), 0);
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_stage_wrong);

        assert_eq!(ZSTD_compressBegin(cctx, 5), 0);
        let frame = compress_parts(
            cctx,
            &[&input[..1], &input[1..150_000], &input[150_000..], &[]],
        )
        .unwrap();
        assert_eq!(decompress_all(&frame, input.len()), input);

        // The header goes out with the first step, even an empty one.
        assert_eq!(ZSTD_compressBegin(cctx, 5), 0);
        let mut out = [0u8; 64];
        let n = ZSTD_compressContinue(cctx, out.as_mut_ptr(), out.len(), core::ptr::null(), 0);
        assert!(n >= 6, "{n}");
        assert_eq!(&out[..4], &0xFD2F_B528u32.to_le_bytes());
        let m = ZSTD_compressEnd(
            cctx,
            out[n..].as_mut_ptr(),
            out.len() - n,
            core::ptr::null(),
            0,
        );
        assert_eq!(decompress_all(&out[..n + m], 0), b"");

        let params = ZSTD_getParams(3, 10, 0);
        assert_eq!(
            ZSTD_compressBegin_advanced(cctx, core::ptr::null(), 0, params, 10),
            0
        );
        assert_eq!(
            code(compress_parts(cctx, &[&input[..20]]).unwrap_err()),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );
        ZSTD_freeCCtx(cctx);
    }
}

/// The dictionary begins compress against the dictionary.
#[test]
fn compress_begin_with_dictionaries() {
    let input = DICT[DICT.len() - 3000..].to_vec();
    let cctx = ZSTD_createCCtx();
    let cdict = unsafe { ZSTD_createCDict(DICT.as_ptr(), DICT.len(), 5) };
    let restore = |frame: &[u8]| unsafe {
        let dctx = ZSTD_createDCtx();
        let mut out = vec![0u8; input.len()];
        let n = crate::attach::ZSTD_decompress_usingDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
            DICT.as_ptr(),
            DICT.len(),
        );
        ZSTD_freeDCtx(dctx);
        assert_eq!(n, input.len(), "{:?}", code(n));
        out
    };
    unsafe {
        assert_eq!(
            ZSTD_compressBegin_usingDict(cctx, DICT.as_ptr(), DICT.len(), 5),
            0
        );
        let frame = compress_parts(cctx, &[&input]).unwrap();
        assert!(frame.len() < input.len() / 4, "{}", frame.len());
        assert_eq!(restore(&frame), input);

        assert_eq!(ZSTD_compressBegin_usingCDict(cctx, cdict), 0);
        let frame = compress_parts(cctx, &[&input[..1000], &input[1000..]]).unwrap();
        assert!(frame.len() < input.len() / 4, "{}", frame.len());
        assert_eq!(restore(&frame), input);
        assert_eq!(
            code(ZSTD_compressBegin_usingCDict(cctx, core::ptr::null())),
            ZSTD_ErrorCode::ZSTD_error_dictionary_wrong
        );
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCDict(cdict);
    }
}

/// Blocks upstream's `ZSTD_compressBlock` produced decode with ours.
#[test]
fn decompress_block_reads_upstream_blocks() {
    let input = fox(3 * 4096);
    // upstream, level 3: `ZSTD_compressBlock` over three 4 KiB blocks.
    let blocks = [
        "a28c261860a5dda31864e4934f3ef9b4117349922409d40803009c07062020317dec331bb72a4de009c6dda795992bd66c0ea4667841e0b4457015a31066516f97aa322cf44addca01538918d6531831222230471a9076debe292a72108324cb6942178ea4dab9d9455bbead10bf2fe4faa9e51dde090cd3472dbf92e98923888fda9ac80be3404f0cbb4d7afaa5313b27a672b9a715fc0d2d574f0c070a0079ea680925bcdc4f1519ccf6c9fd54ad30dbb60d0a48a8b66a3f5576d0ce32",
        "d2c91f1960a5dd4870a055011b6cb02123238520088284240c0080a0018a88c62a018160a342972cc2b10857d23b1ba0445394b960a9b6199688b18547d0bc24a3ca08d6ce4672b0431a65392050493b3cab0dab8fc1bcfa34f447b1dc27691f27da3e0fefc2379ab9ce87cce8f4ed923febf47229408a913b5e1caab989d5b5c901090091a74858a8f673830287d9bedc4f551a663bc8fd54e130db4ab80242",
        "528a201970a5796079a9b4c61f79450dc0b64c68f5ffff9584ffff4b0db619deb79ea97ae04223ea16f68a691edd5ef6e0ac01029ee9f6ec3eb220202c867e79c2e35952650f17ff38d59f8d0d43d660e857b2729a5ffa122c0531a812912d32d654a6b1ae2982e4ca1f697e751250912cabe8409060aa84ea11a26089b87310eaae22860708006e404042b585b941a5733f5571986d96fba9ba61b696640584",
    ];
    let dctx = ZSTD_createDCtx();
    let mut out = vec![0u8; 4096];
    unsafe {
        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        for (i, hex) in blocks.iter().enumerate() {
            let block = unhex(hex);
            let n = ZSTD_decompressBlock(
                dctx,
                out.as_mut_ptr(),
                out.len(),
                block.as_ptr(),
                block.len(),
            );
            assert_eq!(n, 4096, "block {i}: {:?}", code(n));
            assert_eq!(out, input[i * 4096..(i + 1) * 4096], "block {i}");
        }
        ZSTD_freeDCtx(dctx);
    }
}

/// Our blocks round-trip through the block decoder, the ones that do not
/// compress by way of `ZSTD_insertBlock`, and later blocks match against
/// both kinds.
#[test]
fn block_api_round_trips() {
    let mut input = fox(4 * 4096);
    // The third block is noise, which no encoder shortens.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for byte in &mut input[2 * 4096..3 * 4096] {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 24) as u8;
    }
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut block = vec![0u8; 8192];
    let mut out = vec![0u8; 4096];
    unsafe {
        assert_eq!(ZSTD_compressBegin(cctx, 3), 0);
        assert_eq!(ZSTD_getBlockSize(cctx), 131_072);
        assert_eq!(ZSTD_decompressBegin(dctx), 0);
        let mut stored = 0;
        for chunk in input.chunks(4096) {
            let n = ZSTD_compressBlock(
                cctx,
                block.as_mut_ptr(),
                block.len(),
                chunk.as_ptr(),
                chunk.len(),
            );
            assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
            if n == 0 {
                stored += 1;
                assert_eq!(
                    ZSTD_insertBlock(dctx, chunk.as_ptr(), chunk.len()),
                    chunk.len()
                );
                continue;
            }
            let m = ZSTD_decompressBlock(dctx, out.as_mut_ptr(), out.len(), block.as_ptr(), n);
            assert_eq!(m, chunk.len(), "{:?}", code(m));
            assert_eq!(out, chunk);
        }
        assert_eq!(stored, 1, "only the noise block is stored");

        let too_big = vec![0u8; 131_073];
        let rc = ZSTD_compressBlock(
            cctx,
            block.as_mut_ptr(),
            block.len(),
            too_big.as_ptr(),
            too_big.len(),
        );
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);

        let params = ZSTD_getParams(3, 1000, 0);
        assert_eq!(
            ZSTD_compressBegin_advanced(cctx, core::ptr::null(), 0, params, 0),
            0
        );
        assert_eq!(ZSTD_getBlockSize(cctx), 1024);
        ZSTD_freeCCtx(cctx);
        ZSTD_freeDCtx(dctx);
    }
}

/// A context begun and not yet used copies into another, which then makes
/// the same frame; once compressing has started it does not.
#[test]
fn copy_cctx_repeats_the_begin() {
    let input = DICT[DICT.len() - 3000..].to_vec();
    let prepared = ZSTD_createCCtx();
    let copy = ZSTD_createCCtx();
    unsafe {
        assert_eq!(
            ZSTD_compressBegin_usingDict(prepared, DICT.as_ptr(), DICT.len(), 7),
            0
        );
        assert_eq!(ZSTD_copyCCtx(copy, prepared, input.len() as u64), 0);
        assert_eq!(ZSTD_copyCCtx(copy, prepared, 0), 0);
        let from_copy = compress_parts(copy, &[&input]).unwrap();
        let from_prepared = compress_parts(prepared, &[&input]).unwrap();
        assert_eq!(from_copy, from_prepared);

        assert_eq!(ZSTD_compressBegin(prepared, 3), 0);
        let mut out = [0u8; 64];
        ZSTD_compressContinue(prepared, out.as_mut_ptr(), out.len(), b"x".as_ptr(), 1);
        assert_eq!(
            code(ZSTD_copyCCtx(copy, prepared, 0)),
            ZSTD_ErrorCode::ZSTD_error_stage_wrong
        );
        ZSTD_freeCCtx(prepared);
        ZSTD_freeCCtx(copy);
    }
}

/// Dictionary begins decode dictionary frames, and a prepared context copies
/// with its dictionary.
#[test]
fn decompress_begin_with_dictionaries_and_copy() {
    let input = DICT[DICT.len() - 3000..].to_vec();
    let frame = unsafe {
        let cctx = ZSTD_createCCtx();
        assert_eq!(
            ZSTD_compressBegin_usingDict(cctx, DICT.as_ptr(), DICT.len(), 3),
            0
        );
        let frame = compress_parts(cctx, &[&input]).unwrap();
        ZSTD_freeCCtx(cctx);
        frame
    };
    let ddict = unsafe { ZSTD_createDDict(DICT.as_ptr(), DICT.len()) };
    let prepared = ZSTD_createDCtx();
    let copy = ZSTD_createDCtx();
    unsafe {
        let with_dict = |dctx| ZSTD_decompressBegin_usingDict(dctx, DICT.as_ptr(), DICT.len());
        assert_eq!(
            decode_bufferless(prepared, &frame, with_dict).unwrap(),
            input
        );
        let with_ddict = |dctx| ZSTD_decompressBegin_usingDDict(dctx, ddict);
        assert_eq!(
            decode_bufferless(prepared, &frame, with_ddict).unwrap(),
            input
        );
        let without = |dctx| ZSTD_decompressBegin(dctx);
        assert_eq!(
            code(decode_bufferless(prepared, &frame, without).unwrap_err()),
            ZSTD_ErrorCode::ZSTD_error_dictionary_wrong
        );

        assert_eq!(
            ZSTD_decompressBegin_usingDict(prepared, DICT.as_ptr(), DICT.len()),
            0
        );
        ZSTD_copyDCtx(copy, prepared);
        // `copy` is begun already: decode without beginning again.
        assert_eq!(decode_bufferless(copy, &frame, |_| 0).unwrap(), input);
        ZSTD_freeDCtx(prepared);
        ZSTD_freeDCtx(copy);
        ZSTD_freeDDict(ddict);
    }
}
