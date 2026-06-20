//! Dictionary compress matrix: per (frame size, level), compressed size and
//! compress time for structured-zstd vs the C reference, both reusing a
//! prepared dictionary across iterations (the per-label-dict use case). Run on
//! the bench host for timing.
//!
//! Build/run: `cargo run --release -p ffi-bench --example dict_matrix
//!             --features dict_builder`

use std::time::Instant;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};
use zstd::zstd_safe::zstd_sys;

fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let mut b = Vec::with_capacity(len);
    while b.len() < len {
        for l in LINES {
            if b.len() == len {
                break;
            }
            let r = len - b.len();
            b.extend_from_slice(&l.as_bytes()[..l.len().min(r)]);
        }
    }
    b
}

fn time_min<F: FnMut() -> usize>(iters: u32, mut f: F) -> (f64, usize) {
    let mut best = f64::MAX;
    let mut out = 0;
    for _ in 0..iters {
        let t = Instant::now();
        out = f();
        let us = t.elapsed().as_secs_f64() * 1e6;
        if us < best {
            best = us;
        }
        std::hint::black_box(out);
    }
    (best, out)
}

fn main() {
    // Raw-content dictionary (both sides load the same bytes; a non-magic blob
    // is treated as a content dictionary by each). 8 KiB of the same log
    // structure the frames share.
    let dict = repeated_log_lines(8 * 1024);
    let fixtures: &[(&str, usize)] = &[
        ("logs-512", 512),
        ("logs-1k", 1024),
        ("logs-4k", 4096),
        ("logs-16k", 16 * 1024),
    ];
    let levels: &[i32] = &[-3, -1, 1, 2, 3, 6, 9, 12, 19];
    let iters: u32 = 20000;

    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(64 * 1024) };

    println!(
        "{:<9} {:>3} | {:>6} {:>7} | {:>6} {:>7} | rust:sz/sp",
        "fixture", "lvl", "r_B", "r_us", "c_B", "c_us"
    );
    println!("{}", "-".repeat(60));
    for (name, size) in fixtures {
        let data = repeated_log_lines(*size);
        for &level in levels {
            // Rust: prepare the dict once, reuse the compressor across iters.
            let (r_us, r_b) = {
                let mut cctx: FrameCompressor =
                    FrameCompressor::new(CompressionLevel::Level(level));
                // Raw-content dictionary (matches `ZSTD_createCDict` on a
                // non-magic blob: auto-detected raw content, dict id 0).
                let dict_obj =
                    structured_zstd::decoding::Dictionary::from_raw_content(1, dict.clone())
                        .expect("raw dict");
                cctx.set_dictionary_id_flag(false);
                cctx.set_dictionary(dict_obj).expect("attach dict");
                let mut out: Vec<u8> = Vec::new();
                time_min(iters, || {
                    cctx.compress_independent_frame_into(&data, &mut out);
                    out.len()
                })
            };
            // C: createCDict once, reuse CCtx across iters.
            let (c_us, c_b) = unsafe {
                let cdict = zstd_sys::ZSTD_createCDict(dict.as_ptr().cast(), dict.len(), level);
                let cctx = zstd_sys::ZSTD_createCCtx();
                // Both creators return null on allocation failure; passing a
                // null handle to ZSTD_compress_usingCDict is undefined behavior.
                assert!(!cdict.is_null(), "ZSTD_createCDict returned null");
                assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");
                let mut dst: Vec<u8> = vec![0u8; dst_cap];
                let r = time_min(iters, || {
                    let rc = zstd_sys::ZSTD_compress_usingCDict(
                        cctx,
                        dst.as_mut_ptr().cast(),
                        dst_cap,
                        data.as_ptr().cast(),
                        data.len(),
                        cdict,
                    );
                    assert_eq!(zstd_sys::ZSTD_isError(rc), 0);
                    rc
                });
                zstd_sys::ZSTD_freeCCtx(cctx);
                zstd_sys::ZSTD_freeCDict(cdict);
                r
            };
            let sz = r_b as f64 / c_b as f64;
            let sp = r_us / c_us;
            println!(
                "{name:<9} {level:>3} | {r_b:>6} {r_us:>7.2} | {c_b:>6} {c_us:>7.2} | {sz:.2}/{sp:.2}x"
            );
        }
        println!();
    }
}
