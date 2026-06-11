//! In-crate correctness tests for the C ABI wrappers. These call the
//! `extern "C"` entry points directly (reachable in-crate) to exercise the
//! real symbol behaviour; the C-consumer link test in `tests/` is added
//! separately and verifies a real `#include <zstd.h>` consumer.

use crate::cdict::{
    ZSTD_compress_usingCDict, ZSTD_createCDict, ZSTD_createDDict, ZSTD_decompress_usingDDict,
    ZSTD_freeCDict, ZSTD_freeDDict, ZSTD_getDictID_fromCDict, ZSTD_getDictID_fromDDict,
    ZSTD_sizeof_CDict, ZSTD_sizeof_DDict,
};
use crate::context::{
    ZSTD_compressCCtx, ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_decompressDCtx, ZSTD_freeCCtx,
    ZSTD_freeDCtx, ZSTD_sizeof_CCtx,
};
use crate::dict::{ZDICT_getDictHeaderSize, ZDICT_getDictID, ZDICT_isError, ZDICT_trainFromBuffer};
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

#[test]
fn decompress_rejects_corrupted_content_checksum() {
    let input = sample(4096);
    // ZSTD_compress mirrors upstream and emits no content checksum by default,
    // so build a checksum-bearing frame explicitly through the core encoder.
    // Flipping the trailing 4-byte checksum makes the stored value disagree
    // with the decoded output while leaving the block data (and the decode
    // itself) intact. A faithful drop-in must report this as
    // ZSTD_error_checksum_wrong, not silently accept the frame.
    let mut frame = {
        let mut enc: codec::encoding::FrameCompressor =
            codec::encoding::FrameCompressor::new(codec::encoding::CompressionLevel::from_level(3));
        enc.set_content_checksum(true);
        enc.compress_independent_frame(&input)
    };
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;

    let mut out = vec![0u8; input.len()];
    let r = unsafe { ZSTD_decompress(out.as_mut_ptr(), out.len(), frame.as_ptr(), frame.len()) };
    assert_ne!(
        ZSTD_isError(r),
        0,
        "corrupted content checksum must be rejected"
    );
    assert_eq!(
        ZSTD_getErrorCode(r),
        ZSTD_ErrorCode::ZSTD_error_checksum_wrong
    );
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

/// ABI layout lock for `ZSTD_FrameHeader`: it is passed by value across the C
/// boundary, so its field offsets MUST match the `zstd.h` struct. A field
/// reorder / type change that breaks a C consumer fails here rather than
/// silently corrupting reads. Offsets are pointer-width independent (the two
/// leading u64s sit at 0/8 on every ABI); the total size is 8-aligned on
/// 64-bit targets.
#[test]
fn frame_header_abi_layout_is_stable() {
    use core::mem::{align_of, offset_of, size_of};
    assert_eq!(offset_of!(ZSTD_FrameHeader, frameContentSize), 0);
    assert_eq!(offset_of!(ZSTD_FrameHeader, windowSize), 8);
    assert_eq!(offset_of!(ZSTD_FrameHeader, blockSizeMax), 16);
    assert_eq!(offset_of!(ZSTD_FrameHeader, frameType), 20);
    assert_eq!(offset_of!(ZSTD_FrameHeader, headerSize), 24);
    assert_eq!(offset_of!(ZSTD_FrameHeader, dictID), 28);
    assert_eq!(offset_of!(ZSTD_FrameHeader, checksumFlag), 32);
    assert_eq!(offset_of!(ZSTD_FrameHeader, _reserved1), 36);
    assert_eq!(offset_of!(ZSTD_FrameHeader, _reserved2), 40);
    // C enums and the `unsigned` fields are 4 bytes.
    assert_eq!(size_of::<ZSTD_FrameType_e>(), 4);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<ZSTD_FrameHeader>(), 48);
        assert_eq!(align_of::<ZSTD_FrameHeader>(), 8);
    }
}

#[test]
fn compress_emits_no_content_checksum_by_default() {
    // Upstream ZSTD_compress defaults ZSTD_c_checksumFlag = 0, so the simple
    // wrapper must emit no trailing content checksum (cleared flag in header).
    let input = sample(4096);
    let bound = ZSTD_compressBound(input.len());
    let mut frame = vec![0u8; bound];
    let n = unsafe {
        ZSTD_compress(
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    frame.truncate(n);
    let descriptor = frame[4];
    assert_eq!(
        (descriptor >> 2) & 1,
        0,
        "default ZSTD_compress frame must not set the content-checksum flag"
    );
}

#[test]
fn compress_cctx_emits_no_content_checksum_by_default() {
    // Same guarantee for the context path: ZSTD_compressCCtx must also default
    // to no content checksum, matching upstream's ZSTD_c_checksumFlag = 0.
    let input = sample(4096);
    let cctx = ZSTD_createCCtx();
    assert!(!cctx.is_null());

    let bound = ZSTD_compressBound(input.len());
    let mut frame = vec![0u8; bound];
    let n = unsafe {
        ZSTD_compressCCtx(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0);
    frame.truncate(n);

    let descriptor = frame[4];
    assert_eq!(
        (descriptor >> 2) & 1,
        0,
        "default ZSTD_compressCCtx frame must not set the content-checksum flag"
    );

    assert_eq!(unsafe { ZSTD_freeCCtx(cctx) }, 0);
}

#[test]
fn zdict_train_produces_a_valid_dictionary() {
    // Many small, similar samples: a concatenated buffer plus per-sample sizes,
    // the exact layout ZDICT_trainFromBuffer expects.
    let mut samples: Vec<u8> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for i in 0..512u32 {
        let s = format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n");
        sizes.push(s.len());
        samples.extend_from_slice(s.as_bytes());
    }

    let mut dict = vec![0u8; 64 * 1024];
    let n = unsafe {
        ZDICT_trainFromBuffer(
            dict.as_mut_ptr(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    assert_eq!(ZDICT_isError(n), 0, "training reported an error");
    assert!(n > 0 && n <= dict.len());
    dict.truncate(n);

    // A valid dictionary carries a non-zero ID and a header smaller than itself.
    let id = unsafe { ZDICT_getDictID(dict.as_ptr(), dict.len()) };
    assert_ne!(id, 0, "trained dictionary must carry a non-zero ID");
    let header = unsafe { ZDICT_getDictHeaderSize(dict.as_ptr(), dict.len()) };
    assert_eq!(
        ZDICT_isError(header),
        0,
        "header size query reported an error"
    );
    assert!(header > 0 && header < dict.len());

    // A buffer that is not a dictionary reports ID 0.
    let garbage = [0xABu8; 16];
    assert_eq!(
        unsafe { ZDICT_getDictID(garbage.as_ptr(), garbage.len()) },
        0
    );
}

/// Train a dictionary from many small similar records and return the bytes.
fn trained_dictionary() -> Vec<u8> {
    let mut samples: Vec<u8> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for i in 0..512u32 {
        let s = format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n");
        sizes.push(s.len());
        samples.extend_from_slice(s.as_bytes());
    }
    let mut dict = vec![0u8; 64 * 1024];
    let n = unsafe {
        ZDICT_trainFromBuffer(
            dict.as_mut_ptr(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    assert_eq!(ZDICT_isError(n), 0, "training reported an error");
    dict.truncate(n);
    dict
}

#[test]
fn cdict_ddict_roundtrip_and_reuse() {
    let dict = trained_dictionary();
    let dict_id = unsafe { ZDICT_getDictID(dict.as_ptr(), dict.len()) };
    assert_ne!(dict_id, 0);

    let cdict = unsafe { ZSTD_createCDict(dict.as_ptr(), dict.len(), 19) };
    assert!(!cdict.is_null(), "ZSTD_createCDict returned NULL");
    let ddict = unsafe { ZSTD_createDDict(dict.as_ptr(), dict.len()) };
    assert!(!ddict.is_null(), "ZSTD_createDDict returned NULL");

    // The prepared dictionaries echo the trained ID and report a non-zero size.
    assert_eq!(unsafe { ZSTD_getDictID_fromCDict(cdict) }, dict_id);
    assert_eq!(unsafe { ZSTD_getDictID_fromDDict(ddict) }, dict_id);
    assert!(unsafe { ZSTD_sizeof_CDict(cdict) } >= dict.len());
    assert!(unsafe { ZSTD_sizeof_DDict(ddict) } >= dict.len());

    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    assert!(!cctx.is_null() && !dctx.is_null());

    // Compress two distinct payloads through the SAME cctx + cdict to exercise
    // the cached-compressor reuse path (second call must not re-parse/re-prime
    // yet must still produce a correctly-decodable frame).
    let payloads = [
        b"tenant=demo table=orders key=99999 region=eu payload=aaaaabbbbbccccc\n".to_vec(),
        {
            let mut v = Vec::new();
            for i in 1000..1100u32 {
                v.extend_from_slice(
                    format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n")
                        .as_bytes(),
                );
            }
            v
        },
    ];

    for payload in &payloads {
        let bound = ZSTD_compressBound(payload.len());
        let mut compressed = vec![0u8; bound];
        let clen = unsafe {
            ZSTD_compress_usingCDict(
                cctx,
                compressed.as_mut_ptr(),
                compressed.len(),
                payload.as_ptr(),
                payload.len(),
                cdict,
            )
        };
        assert_eq!(ZSTD_isError(clen), 0, "compress_usingCDict errored");
        compressed.truncate(clen);

        let mut restored = vec![0u8; payload.len()];
        let dlen = unsafe {
            ZSTD_decompress_usingDDict(
                dctx,
                restored.as_mut_ptr(),
                restored.len(),
                compressed.as_ptr(),
                compressed.len(),
                ddict,
            )
        };
        assert_eq!(ZSTD_isError(dlen), 0, "decompress_usingDDict errored");
        assert_eq!(dlen, payload.len());
        assert_eq!(&restored, payload, "dictionary round-trip mismatch");
    }

    unsafe {
        ZSTD_freeCCtx(cctx);
        ZSTD_freeDCtx(dctx);
        ZSTD_freeCDict(cdict);
        ZSTD_freeDDict(ddict);
    }
}

#[test]
fn sizeof_cctx_counts_cached_dictionary_compressor() {
    // After a ZSTD_compress_usingCDict call, the context caches a primed
    // FrameCompressor whose match-finder tables dominate its real footprint.
    // ZSTD_sizeof_CCtx must grow to reflect that heap, not just the inline
    // struct + scratch (regression: it previously omitted the cached compressor).
    let dict = trained_dictionary();
    let cdict = unsafe { ZSTD_createCDict(dict.as_ptr(), dict.len(), 19) };
    assert!(!cdict.is_null());
    let cctx = ZSTD_createCCtx();
    assert!(!cctx.is_null());

    let before = unsafe { ZSTD_sizeof_CCtx(cctx) };

    let payload = b"tenant=demo table=orders key=1 region=eu payload=aaaaabbbbbccccc\n";
    let mut compressed = vec![0u8; ZSTD_compressBound(payload.len())];
    let clen = unsafe {
        ZSTD_compress_usingCDict(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            payload.as_ptr(),
            payload.len(),
            cdict,
        )
    };
    assert_eq!(ZSTD_isError(clen), 0);

    let after = unsafe { ZSTD_sizeof_CCtx(cctx) };
    // The primed match-finder tables for a level-19 dictionary are well above
    // 64 KiB, so the reported size must jump substantially once they exist.
    assert!(
        after > before + 64 * 1024,
        "sizeof_CCtx must include the cached dict compressor's heap: before={before}, after={after}"
    );

    unsafe {
        ZSTD_freeCCtx(cctx);
        ZSTD_freeCDict(cdict);
    }
}

#[test]
fn create_cdict_rejects_non_dictionary() {
    // A buffer with no dictionary magic and no valid entropy is not a parseable
    // encoder dictionary; createCDict must return NULL (never crash).
    let garbage = [0xABu8; 64];
    let cdict = unsafe { ZSTD_createCDict(garbage.as_ptr(), garbage.len(), 3) };
    assert!(cdict.is_null(), "createCDict accepted a non-dictionary");
}

// ---- Phase 6.2: advanced parameters + streaming ----

use crate::params::{
    ZSTD_CCtx_getParameter, ZSTD_CCtx_reset, ZSTD_CCtx_setParameter, ZSTD_CCtx_setPledgedSrcSize,
    ZSTD_DCtx_setParameter, ZSTD_bounds, ZSTD_cParam_getBounds, ZSTD_dParam_getBounds,
};
use crate::streaming::{
    ZSTD_CStreamInSize, ZSTD_CStreamOutSize, ZSTD_DStreamInSize, ZSTD_DStreamOutSize,
    ZSTD_compressStream2, ZSTD_decompressStream, ZSTD_endStream, ZSTD_flushStream, ZSTD_inBuffer,
    ZSTD_outBuffer,
};
use core::ffi::c_int;

// ABI invariants: struct sizes match upstream `sizeof` on 64-bit (on
// 32-bit targets `size_t` / pointers halve the layouts).
#[test]
#[cfg(target_pointer_width = "64")]
fn streaming_buffer_abi_sizes() {
    assert_eq!(core::mem::size_of::<ZSTD_inBuffer>(), 24);
    assert_eq!(core::mem::size_of::<ZSTD_outBuffer>(), 24);
    assert_eq!(core::mem::size_of::<ZSTD_bounds>(), 16);
}

#[test]
fn c_param_bounds_cover_stable_set() {
    // Every stable v1.5.7 cParameter discriminant must report bounds.
    for param in [
        100, 101, 102, 103, 104, 105, 106, 107, 130, 160, 161, 162, 163, 164, 200, 201, 202, 400,
        401, 402,
    ] {
        let b = ZSTD_cParam_getBounds(param);
        assert_eq!(ZSTD_isError(b.error), 0, "param {param} must have bounds");
        assert!(b.lowerBound <= b.upperBound, "param {param} bounds order");
    }
    // Unknown discriminant errors.
    let b = ZSTD_cParam_getBounds(99);
    assert_ne!(ZSTD_isError(b.error), 0);
    let d = ZSTD_dParam_getBounds(100);
    assert_eq!(ZSTD_isError(d.error), 0);
    assert_ne!(ZSTD_isError(ZSTD_dParam_getBounds(99).error), 0);
}

#[test]
fn set_parameter_validates_and_reads_back() {
    let cctx = ZSTD_createCCtx();
    unsafe {
        // In-bounds values stick and read back.
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 100, 7), 0); // level
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 101, 20), 0); // windowLog
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 201, 1), 0); // checksum
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 107, 9), 0); // strategy btultra2
        let mut v: c_int = -1;
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 100, &mut v), 0);
        assert_eq!(v, 7);
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 101, &mut v), 0);
        assert_eq!(v, 20);
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 201, &mut v), 0);
        assert_eq!(v, 1);
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 107, &mut v), 0);
        assert_eq!(v, 9);

        // Out-of-bounds rejected with parameter_outOfBound.
        let rc = ZSTD_CCtx_setParameter(cctx, 101, 99);
        assert_ne!(ZSTD_isError(rc), 0);
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        );
        // Unknown parameter rejected as unsupported.
        let rc = ZSTD_CCtx_setParameter(cctx, 9999, 1);
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_parameter_unsupported
        );
        // 0 returns a tunable to auto.
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 101, 0), 0);
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 101, &mut v), 0);
        assert_eq!(v, 0);

        // Parameters reset restores defaults.
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 201, 1), 0);
        assert_eq!(ZSTD_CCtx_reset(cctx, 3), 0); // session_and_parameters
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 201, &mut v), 0);
        assert_eq!(v, 0);
        assert_eq!(ZSTD_CCtx_getParameter(cctx, 100, &mut v), 0);
        assert_eq!(v, 3);

        ZSTD_freeCCtx(cctx);
    }
}

#[test]
fn compress2_roundtrips_with_sticky_parameters() {
    let input = sample(256 * 1024);
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 100, 5), 0);
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 201, 1), 0); // checksum on
        let csize = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(csize), 0, "compress2 reported an error");

        // The checksum flag must show in the frame header.
        let mut zfh = core::mem::MaybeUninit::<ZSTD_FrameHeader>::zeroed();
        assert_eq!(
            ZSTD_getFrameHeader(zfh.as_mut_ptr(), compressed.as_ptr(), csize),
            0
        );
        assert_eq!(zfh.assume_init_ref().checksumFlag, 1);

        let mut restored = vec![0u8; input.len()];
        let dsize = ZSTD_decompressDCtx(
            dctx,
            restored.as_mut_ptr(),
            restored.len(),
            compressed.as_ptr(),
            csize,
        );
        assert_eq!(ZSTD_isError(dsize), 0);
        assert_eq!(dsize, input.len());
        assert_eq!(restored, input);

        // A mismatching pledge is rejected.
        assert_eq!(ZSTD_CCtx_setPledgedSrcSize(cctx, 1), 0);
        let rc = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
        );

        ZSTD_freeCCtx(cctx);
        ZSTD_freeDCtx(dctx);
    }
}

#[test]
fn content_size_and_dict_id_flags_apply() {
    let input = sample(64 * 1024);
    let cctx = ZSTD_createCCtx();
    let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 200, 0), 0); // contentSizeFlag off
        let csize = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(csize), 0);
        // ZSTD_CONTENTSIZE_UNKNOWN == -1 as u64.
        let declared = ZSTD_getFrameContentSize(compressed.as_ptr(), csize);
        assert_eq!(declared, u64::MAX, "FCS must be omitted");
        ZSTD_freeCCtx(cctx);
    }
}

/// Drive a full streaming round-trip with deliberately tiny output buffers
/// so the flush/end loops exercise the partial-drain paths.
#[test]
fn streaming_roundtrip_chunked() {
    let input = sample(1 << 20);
    let zcs = crate::streaming::ZSTD_createCStream();
    let mut compressed: Vec<u8> = Vec::new();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(zcs, 100, 3), 0);
        assert_eq!(ZSTD_CCtx_setParameter(zcs, 201, 1), 0); // checksum

        // Feed in 64 KiB slices with a 1 KiB output buffer.
        let mut outbuf = vec![0u8; 1024];
        for chunk in input.chunks(64 * 1024) {
            let mut inb = ZSTD_inBuffer {
                src: chunk.as_ptr() as *const core::ffi::c_void,
                size: chunk.len(),
                pos: 0,
            };
            loop {
                let mut outb = ZSTD_outBuffer {
                    dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                    size: outbuf.len(),
                    pos: 0,
                };
                let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0);
                assert_eq!(ZSTD_isError(rc), 0, "continue errored");
                compressed.extend_from_slice(&outbuf[..outb.pos]);
                if inb.pos == inb.size && rc == 0 {
                    break;
                }
                if inb.pos == inb.size && outb.pos < outb.size {
                    // Input consumed; leftovers smaller than the buffer will
                    // drain on the next call or at end.
                    break;
                }
            }
        }
        // Finish the frame, looping until fully flushed.
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_endStream(zcs, &mut outb);
            assert_eq!(ZSTD_isError(rc), 0, "end errored");
            compressed.extend_from_slice(&outbuf[..outb.pos]);
            if rc == 0 {
                break;
            }
        }
        crate::streaming::ZSTD_freeCStream(zcs);
    }

    // Decompress through the streaming decoder with small buffers too.
    let zds = crate::streaming::ZSTD_createDStream();
    let mut restored: Vec<u8> = Vec::new();
    unsafe {
        assert_ne!(crate::streaming::ZSTD_initDStream(zds), 0);
        let mut outbuf = vec![0u8; 4096];
        let mut inb = ZSTD_inBuffer {
            src: compressed.as_ptr() as *const core::ffi::c_void,
            size: compressed.len(),
            pos: 0,
        };
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
            assert_eq!(ZSTD_isError(rc), 0, "decompressStream errored");
            restored.extend_from_slice(&outbuf[..outb.pos]);
            if rc == 0 && inb.pos == inb.size {
                break;
            }
            assert!(
                outb.pos > 0 || inb.pos < inb.size,
                "no forward progress in decode loop"
            );
        }
        crate::streaming::ZSTD_freeDStream(zds);
    }
    assert_eq!(restored, input, "streaming round-trip must be byte-exact");
}

#[test]
fn flush_stream_emits_decodable_prefix_and_two_frames_back_to_back() {
    let part1 = sample(100_000);
    let part2 = sample(50_000);
    let zcs = crate::streaming::ZSTD_createCStream();
    let mut compressed: Vec<u8> = Vec::new();
    let mut outbuf = vec![0u8; ZSTD_CStreamOutSize()];
    unsafe {
        // Frame 1: write, flush mid-frame, then end.
        for (data, finish) in [(&part1, false), (&part2, true)] {
            let mut inb = ZSTD_inBuffer {
                src: data.as_ptr() as *const core::ffi::c_void,
                size: data.len(),
                pos: 0,
            };
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0);
            assert_eq!(ZSTD_isError(rc), 0);
            compressed.extend_from_slice(&outbuf[..outb.pos]);
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = if finish {
                ZSTD_endStream(zcs, &mut outb)
            } else {
                ZSTD_flushStream(zcs, &mut outb)
            };
            assert_eq!(ZSTD_isError(rc), 0);
            assert_eq!(rc, 0, "big buffer must fully drain");
            compressed.extend_from_slice(&outbuf[..outb.pos]);
        }
        // Second frame on the same context (sticky params, new frame).
        let mut inb = ZSTD_inBuffer {
            src: part1.as_ptr() as *const core::ffi::c_void,
            size: part1.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2); // e_end
        assert_eq!(ZSTD_isError(rc), 0);
        assert_eq!(rc, 0);
        compressed.extend_from_slice(&outbuf[..outb.pos]);
        crate::streaming::ZSTD_freeCStream(zcs);
    }

    // Both frames decode back-to-back via the streaming decoder.
    let zds = crate::streaming::ZSTD_createDStream();
    let mut restored: Vec<u8> = Vec::new();
    unsafe {
        let mut outbuf = vec![0u8; ZSTD_DStreamOutSize()];
        let mut inb = ZSTD_inBuffer {
            src: compressed.as_ptr() as *const core::ffi::c_void,
            size: compressed.len(),
            pos: 0,
        };
        while inb.pos < inb.size {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
            assert_eq!(ZSTD_isError(rc), 0);
            restored.extend_from_slice(&outbuf[..outb.pos]);
        }
        crate::streaming::ZSTD_freeDStream(zds);
    }
    let mut expected = part1.clone();
    expected.extend_from_slice(&part2);
    expected.extend_from_slice(&part1);
    assert_eq!(restored, expected);
}

#[test]
fn d_window_log_max_rejects_oversized_window() {
    // Compress 1 MiB at a window the decoder limit will reject.
    let input = sample(1 << 20);
    let cctx = ZSTD_createCCtx();
    let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
    let csize = unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 101, 20), 0); // windowLog 20
        // Suppress the FCS so the header carries a window descriptor (a
        // known content size lets decoders ignore the window entirely).
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 200, 0), 0);
        let csize = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        ZSTD_freeCCtx(cctx);
        csize
    };
    assert_eq!(ZSTD_isError(csize), 0);

    let zds = crate::streaming::ZSTD_createDStream();
    unsafe {
        // Limit below the frame's window log → rejected.
        assert_eq!(ZSTD_DCtx_setParameter(zds, 100, 10), 0);
        let mut outbuf = vec![0u8; 4096];
        let mut inb = ZSTD_inBuffer {
            src: compressed.as_ptr() as *const core::ffi::c_void,
            size: compressed.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge
        );
        crate::streaming::ZSTD_freeDStream(zds);
    }
}

#[test]
fn stream_size_hints_match_upstream() {
    assert_eq!(ZSTD_CStreamInSize(), 128 * 1024);
    assert_eq!(ZSTD_DStreamInSize(), 128 * 1024 + 3);
    assert_eq!(ZSTD_DStreamOutSize(), 128 * 1024);
    assert_eq!(
        ZSTD_CStreamOutSize(),
        ZSTD_compressBound(128 * 1024) + 3 + 4
    );
}

#[test]
fn target_cblock_size_caps_emitted_blocks() {
    // 512 KiB of incompressible data would normally emit 4 full 128 KiB
    // blocks; with a 1340-byte target every physical block payload must be
    // at or under the target.
    let input = sample(512 * 1024);
    let cctx = ZSTD_createCCtx();
    let mut compressed = vec![0u8; ZSTD_compressBound(input.len()) + (input.len() / 1340 + 2) * 3];
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 130, 1340), 0); // targetCBlockSize
        let csize = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(csize), 0, "compress2 with target errored");
        ZSTD_freeCCtx(cctx);

        // Walk the frame's block headers: 3-byte LE `(size << 3) | (type
        // << 1) | last`; Raw/RLE regenerate `size` bytes, Compressed
        // blocks carry `size` payload bytes — all must be <= target.
        let mut zfh = core::mem::MaybeUninit::<ZSTD_FrameHeader>::zeroed();
        assert_eq!(
            ZSTD_getFrameHeader(zfh.as_mut_ptr(), compressed.as_ptr(), csize),
            0
        );
        let header_len = ZSTD_frameHeaderSize(compressed.as_ptr(), csize);
        assert_eq!(ZSTD_isError(header_len), 0);
        let mut pos = header_len;
        loop {
            let hdr =
                u32::from_le_bytes([compressed[pos], compressed[pos + 1], compressed[pos + 2], 0]);
            let last = hdr & 1 != 0;
            let block_type = (hdr >> 1) & 3;
            let size = (hdr >> 3) as usize;
            let payload = match block_type {
                1 => 1,    // RLE carries one byte
                _ => size, // Raw / Compressed carry `size` bytes
            };
            assert!(
                size <= 1340,
                "block at {pos} declares {size} bytes, above the 1340 target"
            );
            pos += 3 + payload;
            if last {
                break;
            }
        }
        ZSTD_freeDCtx(crate::context::ZSTD_createDCtx()); // keep symbol use balanced
    }
}

#[test]
fn dctx_parameter_changes_rejected_mid_frame() {
    // Compress two frames; feed only a prefix of the first so the decoder
    // parks mid-frame, then verify parameter mutation is rejected with
    // stage_wrong until the session is reset.
    let input = sample(256 * 1024);
    let cctx = ZSTD_createCCtx();
    let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
    let csize = unsafe {
        let csize = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        ZSTD_freeCCtx(cctx);
        csize
    };
    assert_eq!(ZSTD_isError(csize), 0);

    let zds = crate::streaming::ZSTD_createDStream();
    unsafe {
        // Between frames: parameter changes are accepted.
        assert_eq!(ZSTD_DCtx_setParameter(zds, 100, 25), 0);

        let mut outbuf = vec![0u8; 4096];
        let mut inb = ZSTD_inBuffer {
            src: compressed.as_ptr() as *const core::ffi::c_void,
            size: csize / 2, // truncated input: frame stays in flight
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb);
        assert_eq!(ZSTD_isError(rc), 0);
        assert_ne!(rc, 0, "frame must still be in flight");

        // Mid-frame: setParameter and a parameters-only reset are rejected.
        let rc = ZSTD_DCtx_setParameter(zds, 100, 26);
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_stage_wrong
        );
        let rc = crate::params::ZSTD_DCtx_reset(zds, 2); // ZSTD_reset_parameters
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_stage_wrong
        );

        // A session reset abandons the frame; parameters mutate again.
        assert_eq!(crate::params::ZSTD_DCtx_reset(zds, 1), 0);
        assert_eq!(ZSTD_DCtx_setParameter(zds, 100, 26), 0);
        crate::streaming::ZSTD_freeDStream(zds);
    }
}
