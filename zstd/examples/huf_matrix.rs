//! Full perf + ratio matrix: per (fixture size, level), compressed size and
//! compress time for structured-zstd with the #167 HUF table-log search ON
//! (default) and OFF (cheap single-build), plus the C reference. Reports the
//! size and speed of each against C. Run on the bench host for timing.
//!
//! Build/run: `cargo run --release -p ffi-bench --example huf_matrix
//!             --features bench_internals`

use std::time::Instant;

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};
use structured_zstd::testing::set_force_cheap_huf;

fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let mut b = Vec::with_capacity(len);
    while b.len() < len {
        for l in LINES {
            if b.len() == len {
                break;
            }
            let r = len - b.len();
            b.extend_from_slice(&l.as_bytes()[..l.len().min(r)]);
        }
    }
    b
}

fn time_min<F: FnMut() -> usize>(iters: u32, mut f: F) -> (f64, usize) {
    let mut best = f64::MAX;
    let mut out = 0;
    for _ in 0..iters {
        let t = Instant::now();
        out = f();
        let us = t.elapsed().as_secs_f64() * 1e6;
        if us < best {
            best = us;
        }
        std::hint::black_box(out);
    }
    (best, out)
}

fn main() {
    let fixtures: &[(&str, usize)] = &[
        ("logs-1k", 1024),
        ("logs-4k", 4096),
        ("logs-64k", 64 * 1024),
        ("logs-1M", 1024 * 1024),
    ];
    let levels: &[i32] = &[-5, -3, -1, 1, 2, 3, 6, 9, 12, 16, 19, 22];

    println!(
        "{:<9} {:>3} | {:>6} {:>7} | {:>6} {:>7} | {:>6} {:>7} | on:sz/sp   off:sz/sp",
        "fixture", "lvl", "on_B", "on_us", "off_B", "off_us", "c_B", "c_us"
    );
    println!("{}", "-".repeat(92));
    for (name, size) in fixtures {
        let data = repeated_log_lines(*size);
        let iters: u32 = if *size <= 4096 {
            20000
        } else if *size <= 64 * 1024 {
            3000
        } else {
            300
        };
        for &level in levels {
            set_force_cheap_huf(false);
            let (on_us, on_b) = time_min(iters, || {
                compress_slice_to_vec(&data, CompressionLevel::Level(level)).len()
            });
            set_force_cheap_huf(true);
            let (off_us, off_b) = time_min(iters, || {
                compress_slice_to_vec(&data, CompressionLevel::Level(level)).len()
            });
            set_force_cheap_huf(false);
            let (c_us, c_b) = time_min(iters, || {
                zstd::bulk::compress(&data, level).expect("c").len()
            });

            let on_sz = on_b as f64 / c_b as f64;
            let on_sp = on_us / c_us;
            let off_sz = off_b as f64 / c_b as f64;
            let off_sp = off_us / c_us;
            println!(
                "{name:<9} {level:>3} | {on_b:>6} {on_us:>7.2} | {off_b:>6} {off_us:>7.2} | {c_b:>6} {c_us:>7.2} | {on_sz:.2}/{on_sp:.2}x  {off_sz:.2}/{off_sp:.2}x"
            );
        }
        println!();
    }
}
