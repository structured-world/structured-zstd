//! C-side counterpart of `encode_loop_fresh_z000033`, for instruction-count
//! and cycle comparisons under `perf stat`.
//!
//! Every frame is the benchmark's C arm (`ffi_encode_into` in `compare_ffi`):
//! a fresh context with the level, content size and checksum set, a 14-bit
//! window for inputs up to 16 KiB, the pledged size, then `ZSTD_compress2`
//! into one reused output buffer. Keeping the two binaries the benchmark's
//! two arms is the point: the difference is then attributable to the
//! encoders, not to the harness. `FRAME_CHUNK=<N>` walks the input's N-byte
//! pieces in turn, as the Rust loop does.
//!
//! Build: `cargo build --release -p ffi-bench --example ffi_encode_loop_z000033`
//! Run:   `perf stat -e cycles,instructions ./ffi_encode_loop_z000033 <level> <iters> <corpus> [ldm]`
//!
//! With `ldm` it times the benchmark's `level_<N>_*_ldm` C arm: a fresh
//! context per frame through `ZSTD_compress2`, long-distance matching on,
//! content checksum and content size on.

use std::env;
use std::fs;

use zstd::zstd_safe::zstd_sys;

fn main() {
    if env::args().nth(4).as_deref() == Some("ldm") {
        return ldm_loop();
    }
    let level: i32 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: usize = env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let corpus_path = env::args()
        .nth(3)
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());

    let bytes = fs::read(&corpus_path).expect("read corpus");
    let frames: Vec<&[u8]> = match env::var("FRAME_CHUNK").ok().and_then(|n| n.parse().ok()) {
        Some(n) => bytes.chunks(n).collect(),
        None => vec![&bytes[..]],
    };
    let bound = unsafe { zstd_sys::ZSTD_compressBound(bytes.len()) };
    let mut out = vec![0u8; bound];

    // Summed so the optimiser cannot drop the calls, mirroring the Rust loop.
    let mut sum = 0usize;
    for i in 0..iters {
        sum += compress_fresh(frames[i % frames.len()], level, &mut out);
    }

    println!(
        "encoded {} bytes in {} frames x {} iters at level {}; last-out-sum={}",
        bytes.len(),
        frames.len(),
        iters,
        level,
        sum
    );
}

/// One frame through a fresh context set up as the benchmark's C arm sets it.
fn compress_fresh(input: &[u8], level: i32, out: &mut [u8]) -> usize {
    use zstd_sys::ZSTD_cParameter as P;
    // SAFETY: every call operates on the context created here, freed before
    // returning; `out` holds the compression bound of the largest frame.
    unsafe {
        let cctx = zstd_sys::ZSTD_createCCtx();
        assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");
        let checksum = if cfg!(feature = "hash") { 1 } else { 0 };
        for (param, value) in [
            (P::ZSTD_c_compressionLevel, level),
            (P::ZSTD_c_checksumFlag, checksum),
            (P::ZSTD_c_contentSizeFlag, 1),
        ] {
            let rc = zstd_sys::ZSTD_CCtx_setParameter(cctx, param, value);
            assert!(zstd_sys::ZSTD_isError(rc) == 0, "setParameter failed");
        }
        if input.len() <= (1 << 14) {
            let rc = zstd_sys::ZSTD_CCtx_setParameter(cctx, P::ZSTD_c_windowLog, 14);
            assert!(zstd_sys::ZSTD_isError(rc) == 0, "set windowLog failed");
        }
        let rc = zstd_sys::ZSTD_CCtx_setPledgedSrcSize(cctx, input.len() as u64);
        assert!(zstd_sys::ZSTD_isError(rc) == 0, "setPledgedSrcSize failed");
        let written = zstd_sys::ZSTD_compress2(
            cctx,
            out.as_mut_ptr().cast(),
            out.len(),
            input.as_ptr().cast(),
            input.len(),
        );
        assert!(
            zstd_sys::ZSTD_isError(written) == 0,
            "ZSTD_compress2 failed"
        );
        zstd_sys::ZSTD_freeCCtx(cctx);
        written
    }
}

fn ldm_loop() {
    use zstd_sys::ZSTD_cParameter as P;
    let level: i32 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: usize = env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let corpus_path = env::args().nth(3).expect("corpus path");
    let bytes = fs::read(&corpus_path).expect("read corpus");
    let bound = unsafe { zstd_sys::ZSTD_compressBound(bytes.len()) };
    let mut out = vec![0u8; bound];
    let mut sum = 0usize;
    for _ in 0..iters {
        // SAFETY: every call operates on the context created here, freed at
        // the end of the iteration; errors are asserted.
        unsafe {
            let cctx = zstd_sys::ZSTD_createCCtx();
            assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");
            for (param, value) in [
                (P::ZSTD_c_compressionLevel, level),
                (P::ZSTD_c_checksumFlag, 1),
                (P::ZSTD_c_contentSizeFlag, 1),
                (P::ZSTD_c_enableLongDistanceMatching, 1),
            ] {
                let rc = zstd_sys::ZSTD_CCtx_setParameter(cctx, param, value);
                assert!(zstd_sys::ZSTD_isError(rc) == 0, "setParameter failed");
            }
            let written = zstd_sys::ZSTD_compress2(
                cctx,
                out.as_mut_ptr().cast(),
                out.len(),
                bytes.as_ptr().cast(),
                bytes.len(),
            );
            assert!(
                zstd_sys::ZSTD_isError(written) == 0,
                "ZSTD_compress2 failed"
            );
            sum += written;
            zstd_sys::ZSTD_freeCCtx(cctx);
        }
    }
    println!(
        "encoded {} bytes x {} iters at level {} with ldm; last-out-sum={}",
        bytes.len(),
        iters,
        level,
        sum
    );
}
