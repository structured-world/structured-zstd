//! A reused compressor handed a different dictionary every few frames, as a
//! dictionary trainer's parameter search prices its candidates: per round the
//! next dictionary is prepared and attached, then frames alternate between the
//! input and its first `alt_len` bytes, so they fall either side of the
//! backend's attach cutoff. `ffi_loop_dict_swap` runs the same schedule
//! through libzstd, a `ZSTD_CDict` per dictionary.
//!
//! Build: cargo build --profile bench -p ffi-bench --example encode_loop_dict_swap
//! Run:   target/release/examples/encode_loop_dict_swap <level> <rounds> <frames> <input> <alt_len> <dict>...

use std::env;

use structured_zstd::encoding::{CompressionLevel, EncoderDictionary, FrameCompressor};

fn main() {
    let args: Vec<String> = env::args().collect();
    let usage = "usage: <level> <rounds> <frames> <input> <alt_len> <dict>...";
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).expect(usage);
    let rounds: usize = args.get(2).and_then(|s| s.parse().ok()).expect(usage);
    let frames: usize = args.get(3).and_then(|s| s.parse().ok()).expect(usage);
    let input = std::fs::read(args.get(4).expect(usage)).expect("read input");
    let alt_len: usize = args.get(5).and_then(|s| s.parse().ok()).expect(usage);
    let dicts: Vec<Vec<u8>> = args[6..]
        .iter()
        .map(|path| std::fs::read(path).expect("read dictionary"))
        .collect();
    assert!(!dicts.is_empty(), "{usage}");
    let short = &input[..alt_len.min(input.len())];

    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(level));
    let mut out = Vec::new();
    let mut total = 0usize;
    for round in 0..rounds {
        let dict = EncoderDictionary::from_serialized_or_raw_content(&dicts[round % dicts.len()])
            .expect("dictionary parses");
        cctx.set_encoder_dictionary(dict)
            .expect("dictionary attaches");
        for frame in 0..frames {
            let src = if frame % 2 == 1 { short } else { &input[..] };
            cctx.compress_independent_frame_into(src, &mut out);
            total += out.len();
            core::hint::black_box(&out);
        }
    }
    eprintln!("{rounds} rounds x {frames} frames at level {level}: {total} bytes out");
}
