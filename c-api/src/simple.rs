//! Simple one-shot API: the synchronous `ZSTDLIB_API` slice of `zstd.h`
//! (compress / decompress / sizing / version / level bounds).

use core::ffi::{c_char, c_int, c_uint};
use std::panic::{AssertUnwindSafe, catch_unwind};

use codec::decoding::errors::ReadFrameHeaderError;
use codec::decoding::{
    ContentChecksum, FrameContentSize, FrameDecoder, FrameSizeError, find_frame_compressed_size,
    read_frame_content_size,
};
use codec::encoding::{CompressionLevel, FrameCompressor, compress_bound};

use crate::error::{ZSTD_ErrorCode, code_for_decoder_error, encode};
use crate::ffi::{in_slice, out_slice};

/// `ZSTD_VERSION_NUMBER` for the vendored upstream: MAJOR*10000 + MINOR*100 +
/// RELEASE = 1*10000 + 5*100 + 7.
const VERSION_NUMBER: c_uint = 10_507;

/// `(0ULL - 1)` — frame content size could not be determined from the header.
const CONTENTSIZE_UNKNOWN: u64 = u64::MAX;
/// `(0ULL - 2)` — an error occurred reading the frame header.
const CONTENTSIZE_ERROR: u64 = u64::MAX - 1;

/// `ZSTD_MAX_INPUT_SIZE`: above this `ZSTD_compressBound` reports an error.
const MAX_INPUT_SIZE: usize = if usize::BITS >= 64 {
    0xFF00_FF00_FF00_FF00
} else {
    0xFF00_FF00
};

/// `unsigned ZSTD_versionNumber(void)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_versionNumber() -> c_uint {
    VERSION_NUMBER
}

/// `const char* ZSTD_versionString(void)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_versionString() -> *const c_char {
    c"1.5.7".as_ptr()
}

/// `int ZSTD_minCLevel(void)` — lowest (most negative) level accepted.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_minCLevel() -> c_int {
    CompressionLevel::MIN_LEVEL
}

/// `int ZSTD_maxCLevel(void)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_maxCLevel() -> c_int {
    CompressionLevel::MAX_LEVEL
}

/// `int ZSTD_defaultCLevel(void)`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_defaultCLevel() -> c_int {
    CompressionLevel::DEFAULT_LEVEL
}

/// `size_t ZSTD_compressBound(size_t srcSize)` — worst-case compressed size,
/// or an error code when `srcSize >= ZSTD_MAX_INPUT_SIZE`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_compressBound(src_size: usize) -> usize {
    if src_size >= MAX_INPUT_SIZE {
        encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong)
    } else {
        compress_bound(src_size)
    }
}

/// `size_t ZSTD_compress(void* dst, size_t dstCapacity, const void* src,
/// size_t srcSize, int compressionLevel)`.
///
/// Returns the compressed byte count, or an error code (test with
/// `ZSTD_isError`) when the destination buffer is too small.
///
/// # Safety
/// `dst`/`src` must each be valid for the given capacity/size (or `NULL` with
/// a zero length), per the upstream contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_compress(
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    compression_level: c_int,
) -> usize {
    let src = unsafe { in_slice(src, src_size) };
    let level = CompressionLevel::from_level(compression_level);
    // The bulk encoder aborts via the global allocator on OOM and otherwise
    // does not return errors, but it can panic on an internal invariant
    // break; catch it so a panic never unwinds across the FFI boundary.
    let compressed = match catch_unwind(AssertUnwindSafe(|| {
        let mut enc: FrameCompressor = FrameCompressor::new(level);
        // Upstream ZSTD_compress defaults ZSTD_c_checksumFlag = 0; match it.
        enc.set_content_checksum(false);
        enc.compress_independent_frame(src)
    })) {
        Ok(buf) => buf,
        Err(_) => return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    };
    if compressed.len() > dst_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let dst = unsafe { out_slice(dst, dst_capacity) };
    dst[..compressed.len()].copy_from_slice(&compressed);
    compressed.len()
}

/// `size_t ZSTD_decompress(void* dst, size_t dstCapacity, const void* src,
/// size_t compressedSize)`.
///
/// Returns the decompressed byte count, or an error code (test with
/// `ZSTD_isError`).
///
/// # Safety
/// `dst`/`src` must each be valid for the given capacity/size (or `NULL` with
/// a zero length).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompress(
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    compressed_size: usize,
) -> usize {
    let src = unsafe { in_slice(src, compressed_size) };
    let dst = unsafe { out_slice(dst, dst_capacity) };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = FrameDecoder::new();
        // Verify the trailing content checksum (when the frame carries one), as
        // upstream ZSTD_decompress does: a mismatch surfaces as ChecksumMismatch.
        decoder.set_content_checksum(ContentChecksum::Verify);
        decoder.decode_all(src, dst)
    }));
    match outcome {
        Ok(Ok(written)) => written,
        Ok(Err(err)) => encode(code_for_decoder_error(&err)),
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_GENERIC),
    }
}

/// `unsigned long long ZSTD_getFrameContentSize(const void* src, size_t srcSize)`.
///
/// Returns the declared content size, `ZSTD_CONTENTSIZE_UNKNOWN` when the
/// header omits it, or `ZSTD_CONTENTSIZE_ERROR` when the header is unreadable.
///
/// # Safety
/// `src` must be valid for `src_size` bytes (or `NULL` with `src_size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getFrameContentSize(src: *const u8, src_size: usize) -> u64 {
    let src = unsafe { in_slice(src, src_size) };
    match read_frame_content_size(src) {
        Ok(FrameContentSize::Known(size)) => size,
        Ok(FrameContentSize::Unknown) => CONTENTSIZE_UNKNOWN,
        Err(_) => CONTENTSIZE_ERROR,
    }
}

/// `size_t ZSTD_findFrameCompressedSize(const void* src, size_t srcSize)`.
///
/// Returns the on-disk size of the first frame in `src` (so a caller can step
/// to the next concatenated frame), or an error code (test with `ZSTD_isError`).
///
/// # Safety
/// `src` must be valid for `src_size` bytes (or `NULL` with `src_size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_findFrameCompressedSize(src: *const u8, src_size: usize) -> usize {
    let src = unsafe { in_slice(src, src_size) };
    match find_frame_compressed_size(src) {
        Ok(size) => size,
        // A non-zstd prefix (bad magic / skippable) is "unknown prefix", but a
        // corrupt frame descriptor is a corrupt frame, not an unknown prefix:
        // map it accordingly instead of hiding it as prefix_unknown. Mirrors
        // `fill_frame_header`.
        Err(FrameSizeError::Header(
            ReadFrameHeaderError::BadMagicNumber(_) | ReadFrameHeaderError::SkipFrame { .. },
        )) => encode(ZSTD_ErrorCode::ZSTD_error_prefix_unknown),
        Err(FrameSizeError::Header(ReadFrameHeaderError::InvalidFrameDescriptor(_))) => {
            encode(ZSTD_ErrorCode::ZSTD_error_corruption_detected)
        }
        Err(FrameSizeError::Header(_)) => encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong),
        Err(FrameSizeError::Truncated) => encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong),
        Err(FrameSizeError::ReservedBlock) | Err(FrameSizeError::OversizedBlock) => {
            encode(ZSTD_ErrorCode::ZSTD_error_corruption_detected)
        }
    }
}
