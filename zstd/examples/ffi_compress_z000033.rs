//! One-shot diagnostic: invoke FFI `ZSTD_compress` on z000033 at level 1.
//! A patched upstream `zstd_fast.c` (with `fprintf(stderr, "D_...")`
//! traces) is gated by the `FFI_TRACE_FAST=1` env var. Run this binary with
//! both `FFI_TRACE_FAST=1` set and stderr redirected to a file to
//! capture the C encoder's actual cursor trace.
//!
//! Build: cargo build --release -p ffi-bench --example ffi_compress_z000033
//! Run:   FFI_TRACE_FAST=1 ./target/release/examples/ffi_compress_z000033 \
//!          > /dev/null 2> /tmp/ffi_trace.log

use std::env;
use std::fs;

use zstd::zstd_safe::zstd_sys;

fn main() {
    let corpus_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());
    let bytes = fs::read(&corpus_path).expect("read corpus");
    eprintln!(
        "FFI_TRACE_START corpus={} size={} level=1",
        corpus_path,
        bytes.len()
    );

    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(bytes.len()) };
    let mut dst: Vec<u8> = vec![0u8; dst_cap];

    let rc = unsafe {
        zstd_sys::ZSTD_compress(
            dst.as_mut_ptr() as *mut core::ffi::c_void,
            dst_cap,
            bytes.as_ptr() as *const core::ffi::c_void,
            bytes.len(),
            1,
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(rc) },
        0,
        "ZSTD_compress failed"
    );

    eprintln!("FFI_TRACE_END ffi_bytes={} input_bytes={}", rc, bytes.len());
}
