//! One-shot diagnostic: ask donor what cParams it selects for our exact
//! (level, srcSize, dictSize) tuple. Confirms whether donor's L1 path
//! uses mls=7 vs mls=4/6 for the 1MB decodecorpus-z000033 fixture.
//!
//! Build: cargo build --release -p structured-zstd --example donor_cparams_check
//! Run:   ./target/release/examples/donor_cparams_check

use zstd::zstd_safe::zstd_sys;

fn main() {
    let src_size = 1022035u64; // bytes in zstd/decodecorpus_files/z000033
    let level = 1i32;
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
