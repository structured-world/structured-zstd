//! Print upstream zstd cParams for levels 1..=22 at a given source size.
//! Companion to `cparams_check` for sweeping a whole level band.
//!
//! Run: `cargo run --release -p ffi-bench --example cparams_range
//!       --features dict_builder -- [src_size]` (0 / omitted = unbounded).
use zstd::zstd_safe::zstd_sys;

fn main() {
    let src_size: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    for level in 1..=22i32 {
        // SAFETY: standard libzstd query.
        let cp = unsafe { zstd_sys::ZSTD_getCParams(level, src_size, 0) };
        println!(
            "L{level}: wlog={} clog={} hlog={} slog={} mml={} tlen={} strat={}",
            cp.windowLog,
            cp.chainLog,
            cp.hashLog,
            cp.searchLog,
            cp.minMatch,
            cp.targetLength,
            cp.strategy as u32
        );
    }
}
