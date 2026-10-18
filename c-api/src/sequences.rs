//! The sequence entry points: a parse of the input handed in or out as an
//! array of `ZSTD_Sequence` (`ZSTD_generateSequences`,
//! `ZSTD_compressSequences`, `ZSTD_compressSequencesAndLiterals`), and a
//! caller's parser asked for each block (`ZSTD_registerSequenceProducer`).

use core::ffi::{c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::encoding::sequences::{
    ExternalSequence, RebuiltSource, SequenceProducer, SequenceProducerError, SequenceReplay,
    merge_block_delimiters, sequence_bound, validate_parse,
};
use codec::encoding::{CompressionLevel, CompressionParameters, FrameCompressor};

use crate::cctx_params::{CCtxParams, ZSTD_CCtx_params};
use crate::context::{FrameProgress, ZSTD_CCtx, compress_sticky_frame};
use crate::error::{ZSTD_ErrorCode, encode};
use crate::ffi::{in_slice, out_slice};
use crate::params::ZSTD_C_BLOCK_DELIMITERS;

/// `ZSTD_Sequence`: one sequence of a parse, field for field.
#[allow(non_camel_case_types)]
pub type ZSTD_Sequence = ExternalSequence;

/// `ZSTD_sequenceProducer_F`: a caller's parser. Fills up to
/// `outSeqsCapacity` sequences for `src` and returns how many, or
/// `ZSTD_SEQUENCE_PRODUCER_ERROR` (or any count past the capacity) when it has
/// no parse.
#[allow(non_camel_case_types)]
pub type ZSTD_sequenceProducer_F = unsafe extern "C" fn(
    sequence_producer_state: *mut c_void,
    out_seqs: *mut ZSTD_Sequence,
    out_seqs_capacity: usize,
    src: *const c_void,
    src_size: usize,
    dict: *const c_void,
    dict_size: usize,
    compression_level: c_int,
    window_size: usize,
) -> usize;

/// A registered producer: the caller's function and the state it is handed.
#[derive(Copy, Clone, Debug)]
pub(crate) struct CSequenceProducer {
    state: *mut c_void,
    func: ZSTD_sequenceProducer_F,
}

/// A registered producer as the encoder asks it, for frames at `level`.
pub(crate) struct CProducerCall {
    pub(crate) producer: CSequenceProducer,
    pub(crate) level: c_int,
}

impl SequenceProducer for CProducerCall {
    fn produce(
        &mut self,
        block: &[u8],
        window_size: u64,
        out: &mut Vec<ExternalSequence>,
    ) -> Result<(), SequenceProducerError> {
        let capacity = sequence_bound(block.len());
        out.clear();
        out.reserve(capacity);
        // SAFETY: the C contract of `ZSTD_registerSequenceProducer`: the
        // function writes at most `capacity` sequences, which `out` has room
        // for, and `block` is readable for its length. No dictionary is
        // handed over, as upstream hands none.
        let produced = unsafe {
            (self.producer.func)(
                self.producer.state,
                out.as_mut_ptr(),
                capacity,
                block.as_ptr().cast(),
                block.len(),
                core::ptr::null(),
                0,
                self.level,
                usize::try_from(window_size).unwrap_or(usize::MAX),
            )
        };
        // `ZSTD_SEQUENCE_PRODUCER_ERROR` is `(size_t)-1`, past any capacity.
        if produced > capacity || (produced == 0 && !block.is_empty()) {
            return Err(SequenceProducerError);
        }
        // SAFETY: the function initialised the first `produced` sequences,
        // and every bit pattern is a valid `ExternalSequence`.
        unsafe { out.set_len(produced) };
        Ok(())
    }
}

fn register(params: &mut CCtxParams, state: *mut c_void, func: Option<ZSTD_sequenceProducer_F>) {
    params.producer = func.map(|func| CSequenceProducer { state, func });
}

/// `void ZSTD_registerSequenceProducer(ZSTD_CCtx* cctx, void*
/// sequenceProducerState, ZSTD_sequenceProducer_F sequenceProducer)` — ask
/// `sequenceProducer` for each block's parse from the next frame on, until
/// replaced, cleared with `NULL` or reset with the parameters. A block it
/// fails on fails the frame with `sequenceProducer_failed`, unless
/// `ZSTD_c_enableSeqProducerFallback` lets the built-in matchers take it.
///
/// # Safety
/// `cctx` must be live or `NULL`; `sequence_producer` must honour the
/// `ZSTD_sequenceProducer_F` contract with `sequence_producer_state` for as
/// long as it is registered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_registerSequenceProducer(
    cctx: *mut ZSTD_CCtx,
    sequence_producer_state: *mut c_void,
    sequence_producer: Option<ZSTD_sequenceProducer_F>,
) {
    if let Some(cctx) = unsafe { cctx.as_mut() } {
        register(&mut cctx.params, sequence_producer_state, sequence_producer);
    }
}

/// `void ZSTD_CCtxParams_registerSequenceProducer(ZSTD_CCtx_params* params,
/// void* sequenceProducerState, ZSTD_sequenceProducer_F sequenceProducer)` —
/// [`ZSTD_registerSequenceProducer`] on a free-standing parameter set.
///
/// # Safety
/// As [`ZSTD_registerSequenceProducer`], with `params` live or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_CCtxParams_registerSequenceProducer(
    params: *mut ZSTD_CCtx_params,
    sequence_producer_state: *mut c_void,
    sequence_producer: Option<ZSTD_sequenceProducer_F>,
) {
    if let Some(params) = unsafe { params.as_mut() } {
        register(
            &mut params.params,
            sequence_producer_state,
            sequence_producer,
        );
    }
}

/// `size_t ZSTD_sequenceBound(size_t srcSize)` — the most sequences, block
/// delimiters included, a parse of `srcSize` bytes can take.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_sequenceBound(src_size: usize) -> usize {
    sequence_bound(src_size)
}

/// `size_t ZSTD_mergeBlockDelimiters(ZSTD_Sequence* sequences, size_t
/// seqsSize)` — drop the block delimiters, handing their literals on, and
/// return how many sequences are left at the front.
///
/// # Safety
/// `sequences` must be valid for `seqs_size` elements (or `NULL` + 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_mergeBlockDelimiters(
    sequences: *mut ZSTD_Sequence,
    seqs_size: usize,
) -> usize {
    merge_block_delimiters(unsafe { out_slice(sequences, seqs_size) })
}

/// `size_t ZSTD_generateSequences(ZSTD_CCtx* zc, ZSTD_Sequence* outSeqs,
/// size_t outSeqsSize, const void* src, size_t srcSize)` — the parse
/// `ZSTD_compress2` takes for `src`, as sequences with plain offsets, each
/// block closed by a delimiter carrying its trailing literals. Returns how
/// many it wrote, `dstSize_tooSmall` when `outSeqsSize` cannot hold them
/// ([`ZSTD_sequenceBound`] always can).
///
/// # Safety
/// `zc` must be live or `NULL`; `out_seqs` valid for `out_seqs_size`
/// elements; `src` for `src_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_generateSequences(
    zc: *mut ZSTD_CCtx,
    out_seqs: *mut ZSTD_Sequence,
    out_seqs_size: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(cctx) = (unsafe { zc.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    // Upstream refuses it: the parse is collected per block, and a block
    // target splits blocks after the parse (zstd_compress.c:3529).
    if cctx.params.target_cblock_size != 0 {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_unsupported);
    }
    let src = unsafe { in_slice(src, src_size) };
    let mut parse = Vec::new();
    let frame_len = match compress_sticky_frame(cctx, src, Some(&mut parse)) {
        Ok(len) => len,
        Err(code) => return encode(code),
    };
    cctx.consume_prefix();
    cctx.progress = FrameProgress::one_shot(src.len(), frame_len);
    if parse.len() > out_seqs_size {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let out = unsafe { out_slice(out_seqs, out_seqs_size) };
    out[..parse.len()].copy_from_slice(&parse);
    parse.len()
}

/// The window a frame over `src_len` bytes takes under `frame_params`, or
/// `level`'s own when there are none.
fn frame_window(
    frame_params: Option<&CompressionParameters>,
    level: c_int,
    src_len: usize,
    dict_len: usize,
) -> u64 {
    let level_params;
    let params = match frame_params {
        Some(params) => params,
        None => {
            level_params = CompressionParameters::builder(CompressionLevel::from_level(level))
                .build()
                .expect("a level alone sets no knob out of bounds");
            &level_params
        }
    };
    1u64 << params.window_log_for(Some(src_len as u64), dict_len)
}

/// What a parse is laid over: the input itself, or the literals the parse
/// rebuilds the input from.
#[derive(Clone, Copy)]
enum ParseInput<'a> {
    Bytes(&'a [u8]),
    Literals(&'a [u8]),
}

/// Compress the `src_len` bytes of `input` into `cctx.scratch` as one frame
/// built from `parse` under the sticky parameters and the attached dictionary.
fn compress_parse(
    cctx: &mut ZSTD_CCtx,
    parse: &[ExternalSequence],
    src_len: usize,
    input: ParseInput<'_>,
) -> Result<(), ZSTD_ErrorCode> {
    cctx.begin_one_shot(src_len)?;
    let src_size = Some(src_len as u64);
    let frame_params = cctx.frame_parameters(src_size)?;
    let params = cctx.params;
    let level = cctx.attach_level(src_size);
    let suppress_id = cctx.attach_suppresses_dict_id();
    let ZSTD_CCtx {
        scratch,
        attached_dict,
        ..
    } = cctx;
    // A dictionary the compressor would attach as none gives the parse no
    // history to reach into either.
    let dictionary = attached_dict
        .prepared()
        .filter(|dictionary| dictionary.is_used_for_compression());
    let dict_len = dictionary.map_or(0, codec::encoding::EncoderDictionary::content_size);
    let window = frame_window(frame_params.as_ref(), level, src_len, dict_len);
    // A block is held to the window as well as to its own limit.
    let max_block = params
        .max_block_size()
        .map_or(codec::MAX_BLOCK_SIZE as usize, |size| size as usize)
        .min(usize::try_from(window).unwrap_or(usize::MAX));
    let explicit_delimiters = params.get(ZSTD_C_BLOCK_DELIMITERS) == Ok(1);
    // Always checked, whatever `ZSTD_c_validateSequences` says: a parse that
    // breaks the format would otherwise make a frame no decoder can read.
    validate_parse(
        parse,
        src_len,
        window,
        dict_len,
        explicit_delimiters,
        max_block,
    )
    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_externalSequences_invalid)?;
    let frame = ParseFrame {
        parse,
        window,
        dictionary,
        level,
        params: &params,
        suppress_id,
        src_len,
    };
    match input {
        ParseInput::Bytes(src) => frame.build(src, scratch),
        ParseInput::Literals(literals) => {
            let dict = dictionary.map_or(&[][..], codec::encoding::EncoderDictionary::content);
            let window = usize::try_from(window).unwrap_or(usize::MAX);
            let source = RebuiltSource::new(parse, literals, dict, window)
                .ok()
                .filter(|source| source.len() == src_len)
                .ok_or(ZSTD_ErrorCode::ZSTD_error_externalSequences_invalid)?;
            frame.build(source, scratch)
        }
    }
}

/// One frame built from a parse, settled by `compress_parse`.
struct ParseFrame<'a> {
    parse: &'a [ExternalSequence],
    window: u64,
    dictionary: Option<&'a codec::encoding::EncoderDictionary>,
    level: c_int,
    params: &'a CCtxParams,
    suppress_id: bool,
    src_len: usize,
}

impl ParseFrame<'_> {
    /// Compress what `src` reads into `scratch`.
    fn build<R: codec::io::Read>(
        &self,
        src: R,
        scratch: &mut Vec<u8>,
    ) -> Result<(), ZSTD_ErrorCode> {
        let params = self.params;
        let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
            let mut replay = SequenceReplay::new(self.parse, self.window);
            if self.dictionary.is_some() {
                replay = replay.with_dictionary();
            }
            // The prepared dictionary is borrowed for the one frame, as
            // upstream references the CDict, with no reference count taken.
            let mut compressor: FrameCompressor<R, &mut Vec<u8>, SequenceReplay<'_>, _> =
                FrameCompressor::new_with_matcher_and_dictionary(
                    replay,
                    CompressionLevel::from_level(self.level),
                    self.dictionary,
                )
                .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
            compressor.set_content_checksum(params.checksum_flag);
            compressor.set_content_size_flag(params.content_size_flag);
            compressor.set_dictionary_id_flag(params.dict_id_flag && !self.suppress_id);
            compressor.set_target_block_size(params.target_block_size());
            compressor.set_max_block_size(params.max_block_size());
            compressor.set_magicless(params.magicless);
            compressor.set_source_size_hint(self.src_len as u64);
            scratch.clear();
            compressor.set_source(src);
            compressor.set_drain(scratch);
            compressor.compress();
            Ok(())
        }));
        outcome.unwrap_or(Err(ZSTD_ErrorCode::ZSTD_error_GENERIC))
    }
}

/// Hand the frame `compress_parse` left in `cctx.scratch` to `dst`.
fn deliver(cctx: &mut ZSTD_CCtx, dst: &mut [u8], src_len: usize) -> usize {
    let len = match cctx.hand_out_frame(dst) {
        Ok(len) => len,
        Err(code) => return encode(code),
    };
    cctx.consume_prefix();
    cctx.progress = FrameProgress::one_shot(src_len, len);
    len
}

/// `size_t ZSTD_compressSequences(ZSTD_CCtx* cctx, void* dst, size_t
/// dstCapacity, const ZSTD_Sequence* inSeqs, size_t inSeqsSize, const void*
/// src, size_t srcSize)` — one frame of `src` built from the parse `inSeqs`
/// under the sticky parameters, in the format `ZSTD_c_blockDelimiters`
/// names. The parse is always validated (`externalSequences_invalid`), so
/// no parse yields a frame that does not decode to `src`.
///
/// # Safety
/// `cctx` must be live or `NULL`; `dst`, `in_seqs` and `src` valid for their
/// lengths (or `NULL` + 0 each).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressSequences(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    in_seqs: *const ZSTD_Sequence,
    in_seqs_size: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let parse = unsafe { in_slice(in_seqs, in_seqs_size) };
    let src = unsafe { in_slice(src, src_size) };
    if let Err(code) = compress_parse(cctx, parse, src.len(), ParseInput::Bytes(src)) {
        return encode(code);
    }
    deliver(cctx, unsafe { out_slice(dst, dst_capacity) }, src.len())
}

/// `size_t ZSTD_compressSequencesAndLiterals(ZSTD_CCtx* cctx, void* dst,
/// size_t dstCapacity, const ZSTD_Sequence* inSeqs, size_t nbSequences, const
/// void* literals, size_t litSize, size_t litBufCapacity, size_t
/// decompressedSize)` — [`ZSTD_compressSequences`] given the literals in
/// place of the input, which the parse rebuilds to `decompressedSize` bytes
/// as the frame is built, holding one window of it at a time. Unlike
/// upstream, which writes the blocks without the input, the frame can carry a
/// checksum, the parse may come without block delimiters, and a block may be
/// stored uncompressed.
///
/// # Safety
/// `cctx` must be live or `NULL`; `dst` and `in_seqs` valid for their
/// lengths; `literals` for `lit_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressSequencesAndLiterals(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    in_seqs: *const ZSTD_Sequence,
    nb_sequences: usize,
    literals: *const u8,
    lit_size: usize,
    lit_buf_capacity: usize,
    decompressed_size: usize,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    // The capacity is part of the C contract (upstream reads past `litSize`).
    if lit_buf_capacity < lit_size {
        return encode(ZSTD_ErrorCode::ZSTD_error_workSpace_tooSmall);
    }
    let parse = unsafe { in_slice(in_seqs, nb_sequences) };
    let literals = unsafe { in_slice(literals, lit_size) };
    let input = ParseInput::Literals(literals);
    if let Err(code) = compress_parse(cctx, parse, decompressed_size, input) {
        return encode(code);
    }
    deliver(
        cctx,
        unsafe { out_slice(dst, dst_capacity) },
        decompressed_size,
    )
}
