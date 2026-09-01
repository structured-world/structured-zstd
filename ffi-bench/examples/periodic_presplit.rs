//! Pre-splitter behaviour on a homogeneous but periodic log stream: OUR
//! one-shot frame vs the C reference's one-shot frame (`ZSTD_compress2` via
//! the zstd crate) per level. Used to check that the byChunks sampling tier
//! selected per strategy matches upstream's (an over-split shows up as a
//! ballooned lazy2 / btlazy2 output next to the lazy level).

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

fn main() {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let target: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(512 * 1024);
    let mut data = Vec::with_capacity(target);
    let mut i = 0;
    while data.len() < target {
        let line = LINES[i % LINES.len()].as_bytes();
        let take = line.len().min(target - data.len());
        data.extend_from_slice(&line[..take]);
        i += 1;
    }
    println!("periodic stream {} bytes", data.len());
    for lvl in [3i32, 5, 7, 8, 11, 15, 16] {
        let ours = compress_slice_to_vec(&data, CompressionLevel::Level(lvl));
        let c = zstd::bulk::compress(&data, lvl).expect("c compress");
        println!(
            "L{lvl:<3} ours={:>7}B  C={:>7}B  ours/C={:.3}",
            ours.len(),
            c.len(),
            ours.len() as f64 / c.len() as f64,
        );
    }
}
