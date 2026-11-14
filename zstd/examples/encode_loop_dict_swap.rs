//! A reused compressor handed a different dictionary every few frames, as a
//! dictionary trainer's parameter search prices its candidates: per round the
//! next dictionary is prepared and attached, then frames walk the input's
//! consecutive `frame_len`-byte pieces, every second one cut to its first
//! `alt_len` bytes, so they fall either side of the backend's attach cutoff.
//! `ffi_loop_dict_swap` runs the same schedule through libzstd.
//!
//! The compressor is set up as the benchmark's dictionary cells set theirs:
//! the numeric level, every block searched, the content checksum when the
//! `hash` feature is on, and the dictionary parsed from its file (a
//! dictionary in zstd's format, or raw content when it carries no magic).
//!
//! Build: cargo build --profile bench -p ffi-bench --example encode_loop_dict_swap
//! Run:   target/release/examples/encode_loop_dict_swap <level> <rounds> <frames> <input> <frame_len> <alt_len> <dict>...

use std::env;

use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, EncoderDictionary, FrameCompressor,
};

fn main() {
    let args: Vec<String> = env::args().collect();
    let usage = "usage: <level> <rounds> <frames> <input> <frame_len> <alt_len> <dict>...";
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).expect(usage);
    let rounds: usize = args.get(2).and_then(|s| s.parse().ok()).expect(usage);
    let frames: usize = args.get(3).and_then(|s| s.parse().ok()).expect(usage);
    let input = std::fs::read(args.get(4).expect(usage)).expect("read input");
    let frame_len: usize = args.get(5).and_then(|s| s.parse().ok()).expect(usage);
    let alt_len: usize = args.get(6).and_then(|s| s.parse().ok()).expect(usage);
    let dicts: Vec<Vec<u8>> = args[7..]
        .iter()
        .map(|path| std::fs::read(path).expect("read dictionary"))
        .collect();
    assert!(!dicts.is_empty(), "{usage}");
    let pieces: Vec<&[u8]> = input.chunks_exact(frame_len).collect();
    assert!(!pieces.is_empty(), "the input holds no whole frame");

    // `Level(n)`, as the benchmark builds it: the named presets `from_level`
    // returns for some levels take paths of their own.
    let compression_level = CompressionLevel::Level(level);
    let mut cctx: FrameCompressor = FrameCompressor::new(compression_level);
    let params = CompressionParameters::builder(compression_level)
        .raw_skip(false)
        .build()
        .expect("a level and the raw skip are always valid parameters");
    cctx.set_parameters(&params);
    cctx.set_content_checksum(cfg!(feature = "hash"));
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut call = 0usize;
    for round in 0..rounds {
        let dict = EncoderDictionary::from_serialized_or_raw_content(&dicts[round % dicts.len()])
            .expect("dictionary parses");
        cctx.set_encoder_dictionary(dict)
            .expect("dictionary attaches");
        for frame in 0..frames {
            let piece = pieces[call % pieces.len()];
            call += 1;
            let src = if frame % 2 == 1 {
                &piece[..alt_len.min(piece.len())]
            } else {
                piece
            };
            cctx.compress_independent_frame_into(src, &mut out);
            total += out.len();
            core::hint::black_box(&out);
        }
    }
    eprintln!("{rounds} rounds x {frames} frames at level {level}: {total} bytes out");
}
