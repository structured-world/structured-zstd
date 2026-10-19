//! The stream init and reset family, the simple-argument streaming calls,
//! frame progression, and the `_advanced` creators.

use core::ffi::{c_int, c_void};

use super::experimental::code;
use crate::attach::{ZSTD_DCtx_refDDict, ZSTD_decompress_usingDict, ZSTD_getDictID_fromFrame};
use crate::cdict::{
    ZSTD_createCDict, ZSTD_createDDict, ZSTD_customMem, ZSTD_frameParameters, ZSTD_freeCDict,
    ZSTD_freeDDict,
};
use crate::context::{ZSTD_CCtx, ZSTD_DCtx, ZSTD_freeCCtx, ZSTD_freeDCtx};
use crate::error::{ZSTD_ErrorCode, ZSTD_isError};
use crate::frame::{ZSTD_FrameHeader, ZSTD_getFrameHeader};
use crate::params::{ZSTD_CCtx_setParameter, ZSTD_DCtx_setFormat, ZSTD_getParams};
use crate::streaming::{
    ZSTD_compressStream, ZSTD_compressStream2, ZSTD_compressStream2_simpleArgs, ZSTD_createCStream,
    ZSTD_createCStream_advanced, ZSTD_createDStream, ZSTD_createDStream_advanced,
    ZSTD_decompressStream_simpleArgs, ZSTD_getFrameProgression, ZSTD_inBuffer, ZSTD_initCStream,
    ZSTD_initCStream_advanced, ZSTD_initCStream_srcSize, ZSTD_initCStream_usingCDict,
    ZSTD_initCStream_usingCDict_advanced, ZSTD_initCStream_usingDict, ZSTD_initDStream,
    ZSTD_initDStream_usingDDict, ZSTD_initDStream_usingDict, ZSTD_outBuffer, ZSTD_resetCStream,
    ZSTD_resetDStream, ZSTD_toFlushNow,
};

const DICT: &[u8] = include_bytes!("../../../zstd/dict_tests/dictionary");

fn dict_sample() -> Vec<u8> {
    DICT[DICT.len() - 3000..].to_vec()
}

/// Feed `input` to `zcs` and end the frame, with room for all of the output.
fn end_frame(zcs: *mut ZSTD_CCtx, input: &[u8]) -> Result<Vec<u8>, usize> {
    let mut out = vec![0u8; input.len() + 4096];
    let mut inb = ZSTD_inBuffer {
        src: input.as_ptr().cast(),
        size: input.len(),
        pos: 0,
    };
    let mut outb = ZSTD_outBuffer {
        dst: out.as_mut_ptr().cast(),
        size: out.len(),
        pos: 0,
    };
    let rc = unsafe { ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2) };
    if ZSTD_isError(rc) != 0 {
        return Err(rc);
    }
    assert_eq!(rc, 0);
    out.truncate(outb.pos);
    Ok(out)
}

/// Feed `input` with a continue and end the frame with a second call, so the
/// frame is begun before the end and keeps the pledge it began with (an end
/// call that begins a frame takes its input's size instead).
fn continue_then_end(zcs: *mut ZSTD_CCtx, input: &[u8]) -> Result<Vec<u8>, usize> {
    let mut out = vec![0u8; input.len() + 4096];
    let mut inb = ZSTD_inBuffer {
        src: input.as_ptr().cast(),
        size: input.len(),
        pos: 0,
    };
    let mut outb = ZSTD_outBuffer {
        dst: out.as_mut_ptr().cast(),
        size: out.len(),
        pos: 0,
    };
    for end_op in [0, 2] {
        let rc = unsafe { ZSTD_compressStream2(zcs, &mut outb, &mut inb, end_op) };
        if ZSTD_isError(rc) != 0 {
            return Err(rc);
        }
    }
    out.truncate(outb.pos);
    Ok(out)
}

/// Decode `frame` through `zds` with the simple-argument call and small
/// buffers, so every call moves both positions a little.
fn decode_simple(zds: *mut ZSTD_DCtx, frame: &[u8]) -> Result<Vec<u8>, usize> {
    let mut restored = Vec::new();
    let mut chunk = [0u8; 97];
    let mut src_pos = 0;
    loop {
        let mut dst_pos = 0;
        let before = src_pos;
        let src_end = (src_pos + 61).min(frame.len());
        let rc = unsafe {
            ZSTD_decompressStream_simpleArgs(
                zds,
                chunk.as_mut_ptr().cast::<c_void>(),
                chunk.len(),
                &mut dst_pos,
                frame.as_ptr().cast::<c_void>(),
                src_end,
                &mut src_pos,
            )
        };
        if ZSTD_isError(rc) != 0 {
            return Err(rc);
        }
        restored.extend_from_slice(&chunk[..dst_pos]);
        if rc == 0 && src_pos == frame.len() {
            return Ok(restored);
        }
        // A call that moves neither position would repeat forever.
        assert!(
            src_pos > before || dst_pos > 0,
            "no progress at byte {src_pos} of {}",
            frame.len()
        );
    }
}

fn header(frame: &[u8]) -> ZSTD_FrameHeader {
    let mut zfh = core::mem::MaybeUninit::<ZSTD_FrameHeader>::uninit();
    let rc = unsafe { ZSTD_getFrameHeader(zfh.as_mut_ptr(), frame.as_ptr(), frame.len()) };
    assert_eq!(rc, 0, "{:?}", code(rc));
    unsafe { zfh.assume_init() }
}

/// A `0` pledge is "unknown" for the size-taking init; any other pledge is
/// written into the header and held to.
#[test]
fn init_cstream_src_size_pledges_the_frame() {
    let input = b"pledged stream ".repeat(40);
    let zcs = ZSTD_createCStream();
    unsafe {
        assert_eq!(ZSTD_initCStream_srcSize(zcs, 5, 0), 0);
        let frame = continue_then_end(zcs, &input).unwrap();
        assert_eq!(header(&frame).frameContentSize, u64::MAX);

        assert_eq!(ZSTD_initCStream_srcSize(zcs, 5, input.len() as u64), 0);
        let frame = continue_then_end(zcs, &input).unwrap();
        assert_eq!(header(&frame).frameContentSize, input.len() as u64);

        assert_eq!(ZSTD_initCStream_srcSize(zcs, 5, 10), 0);
        assert_eq!(
            code(continue_then_end(zcs, &input).unwrap_err()),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );
        ZSTD_freeCCtx(zcs);
    }
}

/// `ZSTD_compressStream` answers the input that completes the block being
/// filled, under the block size the context is set to.
#[test]
fn compress_stream_hints_the_rest_of_the_block() {
    const MAX_BLOCK_SIZE: c_int = 1015;
    let zcs = ZSTD_createCStream();
    let input = [7u8; 100];
    let mut out = vec![0u8; 1024];
    let mut feed = |zcs: *mut ZSTD_CCtx| unsafe {
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr().cast(),
            size: input.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: out.len(),
            pos: 0,
        };
        let hint = ZSTD_compressStream(zcs, &mut outb, &mut inb);
        assert_eq!(inb.pos, input.len());
        hint
    };
    unsafe {
        assert_eq!(ZSTD_initCStream(zcs, 3), 0);
        assert_eq!(feed(zcs), (128 << 10) - 100);
        assert_eq!(feed(zcs), (128 << 10) - 200);

        assert_eq!(ZSTD_initCStream(zcs, 3), 0);
        assert_eq!(ZSTD_CCtx_setParameter(zcs, MAX_BLOCK_SIZE, 64 << 10), 0);
        assert_eq!(feed(zcs), (64 << 10) - 100);
        ZSTD_freeCCtx(zcs);
    }
}

/// An advanced init refused for its compression parameters installs nothing:
/// the next frame on the context does not inherit the refused call's pledge.
#[test]
fn a_refused_advanced_init_leaves_no_pledge() {
    let input = b"no pledge survives a refused init ".repeat(40);
    let zcs = ZSTD_createCStream();
    unsafe {
        let mut params = crate::params::ZSTD_getParams(3, 0, 0);
        params.cParams.windowLog = 1;
        let rc = ZSTD_initCStream_advanced(zcs, core::ptr::null(), 0, params, 10);
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
        let frame = continue_then_end(zcs, &input).unwrap();
        assert_eq!(header(&frame).frameContentSize, u64::MAX);
        ZSTD_freeCCtx(zcs);
    }
}

/// The dictionary inits load or reference a dictionary the frame then uses;
/// the advanced ones carry their frame parameters too.
#[test]
fn init_cstream_with_dictionaries() {
    let input = dict_sample();
    let zcs = ZSTD_createCStream();
    let cdict = unsafe { ZSTD_createCDict(DICT.as_ptr(), DICT.len(), 5) };
    let restore = |frame: &[u8]| unsafe {
        let dctx = crate::context::ZSTD_createDCtx();
        let mut out = vec![0u8; input.len()];
        let n = ZSTD_decompress_usingDict(
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
            ZSTD_initCStream_usingDict(zcs, DICT.as_ptr(), DICT.len(), 5),
            0
        );
        let frame = end_frame(zcs, &input).unwrap();
        assert!(frame.len() < input.len() / 4, "{}", frame.len());
        assert_eq!(restore(&frame), input);

        let mut params = ZSTD_getParams(5, input.len() as u64, DICT.len());
        params.fParams.checksumFlag = 1;
        assert_eq!(
            ZSTD_initCStream_advanced(zcs, DICT.as_ptr(), DICT.len(), params, input.len() as u64),
            0
        );
        let frame = end_frame(zcs, &input).unwrap();
        assert_eq!(header(&frame).checksumFlag, 1);
        assert_eq!(header(&frame).frameContentSize, input.len() as u64);
        assert_eq!(restore(&frame), input);
        params.cParams.strategy = 10;
        assert_eq!(
            code(ZSTD_initCStream_advanced(
                zcs,
                core::ptr::null(),
                0,
                params,
                0
            )),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );

        assert_eq!(ZSTD_initCStream_usingCDict(zcs, cdict), 0);
        let frame = end_frame(zcs, &input).unwrap();
        assert_ne!(ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()), 0);
        assert_eq!(restore(&frame), input);

        let fparams = ZSTD_frameParameters {
            contentSizeFlag: 1,
            checksumFlag: 0,
            noDictIDFlag: 1,
        };
        assert_eq!(
            ZSTD_initCStream_usingCDict_advanced(zcs, cdict, fparams, input.len() as u64),
            0
        );
        let frame = end_frame(zcs, &input).unwrap();
        assert_eq!(ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()), 0);
        assert_eq!(restore(&frame), input);

        ZSTD_freeCCtx(zcs);
        ZSTD_freeCDict(cdict);
    }
}

/// A reset abandons the frame in flight and pledges the next.
#[test]
fn reset_cstream_starts_over() {
    let input = b"started over ".repeat(30);
    let zcs = ZSTD_createCStream();
    let mut sink = vec![0u8; 4096];
    unsafe {
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr().cast(),
            size: 7,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: sink.as_mut_ptr().cast(),
            size: sink.len(),
            pos: 0,
        };
        ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0);
        assert_eq!(ZSTD_resetCStream(zcs, input.len() as u64), 0);
        let frame = end_frame(zcs, &input).unwrap();
        assert_eq!(header(&frame).frameContentSize, input.len() as u64);
        let zds = ZSTD_createDStream();
        assert_eq!(decode_simple(zds, &frame).unwrap(), input);
        ZSTD_freeDCtx(zds);
        ZSTD_freeCCtx(zcs);
    }
}

/// The simple-argument calls move the caller's positions as the buffer
/// structs would.
#[test]
fn simple_args_stream_both_ways() {
    let input = b"spelled-out buffer arguments ".repeat(200);
    let zcs = ZSTD_createCStream();
    let mut frame = vec![0u8; input.len() + 1024];
    let mut dst_pos = 0;
    let mut src_pos = 0;
    unsafe {
        while src_pos < input.len() {
            let step = (src_pos + 333).min(input.len());
            let rc = ZSTD_compressStream2_simpleArgs(
                zcs,
                frame.as_mut_ptr().cast(),
                frame.len(),
                &mut dst_pos,
                input.as_ptr().cast(),
                step,
                &mut src_pos,
                0,
            );
            assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
        }
        let rc = ZSTD_compressStream2_simpleArgs(
            zcs,
            frame.as_mut_ptr().cast(),
            frame.len(),
            &mut dst_pos,
            input.as_ptr().cast(),
            input.len(),
            &mut src_pos,
            2,
        );
        assert_eq!(rc, 0, "{:?}", code(rc));
        frame.truncate(dst_pos);
        assert_eq!(src_pos, input.len());
        ZSTD_freeCCtx(zcs);

        let zds = ZSTD_createDStream();
        assert_eq!(decode_simple(zds, &frame).unwrap(), input);
        ZSTD_freeDCtx(zds);
    }
}

/// Progression counts what a frame took in, compressed, produced and handed
/// out, and `ZSTD_toFlushNow` what is waiting for room.
#[test]
fn frame_progression_follows_the_stream() {
    let input = b"progress ".repeat(500);
    let zcs = ZSTD_createCStream();
    let mut out = vec![0u8; 8];
    unsafe {
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr().cast(),
            size: 17,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: out.len(),
            pos: 0,
        };
        assert_eq!(
            ZSTD_isError(ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0)),
            0
        );
        let progress = ZSTD_getFrameProgression(zcs);
        assert_eq!(progress.ingested, 17);
        // Less than a block: taken in, not compressed yet.
        assert_eq!(progress.consumed, 0);
        assert_eq!((progress.currentJobID, progress.nbActiveWorkers), (0, 0));

        inb.size = input.len();
        let mut remaining = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2);
        assert_eq!(ZSTD_isError(remaining), 0);
        assert!(remaining > 0, "an 8-byte buffer cannot take the frame");
        assert_eq!(ZSTD_toFlushNow(zcs), remaining);
        let mut frame = out[..outb.pos].to_vec();
        while remaining > 0 {
            outb.pos = 0;
            remaining = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2);
            assert_eq!(ZSTD_isError(remaining), 0);
            frame.extend_from_slice(&out[..outb.pos]);
        }
        assert_eq!(ZSTD_toFlushNow(zcs), 0);
        let progress = ZSTD_getFrameProgression(zcs);
        assert_eq!(progress.ingested, input.len() as u64);
        assert_eq!(progress.consumed, input.len() as u64);
        assert_eq!(progress.produced, frame.len() as u64);
        assert_eq!(progress.flushed, frame.len() as u64);

        // A one-shot frame reports all of itself.
        let mut one_shot = vec![0u8; input.len()];
        let n = crate::context::ZSTD_compress2(
            zcs,
            one_shot.as_mut_ptr(),
            one_shot.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(n), 0);
        let progress = ZSTD_getFrameProgression(zcs);
        assert_eq!(
            (progress.ingested, progress.produced),
            (input.len() as u64, n as u64)
        );
        ZSTD_freeCCtx(zcs);
        let empty = ZSTD_getFrameProgression(core::ptr::null());
        assert_eq!(empty.ingested, 0);
    }
}

/// The one-shot entry points that predate the advanced parameters start their
/// frame afresh, as upstream's reset the session: a streamed frame left open
/// is abandoned, and the progression describes the one-shot frame.
#[test]
fn simple_one_shots_abandon_an_open_stream() {
    let buffered = b"buffered and never ended ".repeat(4000);
    let small = b"0123456789";
    let zcs = ZSTD_createCStream();
    let mut out = vec![0u8; 1 << 17];
    unsafe {
        let mut inb = ZSTD_inBuffer {
            src: buffered.as_ptr().cast(),
            size: buffered.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: out.len(),
            pos: 0,
        };
        assert_eq!(
            ZSTD_isError(ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0)),
            0
        );
        let mut dst = vec![0u8; 256];
        let n = crate::context::ZSTD_compressCCtx(
            zcs,
            dst.as_mut_ptr(),
            dst.len(),
            small.as_ptr(),
            small.len(),
            3,
        );
        assert_eq!(ZSTD_isError(n), 0);
        let progress = ZSTD_getFrameProgression(zcs);
        assert_eq!((progress.ingested, progress.consumed), (10, 10));
        // Nothing is mid-frame any more: parameters may change again.
        assert_eq!(
            crate::params::ZSTD_CCtx_setParameter(zcs, crate::params::ZSTD_C_COMPRESSION_LEVEL, 5),
            0
        );
        ZSTD_freeCCtx(zcs);
    }
}

/// Input the stream took counts as ingested even when ending the frame then
/// fails on a pledge it falls short of: it sits buffered in the open frame,
/// and the progression must not subtract it from less.
#[test]
fn ingested_counts_input_before_a_failed_end() {
    let input = b"fifty bytes of input that fall short of the pledge";
    let zcs = ZSTD_createCStream();
    let mut out = vec![0u8; 1024];
    unsafe {
        crate::params::ZSTD_CCtx_setPledgedSrcSize(zcs, 100);
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr().cast(),
            size: input.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: out.len(),
            pos: 0,
        };
        // Begun by a continue, so the end is held to the pledge.
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0);
        assert_eq!(ZSTD_isError(rc), 0);
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2);
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
        assert_eq!(inb.pos, input.len());
        let progress = ZSTD_getFrameProgression(zcs);
        assert_eq!(progress.ingested, input.len() as u64);
        assert!(progress.consumed <= progress.ingested);
        ZSTD_freeCCtx(zcs);
    }
}

/// A dictionary under eight bytes is content, not a truncated serialized one,
/// even when it is the magic number alone: both stream inits take it, as
/// upstream does.
#[test]
fn stream_inits_take_a_dictionary_under_eight_bytes() {
    let magic_only = [0x37u8, 0xA4, 0x30, 0xEC];
    let input = b"short dictionaries are content".repeat(20);
    unsafe {
        let zcs = ZSTD_createCStream();
        let rc = ZSTD_initCStream_usingDict(zcs, magic_only.as_ptr().cast(), 4, 3);
        assert_eq!(rc, 0, "{:?}", code(rc));
        let frame = end_frame(zcs, &input).expect("the frame compresses");
        ZSTD_freeCCtx(zcs);

        let zds = ZSTD_createDStream();
        let rc = ZSTD_initDStream_usingDict(zds, magic_only.as_ptr().cast(), 4);
        assert_eq!(rc, 5, "{:?}", code(rc));
        let mut out = vec![0u8; input.len()];
        let n = ZSTD_decompress_usingDict(
            zds,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
            magic_only.as_ptr(),
            4,
        );
        assert_eq!(n, input.len(), "{:?}", code(n));
        assert_eq!(out, input);
        ZSTD_freeDCtx(zds);
    }
}

/// A serialized dictionary given as a prefix with the auto or fullDict
/// selector is a full dictionary for its one frame, as upstream keeps the
/// selector with the prefix (zstd_compress.c:1351, zstd_decompress.c:1725):
/// the frame names it, and the decoding side reads it the same way.
#[test]
fn a_serialized_prefix_is_a_full_dictionary() {
    let input = dict_sample();
    let id = unsafe { crate::attach::ZSTD_getDictID_fromDict(DICT.as_ptr(), DICT.len()) };
    assert_ne!(id, 0);
    for content_type in [0, 2] {
        unsafe {
            let cctx = crate::context::ZSTD_createCCtx();
            let rc = crate::attach::ZSTD_CCtx_refPrefix_advanced(
                cctx,
                DICT.as_ptr(),
                DICT.len(),
                content_type,
            );
            assert_eq!(rc, 0, "{content_type}: {:?}", code(rc));
            let mut frame = vec![0u8; input.len() + 1024];
            let n = crate::context::ZSTD_compress2(
                cctx,
                frame.as_mut_ptr(),
                frame.len(),
                input.as_ptr(),
                input.len(),
            );
            assert_eq!(ZSTD_isError(n), 0, "{content_type}: {:?}", code(n));
            assert_eq!(
                ZSTD_getDictID_fromFrame(frame.as_ptr(), n),
                id,
                "{content_type}"
            );
            ZSTD_freeCCtx(cctx);

            let dctx = crate::context::ZSTD_createDCtx();
            let rc = crate::attach::ZSTD_DCtx_refPrefix_advanced(
                dctx,
                DICT.as_ptr(),
                DICT.len(),
                content_type,
            );
            assert_eq!(rc, 0, "{content_type}: {:?}", code(rc));
            let mut out = vec![0u8; input.len()];
            let m = crate::context::ZSTD_decompressDCtx(
                dctx,
                out.as_mut_ptr(),
                out.len(),
                frame.as_ptr(),
                n,
            );
            assert_eq!(m, input.len(), "{content_type}: {:?}", code(m));
            assert_eq!(out, input);
            ZSTD_freeDCtx(dctx);
        }
    }
}

/// A dictionary under eight bytes takes no part in compression, as zstd.h
/// states for every dictionary entry point: the frame is the one compressed
/// without it, and decodes without it, even when the input starts with it.
#[test]
fn a_dictionary_under_eight_bytes_is_no_dictionary_when_compressing() {
    let tiny = b"short d";
    let input = b"short dictionaries take no part ".repeat(20);
    let plain = |level| {
        let mut frame = vec![0u8; input.len() + 64];
        let n = unsafe {
            crate::simple::ZSTD_compress(
                frame.as_mut_ptr(),
                frame.len(),
                input.as_ptr(),
                input.len(),
                level,
            )
        };
        assert_eq!(ZSTD_isError(n), 0);
        frame.truncate(n);
        frame
    };
    let decodes_alone = |frame: &[u8]| {
        let mut out = vec![0u8; input.len()];
        let n = unsafe {
            crate::simple::ZSTD_decompress(out.as_mut_ptr(), out.len(), frame.as_ptr(), frame.len())
        };
        assert_eq!(n, input.len(), "{:?}", code(n));
        assert_eq!(out, input);
    };
    unsafe {
        let zcs = ZSTD_createCStream();
        assert_eq!(
            ZSTD_initCStream_usingDict(zcs, tiny.as_ptr().cast(), tiny.len(), 3),
            0
        );
        let streamed = end_frame(zcs, &input).expect("the frame compresses");
        decodes_alone(&streamed);

        let mut frame = vec![0u8; input.len() + 64];
        let n = crate::attach::ZSTD_compress_usingDict(
            zcs,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            tiny.as_ptr(),
            tiny.len(),
            3,
        );
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        assert_eq!(&frame[..n], &plain(3)[..]);
        decodes_alone(&frame[..n]);
        ZSTD_freeCCtx(zcs);
    }
}

/// The decoding inits report the first read size and set up the dictionary:
/// the plain one drops a referenced DDict, the others load or reference one.
#[test]
fn init_dstream_variants() {
    let input = dict_sample();
    let frame = unsafe {
        let zcs = ZSTD_createCStream();
        assert_eq!(
            ZSTD_initCStream_usingDict(zcs, DICT.as_ptr(), DICT.len(), 3),
            0
        );
        let frame = end_frame(zcs, &input).unwrap();
        ZSTD_freeCCtx(zcs);
        frame
    };
    unsafe {
        let ddict = ZSTD_createDDict(DICT.as_ptr(), DICT.len());
        let zds = ZSTD_createDStream();
        assert_eq!(ZSTD_initDStream_usingDDict(zds, ddict), 5);
        assert_eq!(decode_simple(zds, &frame).unwrap(), input);

        assert_eq!(ZSTD_initDStream(zds), 5);
        assert_eq!(
            code(decode_simple(zds, &frame).unwrap_err()),
            ZSTD_ErrorCode::ZSTD_error_dictionary_wrong
        );

        assert_eq!(
            ZSTD_initDStream_usingDict(zds, DICT.as_ptr(), DICT.len()),
            5
        );
        assert_eq!(decode_simple(zds, &frame).unwrap(), input);
        // A reset keeps the dictionary.
        assert_eq!(ZSTD_resetDStream(zds), 5);
        assert_eq!(decode_simple(zds, &frame).unwrap(), input);

        assert_eq!(ZSTD_DCtx_refDDict(zds, core::ptr::null()), 0);
        assert_eq!(ZSTD_DCtx_setFormat(zds, 1), 0);
        assert_eq!(ZSTD_resetDStream(zds), 1);
        ZSTD_freeDCtx(zds);
        ZSTD_freeDDict(ddict);
    }
}

/// The default allocator creates the usual contexts; a custom one is refused.
/// Default means both callbacks `NULL`, whatever `opaque` holds: upstream's
/// default path never reads it (`ZSTD_customMalloc`, zstd_internal.h).
#[test]
fn advanced_creators_take_only_the_default_allocator() {
    extern "C" fn no_alloc(_: *mut core::ffi::c_void, _: usize) -> *mut core::ffi::c_void {
        core::ptr::null_mut()
    }
    let marker = 1u8;
    let default = ZSTD_customMem {
        customAlloc: core::ptr::null(),
        customFree: core::ptr::null(),
        opaque: core::ptr::from_ref(&marker).cast(),
    };
    let custom = ZSTD_customMem {
        customAlloc: no_alloc as *const core::ffi::c_void,
        ..default
    };
    let dict = DICT;
    unsafe {
        let cctx = crate::context::ZSTD_createCCtx_advanced(default);
        assert!(!cctx.is_null());
        ZSTD_freeCCtx(cctx);
        let dctx = crate::context::ZSTD_createDCtx_advanced(default);
        assert!(!dctx.is_null());
        ZSTD_freeDCtx(dctx);
        let zcs = ZSTD_createCStream_advanced(default);
        assert!(!zcs.is_null());
        ZSTD_freeCCtx(zcs);
        let zds = ZSTD_createDStream_advanced(default);
        assert!(!zds.is_null());
        ZSTD_freeDCtx(zds);
        let ddict =
            crate::cdict::ZSTD_createDDict_advanced(dict.as_ptr(), dict.len(), 0, 0, default);
        assert!(!ddict.is_null());
        ZSTD_freeDDict(ddict);
        let ddict =
            crate::cdict::ZSTD_createDDict_advanced(dict.as_ptr(), dict.len(), 0, 0, custom);
        assert!(ddict.is_null());
    }
    assert!(crate::context::ZSTD_createCCtx_advanced(custom).is_null());
    assert!(crate::context::ZSTD_createDCtx_advanced(custom).is_null());
    assert!(ZSTD_createCStream_advanced(custom).is_null());
    assert!(ZSTD_createDStream_advanced(custom).is_null());
}

/// Decode `stream` handing over at most `window` bytes per call, starting
/// each call where `input.pos` left off, as a caller with a fixed-size input
/// buffer does. Fails on a call that moves neither position.
fn decode_through_window(zds: *mut ZSTD_DCtx, stream: &[u8], window: usize) -> Vec<u8> {
    let mut decoded = Vec::new();
    let mut out = [0u8; 4096];
    let mut at = 0;
    loop {
        let mut inb = ZSTD_inBuffer {
            src: stream[at..].as_ptr().cast(),
            size: window.min(stream.len() - at),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: out.len(),
            pos: 0,
        };
        let rc = unsafe { crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb) };
        assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
        decoded.extend_from_slice(&out[..outb.pos]);
        at += inb.pos;
        if at == stream.len() && rc == 0 {
            return decoded;
        }
        assert!(
            inb.pos > 0 || outb.pos > 0,
            "no progress at byte {at} of {}",
            stream.len()
        );
    }
}

/// A context that took literal-heavy blocks in small pieces, staging each,
/// stays within the budget `ZSTD_estimateDStreamSize` gives for the frame's
/// window, the staged input block included.
#[test]
fn a_stream_decoded_in_pieces_stays_within_its_estimate() {
    let mut state = 0x9E37_79B9u32;
    // Sixteen symbols in no repeating order: blocks of Huffman literals.
    let input: Vec<u8> = (0..1usize << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            b'a' + (state % 16) as u8
        })
        .collect();
    let mut frame = vec![0u8; crate::simple::ZSTD_compressBound(input.len())];
    let n = unsafe {
        crate::simple::ZSTD_compress(
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
    frame.truncate(n);
    let window = usize::try_from(header(&frame).windowSize).unwrap();

    let zds = ZSTD_createDStream();
    assert!(!zds.is_null());
    assert_eq!(decode_through_window(zds, &frame, 61), input);
    let held = unsafe { crate::context::ZSTD_sizeof_DCtx(zds) };
    let budget = crate::estimate::ZSTD_estimateDStreamSize(window);
    assert!(
        held <= budget,
        "the context holds {held} bytes over a budget of {budget} for a {window}-byte window"
    );
    unsafe { ZSTD_freeDCtx(zds) };
}

/// A frame header split across calls is taken in as it arrives, one byte at
/// a time if need be, and the context counts as inside a frame meanwhile.
#[test]
fn decompress_stream_takes_a_header_in_pieces() {
    let input = b"a frame header that arrives a byte at a time ".repeat(40);
    let mut frame = vec![0u8; input.len() + 64];
    let n = unsafe {
        crate::simple::ZSTD_compress(
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    frame.truncate(n);
    let zds = ZSTD_createDStream();
    assert_eq!(decode_through_window(zds, &frame, 1), input);
    assert_eq!(decode_through_window(zds, &frame, 3), input);

    // Part of a header in, the frame has begun: parameters wait for a reset.
    let mut inb = ZSTD_inBuffer {
        src: frame.as_ptr().cast(),
        size: 3,
        pos: 0,
    };
    let mut outb = ZSTD_outBuffer {
        dst: core::ptr::null_mut(),
        size: 0,
        pos: 0,
    };
    let rc = unsafe { crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb) };
    assert_eq!(ZSTD_isError(rc), 0);
    assert_eq!(inb.pos, 3);
    assert_eq!(
        code(unsafe { ZSTD_DCtx_setFormat(zds, 0) }),
        ZSTD_ErrorCode::ZSTD_error_stage_wrong
    );
    assert_eq!(ZSTD_isError(unsafe { ZSTD_resetDStream(zds) }), 0);
    assert_eq!(unsafe { ZSTD_DCtx_setFormat(zds, 0) }, 0);
    assert_eq!(decode_through_window(zds, &frame, 5), input);

    // Blocks and the checksum after them arrive in pieces too.
    let mut state = 0x2545_F491u32;
    let large: Vec<u8> = (0..300_000)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if i % 3 == 0 { state as u8 } else { b'x' }
        })
        .collect();
    let cctx = crate::context::ZSTD_createCCtx();
    let rc = unsafe {
        crate::params::ZSTD_CCtx_setParameter(cctx, crate::params::ZSTD_C_CHECKSUM_FLAG, 1)
    };
    assert_eq!(ZSTD_isError(rc), 0);
    let mut checked = vec![0u8; large.len() + 1024];
    let n = unsafe {
        crate::context::ZSTD_compress2(
            cctx,
            checked.as_mut_ptr(),
            checked.len(),
            large.as_ptr(),
            large.len(),
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    checked.truncate(n);
    assert_eq!(decode_through_window(zds, &checked, 7), large);
    unsafe { ZSTD_freeCCtx(cctx) };
    unsafe { ZSTD_freeDCtx(zds) };
}

/// `0` means a frame was just finished, as upstream returns it: a call that
/// reaches the end of a skippable frame returns it, and a call with no input
/// at a frame boundary, a fresh one or one after a frame, asks for input
/// instead.
#[test]
fn decompress_stream_returns_zero_only_for_a_finished_frame() {
    let call = |zds: *mut ZSTD_DCtx, src: &[u8]| {
        let mut inb = ZSTD_inBuffer {
            src: src.as_ptr().cast(),
            size: src.len(),
            pos: 0,
        };
        let mut out = [0u8; 256];
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: out.len(),
            pos: 0,
        };
        let rc = unsafe { crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb) };
        assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
        (rc, inb.pos)
    };
    let zds = ZSTD_createDStream();
    assert_ne!(call(zds, &[]).0, 0, "a fresh stream has finished nothing");

    let mut skippable = [0u8; 8 + 16];
    let n = unsafe {
        crate::frame::ZSTD_writeSkippableFrame(
            skippable.as_mut_ptr(),
            skippable.len(),
            [1u8; 16].as_ptr(),
            16,
            0,
        )
    };
    assert_eq!(n, skippable.len());
    assert_eq!(call(zds, &skippable), (0, skippable.len()));
    assert_ne!(call(zds, &[]).0, 0, "nothing new was finished");

    let mut frame = [0u8; 64];
    let n = unsafe {
        crate::simple::ZSTD_compress(frame.as_mut_ptr(), frame.len(), b"bytes".as_ptr(), 5, 3)
    };
    assert_eq!(call(zds, &frame[..n]), (0, n));
    assert_ne!(call(zds, &[]).0, 0, "nothing new was finished");
    unsafe { ZSTD_freeDCtx(zds) };
}

/// Inside a frame, the sixteenth call in a row that moves neither position
/// fails with the reason it could not move, as upstream's guard does
/// (zstd_decompress.c:2356): no input left, or no room left for output. Any
/// progress starts the count again.
#[test]
fn decompress_stream_refuses_to_spin_without_progress() {
    let input = b"a frame that cannot finish without the rest of its input ".repeat(40);
    let mut frame = vec![0u8; input.len() + 64];
    let n = unsafe {
        crate::simple::ZSTD_compress(
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    let frame = &frame[..n];
    let mut out = vec![0u8; input.len()];
    let room = out.len();
    let call = |zds: *mut ZSTD_DCtx, src: &[u8], out: &mut [u8], room: usize| {
        let mut inb = ZSTD_inBuffer {
            src: src.as_ptr().cast(),
            size: src.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: out.as_mut_ptr().cast(),
            size: room,
            pos: 0,
        };
        unsafe { crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb) }
    };

    // Out of input mid-frame.
    let zds = ZSTD_createDStream();
    let rc = call(zds, &frame[..n / 2], &mut out, room);
    assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
    for at in 1..16 {
        let rc = call(zds, &[], &mut out, room);
        assert_eq!(ZSTD_isError(rc), 0, "call {at}: {:?}", code(rc));
    }
    let rc = call(zds, &[], &mut out, room);
    assert_eq!(
        code(rc),
        ZSTD_ErrorCode::ZSTD_error_noForwardProgress_inputEmpty
    );
    unsafe { ZSTD_freeDCtx(zds) };

    // Out of output room mid-frame, with progress in between restarting it.
    let zds = ZSTD_createDStream();
    let rc = call(zds, frame, &mut out, 0);
    assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
    for at in 1..16 {
        let rc = call(zds, &[], &mut out, 0);
        assert_eq!(ZSTD_isError(rc), 0, "call {at}: {:?}", code(rc));
    }
    let rc = call(zds, &[], &mut out, 1);
    assert_eq!(ZSTD_isError(rc), 0, "progress restarts the count");
    for at in 1..16 {
        let rc = call(zds, &[], &mut out, 0);
        assert_eq!(ZSTD_isError(rc), 0, "call {at}: {:?}", code(rc));
    }
    let rc = call(zds, &[], &mut out, 0);
    assert_eq!(
        code(rc),
        ZSTD_ErrorCode::ZSTD_error_noForwardProgress_destFull
    );
    unsafe { ZSTD_freeDCtx(zds) };
}

/// Under `ZSTD_d_refMultipleDDicts` every referenced DDict stays: a frame
/// naming a dictionary ID decodes with that one, a frame naming none with the
/// most recent, frame by frame within one call and across a stream. Without
/// it the last reference replaces the others.
#[test]
fn multiple_ddicts_are_picked_by_the_frame() {
    let raw = b"a raw dictionary is nothing but content to match against ".repeat(30);
    let input = b"content to match against, and more of the dictionary sample ".repeat(30);
    let compress_with = |dict: &[u8]| {
        let cctx = crate::context::ZSTD_createCCtx();
        let mut out = vec![0u8; input.len() + 256];
        let n = unsafe {
            crate::attach::ZSTD_compress_usingDict(
                cctx,
                out.as_mut_ptr(),
                out.len(),
                input.as_ptr(),
                input.len(),
                dict.as_ptr(),
                dict.len(),
                3,
            )
        };
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        out.truncate(n);
        unsafe { ZSTD_freeCCtx(cctx) };
        out
    };
    let named = compress_with(DICT);
    let unnamed = compress_with(&raw);
    assert_ne!(
        unsafe { ZSTD_getDictID_fromFrame(named.as_ptr(), named.len()) },
        0
    );
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(unnamed.as_ptr(), unnamed.len()) },
        0
    );
    let both = [named.as_slice(), unnamed.as_slice()].concat();
    let expected = [input.as_slice(), input.as_slice()].concat();

    unsafe {
        let full = ZSTD_createDDict(DICT.as_ptr(), DICT.len());
        let raw_ddict = ZSTD_createDDict(raw.as_ptr(), raw.len());
        let zds = ZSTD_createDStream();
        let mut out = vec![0u8; expected.len()];

        assert_eq!(crate::params::ZSTD_DCtx_setParameter(zds, 1003, 1), 0);
        assert_eq!(ZSTD_DCtx_refDDict(zds, full), 0);
        assert_eq!(ZSTD_DCtx_refDDict(zds, raw_ddict), 0);
        let n = crate::context::ZSTD_decompressDCtx(
            zds,
            out.as_mut_ptr(),
            out.len(),
            both.as_ptr(),
            both.len(),
        );
        assert_eq!(n, expected.len(), "{:?}", code(n));
        assert_eq!(out, expected);
        assert_eq!(decode_simple(zds, &named).unwrap(), input);
        assert_eq!(decode_simple(zds, &unnamed).unwrap(), input);
        // Referencing a DDict again keeps one entry for it.
        assert_eq!(ZSTD_DCtx_refDDict(zds, full), 0);
        assert_eq!(decode_simple(zds, &named).unwrap(), input);

        // A parameter reset drops the references and the mode.
        assert_eq!(crate::params::ZSTD_DCtx_reset(zds, 2), 0);
        assert_eq!(ZSTD_DCtx_refDDict(zds, full), 0);
        assert_eq!(ZSTD_DCtx_refDDict(zds, raw_ddict), 0);
        let n = crate::context::ZSTD_decompressDCtx(
            zds,
            out.as_mut_ptr(),
            out.len(),
            named.as_ptr(),
            named.len(),
        );
        assert_eq!(
            code(n),
            ZSTD_ErrorCode::ZSTD_error_dictionary_wrong,
            "a single reference is the last one"
        );
        ZSTD_freeDCtx(zds);
        ZSTD_freeDDict(full);
        ZSTD_freeDDict(raw_ddict);
    }
}

/// A frame begun and ended by one `ZSTD_e_end` call takes the size of the
/// input in hand, whatever was pledged before (zstd.h: "the pledge is
/// overridden"; upstream zstd_compress.c:6390); a frame begun by an earlier
/// call is still held to its pledge.
#[test]
fn a_single_end_call_overrides_the_pledge() {
    let zcs = ZSTD_createCStream();
    unsafe {
        assert_eq!(crate::params::ZSTD_CCtx_setPledgedSrcSize(zcs, 10), 0);
        let frame = end_frame(zcs, b"12345").unwrap();
        assert_eq!(
            crate::simple::ZSTD_getFrameContentSize(frame.as_ptr(), frame.len()),
            5
        );
        let mut out = [0u8; 16];
        let n = crate::simple::ZSTD_decompress(
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        );
        assert_eq!(&out[..n], b"12345");

        // Begun by a continue, the frame keeps the pledge it started with.
        assert_eq!(crate::params::ZSTD_CCtx_setPledgedSrcSize(zcs, 10), 0);
        let mut sink = [0u8; 64];
        let mut inb = ZSTD_inBuffer {
            src: b"123".as_ptr().cast(),
            size: 3,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: sink.as_mut_ptr().cast(),
            size: sink.len(),
            pos: 0,
        };
        assert_eq!(
            ZSTD_isError(ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0)),
            0
        );
        assert_eq!(
            code(end_frame(zcs, b"45").unwrap_err()),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );
        ZSTD_freeCCtx(zcs);
    }
}

/// One frame for a whole session, as rsync streams it: each file's data goes
/// in with `ZSTD_e_continue` and is flushed with `ZSTD_e_flush` when the file
/// ends, every call handed a fresh 16 KiB output buffer; the far end decodes
/// each chunk with an output buffer of two blocks and must have every file's
/// bytes in hand by the time its flush arrives (rsync token.c,
/// send_zstd_token / recv_zstd_token).
#[test]
fn a_session_flushed_per_file_decodes_file_by_file() {
    const MAX_DATA_COUNT: usize = 16383;
    let files: Vec<Vec<u8>> = vec![
        (1..=200_000u32)
            .flat_map(|i| format!("{i}\n").into_bytes())
            .collect(),
        b"line of a log that repeats itself\n".repeat(300),
        (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect(),
        Vec::new(),
        b"short".to_vec(),
    ];
    let zcs = ZSTD_createCStream();
    let zds = ZSTD_createDStream();
    let mut out = vec![0u8; MAX_DATA_COUNT];
    let mut dbuf = vec![0u8; crate::streaming::ZSTD_DStreamOutSize() * 2];
    let mut call = |src: &[u8], flush: c_int, chunks: &mut Vec<Vec<u8>>| {
        let mut inb = ZSTD_inBuffer {
            src: src.as_ptr().cast(),
            size: src.len(),
            pos: 0,
        };
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: out.as_mut_ptr().cast(),
                size: out.len(),
                pos: 0,
            };
            let r = unsafe { ZSTD_compressStream2(zcs, &mut outb, &mut inb, flush) };
            assert_eq!(ZSTD_isError(r), 0, "{:?}", code(r));
            if outb.pos != 0 {
                chunks.push(out[..outb.pos].to_vec());
            }
            let finished = if flush == 1 {
                r == 0
            } else {
                inb.pos == inb.size
            };
            if finished {
                return;
            }
        }
    };
    for file in &files {
        let mut chunks = Vec::new();
        for piece in file.chunks(32 * 1024) {
            call(piece, 0, &mut chunks);
        }
        call(&[], 1, &mut chunks);
        // Everything flushed so far decodes to exactly this file.
        let mut restored = Vec::new();
        for chunk in &chunks {
            let mut inb = ZSTD_inBuffer {
                src: chunk.as_ptr().cast(),
                size: chunk.len(),
                pos: 0,
            };
            loop {
                let mut outb = ZSTD_outBuffer {
                    dst: dbuf.as_mut_ptr().cast(),
                    size: dbuf.len(),
                    pos: 0,
                };
                let r =
                    unsafe { crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb) };
                assert_eq!(ZSTD_isError(r), 0, "{:?}", code(r));
                restored.extend_from_slice(&dbuf[..outb.pos]);
                if inb.pos == inb.size && outb.pos < outb.size {
                    break;
                }
            }
        }
        assert_eq!(restored.len(), file.len());
        assert!(restored == *file, "file of {} bytes", file.len());
    }
    unsafe {
        ZSTD_freeCCtx(zcs);
        ZSTD_freeDCtx(zds);
    }
}

/// libarchive's zstd writer (archive_write_add_filter_zstd.c, drive_compressor):
/// an output buffer of `ZSTD_CStreamOutSize()` rounded down to tar's 10240-byte
/// blocks, so smaller than a raw block; tar records fed through
/// `ZSTD_compressStream`, the buffer written only when full; then
/// `ZSTD_endStream` until it returns 0. Incompressible input makes raw blocks
/// that cannot fit the buffer whole, and the stream must still carry the whole
/// frame.
#[test]
fn a_raw_block_larger_than_the_output_buffer_reaches_the_end_of_the_frame() {
    use crate::streaming::{ZSTD_CStreamOutSize, ZSTD_DStreamOutSize, ZSTD_endStream};

    const RECORD: usize = 10240;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let noise: Vec<u8> = (0..300_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    let mut input = b"a line of text that repeats\n".repeat(2000);
    input.extend_from_slice(&noise);
    input.extend_from_slice(b"tail");

    let zcs = ZSTD_createCStream();
    assert_eq!(ZSTD_isError(unsafe { ZSTD_initCStream(zcs, 3) }), 0);
    unsafe {
        ZSTD_CCtx_setParameter(zcs, crate::params::ZSTD_C_NB_WORKERS, 0);
        ZSTD_CCtx_setParameter(zcs, crate::params::ZSTD_C_WINDOW_LOG, 0);
    }
    let size = ZSTD_CStreamOutSize() - ZSTD_CStreamOutSize() % RECORD;
    let mut buf = vec![0u8; size];
    let mut stream = Vec::new();
    let mut outb = ZSTD_outBuffer {
        dst: buf.as_mut_ptr().cast(),
        size,
        pos: 0,
    };
    for record in input.chunks(RECORD) {
        let mut inb = ZSTD_inBuffer {
            src: record.as_ptr().cast(),
            size: record.len(),
            pos: 0,
        };
        while inb.pos < inb.size {
            let r = unsafe { ZSTD_compressStream(zcs, &mut outb, &mut inb) };
            assert_eq!(ZSTD_isError(r), 0, "{:?}", code(r));
            if outb.pos == outb.size {
                stream.extend_from_slice(&buf[..outb.pos]);
                outb.pos = 0;
            }
        }
    }
    loop {
        let r = unsafe { ZSTD_endStream(zcs, &mut outb) };
        assert_eq!(ZSTD_isError(r), 0, "{:?}", code(r));
        stream.extend_from_slice(&buf[..outb.pos]);
        outb.pos = 0;
        if r == 0 {
            break;
        }
    }

    let zds = ZSTD_createDStream();
    let mut restored = Vec::new();
    let mut dbuf = vec![0u8; ZSTD_DStreamOutSize()];
    let mut inb = ZSTD_inBuffer {
        src: stream.as_ptr().cast(),
        size: stream.len(),
        pos: 0,
    };
    let mut last = usize::MAX;
    while inb.pos < inb.size || last != 0 {
        let mut doutb = ZSTD_outBuffer {
            dst: dbuf.as_mut_ptr().cast(),
            size: dbuf.len(),
            pos: 0,
        };
        let before = inb.pos;
        last = unsafe { crate::streaming::ZSTD_decompressStream(zds, &mut doutb, &mut inb) };
        assert_eq!(ZSTD_isError(last), 0, "{:?}", code(last));
        restored.extend_from_slice(&dbuf[..doutb.pos]);
        assert!(
            inb.pos > before || doutb.pos > 0,
            "the frame stops short at byte {} of {}",
            inb.pos,
            stream.len()
        );
    }
    assert_eq!(restored.len(), input.len());
    assert!(restored == input);
    unsafe {
        ZSTD_freeCCtx(zcs);
        ZSTD_freeDCtx(zds);
    }
}

/// libarchive's zstd reader (archive_read_support_filter_zstd.c,
/// zstd_filter_read): an output block of `ZSTD_DStreamOutSize()` filled call
/// by call from 10240-byte reads, a frame counted as open while the last call
/// returned non-zero, and input running out inside an open frame reported as
/// truncated. A finished frame must say so by the time its last byte is taken.
#[test]
fn a_finished_frame_reports_zero_by_its_last_input_byte() {
    use crate::streaming::ZSTD_DStreamOutSize;

    const RECORD: usize = 10240;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let noise: Vec<u8> = (0..300_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    let mut text = b"a line of text that repeats\n".repeat(2000);
    text.extend_from_slice(&noise);
    // Sizes that end the frame on, just before and just after an output block.
    let block = ZSTD_DStreamOutSize();
    let inputs = [
        text.clone(),
        noise[..block].to_vec(),
        noise[..2 * block].to_vec(),
        noise[..block - 1].to_vec(),
        noise[..block + 1].to_vec(),
    ];
    for checksum in [0, 1] {
        for input in &inputs {
            let zcs = ZSTD_createCStream();
            unsafe {
                ZSTD_CCtx_setParameter(zcs, crate::params::ZSTD_C_CHECKSUM_FLAG, checksum);
            }
            let frame = end_frame(zcs, input).unwrap();
            unsafe { ZSTD_freeCCtx(zcs) };

            let zds = ZSTD_createDStream();
            let mut out = vec![0u8; block];
            let mut restored = Vec::new();
            let mut taken = 0;
            let mut in_frame = false;
            let mut eof = false;
            while !eof {
                let mut outb = ZSTD_outBuffer {
                    dst: out.as_mut_ptr().cast(),
                    size: out.len(),
                    pos: 0,
                };
                while outb.pos < outb.size && !eof {
                    if !in_frame {
                        assert_eq!(ZSTD_isError(unsafe { ZSTD_initDStream(zds) }), 0);
                    }
                    let end = frame.len().min(taken - taken % RECORD + RECORD);
                    if taken == frame.len() {
                        assert!(
                            !in_frame,
                            "truncated: {} bytes, checksum {checksum}",
                            input.len()
                        );
                        eof = true;
                        break;
                    }
                    let mut inb = ZSTD_inBuffer {
                        src: frame[taken..].as_ptr().cast(),
                        size: end - taken,
                        pos: 0,
                    };
                    let r = unsafe {
                        crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb)
                    };
                    assert_eq!(ZSTD_isError(r), 0, "{:?}", code(r));
                    taken += inb.pos;
                    in_frame = r != 0;
                }
                restored.extend_from_slice(&out[..outb.pos]);
            }
            assert!(
                restored == *input,
                "{} bytes, checksum {checksum}",
                input.len()
            );
            unsafe { ZSTD_freeDCtx(zds) };
        }
    }
}

/// A skippable frame larger than the input buffer is skipped across calls,
/// and the frame after it decodes.
#[test]
fn decompress_stream_skips_a_frame_larger_than_its_input() {
    let payload = vec![0xA5u8; 1000];
    let mut stream = vec![0u8; payload.len() + 8];
    let n = unsafe {
        crate::frame::ZSTD_writeSkippableFrame(
            stream.as_mut_ptr(),
            stream.len(),
            payload.as_ptr(),
            payload.len(),
            3,
        )
    };
    assert_eq!(n, stream.len());
    let input = b"the frame after the skippable one ".repeat(20);
    let mut frame = vec![0u8; input.len() + 64];
    let n = unsafe {
        crate::simple::ZSTD_compress(
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    stream.extend_from_slice(&frame[..n]);
    let zds = ZSTD_createDStream();
    for window in [1, 7, 64] {
        assert_eq!(
            decode_through_window(zds, &stream, window),
            input,
            "{window}"
        );
    }
    unsafe { ZSTD_freeDCtx(zds) };
}
