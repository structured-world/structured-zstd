//! Allocation SITES of one warm frame, as `alloc_census_warm` is its sizes.
//!
//! The census says a frame allocates in a shape that looks like regrowth; this
//! says which buffers. The profiler is started only after a first frame has
//! run, so everything a compressor builds once and keeps is already allocated
//! and out of the picture, and what it records is what a steady-state frame
//! costs.
//!
//! Run: `cargo run --release -p ffi-bench --example alloc_sites_warm
//!        --features dhat-heap -- <level> [corpus path]`
//! then read `dhat-heap.json` (the viewer, or `jq` over its `pps` array).

use std::env;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let corpus = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("zstd/decodecorpus_files/z000033");
    let data = std::fs::read(corpus)
        .unwrap_or_else(|e| panic!("alloc_sites_warm: cannot read {corpus}: {e}"));

    let mut out = Vec::new();
    let mut compressor: FrameCompressor<&[u8], &mut Vec<u8>> =
        FrameCompressor::new(CompressionLevel::Level(level));
    compressor.set_source(&data[..]);
    compressor.set_drain(&mut out);
    compressor.compress();

    // Everything above is warm-up. From here the compressor is reused, which is
    // the shape under audit.
    // `fresh` audits a compressor built inside the profiled region, which is
    // what a one-shot caller pays and what the upstream one-shot entry point
    // (`ZSTD_compress`, which creates and frees a context per call) is
    // measured against. The default reuses the warmed one.
    let fresh = args.get(3).map(|s| s == "fresh").unwrap_or(false);

    #[cfg(feature = "dhat-heap")]
    let profiler = dhat::Profiler::new_heap();

    let mut out2 = Vec::new();
    if fresh {
        // The one-shot contract, not the streaming one: a context per frame
        // compressing straight from the caller's slice, which is what
        // `ZSTD_compress` does and what `compress_slice_to_vec` wraps.
        let mut fresh_compressor: FrameCompressor<&[u8], &mut Vec<u8>> =
            FrameCompressor::new(CompressionLevel::Level(level));
        fresh_compressor.compress_independent_frame_into(&data[..], &mut out2);
    } else {
        compressor.set_source(&data[..]);
        compressor.set_drain(&mut out2);
        compressor.compress();
    }

    // Dropped explicitly so the report covers the frame and not the teardown
    // of the buffers above it.
    #[cfg(feature = "dhat-heap")]
    drop(profiler);

    println!("level {level}: {} bytes in, {} out", data.len(), out2.len());
}
