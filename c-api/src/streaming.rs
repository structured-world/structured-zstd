//! Streaming API: `ZSTD_compressStream2` / `ZSTD_decompressStream` plus the
//! legacy `ZSTD_initCStream` / `ZSTD_compressStream` / `ZSTD_flushStream` /
//! `ZSTD_endStream` / `ZSTD_initDStream` entry points and the recommended
//! buffer-size queries.
//!
//! `ZSTD_CStream` / `ZSTD_DStream` are the same objects as `ZSTD_CCtx` /
//! `ZSTD_DCtx` (upstream v1.3.0+ semantics); the create/free "stream"
//! functions alias the context ones.

use core::ffi::{c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::encoding::{CompressionContext, CompressionLevel};

use crate::context::{ZSTD_CCtx, ZSTD_DCtx};
use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::params::CONTENTSIZE_UNKNOWN;

/// `ZSTD_BLOCKSIZE_MAX`, the recommended streaming input granule. The codec's
/// block maximum, named rather than re-declared.
const BLOCK_SIZE_MAX: usize = codec::MAX_BLOCK_SIZE as usize;

/// `ZSTD_inBuffer` — ABI mirror: `{ const void* src; size_t size; size_t pos }`.
#[repr(C)]
#[allow(non_camel_case_types, non_snake_case)]
#[derive(Copy, Clone, Debug)]
pub struct ZSTD_inBuffer {
    pub src: *const c_void,
    pub size: usize,
    pub pos: usize,
}

/// `ZSTD_outBuffer` — ABI mirror: `{ void* dst; size_t size; size_t pos }`.
#[repr(C)]
#[allow(non_camel_case_types, non_snake_case)]
#[derive(Copy, Clone, Debug)]
pub struct ZSTD_outBuffer {
    pub dst: *mut c_void,
    pub size: usize,
    pub pos: usize,
}

// `ZSTD_EndDirective` discriminants.
const ZSTD_E_CONTINUE: c_int = 0;
const ZSTD_E_FLUSH: c_int = 1;
const ZSTD_E_END: c_int = 2;

/// Streaming-compression state stored on the `ZSTD_CCtx`, kept from one
/// streamed frame to the next as upstream keeps its `ZSTD_CCtx`: the
/// context's match-finder tables, buffers and dictionary are built once.
pub(crate) struct CStreamState {
    /// Compresses every streamed frame, writing straight into `pending`.
    pub(crate) context: CompressionContext,
    /// Serial of the dictionary attached to `context` (`0` = none), so the
    /// same dictionary on the next frame is not attached again.
    dictionary: u64,
    /// Whether a frame is taking input: from its first call until
    /// `ZSTD_e_end` closes it.
    pub(crate) open: bool,
    /// Frame bytes produced but not yet copied out to the caller.
    pub(crate) pending: Vec<u8>,
    /// Read offset into `pending`.
    pub(crate) pending_pos: usize,
}

impl CStreamState {
    fn new(level: CompressionLevel) -> Self {
        Self {
            context: CompressionContext::new(level),
            dictionary: 0,
            open: false,
            pending: Vec::new(),
            pending_pos: 0,
        }
    }

    /// Heap bytes the stream holds: the context's tables, buffers and
    /// dictionary, and the output not yet copied out. Feeds
    /// `ZSTD_sizeof_CCtx` so a context that streams reports its true
    /// footprint.
    pub(crate) fn heap_size(&self) -> usize {
        self.pending.capacity() + self.context.heap_size()
    }

    /// The dictionary the kept context holds, if any.
    pub(crate) fn dictionary(&self) -> Option<&codec::encoding::EncoderDictionary> {
        self.context.dictionary()
    }

    /// Whether a frame is under way: taking input, or closed with part of
    /// it still to be copied out.
    pub(crate) fn in_frame(&self) -> bool {
        self.open || self.pending_remaining() > 0
    }

    /// Identity of the dictionary the kept context holds (`0` = none).
    pub(crate) fn dictionary_serial(&self) -> u64 {
        self.dictionary
    }

    /// Detach the dictionary `serial` names from the context, if that is the
    /// one it holds and no frame is taking input. A closed frame whose tail is
    /// still being copied out no longer reads it.
    pub(crate) fn release_dictionary(&mut self, serial: u64) {
        if serial != 0 && self.dictionary == serial && !self.open {
            // Clearing is refused only mid-frame, which was excluded above.
            let _cleared = self.context.set_dictionary_from_bytes(&[]);
            self.dictionary = 0;
        }
    }

    fn pending_remaining(&self) -> usize {
        self.pending.len() - self.pending_pos
    }

    /// Bytes reserved for output not yet copied out. Test-only.
    #[cfg(test)]
    pub(crate) fn pending_capacity(&self) -> usize {
        self.pending.capacity()
    }

    /// Copy as much produced output as fits into `out`.
    fn copy_out(&mut self, out: &mut ZSTD_outBuffer, dst: &mut [u8]) {
        let n = self.pending_remaining().min(dst.len() - out.pos);
        dst[out.pos..out.pos + n]
            .copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
        self.pending_pos += n;
        out.pos += n;
        if self.pending_pos == self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
            // A large input written into a small output buffer piles its
            // whole output up here. Once the frame is closed and copied out,
            // the context keeps the room of one block (upstream's
            // `outBuffSize`) for the frames after it, not the peak. Within a
            // frame the room stays: the caller's next call is likely to fill
            // it the same way, and shrinking would re-grow it every time.
            if !self.open {
                self.pending.shrink_to(ZSTD_CStreamOutSize());
            }
        }
    }
}

impl ZSTD_CCtx {
    /// Start a streaming frame from the sticky parameters on the kept
    /// context. No-op if one is already in flight.
    fn ensure_stream(&mut self) -> Result<(), ZSTD_ErrorCode> {
        if self.stream_in_progress() {
            return Ok(());
        }
        let params = self.params;
        // A stream knows its size only if the caller pledged one; without a
        // pledge the frame is of unknown size, which is where a referenced
        // CDict's own parameters are the better guess (upstream reads
        // `pledgedSrcSize` at the same branch).
        let src_size = (params.pledged_src_size != crate::params::CONTENTSIZE_UNKNOWN)
            .then_some(params.pledged_src_size);
        let frame_params = self.frame_parameters(src_size)?;
        let level = self.attach_level(src_size);
        let serial = self.attach_serial();
        let suppress_id = self.attach_suppresses_dict_id();
        let ZSTD_CCtx {
            stream,
            attached_dict,
            ..
        } = self;
        let setup = FrameSetup {
            params: &params,
            frame_params: frame_params.as_ref(),
            level,
            dictionary: attached_dict.prepared(),
            serial,
            suppress_id,
        };
        CStreamState::open_frame(stream, &setup)?;
        self.progress = crate::context::FrameProgress::default();
        self.params.pledged_src_size = CONTENTSIZE_UNKNOWN;
        self.consume_prefix();
        Ok(())
    }
}

/// What the next frame on the kept streaming context runs under.
pub(crate) struct FrameSetup<'a> {
    /// Flags, block limits, format, hint and pledge.
    pub(crate) params: &'a crate::cctx_params::CCtxParams,
    /// Explicit parameters, or `None` for `level`'s own tuning.
    pub(crate) frame_params: Option<&'a codec::encoding::CompressionParameters>,
    pub(crate) level: c_int,
    pub(crate) dictionary: Option<&'a codec::encoding::EncoderDictionary>,
    /// Identity of `dictionary` (`0` = none), so the same one is not attached
    /// to the kept context twice.
    pub(crate) serial: u64,
    /// A raw-content dictionary's synthetic ID never reaches the wire.
    pub(crate) suppress_id: bool,
}

impl CStreamState {
    /// Put the context in `stream`, made at `setup.level` when there is none,
    /// on `setup` and open its next frame. A context that refuses a setting is
    /// dropped: it is not trusted with the next frame either.
    pub(crate) fn open_frame<'s>(
        stream: &'s mut Option<CStreamState>,
        setup: &FrameSetup<'_>,
    ) -> Result<&'s mut CStreamState, ZSTD_ErrorCode> {
        let state = stream
            .get_or_insert_with(|| CStreamState::new(CompressionLevel::from_level(setup.level)));
        if state.configure(setup).is_err() {
            *stream = None;
            return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
        let state = stream.as_mut().expect("configured just above");
        state.open = true;
        Ok(state)
    }

    fn configure(&mut self, setup: &FrameSetup<'_>) -> Result<(), codec::io::Error> {
        let params = setup.params;
        let context = &mut self.context;
        if self.dictionary != setup.serial {
            match setup.dictionary {
                Some(dictionary) => context.set_encoder_dictionary(dictionary.clone())?,
                None => context.set_dictionary_from_bytes(&[])?,
            }
            self.dictionary = setup.serial;
        }
        match setup.frame_params {
            Some(frame_params) => context.set_parameters(frame_params)?,
            None => context.set_compression_level(CompressionLevel::from_level(setup.level))?,
        }
        context.set_content_checksum(params.checksum_flag)?;
        context.set_target_block_size(params.target_block_size())?;
        context.set_max_block_size(params.max_block_size())?;
        context.set_magicless(params.magicless)?;
        // The hint is a parameter: it stays for every frame until changed.
        match params.src_size_hint {
            0 => context.clear_source_size_hint()?,
            hint => context.set_source_size_hint(hint as u64)?,
        }
        // The pledge is single-use (consumed by this frame). It is always
        // enforced against the bytes actually written; the content-size flag
        // only controls whether the header carries the FCS field (upstream
        // validates the pledge at frame end regardless of the flag).
        if params.pledged_src_size != CONTENTSIZE_UNKNOWN {
            context.set_pledged_content_size(params.pledged_src_size)?;
        }
        context.set_content_size_flag(params.content_size_flag)?;
        context.set_dictionary_id_flag(params.dict_id_flag && !setup.suppress_id)?;
        let (producer, fallback) = params.sequence_producer();
        context.set_sequence_producer(producer, fallback);
        Ok(())
    }
}

/// `ZSTD_CStream* ZSTD_createCStream(void)` — same object as a `ZSTD_CCtx`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCStream() -> *mut ZSTD_CCtx {
    crate::context::ZSTD_createCCtx()
}

/// `ZSTD_CStream* ZSTD_createCStream_advanced(ZSTD_customMem customMem)` —
/// same object as a `ZSTD_CCtx`, see `ZSTD_createCCtx_advanced`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCStream_advanced(
    custom_mem: crate::cdict::ZSTD_customMem,
) -> *mut ZSTD_CCtx {
    crate::context::ZSTD_createCCtx_advanced(custom_mem)
}

/// `ZSTD_DStream* ZSTD_createDStream_advanced(ZSTD_customMem customMem)` —
/// same object as a `ZSTD_DCtx`, see `ZSTD_createDCtx_advanced`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createDStream_advanced(
    custom_mem: crate::cdict::ZSTD_customMem,
) -> *mut ZSTD_DCtx {
    crate::context::ZSTD_createDCtx_advanced(custom_mem)
}

/// `size_t ZSTD_freeCStream(ZSTD_CStream* zcs)` — `NULL` is a no-op.
///
/// # Safety
/// `zcs` must be a live pointer from `ZSTD_createCStream` /
/// `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeCStream(zcs: *mut ZSTD_CCtx) -> usize {
    unsafe { crate::context::ZSTD_freeCCtx(zcs) }
}

/// `size_t ZSTD_sizeof_CStream(const ZSTD_CStream* zcs)` — `ZSTD_CStream`
/// is the same object as a `ZSTD_CCtx`, so this is `ZSTD_sizeof_CCtx`.
///
/// # Safety
/// Same as [`ZSTD_freeCStream`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_CStream(zcs: *const ZSTD_CCtx) -> usize {
    unsafe { crate::context::ZSTD_sizeof_CCtx(zcs) }
}

/// `size_t ZSTD_CStreamInSize(void)` — recommended input granule
/// (`ZSTD_BLOCKSIZE_MAX`).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_CStreamInSize() -> usize {
    BLOCK_SIZE_MAX
}

/// `size_t ZSTD_CStreamOutSize(void)` — output size guaranteeing room to
/// flush one complete compressed block
/// (`compressBound(BLOCKSIZE_MAX) + blockHeader(3) + checksum(4)`).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_CStreamOutSize() -> usize {
    crate::simple::ZSTD_compressBound(BLOCK_SIZE_MAX) + 3 + 4
}

/// Borrowed views over the caller's streaming buffer structs.
type StreamBuffers<'a> = (
    &'a mut ZSTD_outBuffer,
    &'a mut ZSTD_inBuffer,
    &'a mut [u8],
    &'a [u8],
);

/// Validate the caller's buffer structs; returns the borrowed `(dst, src)`
/// slices.
///
/// # Safety
/// Caller guarantees the buffer pointers are valid for their `size` fields.
unsafe fn stream_buffers<'a>(
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
) -> Result<StreamBuffers<'a>, ZSTD_ErrorCode> {
    if output.is_null() || input.is_null() {
        return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let out = unsafe { &mut *output };
    let inp = unsafe { &mut *input };
    if out.pos > out.size || inp.pos > inp.size {
        return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dst = unsafe { crate::ffi::out_slice(out.dst as *mut u8, out.size) };
    let src = unsafe { crate::ffi::in_slice(inp.src as *const u8, inp.size) };
    Ok((out, inp, dst, src))
}

/// `size_t ZSTD_compressStream2(ZSTD_CCtx* cctx, ZSTD_outBuffer* output,
/// ZSTD_inBuffer* input, ZSTD_EndDirective endOp)`.
///
/// Single-threaded blocking semantics: input is consumed in full on every
/// call (compressed bytes accumulate on the context when `output` is too
/// small); the return value is the number of bytes still to be flushed
/// (`0` after `ZSTD_e_end` means the frame is complete).
///
/// # Safety
/// `cctx` must be live; `output` / `input` must point to valid buffer
/// structs whose `dst` / `src` are valid for their `size` fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressStream2(
    cctx: *mut ZSTD_CCtx,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
    end_op: c_int,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    let (out, inp, dst, src) = match unsafe { stream_buffers(output, input) } {
        Ok(v) => v,
        Err(code) => return encode(code),
    };
    if !matches!(end_op, ZSTD_E_CONTINUE | ZSTD_E_FLUSH | ZSTD_E_END) {
        return encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound);
    }
    // `ensure_stream` runs unconditionally — including for an empty
    // continue/flush on a fresh context. That intentionally mirrors the
    // reference implementation: upstream's first compressStream2 call
    // moves streamStage to zcss_load regardless of input emptiness, and
    // ZSTD_compress2 / parameter setters report stage_wrong until a
    // reset ends the session. Short-circuiting the idle case here would
    // diverge from that drop-in behavior.
    if let Err(code) = cctx.ensure_stream() {
        return encode(code);
    }
    let attached = cctx.attach_serial();
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<usize, ZSTD_ErrorCode> {
        let stream = cctx.stream.as_mut().expect("ensure_stream installed state");
        let progress = &mut cctx.progress;
        let pending_before = stream.pending.len();
        let input_before = inp.pos;
        // Consume ALL remaining input: the in-memory output has no
        // backpressure, so a write never short-counts on its account.
        if inp.pos < inp.size {
            if !stream.open {
                // Frame already ended but not fully flushed: only flush/end
                // directives are legal (upstream stage_wrong).
                return Err(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
            }
            // Loop over partial writes: the context legally short-counts at
            // the pledged-size boundary (accepts the remaining allowance,
            // then errors on the next call), and stopping at the first
            // short count would leave `inp.pos` claiming nothing was
            // consumed when most of it was.
            while inp.pos < inp.size {
                match stream.context.write(&mut stream.pending, &src[inp.pos..]) {
                    Ok(0) => return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC),
                    Ok(n) => inp.pos += n,
                    // The context reports pledge violations as InvalidInput
                    // (the only InvalidInput reachable from `write` on the
                    // in-memory output); upstream's error for input past the
                    // pledged size is srcSize_wrong.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                        return Err(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
                    }
                    Err(_) => return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC),
                }
            }
        }
        match end_op {
            ZSTD_E_FLUSH if stream.open => {
                if stream.context.flush(&mut stream.pending).is_err() {
                    return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC);
                }
            }
            ZSTD_E_END if stream.open => {
                match stream.context.finish_frame(&mut stream.pending) {
                    // Closed; the context stays for the next frame, and the
                    // stream counts as in progress only while the tail is
                    // still being copied out, however many calls that takes.
                    // A dictionary that is no longer the attached one, a
                    // prefix this frame spent, goes with the frame.
                    Ok(()) => {
                        stream.open = false;
                        if stream.dictionary != attached {
                            stream.release_dictionary(stream.dictionary);
                        }
                    }
                    // Pledge mismatch at frame end surfaces as InvalidInput;
                    // map it to the same srcSize_wrong the write() path
                    // reports so the error code does not depend on where the
                    // contract check fires.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                        return Err(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
                    }
                    Err(_) => return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC),
                }
            }
            _ => {}
        }
        // Without fallback a producer failure fails the frame, as upstream's
        // `ZSTD_buildSeqStore` returns it (zstd_compress.c:3396).
        if stream.context.take_sequence_producer_failure() {
            return Err(ZSTD_ErrorCode::ZSTD_error_sequenceProducer_failed);
        }
        // Nothing drains `pending` before `copy_out`, so what it grew by is
        // what this call produced.
        progress.ingested += (inp.pos - input_before) as u64;
        progress.produced += (stream.pending.len() - pending_before) as u64;
        let flushed_before = out.pos;
        stream.copy_out(out, dst);
        progress.flushed += (out.pos - flushed_before) as u64;
        Ok(stream.pending_remaining())
    }));
    match outcome {
        Ok(Ok(remaining)) => remaining,
        Ok(Err(ZSTD_ErrorCode::ZSTD_error_sequenceProducer_failed)) => {
            // The frame is lost: the next call starts a new one, as after the
            // reset upstream requires.
            cctx.stream = None;
            encode(ZSTD_ErrorCode::ZSTD_error_sequenceProducer_failed)
        }
        Ok(Err(code)) => encode(code),
        Err(_) => {
            // A panic mid-stream leaves the encoder state undefined; drop it
            // so the context is reusable (upstream requires an explicit
            // reset, which `ensure_stream` makes implicit here).
            cctx.stream = None;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}

/// `size_t ZSTD_initCStream(ZSTD_CStream* zcs, int compressionLevel)` —
/// legacy: session reset + clear dictionary + set the level.
///
/// # Safety
/// `zcs` must be a live pointer from `ZSTD_createCStream`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initCStream(zcs: *mut ZSTD_CCtx, compression_level: c_int) -> usize {
    if zcs.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *zcs };
    cctx.reset_session();
    // This init clears any previously loaded dictionary, in the kept
    // compressors too.
    cctx.replace_attached_dict(crate::attach::CCtxDictAttach::None);
    set_level(cctx, compression_level)
}

/// Store `level` as `ZSTD_CCtx_setParameter(ZSTD_c_compressionLevel)` would.
fn set_level(cctx: &mut ZSTD_CCtx, level: c_int) -> usize {
    match cctx
        .params
        .set(crate::params::ZSTD_C_COMPRESSION_LEVEL, level)
    {
        Ok(()) => 0,
        Err(code) => encode(code),
    }
}

/// The pledged-size argument of the older init functions, where `0` still
/// means "unknown".
fn pledge_from(pledged_src_size: u64) -> u64 {
    if pledged_src_size == 0 {
        CONTENTSIZE_UNKNOWN
    } else {
        pledged_src_size
    }
}

/// `size_t ZSTD_initCStream_srcSize(ZSTD_CStream* zcs, int compressionLevel,
/// unsigned long long pledgedSrcSize)` — [`ZSTD_initCStream`] with the size of
/// the next frame pledged (`0` = unknown).
///
/// # Safety
/// `zcs` must be a live pointer from `ZSTD_createCStream`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initCStream_srcSize(
    zcs: *mut ZSTD_CCtx,
    compression_level: c_int,
    pledged_src_size: u64,
) -> usize {
    let rc = unsafe { ZSTD_initCStream(zcs, compression_level) };
    if crate::error::result_is_error(rc) {
        return rc;
    }
    unsafe { crate::params::ZSTD_CCtx_setPledgedSrcSize(zcs, pledge_from(pledged_src_size)) }
}

/// `size_t ZSTD_initCStream_usingDict(ZSTD_CStream* zcs, const void* dict,
/// size_t dictSize, int compressionLevel)` — start a session at
/// `compressionLevel` with `dict` loaded (none when empty).
///
/// # Safety
/// `zcs` must be live or `NULL`; `dict` valid for `dict_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initCStream_usingDict(
    zcs: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
    compression_level: c_int,
) -> usize {
    let Some(cctx) = (unsafe { zcs.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    cctx.reset_session();
    let rc = set_level(cctx, compression_level);
    if crate::error::result_is_error(rc) {
        return rc;
    }
    unsafe { crate::attach::ZSTD_CCtx_loadDictionary(zcs, dict, dict_size) }
}

/// `size_t ZSTD_initCStream_advanced(ZSTD_CStream* zcs, const void* dict,
/// size_t dictSize, ZSTD_parameters params, unsigned long long
/// pledgedSrcSize)` — start a session under explicit compression and frame
/// parameters with `dict` loaded. A `0` pledge means "unknown" only when the
/// frame parameters leave the content size out.
///
/// # Safety
/// `zcs` must be live or `NULL`; `dict` valid for `dict_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initCStream_advanced(
    zcs: *mut ZSTD_CCtx,
    dict: *const u8,
    dict_size: usize,
    params: crate::params::ZSTD_parameters,
    pledged_src_size: u64,
) -> usize {
    let Some(cctx) = (unsafe { zcs.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let pledged = if pledged_src_size == 0 && params.fParams.contentSizeFlag == 0 {
        CONTENTSIZE_UNKNOWN
    } else {
        pledged_src_size
    };
    cctx.reset_session();
    cctx.params.pledged_src_size = pledged;
    let checked = crate::params::ZSTD_checkCParams(params.cParams);
    if crate::error::result_is_error(checked) {
        return checked;
    }
    cctx.params.set_explicit(&params.cParams, &params.fParams);
    unsafe { crate::attach::ZSTD_CCtx_loadDictionary(zcs, dict, dict_size) }
}

/// `size_t ZSTD_initCStream_usingCDict(ZSTD_CStream* zcs, const ZSTD_CDict*
/// cdict)` — start a session referencing `cdict` (none when `NULL`).
///
/// # Safety
/// `zcs` must be live or `NULL`; `cdict` live or `NULL`, outliving the session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initCStream_usingCDict(
    zcs: *mut ZSTD_CCtx,
    cdict: *const crate::cdict::ZSTD_CDict,
) -> usize {
    let Some(cctx) = (unsafe { zcs.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    cctx.reset_session();
    unsafe { crate::attach::ZSTD_CCtx_refCDict(zcs, cdict) }
}

/// `size_t ZSTD_initCStream_usingCDict_advanced(ZSTD_CStream* zcs, const
/// ZSTD_CDict* cdict, ZSTD_frameParameters fParams, unsigned long long
/// pledgedSrcSize)` — [`ZSTD_initCStream_usingCDict`] with frame parameters
/// and the next frame's size.
///
/// # Safety
/// As [`ZSTD_initCStream_usingCDict`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initCStream_usingCDict_advanced(
    zcs: *mut ZSTD_CCtx,
    cdict: *const crate::cdict::ZSTD_CDict,
    fparams: crate::cdict::ZSTD_frameParameters,
    pledged_src_size: u64,
) -> usize {
    let Some(cctx) = (unsafe { zcs.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    cctx.reset_session();
    cctx.params.pledged_src_size = pledged_src_size;
    cctx.params.content_size_flag = fparams.contentSizeFlag != 0;
    cctx.params.checksum_flag = fparams.checksumFlag != 0;
    cctx.params.dict_id_flag = fparams.noDictIDFlag == 0;
    unsafe { crate::attach::ZSTD_CCtx_refCDict(zcs, cdict) }
}

/// `size_t ZSTD_resetCStream(ZSTD_CStream* zcs, unsigned long long
/// pledgedSrcSize)` — abandon any frame in flight and pledge the next one's
/// size (`0` = unknown); parameters and dictionary stay.
///
/// # Safety
/// `zcs` must be a live pointer from `ZSTD_createCStream`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_resetCStream(zcs: *mut ZSTD_CCtx, pledged_src_size: u64) -> usize {
    let Some(cctx) = (unsafe { zcs.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    cctx.reset_session();
    cctx.params.pledged_src_size = pledge_from(pledged_src_size);
    0
}

/// `size_t ZSTD_compressStream2_simpleArgs(ZSTD_CCtx* cctx, void* dst, size_t
/// dstCapacity, size_t* dstPos, const void* src, size_t srcSize, size_t*
/// srcPos, ZSTD_EndDirective endOp)` — [`ZSTD_compressStream2`] with the
/// buffer structs spelled out as arguments.
///
/// # Safety
/// As [`ZSTD_compressStream2`]; `dst_pos` / `src_pos` must be valid.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)] // the C signature
pub unsafe extern "C" fn ZSTD_compressStream2_simpleArgs(
    cctx: *mut ZSTD_CCtx,
    dst: *mut c_void,
    dst_capacity: usize,
    dst_pos: *mut usize,
    src: *const c_void,
    src_size: usize,
    src_pos: *mut usize,
    end_op: c_int,
) -> usize {
    if dst_pos.is_null() || src_pos.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let mut output = ZSTD_outBuffer {
        dst,
        size: dst_capacity,
        pos: unsafe { *dst_pos },
    };
    let mut input = ZSTD_inBuffer {
        src,
        size: src_size,
        pos: unsafe { *src_pos },
    };
    let rc = unsafe { ZSTD_compressStream2(cctx, &mut output, &mut input, end_op) };
    unsafe {
        *dst_pos = output.pos;
        *src_pos = input.pos;
    }
    rc
}

/// `ZSTD_frameProgression` — ABI mirror: bytes taken in, compressed, produced
/// and handed out for the current frame, plus the worker fields a
/// single-threaded context leaves at 0.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZSTD_frameProgression {
    pub ingested: u64,
    pub consumed: u64,
    pub produced: u64,
    pub flushed: u64,
    pub currentJobID: core::ffi::c_uint,
    pub nbActiveWorkers: core::ffi::c_uint,
}

/// `ZSTD_frameProgression ZSTD_getFrameProgression(const ZSTD_CCtx* cctx)` —
/// how far the frame in progress, or the last one, has got. Input waiting for
/// its block to fill counts as ingested but not yet consumed.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getFrameProgression(cctx: *const ZSTD_CCtx) -> ZSTD_frameProgression {
    let mut report = ZSTD_frameProgression {
        ingested: 0,
        consumed: 0,
        produced: 0,
        flushed: 0,
        currentJobID: 0,
        nbActiveWorkers: 0,
    };
    let Some(cctx) = (unsafe { cctx.as_ref() }) else {
        return report;
    };
    let buffered = cctx
        .stream
        .as_ref()
        .filter(|stream| stream.open)
        .map_or(0, |stream| stream.context.buffered_input_len() as u64);
    report.ingested = cctx.progress.ingested;
    report.consumed = cctx.progress.ingested - buffered;
    report.produced = cctx.progress.produced;
    report.flushed = cctx.progress.flushed;
    report
}

/// `size_t ZSTD_toFlushNow(ZSTD_CCtx* cctx)` — compressed bytes ready to be
/// copied out right now: what the last call produced and the output buffer
/// could not take.
///
/// # Safety
/// `cctx` must be a live pointer from `ZSTD_createCCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_toFlushNow(cctx: *mut ZSTD_CCtx) -> usize {
    unsafe { cctx.as_ref() }
        .and_then(|cctx| cctx.stream.as_ref())
        .map_or(0, CStreamState::pending_remaining)
}

/// `size_t ZSTD_compressStream(ZSTD_CStream* zcs, ZSTD_outBuffer* output,
/// ZSTD_inBuffer* input)` — legacy alias for `ZSTD_compressStream2(...,
/// ZSTD_e_continue)`; returns a recommended next input size.
///
/// # Safety
/// Same contracts as [`ZSTD_compressStream2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressStream(
    zcs: *mut ZSTD_CCtx,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
) -> usize {
    let rc = unsafe { ZSTD_compressStream2(zcs, output, input, ZSTD_E_CONTINUE) };
    if crate::error::ZSTD_isError(rc) != 0 {
        return rc;
    }
    // Legacy return contract: a hint for the next read size.
    BLOCK_SIZE_MAX
}

/// `size_t ZSTD_flushStream(ZSTD_CStream* zcs, ZSTD_outBuffer* output)` —
/// `ZSTD_compressStream2(zcs, output, &empty, ZSTD_e_flush)`.
///
/// # Safety
/// Same contracts as [`ZSTD_compressStream2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_flushStream(
    zcs: *mut ZSTD_CCtx,
    output: *mut ZSTD_outBuffer,
) -> usize {
    let mut empty = ZSTD_inBuffer {
        src: core::ptr::null(),
        size: 0,
        pos: 0,
    };
    unsafe { ZSTD_compressStream2(zcs, output, &mut empty, ZSTD_E_FLUSH) }
}

/// `size_t ZSTD_endStream(ZSTD_CStream* zcs, ZSTD_outBuffer* output)` —
/// `ZSTD_compressStream2(zcs, output, &empty, ZSTD_e_end)`.
///
/// # Safety
/// Same contracts as [`ZSTD_compressStream2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_endStream(zcs: *mut ZSTD_CCtx, output: *mut ZSTD_outBuffer) -> usize {
    let mut empty = ZSTD_inBuffer {
        src: core::ptr::null(),
        size: 0,
        pos: 0,
    };
    unsafe { ZSTD_compressStream2(zcs, output, &mut empty, ZSTD_E_END) }
}

/// `ZSTD_DStream* ZSTD_createDStream(void)` — same object as a `ZSTD_DCtx`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createDStream() -> *mut ZSTD_DCtx {
    crate::context::ZSTD_createDCtx()
}

/// `size_t ZSTD_freeDStream(ZSTD_DStream* zds)` — `NULL` is a no-op.
///
/// # Safety
/// `zds` must be a live pointer from `ZSTD_createDStream` /
/// `ZSTD_createDCtx`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeDStream(zds: *mut ZSTD_DCtx) -> usize {
    unsafe { crate::context::ZSTD_freeDCtx(zds) }
}

/// `size_t ZSTD_sizeof_DStream(const ZSTD_DStream* zds)` — `ZSTD_DStream`
/// is the same object as a `ZSTD_DCtx`, so this is `ZSTD_sizeof_DCtx`.
///
/// # Safety
/// Same as [`ZSTD_freeDStream`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_DStream(zds: *const ZSTD_DCtx) -> usize {
    unsafe { crate::context::ZSTD_sizeof_DCtx(zds) }
}

/// `size_t ZSTD_DStreamInSize(void)` — recommended input granule
/// (`ZSTD_BLOCKSIZE_MAX + blockHeader(3)`).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_DStreamInSize() -> usize {
    BLOCK_SIZE_MAX + 3
}

/// `size_t ZSTD_DStreamOutSize(void)` — recommended output granule
/// (`ZSTD_BLOCKSIZE_MAX`).
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_DStreamOutSize() -> usize {
    BLOCK_SIZE_MAX
}

/// The first read a fresh decoding session asks for: enough to learn the
/// frame header's length (magic number and descriptor, upstream
/// `ZSTD_startingInputLength`).
fn starting_input_length(dctx: &ZSTD_DCtx) -> usize {
    if dctx.magicless { 1 } else { 5 }
}

/// `size_t ZSTD_initDStream(ZSTD_DStream* zds)` — start a decoding session
/// without a dictionary; returns the first read size.
///
/// # Safety
/// `zds` must be a live pointer from `ZSTD_createDStream`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initDStream(zds: *mut ZSTD_DCtx) -> usize {
    unsafe { ZSTD_initDStream_usingDDict(zds, core::ptr::null()) }
}

/// `size_t ZSTD_initDStream_usingDict(ZSTD_DStream* zds, const void* dict,
/// size_t dictSize)` — start a decoding session with `dict` loaded (none when
/// empty); returns the first read size.
///
/// # Safety
/// `zds` must be live or `NULL`; `dict` valid for `dict_size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initDStream_usingDict(
    zds: *mut ZSTD_DCtx,
    dict: *const u8,
    dict_size: usize,
) -> usize {
    let Some(dctx) = (unsafe { zds.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    dctx.reset_session();
    let rc = unsafe { crate::attach::ZSTD_DCtx_loadDictionary(zds, dict, dict_size) };
    if crate::error::result_is_error(rc) {
        return rc;
    }
    starting_input_length(unsafe { &*zds })
}

/// `size_t ZSTD_initDStream_usingDDict(ZSTD_DStream* zds, const ZSTD_DDict*
/// ddict)` — start a decoding session referencing `ddict` (none when `NULL`);
/// returns the first read size.
///
/// # Safety
/// `zds` must be live or `NULL`; `ddict` live or `NULL`, outliving the session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initDStream_usingDDict(
    zds: *mut ZSTD_DCtx,
    ddict: *const crate::cdict::ZSTD_DDict,
) -> usize {
    let Some(dctx) = (unsafe { zds.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    dctx.reset_session();
    let rc = unsafe { crate::attach::ZSTD_DCtx_refDDict(zds, ddict) };
    if crate::error::result_is_error(rc) {
        return rc;
    }
    starting_input_length(unsafe { &*zds })
}

/// `size_t ZSTD_resetDStream(ZSTD_DStream* zds)` — abandon any frame in
/// flight, keeping the dictionary and parameters; returns the first read
/// size.
///
/// # Safety
/// `zds` must be a live pointer from `ZSTD_createDStream`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_resetDStream(zds: *mut ZSTD_DCtx) -> usize {
    let Some(dctx) = (unsafe { zds.as_mut() }) else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    dctx.reset_session();
    starting_input_length(dctx)
}

/// `size_t ZSTD_decompressStream_simpleArgs(ZSTD_DCtx* dctx, void* dst,
/// size_t dstCapacity, size_t* dstPos, const void* src, size_t srcSize,
/// size_t* srcPos)` — [`ZSTD_decompressStream`] with the buffer structs
/// spelled out as arguments.
///
/// # Safety
/// As [`ZSTD_decompressStream`]; `dst_pos` / `src_pos` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressStream_simpleArgs(
    dctx: *mut ZSTD_DCtx,
    dst: *mut c_void,
    dst_capacity: usize,
    dst_pos: *mut usize,
    src: *const c_void,
    src_size: usize,
    src_pos: *mut usize,
) -> usize {
    if dst_pos.is_null() || src_pos.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let mut output = ZSTD_outBuffer {
        dst,
        size: dst_capacity,
        pos: unsafe { *dst_pos },
    };
    let mut input = ZSTD_inBuffer {
        src,
        size: src_size,
        pos: unsafe { *src_pos },
    };
    let rc = unsafe { ZSTD_decompressStream(dctx, &mut output, &mut input) };
    unsafe {
        *dst_pos = output.pos;
        *src_pos = input.pos;
    }
    rc
}

/// `size_t ZSTD_decompressStream(ZSTD_DStream* zds, ZSTD_outBuffer* output,
/// ZSTD_inBuffer* input)`.
///
/// Consumes from `input`, flushes decoded bytes into `output`, updating both
/// `pos` fields. Returns `0` once a frame is fully decoded AND flushed; a
/// non-zero non-error value means more input and/or output room is needed.
///
/// # Safety
/// `zds` must be live; `output` / `input` must point to valid buffer structs
/// whose `dst` / `src` are valid for their `size` fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressStream(
    zds: *mut ZSTD_DCtx,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
) -> usize {
    if zds.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *zds };
    let (out, inp, dst, src) = match unsafe { stream_buffers(output, input) } {
        Ok(v) => v,
        Err(code) => return encode(code),
    };
    // Frame boundary: validate the next header and start the frame
    // explicitly. `decode_from_to` self-initialises only a brand-new
    // decoder; a reused one parks on the finished previous frame and would
    // make no progress on the next frame's bytes (an infinite caller loop).
    if dctx.stream_frame_done {
        if inp.pos == inp.size {
            // No active frame and nothing to read: frame-done signal.
            return 0;
        }
        use codec::decoding::errors::ReadFrameHeaderError;
        match codec::decoding::read_frame_header_info(&src[inp.pos..], dctx.magicless) {
            Ok(_) => {
                // The window ceiling is the decoder's own; it refuses a wider
                // window when the header is read below. Set on every frame
                // start, since the decoder may have been replaced since the
                // parameter was.
                if let Err(err) = dctx.decoder.set_max_window_size(dctx.max_window_size) {
                    return encode(code_for_decoder_error(&err));
                }
                let mut reader = &src[inp.pos..];
                // Verify the trailing content checksum like
                // ZSTD_decompressDCtx: the decoder defaults to EmitOnly, so
                // without this a corrupted trailer would decode silently.
                // Idempotent, safe to reapply on every frame start.
                dctx.decoder
                    .set_content_checksum(codec::decoding::ContentChecksum::Verify);
                // Context-attached dictionary (`ZSTD_DCtx_loadDictionary` /
                // `refDDict` / `refPrefix`): start the frame against it; an
                // attached prefix is single-use and consumed by this frame.
                let decoder = &mut dctx.decoder;
                let attached = dctx.attached_ddict.handle();
                let reset = catch_unwind(AssertUnwindSafe(|| match attached {
                    Some(handle) => decoder.reset_with_dict_handle(&mut reader, handle),
                    None => decoder.reset(&mut reader),
                }));
                match reset {
                    Ok(Ok(())) => {
                        inp.pos = inp.size - reader.len();
                        dctx.stream_frame_done = false;
                        dctx.consume_prefix();
                    }
                    Ok(Err(err)) => return encode(code_for_decoder_error(&err)),
                    Err(_) => {
                        dctx.replace_decoder();
                        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
                    }
                }
            }
            // Skippable frame: consume it whole when fully buffered (magic 4
            // + length 4 + payload), else ask for more input.
            Err(ReadFrameHeaderError::SkipFrame { length, .. }) => {
                // `checked_add`: on 32-bit targets a near-u32::MAX declared
                // length overflows the byte count; such a frame can never be
                // fully buffered there, so it stays a need-more-input hint
                // instead of wrapping `pos` past valid data.
                let Some(total) = 8usize.checked_add(length as usize) else {
                    return 1;
                };
                if inp.size - inp.pos < total {
                    return 1;
                }
                inp.pos += total;
                // Stay at a frame boundary; the caller's loop re-enters for
                // whatever follows.
                return 1;
            }
            // A truncated header (the reader hit end-of-input mid-field):
            // consume nothing and ask for more input.
            Err(
                ReadFrameHeaderError::MagicNumberReadError(_)
                | ReadFrameHeaderError::FrameDescriptorReadError(_)
                | ReadFrameHeaderError::WindowDescriptorReadError(_)
                | ReadFrameHeaderError::DictionaryIdReadError(_)
                | ReadFrameHeaderError::FrameContentSizeReadError(_),
            ) => return 1,
            // A malformed header (bad magic, invalid descriptor, ...) is a
            // decode error: the need-more-input hint with nothing consumed
            // would spin the caller forever.
            Err(err) => {
                return encode(code_for_decoder_error(
                    &codec::decoding::errors::FrameDecoderError::ReadFrameHeaderError(err),
                ));
            }
        }
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        dctx.decoder
            .decode_from_to(&src[inp.pos..], &mut dst[out.pos..])
    }));
    match outcome {
        Ok(Ok((read, written))) => {
            inp.pos += read;
            out.pos += written;
            let finished = dctx.decoder.is_finished() && dctx.decoder.can_collect() == 0;
            dctx.stream_frame_done = finished;
            if finished {
                0
            } else {
                // Hint: more input (or output room) is required.
                1
            }
        }
        Ok(Err(err)) => encode(code_for_decoder_error(&err)),
        Err(_) => {
            dctx.replace_decoder();
            dctx.stream_frame_done = true;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}
