//! Per-level speed comparison on the decodecorpus fixtures: structured-zstd
//! (this crate) vs C libzstd vs zrip (third-party pure-Rust).
//!
//! Framing (do not confuse the axes):
//!   * SPEED is the optimization target. For each level we want our encode and
//!     decode throughput to be >= C (`ours/C >= 1.0`). zrip/C shows what a pure-
//!     Rust codec already achieves, i.e. the headroom that is reachable without
//!     leaving Rust.
//!   * RATIO is only a floor: "not worse than C" (`ours_ratio >= C_ratio`).
//!     Compressing *better* than C buys nothing and, on the fast/negative
//!     levels, usually means we did extra work and lost speed. A level that
//!     beats C on ratio while losing on speed is a BUG, not a win.
//!
//! Run: cargo run --release -p ffi-bench --example zrip_compare
//! Every codec's output is round-tripped and checked == input before timing.

use std::time::Instant;

use structured_zstd::decoding::FrameDecoder;
use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

fn corpus(name: &str) -> Option<Vec<u8>> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../zstd/decodecorpus_files")
        .join(name);
    std::fs::read(p).ok()
}

fn mbps(bytes: usize, ns: u128) -> f64 {
    if ns == 0 {
        return 0.0;
    }
    bytes as f64 / ns as f64 * 1000.0
}

/// Format an optional zrip/C ratio for the table: `N/A` (at the `{:>5.2}` ratio
/// width) when zrip does not support the level, the ratio otherwise.
fn fz(o: Option<f64>) -> String {
    o.map_or_else(|| "  N/A".to_string(), |v| format!("{v:>5.2}"))
}

fn time_ns(iters: usize, mut f: impl FnMut()) -> u128 {
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    t.elapsed().as_nanos() / iters as u128
}

/// One measured (fixture, level) cell.
struct Cell {
    fixture: &'static str,
    level: i32,
    // ours / C  and  zrip / C  for encode and decode throughput. The zrip
    // ratios are `None` for levels zrip does not support (carried as N/A, not
    // 0.00x, so they aren't mistaken for real throughput misses).
    enc_o_c: f64,
    enc_z_c: Option<f64>,
    dec_o_c: f64,
    dec_z_c: Option<f64>,
    // ours_ratio / C_ratio: >= 1.0 means the "not worse than C" floor holds.
    ratio_o_c: f64,
}

fn run_fixture(
    name: &'static str,
    data: &[u8],
    iters: usize,
    levels: &[i32],
    cells: &mut Vec<Cell>,
) {
    println!("\n=== {name}  ({} bytes) ===", data.len());
    println!(
        "{:>4} | {:>11} | {:>11} | {:>14}",
        "lvl", "enc o/C z/C", "dec o/C z/C", "ratio o/C (floor)"
    );
    let orig = data.len();
    let mut out = vec![0u8; orig.max(1)];

    for &lvl in levels {
        // structured-zstd
        let sz_comp = compress_slice_to_vec(data, CompressionLevel::Level(lvl));
        let mut dec = FrameDecoder::new();
        let n = dec.decode_all(&sz_comp, &mut out).expect("sz decode");
        let sz_ratio = orig as f64 / sz_comp.len() as f64;
        let ok = n == orig && out[..n] == *data;
        assert!(ok, "structured-zstd roundtrip failed for {name} L{lvl}");
        let sz_enc = time_ns(iters, || {
            let _ = compress_slice_to_vec(data, CompressionLevel::Level(lvl));
        });
        let sz_dec = time_ns(iters, || {
            let mut d = FrameDecoder::new();
            let _ = d.decode_all(&sz_comp, &mut out);
        });

        // C libzstd
        let cc = zstd::bulk::compress(data, lvl).expect("c compress");
        let c_decoded = zstd::bulk::decompress(&cc, orig.max(1)).expect("c decode");
        assert_eq!(
            c_decoded, *data,
            "libzstd roundtrip failed for {name} L{lvl}"
        );
        let c_ratio = orig as f64 / cc.len() as f64;
        let c_enc = time_ns(iters, || {
            let _ = zstd::bulk::compress(data, lvl);
        });
        let c_dec = time_ns(iters, || {
            let _ = zstd::bulk::decompress(&cc, orig);
        });

        // zrip (best-effort: not every level may be supported -> `None`).
        let zr = match zrip::compress(data, lvl) {
            Ok(zc) => {
                let zr_decoded = zrip::decompress(&zc).expect("zrip decode");
                assert_eq!(zr_decoded, *data, "zrip roundtrip failed for {name} L{lvl}");
                Some((
                    time_ns(iters, || {
                        let _ = zrip::compress(data, lvl);
                    }),
                    time_ns(iters, || {
                        let _ = zrip::decompress(&zc);
                    }),
                ))
            }
            Err(_) => None,
        };

        let sz_enc_mb = mbps(orig, sz_enc);
        let sz_dec_mb = mbps(orig, sz_dec);
        let c_enc_mb = mbps(orig, c_enc);
        let c_dec_mb = mbps(orig, c_dec);
        let safe = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.0 };
        let (enc_z_c, dec_z_c) = match zr {
            Some((e, d)) => (
                Some(safe(mbps(orig, e), c_enc_mb)),
                Some(safe(mbps(orig, d), c_dec_mb)),
            ),
            None => (None, None),
        };

        let cell = Cell {
            fixture: name,
            level: lvl,
            enc_o_c: safe(sz_enc_mb, c_enc_mb),
            enc_z_c,
            dec_o_c: safe(sz_dec_mb, c_dec_mb),
            dec_z_c,
            ratio_o_c: safe(sz_ratio, c_ratio),
        };
        println!(
            "{:>4} | {:>5.2} {} | {:>5.2} {} | {:>8.2} {}{}",
            lvl,
            cell.enc_o_c,
            fz(cell.enc_z_c),
            cell.dec_o_c,
            fz(cell.dec_z_c),
            cell.ratio_o_c,
            if cell.ratio_o_c >= 0.999 {
                "OK  "
            } else {
                "BELOW"
            },
            if ok { "" } else { "  !! ROUNDTRIP" },
        );
        cells.push(cell);
    }
}

fn main() {
    let fixtures = [
        ("z000001", 50_000), // ~24 B
        ("z000002", 20_000), // ~2.7 KB
        ("z000000", 500),    // ~224 KB
        ("z000033", 80),     // ~1 MB
    ];
    // Negative/fast band first (where speed dominates and we are weakest),
    // then the strategy transitions up to the high levels.
    let levels = [-7i32, -5, -3, -1, 1, 2, 3, 4, 5, 7, 9, 12, 19];

    let mut cells = Vec::new();
    for (name, iters) in fixtures {
        match corpus(name) {
            Some(data) if !data.is_empty() => run_fixture(name, &data, iters, &levels, &mut cells),
            _ => eprintln!("skip {name}: not found"),
        }
    }

    // Summary: where we MISS the speed goal (slower than C), and any ratio-floor
    // violation (compressing worse than C, which the drop-in contract forbids).
    println!("\n===== SPEED MISSES (ours < C; goal ours/C >= 1.0) =====");
    let mut enc_miss: Vec<&Cell> = cells.iter().filter(|c| c.enc_o_c < 0.95).collect();
    enc_miss.sort_by(|a, b| a.enc_o_c.partial_cmp(&b.enc_o_c).unwrap());
    println!("-- encode (worst first) --");
    for c in enc_miss.iter().take(20) {
        println!(
            "  {:<9} L{:<3} ours/C {:.2}×  (zrip/C {})",
            c.fixture,
            c.level,
            c.enc_o_c,
            c.enc_z_c
                .map_or_else(|| "N/A".to_string(), |v| format!("{v:.2}×")),
        );
    }
    let mut dec_miss: Vec<&Cell> = cells.iter().filter(|c| c.dec_o_c < 0.95).collect();
    dec_miss.sort_by(|a, b| a.dec_o_c.partial_cmp(&b.dec_o_c).unwrap());
    println!("-- decode (worst first) --");
    for c in dec_miss.iter().take(20) {
        println!(
            "  {:<9} L{:<3} ours/C {:.2}×  (zrip/C {})",
            c.fixture,
            c.level,
            c.dec_o_c,
            c.dec_z_c
                .map_or_else(|| "N/A".to_string(), |v| format!("{v:.2}×")),
        );
    }

    println!("\n===== RATIO FLOOR VIOLATIONS (ours worse than C) =====");
    let viol: Vec<&Cell> = cells.iter().filter(|c| c.ratio_o_c < 0.999).collect();
    if viol.is_empty() {
        println!("  none — ratio floor holds everywhere");
    } else {
        for c in viol {
            println!(
                "  {:<9} L{:<3} ratio ours/C {:.3}×",
                c.fixture, c.level, c.ratio_o_c
            );
        }
    }
}
