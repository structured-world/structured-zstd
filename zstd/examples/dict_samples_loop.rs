//! The samples FastCOVER training scores, compressed with a dictionary through
//! one side, ours or libzstd's: the input cut as `train_loop_fastcover` cuts
//! it, every sample one frame, `iters` passes, the dictionary loaded once into
//! a reused context. The scoring pass of training and nothing else, so a
//! counter or a profile of one side holds only that loop.
//!
//! Run: `cargo run --profile bench -p ffi-bench --example dict_samples_loop
//!       -- <rust|c> <level> <iters> <input> <dict>`

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let side = args.get(1).map_or("rust", String::as_str);
    let level: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(100);
    let src = std::fs::read(args.get(4).expect("input path")).expect("read input");
    let dict = std::fs::read(args.get(5).expect("dict path")).expect("read dict");
    let sample_size = src.len().div_ceil(16).clamp(256, 8192);
    let samples: Vec<&[u8]> = src
        .chunks(sample_size)
        .take(64)
        .filter(|chunk| chunk.len() >= 64)
        .collect();
    let mut total = 0usize;
    match side {
        "rust" => {
            use structured_zstd::encoding::{CompressionLevel, FrameCompressor};
            let mut cctx: FrameCompressor =
                FrameCompressor::new(CompressionLevel::from_level(level));
            cctx.set_dictionary_from_bytes(&dict)
                .expect("dictionary should attach");
            let mut out = Vec::new();
            for _ in 0..iters {
                for sample in &samples {
                    cctx.compress_independent_frame_into(sample, &mut out);
                    total += out.len();
                }
            }
        }
        "c" => {
            let mut cctx =
                zstd::bulk::Compressor::with_dictionary(level, &dict).expect("load dictionary");
            let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(sample_size));
            for _ in 0..iters {
                for sample in &samples {
                    out.clear();
                    cctx.compress_to_buffer(sample, &mut out).expect("compress");
                    total += out.len();
                }
            }
        }
        // A fresh context per frame, the dictionary prepared once: what a
        // caller compressing one small frame at a time pays, table set-up
        // included.
        "rust-fresh" => {
            use structured_zstd::encoding::{CompressionLevel, EncoderDictionary, FrameCompressor};
            let prepared = EncoderDictionary::from_bytes(&dict).expect("dictionary should parse");
            let mut out = Vec::new();
            for _ in 0..iters {
                for sample in &samples {
                    let mut cctx: FrameCompressor =
                        FrameCompressor::new(CompressionLevel::from_level(level));
                    cctx.set_encoder_dictionary(prepared.clone())
                        .expect("dictionary should attach");
                    cctx.compress_independent_frame_into(sample, &mut out);
                    total += out.len();
                }
            }
        }
        "c-fresh" => {
            let prepared = zstd::dict::EncoderDictionary::copy(&dict, level);
            let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(sample_size));
            for _ in 0..iters {
                for sample in &samples {
                    let mut cctx = zstd::bulk::Compressor::with_prepared_dictionary(&prepared)
                        .expect("attach dictionary");
                    out.clear();
                    cctx.compress_to_buffer(sample, &mut out).expect("compress");
                    total += out.len();
                }
            }
        }
        other => panic!("unknown side {other:?}: rust, c, rust-fresh or c-fresh"),
    }
    eprintln!(
        "{} samples x {iters} on {side}: {total} bytes",
        samples.len()
    );
}
