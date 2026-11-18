use crate::cpu_kernel::CpuKernelTag;
use crate::decoding::{FrameDecoder, StreamingDecoder};
use crate::encoding::{CompressionLevel, compress_slice_to_vec};
use alloc::vec::Vec;

#[cfg(feature = "std")]
std::thread_local! {
    /// Passes of the loop on this thread that ended `Done`, `Boundary` and
    /// `Pending`, so a test can tell the loop ran rather than infer it from
    /// the CPU tier and the entry threshold.
    static EXITS: core::cell::Cell<[u64; 3]> = const { core::cell::Cell::new([0; 3]) };
}

/// Counts one pass by the assembly's status (0 done, 1 boundary, 2 pending).
#[cfg(feature = "std")]
pub(super) fn count_exit(status: usize) {
    EXITS.with(|exits| {
        let mut counts = exits.get();
        counts[status.min(2)] += 1;
        exits.set(counts);
    });
}

/// Whether this CPU can run the AVX2 tier, the one whose sequence loop is the
/// assembly.
#[cfg(feature = "std")]
fn avx2_runs() -> bool {
    std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("bmi2")
}

/// Decodes `frame` into a slice of `len + slack` bytes on the named tier.
#[cfg(feature = "std")]
fn slice_decode(
    frame: &[u8],
    len: usize,
    slack: usize,
    kernel: CpuKernelTag,
    dict: Option<&crate::decoding::DictionaryHandle>,
) -> Vec<u8> {
    let mut decoder = FrameDecoder::new();
    decoder.decode_with_kernel_for_tests(kernel);
    let mut target = alloc::vec![0u8; len + slack];
    let wrote = match dict {
        Some(handle) => decoder.decode_all_with_dict_handle(frame, &mut target, handle),
        None => decoder.decode_all(frame, &mut target),
    }
    .expect("slice decode");
    target.truncate(wrote);
    target
}

/// Asserts that every exit of the loop was taken since `before`: entered
/// blocks finishing in it, stopping at the stream's start, and handing a
/// sequence to Rust.
#[cfg(feature = "std")]
fn assert_every_exit_since(before: [u64; 3]) {
    let after = EXITS.with(|exits| exits.get());
    for (exit, name) in ["Done", "Boundary", "Pending"].iter().enumerate() {
        assert!(
            after[exit] > before[exit],
            "the assembly loop never exited {name} (exits before {before:?}, after {after:?})"
        );
    }
}

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
    let mut mixed = text[..32 * 1024].to_vec();
    mixed.extend_from_slice(&short_period[..8 * 1024]);
    mixed.extend_from_slice(&long_runs[..32 * 1024]);
    mixed.extend_from_slice(&text[40 * 1024..64 * 1024]);
    alloc::vec![text, short_period, long_runs, mixed]
}

/// A frame decoded into a slice on the AVX2 tier, where the sequence loop runs
/// its common path in assembly, equals the input, as do the scalar tier's
/// slice decode and the same frame streamed through the ring buffer a piece at
/// a time, whose loops are Rust; with the slice exactly the content's size
/// (the last sequences in the tight tail) and with room past it. The tiers are
/// named, not detected: a host whose detector picks a wider tier would
/// otherwise never run the assembly. Where AVX2 runs, the loop must have taken
/// each of its exits.
#[cfg(feature = "std")]
#[test]
fn assembly_loop_decodes_what_the_rust_loop_decodes() {
    let before = EXITS.with(|exits| exits.get());
    for (shape, input) in shapes().iter().enumerate() {
        for level in [1, 3, 9, 16, 19] {
            let frame = compress_slice_to_vec(input, CompressionLevel::from_level(level));
            for slack in [0usize, 64] {
                assert!(
                    slice_decode(&frame, input.len(), slack, CpuKernelTag::Scalar, None) == *input,
                    "scalar slice decode differs: shape {shape} level {level} slack {slack}"
                );
                if avx2_runs() {
                    assert!(
                        slice_decode(&frame, input.len(), slack, CpuKernelTag::Avx2, None)
                            == *input,
                        "AVX2 slice decode differs: shape {shape} level {level} slack {slack}"
                    );
                }
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
    if avx2_runs() {
        assert_every_exit_since(before);
    }
}

/// The same comparison on frames made against a dictionary, whose matches
/// reach into it: wholly inside it (the assembly copies those), running out of
/// it into the output and ending too near its end for sixteen-byte reads (the
/// Rust path executes those). A 16 KiB frame of dictionary pieces of 40 to 199
/// bytes from all over it, the last near its end, between short runs of
/// other text, so nearly every match is a dictionary one in a block long
/// enough for the loop to be entered; and a frame of 64 KiB of other text,
/// where the dictionary's reach ends.
#[cfg(feature = "std")]
#[test]
fn assembly_loop_decodes_dictionary_frames_as_the_rust_loop_does() {
    use crate::decoding::{Dictionary, DictionaryHandle};
    use crate::encoding::FrameCompressor;

    let corpus = include_bytes!("../../../decodecorpus_files/z000033");
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
    let before = EXITS.with(|exits| exits.get());
    for (shape, input) in inputs.iter().enumerate() {
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
            for slack in [0usize, 64] {
                assert!(
                    slice_decode(
                        &frame,
                        input.len(),
                        slack,
                        CpuKernelTag::Scalar,
                        Some(&handle)
                    ) == *input,
                    "scalar slice decode differs: shape {shape} level {level} slack {slack}"
                );
                if avx2_runs() {
                    assert!(
                        slice_decode(
                            &frame,
                            input.len(),
                            slack,
                            CpuKernelTag::Avx2,
                            Some(&handle)
                        ) == *input,
                        "AVX2 slice decode differs: shape {shape} level {level} slack {slack}"
                    );
                }
            }
            let mut decoder =
                StreamingDecoder::new_with_dictionary_handle(frame.as_slice(), &handle)
                    .unwrap_or_else(|_| panic!("dictionary refused"));
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
    if avx2_runs() {
        assert_every_exit_since(before);
    }
}
