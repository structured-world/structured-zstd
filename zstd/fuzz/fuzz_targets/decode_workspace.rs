#![no_main]
#[macro_use]
extern crate libfuzzer_sys;

use structured_zstd::decoding::workspace::{
    DecodeLimits, StepState, WorkBudget, WorkspaceDecodeError, WorkspaceDecoder, workspace_bytes,
};
use structured_zstd::decoding::{ContentChecksum, FrameDecoder};

const MAX_OUTPUT: usize = 1 << 20;

// The first four bytes choose the window limit, the input and output chunk
// sizes and the work budget; the rest is the stream. Whatever they are, the
// decoder must not panic or stall, and where it decodes or rejects for a
// format reason the one-shot decoder must agree.
fuzz_target!(|data: &[u8]| {
    let Some((&[w, i, o, b], stream)) = data.split_first_chunk::<4>() else {
        return;
    };
    let window = 1u64 << (10 + u32::from(w % 11));
    let in_chunk = usize::from(i).max(1);
    let out_chunk = usize::from(o).max(1);
    let budget = if b == 0 { u64::MAX } else { u64::from(b) << 12 };

    let limits = DecodeLimits::new(window).with_max_output(MAX_OUTPUT as u64);
    let mut workspace = vec![0u8; workspace_bytes(window).unwrap()];
    let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
    let mut work = WorkBudget::new(budget);
    let mut out = vec![0u8; out_chunk];
    let mut decoded = Vec::new();
    let mut pos = 0;
    let result = loop {
        let end = (pos + in_chunk).min(stream.len());
        match decoder.step(&stream[pos..end], end == stream.len(), &mut out, &mut work) {
            Ok(progress) => {
                assert!(
                    progress.consumed > 0
                        || progress.produced > 0
                        || progress.state == StepState::Done
                );
                decoded.extend_from_slice(&out[..progress.produced]);
                pos += progress.consumed;
                if progress.state == StepState::Done {
                    break Ok(());
                }
            }
            Err(e) => break Err(e),
        }
    };
    if matches!(
        result,
        Err(WorkspaceDecodeError::Limit(_) | WorkspaceDecodeError::Unsupported(_))
    ) {
        return;
    }

    let mut reference = FrameDecoder::new();
    reference.set_content_checksum(ContentChecksum::Verify);
    let mut target = vec![0u8; MAX_OUTPUT];
    let expected = reference
        .decode_all(stream, &mut target)
        .ok()
        .map(|n| &target[..n]);
    assert_eq!(result.is_ok().then_some(decoded.as_slice()), expected);
});
