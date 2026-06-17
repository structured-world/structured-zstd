//! Trace driver for #220 Fast L1 ratio gap investigation.
//!
//! Loads `decodecorpus_files/z000033`, runs production encoder at
//! `CompressionLevel::Fastest` (Level 1 → Fast strategy), prints the
//! kernel inner-loop trace from `compress_block_fast` to stderr.
//!
//! Build: `cargo build --release --features kernel_trace --example trace_fast_kernel`
//! Run:   `STRUCTURED_ZSTD_KERNEL_TRACE=1 ./target/release/examples/trace_fast_kernel > /dev/null 2> trace.log`
//!
//! The trace records every outer-iter state, every hash-table put, and
//! every match-found event. Diff vs upstream zstd's expected sequence (manually
//! computed from `zstd_fast.c:266-348`) to pinpoint the first
//! divergence in block 0.
//!
//! Caps trace output at the first 2000 lines via `STRUCTURED_ZSTD_KERNEL_TRACE_HEAD`
//! env var (defaults to 2000) so the log stays diffable. Set to 0 to
//! disable the cap and dump every iteration.

use std::env;
use std::fs;
use std::io::{self, Write};

use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

fn main() {
    let corpus_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());
    let bytes = fs::read(&corpus_path).expect("read corpus");
    eprintln!(
        "TRACE_START corpus={} size={} level=Fastest",
        corpus_path,
        bytes.len()
    );

    // Drive the production encoder. Sequence emissions land via the
    // standard FrameCompressor → MatchGeneratorDriver → FastKernelMatcher
    // → compress_block_fast path, hitting every ktrace! site.
    // CRITICAL: use Level(1), NOT CompressionLevel::Fastest. They look
    // synonymous from the docs ("Fastest is roughly equivalent to Zstd
    // compression level 1") but `Fastest` overrides `LEVEL_TABLE[0]`'s
    // `fast_mls = 7` to `fast_mls = 6` for an even-faster preset. The
    // canonical L1 Fast strategy (which `compare_ffi` benches and the
    // upstream zstd `ZSTD_compress_usingCDict(level=1)` both use) is mls=7 via
    // `LEVEL_TABLE[0]` unchanged → reach it via `Level(1)`.
    let compressed = compress_to_vec(&bytes[..], CompressionLevel::Level(1));

    eprintln!(
        "TRACE_END rust_bytes={} input_bytes={}",
        compressed.len(),
        bytes.len()
    );

    // Write the compressed output to stdout so the binary can also be
    // used as a one-shot compressor for ad-hoc verification.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(&compressed).expect("write compressed");
}
