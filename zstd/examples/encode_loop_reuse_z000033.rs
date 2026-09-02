//! Reused-compressor encode-loop for profiling the ENCODER COMPUTE in
//! isolation from per-frame allocation.
//!
//! Unlike [`encode_loop_z000033`] (which constructs a fresh
//! `FrameCompressor` every iteration so the profile INCLUDES the matcher
//! table allocation + memset + first-touch page faults), this binary
//! constructs ONE `FrameCompressor` outside the loop and drives
//! `compress_independent_frame_into` — the CCtx-equivalent reuse path that
//! resets per-frame state but keeps every allocation. After the first
//! iteration the hash tables, history, entropy scratch and output buffer
//! are all warm, so steady-state samples land on the real compute hot
//! path (match-find + sequence emit + entropy encode), letting a
//! flamegraph reveal where the encoder spends CPU once allocation is
//! amortized (the shape a real caller that reuses one compressor sees).
//!
//! Build: `cargo build --profile flamegraph -p structured-zstd
//!          --example encode_loop_reuse_z000033 --features dict-builder`
//! Run:   `cargo flamegraph --example encode_loop_reuse_z000033 --features dict-builder
//!          --profile flamegraph -- <level> <iters> <corpus_path>`

use std::env;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

// With `--features dhat-heap`, route every allocation through the dhat heap
// profiler so the run records per-call-site allocation counts + bytes (the
// reused-compressor churn that broken-unwind flamegraphs can't attribute).
// Writes `dhat-heap.json` on `Profiler` drop.
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let corpus_path: Option<&str> = args.get(3).map(|s| s.as_str());

    let src: Vec<u8> = if let Some(path) = corpus_path {
        std::fs::read(path).expect("read corpus file")
    } else {
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
    };

    let cap = src
        .len()
        .checked_add(src.len() >> 3)
        .and_then(|v| v.checked_add(4096))
        .expect("corpus too large: output-capacity bound overflows usize");
    let mut out: Vec<u8> = Vec::with_capacity(cap);

    let compressor_level = CompressionLevel::from_level(level);

    // ONE compressor, reused across every iteration: allocations happen on
    // the first frame and amortize to zero for the rest, so the profile is
    // dominated by compute, not table alloc / memset / page faults.
    let mut frame_enc: FrameCompressor = FrameCompressor::new(compressor_level);
    frame_enc.set_source_size_hint(src.len() as u64);

    let mut sink: usize = 0;
    for _ in 0..iters {
        // `_into` replaces `out`'s contents and reuses its capacity; the
        // compressor resets per-frame state but keeps its heavy allocations.
        frame_enc.compress_independent_frame_into(src.as_slice(), &mut out);
        sink = sink.wrapping_add(out.len());
        core::hint::black_box(&out);
    }

    eprintln!(
        "encoded {} bytes × {} iters at level {} (reused compressor); last-out-sum={}",
        src.len(),
        iters,
        level,
        sink
    );
}
