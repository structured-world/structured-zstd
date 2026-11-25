//! Trains a FastCOVER dictionary N times through one side, ours or libzstd's,
//! on the samples and dictionary size the `compare_ffi` dict-train cell uses,
//! so a profile or a cycle count of one side holds nothing but its training.
//!
//! Both sides run `ZDICT_trainFromBuffer`'s search: FastCOVER with `d` 8, `k`
//! searched in four steps, scored at the default level, on one thread.
//!
//! Run: `cargo run --profile bench -p ffi-bench --example train_loop_fastcover
//!       -- <rust|c> <iters> <corpus_path>`

use std::env;

use structured_zstd::dictionary::{FastCoverOptions, FinalizeOptions, optimize_fastcover_dict};

/// The bench's samples: consecutive chunks of the input, `len / 16` each
/// clamped to 256..=8192 bytes, at most 64 of them, none under 64 bytes.
fn training_samples(source: &[u8]) -> Vec<&[u8]> {
    let sample_size = source.len().div_ceil(16).clamp(256, 8192);
    source
        .chunks(sample_size)
        .take(64)
        .filter(|chunk| chunk.len() >= 64)
        .collect()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let side = args.get(1).map_or("rust", String::as_str);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let path = args.get(3).expect("usage: <rust|c> <iters> <corpus_path>");
    let source = std::fs::read(path).expect("read corpus file");

    let samples = training_samples(&source);
    let sizes: Vec<usize> = samples.iter().map(|sample| sample.len()).collect();
    let corpus = &source[..sizes.iter().sum::<usize>()];
    // An eighth of the input, clamped to 256 B..16 KiB, as the bench sizes it.
    let dict_size = source.len().div_ceil(8).clamp(256, 16 * 1024);

    let mut sink = 0usize;
    for _ in 0..iters {
        let dict = match side {
            "rust" => {
                optimize_fastcover_dict(
                    corpus,
                    &sizes,
                    dict_size,
                    &FastCoverOptions::default(),
                    FinalizeOptions::default(),
                )
                .expect("FastCOVER training")
                .0
            }
            "c" => zstd::dict::from_samples(&samples, dict_size).expect("ZDICT_trainFromBuffer"),
            other => panic!("unknown side {other:?}: rust or c"),
        };
        sink = sink.wrapping_add(dict.len());
        core::hint::black_box(&dict);
    }
    eprintln!(
        "trained {} samples ({} bytes) into {dict_size} B x {iters} on {side}; sum={sink}",
        samples.len(),
        corpus.len()
    );
}
