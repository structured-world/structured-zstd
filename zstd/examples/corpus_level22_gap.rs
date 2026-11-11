//! Level-22 ratio probe on z000033: our frame against libzstd's, with the
//! block-type breakdown of each, to tell a gap in block split decisions from
//! one in per-block entropy.
//!
//! Run: cargo run --release -p ffi-bench --example corpus_level22_gap

use std::fs;
use std::path::Path;
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

#[path = "support/block_breakdown.rs"]
mod block_breakdown;

fn main() {
    // Built from the sibling `ffi-bench` crate, so resolve the fixture next to
    // the manifest first, then under the sibling `zstd` crate.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let here = manifest.join("decodecorpus_files/z000033");
    let path = if here.is_file() {
        here
    } else {
        manifest.join("../zstd/decodecorpus_files/z000033")
    };
    let data = fs::read(&path).expect("z000033 corpus file present");
    println!("Corpus z000033 size: {} bytes", data.len());

    let rust = compress_to_vec(&data[..], CompressionLevel::Level(22));
    let c = zstd::bulk::compress(&data[..], 22).unwrap();
    let delta = rust.len() as i64 - c.len() as i64;
    let pct = (rust.len() as f64 / c.len() as f64 - 1.0) * 100.0;
    println!(
        "Level 22 output:  rust={}  c={}  delta={delta:+}b ({pct:+.4}%)",
        rust.len(),
        c.len()
    );

    block_breakdown::dump_block_breakdown("Rust", &rust);
    block_breakdown::dump_block_breakdown("C   ", &c);

    // Sanity: our output must roundtrip through libzstd.
    let rt = zstd::bulk::decompress(&rust, data.len() + 64).unwrap();
    assert_eq!(rt, data, "rust output decompresses to different bytes");
}
