//! Standalone C-decoder loop for a clean perf-record profile of the
//! reference decoder, to compare its hot-path breakdown against our
//! `decode_loop_z000033`. Encodes the in-tree z000033 once at the given
//! level via FFI, then decodes it N times through a reused `ZSTD_DCtx`
//! (steady state, no per-iter context alloc).
//!
//! Build: cargo build --profile flamegraph -p structured-zstd \
//!          --example c_decode_loop_z000033 --features dict-builder
//! Run:   perf record -F 999 -g --call-graph dwarf,16384 -- \
//!          target/flamegraph/examples/c_decode_loop_z000033 3 20000

use std::env;
use std::fs;

use zstd::zstd_safe::zstd_sys;

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let corpus = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());
    let src = fs::read(&corpus).expect("read corpus");
    let n = src.len();

    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(n) };
    let mut compressed = vec![0u8; dst_cap];
    let csize = unsafe {
        zstd_sys::ZSTD_compress(
            compressed.as_mut_ptr() as *mut core::ffi::c_void,
            dst_cap,
            src.as_ptr() as *const core::ffi::c_void,
            n,
            level,
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(csize) },
        0,
        "compress failed"
    );

    let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
    assert!(!dctx.is_null(), "createDCtx failed");
    let mut out = vec![0u8; n];
    let mut total: u64 = 0;
    for _ in 0..iters {
        let w = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                dctx,
                out.as_mut_ptr() as *mut core::ffi::c_void,
                n,
                compressed.as_ptr() as *const core::ffi::c_void,
                csize,
            )
        };
        assert_eq!(unsafe { zstd_sys::ZSTD_isError(w) }, 0, "decompress failed");
        assert_eq!(w, n, "decompress produced unexpected output size");
        total = total.wrapping_add(out.first().copied().unwrap_or(0) as u64);
    }
    unsafe { zstd_sys::ZSTD_freeDCtx(dctx) };
    eprintln!(
        "c_decode_loop: level={level} iters={iters} csize={csize} out={n} sink={total} ({} blocks-ish)",
        csize
    );
}
