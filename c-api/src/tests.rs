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
fn create_cdict_treats_unmagicked_bytes_as_raw_content() {
    // Upstream auto content type: a buffer without the dictionary magic is a
    // raw-content dictionary (ID 0), not a creation failure.
    let raw = [0xABu8; 64];
    let cdict = unsafe { ZSTD_createCDict(raw.as_ptr(), raw.len(), 3) };
    assert!(!cdict.is_null(), "raw-content createCDict must succeed");
    assert_eq!(unsafe { ZSTD_getDictID_fromCDict(cdict) }, 0);
    unsafe { ZSTD_freeCDict(cdict) };

    // An empty dictionary is a creation failure.
    let cdict = unsafe { ZSTD_createCDict(core::ptr::null(), 0, 3) };
    assert!(cdict.is_null(), "empty createCDict must fail");

    // A magic-prefixed blob whose tables don't parse is corrupt.
    let mut corrupt = vec![0x37u8, 0xA4, 0x30, 0xEC];
    corrupt.extend_from_slice(&[0xFF; 60]);
    let cdict = unsafe { ZSTD_createCDict(corrupt.as_ptr(), corrupt.len(), 3) };
    assert!(cdict.is_null(), "corrupt full dict must fail");
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
    assert!(!zcs.is_null(), "ZSTD_createCStream returned NULL");
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
    assert!(!zds.is_null(), "ZSTD_createDStream returned NULL");
    let mut restored: Vec<u8> = Vec::new();
    unsafe {
        assert_ne!(
            crate::streaming::ZSTD_initDStream(zds),
            0,
            "initDStream must return a non-zero recommended input size"
        );
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
            // The assertion checks the header's logical `Block_Size`, not
            // the physical payload: for RLE blocks the field is the repeat
            // count (regenerated bytes) while the body is a single byte.
            // `set_target_block_size` caps each block's uncompressed chunk,
            // so the logical size is the right thing to bound here.
            assert!(
                size <= 1340,
                "block at {pos} declares {size} bytes, above the 1340 target"
            );
            pos += 3 + payload;
            if last {
                break;
            }
        }
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

// Bug regression: ZSTD_compress2 on a context with a streaming frame in
// flight must be rejected with stage_wrong — starting a one-shot frame
// mid-stream would interleave two frame lifecycles on one context (and
// consume the pledge that belongs to the streaming frame).
#[test]
fn compress2_rejected_while_stream_in_flight() {
    let input = sample(64 * 1024);
    let zcs = crate::streaming::ZSTD_createCStream();
    let mut outbuf = vec![0u8; 1024];
    unsafe {
        // Open a streaming frame and leave it unfinished.
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr() as *const core::ffi::c_void,
            size: input.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0);
        assert_eq!(ZSTD_isError(rc), 0);

        let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
        let rc = crate::context::ZSTD_compress2(
            zcs,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_stage_wrong,
            "compress2 mid-stream must be rejected"
        );

        // After ending the stream the context accepts one-shots again.
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_endStream(zcs, &mut outb);
            assert_eq!(ZSTD_isError(rc), 0);
            if rc == 0 {
                break;
            }
        }
        let rc = crate::context::ZSTD_compress2(
            zcs,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(rc), 0, "compress2 must work between frames");
        crate::streaming::ZSTD_freeCStream(zcs);
    }
}

// Bug regression: a malformed frame header fed to ZSTD_decompressStream
// must surface a decode error, not the "need more input" hint — the hint
// with no input consumed sends a spec-conformant caller into a spin.
#[test]
fn decompress_stream_errors_on_malformed_header() {
    let zds = crate::streaming::ZSTD_createDStream();
    let garbage = [0xA5u8; 64]; // wrong magic, plenty of bytes
    let mut outbuf = vec![0u8; 4096];
    unsafe {
        let mut inb = ZSTD_inBuffer {
            src: garbage.as_ptr() as *const core::ffi::c_void,
            size: garbage.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = crate::streaming::ZSTD_decompressStream(zds, &mut outb, &mut inb);
        assert_ne!(
            ZSTD_isError(rc),
            0,
            "malformed header must error, not ask for more input (rc={rc})"
        );
        crate::streaming::ZSTD_freeDStream(zds);
    }
}

// Bug regression: the pledged source size must be enforced even when
// `ZSTD_c_contentSizeFlag = 0` omits the FCS field from the header.
// Upstream validates `consumedSrcSize` against the pledge at frame end
// regardless of the flag; only the header field is gated on it.
#[test]
fn streaming_pledge_enforced_when_fcs_flag_off() {
    let input = sample(4096);
    let mut outbuf = vec![0u8; ZSTD_CStreamOutSize()];

    // Undersized stream: pledge 4096, write 1000, end -> must error.
    let zcs = crate::streaming::ZSTD_createCStream();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(zcs, 200, 0), 0); // contentSizeFlag off
        assert_eq!(ZSTD_CCtx_setPledgedSrcSize(zcs, input.len() as u64), 0);
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr() as *const core::ffi::c_void,
            size: 1000,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2); // ZSTD_e_end
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_srcSize_wrong,
            "ending an undersized pledged frame must fail with srcSize_wrong even with contentSizeFlag=0"
        );
        crate::streaming::ZSTD_freeCStream(zcs);
    }

    // Exact-sized stream: succeeds AND the header carries no FCS field.
    let zcs = crate::streaming::ZSTD_createCStream();
    let mut compressed: Vec<u8> = Vec::new();
    unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(zcs, 200, 0), 0);
        assert_eq!(ZSTD_CCtx_setPledgedSrcSize(zcs, input.len() as u64), 0);
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr() as *const core::ffi::c_void,
            size: input.len(),
            pos: 0,
        };
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2);
            assert_eq!(ZSTD_isError(rc), 0, "exact pledged frame must succeed");
            compressed.extend_from_slice(&outbuf[..outb.pos]);
            if rc == 0 {
                break;
            }
        }
        let declared = ZSTD_getFrameContentSize(compressed.as_ptr(), compressed.len());
        assert_eq!(declared, u64::MAX, "FCS field must be omitted from header");
        crate::streaming::ZSTD_freeCStream(zcs);
    }
}

// Bug regression: when input overruns the pledged size, `inp.pos` must
// reflect the bytes the encoder actually consumed (up to the pledge)
// before the call errors — a desynced `pos` tells the C caller none of
// its input was taken when most of it was.
#[test]
fn streaming_input_pos_tracks_consumed_bytes_at_pledge_boundary() {
    let input = sample(1500);
    let pledged = 1000u64;
    let mut outbuf = vec![0u8; ZSTD_CStreamOutSize()];
    let zcs = crate::streaming::ZSTD_createCStream();
    unsafe {
        assert_eq!(ZSTD_CCtx_setPledgedSrcSize(zcs, pledged), 0);
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr() as *const core::ffi::c_void,
            size: input.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0);
        assert_ne!(ZSTD_isError(rc), 0, "overrunning the pledge must error");
        assert_eq!(
            inb.pos, pledged as usize,
            "pos must reflect the bytes consumed up to the pledge boundary"
        );
        crate::streaming::ZSTD_freeCStream(zcs);
    }
}

// Bug regression: ZSTD_decompressStream must verify the trailing XXH64
// content checksum like ZSTD_decompressDCtx does — a corrupted trailer
// has to surface ZSTD_error_checksum_wrong, not decode silently.
#[test]
fn streaming_decompress_rejects_corrupted_content_checksum() {
    let input = sample(64 * 1024);
    let cctx = ZSTD_createCCtx();
    let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
    let csize = unsafe {
        assert_eq!(ZSTD_CCtx_setParameter(cctx, 201, 1), 0); // checksum on
        let rc = crate::context::ZSTD_compress2(
            cctx,
            compressed.as_mut_ptr(),
            compressed.len(),
            input.as_ptr(),
            input.len(),
        );
        assert_eq!(ZSTD_isError(rc), 0);
        ZSTD_freeCCtx(cctx);
        rc
    };
    compressed.truncate(csize);
    // Flip a bit in the 4-byte checksum trailer.
    let last = compressed.len() - 1;
    compressed[last] ^= 0xFF;

    let zds = crate::streaming::ZSTD_createDStream();
    let mut outbuf = vec![0u8; input.len() + 4096];
    unsafe {
        let mut inb = ZSTD_inBuffer {
            src: compressed.as_ptr() as *const core::ffi::c_void,
            size: compressed.len(),
            pos: 0,
        };
        let mut rc;
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
            if ZSTD_isError(rc) != 0 || (rc == 0 && inb.pos == inb.size) {
                break;
            }
            assert!(
                outb.pos > 0 || inb.pos < inb.size,
                "no forward progress in decode loop"
            );
        }
        assert_eq!(
            ZSTD_getErrorCode(rc),
            ZSTD_ErrorCode::ZSTD_error_checksum_wrong,
            "corrupted checksum trailer must be rejected in streaming mode"
        );
        crate::streaming::ZSTD_freeDStream(zds);
    }
}

// Bug regression: a one-shot ZSTD_decompressDCtx on a context whose
// streaming decode was abandoned MID-FRAME must leave the context at a
// frame boundary; the next ZSTD_decompressStream call has to start the
// new frame instead of stalling on the finished decoder.
#[test]
fn decompress_stream_recovers_after_oneshot_on_midframe_context() {
    let input = sample(256 * 1024);
    let frame = compress_frame(&input);
    let zds = crate::streaming::ZSTD_createDStream();
    let mut outbuf = vec![0u8; input.len() + 4096];
    unsafe {
        // Feed a TRUNCATED frame so the stream parks mid-frame.
        let mut inb = ZSTD_inBuffer {
            src: frame.as_ptr() as *const core::ffi::c_void,
            size: frame.len() / 2,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
        assert_eq!(ZSTD_isError(rc), 0, "truncated feed must just ask for more");
        assert_ne!(rc, 0, "mid-frame stream must report more input needed");

        // One-shot decode of a COMPLETE frame on the same context.
        let mut once = vec![0u8; input.len()];
        let n = crate::context::ZSTD_decompressDCtx(
            zds,
            once.as_mut_ptr(),
            once.len(),
            frame.as_ptr(),
            frame.len(),
        );
        assert_eq!(ZSTD_isError(n), 0);
        assert_eq!(n, input.len());

        // Streaming decode of a fresh complete frame: the FIRST call must
        // already consume input (start the new frame). A context left
        // "mid-frame" by the one-shot instead burns a call returning 0
        // ("frame complete") without touching the fresh input.
        let mut inb = ZSTD_inBuffer {
            src: frame.as_ptr() as *const core::ffi::c_void,
            size: frame.len(),
            pos: 0,
        };
        let mut restored: Vec<u8> = Vec::new();
        let mut first_call = true;
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
            assert_eq!(ZSTD_isError(rc), 0, "post-oneshot stream decode errored");
            if first_call {
                assert!(
                    inb.pos > 0,
                    "first call after the one-shot must start the new frame, \
                     not report the previous frame complete (rc={rc})"
                );
                first_call = false;
            }
            restored.extend_from_slice(&outbuf[..outb.pos]);
            if rc == 0 && inb.pos == inb.size {
                break;
            }
            assert!(
                outb.pos > 0 || inb.pos < inb.size,
                "stream stalled: context stuck mid-frame after one-shot decode"
            );
        }
        assert_eq!(restored, input, "post-oneshot streaming must be byte-exact");
        crate::streaming::ZSTD_freeDStream(zds);
    }
}

// Bug regression: when the post-ZSTD_e_end tail is drained by a LATER
// call under a different directive (tiny output buffer forces the
// split), the finished stream state must still be dropped — otherwise
// the context wedges into "Some { encoder: None, pending: [] }" and
// every next input-bearing call fails with stage_wrong forever.
#[test]
fn stream_resets_after_tail_drained_by_non_end_directive() {
    let input = sample(256 * 1024);
    let zcs = crate::streaming::ZSTD_createCStream();
    let mut tiny = vec![0u8; 1024];
    unsafe {
        // Feed everything and finish with e_end into a TINY buffer so a
        // tail stays pending after the encoder is consumed.
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr() as *const core::ffi::c_void,
            size: input.len(),
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: tiny.as_mut_ptr() as *mut core::ffi::c_void,
            size: tiny.len(),
            pos: 0,
        };
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 2); // e_end
        assert_eq!(ZSTD_isError(rc), 0);
        assert_ne!(rc, 0, "tiny buffer must leave a pending tail");

        // Drain the remaining tail with e_flush (NOT e_end) calls.
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: tiny.as_mut_ptr() as *mut core::ffi::c_void,
                size: tiny.len(),
                pos: 0,
            };
            let mut empty = ZSTD_inBuffer {
                src: core::ptr::null(),
                size: 0,
                pos: 0,
            };
            let rc = ZSTD_compressStream2(zcs, &mut outb, &mut empty, 1); // e_flush
            assert_eq!(ZSTD_isError(rc), 0, "tail drain under e_flush errored");
            if rc == 0 {
                break;
            }
        }

        // The frame is complete and fully drained: a new frame must start.
        let mut inb = ZSTD_inBuffer {
            src: input.as_ptr() as *const core::ffi::c_void,
            size: 4096,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: tiny.as_mut_ptr() as *mut core::ffi::c_void,
            size: tiny.len(),
            pos: 0,
        };
        let rc = ZSTD_compressStream2(zcs, &mut outb, &mut inb, 0); // e_continue
        assert_eq!(
            ZSTD_isError(rc),
            0,
            "new frame after fully-drained end must start, not stage_wrong"
        );
        crate::streaming::ZSTD_freeCStream(zcs);
    }
}

// Bug regression: a FAILED one-shot decode on a context abandoned
// mid-stream must still leave the context at a frame boundary — the
// next ZSTD_decompressStream call has to start the new frame instead
// of resuming the failed one-shot's frame state.
#[test]
fn decompress_stream_recovers_after_failed_oneshot_on_midframe_context() {
    let input = sample(256 * 1024);
    let frame = compress_frame(&input);
    let zds = crate::streaming::ZSTD_createDStream();
    let mut outbuf = vec![0u8; input.len() + 4096];
    unsafe {
        // Park the stream mid-frame with a truncated feed.
        let mut inb = ZSTD_inBuffer {
            src: frame.as_ptr() as *const core::ffi::c_void,
            size: frame.len() / 2,
            pos: 0,
        };
        let mut outb = ZSTD_outBuffer {
            dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
            size: outbuf.len(),
            pos: 0,
        };
        let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
        assert_eq!(ZSTD_isError(rc), 0);
        assert_ne!(rc, 0, "mid-frame stream must report more input needed");

        // One-shot decode of a CORRUPT frame on the same context: fails.
        // Deterministic failure with maximal state mutation: a
        // checksum-bearing frame with a flipped trailer decodes ALL blocks
        // (the decoder state is fully exercised) and then reliably fails
        // verification — unlike flipping payload bytes, which a block
        // layout might happily decode into wrong output.
        let mut corrupt = {
            let mut enc: codec::encoding::FrameCompressor = codec::encoding::FrameCompressor::new(
                codec::encoding::CompressionLevel::from_level(3),
            );
            enc.set_content_checksum(true);
            enc.compress_independent_frame(&input)
        };
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        let mut once = vec![0u8; input.len()];
        let n = crate::context::ZSTD_decompressDCtx(
            zds,
            once.as_mut_ptr(),
            once.len(),
            corrupt.as_ptr(),
            corrupt.len(),
        );
        assert_ne!(ZSTD_isError(n), 0, "corrupt one-shot must fail");

        // Streaming decode of a fresh complete frame: the FIRST call must
        // consume input (start the new frame), not resume the failed
        // one-shot's frame state.
        let mut inb = ZSTD_inBuffer {
            src: frame.as_ptr() as *const core::ffi::c_void,
            size: frame.len(),
            pos: 0,
        };
        let mut restored: Vec<u8> = Vec::new();
        let mut first_call = true;
        loop {
            let mut outb = ZSTD_outBuffer {
                dst: outbuf.as_mut_ptr() as *mut core::ffi::c_void,
                size: outbuf.len(),
                pos: 0,
            };
            let rc = ZSTD_decompressStream(zds, &mut outb, &mut inb);
            assert_eq!(ZSTD_isError(rc), 0, "post-failed-oneshot stream errored");
            if first_call {
                assert!(
                    inb.pos > 0,
                    "first call after a failed one-shot must start the new frame (rc={rc})"
                );
                first_call = false;
            }
            restored.extend_from_slice(&outbuf[..outb.pos]);
            if rc == 0 && inb.pos == inb.size {
                break;
            }
            assert!(
                outb.pos > 0 || inb.pos < inb.size,
                "stream stalled after failed one-shot"
            );
        }
        assert_eq!(restored, input, "post-failure streaming must be byte-exact");
        crate::streaming::ZSTD_freeDStream(zds);
    }
}

// ---- Phase 6.2 slice 2: dictionary attach + estimates + fastCover ----

use crate::attach::{
    ZSTD_CCtx_loadDictionary, ZSTD_CCtx_loadDictionary_advanced,
    ZSTD_CCtx_loadDictionary_byReference, ZSTD_CCtx_refCDict, ZSTD_CCtx_refPrefix,
    ZSTD_CCtx_refPrefix_advanced, ZSTD_DCtx_loadDictionary, ZSTD_DCtx_loadDictionary_advanced,
    ZSTD_DCtx_loadDictionary_byReference, ZSTD_DCtx_refDDict, ZSTD_DCtx_refPrefix,
    ZSTD_DCtx_refPrefix_advanced, ZSTD_compress_usingDict, ZSTD_decompress_usingDict,
    ZSTD_getDictID_fromDict, ZSTD_getDictID_fromFrame,
};
use crate::cdict::{
    ZSTD_compress_usingCDict_advanced, ZSTD_compressionParameters, ZSTD_createCDict_advanced,
    ZSTD_createDDict_advanced, ZSTD_customMem, ZSTD_frameParameters,
};
use crate::context::ZSTD_compress2;
use crate::dict::{
    ZDICT_fastCover_params_t, ZDICT_finalizeDictionary, ZDICT_optimizeTrainFromBuffer_fastCover,
    ZDICT_params_t, ZDICT_trainFromBuffer_fastCover,
};
use crate::estimate::{
    ZSTD_estimateCCtxSize, ZSTD_estimateCCtxSize_usingCParams, ZSTD_estimateCStreamSize,
    ZSTD_estimateCStreamSize_usingCParams, ZSTD_estimateDCtxSize, ZSTD_estimateDStreamSize,
};

/// Dict-friendly payload: repeats material the trained dictionary covers.
fn dict_payload() -> Vec<u8> {
    let mut payload = Vec::new();
    for i in 0..40u32 {
        payload.extend_from_slice(
            format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n")
                .as_bytes(),
        );
    }
    payload
}

#[test]
fn cctx_load_dictionary_roundtrips_via_compress2() {
    let dict = trained_dictionary();
    let payload = dict_payload();
    let cctx = ZSTD_createCCtx();
    let n = unsafe { ZSTD_CCtx_loadDictionary(cctx, dict.as_ptr(), dict.len()) };
    assert_eq!(ZSTD_isError(n), 0);
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    frame.truncate(written);

    // The frame carries the dictionary ID and cannot decode without the dict.
    let dict_id = unsafe { ZSTD_getDictID_fromDict(dict.as_ptr(), dict.len()) };
    assert_ne!(dict_id, 0);
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()) },
        dict_id
    );
    let dctx = ZSTD_createDCtx();
    let mut out = vec![0u8; payload.len() + 64];
    let plain = unsafe {
        ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        )
    };
    assert_ne!(ZSTD_isError(plain), 0, "dict frame must not decode bare");

    // With the dictionary loaded on the DCtx it decodes byte-exact.
    let n = unsafe { ZSTD_DCtx_loadDictionary(dctx, dict.as_ptr(), dict.len()) };
    assert_eq!(ZSTD_isError(n), 0);
    let read = unsafe {
        ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());

    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn raw_content_dictionary_roundtrips_without_wire_id() {
    // Raw bytes (no magic) attach as raw content; the frame must NOT carry
    // the synthetic dictionary ID and must decode with the same raw bytes
    // loaded on the decode side.
    let raw: Vec<u8> = dict_payload();
    let mut payload = raw[..1024].to_vec();
    payload.extend_from_slice(b"and a unique tail 0123456789");

    let cctx = ZSTD_createCCtx();
    let n = unsafe { ZSTD_CCtx_loadDictionary(cctx, raw.as_ptr(), raw.len()) };
    assert_eq!(ZSTD_isError(n), 0);
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    frame.truncate(written);
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()) },
        0,
        "raw-content frame must not advertise a dictionary ID"
    );

    let dctx = ZSTD_createDCtx();
    let n = unsafe { ZSTD_DCtx_loadDictionary(dctx, raw.as_ptr(), raw.len()) };
    assert_eq!(ZSTD_isError(n), 0);
    let mut out = vec![0u8; payload.len() + 64];
    let read = unsafe {
        ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn ref_cdict_and_ref_ddict_roundtrip_via_contexts() {
    let dict = trained_dictionary();
    let payload = dict_payload();
    let cdict = unsafe { ZSTD_createCDict(dict.as_ptr(), dict.len(), 7) };
    assert!(!cdict.is_null());
    let ddict = unsafe { ZSTD_createDDict(dict.as_ptr(), dict.len()) };
    assert!(!ddict.is_null());

    let cctx = ZSTD_createCCtx();
    assert_eq!(ZSTD_isError(unsafe { ZSTD_CCtx_refCDict(cctx, cdict) }), 0);
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    frame.truncate(written);

    let dctx = ZSTD_createDCtx();
    assert_eq!(ZSTD_isError(unsafe { ZSTD_DCtx_refDDict(dctx, ddict) }), 0);
    let mut out = vec![0u8; payload.len() + 64];
    let read = unsafe {
        ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());

    // Detach with NULL: the next frame must be dictionary-independent.
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_CCtx_refCDict(cctx, core::ptr::null()) }),
        0
    );
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.capacity(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    let bare = ZSTD_createDCtx();
    let read =
        unsafe { ZSTD_decompressDCtx(bare, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(ZSTD_isError(read), 0, "detached frame must decode bare");
    unsafe { ZSTD_freeDCtx(bare) };

    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
    unsafe { ZSTD_freeCDict(cdict) };
    unsafe { ZSTD_freeDDict(ddict) };
}

#[test]
fn ref_prefix_is_single_use_and_roundtrips() {
    let prefix = dict_payload();
    let mut payload = prefix[..2048].to_vec();
    payload.extend_from_slice(b"unique suffix after prefix material 0123456789");

    let cctx = ZSTD_createCCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr(), prefix.len()) }),
        0
    );
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    frame.truncate(written);
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()) },
        0
    );

    // Single-use: the next frame must not depend on the prefix.
    let mut frame2 = vec![0u8; payload.len() + 512];
    let written2 = unsafe {
        ZSTD_compress2(
            cctx,
            frame2.as_mut_ptr(),
            frame2.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written2), 0);
    let bare = ZSTD_createDCtx();
    let mut out = vec![0u8; payload.len() + 64];
    let read = unsafe {
        ZSTD_decompressDCtx(bare, out.as_mut_ptr(), out.len(), frame2.as_ptr(), written2)
    };
    assert_eq!(ZSTD_isError(read), 0, "post-prefix frame must decode bare");
    assert_eq!(&out[..read], payload.as_slice());
    unsafe { ZSTD_freeDCtx(bare) };

    // The prefixed frame decodes only with the same prefix referenced.
    let dctx = ZSTD_createDCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_DCtx_refPrefix(dctx, prefix.as_ptr(), prefix.len()) }),
        0
    );
    let read = unsafe {
        ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn cctx_ref_prefix_survives_a_too_small_destination() {
    // A prefix is single-use, but "use" means a frame was actually
    // DELIVERED: a dstSize_tooSmall failure must leave the prefix armed so
    // the caller's retry with a bigger buffer still compresses against it.
    let prefix = dict_payload();
    let mut payload = prefix[..2048].to_vec();
    payload.extend_from_slice(b"unique suffix after prefix material 0123456789");

    let cctx = ZSTD_createCCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr(), prefix.len()) }),
        0
    );
    let mut tiny = [0u8; 4];
    let failed = unsafe {
        ZSTD_compress2(
            cctx,
            tiny.as_mut_ptr(),
            tiny.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_ne!(ZSTD_isError(failed), 0, "4-byte destination must fail");

    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    frame.truncate(written);

    // The retried frame must still depend on the prefix: bare decode fails,
    // prefixed decode roundtrips.
    let bare = ZSTD_createDCtx();
    let mut out = vec![0u8; payload.len() + 64];
    let read =
        unsafe { ZSTD_decompressDCtx(bare, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_ne!(
        ZSTD_isError(read),
        0,
        "the retry must still compress against the prefix"
    );
    unsafe { ZSTD_freeDCtx(bare) };

    let dctx = ZSTD_createDCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_DCtx_refPrefix(dctx, prefix.as_ptr(), prefix.len()) }),
        0
    );
    let read =
        unsafe { ZSTD_decompressDCtx(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn streaming_roundtrips_with_loaded_dictionary() {
    let dict = trained_dictionary();
    let payload = dict_payload();

    let cctx = ZSTD_createCCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_CCtx_loadDictionary(cctx, dict.as_ptr(), dict.len()) }),
        0
    );
    let mut frame = vec![0u8; payload.len() + 1024];
    let mut inb = ZSTD_inBuffer {
        src: payload.as_ptr().cast(),
        size: payload.len(),
        pos: 0,
    };
    let mut outb = ZSTD_outBuffer {
        dst: frame.as_mut_ptr().cast(),
        size: frame.len(),
        pos: 0,
    };
    let rc = unsafe {
        ZSTD_compressStream2(cctx, &mut outb, &mut inb, 2 /* ZSTD_e_end */)
    };
    assert_eq!(ZSTD_isError(rc), 0);
    assert_eq!(rc, 0, "single-shot end must finish in one call");
    frame.truncate(outb.pos);

    let dict_id = unsafe { ZSTD_getDictID_fromDict(dict.as_ptr(), dict.len()) };
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()) },
        dict_id
    );

    let dctx = ZSTD_createDCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_DCtx_loadDictionary(dctx, dict.as_ptr(), dict.len()) }),
        0
    );
    let mut out = vec![0u8; payload.len() + 64];
    let mut inb = ZSTD_inBuffer {
        src: frame.as_ptr().cast(),
        size: frame.len(),
        pos: 0,
    };
    let mut outb = ZSTD_outBuffer {
        dst: out.as_mut_ptr().cast(),
        size: out.len(),
        pos: 0,
    };
    let rc = unsafe { ZSTD_decompressStream(dctx, &mut outb, &mut inb) };
    assert_eq!(ZSTD_isError(rc), 0);
    assert_eq!(rc, 0, "whole frame supplied; decode must finish");
    assert_eq!(&out[..outb.pos], payload.as_slice());
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn using_dict_one_shots_roundtrip() {
    let dict = trained_dictionary();
    let payload = dict_payload();
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress_usingDict(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
            dict.as_ptr(),
            dict.len(),
            5,
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    let mut out = vec![0u8; payload.len() + 64];
    let read = unsafe {
        ZSTD_decompress_usingDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            written,
            dict.as_ptr(),
            dict.len(),
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());

    // Empty dictionary degrades to the plain one-shots.
    let written = unsafe {
        ZSTD_compress_usingDict(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
            core::ptr::null(),
            0,
            5,
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    let read = unsafe {
        ZSTD_decompress_usingDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            written,
            core::ptr::null(),
            0,
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn cdict_advanced_honours_cparams_and_fparams() {
    let raw = dict_payload();
    let mut payload = raw[..1024].to_vec();
    payload.extend_from_slice(b"advanced unique tail 0123456789");
    let no_mem = ZSTD_customMem {
        customAlloc: core::ptr::null(),
        customFree: core::ptr::null(),
        opaque: core::ptr::null(),
    };
    let cparams = ZSTD_compressionParameters {
        windowLog: 0,
        chainLog: 0,
        hashLog: 0,
        searchLog: 0,
        minMatch: 0,
        targetLength: 0,
        strategy: 2, // dfast
    };
    let cdict = unsafe {
        ZSTD_createCDict_advanced(
            raw.as_ptr(),
            raw.len(),
            0,
            1, // rawContent
            cparams,
            no_mem,
        )
    };
    assert!(!cdict.is_null());
    let ddict = unsafe { ZSTD_createDDict_advanced(raw.as_ptr(), raw.len(), 0, 1, no_mem) };
    assert!(!ddict.is_null());

    let cctx = ZSTD_createCCtx();
    let fparams = ZSTD_frameParameters {
        contentSizeFlag: 1,
        checksumFlag: 1,
        noDictIDFlag: 1,
    };
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress_usingCDict_advanced(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
            cdict,
            fparams,
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    frame.truncate(written);
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), frame.len()) },
        0
    );

    let dctx = ZSTD_createDCtx();
    let mut out = vec![0u8; payload.len() + 64];
    let read = unsafe {
        ZSTD_decompress_usingDDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            frame.len(),
            ddict,
        )
    };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());

    // A non-NULL custom allocator is unsupported and must fail creation.
    let bad_mem = ZSTD_customMem {
        customAlloc: ZSTD_createCCtx as *const core::ffi::c_void,
        customFree: core::ptr::null(),
        opaque: core::ptr::null(),
    };
    let rejected =
        unsafe { ZSTD_createCDict_advanced(raw.as_ptr(), raw.len(), 0, 1, cparams, bad_mem) };
    assert!(rejected.is_null());

    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
    unsafe { ZSTD_freeCDict(cdict) };
    unsafe { ZSTD_freeDDict(ddict) };
}

#[test]
fn estimates_are_sane_budgets() {
    let l3 = ZSTD_estimateCCtxSize(3);
    let l19 = ZSTD_estimateCCtxSize(19);
    assert!(l3 > 1 << 19, "estimate must cover at least the window");
    assert!(l19 > l3, "higher level must budget more");
    assert!(ZSTD_estimateCStreamSize(3) > l3);
    let dctx = ZSTD_estimateDCtxSize();
    assert!(dctx > core::mem::size_of::<crate::context::ZSTD_DCtx>());
    assert!(ZSTD_estimateDStreamSize(1 << 20) > dctx + (1 << 20));
    let cparams = ZSTD_compressionParameters {
        windowLog: 20,
        chainLog: 17,
        hashLog: 17,
        searchLog: 1,
        minMatch: 5,
        targetLength: 0,
        strategy: 2,
    };
    let one_shot = ZSTD_estimateCCtxSize_usingCParams(cparams);
    assert!(one_shot > (1 << 20) + 2 * (4 << 17));
    // Binary-tree strategies must budget the retained optimal-parser
    // workspace on top of the same table logs.
    let bt_cparams = ZSTD_compressionParameters {
        strategy: 8,
        ..cparams
    };
    assert!(
        ZSTD_estimateCCtxSize_usingCParams(bt_cparams) > one_shot,
        "btultra cParams must budget more than dfast at equal logs"
    );
    // Strategy ordinal is validated like the other cParams bounds.
    let bad_strategy = ZSTD_compressionParameters {
        strategy: 42,
        ..cparams
    };
    assert_ne!(
        crate::error::ZSTD_isError(ZSTD_estimateCCtxSize_usingCParams(bad_strategy)),
        0
    );
    assert!(
        ZSTD_estimateCStreamSize_usingCParams(cparams) > one_shot,
        "streaming must budget more than the one-shot"
    );
    // Invalid parameters propagate the encoded error through the stream
    // variant too.
    let bad = ZSTD_compressionParameters {
        windowLog: 60,
        ..cparams
    };
    assert_ne!(
        crate::error::ZSTD_isError(ZSTD_estimateCStreamSize_usingCParams(bad)),
        0
    );
}

#[test]
fn zdict_fastcover_trains_usable_dictionary() {
    let mut samples: Vec<u8> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for i in 0..512u32 {
        let s = format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n");
        sizes.push(s.len());
        samples.extend_from_slice(s.as_bytes());
    }
    let mut dict = vec![0u8; 64 * 1024];
    let mut params = ZDICT_fastCover_params_t {
        k: 256,
        d: 8,
        f: 20,
        steps: 0,
        nbThreads: 0,
        splitPoint: 0.0,
        accel: 1,
        shrinkDict: 0,
        shrinkDictMaxRegression: 0,
        zParams: ZDICT_params_t {
            compressionLevel: 0,
            notificationLevel: 0,
            dictID: 0,
        },
    };
    let n = unsafe {
        ZDICT_trainFromBuffer_fastCover(
            dict.as_mut_ptr(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    assert_eq!(ZDICT_isError(n), 0, "fastCover training failed");
    assert_ne!(
        unsafe { ZSTD_getDictID_fromDict(dict.as_ptr(), n) },
        0,
        "trained dictionary must carry an ID"
    );

    // The optimizing entry sweeps when k/d are 0 and writes back the choice.
    params.k = 0;
    params.d = 0;
    let n = unsafe {
        ZDICT_optimizeTrainFromBuffer_fastCover(
            dict.as_mut_ptr(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
            &mut params,
        )
    };
    assert_eq!(ZDICT_isError(n), 0, "optimize fastCover failed");
    assert_ne!(params.k, 0, "optimize must write back the chosen k");
    assert_ne!(params.d, 0, "optimize must write back the chosen d");
}

#[test]
fn dict_compressor_cache_drops_stale_target_block_size() {
    // The cached dict-compressor is keyed by attach serial + level; frame
    // FLAGS are re-applied per call, and a target-block-size cap set on one
    // call must not leak into a later call that reset the parameter to 0.
    let dict = trained_dictionary();
    let payload = dict_payload();
    let cctx = ZSTD_createCCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_CCtx_loadDictionary(cctx, dict.as_ptr(), dict.len()) }),
        0
    );
    let mut reference = vec![0u8; payload.len() + 512];
    let ref_len = unsafe {
        ZSTD_compress2(
            cctx,
            reference.as_mut_ptr(),
            reference.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(ref_len), 0);

    // Cap the block size for one frame, then reset the knob to 0 (auto).
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_setParameter(cctx, crate::params::ZSTD_C_TARGET_CBLOCK_SIZE, 1536)
        }),
        0
    );
    let mut capped = vec![0u8; payload.len() + 1024];
    let capped_len = unsafe {
        ZSTD_compress2(
            cctx,
            capped.as_mut_ptr(),
            capped.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(capped_len), 0);
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_setParameter(cctx, crate::params::ZSTD_C_TARGET_CBLOCK_SIZE, 0)
        }),
        0
    );
    let mut after = vec![0u8; payload.len() + 512];
    let after_len = unsafe {
        ZSTD_compress2(
            cctx,
            after.as_mut_ptr(),
            after.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(after_len), 0);
    assert_eq!(
        &after[..after_len],
        &reference[..ref_len],
        "resetting targetCBlockSize to 0 must restore the uncapped frame"
    );
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn by_reference_and_advanced_attach_variants_roundtrip() {
    // The byReference / _advanced wrappers share the load body; exercise
    // each entry point end-to-end so a signature or routing regression in
    // any of them fails loudly.
    let dict = trained_dictionary();
    let payload = dict_payload();
    let dict_id = unsafe { ZSTD_getDictID_fromDict(dict.as_ptr(), dict.len()) };

    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut frame = vec![0u8; payload.len() + 512];
    let mut out = vec![0u8; payload.len() + 64];

    // byReference pair.
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_loadDictionary_byReference(cctx, dict.as_ptr(), dict.len())
        }),
        0
    );
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), written) },
        dict_id
    );
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_DCtx_loadDictionary_byReference(dctx, dict.as_ptr(), dict.len())
        }),
        0
    );
    let read =
        unsafe { ZSTD_decompressDCtx(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());

    // _advanced pair with an explicit fullDict content type. Fresh contexts:
    // reusing the by-reference-attached ones above would mask an _advanced
    // wrapper regressing into a no-op (the earlier attach is still live).
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_loadDictionary_advanced(cctx, dict.as_ptr(), dict.len(), 0, 2)
        }),
        0
    );
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_DCtx_loadDictionary_advanced(dctx, dict.as_ptr(), dict.len(), 0, 2)
        }),
        0
    );
    let read =
        unsafe { ZSTD_decompressDCtx(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());
    // The dict frame must NOT decode on a bare context — proves the
    // _advanced attach (not a leftover) carried the round-trip above.
    let bare = ZSTD_createDCtx();
    let bare_read =
        unsafe { ZSTD_decompressDCtx(bare, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_ne!(
        ZSTD_isError(bare_read),
        0,
        "dict frame decoding bare means the _advanced attach was not exercised"
    );
    unsafe { ZSTD_freeDCtx(bare) };

    // fullDict selector on raw bytes must be rejected on both sides.
    let raw = [0xCDu8; 64];
    assert_ne!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_loadDictionary_advanced(cctx, raw.as_ptr(), raw.len(), 0, 2)
        }),
        0
    );
    assert_ne!(
        ZSTD_isError(unsafe {
            ZSTD_DCtx_loadDictionary_advanced(dctx, raw.as_ptr(), raw.len(), 0, 2)
        }),
        0
    );

    // refPrefix_advanced: rawContent accepted + single-use roundtrip,
    // fullDict rejected — on both the encoder and decoder sides. Fresh
    // contexts: the earlier loadDictionary attaches are sticky and could
    // mask a refPrefix wrapper regressing to a no-op.
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let prefix = dict_payload();
    let mut p2 = prefix[..1024].to_vec();
    p2.extend_from_slice(b"advanced prefix tail 0123456789");
    assert_ne!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_refPrefix_advanced(cctx, prefix.as_ptr(), prefix.len(), 2)
        }),
        0,
        "fullDict selector must be rejected for an encoder prefix"
    );
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_refPrefix_advanced(cctx, prefix.as_ptr(), prefix.len(), 1)
        }),
        0
    );
    let written =
        unsafe { ZSTD_compress2(cctx, frame.as_mut_ptr(), frame.len(), p2.as_ptr(), p2.len()) };
    assert_eq!(ZSTD_isError(written), 0);
    // The frame must really depend on the prefix: no advertised ID and no
    // bare decode.
    assert_eq!(
        unsafe { ZSTD_getDictID_fromFrame(frame.as_ptr(), written) },
        0,
        "prefix frame must not advertise a dictionary ID"
    );
    let bare = ZSTD_createDCtx();
    let bare_read =
        unsafe { ZSTD_decompressDCtx(bare, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_ne!(
        ZSTD_isError(bare_read),
        0,
        "prefix frame must not decode bare"
    );
    unsafe { ZSTD_freeDCtx(bare) };
    assert_ne!(
        ZSTD_isError(unsafe {
            ZSTD_DCtx_refPrefix_advanced(dctx, prefix.as_ptr(), prefix.len(), 2)
        }),
        0,
        "fullDict selector must be rejected for a prefix"
    );
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_DCtx_refPrefix_advanced(dctx, prefix.as_ptr(), prefix.len(), 1)
        }),
        0
    );
    let read =
        unsafe { ZSTD_decompressDCtx(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], p2.as_slice());

    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn dctx_ref_prefix_survives_a_failed_one_shot() {
    // A referenced prefix is single-use, but "use" means a frame actually
    // started with it — the streaming path consumes it only after a
    // successful reset. A one-shot that fails before any frame starts
    // (garbage input here) must leave the prefix attached so the retry
    // with the real frame still decodes.
    let prefix = dict_payload();
    let mut payload = prefix[..1024].to_vec();
    payload.extend_from_slice(b"prefix retry tail 0123456789");

    let cctx = ZSTD_createCCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_CCtx_refPrefix(cctx, prefix.as_ptr(), prefix.len()) }),
        0
    );
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);

    let dctx = ZSTD_createDCtx();
    assert_eq!(
        ZSTD_isError(unsafe { ZSTD_DCtx_refPrefix(dctx, prefix.as_ptr(), prefix.len()) }),
        0
    );
    let mut out = vec![0u8; payload.len() + 64];
    let garbage = [0u8; 24];
    let failed = unsafe {
        ZSTD_decompressDCtx(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            garbage.as_ptr(),
            garbage.len(),
        )
    };
    assert_ne!(ZSTD_isError(failed), 0, "garbage must fail to decode");
    let read =
        unsafe { ZSTD_decompressDCtx(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(
        ZSTD_isError(read),
        0,
        "the prefix must survive the failed one-shot"
    );
    assert_eq!(&out[..read], payload.as_slice());
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeCCtx(cctx) };
}

#[test]
fn creation_time_validation_rejects_bad_fastcover_and_full_dict_inputs() {
    // Plain (non-optimizing) fastCover training requires explicit k and d:
    // upstream rejects 0 with parameter_outOfBound instead of silently
    // substituting defaults.
    let mut samples: Vec<u8> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for i in 0..64u32 {
        let s = format!("sample line {i} with shared structure\n");
        sizes.push(s.len());
        samples.extend_from_slice(s.as_bytes());
    }
    let mut dict = vec![0u8; 16 * 1024];
    let params = ZDICT_fastCover_params_t {
        k: 0,
        d: 0,
        f: 20,
        steps: 0,
        nbThreads: 0,
        splitPoint: 0.0,
        accel: 1,
        shrinkDict: 0,
        shrinkDictMaxRegression: 0,
        zParams: ZDICT_params_t {
            compressionLevel: 0,
            notificationLevel: 0,
            dictID: 0,
        },
    };
    let n = unsafe {
        ZDICT_trainFromBuffer_fastCover(
            dict.as_mut_ptr(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    assert_ne!(
        ZDICT_isError(n),
        0,
        "plain fastCover must reject k == 0 / d == 0"
    );

    // FULL_DICT bytes without the dictionary magic must fail DDict creation
    // (the CDict creator and the loadDictionary paths already do).
    let raw = [0xEEu8; 64];
    let no_mem = ZSTD_customMem {
        customAlloc: core::ptr::null(),
        customFree: core::ptr::null(),
        opaque: core::ptr::null(),
    };
    let ddict = unsafe { ZSTD_createDDict_advanced(raw.as_ptr(), raw.len(), 0, 2, no_mem) };
    assert!(
        ddict.is_null(),
        "FULL_DICT without the dictionary magic must fail at creation"
    );
}

#[test]
fn ref_ddict_honours_raw_content_selection_for_magic_prefixed_bytes() {
    // A DDict created with the explicit rawContent selector must stay raw
    // content at use time even when its bytes happen to start with the
    // dictionary magic — re-classifying on the magic would reject the
    // (perfectly valid) raw bytes as a corrupt serialized dictionary.
    let mut raw = vec![0x37u8, 0xA4, 0x30, 0xEC];
    raw.extend_from_slice(b"raw content that merely starts with the dictionary magic 0123456789");
    let no_mem = ZSTD_customMem {
        customAlloc: core::ptr::null(),
        customFree: core::ptr::null(),
        opaque: core::ptr::null(),
    };
    let ddict = unsafe { ZSTD_createDDict_advanced(raw.as_ptr(), raw.len(), 0, 1, no_mem) };
    assert!(!ddict.is_null(), "rawContent DDict creation must succeed");

    let dctx = ZSTD_createDCtx();
    let attached = unsafe { ZSTD_DCtx_refDDict(dctx, ddict) };
    assert_eq!(
        ZSTD_isError(attached),
        0,
        "refDDict must honour the DDict's rawContent selection"
    );

    // Functional cross-check: a frame compressed against the same raw bytes
    // decodes through the referenced DDict.
    let mut payload = raw.clone();
    payload.extend_from_slice(b"payload tail referencing the raw dict 9876543210");
    let cctx = ZSTD_createCCtx();
    assert_eq!(
        ZSTD_isError(unsafe {
            ZSTD_CCtx_loadDictionary_advanced(cctx, raw.as_ptr(), raw.len(), 0, 1)
        }),
        0
    );
    let mut frame = vec![0u8; payload.len() + 512];
    let written = unsafe {
        ZSTD_compress2(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            payload.as_ptr(),
            payload.len(),
        )
    };
    assert_eq!(ZSTD_isError(written), 0);
    let mut out = vec![0u8; payload.len() + 64];
    let read =
        unsafe { ZSTD_decompressDCtx(dctx, out.as_mut_ptr(), out.len(), frame.as_ptr(), written) };
    assert_eq!(ZSTD_isError(read), 0);
    assert_eq!(&out[..read], payload.as_slice());

    unsafe { ZSTD_freeCCtx(cctx) };
    unsafe { ZSTD_freeDCtx(dctx) };
    unsafe { ZSTD_freeDDict(ddict) };
}

#[test]
fn cdict_advanced_rejects_invalid_strategy_ordinal() {
    // ZSTD_strategy spans 1 (fast) … 9 (btultra2); 0 and out-of-range
    // ordinals are invalid input and must fail creation, not silently map
    // onto the strongest tier.
    let raw = [0xABu8; 256];
    let no_mem = ZSTD_customMem {
        customAlloc: core::ptr::null(),
        customFree: core::ptr::null(),
        opaque: core::ptr::null(),
    };
    let base = ZSTD_compressionParameters {
        windowLog: 0,
        chainLog: 0,
        hashLog: 0,
        searchLog: 0,
        minMatch: 0,
        targetLength: 0,
        strategy: 0,
    };
    for bad in [0u32, 10, 42] {
        let cparams = ZSTD_compressionParameters {
            strategy: bad,
            ..base
        };
        let cdict =
            unsafe { ZSTD_createCDict_advanced(raw.as_ptr(), raw.len(), 0, 1, cparams, no_mem) };
        assert!(
            cdict.is_null(),
            "strategy ordinal {bad} must fail CDict creation"
        );
    }
}

#[test]
fn cparams_estimate_never_underreports_on_32bit() {
    // windowLog=30 + hashLog=29 + chainLog=29 each pass the per-field
    // bounds, but their byte sizes sum past u32::MAX: on a 32-bit target
    // the plain addition wraps and the estimate UNDER-reports — worse than
    // an error for a sizing contract. The sum must come back as either an
    // encoded error or a figure that covers at least the window alone.
    let cparams = ZSTD_compressionParameters {
        windowLog: 30,
        chainLog: 29,
        hashLog: 29,
        searchLog: 1,
        minMatch: 5,
        targetLength: 0,
        strategy: 1,
    };
    let n = ZSTD_estimateCCtxSize_usingCParams(cparams);
    assert!(
        crate::error::ZSTD_isError(n) != 0 || n >= (1usize << 30),
        "estimate must error or cover the window, got {n}"
    );
}

#[test]
fn zdict_train_rejects_null_buffers_with_nonzero_lengths() {
    // A NULL buffer paired with a non-zero length must come back as an
    // encoded error, never reach slice construction: these are documented
    // error-returning entry points, not UB traps.
    let mut samples: Vec<u8> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    for i in 0..64u32 {
        let s = format!("sample line {i} with shared structure\n");
        sizes.push(s.len());
        samples.extend_from_slice(s.as_bytes());
    }
    let mut dict = vec![0u8; 16 * 1024];
    let params = ZDICT_fastCover_params_t {
        k: 200,
        d: 8,
        f: 20,
        steps: 0,
        nbThreads: 0,
        splitPoint: 0.0,
        accel: 1,
        shrinkDict: 0,
        shrinkDictMaxRegression: 0,
        zParams: ZDICT_params_t {
            compressionLevel: 0,
            notificationLevel: 0,
            dictID: 0,
        },
    };

    // NULL samples buffer while the sizes sum to a non-zero total.
    let n = unsafe {
        ZDICT_trainFromBuffer(
            dict.as_mut_ptr(),
            dict.len(),
            core::ptr::null(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    assert_ne!(ZDICT_isError(n), 0, "NULL samplesBuffer must error");

    let n = unsafe {
        ZDICT_trainFromBuffer_fastCover(
            dict.as_mut_ptr(),
            dict.len(),
            core::ptr::null(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    assert_ne!(
        ZDICT_isError(n),
        0,
        "NULL samplesBuffer must error (fastCover)"
    );

    // NULL destination buffer with a non-zero capacity.
    let n = unsafe {
        ZDICT_trainFromBuffer(
            core::ptr::null_mut(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    assert_ne!(ZDICT_isError(n), 0, "NULL dictBuffer must error");

    // NULL raw content with a non-zero length in the finalizer.
    let zparams = ZDICT_params_t {
        compressionLevel: 0,
        notificationLevel: 0,
        dictID: 0,
    };
    let n = unsafe {
        ZDICT_finalizeDictionary(
            dict.as_mut_ptr(),
            dict.len(),
            core::ptr::null(),
            1024,
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
            zparams,
        )
    };
    assert_ne!(ZDICT_isError(n), 0, "NULL dictContent must error");
}
