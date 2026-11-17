use crate::decoding::{FrameDecoder, StreamingDecoder};
use crate::encoding::{CompressionLevel, compress_slice_to_vec};
use alloc::vec::Vec;

/// Inputs whose sequences reach every exit of the assembly loop: text with
/// new and repeated offsets, short-period runs (offsets below 16), long runs
/// (matches of 256 bytes and more), literal runs past 16, and a mix.
fn shapes() -> Vec<Vec<u8>> {
    let corpus = include_bytes!("../../../decodecorpus_files/z000033");
    let text = corpus[..96 * 1024].to_vec();
    let mut short_period = Vec::new();
    for i in 0..40_000u32 {
        let period = 1 + (i / 997) % 15;
        short_period.push(b"abcdefghijklmnop"[(i % period) as usize]);
    }
    let mut long_runs = Vec::new();
    let mut state = 0x9e37_79b9u32;
    while long_runs.len() < 120_000 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let start = long_runs
            .len()
            .saturating_sub(4096 + (state as usize % 4096));
        if long_runs.len() > 8192 && state % 3 == 0 {
            let len = 256 + (state as usize >> 8) % 900;
            for k in 0..len {
                let b = long_runs[start + k];
                long_runs.push(b);
            }
        } else {
            for _ in 0..(17 + state as usize % 60) {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                long_runs.push((state >> 24) as u8);
            }
        }
    }
    let mut mixed = text[..32 * 1024].to_vec();
    mixed.extend_from_slice(&short_period[..8 * 1024]);
    mixed.extend_from_slice(&long_runs[..32 * 1024]);
    mixed.extend_from_slice(&text[40 * 1024..64 * 1024]);
    alloc::vec![text, short_period, long_runs, mixed]
}

/// A frame decoded into a slice, where the sequence loop runs its common path
/// in assembly, equals the input, as does the same frame streamed through the
/// ring buffer a piece at a time, whose loop is the Rust one; with the slice
/// exactly the content's size (the last sequences in the tight tail) and with
/// room past it.
#[cfg(feature = "std")]
#[test]
fn assembly_loop_decodes_what_the_rust_loop_decodes() {
    for (shape, input) in shapes().iter().enumerate() {
        for level in [1, 3, 9, 16, 19] {
            let frame = compress_slice_to_vec(input, CompressionLevel::from_level(level));
            for slack in [0usize, 64] {
                let mut target = alloc::vec![0u8; input.len() + slack];
                let wrote = FrameDecoder::new()
                    .decode_all(&frame, &mut target)
                    .expect("slice decode");
                assert_eq!(wrote, input.len(), "shape {shape} level {level}");
                assert!(
                    target[..wrote] == input[..],
                    "slice decode differs: shape {shape} level {level} slack {slack}"
                );
            }
            // `read` in small pieces, so the frame decodes block by block
            // through the ring buffer; `read_to_end` of a sized frame would
            // decode it straight into a slice.
            let mut decoder = StreamingDecoder::new(frame.as_slice());
            let mut streamed = Vec::new();
            let mut piece = [0u8; 4096];
            loop {
                let n = std::io::Read::read(&mut decoder, &mut piece).expect("stream decode");
                if n == 0 {
                    break;
                }
                streamed.extend_from_slice(&piece[..n]);
            }
            assert!(
                streamed == *input,
                "stream decode differs: shape {shape} level {level}"
            );
        }
    }
}
