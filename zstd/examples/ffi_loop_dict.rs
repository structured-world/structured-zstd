//! C-FFI counterpart of `encode_loop_dict` for a like-for-like perf profile:
//! parse the dictionary into a `ZSTD_CDict` ONCE, then loop
//! `ZSTD_compress_usingCDict` over a contiguous `&[u8]` at the given level with
//! a reused `ZSTD_CCtx`. Lets `perf record` attribute time to libzstd's
//! internal functions (ZSTD_compressBlock_lazy, HUF_compress, FSE_*, ...) so
//! the per-function TIME graph can be compared directly against ours.
//!
//! Build: cargo build --profile flamegraph -p structured-zstd
//!          --example ffi_loop_dict --features dict-builder
//! Run:   ./target/flamegraph/examples/ffi_loop_dict <level> <iters> logs<N> <dict_path>

use std::env;

use zstd::zstd_safe::zstd_sys;

/// Byte-for-byte the bench `repeated_log_lines(len)` fixture (same as
/// `encode_loop_dict`) so a `logs<N>` input reproduces the `*-log-lines`
/// scenario exactly.
fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        for line in LINES {
            if bytes.len() == len {
                break;
            }
            let remaining = len - bytes.len();
            bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
        }
    }
    bytes
}

fn resolve_input(spec: &str) -> Vec<u8> {
    if let Some(rest) = spec.strip_prefix("logs") {
        let n: usize = rest.parse().expect("logs<N>: N must be a byte count");
        repeated_log_lines(n)
    } else {
        std::fs::read(spec).expect("read input file")
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(6);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let input_spec: &str = args.get(3).map(|s| s.as_str()).unwrap_or("logs4096");
    let dict_path: &str = args.get(4).map(|s| s.as_str()).expect("dict path required");

    let src = resolve_input(input_spec);
    let dict = std::fs::read(dict_path).expect("read dict file");

    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(src.len()) };
    let mut dst: Vec<u8> = vec![0u8; dst_cap];

    // Parse the dict into a CDict ONCE (mirrors our reused EncoderDictionary).
    let cdict = unsafe {
        zstd_sys::ZSTD_createCDict(dict.as_ptr() as *const core::ffi::c_void, dict.len(), level)
    };
    assert!(!cdict.is_null(), "ZSTD_createCDict failed");
    let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
    assert!(!cctx.is_null(), "ZSTD_createCCtx failed");

    let mut sink: usize = 0;
    for _ in 0..iters {
        let rc = unsafe {
            zstd_sys::ZSTD_compress_usingCDict(
                cctx,
                dst.as_mut_ptr() as *mut core::ffi::c_void,
                dst_cap,
                src.as_ptr() as *const core::ffi::c_void,
                src.len(),
                cdict,
            )
        };
        assert_eq!(unsafe { zstd_sys::ZSTD_isError(rc) }, 0, "compress failed");
        sink = sink.wrapping_add(rc);
        core::hint::black_box(&dst);
    }

    // Release the raw C contexts (zstd-sys exposes no Drop wrapper for the bare
    // pointers). SAFETY: both were created above and not freed elsewhere.
    unsafe {
        zstd_sys::ZSTD_freeCCtx(cctx);
        zstd_sys::ZSTD_freeCDict(cdict);
    }

    eprintln!(
        "ffi encoded {} bytes x {} iters at level {} dict={}; last-out-sum={}",
        src.len(),
        iters,
        level,
        dict_path,
        sink
    );
}
