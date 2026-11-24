//! Every CPU tier this host can execute decodes the same frames to the same
//! bytes. Which tier runs is otherwise a property of the machine, so ordinary
//! tests cover exactly one; each tier's sequence body is its own monomorph,
//! with its own copies and gates, and is compared here against the input on
//! every architecture, 32-bit x86 included.

use crate::cpu_kernel::{CpuKernelTag, supported_kernels_for_tests};
use crate::decoding::{FrameDecoder, StreamingDecoder};
use crate::encoding::{CompressionLevel, compress_slice_to_vec};
use alloc::vec::Vec;

/// On x86_64 the AVX2 tier's sequence loop runs its common path in assembly,
/// whose exits are counted, so a test can tell the loop ran.
#[cfg(all(
    target_arch = "x86_64",
    target_pointer_width = "64",
    feature = "kernel-avx2",
    not(feature = "fuzz-exports")
))]
use super::seq_loop_asm::tests::{
    assembly_runs, assert_loop_ran_since, exits, for_each_entry_form,
};

/// Without the assembly loop there is nothing to count.
#[cfg(not(all(
    target_arch = "x86_64",
    target_pointer_width = "64",
    feature = "kernel-avx2",
    not(feature = "fuzz-exports")
)))]
mod no_assembly {
    pub(super) fn assembly_runs(_: &[super::CpuKernelTag]) -> bool {
        false
    }
    pub(super) fn exits() -> [u64; 3] {
        [0; 3]
    }
    pub(super) fn assert_loop_ran_since(_: [u64; 3], _: usize) {}
    pub(super) fn for_each_entry_form(_: super::CpuKernelTag, mut f: impl FnMut(&str)) {
        f("-")
    }
}
#[cfg(not(all(
    target_arch = "x86_64",
    target_pointer_width = "64",
    feature = "kernel-avx2",
    not(feature = "fuzz-exports")
)))]
use no_assembly::{assembly_runs, assert_loop_ran_since, exits, for_each_entry_form};

/// Decodes `frame` into a slice of `len + slack` bytes on the named tier, and
/// with a dictionary a second time on the same decoder. A dictionary is cold
/// on its first frame, whose blocks of 16 sequences or more take the long
/// pipeline, as upstream's do; only from the next frame on do its blocks reach
/// the short loop, as on a decoder reused across frames.
fn slice_decodes(
    frame: &[u8],
    len: usize,
    slack: usize,
    kernel: CpuKernelTag,
    dict: Option<&crate::decoding::DictionaryHandle>,
) -> Vec<Vec<u8>> {
    let mut decoder = FrameDecoder::new();
    decoder.decode_with_kernel_for_tests(kernel);
    let rounds = if dict.is_some() { 2 } else { 1 };
    (0..rounds)
        .map(|_| {
            let mut target = alloc::vec![0u8; len + slack];
            let wrote = match dict {
                Some(handle) => decoder.decode_all_with_dict_handle(frame, &mut target, handle),
                None => decoder.decode_all(frame, &mut target),
            }
            .expect("slice decode");
            target.truncate(wrote);
            target
        })
        .collect()
}

/// `read` in small pieces, so the frame decodes block by block through the
/// ring buffer; `read_to_end` of a sized frame would decode it straight into
/// a slice.
fn stream_decode(mut decoder: StreamingDecoder<&[u8], FrameDecoder>) -> Vec<u8> {
    let mut streamed = Vec::new();
    let mut piece = [0u8; 4096];
    loop {
        let n = std::io::Read::read(&mut decoder, &mut piece).expect("stream decode");
        if n == 0 {
            break;
        }
        streamed.extend_from_slice(&piece[..n]);
    }
    streamed
}

/// Inputs whose sequences take every copy each tier has: text with new and
/// repeated offsets, short-period runs (offsets below 16), long runs (matches
/// of 256 bytes and more), literal runs past one and two 16-byte moves with
/// matches whose lengths straddle 256, and a mix.
fn shapes() -> Vec<Vec<u8>> {
    let corpus = include_bytes!("../../decodecorpus_files/z000033");
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
        if long_runs.len() > 8192 && state.is_multiple_of(3) {
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
    const MATCH_LENGTHS: [usize; 8] = [64, 200, 255, 256, 257, 300, 1024, 3000];
    let mut boundary = Vec::new();
    for round in 0..400usize {
        for _ in 0..(17 + state as usize % 96) {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            boundary.push((state >> 24) as u8);
        }
        if boundary.len() > 16 * 1024 {
            let len = MATCH_LENGTHS[round % MATCH_LENGTHS.len()];
            let start = boundary.len() - (4096 + (state as usize >> 8) % 8192);
            for k in 0..len {
                boundary.push(boundary[start + k]);
            }
        }
    }
    let mut mixed = text[..32 * 1024].to_vec();
    mixed.extend_from_slice(&short_period[..8 * 1024]);
    mixed.extend_from_slice(&long_runs[..32 * 1024]);
    mixed.extend_from_slice(&text[40 * 1024..64 * 1024]);
    alloc::vec![text, short_period, long_runs, boundary, mixed]
}

/// A frame decoded on every kernel this CPU runs, each named, equals the
/// input, both into a slice and streamed through the ring buffer a piece at a
/// time; with the slice exactly the content's size (the last
/// sequences in the tight tail) and with room past it. Where the AVX2 kernel's
/// loop is assembly, the loop must have run for every shape.
#[test]
fn every_cpu_tier_decodes_what_the_scalar_tier_decodes() {
    let kernels = supported_kernels_for_tests();
    let assembly = assembly_runs(&kernels);
    for (shape, input) in shapes().iter().enumerate() {
        let before = exits();
        for level in [1, 3, 9, 16, 19] {
            let frame = compress_slice_to_vec(input, CompressionLevel::from_level(level));
            for &kernel in &kernels {
                for_each_entry_form(kernel, |form| {
                    for slack in [0usize, 64] {
                        for decoded in slice_decodes(&frame, input.len(), slack, kernel, None) {
                            assert!(
                                decoded == *input,
                                "{kernel:?} ({form}) slice decode differs: \
                                 shape {shape} level {level} slack {slack}"
                            );
                        }
                    }
                    let mut stream = StreamingDecoder::new(frame.as_slice());
                    stream.decoder.decode_with_kernel_for_tests(kernel);
                    assert!(
                        stream_decode(stream) == *input,
                        "{kernel:?} ({form}) stream decode differs: shape {shape} level {level}"
                    );
                });
            }
        }
        if assembly {
            assert_loop_ran_since(before, shape);
        }
    }
}

/// The same comparison on frames made against a dictionary, whose matches
/// reach into it: wholly inside it, running out of it into the output and
/// ending too near its end for sixteen-byte reads. A 16 KiB frame of
/// dictionary pieces of 40 to 199 bytes from all over it, the last near its
/// end, between short runs of other text, so nearly every match is a
/// dictionary one in a block long enough for the short loop to be entered;
/// and a frame of 64 KiB of other text, where the dictionary's reach ends.
#[test]
fn every_cpu_tier_decodes_dictionary_frames_as_the_scalar_tier_does() {
    use crate::decoding::{Dictionary, DictionaryHandle};
    use crate::encoding::FrameCompressor;

    let corpus = include_bytes!("../../decodecorpus_files/z000033");
    let dict_content = corpus[600 * 1024..632 * 1024].to_vec();
    let dict_id = 0x5E0_D1C7;
    let handle = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(dict_id, dict_content.clone()).expect("decoder dictionary"),
    );
    let mut pieces = Vec::new();
    let mut k = 0usize;
    while pieces.len() < 16 * 1024 {
        let len = 40 + k % 160;
        let at = if k % 9 == 8 {
            dict_content.len() - len
        } else {
            (k * 7919) % (dict_content.len() - len)
        };
        pieces.extend_from_slice(&dict_content[at..at + len]);
        let gap = 100 * 1024 + k * 37;
        pieces.extend_from_slice(&corpus[gap..gap + 12]);
        k += 1;
    }
    let inputs = [pieces, corpus[300 * 1024..364 * 1024].to_vec()];
    let kernels = supported_kernels_for_tests();
    let assembly = assembly_runs(&kernels);
    for (shape, input) in inputs.iter().enumerate() {
        let before = exits();
        for level in [1, 3, 9, 16, 19] {
            let mut compressor: FrameCompressor =
                FrameCompressor::new(CompressionLevel::from_level(level));
            compressor
                .set_dictionary(
                    Dictionary::from_raw_content(dict_id, dict_content.clone())
                        .expect("encoder dictionary"),
                )
                .expect("attach dictionary");
            let frame = compressor.compress_independent_frame(input);
            for &kernel in &kernels {
                for_each_entry_form(kernel, |form| {
                    for slack in [0usize, 64] {
                        let decodes =
                            slice_decodes(&frame, input.len(), slack, kernel, Some(&handle));
                        for (round, decoded) in decodes.iter().enumerate() {
                            assert!(
                                *decoded == *input,
                                "{kernel:?} ({form}) slice decode {round} differs: \
                                 shape {shape} level {level} slack {slack}"
                            );
                        }
                    }
                    let mut stream =
                        StreamingDecoder::new_with_dictionary_handle(frame.as_slice(), &handle)
                            .unwrap_or_else(|_| panic!("dictionary refused"));
                    stream.decoder.decode_with_kernel_for_tests(kernel);
                    assert!(
                        stream_decode(stream) == *input,
                        "{kernel:?} ({form}) stream decode differs: shape {shape} level {level}"
                    );
                });
            }
        }
        if assembly {
            assert_loop_ran_since(before, shape);
        }
    }
}
