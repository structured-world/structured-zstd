//! Hash the owned (reader-path) frame for every level, so the in-place ingest
//! can be proven byte-identical to the staged path it replaced.
use std::env;
use std::fs;

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

fn main() {
    let corpus = env::args()
        .nth(1)
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());
    let data = fs::read(&corpus).expect("read corpus");
    for level in 1i32..=22 {
        let mut out = Vec::new();
        let mut fc: FrameCompressor<&[u8], &mut Vec<u8>> =
            FrameCompressor::new(CompressionLevel::Level(level));
        fc.set_source_size_hint(data.len() as u64);
        fc.set_source(&data[..]);
        fc.set_drain(&mut out);
        fc.compress();
        // Cheap content digest; a byte change anywhere moves it.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in &out {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        println!("level {level:2}: len={} fnv={h:016x}", out.len());
    }
}
