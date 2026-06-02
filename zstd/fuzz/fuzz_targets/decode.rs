#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use std::io::{self, Read};

fuzz_target!(|data: &[u8]| {
    if let Ok(decoder) = structured_zstd::decoding::StreamingDecoder::new(data) {
        // Cap decoded output: an adversarial frame can legitimately
        // decompress to far more than its input (RLE / repeated matches),
        // so an unbounded read would OOM the fuzzer materializing a
        // multi-GB decompression bomb. Stream the bounded output through
        // `io::sink()` so the decoder still walks every decode path
        // (up to 256 MiB) without ever buffering it in memory.
        let _ = io::copy(&mut decoder.take(256 << 20), &mut io::sink());
    }
});
