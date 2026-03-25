#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use structured_zstd::huff0::round_trip;

fuzz_target!(|data: &[u8]| {
    round_trip(data);
});
