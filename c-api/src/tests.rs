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
fn create_cdict_rejects_non_dictionary() {
    // A buffer with no dictionary magic and no valid entropy is not a parseable
    // encoder dictionary; createCDict must return NULL (never crash).
    let garbage = [0xABu8; 64];
    let cdict = unsafe { ZSTD_createCDict(garbage.as_ptr(), garbage.len(), 3) };
    assert!(cdict.is_null(), "createCDict accepted a non-dictionary");
}
