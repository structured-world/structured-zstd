//! C-FFI counterpart of `encode_loop_dict` for a like-for-like perf profile:
//! load the dictionary into a reused `ZSTD_CCtx` ONCE at the given level, then
//! loop `ZSTD_compress2` over a contiguous `&[u8]`, the shape of the
//! benchmark's `compress-dict` C arm (no checksum, as `encode_loop_dict`).
//! Lets `perf record` attribute time to libzstd's internal functions
//! (ZSTD_compressBlock_lazy, HUF_compress, FSE_*, ...) so the per-function TIME
//! graph can be compared directly against ours.
//!
//! Not `ZSTD_compress_usingCDict`: past 128 KiB of source it re-derives the
//! frame's parameters from the level and the source size, which is a different
//! strategy from the loaded dictionary's (level 14 over 1 MiB runs btlazy2
//! there and btopt in the benchmark).
//!
//! `FRAME_CHUNK=<N>` compresses the input's consecutive N-byte pieces in turn,
//! one per frame, as the benchmark walks a small fixture's distinct frames.
//!
//! Build: cargo build --profile flamegraph -p structured-zstd
//!          --example ffi_loop_dict --features dict-builder
//! Run:   ./target/flamegraph/examples/ffi_loop_dict <level> <iters> logs<N> [dict_path]
//!
//! `ffi_loop_dict train <input> <dict_path>` instead writes the dictionary the
//! benchmark's `compress-dict` cells train for that input: libzstd's trainer
//! over the input cut into `len / 16` chunks (256..=8192 bytes), asking for
//! `len / 8` bytes (256..=16 KiB).

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

/// The benchmark's dictionary for `src`: its training samples and size.
fn train(src: &[u8], dict_path: &str) {
    let sample_size = src.len().div_ceil(16).clamp(256, 8192);
    let samples: Vec<&[u8]> = src.chunks(sample_size).collect();
    let dict_size = src
        .len()
        .div_ceil(8)
        .clamp(256, 16 * 1024)
        .min(src.len() - 64);
    let dict = zstd::dict::from_samples(&samples, dict_size).expect("dictionary training");
    std::fs::write(dict_path, &dict).expect("write dict file");
    eprintln!(
        "trained {} bytes from {} samples into {dict_path}",
        dict.len(),
        samples.len()
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("train") {
        let input = args.get(2).expect("input");
        let dict_path = args.get(3).expect("dict path");
        return train(&resolve_input(input), dict_path);
    }
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(6);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let input_spec: &str = args.get(3).map(|s| s.as_str()).unwrap_or("logs4096");
    // Without a dictionary path the loop compresses with the reused context
    // alone, the counterpart of `encode_loop_dict` run without one.
    let dict_path: Option<&str> = args.get(4).map(|s| s.as_str());

    let src = resolve_input(input_spec);

    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(src.len()) };
    let mut dst: Vec<u8> = vec![0u8; dst_cap];

    let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
    assert!(!cctx.is_null(), "ZSTD_createCCtx failed");
    let set = |param, value| {
        let rc = unsafe { zstd_sys::ZSTD_CCtx_setParameter(cctx, param, value) };
        assert_eq!(
            unsafe { zstd_sys::ZSTD_isError(rc) },
            0,
            "setParameter failed"
        );
    };
    set(zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel, level);
    // Loaded ONCE and kept by the context across frames (mirrors our reused
    // dictionary).
    if let Some(path) = dict_path {
        let dict = std::fs::read(path).expect("read dict file");
        let rc = unsafe {
            zstd_sys::ZSTD_CCtx_loadDictionary(
                cctx,
                dict.as_ptr() as *const core::ffi::c_void,
                dict.len(),
            )
        };
        assert_eq!(
            unsafe { zstd_sys::ZSTD_isError(rc) },
            0,
            "loadDictionary failed"
        );
    }

    let frames: Vec<&[u8]> = match env::var("FRAME_CHUNK").ok().and_then(|n| n.parse().ok()) {
        Some(n) => src.chunks(n).collect(),
        None => vec![src.as_slice()],
    };
    let mut sink: usize = 0;
    for i in 0..iters {
        let frame = frames[i as usize % frames.len()];
        let rc = unsafe {
            zstd_sys::ZSTD_compress2(
                cctx,
                dst.as_mut_ptr() as *mut core::ffi::c_void,
                dst_cap,
                frame.as_ptr() as *const core::ffi::c_void,
                frame.len(),
            )
        };
        assert_eq!(unsafe { zstd_sys::ZSTD_isError(rc) }, 0, "compress failed");
        sink = sink.wrapping_add(rc);
        core::hint::black_box(&dst);
    }

    // Release the raw C context (zstd-sys exposes no Drop wrapper for the bare
    // pointer). SAFETY: created above and not freed elsewhere.
    unsafe {
        zstd_sys::ZSTD_freeCCtx(cctx);
    }

    eprintln!(
        "ffi encoded {} bytes in {} frames x {} iters at level {} dict={}; last-out-sum={}",
        src.len(),
        frames.len(),
        iters,
        level,
        dict_path.unwrap_or("none"),
        sink
    );
}
