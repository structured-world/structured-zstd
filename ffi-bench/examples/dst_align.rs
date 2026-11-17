//! Diagnostic: decode time of the bench's `large-log-stream` frame into a
//! destination placed at different offsets, ours (`decode_all`, the
//! dashboard's pure_rust arm) and libzstd (`ZSTD_decompressDCtx` on a reused
//! context), so a placement that only one side is sensitive to shows up.
//!
//! Run: `dst_align <level> <iters> [bytes]`. Prints, per destination offset,
//! the minimum and median ns per frame of each side over `iters` decodes,
//! taken in interleaved rounds.

use std::env;
use std::time::Instant;

use structured_zstd::decoding::FrameDecoder;
use zstd::zstd_safe::zstd_sys;

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

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(12);
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    let len: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(16 << 20);
    let input = repeated_log_lines(len);
    let frame = structured_zstd::encoding::compress_to_vec(
        input.as_slice(),
        structured_zstd::encoding::CompressionLevel::Level(level),
    );
    eprintln!("level {level}: {len} bytes -> {} byte frame", frame.len());

    // One allocation with room for every offset; each destination starts at
    // `base + offset`, `base` page aligned.
    let room = len + structured_zstd::WILDCOPY_OVERLENGTH;
    let span = room + 4 * 4096 + 64;
    let mut arena = vec![0u8; span + 4096];
    let base = arena.as_ptr().align_offset(4096);
    for page in arena.chunks_mut(4096) {
        page[0] = 1;
    }
    // Ours as detected, and ours with far long matches copied by the vector
    // loop: each decoder takes the far-copy choice when its scratch is built,
    // on its first frame.
    let mut build = |mode: u8| {
        structured_zstd::EXP_FAR_COPY.store(mode, std::sync::atomic::Ordering::Relaxed);
        let mut d = FrameDecoder::new();
        let dst = &mut arena[base..base + room];
        d.decode_all(&frame, dst).expect("warm decode");
        d
    };
    let mut decoder = build(0);
    let mut vector = build(2);
    structured_zstd::EXP_FAR_COPY.store(0, std::sync::atomic::Ordering::Relaxed);
    let dctx = unsafe { zstd_sys::ZSTD_createDCtx() };
    assert!(!dctx.is_null());

    let offsets: Vec<usize> = (0..8)
        .map(|k| k * 8)
        .chain([1, 3, 31, 4096, 4096 + 32, 8192, 12288])
        .collect();
    println!("offset  auto_min  auto_med   vec_min   vec_med  ffi_min  ffi_med  (ns per frame)");
    for &offset in &offsets {
        let start = base + offset;
        let mut rust_ns = Vec::with_capacity(iters);
        let mut vec_ns = Vec::with_capacity(iters);
        let mut ffi_ns = Vec::with_capacity(iters);
        for _ in 0..iters {
            let dst = &mut arena[start..start + room];
            let t = Instant::now();
            let n = decoder
                .decode_all(std::hint::black_box(&frame), dst)
                .expect("decode");
            rust_ns.push(t.elapsed().as_nanos());
            assert_eq!(n, len);
            let dst = &mut arena[start..start + room];
            let t = Instant::now();
            let n = vector
                .decode_all(std::hint::black_box(&frame), dst)
                .expect("decode");
            vec_ns.push(t.elapsed().as_nanos());
            assert_eq!(n, len);
            let dst = &mut arena[start..start + room];
            let t = Instant::now();
            let n = unsafe {
                zstd_sys::ZSTD_decompressDCtx(
                    dctx,
                    dst.as_mut_ptr().cast(),
                    dst.len(),
                    std::hint::black_box(frame.as_ptr()).cast(),
                    frame.len(),
                )
            };
            ffi_ns.push(t.elapsed().as_nanos());
            assert_eq!(n, len);
        }
        let rust_min = *rust_ns.iter().min().unwrap();
        let vec_min = *vec_ns.iter().min().unwrap();
        let ffi_min = *ffi_ns.iter().min().unwrap();
        println!(
            "{offset:>6}  {rust_min:>8}  {:>8}  {vec_min:>8}  {:>8}  {ffi_min:>7}  {:>7}",
            median(rust_ns),
            median(vec_ns),
            median(ffi_ns)
        );
    }
    unsafe { zstd_sys::ZSTD_freeDCtx(dctx) };
}
