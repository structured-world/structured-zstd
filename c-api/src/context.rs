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
use codec::encoding::{CompressionLevel, FrameCompressor};

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

/// Opaque compression context. Carries a reusable output buffer so repeated
/// `ZSTD_compressCCtx` calls amortise the destination allocation.
///
/// For the dictionary path ([`ZSTD_compress_usingCDict`](crate::cdict::ZSTD_compress_usingCDict))
/// it also caches a [`FrameCompressor`] with the dictionary already attached,
/// keyed by the `ZSTD_CDict`'s never-reused serial + level, so back-to-back
/// compressions with the same `CDict` reuse the parsed dictionary + primed
/// match-finder snapshot instead of re-parsing and re-priming each call (the
/// encoder-side analogue of upstream's `ZSTD_CCtx_refCDict` reuse).
#[allow(non_camel_case_types)]
pub struct ZSTD_CCtx {
    pub(crate) scratch: Vec<u8>,
    /// `FrameCompressor` with a dictionary attached, lazily built by the CDict
    /// path. `None` until the first `ZSTD_compress_usingCDict`.
    pub(crate) dict_compressor: Option<FrameCompressor>,
    /// Identity of the `CDict` currently attached to `dict_compressor` (its
    /// never-reused serial; `0` = none) plus the level it was built at, so a
    /// different CDict or level rebuilds the cached compressor. Keyed by serial
    /// rather than raw address so a freed-then-realloc'd handle can't alias.
    pub(crate) dict_serial: u64,
    pub(crate) dict_level: c_int,
}

/// Opaque decompression context. Wraps a reusable [`FrameDecoder`] so its
/// internal buffers persist across `ZSTD_decompressDCtx` calls.
#[allow(non_camel_case_types)]
pub struct ZSTD_DCtx {
    pub(crate) decoder: FrameDecoder,
    /// Identity of the `DDict` whose content was last loaded into `decoder`
    /// (its never-reused serial; `0` = none), so repeated
    /// `ZSTD_decompress_usingDDict` calls with the same DDict skip re-adding it.
    /// Reset to `0` whenever `decoder` is replaced, so a fresh decoder is never
    /// trusted to still hold the previously-loaded dictionary.
    pub(crate) ddict_serial: u64,
}

/// `ZSTD_CCtx* ZSTD_createCCtx(void)`. Returns `NULL` on allocation failure
/// (never aborts), matching upstream.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCCtx() -> *mut ZSTD_CCtx {
    try_box(ZSTD_CCtx {
        scratch: Vec::new(),
        dict_compressor: None,
        dict_serial: 0,
        dict_level: 0,
    })
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
/// Counts the inline struct and the reusable output `scratch`. The cached
/// `dict_compressor`'s primed match-finder tables and dictionary snapshot are
/// not yet summed (the encoder lacks a heap-size accessor); tracked in #388.
///
/// # Safety
/// `cctx` must be a live pointer from [`ZSTD_createCCtx`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_CCtx(cctx: *const ZSTD_CCtx) -> usize {
    if cctx.is_null() {
        return 0;
    }
    let cctx = unsafe { &*cctx };
    core::mem::size_of::<ZSTD_CCtx>() + cctx.scratch.capacity()
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
    let src = unsafe { in_slice(src, src_size) };
    let level = CompressionLevel::from_level(compression_level);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        cctx.scratch.clear();
        let mut enc: FrameCompressor = FrameCompressor::new(level);
        // Upstream ZSTD_compressCCtx defaults ZSTD_c_checksumFlag = 0; match it.
        enc.set_content_checksum(false);
        enc.compress_independent_frame_into(src, &mut cctx.scratch);
    }));
    if outcome.is_err() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    let len = cctx.scratch.len();
    if len > dst_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    dst[..len].copy_from_slice(&cctx.scratch);
    len
}

/// `ZSTD_DCtx* ZSTD_createDCtx(void)`. Returns `NULL` on allocation failure
/// (never aborts), matching upstream.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createDCtx() -> *mut ZSTD_DCtx {
    try_box(ZSTD_DCtx {
        decoder: FrameDecoder::new(),
        ddict_serial: 0,
    })
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
/// is 0 until the first frame allocates it. Shared dictionaries (ref-counted
/// handles) are not counted, as upstream excludes `refDDict` memory.
///
/// # Safety
/// `dctx` must be a live pointer from [`ZSTD_createDCtx`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_DCtx(dctx: *const ZSTD_DCtx) -> usize {
    if dctx.is_null() {
        return 0;
    }
    let dctx = unsafe { &*dctx };
    core::mem::size_of::<ZSTD_DCtx>() + dctx.decoder.workspace_size()
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
    let src = unsafe { in_slice(src, src_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    // Verify the trailing content checksum like upstream ZSTD_decompress; the
    // setter is idempotent, so reapplying it on a reused DCtx is fine.
    dctx.decoder.set_content_checksum(ContentChecksum::Verify);
    let outcome = catch_unwind(AssertUnwindSafe(|| dctx.decoder.decode_all(src, dst)));
    match outcome {
        Ok(Ok(written)) => written,
        Ok(Err(err)) => encode(code_for_decoder_error(&err)),
        Err(_) => {
            // A panic mid-decode can leave the decoder's internal state
            // partially consumed; replace it with a fresh one (same as
            // ZSTD_createDCtx) so a later call on this reused DCtx starts clean
            // instead of observing a broken invariant. The fresh decoder no
            // longer holds any previously-loaded dictionary, so clear the DDict
            // cache key too — otherwise the next ZSTD_decompress_usingDDict with
            // the same handle would skip re-loading it and decode without it.
            dctx.decoder = FrameDecoder::new();
            dctx.ddict_serial = 0;
            encode(ZSTD_ErrorCode::ZSTD_error_GENERIC)
        }
    }
}
