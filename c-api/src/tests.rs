//! In-crate correctness tests for the C ABI wrappers. These call the
//! `extern "C"` entry points directly (reachable in-crate) to exercise the
//! real symbol behaviour; the C-consumer link test in `tests/` is added
//! separately and verifies a real `#include <zstd.h>` consumer.

use crate::context::{
    ZSTD_compressCCtx, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_decompressDCtx, ZSTD_freeCCtx,
    ZSTD_freeDCtx, ZSTD_sizeof_CCtx,
};
use crate::error::{ZSTD_ErrorCode, ZSTD_getErrorCode, ZSTD_isError};
use crate::frame::{
    ZSTD_FrameHeader, ZSTD_FrameType_e, ZSTD_decompressBound, ZSTD_findDecompressedSize,
    ZSTD_frameHeaderSize, ZSTD_getFrameHeader,
};
use crate::simple::{
    ZSTD_compress, ZSTD_compressBound, ZSTD_decompress, ZSTD_defaultCLevel,
    ZSTD_findFrameCompressedSize, ZSTD_getFrameContentSize, ZSTD_maxCLevel, ZSTD_minCLevel,
    ZSTD_versionNumber,
};

fn sample(len: usize) -> Vec<u8> {
    // Deterministic, mildly compressible bytes (no rng dependency).
    (0..len)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

#[test]
fn simple_roundtrips_one_mib() {
    let input = sample(1 << 20);
    let bound = ZSTD_compressBound(input.len());
    let mut compressed = vec![0u8; bound];
    let csize = unsafe {
        ZSTD_compress(
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(csize), 0, "compress reported an error");
    assert!(csize <= bound);

    let declared = unsafe { ZSTD_getFrameContentSize(compressed.as_ptr(), csize) };
    assert_eq!(declared, input.len() as u64);

    let mut restored = vec![0u8; input.len()];
    let dsize = unsafe {
        ZSTD_decompress(
            restored.as_mut_ptr(),
            restored.len(),
            compressed.as_ptr(),
            csize,
        )
    };
    assert_eq!(ZSTD_isError(dsize), 0, "decompress reported an error");
    assert_eq!(dsize, input.len());
    assert_eq!(restored, input);
}

#[test]
fn compress_into_too_small_dst_is_error() {
    let input = sample(4096);
    let mut tiny = [0u8; 4];
    let r = unsafe {
        ZSTD_compress(
            tiny.as_mut_ptr(),
            tiny.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_ne!(ZSTD_isError(r), 0);
    assert_eq!(
        ZSTD_getErrorCode(r),
        ZSTD_ErrorCode::ZSTD_error_dstSize_tooSmall
    );
}

#[test]
fn decompress_garbage_maps_to_error_code() {
    let garbage = [0xABu8; 64];
    let mut out = [0u8; 64];
    let r =
        unsafe { ZSTD_decompress(out.as_mut_ptr(), out.len(), garbage.as_ptr(), garbage.len()) };
    assert_ne!(ZSTD_isError(r), 0);
    // Bad magic -> prefix_unknown (the code upstream returns for a non-frame).
    assert_eq!(
        ZSTD_getErrorCode(r),
        ZSTD_ErrorCode::ZSTD_error_prefix_unknown
    );
}

#[test]
fn context_api_roundtrips_and_reuses() {
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    assert!(!cctx.is_null() && !dctx.is_null());

    for len in [0usize, 1, 4096, 200_000] {
        let input = sample(len);
        let bound = ZSTD_compressBound(len);
        let mut compressed = vec![0u8; bound];
        let csize = unsafe {
            ZSTD_compressCCtx(
                cctx,
                compressed.as_mut_ptr(),
                compressed.len(),
                input.as_ptr(),
                input.len(),
                5,
            )
        };
        assert_eq!(ZSTD_isError(csize), 0);

        let mut restored = vec![0u8; len];
        let dsize = unsafe {
            ZSTD_decompressDCtx(
                dctx,
                restored.as_mut_ptr(),
                restored.len(),
                compressed.as_ptr(),
                csize,
            )
        };
        assert_eq!(ZSTD_isError(dsize), 0);
        assert_eq!(dsize, len);
        assert_eq!(restored, input);
    }

    // Context tracks a non-zero footprint after use; free is a clean no-op on NULL.
    assert!(unsafe { ZSTD_sizeof_CCtx(cctx) } >= core::mem::size_of::<crate::ZSTD_CCtx>());
    assert_eq!(unsafe { ZSTD_freeCCtx(cctx) }, 0);
    assert_eq!(unsafe { ZSTD_freeDCtx(dctx) }, 0);
    assert_eq!(unsafe { ZSTD_freeCCtx(core::ptr::null_mut()) }, 0);
}

fn compress_frame(input: &[u8]) -> Vec<u8> {
    let bound = ZSTD_compressBound(input.len());
    let mut out = vec![0u8; bound];
    let n = unsafe { ZSTD_compress(out.as_mut_ptr(), out.len(), input.as_ptr(), input.len(), 3) };
    assert_eq!(ZSTD_isError(n), 0);
    out.truncate(n);
    out
}

#[test]
fn find_frame_compressed_size_locates_frame_boundary() {
    let frame = compress_frame(&sample(4096));
    // A lone frame's compressed size is the whole buffer.
    let size = unsafe { ZSTD_findFrameCompressedSize(frame.as_ptr(), frame.len()) };
    assert_eq!(ZSTD_isError(size), 0);
    assert_eq!(size, frame.len());

    // With a second frame appended, it still reports only the first frame, so
    // a caller can step to the next one.
    let mut two = frame.clone();
    two.extend_from_slice(&compress_frame(&sample(100)));
    let first = unsafe { ZSTD_findFrameCompressedSize(two.as_ptr(), two.len()) };
    assert_eq!(first, frame.len());
}

#[test]
fn find_frame_compressed_size_rejects_garbage() {
    let garbage = [0u8; 16];
    let r = unsafe { ZSTD_findFrameCompressedSize(garbage.as_ptr(), garbage.len()) };
    assert_ne!(ZSTD_isError(r), 0);
}

#[test]
fn get_frame_header_fills_fields() {
    let frame = compress_frame(&sample(2048));
    let hdr_size = unsafe { ZSTD_frameHeaderSize(frame.as_ptr(), frame.len()) };
    assert_eq!(ZSTD_isError(hdr_size), 0);
    assert!((5..=18).contains(&hdr_size));

    let mut zfh = ZSTD_FrameHeader {
        frameContentSize: 0,
        windowSize: 0,
        blockSizeMax: 0,
        frameType: ZSTD_FrameType_e::ZSTD_skippableFrame,
        headerSize: 0,
        dictID: 0,
        checksumFlag: 7,
        _reserved1: 0,
        _reserved2: 0,
    };
    let r = unsafe { ZSTD_getFrameHeader(&mut zfh, frame.as_ptr(), frame.len()) };
    assert_eq!(r, 0, "header complete");
    assert_eq!(zfh.frameType, ZSTD_FrameType_e::ZSTD_frame);
    assert_eq!(zfh.frameContentSize, 2048);
    assert!(zfh.windowSize >= 2048);
    assert_eq!(zfh.headerSize as usize, hdr_size);
    assert_eq!(zfh.dictID, 0);
}

#[test]
fn get_frame_header_short_input_asks_for_more() {
    let frame = compress_frame(&sample(2048));
    let mut zfh = ZSTD_FrameHeader {
        frameContentSize: 0,
        windowSize: 0,
        blockSizeMax: 0,
        frameType: ZSTD_FrameType_e::ZSTD_frame,
        headerSize: 0,
        dictID: 0,
        checksumFlag: 0,
        _reserved1: 0,
        _reserved2: 0,
    };
    // Only 2 bytes: too short for even the magic; expect a positive size hint.
    let r = unsafe { ZSTD_getFrameHeader(&mut zfh, frame.as_ptr(), 2) };
    assert_eq!(ZSTD_isError(r), 0);
    assert!(r > 0 && r <= 18);
}

#[test]
fn decompressed_size_queries_span_multiple_frames() {
    let mut two = compress_frame(&sample(4096));
    two.extend_from_slice(&compress_frame(&sample(1000)));

    let total = unsafe { ZSTD_findDecompressedSize(two.as_ptr(), two.len()) };
    assert_eq!(total, 4096 + 1000);

    let bound = unsafe { ZSTD_decompressBound(two.as_ptr(), two.len()) };
    assert!(bound >= 4096 + 1000, "bound must not undercount");
}

#[test]
fn level_bounds_match_crate() {
    assert_eq!(ZSTD_minCLevel(), -131072);
    assert_eq!(ZSTD_maxCLevel(), 22);
    assert_eq!(ZSTD_defaultCLevel(), 3);
    assert_eq!(ZSTD_versionNumber(), 10_507);
}
