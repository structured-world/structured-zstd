//! Frame-inspection entry points (the experimental `ZSTDLIB_STATIC_API` slice
//! of `zstd.h`): frame-header size, header decode, and decompressed-size
//! queries across one or more concatenated frames.

use core::ffi::{c_uint, c_ulonglong};

use codec::decoding::errors::ReadFrameHeaderError;
use codec::decoding::{
    FrameContentSize, FrameMagic, FrameSizeError, decompression_margin, find_frame_compressed_size,
    frame_decompressed_bound, frame_header_size, frame_magic, min_decoding_buffer_size,
    read_frame_content_size, read_frame_header_info,
};
use codec::skippable::{
    SkippableFrameError, SkippableSliceError, parse_skippable_frame, write_skippable_frame,
};

use crate::error::{ZSTD_ErrorCode, code_for_descriptor_error, encode};
use crate::ffi::{in_slice, out_slice};

/// `(0ULL - 1)` — content size could not be determined.
const CONTENTSIZE_UNKNOWN: c_ulonglong = u64::MAX;
/// `(0ULL - 2)` — an error occurred.
const CONTENTSIZE_ERROR: c_ulonglong = u64::MAX - 1;
/// `ZSTD_BLOCKSIZE_MAX`: the codec's block maximum, named rather than
/// re-declared.
const BLOCKSIZE_MAX: u64 = codec::MAX_BLOCK_SIZE as u64;
/// Base magic of the skippable-frame range; the low nibble is the variant.
const SKIPPABLE_MAGIC_BASE: u32 = codec::skippable::SKIPPABLE_MAGIC_START;
/// `ZSTD_SKIPPABLEHEADERSIZE` = 4-byte magic + 4-byte `Frame_Size`.
const SKIPPABLE_HEADER_SIZE: usize = codec::skippable::SKIPPABLE_HEADER_SIZE;

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
        // A bad frame descriptor is a refused frame, not a too-short read:
        // report it as such (mirrors `fill_frame_header`) instead of the
        // retryable `srcSize_wrong`.
        Err(ReadFrameHeaderError::InvalidFrameDescriptor(descriptor)) => {
            encode(code_for_descriptor_error(&descriptor))
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
        Err(ReadFrameHeaderError::InvalidFrameDescriptor(descriptor)) => {
            encode(code_for_descriptor_error(&descriptor))
        }
        // The remaining variants are short-read failures: ask for as much as
        // the header still needs, as upstream does.
        Err(_) => match codec::decoding::frame_header_input_needed(src, magicless) {
            Ok(needed) => needed,
            Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_prefix_unknown),
        },
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

/// `unsigned ZSTD_isFrame(const void* buffer, size_t size)` — 1 when `buffer`
/// starts with a frame or skippable-frame magic number, else 0.
///
/// # Safety
/// `buffer` must be valid for `size` bytes (or `NULL` with `size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_isFrame(buffer: *const u8, size: usize) -> c_uint {
    let buffer = unsafe { in_slice(buffer, size) };
    c_uint::from(frame_magic(buffer).is_some())
}

/// `unsigned ZSTD_isSkippableFrame(const void* buffer, size_t size)` — 1 when
/// `buffer` starts with a skippable-frame magic number, else 0.
///
/// # Safety
/// `buffer` must be valid for `size` bytes (or `NULL` with `size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_isSkippableFrame(buffer: *const u8, size: usize) -> c_uint {
    let buffer = unsafe { in_slice(buffer, size) };
    c_uint::from(matches!(
        frame_magic(buffer),
        Some(FrameMagic::Skippable(_))
    ))
}

/// `size_t ZSTD_readSkippableFrame(void* dst, size_t dstCapacity, unsigned*
/// magicVariant, const void* src, size_t srcSize)` — copy the payload of the
/// skippable frame `src` starts with into `dst` and return its length;
/// `magicVariant` (optional) receives the variant.
///
/// # Safety
/// `dst` valid for `dst_capacity` bytes, `src` for `src_size` (either may be
/// `NULL` with a zero length); `magic_variant` writable or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_readSkippableFrame(
    dst: *mut u8,
    dst_capacity: usize,
    magic_variant: *mut c_uint,
    src: *const u8,
    src_size: usize,
) -> usize {
    let src = unsafe { in_slice(src, src_size) };
    let (variant, payload) = match parse_skippable_frame(src) {
        Ok(frame) => frame,
        Err(SkippableSliceError::BadMagicNumber(_)) => {
            return encode(ZSTD_ErrorCode::ZSTD_error_frameParameter_unsupported);
        }
        Err(_) => return encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong),
    };
    if payload.len() > dst_capacity {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    // Upstream copies only into a non-NULL destination; NULL just measures.
    if !payload.is_empty() && !dst.is_null() {
        let dst = unsafe { out_slice(dst, dst_capacity) };
        dst[..payload.len()].copy_from_slice(payload);
    }
    if !magic_variant.is_null() {
        unsafe { magic_variant.write(c_uint::from(variant)) };
    }
    payload.len()
}

/// `size_t ZSTD_writeSkippableFrame(void* dst, size_t dstCapacity, const void*
/// src, size_t srcSize, unsigned magicVariant)` — write `src` as a skippable
/// frame with magic `0x184D2A50 + magicVariant` (`0..=15`); returns the bytes
/// written.
///
/// # Safety
/// `dst` valid for `dst_capacity` bytes, `src` for `src_size` (either may be
/// `NULL` with a zero length).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_writeSkippableFrame(
    dst: *mut u8,
    dst_capacity: usize,
    src: *const u8,
    src_size: usize,
    magic_variant: c_uint,
) -> usize {
    if src_size
        .checked_add(SKIPPABLE_HEADER_SIZE)
        .is_none_or(|size| size > dst_capacity)
    {
        return encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall);
    }
    let src = unsafe { in_slice(src, src_size) };
    let mut dst = unsafe { out_slice(dst, dst_capacity) };
    // A variant past `u8` is out of range all the same; the codec rejects it.
    let variant = u8::try_from(magic_variant).unwrap_or(u8::MAX);
    match write_skippable_frame(variant, src, &mut dst) {
        Ok(written) => written,
        Err(SkippableFrameError::InvalidMagicVariant(_)) => {
            encode(ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound)
        }
        Err(SkippableFrameError::PayloadTooLarge(_)) => {
            encode(ZSTD_ErrorCode::ZSTD_error_srcSize_wrong)
        }
        // The capacity was checked above, so the slice writer cannot run out.
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall),
    }
}

/// `size_t ZSTD_decompressionMargin(const void* src, size_t srcSize)` — how
/// far the compressed frames in `src` must end past the end of the output for
/// an in-place decode, or an error code.
///
/// # Safety
/// `src` must be valid for `src_size` bytes (or `NULL` with `src_size == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ZSTD_decompressionMargin(src: *const u8, src_size: usize) -> usize {
    let src = unsafe { in_slice(src, src_size) };
    match decompression_margin(src) {
        Ok(margin) => margin,
        Err(FrameSizeError::Header(ReadFrameHeaderError::BadMagicNumber(_))) => {
            encode(ZSTD_ErrorCode::ZSTD_error_prefix_unknown)
        }
        Err(FrameSizeError::Header(ReadFrameHeaderError::InvalidFrameDescriptor(descriptor))) => {
            encode(code_for_descriptor_error(&descriptor))
        }
        Err(_) => encode(ZSTD_ErrorCode::ZSTD_error_corruption_detected),
    }
}

/// `size_t ZSTD_decodingBufferSize_min(unsigned long long windowSize, unsigned
/// long long frameContentSize)` — the smallest output buffer a block-by-block
/// decode of a frame with this window and content size runs in.
/// `frameContentSize` may be `ZSTD_CONTENTSIZE_UNKNOWN`.
#[unsafe(no_mangle)]
pub extern "C" fn ZSTD_decodingBufferSize_min(
    window_size: c_ulonglong,
    frame_content_size: c_ulonglong,
) -> usize {
    let content_size = (frame_content_size != CONTENTSIZE_UNKNOWN).then_some(frame_content_size);
    min_decoding_buffer_size(window_size, content_size)
        .unwrap_or_else(|| encode(ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge))
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
