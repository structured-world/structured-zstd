//! libzstd counterpart of `encode_loop_dict_swap`: per round the next
//! dictionary is loaded into one reused `ZSTD_CCtx`, then frames walk the
//! input's consecutive `frame_len`-byte pieces through `ZSTD_compress2`, every
//! second one cut to its first `alt_len` bytes.
//!
//! The context is set up as the benchmark's dictionary cells set libzstd's:
//! the level, `ZSTD_CCtx_loadDictionary`, and the content checksum when the
//! `hash` feature is on.
//!
//! Build: cargo build --profile bench -p ffi-bench --example ffi_loop_dict_swap
//! Run:   target/release/examples/ffi_loop_dict_swap <level> <rounds> <frames> <input> <frame_len> <alt_len> <dict>...

use std::env;

use zstd::zstd_safe::zstd_sys;

fn main() {
    let args: Vec<String> = env::args().collect();
    let usage = "usage: <level> <rounds> <frames> <input> <frame_len> <alt_len> <dict>...";
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).expect(usage);
    let rounds: usize = args.get(2).and_then(|s| s.parse().ok()).expect(usage);
    let frames: usize = args.get(3).and_then(|s| s.parse().ok()).expect(usage);
    let input = std::fs::read(args.get(4).expect(usage)).expect("read input");
    let frame_len: usize = args.get(5).and_then(|s| s.parse().ok()).expect(usage);
    let alt_len: usize = args.get(6).and_then(|s| s.parse().ok()).expect(usage);
    let dicts: Vec<Vec<u8>> = args[7..]
        .iter()
        .map(|path| std::fs::read(path).expect("read dictionary"))
        .collect();
    assert!(!dicts.is_empty(), "{usage}");
    let pieces: Vec<&[u8]> = input.chunks_exact(frame_len).collect();
    assert!(!pieces.is_empty(), "the input holds no whole frame");

    // SAFETY (every call below): the pointers and lengths come from live
    // slices and vectors of this function, and the context is created before
    // use and freed once.
    let cap = unsafe { zstd_sys::ZSTD_compressBound(frame_len) };
    let mut out = vec![0u8; cap];
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
    set(
        zstd_sys::ZSTD_cParameter::ZSTD_c_checksumFlag,
        i32::from(cfg!(feature = "hash")),
    );
    let mut total = 0usize;
    let mut call = 0usize;
    for round in 0..rounds {
        let dict = &dicts[round % dicts.len()];
        let rc =
            unsafe { zstd_sys::ZSTD_CCtx_loadDictionary(cctx, dict.as_ptr().cast(), dict.len()) };
        assert_eq!(
            unsafe { zstd_sys::ZSTD_isError(rc) },
            0,
            "loadDictionary failed"
        );
        for frame in 0..frames {
            let piece = pieces[call % pieces.len()];
            call += 1;
            let src = if frame % 2 == 1 {
                &piece[..alt_len.min(piece.len())]
            } else {
                piece
            };
            let written = unsafe {
                zstd_sys::ZSTD_compress2(
                    cctx,
                    out.as_mut_ptr().cast(),
                    cap,
                    src.as_ptr().cast(),
                    src.len(),
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
    }
    unsafe { zstd_sys::ZSTD_freeCCtx(cctx) };
    eprintln!("ffi {rounds} rounds x {frames} frames at level {level}: {total} bytes out");
}
