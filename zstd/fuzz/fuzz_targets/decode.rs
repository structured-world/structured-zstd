#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use std::io::Read;

fuzz_target!(|data: &[u8]| {
    if let Ok(decoder) = structured_zstd::decoding::StreamingDecoder::new(data) {
        let mut output = Vec::new();
        // Cap decoded output: an adversarial frame can legitimately
        // decompress to far more than its input (RLE / repeated matches),
        // so an unbounded `read_to_end` would OOM the fuzzer materializing
        // a multi-GB decompression bomb. The decoder streams correctly;
        // bounding total output is the caller's policy. 256 MiB exercises
        // every decode path without the OOM risk.
        let _ = decoder.take(256 << 20).read_to_end(&mut output);
    }
});
