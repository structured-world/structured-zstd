//! Standalone decode-loop binary for CLEAN perf-record profiles of the
//! dictionary DECODE hot path. Mirrors the `decompress-dict/...` bench arm
//! exactly: parse the dictionary into a [`DictionaryHandle`] ONCE, build one
//! reused [`FrameDecoder`], then loop `decode_all_with_dict_handle` over a
//! fixed `.zst` payload into a preallocated output buffer — no criterion, no
//! FFI, no training; samples land purely on the per-frame decode path.
//!
//! The payload should be the FFI dict-encoded frame the bench measures; dump
//! both files from a bench run via
//! `STRUCTURED_ZSTD_DUMP_DICT_DIR=<dir> cargo bench --bench compare_ffi ...`
//! (`<scenario>.dict` + `<scenario>.<level>.zst`).
//!
//! Build: `cargo build --profile flamegraph -p structured-zstd
//!          --example decode_loop_dict`
//! Run:   `decode_loop_dict <iters> <payload.zst> <dict> <expected_len>`

use std::env;

use structured_zstd::decoding::{DictionaryHandle, FrameDecoder};

fn main() {
    let args: Vec<String> = env::args().collect();
    let iters: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(500_000);
    let payload_path = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("/tmp/szstd-dicts/small-4k-log-lines.level_2_fast.zst");
    let dict_path = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("/tmp/szstd-dicts/small-4k-log-lines.dict");
    let expected_len: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(4096);

    let payload = std::fs::read(payload_path).expect("read payload file");
    let dict = std::fs::read(dict_path).expect("read dict file");
    let handle = DictionaryHandle::decode_dict(dict.as_slice()).expect("parse dictionary");

    let mut decoder = FrameDecoder::new();
    let mut output = vec![0u8; expected_len];
    let mut sink: usize = 0;
    for _ in 0..iters {
        let n = decoder
            .decode_all_with_dict_handle(payload.as_slice(), output.as_mut_slice(), &handle)
            .expect("dict decode should succeed");
        // A short decode means the (payload, dict, expected_len) triple is
        // mismatched — fail loudly instead of profiling the wrong workload.
        assert_eq!(n, expected_len, "decoded length mismatch");
        sink = sink.wrapping_add(n);
        core::hint::black_box(&output[..n]);
    }

    eprintln!(
        "decoded {} bytes x {} iters (payload {} bytes, dict {} bytes); out-sum={}",
        expected_len,
        iters,
        payload.len(),
        dict.len(),
        sink
    );
}
