use super::*;
use crate::encoding::compress_slice_to_vec;
use std::string::String;

/// Log lines with a shared vocabulary, one sample each.
fn log_samples() -> (Vec<u8>, Vec<usize>) {
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..400u32 {
        let line = format!(
            "ts=2026-09-{:02} level={} svc=orders tenant=t{} msg=\"{}\" latency_ms={}\n",
            i % 28 + 1,
            ["INFO", "WARN", "ERROR"][i as usize % 3],
            i % 17,
            [
                "flush memtable",
                "rotate segment",
                "compact level",
                "write block"
            ][i as usize % 4],
            (i * 37) % 1000
        );
        sizes.push(line.len());
        data.extend_from_slice(line.as_bytes());
    }
    (data, sizes)
}

/// Every sample compressed at the default level with `dictionary`, frames
/// without the dictionary id (raw content has none to record, so the sides
/// would otherwise differ by four bytes a frame).
fn compressed_total(dictionary: &[u8], samples: &SampleSet<'_>) -> usize {
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_dictionary_id_flag(false);
    compressor
        .set_encoder_dictionary(
            EncoderDictionary::from_serialized_or_raw_content(dictionary).expect("dictionary"),
        )
        .unwrap();
    (0..samples.count())
        .map(|index| {
            compressor
                .compress_independent_frame(samples.sample(index))
                .len()
        })
        .sum()
}

/// The entropy tables describe what compressing the samples with the content
/// actually produces, so a finalized dictionary compresses them at least as
/// well as its content alone does. Tables guessed from the samples' raw bytes
/// (counting byte values as if they were sequence codes) made it compress them
/// worse than the bare content.
#[test]
fn finalized_tables_compress_the_samples_better_than_the_content_alone() {
    let (data, sizes) = log_samples();
    let samples = SampleSet::new(&data, &sizes).unwrap();
    // A plausible content: the most common line shapes, best last.
    let content: Vec<u8> = (0..40)
        .flat_map(|i| samples.sample(i * 7).iter().copied())
        .collect();
    let dict = finalize(
        &content,
        &samples,
        samples.count(),
        8192,
        FinalizeOptions::default(),
    )
    .unwrap();
    assert!(dict.ends_with(&content));
    let with_tables = compressed_total(&dict, &samples);
    let content_only = compressed_total(&content, &samples);
    assert!(
        with_tables < content_only,
        "tables {with_tables} vs content alone {content_only}"
    );
}

/// Samples that do not compress teach nothing: their blocks are written raw,
/// so the counts stay at their floor of one, and the flat literal distribution
/// that leaves is replaced by one a table description can express.
#[test]
fn samples_that_do_not_compress_leave_a_describable_flat_table() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let data: Vec<u8> = (0..20_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let samples = SampleSet::new(&data, &[2_000; 10]).unwrap();
    let dict = finalize(
        &data[..1024],
        &samples,
        samples.count(),
        4096,
        FinalizeOptions::default(),
    )
    .unwrap();
    crate::decoding::Dictionary::decode_dict(&dict).expect("the dictionary parses back");
}

/// The literals table is scaled to its longest code, not to the length limit:
/// a flat alphabet comes out at eight bits, which is how the finalizer tells
/// it apart, and the stand-in distribution at nine, with a description that
/// can be written (every weight the same would have none).
#[test]
fn literal_tables_take_the_depth_of_their_longest_code() {
    assert_eq!(literals_table(&[1; 256]).table_log(), 8);
    let mut counts = [2usize; 256];
    counts[0] = 4;
    counts[253] = 1;
    counts[254] = 1;
    let table = literals_table(&counts);
    assert_eq!(table.table_log(), 9);
    assert!(!huffman_description(&table).unwrap().is_empty());
}

/// Only a frame whose first block is compressed counts as one.
#[test]
fn first_block_kind_is_read_from_the_frame() {
    let text: String = core::iter::repeat_n("compressible text ", 200).collect();
    assert!(first_block_is_compressed(&compress_slice_to_vec(
        text.as_bytes(),
        CompressionLevel::Default
    )));
    let mut state = 1u32;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    assert!(!first_block_is_compressed(&compress_slice_to_vec(
        &noise,
        CompressionLevel::Default
    )));
    assert!(!first_block_is_compressed(b"not a frame"));
}

/// The id is derived from the content when none is given, inside the range
/// upstream reserves for them, and stays the same for the same content.
#[test]
fn derived_ids_are_stable_and_compliant() {
    let id = derive_dict_id(b"some content");
    assert_eq!(id, derive_dict_id(b"some content"));
    assert!((32_768..1 << 31).contains(&id));
}
