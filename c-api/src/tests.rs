//! In-crate correctness tests for the C ABI wrappers. These call the
//! `extern "C"` entry points directly (reachable in-crate) to exercise the
//! real symbol behaviour; the C-consumer link test in `tests/` is added
//! separately and verifies a real `#include <zstd.h>` consumer.

use crate::context::{
    ZSTD_compressCCtx, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_decompressDCtx, ZSTD_freeCCtx,
    ZSTD_freeDCtx, ZSTD_sizeof_CCtx,
};
use crate::error::{ZSTD_ErrorCode, ZSTD_getErrorCode, ZSTD_isError};
use crate::simple::{
    ZSTD_compress, ZSTD_compressBound, ZSTD_decompress, ZSTD_defaultCLevel,
    ZSTD_getFrameContentSize, ZSTD_maxCLevel, ZSTD_minCLevel, ZSTD_versionNumber,
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

#[test]
fn level_bounds_match_crate() {
    assert_eq!(ZSTD_minCLevel(), -131072);
    assert_eq!(ZSTD_maxCLevel(), 22);
    assert_eq!(ZSTD_defaultCLevel(), 3);
    assert_eq!(ZSTD_versionNumber(), 10_507);
}
