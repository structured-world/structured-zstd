//! C-side counterpart of `encode_loop_z000033`, for instruction-count and
//! cycle comparisons under `perf stat`.
//!
//! Same shape as the Rust loop: read the corpus once, allocate the output
//! buffer once, then compress it N times at the given level through the
//! upstream `ZSTD_compress` one-shot API. Keeping the two binaries
//! structurally identical is the point — the difference in retired
//! instructions is then attributable to the encoders, not to harness
//! overhead.
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
    let bound = unsafe { zstd_sys::ZSTD_compressBound(bytes.len()) };
    let mut out = vec![0u8; bound];

    // Summed so the optimiser cannot drop the calls, mirroring the Rust loop.
    let mut sum = 0usize;
    for _ in 0..iters {
        let written = unsafe {
            zstd_sys::ZSTD_compress(
                out.as_mut_ptr().cast(),
                out.len(),
                bytes.as_ptr().cast(),
                bytes.len(),
                level,
            )
        };
        assert!(
            unsafe { zstd_sys::ZSTD_isError(written) } == 0,
            "ZSTD_compress failed"
        );
        sum += written;
    }

    println!(
        "encoded {} bytes x {} iters at level {}; last-out-sum={}",
        bytes.len(),
        iters,
        level,
        sum
    );
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
