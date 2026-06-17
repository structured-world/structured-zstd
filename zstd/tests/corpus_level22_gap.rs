use std::fs;
use std::path::Path;
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

mod common;

#[test]
#[ignore = "manual probe — run with --ignored"]
fn corpus_z000033_level22_ratio_breakdown() {
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

    // Count block types in each output so we can tell if the gap is
    // structural (different block split decisions) or content
    // (per-block entropy differences within identical splits).
    common::dump_block_breakdown("Rust", &rust);
    common::dump_block_breakdown("C   ", &c);

    // Sanity: outputs must roundtrip.
    let rt = zstd::bulk::decompress(&rust, data.len() + 64).unwrap();
    assert_eq!(
        rt.len(),
        data.len(),
        "rust output decompresses to wrong size"
    );
    assert_eq!(rt, data, "rust output decompresses to different bytes");
}
