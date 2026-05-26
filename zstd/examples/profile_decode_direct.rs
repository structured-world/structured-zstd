//! Hot-loop decode binary for targeted flamegraph profiling on the
//! direct-path (`UserSliceBackend`) decode. Mirrors the
//! `compare_ffi.rs` `pure_rust` arm — pre-sizes the target with
//! `WILDCOPY_OVERLENGTH` slack so the per-frame eligibility gate
//! sends the decode through `run_direct_decode`.
//!
//! Usage:
//!   profile_decode_direct <compressed_blob> <decompressed_size> [iters]
//!
//! The custom binary avoids criterion's setup overhead (build_raw_dict,
//! reservation churn, page-fault noise) that drowned the decode hot
//! path in `cargo flamegraph --bench compare_ffi` runs.

use std::env;
use std::fs;
use structured_zstd::WILDCOPY_OVERLENGTH;
use structured_zstd::decoding::FrameDecoder;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(1)
        .expect("usage: profile_decode_direct <blob> <expected_size> [iters]");
    let expected: usize = args
        .get(2)
        .expect("expected_size required")
        .parse()
        .expect("expected_size parse");
    let iters: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(50_000);

    let compressed = fs::read(path).expect("read");
    let mut target = vec![0u8; expected + WILDCOPY_OVERLENGTH];
    // Pre-touch pages so kernel zero-init isn't in the flamegraph.
    for slot in target.iter_mut().step_by(4096) {
        *slot = 0;
    }

    let mut decoder = FrameDecoder::new();
    let t0 = std::time::Instant::now();
    let mut written_total = 0u64;
    for _ in 0..iters {
        let n = decoder
            .decode_all(compressed.as_slice(), &mut target)
            .expect("decode_all");
        written_total = written_total.wrapping_add(n as u64);
        std::hint::black_box(&target[..n]);
    }
    let elapsed = t0.elapsed();
    eprintln!(
        "iters={} elapsed={:.3?} per_iter={:.3?} total_written={}",
        iters,
        elapsed,
        elapsed / iters as u32,
        written_total
    );
}
