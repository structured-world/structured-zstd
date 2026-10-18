//! Thread pools, static workspaces, and the dictionary builders outside the
//! stable `zdict.h` section.

use super::experimental::code;
use crate::attach::{ZSTD_compress_usingDict, ZSTD_decompress_usingDict};
use crate::context::{ZSTD_createCCtx, ZSTD_createDCtx, ZSTD_freeCCtx, ZSTD_freeDCtx};
use crate::dict::{
    ZDICT_addEntropyTablesFromBuffer, ZDICT_getDictHeaderSize, ZDICT_getDictID, ZDICT_isError,
    ZDICT_legacy_params_t, ZDICT_params_t, ZDICT_trainFromBuffer_legacy,
};
use crate::error::{ZSTD_ErrorCode, ZSTD_isError};
use crate::provided::{
    ZSTD_CCtx_refThreadPool, ZSTD_createThreadPool, ZSTD_freeThreadPool, ZSTD_initStaticCCtx,
    ZSTD_initStaticCDict, ZSTD_initStaticCStream, ZSTD_initStaticDCtx, ZSTD_initStaticDDict,
    ZSTD_initStaticDStream,
};
use crate::simple::ZSTD_compressBound;
use crate::streaming::{ZSTD_compressStream2, ZSTD_inBuffer, ZSTD_outBuffer};

/// Lines that repeat their structure: what a dictionary is trained on.
fn samples() -> (Vec<u8>, Vec<usize>) {
    let mut samples = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..400u32 {
        let line = format!("tenant=demo table=orders key={i} region=eu status=shipped\n");
        sizes.push(line.len());
        samples.extend_from_slice(line.as_bytes());
    }
    (samples, sizes)
}

/// Compress `input` with `dict` and decode it back with the same bytes.
fn round_trip_with(dict: &[u8], input: &[u8]) {
    let cctx = ZSTD_createCCtx();
    let dctx = ZSTD_createDCtx();
    let mut frame = vec![0u8; ZSTD_compressBound(input.len())];
    let n = unsafe {
        ZSTD_compress_usingDict(
            cctx,
            frame.as_mut_ptr(),
            frame.len(),
            input.as_ptr(),
            input.len(),
            dict.as_ptr(),
            dict.len(),
            3,
        )
    };
    assert_eq!(ZSTD_isError(n), 0, "{:?}", code(n));
    let mut out = vec![0u8; input.len()];
    let m = unsafe {
        ZSTD_decompress_usingDict(
            dctx,
            out.as_mut_ptr(),
            out.len(),
            frame.as_ptr(),
            n,
            dict.as_ptr(),
            dict.len(),
        )
    };
    assert_eq!(m, input.len(), "{:?}", code(m));
    assert_eq!(out, input);
    unsafe {
        ZSTD_freeDCtx(dctx);
        ZSTD_freeCCtx(cctx);
    }
}

/// The pool is never `NULL`, a context takes it between frames and refuses
/// it within one (`stage_wrong`), and freeing it is harmless.
#[test]
fn thread_pool_is_accepted_between_frames() {
    let pool = ZSTD_createThreadPool(4);
    assert!(!pool.is_null());
    let cctx = ZSTD_createCCtx();
    assert_eq!(unsafe { ZSTD_CCtx_refThreadPool(cctx, pool) }, 0);
    let input = [7u8; 100];
    let mut frame = vec![0u8; 256];
    let mut out = ZSTD_outBuffer {
        dst: frame.as_mut_ptr().cast(),
        size: frame.len(),
        pos: 0,
    };
    let mut inp = ZSTD_inBuffer {
        src: input.as_ptr().cast(),
        size: input.len(),
        pos: 0,
    };
    unsafe { ZSTD_compressStream2(cctx, &mut out, &mut inp, 0) };
    assert_eq!(
        code(unsafe { ZSTD_CCtx_refThreadPool(cctx, pool) }),
        ZSTD_ErrorCode::ZSTD_error_stage_wrong
    );
    ZSTD_freeThreadPool(pool);
    unsafe { ZSTD_freeCCtx(cctx) };
}

/// Every static constructor answers `NULL`, the value upstream gives for a
/// workspace it cannot build in.
#[test]
fn static_constructors_answer_null() {
    let mut workspace = vec![0u64; 1 << 16];
    let ws = workspace.as_mut_ptr().cast();
    let size = workspace.len() * 8;
    assert!(ZSTD_initStaticCCtx(ws, size).is_null());
    assert!(ZSTD_initStaticCStream(ws, size).is_null());
    assert!(ZSTD_initStaticDCtx(ws, size).is_null());
    assert!(ZSTD_initStaticDStream(ws, size).is_null());
    let dict = b"raw content";
    let cparams = crate::params::ZSTD_getCParams(3, 0, dict.len());
    assert!(
        ZSTD_initStaticCDict(ws, size, dict.as_ptr().cast(), dict.len(), 0, 0, cparams).is_null()
    );
    assert!(ZSTD_initStaticDDict(ws, size, dict.as_ptr().cast(), dict.len(), 0, 0).is_null());
}

/// Content placed at the end of the buffer becomes a dictionary at its
/// start: magic, a derived id, tables, then the content, usable to compress.
#[test]
fn entropy_tables_wrap_content_in_place() {
    let (samples, sizes) = samples();
    let content = b"tenant=demo table=orders key= region=eu status=shipped\n".repeat(8);
    let mut buffer = vec![0u8; 4096];
    let at = buffer.len() - content.len();
    buffer[at..].copy_from_slice(&content);
    let n = unsafe {
        ZDICT_addEntropyTablesFromBuffer(
            buffer.as_mut_ptr(),
            content.len(),
            buffer.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    assert_eq!(ZDICT_isError(n), 0, "{:?}", code(n));
    buffer.truncate(n);
    assert_eq!(buffer[..4], [0x37, 0xA4, 0x30, 0xEC]);
    assert_ne!(unsafe { ZDICT_getDictID(buffer.as_ptr(), n) }, 0);
    let header = unsafe { ZDICT_getDictHeaderSize(buffer.as_ptr(), n) };
    assert_eq!(header + content.len(), n);
    assert!(buffer.ends_with(&content));
    round_trip_with(&buffer, &samples[..2000]);

    // Content longer than its buffer is refused.
    let rc = unsafe {
        ZDICT_addEntropyTablesFromBuffer(
            buffer.as_mut_ptr(),
            buffer.len() + 1,
            buffer.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
        )
    };
    assert_ne!(ZDICT_isError(rc), 0);
}

/// The original trainer gives a usable dictionary with the id asked for, and
/// 0 (no dictionary, not an error) for samples too few to train on, as
/// upstream does.
#[test]
fn legacy_trainer_trains_or_gives_nothing() {
    let (samples, sizes) = samples();
    let params = ZDICT_legacy_params_t {
        selectivityLevel: 0,
        zParams: ZDICT_params_t {
            compressionLevel: 0,
            notificationLevel: 0,
            dictID: 4242,
        },
    };
    let mut dict = vec![0u8; 4096];
    let n = unsafe {
        ZDICT_trainFromBuffer_legacy(
            dict.as_mut_ptr(),
            dict.len(),
            samples.as_ptr(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    assert_eq!(ZDICT_isError(n), 0, "{:?}", code(n));
    dict.truncate(n);
    assert_eq!(unsafe { ZDICT_getDictID(dict.as_ptr(), n) }, 4242);
    round_trip_with(&dict, &samples[..2000]);

    let mut small = vec![0u8; 4096];
    let few = [100usize];
    let n = unsafe {
        ZDICT_trainFromBuffer_legacy(
            small.as_mut_ptr(),
            small.len(),
            samples.as_ptr(),
            few.as_ptr(),
            1,
            params,
        )
    };
    assert_eq!(n, 0);
}
