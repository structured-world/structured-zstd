//! libzstd counterpart of `encode_loop_dict_swap`: per round a `ZSTD_CDict`
//! is created for the next dictionary, as upstream's dictionary trainer
//! prices each candidate (`COVER_checkTotalCompressedSize`), then frames
//! alternate between the input and its first `alt_len` bytes through one
//! reused `ZSTD_CCtx`.
//!
//! Build: cargo build --profile bench -p ffi-bench --example ffi_loop_dict_swap
//! Run:   target/release/examples/ffi_loop_dict_swap <level> <rounds> <frames> <input> <alt_len> <dict>...

use std::env;

use zstd::zstd_safe::zstd_sys;

fn main() {
    let args: Vec<String> = env::args().collect();
    let usage = "usage: <level> <rounds> <frames> <input> <alt_len> <dict>...";
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).expect(usage);
    let rounds: usize = args.get(2).and_then(|s| s.parse().ok()).expect(usage);
    let frames: usize = args.get(3).and_then(|s| s.parse().ok()).expect(usage);
    let input = std::fs::read(args.get(4).expect(usage)).expect("read input");
    let alt_len: usize = args.get(5).and_then(|s| s.parse().ok()).expect(usage);
    let dicts: Vec<Vec<u8>> = args[6..]
        .iter()
        .map(|path| std::fs::read(path).expect("read dictionary"))
        .collect();
    assert!(!dicts.is_empty(), "{usage}");
    let short = &input[..alt_len.min(input.len())];

    // SAFETY (every call below): the pointers and lengths come from live
    // slices and vectors of this function, and each context is created
    // before use and freed once.
    let cap = unsafe { zstd_sys::ZSTD_compressBound(input.len()) };
    let mut out = vec![0u8; cap];
    let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
    assert!(!cctx.is_null(), "ZSTD_createCCtx failed");
    let mut total = 0usize;
    for round in 0..rounds {
        let dict = &dicts[round % dicts.len()];
        let cdict = unsafe { zstd_sys::ZSTD_createCDict(dict.as_ptr().cast(), dict.len(), level) };
        assert!(!cdict.is_null(), "ZSTD_createCDict failed");
        for frame in 0..frames {
            let src = if frame % 2 == 1 { short } else { &input[..] };
            let written = unsafe {
                zstd_sys::ZSTD_compress_usingCDict(
                    cctx,
                    out.as_mut_ptr().cast(),
                    cap,
                    src.as_ptr().cast(),
                    src.len(),
                    cdict,
                )
            };
            assert_eq!(
                unsafe { zstd_sys::ZSTD_isError(written) },
                0,
                "compress failed"
            );
            total += written;
            core::hint::black_box(&out);
        }
        unsafe { zstd_sys::ZSTD_freeCDict(cdict) };
    }
    unsafe { zstd_sys::ZSTD_freeCCtx(cctx) };
    eprintln!("ffi {rounds} rounds x {frames} frames at level {level}: {total} bytes out");
}
