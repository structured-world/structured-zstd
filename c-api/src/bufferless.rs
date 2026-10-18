//! The buffer-less and block-level API of `zstd.h`: a frame compressed or
//! decompressed in steps the caller sizes, with no streaming buffers between
//! the calls, and raw blocks with no frame around them.
//!
//! Upstream keeps the history of these calls in the caller's buffers and asks
//! them to stay in place; here the codec keeps it, so the caller's buffers may
//! be reused freely, which every upstream use of the API also satisfies.

use core::ffi::{c_int, c_uint};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::DictionaryHandle;
use codec::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use codec::encoding::{CompressionParameters, EncoderDictionary};

use crate::cctx_params::CCtxParams;
use crate::cdict::{ZSTD_CDict, ZSTD_DDict, ZSTD_frameParameters, next_dict_serial};
use crate::context::{FrameProgress, ZSTD_CCtx, ZSTD_DCtx};
use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::ffi::{in_slice, out_slice};
use crate::params::{CONTENTSIZE_UNKNOWN, ZSTD_parameters};
use crate::streaming::{CStreamState, FrameSetup};

/// How a buffer-less frame was begun, kept so `ZSTD_copyCCtx` can begin
/// another one the same way.
#[derive(Clone)]
pub(crate) struct BufferlessBegin {
    params: CCtxParams,
    frame_params: Option<CompressionParameters>,
    level: c_int,
    dictionary: Option<EncoderDictionary>,
    serial: u64,
    suppress_id: bool,
}

/// A buffer-less frame in progress on a context.
pub(crate) struct BufferlessFrame {
    begin: BufferlessBegin,
    /// Nothing is compressed into the frame yet: its header still waits in
    /// the stream's pending output, for the first `ZSTD_compressContinue`
    /// to hand out or the first `ZSTD_compressBlock` to drop.
    fresh: bool,
}

impl BufferlessFrame {
    /// The dictionary the frame was begun with, kept for `ZSTD_copyCCtx`.
    pub(crate) fn dictionary(&self) -> Option<&EncoderDictionary> {
        self.begin.dictionary.as_ref()
    }
}

impl BufferlessBegin {
    fn at_level(level: c_int, dict: &[u8]) -> Result<Self, ZSTD_ErrorCode> {
        let (dictionary, serial, suppress_id) = if dict.is_empty() {
            (None, 0, false)
        } else {
            let raw_content =
                crate::attach::encode_raw_content(dict, crate::attach::ZSTD_DCT_AUTO)?;
            let prepared = crate::cdict::prepare_compression_dictionary(dict, raw_content)
                .ok_or(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
            (Some(prepared), next_dict_serial(), raw_content)
        };
        Ok(Self {
            params: CCtxParams::with_level(level),
            frame_params: None,
            level,
            dictionary,
            serial,
            suppress_id,
        })
    }

    fn from_cdict(cdict: &ZSTD_CDict, fparams: &ZSTD_frameParameters, pledged: u64) -> Self {
        let mut params = CCtxParams::with_level(cdict.level);
        params.content_size_flag = fparams.contentSizeFlag != 0;
        params.checksum_flag = fparams.checksumFlag != 0;
        params.dict_id_flag = fparams.noDictIDFlag == 0;
        params.pledged_src_size = pledged;
        let src_size = (pledged != CONTENTSIZE_UNKNOWN).then_some(pledged);
        let frame_params = if crate::attach::cdict_params_describe_frame(cdict, src_size) {
            crate::cdict::cdict_frame_parameters(cdict, src_size)
        } else {
            None
        };
        Self {
            params,
            frame_params,
            level: cdict.level,
            dictionary: cdict.dict.clone(),
            serial: cdict.serial,
            suppress_id: cdict.raw_content,
        }
    }
}

/// Begin a buffer-less frame on `cctx` the way `begin` says; any frame in
/// flight on it is abandoned.
fn begin_frame(cctx: &mut ZSTD_CCtx, begin: BufferlessBegin) -> usize {
    cctx.reset_session();
    let ZSTD_CCtx { stream, .. } = cctx;
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
        let setup = FrameSetup {
            params: &begin.params,
            frame_params: begin.frame_params.as_ref(),
            level: begin.level,
            dictionary: begin.dictionary.as_ref(),
            serial: begin.serial,
            suppress_id: begin.suppress_id,
        };
        let state = CStreamState::open_frame(stream, &setup)?;
        state.pending.clear();
        state.pending_pos = 0;
        state
            .context
            .start_frame(&mut state.pending)
            .map_err(|_| ZSTD_ErrorCode::ZSTD_error_GENERIC)
    }));
    match outcome {
        Ok(Ok(())) => {
            cctx.bufferless = Some(BufferlessFrame { begin, fresh: true });
            cctx.progress = FrameProgress::default();
            0
        }
        Ok(Err(code)) => {
            cctx.stream = None;
            encode(code)
        }
        Err(_) => {
            cctx.stream = None;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}

/// `size_t ZSTD_compressBegin(ZSTD_CCtx* cctx, int compressionLevel)` — begin
/// a buffer-less frame at `compressionLevel`, of unknown size.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressBegin(
    cctx: *mut ZSTD_CCtx,
    compression_level: c_int,
) -> usize {
    unsafe { ZSTD_compressBegin_usingDict(cctx, core::ptr::null(), 0, compression_level) }
}

/// `size_t ZSTD_compressBegin_usingDict(ZSTD_CCtx* cctx, const void* dict,
/// size_t dictSize, int compressionLevel)` — [`ZSTD_compressBegin`] with a
/// dictionary (none when empty).
///
/// # Safety
/// `cctx` live or `NULL`; `dict` valid for `dict_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressBegin_usingDict(
    cctx: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
    compression_level: c_int,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let dict = unsafe { in_slice(dict, dict_size) };
    match BufferlessBegin::at_level(compression_level, dict) {
        Ok(begin) => begin_frame(cctx, begin),
        Err(code) => encode(code),
    }
}

/// `size_t ZSTD_compressBegin_advanced(ZSTD_CCtx* cctx, const void* dict,
/// size_t dictSize, ZSTD_parameters params, unsigned long long
/// pledgedSrcSize)` — begin a buffer-less frame under explicit parameters,
/// with a dictionary (none when empty) and the frame's size pledged.
///
/// # Safety
/// `cctx` live or `NULL`; `dict` valid for `dict_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressBegin_advanced(
    cctx: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
    params: ZSTD_parameters,
    pledged_src_size: u64,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let checked = crate::params::ZSTD_checkCParams(params.cParams);
    if crate::error::result_is_error(checked) {
        return checked;
    }
    let dict = unsafe { in_slice(dict, dict_size) };
    let mut begin = match BufferlessBegin::at_level(0, dict) {
        Ok(begin) => begin,
        Err(code) => return encode(code),
    };
    begin.params.set_explicit(&params.cParams, &params.fParams);
    begin.params.pledged_src_size = pledged_src_size;
    begin.frame_params = begin.params.resolve();
    if begin.frame_params.is_none() {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_combination_unsupported);
    }
    begin_frame(cctx, begin)
}

/// `size_t ZSTD_compressBegin_usingCDict(ZSTD_CCtx* cctx, const ZSTD_CDict*
/// cdict)` — begin a buffer-less frame of unknown size with `cdict`, whose
/// parameters it runs under; the content size is left out of the header.
///
/// # Safety
/// `cctx` live or `NULL`; `cdict` live (it must outlive the frame) or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressBegin_usingCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
) -> usize {
    let fparams = ZSTD_frameParameters {
        contentSizeFlag: 0,
        checksumFlag: 0,
        noDictIDFlag: 0,
    };
    unsafe { ZSTD_compressBegin_usingCDict_advanced(cctx, cdict, fparams, CONTENTSIZE_UNKNOWN) }
}

/// `size_t ZSTD_compressBegin_usingCDict_advanced(ZSTD_CCtx* cctx, const
/// ZSTD_CDict* cdict, ZSTD_frameParameters fParams, unsigned long long
/// pledgedSrcSize)` — [`ZSTD_compressBegin_usingCDict`] with frame
/// parameters and the frame's size pledged.
///
/// # Safety
/// As [`ZSTD_compressBegin_usingCDict`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressBegin_usingCDict_advanced(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
    fparams: ZSTD_frameParameters,
    pledged_src_size: u64,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let Some(cdict) = (unsafe { cdict.as_ref() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_dictionary_wrong);
    };
    begin_frame(
        cctx,
        BufferlessBegin::from_cdict(cdict, &fparams, pledged_src_size),
    )
}

/// Compress `src` into the open buffer-less frame of `cctx` and copy what it
/// produced to `dst`, closing the frame when `end`. A failed call drops the
/// frame, as upstream leaves it unusable.
fn compress_step(cctx: &mut ZSTD_CCtx, dst: &mut [u8], src: &[u8], end: bool) -> usize {
    let attached = cctx.attach_serial();
    let ZSTD_CCtx {
        bufferless,
        stream,
        progress,
        ..
    } = cctx;
    let (Some(frame), Some(state)) = (bufferless.as_mut(), stream.as_mut().filter(|s| s.open))
    else {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
        let refused = |e: codec::io::Error| {
            // A pledge broken by the input reads as invalid input.
            if e.kind() == std::io::ErrorKind::InvalidInput {
                ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
            } else {
                ZSTD_ErrorCode::ZSTD_error_GENERIC
            }
        };
        let mut rest = src;
        while !rest.is_empty() {
            match state.context.write(&mut state.pending, rest) {
                Ok(0) => return Err(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong),
                Ok(n) => rest = &rest[n..],
                Err(e) => return Err(refused(e)),
            }
        }
        if end {
            state
                .context
                .finish_frame(&mut state.pending)
                .map_err(refused)
        } else {
            state.context.flush(&mut state.pending).map_err(refused)
        }
    }));
    let failed = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(code)) => Some(code),
        Err(_) => Some(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    };
    let produced = state.pending.len() - state.pending_pos;
    let failed = failed
        .or_else(|| (produced > dst.len()).then_some(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall));
    if let Some(code) = failed {
        *stream = None;
        *bufferless = None;
        return encode(code);
    }
    dst[..produced].copy_from_slice(&state.pending[state.pending_pos..]);
    state.pending.clear();
    state.pending_pos = 0;
    progress.ingested += src.len() as u64;
    progress.produced += produced as u64;
    progress.flushed += produced as u64;
    frame.fresh = false;
    if end {
        state.open = false;
        state.release_frame_output();
        *bufferless = None;
        let held = state.dictionary_serial();
        if held != attached {
            state.release_dictionary(held);
        }
    }
    produced
}

/// `size_t ZSTD_compressContinue(ZSTD_CCtx* cctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize)` — compress `src` into the
/// buffer-less frame, the frame header first on the first call; returns the
/// bytes written.
///
/// # Safety
/// `cctx` live or `NULL`; `dst` / `src` valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressContinue(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    compress_step(cctx, dst, src, false)
}

/// `size_t ZSTD_compressEnd(ZSTD_CCtx* cctx, void* dst, size_t dstCapacity,
/// const void* src, size_t srcSize)` — [`ZSTD_compressContinue`] that also
/// closes the frame: last block and checksum.
///
/// # Safety
/// As [`ZSTD_compressContinue`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressEnd(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    compress_step(cctx, dst, src, true)
}

/// `size_t ZSTD_compressBlock(ZSTD_CCtx* cctx, void* dst, size_t dstCapacity,
/// const void* src, size_t srcSize)` — compress `src`, at most
/// [`ZSTD_getBlockSize`] bytes, as one raw block of no frame: its content
/// without a block header, or 0 when it does not compress, in which case the
/// caller stores it as is and the decoder takes it with `ZSTD_insertBlock`.
///
/// # Safety
/// As [`ZSTD_compressContinue`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressBlock(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(cctx) = (unsafe { cctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let src = unsafe { in_slice(src, src_size) };
    let ZSTD_CCtx {
        bufferless,
        stream,
        progress,
        ..
    } = cctx;
    let (Some(frame), Some(state)) = (bufferless.as_mut(), stream.as_mut().filter(|s| s.open))
    else {
        return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
    };
    // Blocks go out without a frame around them.
    if frame.fresh {
        state.pending.clear();
        state.pending_pos = 0;
        frame.fresh = false;
    }
    if state
        .context
        .frame_block_size()
        .is_none_or(|limit| src.len() > limit)
    {
        return encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        state.context.compress_block(src, &mut state.pending)
    }));
    let compressed = match outcome {
        Ok(Ok(compressed)) => compressed,
        Ok(Err(_)) | Err(_) => {
            *stream = None;
            *bufferless = None;
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
    };
    progress.ingested += src.len() as u64;
    if !compressed {
        return 0;
    }
    let len = state.pending.len();
    if len > dst_capacity {
        // The block is already history a next block could refer to, and the
        // caller never received it: the session ends rather than go on.
        *stream = None;
        *bufferless = None;
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    dst[..len].copy_from_slice(&state.pending);
    state.pending.clear();
    progress.produced += len as u64;
    progress.flushed += len as u64;
    len
}

/// `size_t ZSTD_getBlockSize(const ZSTD_CCtx* cctx)` — the largest block the
/// buffer-less frame begun on `cctx` takes: its block size limits held to its
/// window; the format's 128 KiB when no frame is begun.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getBlockSize(cctx: *const ZSTD_CCtx) -> usize {
    unsafe { cctx.as_ref() }
        .filter(|cctx| cctx.bufferless.is_some())
        .and_then(|cctx| cctx.stream.as_ref())
        .and_then(|stream| stream.context.frame_block_size())
        .unwrap_or(codec::MAX_BLOCK_SIZE as usize)
}

/// `size_t ZSTD_copyCCtx(ZSTD_CCtx* dstCCtx, const ZSTD_CCtx* srcCCtx,
/// unsigned long long pledgedSrcSize)` — begin on `dstCCtx` the buffer-less
/// frame `srcCCtx` was begun with, before anything was compressed into it,
/// with its own pledge (`0` or `ZSTD_CONTENTSIZE_UNKNOWN` = unknown, as
/// upstream reads it) and the default frame flags.
///
/// # Safety
/// Both contexts live or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_copyCCtx(
    dst_cctx: *mut ZSTD_CCtx,
    src_cctx: *const ZSTD_CCtx,
    pledged_src_size: u64,
) -> usize {
    if dst_cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    // The source is read, and its borrow ended, before the destination is
    // borrowed mutably: the two may be the same context.
    let begin = {
        let Some(src) = (unsafe { src_cctx.as_ref() }) else {
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        };
        let Some(frame) = src.bufferless.as_ref().filter(|frame| frame.fresh) else {
            return encode(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
        };
        // 0 is unknown here, whatever the note beside the declaration in
        // zstd.h suggests: upstream converts it (zstd_compress.c:2596) and its
        // own callers rely on that, copying with 0 and then compressing input
        // (tests/fuzzer.c:2450, 4885). Reading it as an empty frame would
        // refuse what every upstream build accepts.
        let pledged = if pledged_src_size == 0 {
            CONTENTSIZE_UNKNOWN
        } else {
            pledged_src_size
        };
        let mut begin = frame.begin.clone();
        begin.params.pledged_src_size = pledged;
        begin.params.content_size_flag = pledged != CONTENTSIZE_UNKNOWN;
        begin.params.checksum_flag = false;
        begin.params.dict_id_flag = true;
        begin
    };
    begin_frame(unsafe { &mut *dst_cctx }, begin)
}

/// Where a buffer-less decode stands: which part of the frame the next
/// `ZSTD_decompressContinue` takes (upstream `ZSTD_dStage`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Stage {
    FrameHeaderSize,
    FrameHeader,
    BlockHeader,
    Block,
    LastBlock,
    Checksum,
    SkippableHeader,
    SkipFrame,
}

/// The buffer-less decode state of a decompression context.
pub(crate) struct BufferlessDecode {
    stage: Stage,
    /// Bytes the next `ZSTD_decompressContinue` must be given.
    expected: usize,
    /// The frame header gathered so far (`ZSTD_FRAMEHEADERSIZE_MAX` bytes).
    header: [u8; 18],
    header_len: usize,
    block_header: [u8; 3],
    /// Applied to every frame the run decodes.
    dictionary: Option<DictionaryHandle>,
    /// The frame format the run was begun in. A parameter reset after the
    /// begin changes the context's format for the next run, not this one.
    magicless: bool,
}

impl BufferlessDecode {
    /// The dictionary the run decodes every frame with.
    pub(crate) fn dictionary(&self) -> Option<&DictionaryHandle> {
        self.dictionary.as_ref()
    }

    /// A run waiting for a frame, in `magicless` format or not.
    pub(crate) fn new(magicless: bool) -> Self {
        Self {
            stage: Stage::FrameHeaderSize,
            expected: starting_input_length(magicless),
            header: [0; 18],
            header_len: 0,
            block_header: [0; 3],
            dictionary: None,
            magicless,
        }
    }
}

/// The first read of a frame: enough to learn its header's length (magic
/// number and descriptor, upstream `ZSTD_startingInputLength`).
pub(crate) fn starting_input_length(magicless: bool) -> usize {
    if magicless { 1 } else { 5 }
}

/// Begin a buffer-less run on `dctx` with `dictionary`: the frame state for
/// `ZSTD_decompressContinue` and a block run for `ZSTD_decompressBlock`.
fn begin_decode(dctx: &mut ZSTD_DCtx, dictionary: Option<DictionaryHandle>) -> usize {
    let mut run = BufferlessDecode::new(dctx.magicless);
    // A run of blocks has no frame window: upstream's block API matches as far
    // back as the history the caller kept, and its window ceiling binds the
    // streaming decoder alone (zstd_decompress.c:2231). Each frame of the run
    // is held to the ceiling again when its header arrives.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        dctx.decoder
            .set_max_window_size(codec::decoding::MAX_DECODER_WINDOW_SIZE)?;
        dctx.decoder.begin_blocks(dictionary.as_ref())
    }));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return encode(code_for_decoder_error(&err)),
        Err(_) => {
            dctx.replace_decoder();
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
    }
    run.dictionary = dictionary;
    dctx.bufferless = run;
    dctx.stream_frame_done = true;
    0
}

/// `size_t ZSTD_decompressBegin(ZSTD_DCtx* dctx)` — begin a buffer-less run
/// without a dictionary.
///
/// The history blocks match against is held by `dctx`, where upstream reads
/// it from the caller's buffers: it grows with the output decoded, nothing is
/// reserved up front, and it is capped at the decoder's largest window plus
/// one block. `ZSTD_sizeof_DCtx` reports what it holds.
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressBegin(dctx: *mut ZSTD_DCtx) -> usize {
    let Some(dctx) = (unsafe { dctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    begin_decode(dctx, None)
}

/// `size_t ZSTD_decompressBegin_usingDict(ZSTD_DCtx* dctx, const void* dict,
/// size_t dictSize)` — begin a buffer-less run with a dictionary (none when
/// empty).
///
/// # Safety
/// `dctx` live or `NULL`; `dict` valid for `dict_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressBegin_usingDict(
    dctx: *mut ZSTD_DCtx,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    let Some(dctx) = (unsafe { dctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let dict = unsafe { in_slice(dict, dict_size) };
    if dict.is_empty() {
        return begin_decode(dctx, None);
    }
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        crate::attach::parse_decode_dict(dict, crate::attach::ZSTD_DCT_AUTO)
    }));
    match parsed {
        Ok(Ok(handle)) => begin_decode(dctx, Some(handle)),
        Ok(Err(_)) | Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted),
    }
}

/// `size_t ZSTD_decompressBegin_usingDDict(ZSTD_DCtx* dctx, const ZSTD_DDict*
/// ddict)` — begin a buffer-less run with `ddict` (none when `NULL`).
///
/// # Safety
/// `dctx` live or `NULL`; `ddict` live or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressBegin_usingDDict(
    dctx: *mut ZSTD_DCtx,
    ddict: *const ZSTD_DDict,
) -> usize {
    let Some(dctx) = (unsafe { dctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    // The DDict was prepared at creation; the run shares its handle.
    let handle = unsafe { ddict.as_ref() }.and_then(|ddict| ddict.prepared.clone());
    begin_decode(dctx, handle)
}

/// `size_t ZSTD_nextSrcSizeToDecompress(ZSTD_DCtx* dctx)` — the exact number
/// of bytes the next `ZSTD_decompressContinue` takes; 0 once a frame is done.
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_nextSrcSizeToDecompress(dctx: *mut ZSTD_DCtx) -> usize {
    unsafe { dctx.as_ref() }.map_or(0, |dctx| dctx.bufferless.expected)
}

// `ZSTD_nextInputType_e` values.
const ZSTDNIT_FRAME_HEADER: c_uint = 0;
const ZSTDNIT_BLOCK_HEADER: c_uint = 1;
const ZSTDNIT_BLOCK: c_uint = 2;
const ZSTDNIT_LAST_BLOCK: c_uint = 3;
const ZSTDNIT_CHECKSUM: c_uint = 4;
const ZSTDNIT_SKIPPABLE_FRAME: c_uint = 5;

/// `ZSTD_nextInputType_e ZSTD_nextInputType(ZSTD_DCtx* dctx)` — which part of
/// the frame the next `ZSTD_decompressContinue` takes.
///
/// # Safety
/// `dctx` must be a live pointer from `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_nextInputType(dctx: *mut ZSTD_DCtx) -> c_uint {
    let Some(dctx) = (unsafe { dctx.as_ref() }) else {
        return ZSTDNIT_FRAME_HEADER;
    };
    match dctx.bufferless.stage {
        Stage::FrameHeaderSize | Stage::FrameHeader => ZSTDNIT_FRAME_HEADER,
        Stage::BlockHeader => ZSTDNIT_BLOCK_HEADER,
        Stage::Block => ZSTDNIT_BLOCK,
        Stage::LastBlock => ZSTDNIT_LAST_BLOCK,
        Stage::Checksum => ZSTDNIT_CHECKSUM,
        Stage::SkippableHeader | Stage::SkipFrame => ZSTDNIT_SKIPPABLE_FRAME,
    }
}

/// `size_t ZSTD_decompressContinue(ZSTD_DCtx* dctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize)` — take the next part of the
/// frame, exactly `ZSTD_nextSrcSizeToDecompress` bytes, and return the bytes
/// it decoded into `dst` (0 for a part that decodes to nothing).
///
/// # Safety
/// `dctx` live or `NULL`; `dst` / `src` valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressContinue(
    dctx: *mut ZSTD_DCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(dctx) = (unsafe { dctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    if src_size != dctx.bufferless.expected {
        return encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
    }
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let outcome = catch_unwind(AssertUnwindSafe(|| decompress_step(dctx, dst, src)));
    match outcome {
        Ok(Ok(written)) => written,
        Ok(Err(code)) => encode(code),
        Err(_) => {
            dctx.replace_decoder();
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}

/// The body of [`ZSTD_decompressContinue`], once `src` is the size expected.
fn decompress_step(
    dctx: &mut ZSTD_DCtx,
    dst: &mut [u8],
    src: &[u8],
) -> Result<usize, ZSTD_ErrorCode> {
    let failed = |err: FrameDecoderError| code_for_decoder_error(&err);
    let run = &mut dctx.bufferless;
    match run.stage {
        Stage::FrameHeaderSize => {
            if let Some(codec::decoding::FrameMagic::Skippable(_)) = (!run.magicless)
                .then(|| codec::decoding::frame_magic(src))
                .flatten()
            {
                run.header[..src.len()].copy_from_slice(src);
                run.header_len = src.len();
                run.expected = codec::skippable::SKIPPABLE_HEADER_SIZE - src.len();
                run.stage = Stage::SkippableHeader;
                return Ok(0);
            }
            let header_size = codec::decoding::frame_header_size_in_format(src, run.magicless)
                .map_err(|err| match err {
                    ReadFrameHeaderError::BadMagicNumber(_) => {
                        ZSTD_ErrorCode::ZSTD_error_prefix_unknown
                    }
                    other => failed(FrameDecoderError::ReadFrameHeaderError(other)),
                })?;
            run.header[..src.len()].copy_from_slice(src);
            run.header_len = src.len();
            run.expected = header_size - src.len();
            run.stage = Stage::FrameHeader;
            Ok(0)
        }
        Stage::FrameHeader => {
            let end = run.header_len + src.len();
            run.header[run.header_len..end].copy_from_slice(src);
            run.header_len = end;
            dctx.decoder.set_content_checksum(dctx.content_checksum);
            dctx.decoder
                .set_max_window_size(dctx.max_window_size)
                .map_err(failed)?;
            let mut header = &run.header[..end];
            // The header is read in the run's format, and the decoder goes
            // back to the context's for the frames after it.
            dctx.decoder.set_magicless(run.magicless);
            let parsed = match run.dictionary.as_ref() {
                Some(dictionary) => dctx.decoder.reset_with_dict_handle(&mut header, dictionary),
                None => dctx.decoder.reset(&mut header),
            };
            dctx.decoder.set_magicless(dctx.magicless);
            parsed.map_err(failed)?;
            run.expected = 3;
            run.stage = Stage::BlockHeader;
            Ok(0)
        }
        Stage::BlockHeader => {
            let header = [src[0], src[1], src[2]];
            let (content_len, last) = dctx.decoder.block_content_len(header).map_err(failed)?;
            run.block_header = header;
            if content_len > 0 {
                run.expected = content_len;
                run.stage = if last { Stage::LastBlock } else { Stage::Block };
                return Ok(0);
            }
            // An empty block is all header: decoded at once, it ends the
            // frame when it is the last.
            dctx.decoder
                .decode_block(header, &[], &mut [])
                .map_err(failed)?;
            finish_block(dctx, last);
            Ok(0)
        }
        Stage::Block | Stage::LastBlock => {
            let last = run.stage == Stage::LastBlock;
            let header = run.block_header;
            let written = dctx
                .decoder
                .decode_block(header, src, dst)
                .map_err(|err| match err {
                    FrameDecoderError::TargetTooSmall => {
                        ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall
                    }
                    other => failed(other),
                })?;
            finish_block(dctx, last);
            Ok(written)
        }
        Stage::Checksum => {
            let checksum = [src[0], src[1], src[2], src[3]];
            dctx.decoder
                .read_content_checksum(checksum)
                .map_err(failed)?;
            run.expected = 0;
            run.stage = Stage::FrameHeaderSize;
            Ok(0)
        }
        Stage::SkippableHeader => {
            let size = codec::skippable::SKIPPABLE_HEADER_SIZE;
            run.header[run.header_len..size].copy_from_slice(src);
            run.expected =
                u32::from_le_bytes([run.header[4], run.header[5], run.header[6], run.header[7]])
                    as usize;
            run.stage = Stage::SkipFrame;
            Ok(0)
        }
        Stage::SkipFrame => {
            run.expected = 0;
            run.stage = Stage::FrameHeaderSize;
            Ok(0)
        }
    }
}

/// Move a run past the block just decoded: to the next block header, or, for
/// the last block, to the checksum or the end of the frame.
fn finish_block(dctx: &mut ZSTD_DCtx, last: bool) {
    let run = &mut dctx.bufferless;
    if !last {
        run.expected = 3;
        run.stage = Stage::BlockHeader;
    } else if dctx.decoder.awaits_checksum() {
        run.expected = 4;
        run.stage = Stage::Checksum;
    } else {
        run.expected = 0;
        run.stage = Stage::FrameHeaderSize;
    }
}

/// `size_t ZSTD_decompressBlock(ZSTD_DCtx* dctx, void* dst, size_t
/// dstCapacity, const void* src, size_t srcSize)` — decode the content of
/// one compressed raw block, as `ZSTD_compressBlock` produced it, against the
/// blocks before it in the run `ZSTD_decompressBegin` started; returns the
/// bytes written.
///
/// # Safety
/// `dctx` live or `NULL`; `dst` / `src` valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressBlock(
    dctx: *mut ZSTD_DCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    let Some(dctx) = (unsafe { dctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    if src_size > codec::MAX_BLOCK_SIZE as usize {
        return encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
    }
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    // RFC 8878 3.1.1.2: a Compressed block (type 2) of `src_size` bytes that
    // never ends the run; the size fits the header's 21 bits, checked above.
    let header = ((src_size as u32) << 3 | 2 << 1).to_le_bytes();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        dctx.decoder
            .decode_block([header[0], header[1], header[2]], src, dst)
    }));
    match outcome {
        Ok(Ok(written)) => written,
        Ok(Err(FrameDecoderError::TargetTooSmall)) => {
            encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall)
        }
        Ok(Err(err)) => encode(code_for_decoder_error(&err)),
        Err(_) => {
            dctx.replace_decoder();
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}

/// `size_t ZSTD_insertBlock(ZSTD_DCtx* dctx, const void* blockStart, size_t
/// blockSize)` — take a block the caller stored uncompressed into the history
/// of the run, so later compressed blocks can match against it; returns
/// `blockSize`.
///
/// # Safety
/// `dctx` live or `NULL`; `block_start` valid for `block_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_insertBlock(
    dctx: *mut ZSTD_DCtx,
    block_start: *const u8,
    block_size: usize,
) -> usize {
    let Some(dctx) = (unsafe { dctx.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let block = unsafe { in_slice(block_start, block_size) };
    match dctx.decoder.insert_block(block) {
        Ok(()) => block_size,
        Err(err) => encode(code_for_decoder_error(&err)),
    }
}

/// `void ZSTD_copyDCtx(ZSTD_DCtx* dctx, const ZSTD_DCtx* preparedDCtx)` —
/// make `dctx` a copy of `preparedDCtx` as `ZSTD_decompressBegin*` left it:
/// its format, window limit, dictionaries and buffer-less run, ready for its
/// first frame or block. A run part-way through a frame is copied as begun
/// afresh with the same dictionary.
///
/// # Safety
/// Both contexts live or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_copyDCtx(dctx: *mut ZSTD_DCtx, prepared_dctx: *const ZSTD_DCtx) {
    // Compared as pointers before any reference exists: a `&mut` and a `&` to
    // one context would alias.
    if core::ptr::eq(dctx.cast_const(), prepared_dctx) {
        return;
    }
    let (Some(dst), Some(src)) = (unsafe { dctx.as_mut() }, unsafe { prepared_dctx.as_ref() })
    else {
        return;
    };
    dst.max_window_size = src.max_window_size;
    dst.magicless = src.magicless;
    dst.decoder.set_magicless(src.magicless);
    dst.content_checksum = src.content_checksum;
    dst.ref_multiple_ddicts = src.ref_multiple_ddicts;
    dst.max_block_size = src.max_block_size;
    dst.apply_max_block_size();
    dst.stable_out_buffer = src.stable_out_buffer;
    dst.disable_huffman_assembly = src.disable_huffman_assembly;
    dst.attached_ddict = src.attached_ddict.clone();
    let dictionary = src.bufferless.dictionary.clone();
    // `begin_decode` leaves `dst` at the start of a run with the same
    // dictionary, the documented copy. A copy cannot fail, so a begin that
    // fails leaves `dst` with the settings and no run.
    if crate::error::result_is_error(begin_decode(dst, dictionary)) {
        dst.bufferless = BufferlessDecode::new(dst.magicless);
    }
}
