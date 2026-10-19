#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use std::io::Read;

use structured_zstd::decoding::{
    FrameDecoder, LegacyVersion, StreamingDecoder, frame_decompressed_bound,
};

/// Output either path may produce before the input is set aside: enough for
/// many blocks, small enough that a bomb costs nothing.
const OUTPUT_CAP: usize = 4 << 20;

fuzz_target!(|data: &[u8]| {
    // Keep the input on a v0.5-v0.7 frame: when it does not open with one of
    // their magic numbers, its first byte picks one and the number replaces
    // its first four bytes. The fixtures in `zstd/legacy_frames` seed it as
    // they are.
    if data.len() < 4 {
        return;
    }
    let mut input = data.to_vec();
    if LegacyVersion::of(&input).is_none() {
        let minor = 0x25 + input[0] % 3;
        input[..4].copy_from_slice(&[minor, 0xB5, 0x2F, 0xFD]);
    }

    // The whole-buffer path, into a slice the frame's bound sizes.
    let Ok(bound) = frame_decompressed_bound(&input) else {
        // A frame whose blocks do not walk still goes to the stream, which
        // must refuse it without panicking.
        drain(&input, OUTPUT_CAP);
        return;
    };
    let room = usize::try_from(bound).map_or(OUTPUT_CAP, |bound| bound.min(OUTPUT_CAP));
    let mut whole = vec![0u8; room];
    let decoded = FrameDecoder::new().decode_all(&input, &mut whole);

    // The streaming path, a block at a time with history dropped behind it.
    let streamed = drain(&input, room + 1);

    // Both paths decode the same frames to the same bytes.
    if let (Ok(len), Some(streamed)) = (decoded, streamed) {
        assert!(
            streamed == whole[..len],
            "the stream and the whole-buffer decode disagree"
        );
    }
});

/// Read up to `cap` bytes of `input` through a `StreamingDecoder`; `None`
/// when the stream fails.
fn drain(input: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    StreamingDecoder::new(input)
        .take(cap as u64)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}
