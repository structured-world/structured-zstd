//! Synchronous context API: `ZSTD_CCtx` / `ZSTD_DCtx` create / free / one-shot
//! compress / decompress / sizeof.
//!
//! The contexts are opaque heap allocations holding reusable scratch (the
//! compressor's output buffer; the decoder instance) so repeated one-shot
//! calls reuse allocations — the reason a caller holds a context rather than
//! calling the simple API. Advanced parameter setters land in Phase 6.2.

use core::ffi::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::FrameDecoder;
use codec::encoding::{CompressionLevel, FrameCompressor};

use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::ffi::{in_slice, out_slice};

/// Opaque compression context. Carries a reusable output buffer so repeated
/// `ZSTD_compressCCtx` calls amortise the destination allocation.
#[allow(non_camel_case_types)]
pub struct ZSTD_CCtx {
    scratch: Vec<u8>,
}

/// Opaque decompression context. Wraps a reusable [`FrameDecoder`] so its
/// internal buffers persist across `ZSTD_decompressDCtx` calls.
#[allow(non_camel_case_types)]
pub struct ZSTD_DCtx {
    decoder: FrameDecoder,
}

/// `ZSTD_CCtx* ZSTD_createCCtx(void)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createCCtx() -> *mut ZSTD_CCtx {
    Box::into_raw(Box::new(ZSTD_CCtx {
        scratch: Vec::new(),
    }))
}

/// `size_t ZSTD_freeCCtx(ZSTD_CCtx* cctx)` — frees the context; `NULL` is a
/// no-op (returns 0), matching upstream.
///
/// # Safety
/// `cctx` must be a pointer returned by [`ZSTD_createCCtx`] and not already
/// freed, or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeCCtx(cctx: *mut ZSTD_CCtx) -> usize {
    if !cctx.is_null() {
        drop(unsafe { Box::from_raw(cctx) });
    }
    0
}

/// `size_t ZSTD_sizeof_CCtx(const ZSTD_CCtx* cctx)` — current heap footprint,
/// or 0 for `NULL`.
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

/// `ZSTD_DCtx* ZSTD_createDCtx(void)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_createDCtx() -> *mut ZSTD_DCtx {
    Box::into_raw(Box::new(ZSTD_DCtx {
        decoder: FrameDecoder::new(),
    }))
}

/// `size_t ZSTD_freeDCtx(ZSTD_DCtx* dctx)` — frees the context; `NULL` is a
/// no-op (returns 0).
///
/// # Safety
/// `dctx` must be a pointer from [`ZSTD_createDCtx`] and not already freed, or
/// `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_freeDCtx(dctx: *mut ZSTD_DCtx) -> usize {
    if !dctx.is_null() {
        drop(unsafe { Box::from_raw(dctx) });
    }
    0
}

/// `size_t ZSTD_sizeof_DCtx(const ZSTD_DCtx* dctx)` — heap footprint of the
/// context struct, or 0 for `NULL`.
///
/// # Safety
/// `dctx` must be a live pointer from [`ZSTD_createDCtx`], or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_sizeof_DCtx(dctx: *const ZSTD_DCtx) -> usize {
    if dctx.is_null() {
        return 0;
    }
    core::mem::size_of::<ZSTD_DCtx>()
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
    let outcome = catch_unwind(AssertUnwindSafe(|| dctx.decoder.decode_all(src, dst)));
    match outcome {
        Ok(Ok(written)) => written,
        Ok(Err(err)) => encode(code_for_decoder_error(&err)),
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    }
}
