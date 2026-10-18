//! The experimental (`ZSTD_STATIC_LINKING_ONLY`) entry points. Expected values
//! marked "upstream" were read from `libzstd` 1.5.7 called with the same
//! arguments.

use crate::cdict::{
    ZSTD_compressionParameters, ZSTD_createCDict, ZSTD_createDDict, ZSTD_freeCDict, ZSTD_freeDDict,
    ZSTD_sizeof_CDict, ZSTD_sizeof_DDict,
};
use crate::context::{ZSTD_createDCtx, ZSTD_decompressDCtx, ZSTD_freeDCtx};
use crate::error::{ZSTD_ErrorCode, ZSTD_getErrorCode, ZSTD_isError};
use crate::estimate::{
    ZSTD_estimateCDictSize, ZSTD_estimateCDictSize_advanced, ZSTD_estimateDDictSize,
    ZSTD_estimateDStreamSize, ZSTD_estimateDStreamSize_fromFrame,
};
use crate::frame::{
    ZSTD_decodingBufferSize_min, ZSTD_decompressionMargin, ZSTD_isFrame, ZSTD_isSkippableFrame,
    ZSTD_readSkippableFrame, ZSTD_writeSkippableFrame,
};
use crate::params::{
    ZSTD_DCtx_getParameter, ZSTD_DCtx_reset, ZSTD_DCtx_setFormat, ZSTD_DCtx_setMaxWindowSize,
    ZSTD_DCtx_setParameter, ZSTD_adjustCParams, ZSTD_checkCParams, ZSTD_dParam_getBounds,
    ZSTD_getCParams, ZSTD_getParams,
};
use crate::simple::{ZSTD_compress, ZSTD_compressBound};
use crate::streaming::{ZSTD_decompressStream, ZSTD_inBuffer, ZSTD_outBuffer};

/// A frame of two raw blocks (`abc`, `de`) under a 1 KiB window with a
/// checksum, followed by a skippable frame of variant 2 carrying `vwxyz`.
const FRAMES: [u8; 34] = [
    0x28, 0xB5, 0x2F, 0xFD, 0x04, 0x00, 0x18, 0, 0, b'a', b'b', b'c', 0x11, 0, 0, b'd', b'e', 1, 2,
    3, 4, 0x52, 0x2A, 0x4D, 0x18, 5, 0, 0, 0, b'v', b'w', b'x', b'y', b'z',
];
/// Where the skippable frame starts in [`FRAMES`].
const SKIPPABLE_AT: usize = 21;

pub(super) fn code(rc: usize) -> ZSTD_ErrorCode {
    ZSTD_getErrorCode(rc)
}

pub(super) fn cparams(p: [u32; 7]) -> ZSTD_compressionParameters {
    ZSTD_compressionParameters {
        windowLog: p[0],
        chainLog: p[1],
        hashLog: p[2],
        searchLog: p[3],
        minMatch: p[4],
        targetLength: p[5],
        strategy: p[6],
    }
}

fn fields(p: ZSTD_compressionParameters) -> [u32; 7] {
    [
        p.windowLog,
        p.chainLog,
        p.hashLog,
        p.searchLog,
        p.minMatch,
        p.targetLength,
        p.strategy,
    ]
}

/// The level table, tiered by source size and dictionary, answers as
/// upstream's does: callers size buffers and pick parameters from it.
#[test]
fn get_cparams_matches_upstream() {
    // (level, source size, dictionary size) -> upstream's answer.
    let cases: [(i32, u64, usize, [u32; 7]); 20] = [
        (-5, 0, 0, [19, 12, 13, 1, 6, 5, 1]),
        (-5, 1000, 4000, [13, 12, 13, 1, 5, 5, 1]),
        (1, 0, 4000, [14, 14, 15, 1, 5, 0, 1]),
        (1, 20000, 0, [15, 12, 13, 1, 6, 0, 1]),
        (3, 0, 0, [21, 16, 17, 1, 5, 0, 2]),
        (3, 1000, 0, [10, 10, 11, 2, 4, 0, 2]),
        (3, 200000, 4000, [18, 16, 16, 1, 4, 0, 2]),
        (3, 1000000, 0, [20, 16, 17, 1, 5, 0, 2]),
        (9, 0, 0, [22, 20, 21, 4, 5, 16, 5]),
        (9, 0, 4000, [14, 15, 14, 5, 4, 8, 6]),
        (9, 1000, 0, [10, 11, 11, 5, 4, 8, 6]),
        (13, 0, 0, [22, 22, 22, 4, 5, 32, 6]),
        (13, 20000, 0, [15, 16, 16, 3, 4, 12, 7]),
        (13, 1000, 4000, [13, 14, 14, 5, 3, 32, 8]),
        (19, 0, 0, [23, 24, 22, 7, 3, 256, 9]),
        (19, 200000, 0, [18, 19, 19, 8, 3, 256, 9]),
        (22, 0, 0, [27, 27, 25, 9, 3, 999, 9]),
        (22, 1000000, 4000, [20, 21, 21, 9, 3, 999, 9]),
        (30, 0, 0, [27, 27, 25, 9, 3, 999, 9]),
        (30, 20000, 4000, [15, 16, 16, 11, 3, 999, 9]),
    ];
    for (level, size, dict, expected) in cases {
        assert_eq!(
            fields(ZSTD_getCParams(level, size, dict)),
            expected,
            "level {level} size {size} dict {dict}"
        );
    }
    // ZSTD_CONTENTSIZE_UNKNOWN reads as unknown, like 0.
    assert_eq!(
        fields(ZSTD_getCParams(3, u64::MAX, 0)),
        fields(ZSTD_getCParams(3, 0, 0))
    );
    let params = ZSTD_getParams(19, 1000, 0);
    assert_eq!(fields(params.cParams), [10, 11, 11, 8, 3, 256, 9]);
    assert_eq!(
        (
            params.fParams.contentSizeFlag,
            params.fParams.checksumFlag,
            params.fParams.noDictIDFlag
        ),
        (1, 0, 0)
    );
}

/// Adjusting clamps every field into its bounds first, then fits the window
/// and tables to the source and dictionary, as upstream does.
#[test]
fn adjust_cparams_matches_upstream() {
    let ultra = cparams([27, 27, 25, 9, 3, 999, 9]);
    assert_eq!(
        fields(ZSTD_adjustCParams(ultra, 1000, 0)),
        [10, 11, 11, 9, 3, 999, 9]
    );
    assert_eq!(
        fields(ZSTD_adjustCParams(ultra, 1000, 4000)),
        [13, 14, 14, 9, 3, 999, 9]
    );
    assert_eq!(fields(ZSTD_adjustCParams(ultra, 0, 0)), fields(ultra));
    assert_eq!(
        fields(ZSTD_adjustCParams(cparams([0; 7]), 0, 0)),
        [10, 6, 6, 1, 3, 0, 1]
    );
    // Lazy with a 30-bit hash and 64-entry rows still fits the 32-bit row hash.
    let lazy = cparams([20, 20, 30, 6, 4, 0, 4]);
    assert_eq!(fields(ZSTD_adjustCParams(lazy, 0, 0)), fields(lazy));
    // With 16-entry rows it does not, and the hash comes down to 28 bits.
    let narrow_rows = cparams([20, 20, 30, 1, 4, 0, 4]);
    assert_eq!(ZSTD_adjustCParams(narrow_rows, 0, 0).hashLog, 28);
    // Upstream keeps windowLog 31 on 64-bit; this encoder's ceiling is 30.
    assert_eq!(
        fields(ZSTD_adjustCParams(
            cparams([31, 30, 30, 30, 9, 200_000, 12]),
            0,
            0
        )),
        [30, 30, 30, 30, 7, 131_072, 9]
    );
}

/// Unlike the setters, the check takes 0 as a value, which is below most
/// fields' minimum.
#[test]
fn check_cparams_takes_zero_literally() {
    assert_eq!(ZSTD_checkCParams(cparams([27, 27, 25, 9, 3, 999, 9])), 0);
    assert_eq!(
        code(ZSTD_checkCParams(cparams([0; 7]))),
        ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
    );
    assert_eq!(
        code(ZSTD_checkCParams(cparams([20, 20, 20, 5, 4, 0, 10]))),
        ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
    );
}

#[test]
fn frame_magic_queries() {
    unsafe {
        assert_eq!(ZSTD_isFrame(FRAMES.as_ptr(), FRAMES.len()), 1);
        assert_eq!(ZSTD_isSkippableFrame(FRAMES.as_ptr(), FRAMES.len()), 0);
        let skippable = &FRAMES[SKIPPABLE_AT..];
        assert_eq!(ZSTD_isFrame(skippable.as_ptr(), skippable.len()), 1);
        assert_eq!(ZSTD_isSkippableFrame(skippable.as_ptr(), 4), 1);
        // Three bytes are never enough to tell.
        assert_eq!(ZSTD_isFrame(FRAMES.as_ptr(), 3), 0);
        assert_eq!(ZSTD_isFrame(b"PK\x03\x04".as_ptr(), 4), 0);
        assert_eq!(ZSTD_isFrame(core::ptr::null(), 0), 0);
    }
}

/// Reading a skippable frame copies the payload and reports the variant;
/// the errors arrive in upstream's order.
#[test]
fn read_skippable_frame_payload_and_errors() {
    let skippable = &FRAMES[SKIPPABLE_AT..];
    let mut out = [0u8; 8];
    let mut variant = 99u32;
    unsafe {
        let n = ZSTD_readSkippableFrame(
            out.as_mut_ptr(),
            out.len(),
            &mut variant,
            skippable.as_ptr(),
            skippable.len(),
        );
        assert_eq!(n, 5);
        assert_eq!(&out[..5], b"vwxyz");
        assert_eq!(variant, 2);
        // The variant pointer is optional.
        let n = ZSTD_readSkippableFrame(
            out.as_mut_ptr(),
            out.len(),
            core::ptr::null_mut(),
            skippable.as_ptr(),
            skippable.len(),
        );
        assert_eq!(n, 5);
        let read = |cap: usize, src: &[u8]| {
            code(ZSTD_readSkippableFrame(
                out.as_ptr().cast_mut(),
                cap,
                core::ptr::null_mut(),
                src.as_ptr(),
                src.len(),
            ))
        };
        assert_eq!(
            read(4, skippable),
            ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall
        );
        assert_eq!(
            read(8, &FRAMES[..SKIPPABLE_AT]),
            ZSTD_ErrorCode::ZSTD_error_frameParameter_unsupported
        );
        assert_eq!(
            read(8, &skippable[..12]),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );
        assert_eq!(
            read(8, &FRAMES[..7]),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );
    }
}

/// A `NULL` destination with room claimed only measures the payload, as
/// upstream copies only into a non-`NULL` one.
#[test]
fn read_skippable_frame_into_null_measures_the_payload() {
    let skippable = &FRAMES[SKIPPABLE_AT..];
    let n = unsafe {
        ZSTD_readSkippableFrame(
            core::ptr::null_mut(),
            8,
            core::ptr::null_mut(),
            skippable.as_ptr(),
            skippable.len(),
        )
    };
    assert_eq!(n, 5);
}

#[test]
fn write_skippable_frame_layout_and_errors() {
    let mut out = [0u8; 16];
    unsafe {
        let n = ZSTD_writeSkippableFrame(out.as_mut_ptr(), out.len(), b"hello".as_ptr(), 5, 15);
        assert_eq!(n, 13);
        assert_eq!(&out[..13], b"\x5F\x2A\x4D\x18\x05\x00\x00\x00hello");
        assert_eq!(
            code(ZSTD_writeSkippableFrame(
                out.as_mut_ptr(),
                out.len(),
                b"hello".as_ptr(),
                5,
                16
            )),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
        assert_eq!(
            code(ZSTD_writeSkippableFrame(
                out.as_mut_ptr(),
                12,
                b"hello".as_ptr(),
                5,
                1
            )),
            ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall
        );
        // An empty payload is a valid frame.
        let n = ZSTD_writeSkippableFrame(out.as_mut_ptr(), 8, core::ptr::null(), 0, 0);
        assert_eq!(n, 8);
    }
}

/// The in-place layout `ZSTD_decompressionMargin` sizes, the frame stored at
/// the end of the output buffer, decodes through every one-shot entry point.
#[test]
fn one_shots_decode_in_place() {
    // A ready frame, so the test reaches only the decoders: single segment,
    // content size 5, one last raw block.
    let input = b"abcde";
    let frame = &[
        0x28, 0xB5, 0x2F, 0xFD, 0x20, 5, 0x29, 0, 0, b'a', b'b', b'c', b'd', b'e',
    ][..];
    let margin = unsafe { ZSTD_decompressionMargin(frame.as_ptr(), frame.len()) };
    assert_eq!(ZSTD_isError(margin), 0);
    let capacity = input.len() + margin;
    let at = capacity - frame.len();
    let decode = |run: &dyn Fn(*mut u8, usize, *const u8, usize) -> usize| {
        let mut buffer = vec![0u8; capacity];
        buffer[at..].copy_from_slice(frame);
        let base = buffer.as_mut_ptr();
        let written = run(base, capacity, unsafe { base.add(at) }, frame.len());
        assert_eq!(written, input.len(), "{:?}", code(written));
        assert_eq!(&buffer[..written], input.as_slice());
    };
    let dctx = ZSTD_createDCtx();
    unsafe {
        decode(&|dst, cap, src, len| crate::simple::ZSTD_decompress(dst, cap, src, len));
        decode(&|dst, cap, src, len| ZSTD_decompressDCtx(dctx, dst, cap, src, len));
        decode(&|dst, cap, src, len| {
            crate::attach::ZSTD_decompress_usingDict(dctx, dst, cap, src, len, core::ptr::null(), 0)
        });
        decode(&|dst, cap, src, len| {
            crate::cdict::ZSTD_decompress_usingDDict(dctx, dst, cap, src, len, core::ptr::null())
        });
        ZSTD_freeDCtx(dctx);
    }
}

/// Each frame costs its header, checksum and three bytes per block, a
/// skippable frame its whole length, plus one largest block (upstream: 1053
/// for both frames, 1040 for the first alone).
#[test]
fn decompression_margin_matches_upstream() {
    unsafe {
        assert_eq!(
            ZSTD_decompressionMargin(FRAMES.as_ptr(), FRAMES.len()),
            1053
        );
        assert_eq!(
            ZSTD_decompressionMargin(FRAMES.as_ptr(), SKIPPABLE_AT),
            1040
        );
        assert_eq!(
            code(ZSTD_decompressionMargin(FRAMES.as_ptr(), SKIPPABLE_AT - 1)),
            ZSTD_ErrorCode::ZSTD_error_corruption_detected
        );
        assert_eq!(
            code(ZSTD_decompressionMargin(b"PK\x03\x04".as_ptr(), 4)),
            ZSTD_ErrorCode::ZSTD_error_prefix_unknown
        );
    }
}

#[test]
fn decoding_buffer_size_matches_upstream() {
    assert_eq!(ZSTD_decodingBufferSize_min(1 << 20, u64::MAX), 1_310_784);
    assert_eq!(ZSTD_decodingBufferSize_min(1 << 20, 1000), 1000);
    assert_eq!(ZSTD_decodingBufferSize_min(100, u64::MAX), 364);
    #[cfg(target_pointer_width = "32")]
    assert_eq!(
        code(ZSTD_decodingBufferSize_min(1 << 40, u64::MAX)),
        ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge
    );
}

#[test]
fn estimate_dstream_size_from_frame_header() {
    unsafe {
        assert_eq!(
            ZSTD_estimateDStreamSize_fromFrame(FRAMES.as_ptr(), 6),
            ZSTD_estimateDStreamSize(1024)
        );
        assert_eq!(
            code(ZSTD_estimateDStreamSize_fromFrame(FRAMES.as_ptr(), 5)),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );
        let skippable = &FRAMES[SKIPPABLE_AT..];
        assert_eq!(
            ZSTD_estimateDStreamSize_fromFrame(skippable.as_ptr(), skippable.len()),
            ZSTD_estimateDStreamSize(0)
        );
    }
}

/// A dictionary a context keeps for itself is part of its size, however it
/// came to keep it: a buffer-less begin holds one beside the frame it opened,
/// and a decoder keeps the dictionary of the last frame it decoded.
#[test]
fn context_sizes_count_the_dictionaries_they_keep() {
    use crate::bufferless::{
        ZSTD_compressBegin, ZSTD_compressBegin_usingDict, ZSTD_decompressBegin,
        ZSTD_decompressBegin_usingDict,
    };
    use crate::context::{ZSTD_createCCtx, ZSTD_freeCCtx, ZSTD_sizeof_CCtx, ZSTD_sizeof_DCtx};
    let dict = include_bytes!("../../../zstd/dict_tests/dictionary");
    unsafe {
        let plain = ZSTD_createCCtx();
        let primed = ZSTD_createCCtx();
        assert_eq!(ZSTD_compressBegin(plain, 3), 0);
        assert_eq!(
            ZSTD_compressBegin_usingDict(primed, dict.as_ptr(), dict.len(), 3),
            0
        );
        let kept = ZSTD_sizeof_CCtx(primed) - ZSTD_sizeof_CCtx(plain);
        assert!(kept >= dict.len(), "a begun dictionary adds {kept} bytes");
        ZSTD_freeCCtx(plain);
        ZSTD_freeCCtx(primed);

        let plain = ZSTD_createDCtx();
        let primed = ZSTD_createDCtx();
        assert_eq!(ZSTD_decompressBegin(plain), 0);
        assert_eq!(
            ZSTD_decompressBegin_usingDict(primed, dict.as_ptr(), dict.len()),
            0
        );
        let kept = ZSTD_sizeof_DCtx(primed) - ZSTD_sizeof_DCtx(plain);
        assert!(kept >= dict.len(), "a begun dictionary adds {kept} bytes");
        ZSTD_freeDCtx(plain);
        ZSTD_freeDCtx(primed);

        // A one-shot leaves its dictionary with the decoder.
        let input = b"decoded once with a dictionary ".repeat(20);
        let cctx = ZSTD_createCCtx();
        let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
        let n = crate::attach::ZSTD_compress_usingDict(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            dict.as_ptr(),
            dict.len(),
            3,
        );
        assert_eq!(ZSTD_isError(n), 0);
        ZSTD_freeCCtx(cctx);
        let dctx = ZSTD_createDCtx();
        let before = ZSTD_sizeof_DCtx(dctx);
        let mut out = vec![0u8; input.len()];
        let m = crate::attach::ZSTD_decompress_usingDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            n,
            dict.as_ptr(),
            dict.len(),
        );
        assert_eq!(m, input.len());
        let kept = ZSTD_sizeof_DCtx(dctx) - before;
        assert!(
            kept >= dict.len(),
            "a one-shot's dictionary adds {kept} bytes"
        );
        ZSTD_freeDCtx(dctx);
    }
}

/// The dictionary estimates bound what creating the dictionary then takes,
/// so a caller budgeting by them is never overrun.
#[test]
fn dictionary_estimates_bound_the_created_objects() {
    let dict = include_bytes!("../../../zstd/dict_tests/dictionary");
    unsafe {
        let cdict = ZSTD_createCDict(dict.as_ptr(), dict.len(), 19);
        assert!(!cdict.is_null());
        let held = ZSTD_sizeof_CDict(cdict);
        ZSTD_freeCDict(cdict);
        assert!(held <= ZSTD_estimateCDictSize(dict.len(), 19), "{held}");
        assert_eq!(
            ZSTD_estimateCDictSize_advanced(dict.len(), ZSTD_getCParams(19, 0, dict.len()), 0),
            ZSTD_estimateCDictSize(dict.len(), 19)
        );

        let ddict = ZSTD_createDDict(dict.as_ptr(), dict.len());
        let held = ZSTD_sizeof_DDict(ddict);
        // The parsed dictionary lives in a shared allocation, and that
        // allocation carries two counts ahead of the value: they are heap the
        // DDict holds too. Read through a checked reference, which is live
        // until the free below.
        let parsed = ddict
            .as_ref()
            .expect("a dictionary that parses makes a DDict")
            .prepared
            .as_ref()
            .unwrap()
            .as_dict();
        let shared =
            2 * core::mem::size_of::<usize>() + core::mem::size_of::<codec::decoding::Dictionary>();
        assert!(
            held >= core::mem::size_of::<crate::cdict::ZSTD_DDict>() + shared + parsed.heap_bytes(),
            "{held} leaves out the shared allocation"
        );
        ZSTD_freeDDict(ddict);
        assert!(held <= ZSTD_estimateDDictSize(dict.len(), 0), "{held}");
    }
}

pub(super) fn compressed(input: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; ZSTD_compressBound(input.len())];
    let n = unsafe { ZSTD_compress(out.as_mut_ptr(), out.len(), input.as_ptr(), input.len(), 3) };
    assert_eq!(ZSTD_isError(n), 0);
    out.truncate(n);
    out
}

pub(super) fn stream_decode(
    dctx: *mut crate::context::ZSTD_DCtx,
    src: &[u8],
) -> Result<Vec<u8>, usize> {
    let mut out = vec![0u8; 1 << 16];
    let mut inb = ZSTD_inBuffer {
        src: src.as_ptr().cast(),
        size: src.len(),
        pos: 0,
    };
    let mut outb = ZSTD_outBuffer {
        dst: out.as_mut_ptr().cast(),
        size: out.len(),
        pos: 0,
    };
    loop {
        let (in_before, out_before) = (inb.pos, outb.pos);
        let rc = unsafe { ZSTD_decompressStream(dctx, &mut outb, &mut inb) };
        if ZSTD_isError(rc) != 0 {
            return Err(rc);
        }
        if rc == 0 {
            out.truncate(outb.pos);
            return Ok(out);
        }
        // A call that moves neither position would repeat forever.
        assert!(
            inb.pos > in_before || outb.pos > out_before,
            "decoder stalled"
        );
    }
}

/// A magicless frame decodes once the context is told the format, through
/// the one-shot and the streaming entry points alike, and a parameter reset
/// restores the magic.
#[test]
fn dctx_format_reads_magicless_frames() {
    let input = b"magicless frames start at the frame header ".repeat(40);
    let frame = compressed(&input);
    let magicless = &frame[4..];
    let mut out = vec![0u8; input.len()];
    unsafe {
        let dctx = ZSTD_createDCtx();
        let n = ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            magicless.as_ptr(),
            magicless.len(),
        );
        assert_eq!(code(n), ZSTD_ErrorCode::ZSTD_error_prefix_unknown);

        assert_eq!(ZSTD_DCtx_setFormat(dctx, 1), 0);
        let mut format = -1;
        assert_eq!(ZSTD_DCtx_getParameter(dctx, 1000, &mut format), 0);
        assert_eq!(format, 1);
        let n = ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            magicless.as_ptr(),
            magicless.len(),
        );
        assert_eq!(n, input.len(), "{:?}", code(n));
        assert_eq!(out, input);
        assert_eq!(stream_decode(dctx, magicless).unwrap(), input);

        assert_eq!(
            code(ZSTD_DCtx_setFormat(dctx, 2)),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
        let bounds = ZSTD_dParam_getBounds(1000);
        assert_eq!(
            (bounds.error, bounds.lowerBound, bounds.upperBound),
            (0, 0, 1)
        );

        assert_eq!(ZSTD_DCtx_reset(dctx, 2), 0);
        assert_eq!(ZSTD_DCtx_getParameter(dctx, 1000, &mut format), 0);
        assert_eq!(format, 0);
        assert_eq!(stream_decode(dctx, &frame).unwrap(), input);
        ZSTD_freeDCtx(dctx);
    }
}

/// The byte-exact ceiling admits a window equal to it and refuses one past
/// it; the log reads back as the power of two at or below it.
#[test]
fn dctx_max_window_size_is_byte_exact() {
    // Magic | descriptor (no FCS, not single-segment) | window log 28 | last
    // raw block of 3 bytes | "abc".
    let mut frame = 0xFD2F_B528u32.to_le_bytes().to_vec();
    frame.extend_from_slice(&[0x00, (28 - 10) << 3, 0x19, 0x00, 0x00]);
    frame.extend_from_slice(b"abc");
    unsafe {
        let dctx = ZSTD_createDCtx();
        assert_eq!(ZSTD_DCtx_setMaxWindowSize(dctx, (1 << 28) - 1), 0);
        let mut log = 0;
        assert_eq!(ZSTD_DCtx_getParameter(dctx, 100, &mut log), 0);
        assert_eq!(log, 27);
        assert_eq!(
            code(stream_decode(dctx, &frame).unwrap_err()),
            ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge
        );
        assert_eq!(ZSTD_DCtx_reset(dctx, 1), 0);
        assert_eq!(ZSTD_DCtx_setMaxWindowSize(dctx, 1 << 28), 0);
        assert_eq!(stream_decode(dctx, &frame).unwrap(), b"abc");

        assert_eq!(
            code(ZSTD_DCtx_setMaxWindowSize(dctx, 1023)),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
        // The log setter and the byte setter drive the same ceiling.
        assert_eq!(ZSTD_DCtx_setParameter(dctx, 100, 20), 0);
        assert_eq!(ZSTD_DCtx_getParameter(dctx, 100, &mut log), 0);
        assert_eq!(log, 20);
        ZSTD_freeDCtx(dctx);
    }
}
