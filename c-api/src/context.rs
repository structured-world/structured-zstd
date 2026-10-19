//! Synchronous context API: `ZSTD_CCtx` / `ZSTD_DCtx` create / free / one-shot
//! compress / decompress / sizeof.
//!
//! The contexts are opaque heap allocations holding reusable scratch (the
//! compressor's output buffer; the decoder instance) so repeated one-shot
//! calls reuse allocations — the reason a caller holds a context rather than
//! calling the simple API. Advanced parameter setters land in Phase 6.2.

use core::ffi::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::{ContentChecksum, FrameDecoder};
use codec::encoding::{
    CompressionLevel, CompressionParameters, EncoderDictionary, FrameCompressor,
};

use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::ffi::{in_slice, out_slice};

/// Heap-allocate `value` fallibly, returning a raw owning pointer or `null` on
/// allocation failure. Unlike `Box::new`, this never aborts the host process on
/// OOM, matching libzstd's `ZSTD_create*` NULL-on-failure contract. Pair every
/// non-null result with [`free_boxed`].
pub(crate) fn try_box<T>(value: T) -> *mut T {
    let layout = core::alloc::Layout::new::<T>();
    // Both context types own a `Vec` / `FrameDecoder`, so they are never
    // zero-sized and the allocator path always applies.
    debug_assert!(layout.size() != 0);
    // SAFETY: `layout` has non-zero size.
    let raw = unsafe { std::alloc::alloc(layout) } as *mut T;
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `raw` is freshly allocated for `T`'s layout and currently
    // uninitialised, so a plain write (no drop of prior contents) is correct.
    unsafe { raw.write(value) };
    raw
}

/// Drop and free a pointer previously returned by [`try_box`]. `null` is a no-op.
///
/// # Safety
/// `ptr` must be a live, not-yet-freed pointer from [`try_box::<T>`], or `null`.
pub(crate) unsafe fn free_boxed<T>(ptr: *mut T) {
    if ptr.is_null() {
        return;
    }
    let layout = core::alloc::Layout::new::<T>();
    // SAFETY: `ptr` came from `try_box::<T>` (same layout) and is still live.
    unsafe {
        core::ptr::drop_in_place(ptr);
        std::alloc::dealloc(ptr as *mut u8, layout);
    }
}

/// Bytes the current or last frame of a context has taken in, produced and
/// handed out; reset when a frame starts.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct FrameProgress {
    pub(crate) ingested: u64,
    pub(crate) produced: u64,
    pub(crate) flushed: u64,
}

impl FrameProgress {
    /// A one-shot frame: all of `src_len` taken, all of `frame_len` handed out.
    pub(crate) fn one_shot(src_len: usize, frame_len: usize) -> Self {
        Self {
            ingested: src_len as u64,
            produced: frame_len as u64,
            flushed: frame_len as u64,
        }
    }
}

/// Opaque compression context: what upstream keeps in a `ZSTD_CCtx`, kept
/// here the same way, from one call to the next.
///
/// The one-shot entry points (`ZSTD_compressCCtx`, `ZSTD_compress2`, the
/// `*_usingDict` / `*_usingCDict` family) share one [`FrameCompressor`] and the
/// streaming entry point one compression context, each with its match-finder
/// tables, buffers and dictionary snapshot, so a context compressing frame
/// after frame allocates them once. Each holds the dictionary last attached to
/// it, named by the dictionary's never-reused serial, so the same dictionary on
/// the next call is neither parsed nor attached again; a serial rather than an
/// address, so a freed-then-reallocated handle cannot alias.
#[allow(non_camel_case_types)]
pub struct ZSTD_CCtx {
    pub(crate) scratch: Vec<u8>,
    /// Sticky advanced parameters (`ZSTD_CCtx_setParameter` family).
    pub(crate) params: crate::cctx_params::CCtxParams,
    /// Streaming state (`ZSTD_compressStream2`), kept between frames; `None`
    /// before the first streamed frame.
    pub(crate) stream: Option<crate::streaming::CStreamState>,
    /// The one-shot compressor and the serial of the dictionary it holds
    /// (`0` = none); `None` before the first one-shot call.
    pub(crate) compressor: Option<(u64, FrameCompressor)>,
    /// How far the current or last frame got (`ZSTD_getFrameProgression`).
    pub(crate) progress: FrameProgress,
    /// The buffer-less frame begun by `ZSTD_compressBegin*`, compressed on
    /// the streaming context; `None` outside one.
    pub(crate) bufferless: Option<crate::bufferless::BufferlessFrame>,
    /// Context-attached dictionary (`ZSTD_CCtx_loadDictionary` /
    /// `ZSTD_CCtx_refCDict` / `ZSTD_CCtx_refPrefix`), applied at every frame
    /// start by `ZSTD_compress2` / `ZSTD_compressStream2`.
    pub(crate) attached_dict: crate::attach::CCtxDictAttach,
}

/// The one-shot compressor `slot` keeps, holding the dictionary `serial`
/// names: `dictionary`, or none for serial `0`. Created on first use; kept as
/// it is when it already holds that dictionary, otherwise given it.
pub(crate) fn kept_compressor<'a>(
    slot: &'a mut Option<(u64, FrameCompressor)>,
    serial: u64,
    dictionary: Option<&EncoderDictionary>,
    level: c_int,
) -> Result<&'a mut FrameCompressor, ZSTD_ErrorCode> {
    let (held, compressor) =
        slot.get_or_insert_with(|| (0, FrameCompressor::new(CompressionLevel::from_level(level))));
    if *held != serial {
        match dictionary {
            Some(dictionary) => {
                compressor
                    .set_encoder_dictionary(dictionary.clone())
                    .map_err(|_| ZSTD_ErrorCode::ZSTD_error_dictionary_corrupted)?;
            }
            None => {
                compressor.clear_dictionary();
            }
        }
        *held = serial;
    }
    Ok(compressor)
}

/// Put `compressor` on what a frame runs under: its explicit `params`, or
/// `level` with the level's own tuning.
pub(crate) fn set_frame_parameters(
    compressor: &mut FrameCompressor,
    params: Option<&CompressionParameters>,
    level: c_int,
) {
    match params {
        Some(params) => compressor.set_parameters(params),
        None => {
            compressor.set_compression_level(CompressionLevel::from_level(level));
        }
    }
}

/// Put the block limits, the magic number and the matchers back to a plain
/// frame's: the compressor is shared with `ZSTD_compress2`, whose sticky
/// parameters may have set them, and the one-shot entry points do not read
/// those (upstream runs them on `simpleApiParams`, without a sequence
/// producer).
pub(crate) fn plain_frame_shape(compressor: &mut FrameCompressor) {
    compressor.set_target_block_size(None);
    compressor.set_max_block_size(None);
    compressor.set_magicless(false);
    compressor.set_sequence_producer(None, false);
}

/// Opaque decompression context. Wraps a reusable [`FrameDecoder`] so its
/// internal buffers persist across `ZSTD_decompressDCtx` calls.
#[allow(non_camel_case_types)]
pub struct ZSTD_DCtx {
    pub(crate) decoder: FrameDecoder,
    /// Streaming window-size acceptance ceiling in bytes, set as a log by
    /// `ZSTD_d_windowLogMax` or exactly by `ZSTD_DCtx_setMaxWindowSize`.
    /// Defaults to `1 << ZSTD_WINDOWLOG_LIMIT_DEFAULT`.
    pub(crate) max_window_size: u64,
    /// `ZSTD_d_format`: frames carry no magic number. Kept here as well as in
    /// `decoder` so a replacement decoder starts with it.
    pub(crate) magicless: bool,
    /// How every decode path takes a frame's checksum: verified, or with
    /// `ZSTD_d_forceIgnoreChecksum` not computed at all.
    pub(crate) content_checksum: ContentChecksum,
    /// `ZSTD_d_refMultipleDDicts`: `ZSTD_DCtx_refDDict` adds to the DDicts
    /// a frame may pick from by its dictionary ID, rather than replacing.
    pub(crate) ref_multiple_ddicts: bool,
    /// `ZSTD_d_maxBlockSize`; 0 = the format's 128 KiB.
    pub(crate) max_block_size: c_int,
    /// `ZSTD_d_stableOutBuffer` and `ZSTD_d_disableHuffmanAssembly`: promises
    /// and switches about upstream's own buffering and assembly, stored so
    /// they read back. This decoder keeps its window either way and has no
    /// assembly kernels, so neither changes what a decode produces.
    pub(crate) stable_out_buffer: bool,
    pub(crate) disable_huffman_assembly: bool,
    /// Where the buffer-less decode (`ZSTD_decompressBegin*` /
    /// `ZSTD_decompressContinue`) stands.
    pub(crate) bufferless: crate::bufferless::BufferlessDecode,
    /// Whether the streaming decoder sits at a frame boundary (the next
    /// `ZSTD_decompressStream` input starts a new frame). `true` initially.
    pub(crate) stream_frame_done: bool,
    /// How far `ZSTD_decompressStream` has got into the frame it is entering,
    /// read only while `stream_frame_done` is `false`: a boundary leaves it
    /// behind, whatever set the boundary.
    pub(crate) stream_entry: crate::streaming::StreamEntry,
    /// The part of the frame's next block (or of its checksum) that the input
    /// has brought so far, held until the rest arrives; at most one block.
    /// Read only inside a frame, and emptied as each frame starts.
    pub(crate) stream_staged: Vec<u8>,
    /// Consecutive `ZSTD_decompressStream` calls inside a frame that moved
    /// neither position; see [`crate::streaming::NO_FORWARD_PROGRESS_MAX`].
    pub(crate) stream_no_progress: u32,
    /// Whether `ZSTD_decompressStream` holds back the frame's last input byte
    /// because the frame is decoded but its output not all handed out yet.
    pub(crate) stream_hostage: bool,
    /// Context-attached dictionary (`ZSTD_DCtx_loadDictionary` /
    /// `ZSTD_DCtx_refDDict` / `ZSTD_DCtx_refPrefix`), applied at every frame
    /// start by `ZSTD_decompressDCtx` / `ZSTD_decompressStream`.
    pub(crate) attached_ddict: crate::attach::DCtxDictAttach,
}

/// `ZSTD_CCtx* ZSTD_createCCtx(void)`. Returns `NULL` on allocation failure
/// (never aborts), matching upstream.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCCtx() -> *mut ZSTD_CCtx {
    try_box(ZSTD_CCtx {
        scratch: Vec::new(),
        params: crate::cctx_params::CCtxParams::default(),
        stream: None,
        compressor: None,
        progress: FrameProgress::default(),
        bufferless: None,
        attached_dict: crate::attach::CCtxDictAttach::None,
    })
}

/// `ZSTD_CCtx* ZSTD_createCCtx_advanced(ZSTD_customMem customMem)` —
/// [`ZSTD_createCCtx`]; custom allocators are not supported, so anything but
/// `ZSTD_defaultCMem` returns `NULL`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCCtx_advanced(
    custom_mem: crate::cdict::ZSTD_customMem,
) -> *mut ZSTD_CCtx {
    if !custom_mem.is_default() {
        return core::ptr::null_mut();
    }
    ZSTD_createCCtx()
}

/// The most a context keeps of its one-shot frame buffer between calls: a
/// frame of one full block, its header and checksum.
pub(crate) const RETAINED_SCRATCH: usize =
    codec::encoding::compress_bound(codec::MAX_BLOCK_SIZE as usize);

impl ZSTD_CCtx {
    /// Hand the one-shot frame in `scratch` to `dst`, or `dstSize_tooSmall`,
    /// then let go of a buffer larger than a block's worth: upstream writes the
    /// frame into `dst` and keeps none of it, so a context keeps only the room
    /// that spares a small frame an allocation per call.
    pub(crate) fn hand_out_frame(&mut self, dst: &mut [u8]) -> Result<usize, ZSTD_ErrorCode> {
        let len = self.scratch.len();
        let handed = if len > dst.len() {
            Err(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall)
        } else {
            dst[..len].copy_from_slice(&self.scratch);
            Ok(len)
        };
        if self.scratch.capacity() > RETAINED_SCRATCH {
            self.scratch = Vec::new();
        } else {
            self.scratch.clear();
        }
        handed
    }

    /// Whether a streaming frame is currently mid-flight (parameters are
    /// frozen until it finishes or the session is reset).
    pub(crate) fn stream_in_progress(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(crate::streaming::CStreamState::in_frame)
    }

    /// `ZSTD_reset_session_only`: abandon any in-flight frame and the
    /// pledged size; sticky parameters and dictionary references survive,
    /// and so does the streaming context unless a frame was abandoned in it.
    pub(crate) fn reset_session(&mut self) {
        self.scratch.clear();
        if self.stream_in_progress() {
            self.stream = None;
        }
        self.bufferless = None;
        self.params.pledged_src_size = crate::params::CONTENTSIZE_UNKNOWN;
    }

    /// Start a one-shot frame: it abandons a streamed or buffer-less frame
    /// left open, with its pledge, and the real size of the input it has in
    /// hand drives the header. Upstream begins every one-shot from a fresh
    /// session: `ZSTD_compress2` resets it (zstd_compress.c:6575, and
    /// `ZSTD_generateSequences` runs through it), the simple calls through
    /// `ZSTD_compressBegin_internal`, and `ZSTD_compressSequences` re-inits the
    /// stream with the input's size as its pledge (`ZSTD_CCtx_init_compressStream2`).
    pub(crate) fn begin_one_shot(&mut self) {
        self.reset_session();
    }

    /// `ZSTD_reset_parameters`: restore parameter defaults and drop
    /// dictionary references, including the ones the kept compressors hold.
    pub(crate) fn reset_parameters(&mut self) {
        self.params = crate::cctx_params::CCtxParams::default();
        self.replace_attached_dict(crate::attach::CCtxDictAttach::None);
        // A `*_usingCDict` call's dictionary is not an attachment, and goes too.
        if let Some((held, compressor)) = &mut self.compressor
            && *held != 0
        {
            compressor.clear_dictionary();
            *held = 0;
        }
    }
}

/// `size_t ZSTD_freeCCtx(ZSTD_CCtx* cctx)` — frees the context; `NULL` is a
/// no-op (returns 0), matching upstream.
///
/// # Safety
/// `cctx` must be a pointer returned by [`ZSTD_createCCtx`] and not already
/// freed, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeCCtx(cctx: *mut ZSTD_CCtx) -> usize {
    unsafe { free_boxed(cctx) };
    0
}

/// `size_t ZSTD_sizeof_CCtx(const ZSTD_CCtx* cctx)` — current heap footprint,
/// or 0 for `NULL`.
///
/// Counts the inline struct, the reusable output `scratch`, and the kept
/// one-shot compressor and streaming context once they exist: their
/// match-finder tables / history, the recycled-buffer pool and the dictionary
/// snapshot. Matches upstream `ZSTD_sizeof_CCtx`, which includes the
/// CDict-copied working tables.
///
/// The dictionaries are shared rather than copied, so the attached one and
/// the ones the compressors hold may be a single allocation. Each counts
/// once, and only when the context alone keeps it alive: a referenced CDict's
/// is the CDict's, as upstream leaves referenced dictionaries out.
///
/// # Safety
/// `cctx` must be a live pointer from [`ZSTD_createCCtx`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_CCtx(cctx: *const ZSTD_CCtx) -> usize {
    if cctx.is_null() {
        return 0;
    }
    let cctx = unsafe { &*cctx };
    let compressor = cctx.compressor.as_ref().map(|(_, compressor)| compressor);
    let compressor_dictionary = compressor.and_then(FrameCompressor::dictionary);
    let stream_dictionary = cctx
        .stream
        .as_ref()
        .and_then(crate::streaming::CStreamState::dictionary);
    // Each holder's own size carries the dictionary it holds; that share is
    // taken back out and the dictionaries are counted once, below.
    let dictionary_size =
        |held: Option<&EncoderDictionary>| held.map_or(0, EncoderDictionary::heap_size);
    core::mem::size_of::<ZSTD_CCtx>()
        + cctx.scratch.capacity()
        + compressor.map_or(0, FrameCompressor::heap_size)
        - dictionary_size(compressor_dictionary)
        + cctx
            .stream
            .as_ref()
            .map_or(0, crate::streaming::CStreamState::heap_size)
        - dictionary_size(stream_dictionary)
        + EncoderDictionary::exclusive_heap_size(
            [
                cctx.attached_dict.owned(),
                compressor_dictionary,
                stream_dictionary,
                cctx.bufferless
                    .as_ref()
                    .and_then(crate::bufferless::BufferlessFrame::dictionary),
            ]
            .into_iter()
            .flatten(),
        )
}

/// `size_t ZSTD_compressCCtx(ZSTD_CCtx* cctx, void* dst, size_t dstCapacity,
/// const void* src, size_t srcSize, int compressionLevel)` — same result as
/// [`ZSTD_compress`](crate::simple::ZSTD_compress), reusing the context's
/// output buffer.
///
/// # Safety
/// `cctx` must be live; `dst`/`src` valid for their lengths (or `NULL`+0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compressCCtx(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    compression_level: c_int,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    cctx.begin_one_shot();
    let src = unsafe { in_slice(src, src_size) };
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), ZSTD_ErrorCode> {
        let ZSTD_CCtx {
            scratch,
            compressor,
            ..
        } = cctx;
        // Upstream `ZSTD_compressCCtx` is `ZSTD_compress_usingDict` with no
        // dictionary: whatever the context has attached is not used.
        let enc = kept_compressor(compressor, 0, None, compression_level)?;
        enc.set_compression_level(CompressionLevel::from_level(compression_level));
        // Upstream ZSTD_compressCCtx defaults ZSTD_c_checksumFlag = 0; match it.
        enc.set_content_checksum(false);
        enc.set_content_size_flag(true);
        plain_frame_shape(enc);
        scratch.clear();
        enc.compress_independent_frame_into(src, scratch);
        Ok(())
    }));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(code)) => return encode(code),
        Err(_) => {
            // A panic can leave the kept compressor torn; the next call
            // builds a new one.
            cctx.compressor = None;
            return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
        }
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let len = match cctx.hand_out_frame(dst) {
        Ok(len) => len,
        Err(code) => return encode(code),
    };
    cctx.progress = FrameProgress::one_shot(src.len(), len);
    len
}

/// Compress `src` as one frame under the sticky parameters and the attached
/// dictionary (the frame of `ZSTD_compress2`) into `cctx.scratch`, or, when
/// `capture` is given, only record the parse it takes there; returns the
/// frame's length either way.
pub(crate) fn compress_sticky_frame(
    cctx: &mut ZSTD_CCtx,
    src: &[u8],
    capture: Option<&mut Vec<codec::encoding::sequences::ExternalSequence>>,
) -> Result<usize, ZSTD_ErrorCode> {
    cctx.begin_one_shot();
    // A referenced CDict's compression parameters are authoritative
    // (upstream rule), so the sticky knobs are resolved — and can reject —
    // only when they will actually drive the frame; an unsupported sticky
    // combination must not break a valid RefCDict path.
    // All the input is here, so the frame's size is known and decides whether a
    // referenced CDict's parameters still describe it.
    let src_size = Some(src.len() as u64);
    let frame_params = cctx.frame_parameters(src_size)?;
    let params = cctx.params;
    let level = cctx.attach_level(src_size);
    let serial = cctx.attach_serial();
    let suppress_id = cctx.attach_suppresses_dict_id();
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<usize, ZSTD_ErrorCode> {
        let ZSTD_CCtx {
            scratch,
            compressor,
            attached_dict,
            ..
        } = cctx;
        // The kept compressor, holding the attached dictionary: the same one
        // on the next call is neither parsed nor attached again.
        let enc = kept_compressor(compressor, serial, attached_dict.prepared(), level)?;
        set_frame_parameters(enc, frame_params.as_ref(), level);
        enc.set_content_checksum(params.checksum_flag);
        enc.set_content_size_flag(params.content_size_flag);
        enc.set_dictionary_id_flag(params.dict_id_flag && !suppress_id);
        // The kept compressor outlives parameter changes: clear the cap
        // explicitly when the sticky knob is back at 0 (auto), or a cap set
        // by an earlier call would silently survive.
        enc.set_target_block_size(params.target_block_size());
        enc.set_max_block_size(params.max_block_size());
        enc.set_magicless(params.magicless);
        // An attached dictionary stays attached next to a producer: the frame
        // names it and its entropy seeds the first block, as upstream 1.5.7
        // writes the dictionary ID in that case too. The producer itself sees
        // no history, which is what "no effect" means in the zstd.h notes.
        // Long-distance matching stays on beside it, see `CStreamState`'s
        // `configure`.
        let (producer, fallback) = params.sequence_producer(level);
        enc.set_sequence_producer(producer, fallback);
        enc.set_sequence_capture(capture.is_some());
        // A parse is all a capturing caller hands out, so its frame goes to a
        // buffer freed on return rather than into the kept scratch, as
        // upstream's generateSequences frees its dst (zstd_compress.c:3548).
        let mut transient = Vec::new();
        let out = if capture.is_some() {
            &mut transient
        } else {
            scratch
        };
        out.clear();
        enc.compress_independent_frame_into(src, out);
        let frame_len = out.len();
        if let Some(capture) = capture {
            *capture = enc.take_captured_sequences();
            enc.set_sequence_capture(false);
        }
        // Without fallback a producer failure fails the frame, as upstream's
        // `ZSTD_buildSeqStore` returns it (zstd_compress.c:3396).
        if enc.take_sequence_producer_failure() {
            return Err(ZSTD_ErrorCode::ZSTD_error_sequenceProducer_failed);
        }
        Ok(frame_len)
    }));
    match outcome {
        Ok(result) => result,
        Err(_) => {
            // A panic can leave the kept compressor torn; the next call
            // builds a new one.
            cctx.compressor = None;
            Err(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}

/// `size_t ZSTD_compress2(ZSTD_CCtx* cctx, void* dst, size_t dstCapacity,
/// const void* src, size_t srcSize)` — one-shot compression driven entirely
/// by the sticky advanced parameters (`ZSTD_CCtx_set*`); always starts a new
/// frame.
///
/// # Safety
/// `cctx` must be live; `dst`/`src` valid for their lengths (or `NULL`+0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compress2(
    cctx: *mut ZSTD_CCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    if cctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let cctx = unsafe { &mut *cctx };
    let src = unsafe { in_slice(src, src_size) };
    let built = compress_sticky_frame(cctx, src, None);
    // The single-use prefix is spent once the frame has started, whatever
    // happens to it after: upstream clears it in ZSTD_CCtx_init_compressStream2
    // (zstd_compress.c:6356), before compressing, so a caller that retries a
    // failed call (dstSize_tooSmall included) does so without it.
    cctx.consume_prefix();
    if let Err(code) = built {
        return encode(code);
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let len = match cctx.hand_out_frame(dst) {
        Ok(len) => len,
        Err(code) => return encode(code),
    };
    cctx.progress = FrameProgress::one_shot(src.len(), len);
    len
}

/// `ZSTD_DCtx* ZSTD_createDCtx(void)`. Returns `NULL` on allocation failure
/// (never aborts), matching upstream.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createDCtx() -> *mut ZSTD_DCtx {
    try_box(ZSTD_DCtx {
        decoder: FrameDecoder::new(),
        max_window_size: crate::params::MAX_WINDOW_SIZE_DEFAULT,
        magicless: false,
        content_checksum: ContentChecksum::Verify,
        ref_multiple_ddicts: false,
        max_block_size: 0,
        stable_out_buffer: false,
        disable_huffman_assembly: false,
        bufferless: crate::bufferless::BufferlessDecode::new(false),
        stream_frame_done: true,
        stream_entry: crate::streaming::StreamEntry::Entered,
        stream_staged: Vec::new(),
        stream_no_progress: 0,
        stream_hostage: false,
        attached_ddict: crate::attach::DCtxDictAttach::None,
    })
}

/// `ZSTD_DCtx* ZSTD_createDCtx_advanced(ZSTD_customMem customMem)` —
/// [`ZSTD_createDCtx`]; custom allocators are not supported, so anything but
/// `ZSTD_defaultCMem` returns `NULL`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createDCtx_advanced(
    custom_mem: crate::cdict::ZSTD_customMem,
) -> *mut ZSTD_DCtx {
    if !custom_mem.is_default() {
        return core::ptr::null_mut();
    }
    ZSTD_createDCtx()
}

impl ZSTD_DCtx {
    /// `ZSTD_reset_session_only`: abandon any in-flight frame. The decoder
    /// instance (and its reusable workspace) survives; loaded dictionaries
    /// and parameters are untouched.
    pub(crate) fn reset_session(&mut self) {
        // A frame mid-decode lives inside `decoder`'s internal state and is
        // re-initialised by the next `init`/`reset` call; just mark the
        // stream as sitting at a frame boundary again.
        self.stream_frame_done = true;
        self.stream_no_progress = 0;
        self.stream_hostage = false;
    }

    /// `ZSTD_reset_parameters`: restore parameter defaults and drop
    /// dictionary references. Upstream accepts it only at a stream's frame
    /// boundary and leaves a begun buffer-less run as it was begun
    /// (`ZSTD_DCtx_resetParameters` touches neither), so the decoder and the
    /// run stay; the new parameters apply from the next frame.
    pub(crate) fn reset_parameters(&mut self) {
        self.max_window_size = crate::params::MAX_WINDOW_SIZE_DEFAULT;
        self.magicless = false;
        self.decoder.set_magicless(false);
        self.content_checksum = ContentChecksum::Verify;
        self.ref_multiple_ddicts = false;
        self.max_block_size = 0;
        self.apply_max_block_size();
        self.stable_out_buffer = false;
        self.disable_huffman_assembly = false;
        self.attached_ddict = crate::attach::DCtxDictAttach::None;
    }

    /// Hand `ZSTD_d_maxBlockSize` to the decoder, which holds every frame it
    /// starts to it; 0 is the format's 128 KiB.
    pub(crate) fn apply_max_block_size(&mut self) {
        let limit = u32::try_from(self.max_block_size)
            .ok()
            .filter(|&size| size != 0);
        self.decoder.set_max_block_size(limit);
    }

    /// Start over with a fresh decoder, as after a panic that may have torn the
    /// old one: the frame format and block limit carry over, the loaded
    /// dictionaries do not.
    pub(crate) fn replace_decoder(&mut self) {
        self.decoder = FrameDecoder::new();
        self.decoder.set_magicless(self.magicless);
        self.apply_max_block_size();
    }
}

/// `size_t ZSTD_freeDCtx(ZSTD_DCtx* dctx)` — frees the context; `NULL` is a
/// no-op (returns 0).
///
/// # Safety
/// `dctx` must be a pointer from [`ZSTD_createDCtx`] and not already freed, or
/// `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeDCtx(dctx: *mut ZSTD_DCtx) -> usize {
    unsafe { free_boxed(dctx) };
    0
}

/// `size_t ZSTD_sizeof_DCtx(const ZSTD_DCtx* dctx)` — total footprint of the
/// context, or 0 for `NULL`.
///
/// Sums the inline struct size and the `FrameDecoder`'s lazily-grown workspace
/// (decode-window buffer, per-block literal/content buffers, entropy tables),
/// matching the workspace term of upstream's `ZSTD_sizeof_DCtx`. The workspace
/// is 0 until the first frame allocates it.
///
/// The dictionaries the context holds (the attached one, a buffer-less run's,
/// the one the decoder kept from its last frame) may be one allocation: each
/// counts once, and only when the context alone keeps it alive, so a
/// referenced DDict's is the DDict's, as upstream leaves `refDDict` memory out.
///
/// # Safety
/// `dctx` must be a live pointer from [`ZSTD_createDCtx`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_DCtx(dctx: *const ZSTD_DCtx) -> usize {
    if dctx.is_null() {
        return 0;
    }
    let dctx = unsafe { &*dctx };
    core::mem::size_of::<ZSTD_DCtx>()
        + dctx.decoder.workspace_size()
        + dctx.stream_staged.capacity()
        + codec::decoding::DictionaryHandle::exclusive_heap_size(
            dctx.attached_ddict
                .handles()
                .iter()
                .chain(dctx.bufferless.dictionary())
                .chain(dctx.decoder.dictionary()),
        )
        + attached_set_size(&dctx.attached_ddict)
}

/// The list a multiple-DDict reference keeps of its handles, which upstream
/// counts too (its DDict hash set lives in the context).
fn attached_set_size(attached: &crate::attach::DCtxDictAttach) -> usize {
    match attached {
        crate::attach::DCtxDictAttach::RefDDicts { handles } => {
            handles.capacity() * core::mem::size_of::<codec::decoding::DictionaryHandle>()
        }
        _ => 0,
    }
}

/// `size_t ZSTD_decompressDCtx(ZSTD_DCtx* dctx, void* dst, size_t dstCapacity,
/// const void* src, size_t srcSize)` — same result as
/// [`ZSTD_decompress`](crate::simple::ZSTD_decompress), reusing the context's
/// decoder.
///
/// # Safety
/// `dctx` must be live; `dst`/`src` valid for their lengths (or `NULL`+0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressDCtx(
    dctx: *mut ZSTD_DCtx,
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
) -> usize {
    if dctx.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let dctx = unsafe { &mut *dctx };
    let src = match unsafe { crate::ffi::decode_input(src, src_size, dst, dst_capacity) } {
        Ok(src) => src,
        Err(code) => return encode(code),
    };
    let src = &*src;
    let dst = unsafe { out_slice(dst, dst_capacity) };
    // Verify the trailing content checksum like upstream ZSTD_decompress; the
    // setter is idempotent, so reapplying it on a reused DCtx is fine.
    //
    // No explicit decoder restart is needed before this one-shot:
    // `decode_all` re-initializes per frame (it re-parses the header and
    // resets the frame state, entropy tables, offset history, and the
    // scratch backend kind at every frame start), so state left by an
    // abandoned mid-frame streaming decode cannot leak in. Covered by
    // the decompress_stream_recovers_after_oneshot_on_midframe_context
    // regression test.
    //
    // `ZSTD_d_windowLogMax` is deliberately NOT enforced here: upstream
    // documents it as a streaming-API memory cap ("does not apply for
    // one-pass decoders ... since no additional memory is allocated"), and
    // its one-shot path checks only the absolute ZSTD_WINDOWLOG_MAX bound.
    // The single-pass decode writes into the caller's dst with no window
    // buffer, so there is no allocation for the parameter to limit.
    dctx.decoder.set_content_checksum(dctx.content_checksum);
    // Context-attached dictionary (`ZSTD_DCtx_loadDictionary` / `refDDict` /
    // `refPrefix`) applies to the frames of this one-shot. A prefix is
    // single-use, but "use" means a frame actually started with it — the
    // streaming path consumes it only after a successful reset, so the
    // one-shot consumes it only on success (a garbage input must not eat
    // the prefix out from under the retry).
    let decoder = &mut dctx.decoder;
    let attached = dctx.attached_ddict.handles();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        decoder.decode_all_with_dict_handles(src, dst, attached)
    }));
    match outcome {
        Ok(Ok(written)) => {
            dctx.consume_prefix();
            // The one-shot consumed whole frames; the context sits at a
            // frame boundary again. Without this, a context abandoned
            // mid-stream earlier would make the next
            // ZSTD_decompressStream call report the stale frame complete
            // instead of starting the new one.
            dctx.stream_frame_done = true;
            written
        }
        Ok(Err(err)) => {
            // An ordinary decode error leaves the decoder's state coherent
            // (the next one-shot re-initializes it per frame), but the
            // context must still sit at a frame boundary for the STREAMING
            // entry point — without this, a context abandoned mid-stream
            // would resume the failed one-shot's frame state on the next
            // ZSTD_decompressStream call. The decoder itself is kept: its
            // warm workspace survives routine corrupt-input failures, the
            // full replacement is reserved for the panic arm where the
            // state may be torn mid-unwind.
            dctx.stream_frame_done = true;
            encode(code_for_decoder_error(&err))
        }
        Err(_) => {
            // A panic mid-decode can leave the decoder's internal state
            // partially consumed; replace it with a fresh one (same as
            // ZSTD_createDCtx) so a later call on this reused DCtx starts clean
            // instead of observing a broken invariant.
            dctx.replace_decoder();
            dctx.stream_frame_done = true;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}
