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

use codec::encoding::{CompressionLevel, StreamingEncoder};

use crate::context::{ZSTD_CCtx, ZSTD_DCtx};
use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::params::CONTENTSIZE_UNKNOWN;

/// `ZSTD_BLOCKSIZE_MAX`: 128 KiB, the recommended streaming input granule.
const BLOCK_SIZE_MAX: usize = 128 * 1024;

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

/// Streaming-compression state stored on the `ZSTD_CCtx` between
/// `ZSTD_compressStream2` calls.
pub(crate) struct CStreamState {
    /// Push-side encoder writing compressed bytes into its owned `Vec`
    /// drain. `None` after `ZSTD_e_end` finished the frame (the epilogue
    /// then lives in `pending`).
    encoder: Option<StreamingEncoder<Vec<u8>>>,
    /// Frame bytes produced but not yet copied out to the caller (the
    /// finished-frame tail after `ZSTD_e_end`, or `None`-encoder leftovers).
    pending: Vec<u8>,
    /// Read offset into `pending`.
    pending_pos: usize,
}

impl CStreamState {
    fn pending_remaining(&self) -> usize {
        self.pending.len() - self.pending_pos
    }

    /// Copy as much produced output as fits into `out`, draining the
    /// encoder's accumulated drain bytes first into `pending`.
    fn copy_out(&mut self, out: &mut ZSTD_outBuffer, dst: &mut [u8]) {
        if let Some(enc) = self.encoder.as_mut() {
            let drain = enc.get_mut();
            if !drain.is_empty() {
                if self.pending_pos == self.pending.len() {
                    self.pending.clear();
                    self.pending_pos = 0;
                }
                self.pending.extend_from_slice(drain);
                drain.clear();
            }
        }
        let n = self.pending_remaining().min(dst.len() - out.pos);
        dst[out.pos..out.pos + n]
            .copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
        self.pending_pos += n;
        out.pos += n;
        if self.pending_pos == self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }
    }
}

impl ZSTD_CCtx {
    /// Lazily start a streaming frame from the sticky parameters. No-op if
    /// one is already in flight.
    fn ensure_stream(&mut self) -> Result<(), ZSTD_ErrorCode> {
        if self.stream.is_some() {
            return Ok(());
        }
        let params = self.params;
        let Some(resolved) = params.resolve() else {
            return Err(ZSTD_ErrorCode::ZSTD_error_parameter_combination_unsupported);
        };
        let mut enc = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(params.level));
        let mut setup = || -> Result<(), codec::io::Error> {
            enc.set_parameters(&resolved)?;
            enc.set_content_checksum(params.checksum_flag)?;
            if params.target_cblock_size > 0 {
                enc.set_target_block_size(Some(params.target_cblock_size as u32))?;
            }
            // The pledge is single-use (consumed by this frame). It is
            // always enforced against the bytes actually written; the
            // content-size flag only controls whether the header carries
            // the FCS field (upstream validates the pledge at frame end
            // regardless of the flag).
            if params.pledged_src_size != CONTENTSIZE_UNKNOWN {
                enc.set_pledged_content_size(params.pledged_src_size)?;
            }
            enc.set_content_size_flag(params.content_size_flag)?;
            Ok(())
        };
        if setup().is_err() {
            return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
        self.params.pledged_src_size = CONTENTSIZE_UNKNOWN;
        self.stream = Some(CStreamState {
            encoder: Some(enc),
            pending: Vec::new(),
            pending_pos: 0,
        });
        Ok(())
    }
}

/// `ZSTD_CStream* ZSTD_createCStream(void)` — same object as a `ZSTD_CCtx`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCStream() -> *mut ZSTD_CCtx {
    crate::context::ZSTD_createCCtx()
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
    if let Err(code) = cctx.ensure_stream() {
        return encode(code);
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<usize, ZSTD_ErrorCode> {
        let stream = cctx.stream.as_mut().expect("ensure_stream installed state");
        // Consume ALL remaining input: the in-memory drain has no
        // backpressure, so `Write` never short-counts.
        if inp.pos < inp.size {
            let Some(enc) = stream.encoder.as_mut() else {
                // Frame already ended but not fully flushed: only flush/end
                // directives are legal (upstream stage_wrong).
                return Err(ZSTD_ErrorCode::ZSTD_error_stage_wrong);
            };
            use std::io::Write;
            // Loop over partial writes instead of `write_all`: the encoder
            // legally short-counts at the pledged-size boundary (accepts the
            // remaining allowance, then errors on the next call), and
            // `write_all` would discard that partial progress — leaving
            // `inp.pos` claiming nothing was consumed when most of it was.
            while inp.pos < inp.size {
                match enc.write(&src[inp.pos..]) {
                    Ok(0) => return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC),
                    Ok(n) => inp.pos += n,
                    // The encoder reports pledge violations as InvalidInput
                    // (the only InvalidInput reachable from `write` on the
                    // in-memory drain); upstream's error for input past the
                    // pledged size is srcSize_wrong.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                        return Err(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong);
                    }
                    Err(_) => return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC),
                }
            }
        }
        match end_op {
            ZSTD_E_FLUSH => {
                if let Some(enc) = stream.encoder.as_mut() {
                    use std::io::Write;
                    if enc.flush().is_err() {
                        return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC);
                    }
                }
            }
            ZSTD_E_END => {
                if let Some(enc) = stream.encoder.take() {
                    match enc.finish() {
                        Ok(drain) => {
                            if stream.pending_pos == stream.pending.len() {
                                stream.pending.clear();
                                stream.pending_pos = 0;
                            }
                            stream.pending.extend_from_slice(&drain);
                        }
                        Err(_) => return Err(ZSTD_ErrorCode::ZSTD_error_GENERIC),
                    }
                }
            }
            _ => {}
        }
        stream.copy_out(out, dst);
        let remaining = stream.pending_remaining();
        if end_op == ZSTD_E_END && remaining == 0 {
            // Frame complete and fully flushed: drop the stream state so the
            // next call starts a new frame from the sticky parameters.
            cctx.stream = None;
        }
        Ok(remaining)
    }));
    match outcome {
        Ok(Ok(remaining)) => remaining,
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
    // Legacy init clears any previously loaded dictionary.
    cctx.dict_compressor = None;
    cctx.dict_serial = 0;
    cctx.dict_level = 0;
    cctx.params.level = if compression_level == 0 {
        3
    } else {
        compression_level
    };
    0
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

/// `size_t ZSTD_initDStream(ZSTD_DStream* zds)` — legacy session reset;
/// returns the recommended first input size.
///
/// # Safety
/// `zds` must be a live pointer from `ZSTD_createDStream`, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_initDStream(zds: *mut ZSTD_DCtx) -> usize {
    if zds.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *zds };
    dctx.reset_session();
    ZSTD_DStreamInSize()
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
        match codec::decoding::read_frame_header_info(&src[inp.pos..], false) {
            Ok(info) => {
                // `ZSTD_d_windowLogMax`: refuse oversized windows before any
                // decode work runs.
                let limit = 1u64 << dctx.window_log_max;
                if info.window_size > limit {
                    return encode(ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge);
                }
                let mut reader = &src[inp.pos..];
                // Verify the trailing content checksum like
                // ZSTD_decompressDCtx: the decoder defaults to EmitOnly, so
                // without this a corrupted trailer would decode silently.
                // Idempotent, safe to reapply on every frame start.
                dctx.decoder
                    .set_content_checksum(codec::decoding::ContentChecksum::Verify);
                let reset = catch_unwind(AssertUnwindSafe(|| dctx.decoder.reset(&mut reader)));
                match reset {
                    Ok(Ok(())) => {
                        inp.pos = inp.size - reader.len();
                        dctx.stream_frame_done = false;
                    }
                    Ok(Err(err)) => return encode(code_for_decoder_error(&err)),
                    Err(_) => {
                        dctx.decoder = codec::decoding::FrameDecoder::new();
                        dctx.ddict_serial = 0;
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
            dctx.decoder = codec::decoding::FrameDecoder::new();
            dctx.ddict_serial = 0;
            dctx.stream_frame_done = true;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}
