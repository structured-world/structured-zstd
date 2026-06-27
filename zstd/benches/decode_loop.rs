//! Tight decode loop (no criterion harness) for clean callgrind / perf
//! profiling of the decode hot path. Criterion's statistical machinery
//! (`exp`, rayon) swamps the Ir under callgrind, drowning the decode itself;
//! this plain loop decodes the z000033 frame N times so the decode functions
//! dominate the profile.
//!
//! Run:  cargo build --release --bench decode_loop  (then run the binary)
//! Callgrind:  valgrind --tool=callgrind <binary> 20

use structured_zstd::decoding::FrameDecoder;

fn main() {
    let src = include_bytes!("../decodecorpus_files/z000033.zst");
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let mut fr = FrameDecoder::new();
    let target = &mut vec![0u8; 4 * 1024 * 1024];
    let mut sink = 0u64;
    for _ in 0..n {
        fr.decode_all(src, target).unwrap();
        sink = sink.wrapping_add(target[0] as u64);
    }
    println!("done n={n} sink={sink}");
}
