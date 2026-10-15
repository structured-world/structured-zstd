//! Decode loop comparing the caller-workspace decoder with the existing ones
//! on the same build, for `perf stat` A/B runs.
//!
//! ```text
//! workspace_decode_loop <level> <iters> <mode> [file|synthetic] [window_log]
//! ```
//!
//! `mode` is `workspace` (the step decoder, 64 KiB input and output chunks),
//! `stream` (`StreamingDecoder` read through a 64 KiB buffer, the existing
//! streaming path) or `all` (`FrameDecoder::decode_all` into one buffer). The
//! input is compressed once with this crate's encoder at `level`; the default
//! file is the in-tree `decodecorpus_files/z000033`, and `synthetic` is random
//! bytes, which compress to Raw blocks. `window_log` caps the encoder's window
//! (default: the level's own) so small and large windows can be compared.

use std::hint::black_box;
use std::io::Read;

use structured_zstd::decoding::workspace::{
    DecodeLimits, StepState, WorkBudget, WorkspaceDecoder, workspace_bytes,
};
use structured_zstd::decoding::{FrameDecoder, StreamingDecoder, read_frame_header_info};
use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, compress_with_parameters,
};

const CHUNK: usize = 64 * 1024;

fn source(arg: Option<&str>) -> Vec<u8> {
    match arg {
        Some(s) if s.eq_ignore_ascii_case("synthetic") => {
            let mut x = 0x517c_c1b7_2722_0a95u64;
            (0..(1 << 20))
                .map(|_| {
                    x = x
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (x >> 56) as u8
                })
                .collect()
        }
        Some(path) => std::fs::read(path).expect("read input file"),
        None => {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/decodecorpus_files/z000033");
            std::fs::read(path).expect("read decodecorpus_files/z000033")
        }
    }
}

fn compress(data: &[u8], level: i32, window_log: Option<u32>) -> Vec<u8> {
    let level = CompressionLevel::Level(level);
    let mut parameters = CompressionParameters::builder(level);
    if let Some(log) = window_log {
        parameters = parameters.window_log(log);
    }
    compress_with_parameters(data, &parameters.build().expect("parameters"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1_000);
    let mode = args.get(3).map_or("workspace", String::as_str);
    let data = source(args.get(4).map(String::as_str));
    let window_log: Option<u32> = args.get(5).and_then(|s| s.parse().ok());

    let compressed = compress(&data, level, window_log);
    let window = read_frame_header_info(&compressed, false)
        .expect("frame header")
        .window_size
        .max(1024);
    eprintln!(
        "{} bytes -> {} at level {level}, window {window}, mode {mode}",
        data.len(),
        compressed.len()
    );

    let mut total = 0u64;
    let start = std::time::Instant::now();
    match mode {
        "workspace" => {
            let limits = DecodeLimits::new(window);
            let mut workspace = vec![0u8; workspace_bytes(window).expect("window")];
            let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).expect("workspace");
            let mut out = vec![0u8; CHUNK];
            for _ in 0..iters {
                decoder.reset();
                let mut work = WorkBudget::new(u64::MAX);
                let mut input = black_box(compressed.as_slice());
                loop {
                    let chunk = &input[..input.len().min(CHUNK)];
                    let last = chunk.len() == input.len();
                    let progress = decoder
                        .step(chunk, last, &mut out, &mut work)
                        .expect("workspace decode");
                    total += black_box(&out[..progress.produced]).len() as u64;
                    input = &input[progress.consumed..];
                    if progress.state == StepState::Done {
                        break;
                    }
                }
            }
        }
        "stream" => {
            let mut out = vec![0u8; CHUNK];
            for _ in 0..iters {
                let mut decoder = StreamingDecoder::new(black_box(compressed.as_slice()));
                loop {
                    let n = decoder.read(&mut out).expect("stream decode");
                    if n == 0 {
                        break;
                    }
                    total += black_box(&out[..n]).len() as u64;
                }
            }
        }
        "all" => {
            let mut target = vec![0u8; data.len() + 64];
            let mut decoder = FrameDecoder::new();
            for _ in 0..iters {
                total += decoder
                    .decode_all(black_box(&compressed), &mut target)
                    .expect("decode_all") as u64;
            }
        }
        other => panic!("unknown mode {other}; use workspace, stream or all"),
    }
    let elapsed = start.elapsed();
    assert_eq!(total, data.len() as u64 * u64::from(iters));
    eprintln!(
        "{iters} iters, {:.0} ns/iter, {:.1} MB/s",
        elapsed.as_nanos() as f64 / f64::from(iters),
        total as f64 / elapsed.as_secs_f64() / 1e6
    );
}
