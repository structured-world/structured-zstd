//! `ZSTD_CCtx_params`, the experimental compression parameters, and the entry
//! points that take parameters wholesale. Values marked "upstream" were read
//! from `libzstd` 1.5.7 called with the same arguments.

use core::ffi::c_int;

use super::experimental::{code, compressed, cparams, stream_decode};
use crate::cctx_params::{
    ZSTD_CCtx_params, ZSTD_CCtx_setParametersUsingCCtxParams, ZSTD_CCtxParams_getParameter,
    ZSTD_CCtxParams_init, ZSTD_CCtxParams_init_advanced, ZSTD_CCtxParams_reset,
    ZSTD_CCtxParams_setParameter, ZSTD_compress_advanced, ZSTD_createCCtxParams,
    ZSTD_createCDict_advanced2, ZSTD_estimateCCtxSize_usingCCtxParams,
    ZSTD_estimateCStreamSize_usingCCtxParams, ZSTD_freeCCtxParams,
};
use crate::cdict::{
    ZSTD_compress_usingCDict, ZSTD_createDDict, ZSTD_customMem, ZSTD_decompress_usingDDict,
    ZSTD_freeCDict, ZSTD_freeDDict,
};
use crate::context::{
    ZSTD_CCtx, ZSTD_compress2, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_decompressDCtx,
    ZSTD_freeCCtx, ZSTD_freeDCtx,
};
use crate::error::{ZSTD_ErrorCode, ZSTD_isError};
use crate::params::{
    ZSTD_CCtx_getParameter, ZSTD_CCtx_setCParams, ZSTD_CCtx_setParameter, ZSTD_CCtx_setParams,
    ZSTD_CCtx_setPledgedSrcSize, ZSTD_DCtx_setFormat, ZSTD_cParam_getBounds, ZSTD_getParams,
    ZSTD_parameters,
};
use crate::simple::ZSTD_compressBound;
use crate::streaming::{ZSTD_compressStream2, ZSTD_inBuffer, ZSTD_outBuffer};

const LEVEL: c_int = 100;
const WINDOW_LOG: c_int = 101;
const STRATEGY: c_int = 107;
const TARGET_CBLOCK_SIZE: c_int = 130;
const ENABLE_LDM: c_int = 160;
const CHECKSUM: c_int = 201;
const DICT_ID: c_int = 202;
const NB_WORKERS: c_int = 400;
const JOB_SIZE: c_int = 401;
const FORMAT: c_int = 10;
const FORCE_MAX_WINDOW: c_int = 1000;
const FORCE_ATTACH_DICT: c_int = 1001;
const LITERAL_COMPRESSION_MODE: c_int = 1002;
const SRC_SIZE_HINT: c_int = 1004;
const MAX_BLOCK_SIZE: c_int = 1015;
const BLOCK_SPLITTER_LEVEL: c_int = 1017;

const NO_CUSTOM_MEM: ZSTD_customMem = ZSTD_customMem {
    customAlloc: core::ptr::null(),
    customFree: core::ptr::null(),
    opaque: core::ptr::null(),
};

/// A parameter set applied while a frame streams waits for the next frame,
/// as zstd.h promises: the frame under way keeps the settings it began with.
#[test]
fn parameter_sets_queue_behind_a_streaming_frame() {
    let cctx = ZSTD_createCCtx();
    let params = ZSTD_createCCtxParams();
    let input = b"queued parameters ".repeat(50);
    let mut first = vec![0u8; 4096];
    unsafe {
        ZSTD_CCtxParams_setParameter(params, CHECKSUM, 1);
        let mut out = ZSTD_outBuffer {
            dst: first.as_mut_ptr().cast(),
            size: first.len(),
            pos: 0,
        };
        let mut inp = ZSTD_inBuffer {
            src: input.as_ptr().cast(),
            size: input.len(),
            pos: 0,
        };
        assert_eq!(
            ZSTD_isError(ZSTD_compressStream2(cctx, &mut out, &mut inp, 0)),
            0
        );
        assert_eq!(ZSTD_CCtx_setParametersUsingCCtxParams(cctx, params), 0);
        assert_eq!(ZSTD_compressStream2(cctx, &mut out, &mut inp, 2), 0);
        first.truncate(out.pos);
    }
    // RFC 8878 3.1.1.1.1: Content_Checksum_Flag is bit 2 of the descriptor.
    assert_eq!(first[4] >> 2 & 1, 0, "the open frame keeps its settings");
    let mut second = vec![0u8; 4096];
    let n = unsafe {
        ZSTD_compress2(
            cctx,
            second.as_mut_ptr(),
            second.len(),
            input.as_ptr(),
            input.len(),
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    assert_eq!(second[4] >> 2 & 1, 1, "the next frame takes the queued set");
    unsafe {
        ZSTD_freeCCtxParams(params);
        ZSTD_freeCCtx(cctx);
    }
}

/// The on/off flags take any non-zero value as on and read back as 1, as
/// upstream coerces them (zstd_compress.c:835-846), on a context and on a
/// parameter set alike.
#[test]
fn on_off_flags_coerce_non_zero_values() {
    let cctx = ZSTD_createCCtx();
    let params = ZSTD_createCCtxParams();
    for flag in [200, CHECKSUM, DICT_ID] {
        for value in [2, -1, 7] {
            unsafe {
                assert_eq!(ZSTD_CCtx_setParameter(cctx, flag, value), 0);
                let mut read = -7;
                assert_eq!(ZSTD_CCtx_getParameter(cctx, flag, &mut read), 0);
                assert_eq!(read, 1, "flag {flag} = {value}");
                assert_eq!(ZSTD_CCtxParams_setParameter(params, flag, value), 0);
            }
            assert_eq!(get(params, flag), Ok(1));
        }
        unsafe { assert_eq!(ZSTD_CCtx_setParameter(cctx, flag, 0), 0) };
        let mut read = -7;
        unsafe { ZSTD_CCtx_getParameter(cctx, flag, &mut read) };
        assert_eq!(read, 0);
    }
    unsafe {
        ZSTD_freeCCtxParams(params);
        ZSTD_freeCCtx(cctx);
    }
}

fn get(params: *const ZSTD_CCtx_params, param: c_int) -> Result<c_int, ZSTD_ErrorCode> {
    let mut value = -7;
    let rc = unsafe { ZSTD_CCtxParams_getParameter(params, param, &mut value) };
    if ZSTD_isError(rc) != 0 {
        Err(code(rc))
    } else {
        Ok(value)
    }
}

fn get_cctx(cctx: *const ZSTD_CCtx, param: c_int) -> c_int {
    let mut value = -7;
    let rc = unsafe { ZSTD_CCtx_getParameter(cctx, param, &mut value) };
    assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
    value
}

/// Bytes that no match finder can shorten, so every block is stored raw and
/// its header carries its regenerated size.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect()
}

/// The `Block_Size` of every block in the frame `frame` starts with; `skip`
/// is the frame header's length.
fn block_sizes(frame: &[u8], skip: usize) -> Vec<usize> {
    let mut sizes = Vec::new();
    let mut at = skip;
    loop {
        let raw =
            u32::from(frame[at]) | u32::from(frame[at + 1]) << 8 | u32::from(frame[at + 2]) << 16;
        let size = (raw >> 3) as usize;
        sizes.push(size);
        // RLE blocks carry one byte, raw and compressed ones `size` bytes.
        at += 3 + if (raw >> 1) & 3 == 1 { 1 } else { size };
        if raw & 1 == 1 {
            return sizes;
        }
    }
}

fn compress2(cctx: *mut ZSTD_CCtx, input: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; ZSTD_compressBound(input.len())];
    let n = unsafe {
        ZSTD_compress2(
            cctx,
            out.as_mut_ptr(),
            out.len(),
            input.as_ptr(),
            input.len(),
        )
    };
    assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
    out.truncate(n);
    out
}

fn compress_stream(cctx: *mut ZSTD_CCtx, input: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; ZSTD_compressBound(input.len()) + 64];
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
    let rc = unsafe { ZSTD_compressStream2(cctx, &mut outb, &mut inb, 2) };
    assert_eq!(rc, 0, "{:?}", code(rc));
    out.truncate(outb.pos);
    out
}

fn decompress(frame: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    unsafe {
        let dctx = ZSTD_createDCtx();
        let n = ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        );
        ZSTD_freeDCtx(dctx);
        assert_eq!(n, len, "{:?}", code(n));
    }
    out
}

/// A free-standing set stores what upstream's stores: the level clamped, the
/// LDM switch as a tri-state, a block target raised to its minimum, the
/// worker knobs refused in a single-threaded build.
#[test]
fn parameter_set_stores_like_upstream() {
    let params = ZSTD_createCCtxParams();
    assert!(!params.is_null());
    unsafe {
        assert_eq!(ZSTD_CCtxParams_setParameter(params, LEVEL, 100), 0);
        assert_eq!(get(params, LEVEL), Ok(22));
        assert_eq!(ZSTD_CCtxParams_setParameter(params, ENABLE_LDM, 2), 0);
        assert_eq!(get(params, ENABLE_LDM), Ok(2));
        assert_eq!(
            ZSTD_CCtxParams_setParameter(params, TARGET_CBLOCK_SIZE, 100),
            0
        );
        assert_eq!(get(params, TARGET_CBLOCK_SIZE), Ok(1340));
        assert_eq!(
            code(ZSTD_CCtxParams_setParameter(params, MAX_BLOCK_SIZE, 100)),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
        assert_eq!(
            code(ZSTD_CCtxParams_setParameter(params, NB_WORKERS, 2)),
            ZSTD_ErrorCode::ZSTD_error_parameter_unsupported
        );
        assert_eq!(
            get(params, JOB_SIZE),
            Err(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported)
        );
        assert_eq!(ZSTD_CCtxParams_setParameter(params, FORCE_MAX_WINDOW, 7), 0);
        assert_eq!(get(params, FORCE_MAX_WINDOW), Ok(1));
        assert_eq!(
            code(ZSTD_CCtxParams_setParameter(params, FORCE_ATTACH_DICT, 4)),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
        assert_eq!(
            ZSTD_CCtxParams_setParameter(params, BLOCK_SPLITTER_LEVEL, 6),
            0
        );
        assert_eq!(get(params, BLOCK_SPLITTER_LEVEL), Ok(6));

        assert_eq!(ZSTD_CCtxParams_init(params, 0), 0);
        assert_eq!(get(params, LEVEL), Ok(0));
        assert_eq!(get(params, DICT_ID), Ok(1));
        assert_eq!(get(params, FORCE_MAX_WINDOW), Ok(0));

        assert_eq!(
            ZSTD_CCtxParams_init_advanced(params, ZSTD_getParams(19, 1000, 0)),
            0
        );
        assert_eq!(get(params, LEVEL), Ok(0));
        assert_eq!(get(params, WINDOW_LOG), Ok(10));
        assert_eq!(get(params, STRATEGY), Ok(9));
        let mut bad = ZSTD_getParams(19, 1000, 0);
        bad.cParams.strategy = 10;
        assert_eq!(
            code(ZSTD_CCtxParams_init_advanced(params, bad)),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );

        assert_eq!(ZSTD_CCtxParams_reset(params), 0);
        assert_eq!(get(params, LEVEL), Ok(3));
        assert_eq!(ZSTD_freeCCtxParams(params), 0);
        assert_eq!(ZSTD_freeCCtxParams(core::ptr::null_mut()), 0);
        assert_eq!(
            code(ZSTD_CCtxParams_init(core::ptr::null_mut(), 3)),
            ZSTD_ErrorCode::ZSTD_error_GENERIC
        );
    }
    // Every experimental parameter reports bounds.
    for param in [
        500, 10, 1000, 1001, 1002, 1004, 1005, 1006, 1007, 1008, 1009, 1010, 1011, 1012, 1013,
        1014, 1015, 1016, 1017,
    ] {
        let bounds = ZSTD_cParam_getBounds(param);
        assert_eq!(ZSTD_isError(bounds.error), 0, "param {param}");
        assert!(bounds.lowerBound <= bounds.upperBound, "param {param}");
    }
}

/// The context's own setter shares the storage rules.
#[test]
fn context_setter_clamps_the_level() {
    let cctx = ZSTD_createCCtx();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, LEVEL, 100), 0);
        assert_eq!(get_cctx(cctx, LEVEL), 22);
        assert_eq!(ZSTD_CCtx_setParameter(cctx, LEVEL, -1_000_000), 0);
        assert_eq!(get_cctx(cctx, LEVEL), -131_072);
        assert_eq!(ZSTD_CCtx_setParameter(cctx, ENABLE_LDM, 2), 0);
        assert_eq!(get_cctx(cctx, ENABLE_LDM), 2);
        ZSTD_freeCCtx(cctx);
    }
}

/// A magicless frame leaves out the four magic bytes, one-shot and streamed,
/// and decodes once the decoder is told the format.
#[test]
fn magicless_format_drops_the_magic_number() {
    let input = b"frames without a magic number ".repeat(50);
    let plain = compressed(&input);
    let cctx = ZSTD_createCCtx();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, FORMAT, 1), 0);
        assert_eq!(get_cctx(cctx, FORMAT), 1);
    }
    let oneshot = compress2(cctx, &input);
    let streamed = compress_stream(cctx, &input);
    unsafe { ZSTD_freeCCtx(cctx) };
    for frame in [&oneshot, &streamed] {
        assert_ne!(frame[..4], plain[..4]);
        let mut out = vec![0u8; input.len()];
        unsafe {
            let dctx = ZSTD_createDCtx();
            assert_eq!(ZSTD_DCtx_setFormat(dctx, 1), 0);
            let n = ZSTD_decompressDCtx(
                dctx,
                out.as_mut_ptr(),
                out.len(),
                frame.as_ptr(),
                frame.len(),
            );
            assert_eq!(n, input.len(), "{:?}", code(n));
            assert_eq!(stream_decode(dctx, frame).unwrap(), input);
            ZSTD_freeDCtx(dctx);
        }
        assert_eq!(out, input);
    }
}

/// Under `ZSTD_c_maxBlockSize` no block regenerates more than the maximum,
/// below the 1340-byte floor `ZSTD_c_targetCBlockSize` has.
#[test]
fn max_block_size_caps_every_block() {
    let input = noise(10_000);
    let cctx = ZSTD_createCCtx();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, MAX_BLOCK_SIZE, 1024), 0);
    }
    let oneshot = compress2(cctx, &input);
    let streamed = compress_stream(cctx, &input);
    // A one-shot entry point does not read the sticky parameters.
    let plain = {
        let mut out = vec![0u8; ZSTD_compressBound(input.len())];
        let n = unsafe {
            crate::context::ZSTD_compressCCtx(
                cctx,
                out.as_mut_ptr(),
                out.len(),
                input.as_ptr(),
                input.len(),
                3,
            )
        };
        out.truncate(n);
        out
    };
    unsafe { ZSTD_freeCCtx(cctx) };
    for frame in [&oneshot, &streamed] {
        let header = unsafe { crate::frame::ZSTD_frameHeaderSize(frame.as_ptr(), frame.len()) };
        let sizes = block_sizes(frame, header);
        assert_eq!(sizes.len(), 10, "{sizes:?}");
        assert!(sizes.iter().all(|&size| size <= 1024), "{sizes:?}");
        assert_eq!(decompress(frame, input.len()), input);
    }
    let header = unsafe { crate::frame::ZSTD_frameHeaderSize(plain.as_ptr(), plain.len()) };
    assert_eq!(block_sizes(&plain, header), [10_000]);
}

/// Turning literal compression off leaves the literals raw, which costs
/// bytes on text and still decodes.
#[test]
fn literal_compression_mode_reaches_the_encoder() {
    let text: Vec<u8> = (0..8192u32)
        .map(|i| b'a' + (i.wrapping_mul(2_654_435_761) >> 27) as u8)
        .collect();
    let cctx = ZSTD_createCCtx();
    let auto = compress2(cctx, &text);
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, LITERAL_COMPRESSION_MODE, 2), 0);
        assert_eq!(get_cctx(cctx, LITERAL_COMPRESSION_MODE), 2);
    }
    let raw = compress2(cctx, &text);
    unsafe { ZSTD_freeCCtx(cctx) };
    assert!(raw.len() > auto.len(), "{} <= {}", raw.len(), auto.len());
    assert_eq!(decompress(&raw, text.len()), text);
}

/// A size hint stays for every streamed frame and can be cleared again.
#[test]
fn src_size_hint_round_trips_streamed_frames() {
    let input = b"a small streamed frame ".repeat(20);
    let cctx = ZSTD_createCCtx();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, SRC_SIZE_HINT, 1000), 0);
        assert_eq!(get_cctx(cctx, SRC_SIZE_HINT), 1000);
    }
    let first = compress_stream(cctx, &input);
    let second = compress_stream(cctx, &input);
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, SRC_SIZE_HINT, 0), 0);
        assert_eq!(
            code(ZSTD_CCtx_setParameter(cctx, SRC_SIZE_HINT, -1)),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
    }
    let cleared = compress_stream(cctx, &input);
    unsafe { ZSTD_freeCCtx(cctx) };
    for frame in [&first, &second, &cleared] {
        assert_eq!(decompress(frame, input.len()), input);
    }
}

/// The seven compression parameters land together or not at all.
#[test]
fn set_cparams_is_all_or_nothing() {
    let cctx = ZSTD_createCCtx();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, WINDOW_LOG, 15), 0);
        let rc = ZSTD_CCtx_setCParams(cctx, cparams([20, 16, 17, 2, 4, 0, 10]));
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
        assert_eq!(get_cctx(cctx, WINDOW_LOG), 15);

        assert_eq!(
            ZSTD_CCtx_setCParams(cctx, cparams([20, 16, 17, 2, 4, 8, 5])),
            0
        );
        assert_eq!(get_cctx(cctx, WINDOW_LOG), 20);
        assert_eq!(get_cctx(cctx, STRATEGY), 5);

        let mut params = ZSTD_getParams(5, 0, 0);
        params.fParams.checksumFlag = 1;
        params.fParams.noDictIDFlag = 1;
        assert_eq!(ZSTD_CCtx_setParams(cctx, params), 0);
        assert_eq!(get_cctx(cctx, CHECKSUM), 1);
        assert_eq!(get_cctx(cctx, DICT_ID), 0);
        assert_eq!(
            get_cctx(cctx, WINDOW_LOG),
            params.cParams.windowLog as c_int
        );
    }
    let input = b"checksummed ".repeat(100);
    let frame = compress2(cctx, &input);
    unsafe { ZSTD_freeCCtx(cctx) };
    // Frame_Header_Descriptor bit 2: Content_Checksum_flag.
    assert_eq!(frame[4] & 0x04, 0x04);
    assert_eq!(decompress(&frame, input.len()), input);
}

/// A set built on its own replaces the context's parameters, keeps its
/// pledge, and is refused while a CDict is referenced.
#[test]
fn context_takes_a_parameter_set() {
    let params = ZSTD_createCCtxParams();
    let cctx = ZSTD_createCCtx();
    let dict = include_bytes!("../../../zstd/dict_tests/dictionary");
    unsafe {
        assert_eq!(ZSTD_CCtxParams_setParameter(params, LEVEL, 9), 0);
        assert_eq!(ZSTD_CCtxParams_setParameter(params, CHECKSUM, 1), 0);
        assert_eq!(ZSTD_CCtx_setPledgedSrcSize(cctx, 1200), 0);
        assert_eq!(ZSTD_CCtx_setParametersUsingCCtxParams(cctx, params), 0);
        assert_eq!(get_cctx(cctx, LEVEL), 9);
        assert_eq!(get_cctx(cctx, CHECKSUM), 1);
        let input = b"pledged ".repeat(150);
        let frame = compress2(cctx, &input);
        assert_eq!(decompress(&frame, input.len()), input);

        let cdict = crate::cdict::ZSTD_createCDict(dict.as_ptr(), dict.len(), 3);
        assert_eq!(crate::attach::ZSTD_CCtx_refCDict(cctx, cdict), 0);
        assert_eq!(
            code(ZSTD_CCtx_setParametersUsingCCtxParams(cctx, params)),
            ZSTD_ErrorCode::ZSTD_error_stage_wrong
        );
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCDict(cdict);
        ZSTD_freeCCtxParams(params);
    }
}

/// Explicit parameters drive the one-shot frame, the sticky ones do not, and
/// a dictionary passed alongside is used.
#[test]
fn compress_advanced_takes_explicit_parameters() {
    let dict = include_bytes!("../../../zstd/dict_tests/dictionary");
    let input: Vec<u8> = dict[dict.len() - 3000..].to_vec();
    let mut params: ZSTD_parameters = ZSTD_getParams(19, input.len() as u64, dict.len());
    params.fParams.checksumFlag = 1;
    let cctx = ZSTD_createCCtx();
    let mut out = vec![0u8; ZSTD_compressBound(input.len())];
    unsafe {
        // Sticky magicless must not leak into this entry point.
        assert_eq!(ZSTD_CCtx_setParameter(cctx, FORMAT, 1), 0);
        let n = ZSTD_compress_advanced(
            cctx,
            out.as_mut_ptr(),
            out.len(),
            input.as_ptr(),
            input.len(),
            dict.as_ptr(),
            dict.len(),
            params,
        );
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        out.truncate(n);
        assert_eq!(
            u32::from_le_bytes(out[..4].try_into().unwrap()),
            0xFD2F_B528
        );
        assert_eq!(out[4] & 0x04, 0x04);
        assert!(
            out.len() < input.len() / 4,
            "{} bytes: the dictionary was not used",
            out.len()
        );

        let mut restored = vec![0u8; input.len()];
        let dctx = ZSTD_createDCtx();
        let m = crate::attach::ZSTD_decompress_usingDict(
            dctx,
            restored.as_mut_ptr(),
            restored.len(),
            out.as_ptr(),
            out.len(),
            dict.as_ptr(),
            dict.len(),
        );
        assert_eq!(m, input.len(), "{:?}", code(m));
        assert_eq!(restored, input);
        ZSTD_freeDCtx(dctx);

        params.cParams.windowLog = 40;
        let mut spare = vec![0u8; 64];
        let rc = ZSTD_compress_advanced(
            cctx,
            spare.as_mut_ptr(),
            spare.len(),
            input.as_ptr(),
            input.len(),
            core::ptr::null(),
            0,
            params,
        );
        assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
        ZSTD_freeCCtx(cctx);
    }
}

/// A CDict made from a parameter set compresses under it.
#[test]
fn cdict_from_a_parameter_set_round_trips() {
    let dict = include_bytes!("../../../zstd/dict_tests/dictionary");
    let input: Vec<u8> = dict[dict.len() - 3000..].to_vec();
    let params = ZSTD_createCCtxParams();
    unsafe {
        assert_eq!(ZSTD_CCtxParams_setParameter(params, LEVEL, 19), 0);
        assert_eq!(ZSTD_CCtxParams_setParameter(params, WINDOW_LOG, 20), 0);
        let cdict =
            ZSTD_createCDict_advanced2(dict.as_ptr(), dict.len(), 0, 0, params, NO_CUSTOM_MEM);
        assert!(!cdict.is_null());
        assert!(
            ZSTD_createCDict_advanced2(
                dict.as_ptr(),
                dict.len(),
                0,
                0,
                core::ptr::null(),
                NO_CUSTOM_MEM
            )
            .is_null()
        );
        let cctx = ZSTD_createCCtx();
        let mut out = vec![0u8; ZSTD_compressBound(input.len())];
        let n = ZSTD_compress_usingCDict(
            cctx,
            out.as_mut_ptr(),
            out.len(),
            input.as_ptr(),
            input.len(),
            cdict,
        );
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        out.truncate(n);
        assert!(
            out.len() < input.len() / 4,
            "{} bytes: the dictionary was not used",
            out.len()
        );

        let ddict = ZSTD_createDDict(dict.as_ptr(), dict.len());
        let dctx = ZSTD_createDCtx();
        let mut restored = vec![0u8; input.len()];
        let m = ZSTD_decompress_usingDDict(
            dctx,
            restored.as_mut_ptr(),
            restored.len(),
            out.as_ptr(),
            out.len(),
            ddict,
        );
        assert_eq!(m, input.len(), "{:?}", code(m));
        assert_eq!(restored, input);
        ZSTD_freeDCtx(dctx);
        ZSTD_freeDDict(ddict);
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCDict(cdict);
        ZSTD_freeCCtxParams(params);
    }
}

/// An empty dictionary makes a CDict that carries its parameters and no
/// content, as upstream's does (zstd_compress.c:5562-5598); only fullDict,
/// which names a dictionary it does not get, refuses it (:5206-5207).
#[test]
fn an_empty_cdict_carries_parameters_only() {
    let input: Vec<u8> = b"compressed under a parameter-only cdict "
        .iter()
        .copied()
        .cycle()
        .take(1 << 20)
        .collect();
    let default_mem = ZSTD_customMem {
        customAlloc: core::ptr::null(),
        customFree: core::ptr::null(),
        opaque: core::ptr::null(),
    };
    let content = b"raw dictionary content the frame never matches ".repeat(20);
    let params = ZSTD_createCCtxParams();
    let cctx = ZSTD_createCCtx();
    // The window the frame of `input` declares under `cdict`; the frame
    // decodes with no dictionary when `standalone`.
    let window_under = |cdict: *mut crate::cdict::ZSTD_CDict, standalone: bool| unsafe {
        let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
        let n = ZSTD_compress_usingCDict(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            cdict,
        );
        assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
        let mut header = core::mem::MaybeUninit::<crate::frame::ZSTD_FrameHeader>::uninit();
        let rc = crate::frame::ZSTD_getFrameHeader(header.as_mut_ptr(), frame.as_ptr(), n);
        assert_eq!(rc, 0, "{:?}", code(rc));
        let header = header.assume_init();
        assert_eq!(header.dictID, 0);
        if standalone {
            let mut out = vec![0u8; input.len()];
            let m = crate::simple::ZSTD_decompress(out.as_mut_ptr(), out.len(), frame.as_ptr(), n);
            assert_eq!(m, input.len(), "{:?}", code(m));
            assert_eq!(out, input);
        }
        header.windowSize
    };
    unsafe {
        assert_eq!(ZSTD_CCtxParams_setParameter(params, WINDOW_LOG, 10), 0);
        let plain = crate::cdict::ZSTD_createCDict(core::ptr::null(), 0, 3);
        let empty = ZSTD_createCDict_advanced2(core::ptr::null(), 0, 0, 0, params, default_mem);
        let full = ZSTD_createCDict_advanced2(core::ptr::null(), 0, 0, 2, params, default_mem);
        let filled =
            ZSTD_createCDict_advanced2(content.as_ptr(), content.len(), 0, 1, params, default_mem);
        assert!(!plain.is_null() && !empty.is_null() && !filled.is_null());
        assert!(full.is_null(), "fullDict over nothing is refused");

        let plain_window = window_under(plain, true);
        let empty_window = window_under(empty, true);
        assert_eq!(
            empty_window,
            window_under(filled, false),
            "an empty CDict runs its parameters as a filled one does"
        );
        assert!(
            empty_window < plain_window,
            "{empty_window} vs {plain_window}"
        );
        for cdict in [plain, empty, filled] {
            ZSTD_freeCDict(cdict);
        }
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCCtxParams(params);
    }
}

/// A CDict's explicit window is raised toward a source of known size, as far
/// as 512 KiB, as upstream raises it (zstd_compress.c:5844-5852): a 1 KiB
/// window over 1 MiB declares 512 KiB, through both usingCDict calls.
#[test]
fn a_cdict_window_is_raised_toward_the_source() {
    let input: Vec<u8> = b"a source far larger than the cdict's window "
        .iter()
        .copied()
        .cycle()
        .take(1 << 20)
        .collect();
    let content = b"raw dictionary content for the window test ".repeat(20);
    unsafe {
        let params = ZSTD_createCCtxParams();
        assert_eq!(ZSTD_CCtxParams_setParameter(params, WINDOW_LOG, 10), 0);
        let cdict = ZSTD_createCDict_advanced2(
            content.as_ptr(),
            content.len(),
            0,
            1,
            params,
            ZSTD_customMem {
                customAlloc: core::ptr::null(),
                customFree: core::ptr::null(),
                opaque: core::ptr::null(),
            },
        );
        assert!(!cdict.is_null());
        let cctx = ZSTD_createCCtx();
        for advanced in [false, true] {
            let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
            let n = if advanced {
                crate::cdict::ZSTD_compress_usingCDict_advanced(
                    cctx,
                    frame.as_mut_ptr(),
                    frame.len(),
                    input.as_ptr(),
                    input.len(),
                    cdict,
                    crate::cdict::ZSTD_frameParameters {
                        contentSizeFlag: 1,
                        checksumFlag: 0,
                        noDictIDFlag: 0,
                    },
                )
            } else {
                ZSTD_compress_usingCDict(
                    cctx,
                    frame.as_mut_ptr(),
                    frame.len(),
                    input.as_ptr(),
                    input.len(),
                    cdict,
                )
            };
            assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
            let mut header = core::mem::MaybeUninit::<crate::frame::ZSTD_FrameHeader>::uninit();
            let rc = crate::frame::ZSTD_getFrameHeader(header.as_mut_ptr(), frame.as_ptr(), n);
            assert_eq!(rc, 0, "{:?}", code(rc));
            assert_eq!(
                header.assume_init().windowSize,
                1 << 19,
                "advanced: {advanced}"
            );
        }
        ZSTD_freeCDict(cdict);
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCCtxParams(params);
    }
}

/// The estimates read the set: a wider window costs more, and the streaming
/// budget covers the one-shot one.
#[test]
fn estimates_follow_the_parameter_set() {
    let params = ZSTD_createCCtxParams();
    unsafe {
        assert_eq!(ZSTD_CCtxParams_setParameter(params, WINDOW_LOG, 17), 0);
        let narrow = ZSTD_estimateCCtxSize_usingCCtxParams(params);
        assert_eq!(ZSTD_isError(narrow), 0, "{:?}", code(narrow));
        assert_eq!(ZSTD_CCtxParams_setParameter(params, WINDOW_LOG, 24), 0);
        let wide = ZSTD_estimateCCtxSize_usingCCtxParams(params);
        assert!(wide > narrow, "{wide} <= {narrow}");
        let stream = ZSTD_estimateCStreamSize_usingCCtxParams(params);
        assert!(stream > wide, "{stream} <= {wide}");
        assert_eq!(
            code(ZSTD_estimateCCtxSize_usingCCtxParams(core::ptr::null())),
            ZSTD_ErrorCode::ZSTD_error_GENERIC
        );
        ZSTD_freeCCtxParams(params);
    }
}
