//! Standalone decode-loop binary for clean perf-record profiles.
//! Generates a 1MB deterministic corpus, FFI-encodes once at the given
//! level, then loops `decode_all` for N iters. No criterion overhead,
//! no per-iter encode — pure decoder hot path.
//!
//! Build: cargo build --profile flamegraph -p structured-zstd \
//!          --example decode_loop_z000033 --features dict_builder
//! Run:   perf record -F 999 -g --call-graph dwarf,16384 -- \
//!          target/flamegraph/examples/decode_loop_z000033 3 50000

use std::env;

use structured_zstd::WILDCOPY_OVERLENGTH;
use structured_zstd::decoding::FrameDecoder;
use zstd::zstd_safe::zstd_sys;

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50_000);

    // Deterministic 1MB synthetic — LCG, repeatable across hosts.
    let n = 1_048_576usize;
    let mut src = Vec::with_capacity(n);
    let mut state: u64 = 0x517cc1b727220a95;
    while src.len() < n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        src.push((state >> 56) as u8);
    }

    // FFI encode once at requested level.
    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(src.len()) };
    let mut compressed: Vec<u8> = vec![0u8; dst_cap];
    let written = unsafe {
        zstd_sys::ZSTD_compress(
            compressed.as_mut_ptr() as *mut core::ffi::c_void,
            dst_cap,
            src.as_ptr() as *const core::ffi::c_void,
            src.len(),
            level,
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(written) },
        0,
        "encode failed"
    );
    compressed.truncate(written);
    eprintln!(
        "encoded {} bytes → {} bytes at level {}",
        src.len(),
        written,
        level
    );
    eprintln!(
        "FHD byte4=0x{:02x}, content_checksum_flag (bit 2) = {}",
        compressed[4],
        (compressed[4] >> 2) & 1
    );

    let mut target = vec![0u8; n + WILDCOPY_OVERLENGTH];
    // Pre-touch pages so first iter doesn't pay anon-fault cost.
    for chunk in target.chunks_mut(4096) {
        chunk[0] = 0;
    }
    let mut decoder = FrameDecoder::new();

    eprintln!("starting {} decode iters", iters);
    let start = std::time::Instant::now();
    let mut total = 0usize;
    for _ in 0..iters {
        let wrote = decoder
            .decode_all(std::hint::black_box(&compressed), &mut target)
            .expect("decode_all");
        total = total.wrapping_add(wrote);
    }
    let elapsed = start.elapsed();
    eprintln!(
        "decoded {} iters, {} total bytes, {:?} wall, {:.0} ns/iter",
        iters,
        total,
        elapsed,
        elapsed.as_nanos() as f64 / iters as f64
    );
}
