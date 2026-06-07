#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use structured_zstd::decoding::FrameDecoder;

// Direct-decode (`FrameDecoder::decode_all` into a fixed-capacity slice)
// adversarial-input harness for #246. The existing `decode` target only
// fuzzes the growable `StreamingDecoder` path (FlatBuf / RingBuffer, which
// can never overflow because the backing Vec grows on demand). The direct
// path routes through `UserSliceBackend`, whose writes are bounded by the
// caller-provided slice — a malformed frame that expands past the declared
// `frame_content_size` MUST surface a structured error, never panic or
// write out of bounds. This target drives exactly that path.
fuzz_target!(|data: &[u8]| {
    // A fixed output slice forces the direct path when the frame's
    // declared content size fits. Cap at 1 MiB so an adversarial frame
    // declaring a huge `frame_content_size` can't OOM the fuzzer via the
    // up-front output allocation; frames larger than this fall back to the
    // growable path (still panic-free) or are rejected. The decoder's own
    // window cap (100 MiB) and the per-block fallible write surface are
    // what we're exercising — any panic here is a bug.
    let mut out = vec![0u8; 1 << 20];
    let mut dec = FrameDecoder::new();
    let _ = dec.decode_all(data, &mut out);
});
