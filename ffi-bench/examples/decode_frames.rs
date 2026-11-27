//! Decode (or encode) a list of standalone frames block by block, as a store
//! reading one data block per point read does, with either codec.
//!
//! Input file: for each block a little-endian `u32` frame length, a `u32`
//! decoded length, then the frame bytes (for a raw list both lengths are the
//! block's size and the bytes are the block).
//!
//! Usage: decode_frames <rust|ffi|enc-rust|enc-ffi> <passes> <file> [level]
//! Decode modes write each block into a buffer of exactly its decoded size
//! and check the length; encode modes write one standalone frame per block
//! through a reused context at `level` (default 22).

use std::env;

use structured_zstd::decoding::FrameDecoder;
use structured_zstd::encoding::{CompressionLevel, FrameCompressor};
use zstd::zstd_safe::zstd_sys;

struct Entry {
    bytes: Vec<u8>,
    decoded: usize,
}

fn read_list(path: &str) -> Vec<Entry> {
    let data = std::fs::read(path).expect("read list file");
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes")) as usize;
        let decoded =
            u32::from_le_bytes(data[at + 4..at + 8].try_into().expect("4 bytes")) as usize;
        at += 8;
        entries.push(Entry {
            bytes: data[at..at + len].to_vec(),
            decoded,
        });
        at += len;
    }
    entries
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).expect("mode");
    let passes: u32 = args.get(2).and_then(|s| s.parse().ok()).expect("passes");
    let path = args.get(3).map(String::as_str).expect("list file");
    let level: i32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(22);

    let entries = read_list(path);
    let blocks = entries.len();
    let input_bytes: usize = entries.iter().map(|e| e.bytes.len()).sum();
    let decoded_bytes: usize = entries.iter().map(|e| e.decoded).sum();
    let mut targets: Vec<Vec<u8>> = entries.iter().map(|e| vec![0u8; e.decoded]).collect();
    eprintln!(
        "{path}: {blocks} blocks, {input_bytes} input bytes, {decoded_bytes} decoded bytes, mode {mode}, {passes} passes"
    );

    let start = std::time::Instant::now();
    let mut total = 0usize;
    match mode {
        "rust" => {
            let mut decoder = FrameDecoder::new();
            for _ in 0..passes {
                for (e, t) in entries.iter().zip(targets.iter_mut()) {
                    let wrote = decoder
                        .decode_all(std::hint::black_box(&e.bytes), t)
                        .expect("decode_all");
                    assert_eq!(wrote, e.decoded);
                    total = total.wrapping_add(wrote);
                }
            }
        }
        "ffi" => {
            let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
            assert!(!dctx.is_null(), "ZSTD_createDCtx failed");
            for _ in 0..passes {
                for (e, t) in entries.iter().zip(targets.iter_mut()) {
                    let wrote = unsafe {
                        zstd_sys::ZSTD_decompressDCtx(
                            dctx,
                            t.as_mut_ptr().cast(),
                            t.len(),
                            std::hint::black_box(e.bytes.as_ptr()).cast(),
                            e.bytes.len(),
                        )
                    };
                    assert_eq!(unsafe { zstd_sys::ZSTD_isError(wrote) }, 0, "decompress");
                    assert_eq!(wrote, e.decoded);
                    total = total.wrapping_add(wrote);
                }
            }
            unsafe { zstd_sys::ZSTD_freeDCtx(dctx) };
        }
        "enc-rust" => {
            let mut compressor: FrameCompressor =
                FrameCompressor::new(CompressionLevel::Level(level));
            let mut out = Vec::new();
            for _ in 0..passes {
                for e in &entries {
                    compressor
                        .compress_independent_frame_into(std::hint::black_box(&e.bytes), &mut out);
                    total = total.wrapping_add(out.len());
                }
            }
        }
        "enc-ffi" => {
            let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
            assert!(!cctx.is_null(), "ZSTD_createCCtx failed");
            unsafe {
                zstd_sys::ZSTD_CCtx_setParameter(
                    cctx,
                    zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
                    level,
                );
            }
            let cap = entries
                .iter()
                .map(|e| unsafe { zstd_sys::ZSTD_compressBound(e.bytes.len()) })
                .max()
                .unwrap_or(0);
            let mut out = vec![0u8; cap];
            for _ in 0..passes {
                for e in &entries {
                    let w = unsafe {
                        zstd_sys::ZSTD_compress2(
                            cctx,
                            out.as_mut_ptr().cast(),
                            cap,
                            std::hint::black_box(e.bytes.as_ptr()).cast(),
                            e.bytes.len(),
                        )
                    };
                    assert_eq!(unsafe { zstd_sys::ZSTD_isError(w) }, 0, "ZSTD_compress2");
                    total = total.wrapping_add(w);
                }
            }
            unsafe { zstd_sys::ZSTD_freeCCtx(cctx) };
        }
        other => panic!("unknown mode {other}"),
    }
    let elapsed = start.elapsed();
    let calls = u64::from(passes) * blocks as u64;
    eprintln!(
        "{calls} calls, {total} output bytes, {elapsed:?} wall, {:.0} ns/block ({mode})",
        elapsed.as_nanos() as f64 / calls as f64
    );
}
