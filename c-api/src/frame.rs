//! Frame-inspection entry points (the experimental `ZSTDLIB_STATIC_API` slice
//! of `zstd.h`): frame-header size, header decode, and decompressed-size
//! queries across one or more concatenated frames.

use core::ffi::{c_uint, c_ulonglong};

use codec::decoding::errors::ReadFrameHeaderError;
use codec::decoding::{
    FrameContentSize, find_frame_compressed_size, frame_decompressed_bound, frame_header_size,
    read_frame_content_size, read_frame_header_info,
};

use crate::error::{ZSTD_ErrorCode, encode};
use crate::ffi::in_slice;

/// `(0ULL - 1)` — content size could not be determined.
const CONTENTSIZE_UNKNOWN: c_ulonglong = u64::MAX;
/// `(0ULL - 2)` — an error occurred.
const CONTENTSIZE_ERROR: c_ulonglong = u64::MAX - 1;
/// `ZSTD_FRAMEHEADERSIZE_MAX` — the "need at most this many bytes" hint
/// `ZSTD_getFrameHeader` returns when `src` is too short.
const FRAMEHEADERSIZE_MAX: usize = 18;
/// `ZSTD_BLOCKSIZE_MAX` = `1 << 17` (128 KiB).
const BLOCKSIZE_MAX: u64 = 1 << 17;
/// Base magic of the skippable-frame range; the low nibble is the variant.
const SKIPPABLE_MAGIC_BASE: u32 = 0x184D_2A50;
/// `ZSTD_SKIPPABLEHEADERSIZE` = 4-byte magic + 4-byte `Frame_Size`.
const SKIPPABLE_HEADER_SIZE: usize = 8;

/// `ZSTD_FrameType_e` — `ZSTD_frame` (0) or `ZSTD_skippableFrame` (1).
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[allow(non_camel_case_types)]
pub enum ZSTD_FrameType_e {
    ZSTD_frame = 0,
    ZSTD_skippableFrame = 1,
}

/// `ZSTD_format_e` — selects the frame format for the `_advanced` query.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[allow(non_camel_case_types)]
pub enum ZSTD_format_e {
    ZSTD_f_zstd1 = 0,
    ZSTD_f_zstd1_magicless = 1,
}

/// `ZSTD_FrameHeader` — layout-compatible with `zstd.h` (field order, sizes,
/// and types are byte-for-byte identical so a C consumer reads it correctly).
#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_camel_case_types, non_snake_case)]
pub struct ZSTD_FrameHeader {
    pub frameContentSize: c_ulonglong,
    pub windowSize: c_ulonglong,
    pub blockSizeMax: c_uint,
    pub frameType: ZSTD_FrameType_e,
    pub headerSize: c_uint,
    pub dictID: c_uint,
    pub checksumFlag: c_uint,
    pub _reserved1: c_uint,
    pub _reserved2: c_uint,
}

/// `size_t ZSTD_frameHeaderSize(const void* src, size_t srcSize)` — length of
/// the frame header (including the magic number), or an error code.
///
/// # Safety
/// `src` must be valid for `src_size` bytes (or `NULL` with `src_size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_frameHeaderSize(src: *const u8, src_size: usize) -> usize {
    let src = unsafe { in_slice(src, src_size) };
    match frame_header_size(src) {
        Ok(size) => size,
        // A skippable frame's header is its fixed 8-byte prefix (4-byte magic +
        // 4-byte Frame_Size), which is what `fill_frame_header` reports for it
        // and what upstream returns here, so a caller can step over the frame.
        Err(ReadFrameHeaderError::SkipFrame { .. }) => SKIPPABLE_HEADER_SIZE,
        Err(ReadFrameHeaderError::BadMagicNumber(_)) => {
            encode(ZSTD_ErrorCode::ZSTD_error_prefix_unknown)
        }
        // A bad frame descriptor is a corrupt frame, not a too-short read:
        // report it as such (mirrors `fill_frame_header`) instead of the
        // retryable `srcSize_wrong`.
        Err(ReadFrameHeaderError::InvalidFrameDescriptor(_)) => {
            encode(ZSTD_ErrorCode::ZSTD_error_corruption_detected)
        }
        // The remaining variants are genuine short reads.
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong),
    }
}

/// Shared body for [`ZSTD_getFrameHeader`] and [`ZSTD_getFrameHeader_advanced`].
///
/// # Safety
/// `zfh` must be a valid, writable `ZSTD_FrameHeader` pointer; `src` valid for
/// its length.
unsafe fn fill_frame_header(zfh: *mut ZSTD_FrameHeader, src: &[u8], magicless: bool) -> usize {
    if zfh.is_null() {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    }
    match read_frame_header_info(src, magicless) {
        Ok(info) => {
            let header = ZSTD_FrameHeader {
                frameContentSize: match info.content_size {
                    FrameContentSize::Known(size) => size,
                    FrameContentSize::Unknown => CONTENTSIZE_UNKNOWN,
                },
                windowSize: info.window_size,
                blockSizeMax: info.window_size.min(BLOCKSIZE_MAX) as c_uint,
                frameType: ZSTD_FrameType_e::ZSTD_frame,
                headerSize: info.header_size as c_uint,
                dictID: info.dictionary_id.unwrap_or(0),
                checksumFlag: u32::from(info.content_checksum),
                _reserved1: 0,
                _reserved2: 0,
            };
            unsafe { zfh.write(header) };
            0
        }
        // Skippable frames are only recognised when the magic is present.
        Err(ReadFrameHeaderError::SkipFrame {
            magic_number,
            length,
        }) if !magicless => {
            let header = ZSTD_FrameHeader {
                frameContentSize: c_ulonglong::from(length),
                windowSize: 0,
                blockSizeMax: 0,
                frameType: ZSTD_FrameType_e::ZSTD_skippableFrame,
                headerSize: 8,
                // For skippable frames upstream stores the magic variant (0-15).
                dictID: magic_number.wrapping_sub(SKIPPABLE_MAGIC_BASE),
                checksumFlag: 0,
                _reserved1: 0,
                _reserved2: 0,
            };
            unsafe { zfh.write(header) };
            0
        }
        Err(ReadFrameHeaderError::BadMagicNumber(_) | ReadFrameHeaderError::SkipFrame { .. }) => {
            encode(ZSTD_ErrorCode::ZSTD_error_prefix_unknown)
        }
        Err(ReadFrameHeaderError::InvalidFrameDescriptor(_)) => {
            encode(ZSTD_ErrorCode::ZSTD_error_corruption_detected)
        }
        // The remaining variants are short-read failures: ask for more input.
        Err(_) => FRAMEHEADERSIZE_MAX,
    }
}

/// `size_t ZSTD_getFrameHeader(ZSTD_FrameHeader* zfhPtr, const void* src,
/// size_t srcSize)` — fills `*zfhPtr`; returns 0 on success, a positive
/// "wanted srcSize" hint when `src` is too short, or an error code.
///
/// # Safety
/// `zfhPtr` must be writable; `src` valid for `src_size` (or `NULL`+0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getFrameHeader(
    zfh_ptr: *mut ZSTD_FrameHeader,
    src: *const u8,
    src_size: usize,
) -> usize {
    let src = unsafe { in_slice(src, src_size) };
    unsafe { fill_frame_header(zfh_ptr, src, false) }
}

/// `size_t ZSTD_getFrameHeader_advanced(ZSTD_FrameHeader* zfhPtr, const void*
/// src, size_t srcSize, ZSTD_format_e format)`.
///
/// `format` is taken as a primitive `c_uint`, not the `ZSTD_format_e` enum: a C
/// caller may pass any integer, and materializing an out-of-range enum
/// discriminant across the FFI boundary is undefined behavior. Only the two
/// defined values are accepted (`0` = zstd1, `1` = magicless); anything else
/// returns a generic error.
///
/// # Safety
/// As [`ZSTD_getFrameHeader`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_getFrameHeader_advanced(
    zfh_ptr: *mut ZSTD_FrameHeader,
    src: *const u8,
    src_size: usize,
    format: c_uint,
) -> usize {
    let magicless = if format == ZSTD_format_e::ZSTD_f_zstd1 as c_uint {
        false
    } else if format == ZSTD_format_e::ZSTD_f_zstd1_magicless as c_uint {
        true
    } else {
        return encode(ZSTD_ErrorCode::ZSTD_error_GENERIC);
    };
    let src = unsafe { in_slice(src, src_size) };
    unsafe { fill_frame_header(zfh_ptr, src, magicless) }
}

/// `unsigned long long ZSTD_findDecompressedSize(const void* src, size_t
/// srcSize)` — sum of declared content sizes across every frame in `src`.
///
/// Returns `ZSTD_CONTENTSIZE_UNKNOWN` if any frame omits its size, or
/// `ZSTD_CONTENTSIZE_ERROR` on a malformed frame.
///
/// # Safety
/// `src` must be valid for `src_size` bytes (or `NULL` with `src_size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_findDecompressedSize(src: *const u8, src_size: usize) -> c_ulonglong {
    let mut rest = unsafe { in_slice(src, src_size) };
    let mut total: u64 = 0;
    while !rest.is_empty() {
        match read_frame_content_size(rest) {
            // checked_add, not saturating: a saturated u64::MAX would alias the
            // CONTENTSIZE_UNKNOWN sentinel and mask the overflow.
            Ok(FrameContentSize::Known(size)) => match total.checked_add(size) {
                Some(sum) => total = sum,
                None => return CONTENTSIZE_ERROR,
            },
            Ok(FrameContentSize::Unknown) => return CONTENTSIZE_UNKNOWN,
            // Skippable frames add nothing to the decompressed size.
            Err(ReadFrameHeaderError::SkipFrame { .. }) => {}
            Err(_) => return CONTENTSIZE_ERROR,
        }
        match find_frame_compressed_size(rest) {
            Ok(consumed) if consumed > 0 && consumed <= rest.len() => rest = &rest[consumed..],
            _ => return CONTENTSIZE_ERROR,
        }
    }
    total
}

/// `unsigned long long ZSTD_decompressBound(const void* src, size_t srcSize)` —
/// an upper bound on the total decompressed size of every frame in `src`.
///
/// Always returns a bound (exact when sizes are declared, otherwise a loose
/// per-block bound) or `ZSTD_CONTENTSIZE_ERROR` on a malformed frame.
///
/// # Safety
/// `src` must be valid for `src_size` bytes (or `NULL` with `src_size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressBound(src: *const u8, src_size: usize) -> c_ulonglong {
    let mut rest = unsafe { in_slice(src, src_size) };
    let mut total: u64 = 0;
    while !rest.is_empty() {
        match frame_decompressed_bound(rest) {
            // checked_add, not saturating: a saturated u64::MAX would alias the
            // CONTENTSIZE_UNKNOWN sentinel and mask the overflow.
            Ok(bound) => match total.checked_add(bound) {
                Some(sum) => total = sum,
                None => return CONTENTSIZE_ERROR,
            },
            Err(_) => return CONTENTSIZE_ERROR,
        }
        match find_frame_compressed_size(rest) {
            Ok(consumed) if consumed > 0 && consumed <= rest.len() => rest = &rest[consumed..],
            _ => return CONTENTSIZE_ERROR,
        }
    }
    total
}
