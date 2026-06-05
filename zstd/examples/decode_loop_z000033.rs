//! Standalone decode-loop binary for clean perf-record profiles.
//! Decodes the in-tree z000033 corpus (or a user-provided file) in a tight
//! loop after one-time FFI encoding at the given level. N iters of
//! `decode_all`, no criterion overhead, no per-iter encode: pure decoder
//! hot path. The random LCG synthetic source is opt-in via the `synthetic`
//! arg only.
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
    let mode: &str = args.get(3).map(|s| s.as_str()).unwrap_or("rust");
    let corpus_path: Option<&str> = args.get(4).map(|s| s.as_str());
    // 6th arg: "checksum" → encode with content_checksum_flag = 1 so the
    // decode loop exercises the post-decode XXH64 verify pass. Lets us
    // isolate the checksum cost (flag-on vs flag-off) in one harness.
    let checksum: bool = args.get(5).map(|s| s == "checksum").unwrap_or(false);

    // Source resolution. This example is named after the `z000033` corpus,
    // so an explicit path or the in-tree corpus is the contract. The LCG
    // synthetic is RANDOM (≈incompressible → mostly RAW blocks → a trivial,
    // unrepresentative decode workload), so it is NEVER substituted silently:
    // a missing corpus fails loudly, and synthetic is opt-in via the literal
    // `synthetic` arg. (A silent fallback here previously masked a missing
    // corpus and produced ~30 GB/s "decode" numbers that hid the real gap.)
    let src: Vec<u8> = match corpus_path {
        Some("synthetic") => {
            eprintln!(
                "decode_loop_z000033: WARNING — using the random LCG synthetic \
                 source; decode timings are NOT representative of z000033 \
                 (random data decodes as RAW blocks)."
            );
            let n = 1_048_576usize;
            let mut src = Vec::with_capacity(n);
            let mut state: u64 = 0x517cc1b727220a95;
            while src.len() < n {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                src.push((state >> 56) as u8);
            }
            src
        }
        Some(path) => std::fs::read(path).expect("read corpus file"),
        None => {
            // Default to the in-tree z000033 this example is named after.
            // Follow the same resolution order as the benchmarks (see
            // `zstd/benches/support/mod.rs`): explicit env var first, then
            // the cargo-driven manifest dir, then cwd-relative locations
            // (repo root / `zstd/`). Fail loudly if absent — never silently
            // synthesize.
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            if let Ok(explicit) = std::env::var("STRUCTURED_ZSTD_BENCH_CORPUS_PATH") {
                let trimmed = explicit.trim();
                if !trimmed.is_empty() {
                    candidates.push(std::path::PathBuf::from(trimmed));
                }
            }
            if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
                candidates.push(
                    std::path::PathBuf::from(manifest_dir).join("decodecorpus_files/z000033"),
                );
            }
            candidates.push(std::path::PathBuf::from("zstd/decodecorpus_files/z000033"));
            candidates.push(std::path::PathBuf::from("decodecorpus_files/z000033"));
            candidates
                .iter()
                .find_map(|p| std::fs::read(p).ok())
                .unwrap_or_else(|| {
                    panic!(
                        "decode_loop_z000033: corpus z000033 not found via env vars or in \
                         {candidates:?}; pass an explicit path as arg 4, or `synthetic` to \
                         opt into the random LCG fallback"
                    )
                })
        }
    };
    let n = src.len();

    // FFI encode once at requested level.
    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(src.len()) };
    let mut compressed: Vec<u8> = vec![0u8; dst_cap];
    let written = if checksum {
        // ZSTD_compress2 honours CCtx params, including the checksum flag.
        let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
        assert!(!cctx.is_null(), "ZSTD_createCCtx failed");
        unsafe {
            zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
                level,
            );
            zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_checksumFlag,
                1,
            );
        }
        let w = unsafe {
            zstd_sys::ZSTD_compress2(
                cctx,
                compressed.as_mut_ptr() as *mut core::ffi::c_void,
                dst_cap,
                src.as_ptr() as *const core::ffi::c_void,
                src.len(),
            )
        };
        unsafe { zstd_sys::ZSTD_freeCCtx(cctx) };
        w
    } else {
        unsafe {
            zstd_sys::ZSTD_compress(
                compressed.as_mut_ptr() as *mut core::ffi::c_void,
                dst_cap,
                src.as_ptr() as *const core::ffi::c_void,
                src.len(),
                level,
            )
        }
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
    eprintln!("starting {} decode iters in mode {}", iters, mode);
    let start = std::time::Instant::now();
    let mut total = 0usize;
    match mode {
        "ffi" => {
            // Reuse one DCtx to mirror the c_ffi compare_ffi arm. RAII
            // guard ensures `ZSTD_freeDCtx` runs even if the per-iter
            // `assert_eq!` below panics on a corrupted fixture.
            struct DCtxGuard(*mut zstd_sys::ZSTD_DCtx_s);
            impl Drop for DCtxGuard {
                fn drop(&mut self) {
                    unsafe { zstd_sys::ZSTD_freeDCtx(self.0) };
                }
            }
            let dctx = DCtxGuard(unsafe { zstd_sys::ZSTD_createDCtx() });
            assert!(!dctx.0.is_null(), "ZSTD_createDCtx failed");
            for _ in 0..iters {
                let wrote = unsafe {
                    zstd_sys::ZSTD_decompressDCtx(
                        dctx.0,
                        target.as_mut_ptr() as *mut _,
                        target.len(),
                        std::hint::black_box(compressed.as_ptr() as *const _),
                        compressed.len(),
                    )
                };
                assert_eq!(
                    unsafe { zstd_sys::ZSTD_isError(wrote) },
                    0,
                    "ZSTD_decompressDCtx"
                );
                total = total.wrapping_add(wrote);
            }
        }
        _ => {
            let mut decoder = FrameDecoder::new();
            for _ in 0..iters {
                let wrote = decoder
                    .decode_all(std::hint::black_box(&compressed), &mut target)
                    .expect("decode_all");
                total = total.wrapping_add(wrote);
            }
        }
    }
    let elapsed = start.elapsed();
    eprintln!(
        "decoded {} iters, {} total bytes, {:?} wall, {:.0} ns/iter ({})",
        iters,
        total,
        elapsed,
        elapsed.as_nanos() as f64 / iters as f64,
        mode
    );
}
