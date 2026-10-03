//! Decodes on a thread whose stack is the budget a kernel task gets.
//!
//! `cargo test` always builds with `panic = "unwind"`, and under unwind LLVM
//! elides a copy of the frame state that a `panic = "abort"` build (every
//! `*-none` target) materialises inside `FrameDecoder::decode_all`. A stack
//! test run by the harness therefore cannot see that regression. This example
//! can: CI runs it in release with `CARGO_PROFILE_RELEASE_PANIC=abort`.
//!
//! A frame that does not fit hits the guard page and the process dies with
//! "has overflowed its stack"; success prints one line per decode path.
//!
//! ```text
//! CARGO_PROFILE_RELEASE_PANIC=abort \
//!     cargo run --release -p ffi-bench --example stack_budget [KiB]
//! ```

use std::io::Read;
use structured_zstd::decoding::{FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

/// The task stack the decoder must fit in, including the caller's frames.
const DEFAULT_BUDGET_KIB: usize = 32;

/// Several blocks of each type: compressed (with sequences), RLE and Raw.
fn mixed_input() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..300_000u32 {
        data.push((i * 7 % 251) as u8 ^ (i >> 9) as u8);
    }
    data.extend(core::iter::repeat_n(0xA5, 200_000));
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..200_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        data.push(x as u8);
    }
    data
}

fn on_budget(kib: usize, name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(kib * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .unwrap_or_else(|_| panic!("{name} panicked"));
    println!("ok: {name} fits {kib} KiB");
}

fn main() {
    let kib = std::env::args()
        .nth(1)
        .map(|s| s.parse().expect("KiB"))
        .unwrap_or(DEFAULT_BUDGET_KIB);

    let data = mixed_input();
    let frame = compress_to_vec(data.as_slice(), CompressionLevel::Default);

    let (d, f) = (data.clone(), frame.clone());
    on_budget(kib, "FrameDecoder::decode_all", move || {
        let mut out = vec![0u8; d.len()];
        let n = FrameDecoder::new()
            .decode_all(&f, &mut out)
            .expect("decode");
        assert_eq!(&out[..n], d.as_slice());
    });

    on_budget(kib, "StreamingDecoder", move || {
        let mut out = Vec::new();
        StreamingDecoder::new(frame.as_slice())
            .expect("header")
            .read_to_end(&mut out)
            .expect("decode");
        assert_eq!(out, data);
    });

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../zstd/dict_tests");
    let read = |name: &str| {
        let path = format!("{dir}/{name}");
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    };
    let dict = read("dictionary");
    let frame = read("files/ModemManager.service.zst");
    let expected = read("files/ModemManager.service");
    on_budget(kib, "FrameDecoder::decode_all_with_dict_bytes", move || {
        let mut out = vec![0u8; expected.len()];
        let n = FrameDecoder::new()
            .decode_all_with_dict_bytes(&frame, &mut out, &dict)
            .expect("decode");
        assert_eq!(&out[..n], expected.as_slice());
    });
}
