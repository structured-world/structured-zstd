//! One-shot diagnostic: ask upstream zstd which cParams it selects for a
//! given (level, srcSize, dictSize=0) tuple via `ZSTD_getCParams`. Useful
//! for checking our per-level table widths (windowLog / hashLog / chainLog)
//! against upstream's source-size-adjusted values.
//!
//! Build: cargo build --release -p structured-zstd --example donor_cparams_check
//! Run:   ./target/release/examples/donor_cparams_check [level] [src_size]
//!        level    compression level (default 1)
//!        src_size source size in bytes for the size hint (default 1022035,
//!                 the decodecorpus-z000033 fixture; 0 = unknown/unbounded)

use zstd::zstd_safe::zstd_sys;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let src_size: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1022035);
    let dict_size = 0usize;

    // SAFETY: standard libzstd query.
    let cp = unsafe { zstd_sys::ZSTD_getCParams(level, src_size, dict_size) };

    println!("L{level} srcSize={src_size} dictSize={dict_size}:");
    println!("  windowLog = {}", cp.windowLog);
    println!("  chainLog  = {}", cp.chainLog);
    println!("  hashLog   = {}", cp.hashLog);
    println!("  searchLog = {}", cp.searchLog);
    println!("  minMatch  = {} (mls)", cp.minMatch);
    println!("  targetLength = {}", cp.targetLength);
    println!("  strategy  = {}", cp.strategy as u32);
}
