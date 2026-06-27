//! Tight small-input compress loop for flamegraph profiling. Isolates the
//! per-frame encoder cost that dominates small frames (where setup / entropy /
//! table work is not amortised over many bytes). Pure encoder, no FFI.
//!
//! Usage: encode_small [level] [iters]   (defaults: level 3, 2_000_000 iters)
//! Profile on the i9: cargo flamegraph --example encode_small --features dict_builder -- 3 2000000

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

fn main() {
    let data = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../zstd/decodecorpus_files/z000002"),
    )
    .expect("z000002 fixture");
    let level: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);

    let mut sink = 0u64;
    for _ in 0..iters {
        let out = compress_slice_to_vec(&data, CompressionLevel::Level(level));
        sink = sink.wrapping_add(out.len() as u64);
    }
    println!(
        "done level={level} iters={iters} input={} sink={sink}",
        data.len()
    );
}
