//! The sequence entry points. Expected values marked "upstream" were read
//! from `libzstd` 1.5.7 driven the same way.

use core::ffi::{c_int, c_void};

use super::experimental::code;
use crate::attach::{ZSTD_CCtx_loadDictionary, ZSTD_decompress_usingDict};
use crate::cctx_params::{
    ZSTD_CCtx_setParametersUsingCCtxParams, ZSTD_CCtxParams_setParameter, ZSTD_createCCtxParams,
    ZSTD_freeCCtxParams,
};
use crate::context::{
    ZSTD_CCtx, ZSTD_compress2, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_freeCCtx, ZSTD_freeDCtx,
};
use crate::error::{ZSTD_ErrorCode, ZSTD_isError};
use crate::params::{
    ZSTD_C_BLOCK_DELIMITERS, ZSTD_C_CHECKSUM_FLAG, ZSTD_C_ENABLE_LDM,
    ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK, ZSTD_C_TARGET_CBLOCK_SIZE, ZSTD_CCtx_setParameter,
};
use crate::sequences::{
    ZSTD_CCtxParams_registerSequenceProducer, ZSTD_Sequence, ZSTD_compressSequences,
    ZSTD_compressSequencesAndLiterals, ZSTD_generateSequences, ZSTD_mergeBlockDelimiters,
    ZSTD_registerSequenceProducer, ZSTD_sequenceBound,
};
use crate::simple::{ZSTD_compressBound, ZSTD_decompress};
use crate::streaming::{ZSTD_compressStream2, ZSTD_inBuffer, ZSTD_outBuffer};

/// `ZSTD_sf_explicitBlockDelimiters`.
const EXPLICIT_DELIMITERS: c_int = 1;

/// Text-like input, changing every thousand bytes.
fn fox(len: usize) -> Vec<u8> {
    let phrase = b"the quick brown fox jumps over the lazy dog ";
    (0..len)
        .map(|i| phrase[i % phrase.len()] ^ (i / 1000) as u8)
        .collect()
}

fn seq(offset: u32, lit_len: u32, match_len: u32) -> ZSTD_Sequence {
    ZSTD_Sequence {
        offset,
        lit_len,
        match_len,
        rep: 0,
    }
}

fn decode(frame: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let rc = unsafe { ZSTD_decompress(out.as_mut_ptr(), out.len(), frame.as_ptr(), frame.len()) };
    assert_eq!(ZSTD_isError(rc), 0, "{:?}", code(rc));
    out.truncate(rc);
    out
}

fn generate(cctx: *mut ZSTD_CCtx, input: &[u8]) -> Vec<ZSTD_Sequence> {
    let mut parse = vec![ZSTD_Sequence::default(); ZSTD_sequenceBound(input.len())];
    let n = unsafe {
        ZSTD_generateSequences(
            cctx,
            parse.as_mut_ptr(),
            parse.len(),
            input.as_ptr(),
            input.len(),
        )
    };
    assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
    parse.truncate(n);
    parse
}

/// `ZSTD_compressSequences` through `cctx`: the frame, or the error code.
fn compress_sequences(
    cctx: *mut ZSTD_CCtx,
    parse: &[ZSTD_Sequence],
    input: &[u8],
) -> Result<Vec<u8>, ZSTD_ErrorCode> {
    let mut frame = vec![0u8; ZSTD_compressBound(input.len()) + 64];
    let rc = unsafe {
        ZSTD_compressSequences(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            parse.as_ptr(),
            parse.len(),
            input.as_ptr(),
            input.len(),
        )
    };
    if ZSTD_isError(rc) != 0 {
        return Err(code(rc));
    }
    frame.truncate(rc);
    Ok(frame)
}

/// `ZSTD_compress2` through `cctx`: the frame, or the error code.
fn compress2(cctx: *mut ZSTD_CCtx, input: &[u8]) -> Result<Vec<u8>, ZSTD_ErrorCode> {
    let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
    let rc = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
        )
    };
    if ZSTD_isError(rc) != 0 {
        return Err(code(rc));
    }
    frame.truncate(rc);
    Ok(frame)
}

/// The bound counts one sequence per minimal match and one delimiter per
/// smallest block (upstream: 2, 1004, 43820).
#[test]
fn sequence_bound_matches_upstream() {
    assert_eq!(ZSTD_sequenceBound(0), 2);
    assert_eq!(ZSTD_sequenceBound(3000), 1004);
    assert_eq!(ZSTD_sequenceBound(131072), 43820);
}

/// A generated parse covers the input, closes each block with a delimiter,
/// numbers repeat offsets, and builds a frame that decodes to the input.
#[test]
fn generated_parse_round_trips_through_compress_sequences() {
    let input = fox(20_000);
    let cctx = ZSTD_createCCtx();
    let parse = generate(cctx, &input);
    let covered: usize = parse
        .iter()
        .map(|s| (s.lit_len + s.match_len) as usize)
        .sum();
    assert_eq!(covered, input.len());
    assert!(parse.last().unwrap().is_delimiter());
    assert!(
        parse.iter().any(|s| s.rep != 0),
        "upstream numbers 5 repeats"
    );

    unsafe { ZSTD_CCtx_setParameter(cctx, ZSTD_C_BLOCK_DELIMITERS, EXPLICIT_DELIMITERS) };
    let frame = compress_sequences(cctx, &parse, &input).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    assert!(frame.len() < input.len() / 10);

    // The same parse without delimiters, in the default format.
    let mut merged = parse.clone();
    let kept = unsafe { ZSTD_mergeBlockDelimiters(merged.as_mut_ptr(), merged.len()) };
    merged.truncate(kept);
    assert!(merged.iter().all(|s| !s.is_delimiter()));
    unsafe { ZSTD_CCtx_setParameter(cctx, ZSTD_C_BLOCK_DELIMITERS, 0) };
    let frame = compress_sequences(cctx, &merged, &input).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// Generating the parse of an incompressible input leaves the context no
/// larger than before by the frame it never hands out: upstream compresses
/// into a buffer it frees before returning (zstd_compress.c:3537-3548).
#[test]
fn generate_sequences_keeps_no_frame_behind() {
    let mut state = 0x2468_ACE1u32;
    let input: Vec<u8> = (0..1 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let cctx = ZSTD_createCCtx();
    let parse = generate(cctx, &input);
    assert!(parse.last().unwrap().is_delimiter());
    let kept = unsafe { cctx.as_ref() }
        .expect("a context was created")
        .scratch
        .capacity();
    assert!(
        kept < input.len() / 2,
        "the context keeps {kept} bytes of a frame it never handed out"
    );
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// A one-shot frame handed out leaves the context no frame-sized buffer: it
/// keeps at most room for a block, as upstream writes straight into `dst`
/// and keeps nothing of the frame.
#[test]
fn a_one_shot_frame_is_not_kept_after_it_is_handed_out() {
    let mut state = 0x1357_2468u32;
    let input: Vec<u8> = (0..1 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let cctx = ZSTD_createCCtx();
    let frame = compress2(cctx, &input).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    let kept = unsafe { cctx.as_ref() }
        .expect("a context was created")
        .scratch
        .capacity();
    assert!(
        kept <= ZSTD_compressBound(codec::MAX_BLOCK_SIZE as usize),
        "the context keeps {kept} bytes of a {}-byte frame it handed out",
        frame.len()
    );
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// Too small an output array and a block target are refused as upstream
/// refuses them (`dstSize_tooSmall`, `parameter_unsupported`).
#[test]
fn generate_sequences_refuses_what_upstream_refuses() {
    let input = fox(20_000);
    let cctx = ZSTD_createCCtx();
    let mut parse = [ZSTD_Sequence::default(); 3];
    let rc =
        unsafe { ZSTD_generateSequences(cctx, parse.as_mut_ptr(), 3, input.as_ptr(), input.len()) };
    assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    unsafe { ZSTD_CCtx_setParameter(cctx, ZSTD_C_TARGET_CBLOCK_SIZE, 1340) };
    let rc =
        unsafe { ZSTD_generateSequences(cctx, parse.as_mut_ptr(), 3, input.as_ptr(), input.len()) };
    assert_eq!(code(rc), ZSTD_ErrorCode::ZSTD_error_parameter_unsupported);
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// A parse no decoder could follow is refused, never written: a match that
/// reaches before the input, a length that runs past it, a short match.
#[test]
fn invalid_parses_are_refused() {
    let input = fox(2_000);
    let cctx = ZSTD_createCCtx();
    let reaching = [seq(30, 3, 10)];
    let overlong = [seq(3, 3, 5_000)];
    let short = [seq(3, 3, 2), seq(3, 1, 10)];
    for parse in [&reaching[..], &overlong[..], &short[..]] {
        assert_eq!(
            compress_sequences(cctx, parse, &input),
            Err(ZSTD_ErrorCode::ZSTD_error_externalSequences_invalid)
        );
    }
    // Well-formed, but describing other bytes: the second half copies the
    // `a` before it, where the input holds `b`. Encoded as given, the frame
    // would decode to 128 `a`s.
    let mut halves = vec![b'a'; 64];
    halves.extend_from_slice(&[b'b'; 64]);
    assert_eq!(
        compress_sequences(cctx, &[seq(1, 64, 64)], &halves),
        Err(ZSTD_ErrorCode::ZSTD_error_externalSequences_invalid)
    );
    let same = vec![b'a'; 128];
    let frame = compress_sequences(cctx, &[seq(1, 64, 64)], &same).unwrap();
    assert_eq!(decode(&frame, same.len()), same);
    // Explicit delimiters must cover the input exactly.
    unsafe { ZSTD_CCtx_setParameter(cctx, ZSTD_C_BLOCK_DELIMITERS, EXPLICIT_DELIMITERS) };
    assert_eq!(
        compress_sequences(cctx, &[ZSTD_Sequence::delimiter(10)], &input),
        Err(ZSTD_ErrorCode::ZSTD_error_externalSequences_invalid)
    );
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// Matches may reach into an attached dictionary; the frame names it and
/// decodes with it.
#[test]
fn compress_sequences_reaches_into_the_dictionary() {
    let dict = b"a dictionary of shared words: alpha beta gamma delta epsilon".repeat(4);
    let input = b"alpha beta gamma delta epsilon, then something new".to_vec();
    let cctx = ZSTD_createCCtx();
    unsafe { ZSTD_CCtx_loadDictionary(cctx, dict.as_ptr(), dict.len()) };
    // "alpha beta gamma delta epsilon" sits 30 bytes before the end of the
    // dictionary, and is the input's first 30 bytes.
    let parse = [seq(30, 0, 30)];
    let frame = compress_sequences(cctx, &parse, &input).unwrap();
    let dctx = ZSTD_createDCtx();
    let mut out = vec![0u8; input.len()];
    let rc = unsafe {
        ZSTD_decompress_usingDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
            dict.as_ptr(),
            dict.len(),
        )
    };
    assert_eq!(rc, input.len(), "{:?}", code(rc));
    assert_eq!(out, input);
    unsafe {
        ZSTD_freeDCtx(dctx);
        ZSTD_freeCCtx(cctx);
    }
}

/// The literals and the parse rebuild the input, and the frame decodes to
/// it, with a checksum too (which upstream refuses here).
#[test]
fn compress_sequences_and_literals_rebuilds_the_input() {
    let input = fox(20_000);
    let cctx = ZSTD_createCCtx();
    let parse = generate(cctx, &input);
    let mut literals = Vec::new();
    let mut at = 0;
    for s in &parse {
        literals.extend_from_slice(&input[at..at + s.lit_len as usize]);
        at += (s.lit_len + s.match_len) as usize;
    }
    unsafe {
        ZSTD_CCtx_setParameter(cctx, ZSTD_C_BLOCK_DELIMITERS, EXPLICIT_DELIMITERS);
        ZSTD_CCtx_setParameter(cctx, ZSTD_C_CHECKSUM_FLAG, 1);
    }
    let run = |lit_capacity: usize, size: usize| {
        let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
        let rc = unsafe {
            ZSTD_compressSequencesAndLiterals(
                cctx,
                frame.as_mut_ptr(),
                frame.len(),
                parse.as_ptr(),
                parse.len(),
                literals.as_ptr(),
                literals.len(),
                lit_capacity,
                size,
            )
        };
        if ZSTD_isError(rc) != 0 {
            return Err(code(rc));
        }
        frame.truncate(rc);
        Ok(frame)
    };
    let frame = run(literals.len() + 8, input.len()).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    assert_eq!(
        run(literals.len() - 1, input.len()),
        Err(ZSTD_ErrorCode::ZSTD_error_workSpace_tooSmall)
    );
    assert_eq!(
        run(literals.len(), input.len() - 1),
        Err(ZSTD_ErrorCode::ZSTD_error_externalSequences_invalid)
    );
    unsafe { ZSTD_freeCCtx(cctx) };
}

unsafe extern "C" fn failing_producer(
    _state: *mut c_void,
    _out: *mut ZSTD_Sequence,
    _capacity: usize,
    _src: *const c_void,
    _src_size: usize,
    _dict: *const c_void,
    _dict_size: usize,
    _level: c_int,
    _window: usize,
) -> usize {
    usize::MAX
}

/// Counts its calls in the `usize` its state points at and parses each
/// block as literals only.
unsafe extern "C" fn literal_producer(
    state: *mut c_void,
    out: *mut ZSTD_Sequence,
    _capacity: usize,
    _src: *const c_void,
    src_size: usize,
    _dict: *const c_void,
    _dict_size: usize,
    _level: c_int,
    _window: usize,
) -> usize {
    unsafe {
        *state.cast::<usize>() += 1;
        out.write(ZSTD_Sequence::delimiter(src_size as u32));
    }
    1
}

/// Parses a block of one repeated byte as densely as a parse can be: one
/// literal, then three-byte matches at offset 1, close to the sequence bound.
unsafe extern "C" fn dense_producer(
    _state: *mut c_void,
    out: *mut ZSTD_Sequence,
    capacity: usize,
    _src: *const c_void,
    src_size: usize,
    _dict: *const c_void,
    _dict_size: usize,
    _level: c_int,
    _window: usize,
) -> usize {
    let mut parse = Vec::new();
    if src_size >= 4 {
        parse.push(seq(1, 1, 3));
        let mut at = 4;
        while src_size - at >= 3 {
            parse.push(seq(1, 0, 3));
            at += 3;
        }
        parse.push(ZSTD_Sequence::delimiter((src_size - at) as u32));
    } else {
        parse.push(ZSTD_Sequence::delimiter(src_size as u32));
    }
    if parse.len() > capacity {
        return usize::MAX;
    }
    unsafe { core::ptr::copy_nonoverlapping(parse.as_ptr(), out, parse.len()) };
    parse.len()
}

/// The context a parameter set compresses with, after one frame of `input`:
/// its footprint and the set's estimates, one-shot and streaming.
fn footprint_and_estimates(
    input: &[u8],
    producer: Option<crate::sequences::ZSTD_sequenceProducer_F>,
) -> (usize, usize, usize) {
    let params = ZSTD_createCCtxParams();
    let cctx = ZSTD_createCCtx();
    assert!(!params.is_null() && !cctx.is_null());
    unsafe {
        // The estimates and the context size the same frame.
        let hint = c_int::try_from(input.len()).unwrap();
        assert_eq!(
            ZSTD_CCtxParams_setParameter(params, crate::params::ZSTD_C_SRC_SIZE_HINT, hint),
            0
        );
        ZSTD_CCtxParams_registerSequenceProducer(params, core::ptr::null_mut(), producer);
        assert_eq!(ZSTD_CCtx_setParametersUsingCCtxParams(cctx, params), 0);
    }
    let frame = compress2(cctx, input).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    let held = unsafe { crate::context::ZSTD_sizeof_CCtx(cctx) };
    let estimate = unsafe { crate::cctx_params::ZSTD_estimateCCtxSize_usingCCtxParams(params) };
    let stream = unsafe { crate::cctx_params::ZSTD_estimateCStreamSize_usingCCtxParams(params) };
    assert_eq!(ZSTD_isError(estimate), 0, "{:?}", code(estimate));
    assert_eq!(ZSTD_isError(stream), 0, "{:?}", code(stream));
    unsafe {
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCCtxParams(params);
    }
    (held, estimate, stream)
}

/// Registering a producer on a parameter set raises both of its estimates by
/// at least what the context keeps for the producer's parse, as upstream
/// budgets `ZSTD_sequenceBound(blockSize)` sequences for it
/// (zstd_compress.c:1734-1736).
#[test]
fn a_producer_raises_the_estimates_by_its_parse() {
    let input = vec![b'a'; 1 << 18];
    let (held_plain, estimate_plain, stream_plain) = footprint_and_estimates(&input, None);
    let (held, estimate, stream) = footprint_and_estimates(&input, Some(dense_producer));
    let parse = held - held_plain;
    assert!(parse > 0, "the producer's parse is kept");
    assert!(
        estimate - estimate_plain >= parse && stream - stream_plain >= parse,
        "a {parse}-byte parse raised the estimates by {} and {}",
        estimate - estimate_plain,
        stream - stream_plain
    );
}

/// The parse a producer fills is kept between blocks and budgeted at the
/// sequence bound of the largest block, so growing it for a larger block must
/// take that bound and no more: an amortised reserve would double a buffer
/// sized for a slightly smaller block past it.
#[test]
fn a_producer_parse_grows_to_the_bound_of_its_block() {
    use codec::encoding::sequences::sequence_bound;
    let params = ZSTD_createCCtxParams();
    unsafe {
        ZSTD_CCtxParams_registerSequenceProducer(
            params,
            core::ptr::null_mut(),
            Some(dense_producer),
        )
    };
    let (producer, _) = unsafe { params.as_ref() }
        .expect("a parameter set")
        .params
        .sequence_producer(3);
    let mut producer = producer.expect("a registered producer");
    let mut parse = Vec::new();
    let smaller = vec![b'a'; 96 << 10];
    let larger = vec![b'a'; 128 << 10];
    producer.produce(&smaller, 1 << 20, &mut parse).unwrap();
    producer.produce(&larger, 1 << 20, &mut parse).unwrap();
    assert!(
        parse.capacity() <= sequence_bound(larger.len()),
        "{} sequences kept for a bound of {}",
        parse.capacity(),
        sequence_bound(larger.len())
    );
    unsafe { ZSTD_freeCCtxParams(params) };
}

/// Records the compression level it is asked at in the `c_int` its state
/// points at, and parses each block as literals.
unsafe extern "C" fn level_recorder(
    state: *mut c_void,
    out: *mut ZSTD_Sequence,
    _capacity: usize,
    _src: *const c_void,
    src_size: usize,
    _dict: *const c_void,
    _dict_size: usize,
    level: c_int,
    _window: usize,
) -> usize {
    unsafe {
        *state.cast::<c_int>() = level;
        out.write(ZSTD_Sequence::delimiter(src_size as u32));
    }
    1
}

/// A producer is told the level the frame runs at: a referenced CDict's on
/// a small source, not the context's own.
#[test]
fn producer_hears_the_frames_level() {
    let input = fox(2_000);
    let dict = b"some dictionary content for the cdict".repeat(8);
    let cctx = ZSTD_createCCtx();
    let cdict = unsafe { crate::cdict::ZSTD_createCDict(dict.as_ptr(), dict.len(), 10) };
    let mut level: c_int = -1;
    unsafe {
        crate::attach::ZSTD_CCtx_refCDict(cctx, cdict);
        ZSTD_registerSequenceProducer(cctx, (&raw mut level).cast(), Some(level_recorder));
    }
    compress2(cctx, &input).unwrap();
    assert_eq!(level, 10);
    unsafe {
        ZSTD_freeCCtx(cctx);
        crate::cdict::ZSTD_freeCDict(cdict);
    }
}

/// A producer that fails fails the frame (upstream `sequenceProducer_failed`)
/// unless fallback is on; a working one is asked once per block; `NULL`
/// removes it. Unlike upstream, long-distance matching may stay on beside it.
#[test]
fn registered_producer_drives_compress2() {
    let input = fox(20_000);
    let cctx = ZSTD_createCCtx();
    unsafe { ZSTD_registerSequenceProducer(cctx, core::ptr::null_mut(), Some(failing_producer)) };
    assert_eq!(
        compress2(cctx, &input),
        Err(ZSTD_ErrorCode::ZSTD_error_sequenceProducer_failed)
    );
    unsafe { ZSTD_CCtx_setParameter(cctx, ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK, 1) };
    let frame = compress2(cctx, &input).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    assert!(
        frame.len() < input.len() / 10,
        "the built-in matchers took over"
    );

    let mut calls = 0usize;
    let state = (&raw mut calls).cast::<c_void>();
    unsafe { ZSTD_registerSequenceProducer(cctx, state, Some(literal_producer)) };
    unsafe { ZSTD_CCtx_setParameter(cctx, ZSTD_C_ENABLE_LDM, 1) };
    let frame = compress2(cctx, &input).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    assert!(calls >= 1);
    assert!(
        frame.len() > input.len() / 2,
        "literals only: upstream 20010"
    );

    unsafe { ZSTD_registerSequenceProducer(cctx, core::ptr::null_mut(), None) };
    let frame = compress2(cctx, &input).unwrap();
    assert!(frame.len() < input.len() / 10);
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// A producer registered on a parameter set reaches the context with it, and
/// a streamed frame it fails on reports the failure, after which the next
/// frame starts clean.
#[test]
fn registered_producer_drives_streams() {
    let input = fox(20_000);
    let cctx = ZSTD_createCCtx();
    let params = ZSTD_createCCtxParams();
    unsafe {
        ZSTD_CCtxParams_registerSequenceProducer(
            params,
            core::ptr::null_mut(),
            Some(failing_producer),
        );
        ZSTD_CCtx_setParametersUsingCCtxParams(cctx, params);
    }
    let stream = |cctx: *mut ZSTD_CCtx| {
        let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
        let mut out = ZSTD_outBuffer {
            dst: frame.as_mut_ptr().cast(),
            size: frame.len(),
            pos: 0,
        };
        let mut inp = ZSTD_inBuffer {
            src: input.as_ptr().cast(),
            size: input.len(),
            pos: 0,
        };
        let rc = unsafe { ZSTD_compressStream2(cctx, &mut out, &mut inp, 2) };
        if ZSTD_isError(rc) != 0 {
            return Err(code(rc));
        }
        assert_eq!(rc, 0);
        frame.truncate(out.pos);
        Ok(frame)
    };
    assert_eq!(
        stream(cctx),
        Err(ZSTD_ErrorCode::ZSTD_error_sequenceProducer_failed)
    );
    unsafe {
        ZSTD_CCtxParams_setParameter(params, ZSTD_C_ENABLE_SEQ_PRODUCER_FALLBACK, 1);
        assert_eq!(ZSTD_CCtx_setParametersUsingCCtxParams(cctx, params), 0);
    }
    let frame = stream(cctx).unwrap();
    assert_eq!(decode(&frame, input.len()), input);
    unsafe {
        ZSTD_freeCCtxParams(params);
        ZSTD_freeCCtx(cctx);
    }
}
