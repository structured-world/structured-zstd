use super::*;
use crate::encoding::compress_slice_to_vec;
use std::{format, string::String};

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
    let mut writer = BitWriter::new();
    HuffmanEncoder::new(&table, &mut writer).write_table();
    let description = writer.dump();
    let parsed = crate::huff0::HuffmanTable::new()
        .build_decoder(&description)
        .expect("the description parses back");
    assert_eq!(parsed as usize, description.len());
}

/// Only a sample's first block is counted (upstream zstd `ZDICT_countEStats`
/// compresses `MIN(128 KiB, 1 << windowLog)` bytes as one block, the window of
/// the average sample plus the content). Samples averaging a line with half a
/// KiB of content keep that window at its 1 KiB floor; two versions of an
/// 8 KiB sample that differ only past 4 KiB must finalize to the same bytes.
#[test]
fn only_the_first_block_of_a_sample_is_counted() {
    let (mut data, mut sizes) = log_samples();
    let content: Vec<u8> = data[..512].to_vec();
    let head = data[..4096].to_vec();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    sizes.push(8192);
    let finalized = |tail: &[u8], data: &mut Vec<u8>| {
        let base = data.len();
        data.extend_from_slice(&head);
        data.extend_from_slice(tail);
        let samples = SampleSet::new(data, &sizes).unwrap();
        let dict = finalize(
            &content,
            &samples,
            samples.count(),
            8192,
            FinalizeOptions::default(),
        )
        .unwrap();
        data.truncate(base);
        dict
    };
    let with_runs = finalized(&[b'z'; 4096], &mut data);
    let with_noise = finalized(&noise, &mut data);
    assert!(
        with_runs == with_noise,
        "a sample's second block reached the tables"
    );
}

/// The first block spans the window of the average sample plus the content,
/// not of the sample alone: with 4 KiB of content that window is 8 KiB, so the
/// second half of an 8 KiB sample is counted and two versions differing there
/// finalize differently.
#[test]
fn the_first_block_spans_the_window_of_sample_and_content() {
    let (mut data, mut sizes) = log_samples();
    let content: Vec<u8> = data[..4096].to_vec();
    let head = data[..4096].to_vec();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    sizes.push(8192);
    let finalized = |tail: &[u8], data: &mut Vec<u8>| {
        let base = data.len();
        data.extend_from_slice(&head);
        data.extend_from_slice(tail);
        let samples = SampleSet::new(data, &sizes).unwrap();
        let dict = finalize(
            &content,
            &samples,
            samples.count(),
            8192,
            FinalizeOptions::default(),
        )
        .unwrap();
        data.truncate(base);
        dict
    };
    let with_runs = finalized(&[b'z'; 4096], &mut data);
    let with_noise = finalized(&noise, &mut data);
    assert!(
        with_runs != with_noise,
        "the second half of the sample's first block was not counted"
    );
}

/// Offset codes are counted with the repeat policy the frame's blocks used.
/// Two tokens alternating behind short varied gaps make an offset often equal
/// the one before last: the full search writes it as repeat 2 or 3 (offset
/// code 1), which the fast strategy never emits, so at level 1 that code keeps
/// its floor of one while at level 3 it is counted.
#[test]
fn offset_codes_follow_the_frames_repeat_policy() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let tokens = [*b"alpha-token-0001", *b"bravo-token-0002"];
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for _ in 0..64 {
        let start = data.len();
        for i in 0..40 {
            data.extend_from_slice(&tokens[i % 2]);
            for _ in 0..1 + next() % 2 {
                data.push(b'!' + (next() % 60) as u8);
            }
        }
        sizes.push(data.len() - start);
    }
    let samples = SampleSet::new(&data, &sizes).unwrap();
    let content = samples.sample(0).to_vec();
    let offset_code_one = |level| {
        let mut counts = EntropyCounts {
            literals: [1; 256],
            offset_codes: vec![1; OFFCODE_MAX as usize + 1],
            match_lengths: [1; 53],
            literal_lengths: [1; 36],
        };
        count_samples(&mut counts, &content, &samples, samples.count(), level).unwrap();
        counts.offset_codes[1]
    };
    assert!(offset_code_one(3) > 1, "the input exercises deeper repeats");
    assert_eq!(offset_code_one(1), 1);
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
