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
    let mut scratch = WeightScratch::default();
    let flat = HuffmanTable::build_limited_in(&[1; 256], HUF_MAX_BITS, &mut scratch);
    assert_eq!(flat.table_log(), 8);
    scratch.recycle(flat);
    let mut counts = [2usize; 256];
    counts[0] = 4;
    counts[253] = 1;
    counts[254] = 1;
    let table = HuffmanTable::build_limited_in(&counts, HUF_MAX_BITS, &mut scratch);
    assert_eq!(table.table_log(), 9);
    let mut writer = BitWriter::new();
    HuffmanEncoder::new(&table, &mut writer).write_table();
    let description = writer.dump();
    let parsed = crate::huff0::HuffmanTable::new()
        .build_decoder(&description)
        .expect("the description parses back");
    assert_eq!(parsed as usize, description.len());
}

/// Literal counts summed over the samples can pass what a Huffman tree node
/// holds (more than 4 GiB of literals); the code is still built, from the same
/// proportions, instead of overflowing the tree.
#[test]
fn literal_counts_past_a_tree_node_still_build_a_code() {
    let mut counts = [1usize; 256];
    counts[b'e' as usize] = 3 << 31;
    counts[b't' as usize] = 1 << 31;
    counts[b'a' as usize] = 1 << 28;
    let table = literals_table(&mut counts, &mut WeightScratch::default());
    let bits = |symbol: u8| table.num_bits_for_symbol(symbol).unwrap();
    assert!(bits(b'e') <= bits(b't'));
    assert!(bits(b't') <= bits(b'a'));
    assert!(bits(b'a') < bits(b'z'));
}

/// Below the limit a code is described at its longest length with the same
/// lengths the limit gave it: building under the limit equals building at the
/// longest code, over a scratch whose recycled table held another code.
#[test]
fn a_literal_table_under_the_limit_keeps_its_code_lengths() {
    let mut counts = [0usize; 256];
    for (symbol, count) in counts.iter_mut().enumerate() {
        *count = 10 + (symbol * symbol) % 7;
    }
    let mut scratch = WeightScratch::default();
    let stale = HuffmanTable::build_limited_in(&[1; 256], HUF_MAX_BITS, &mut scratch);
    scratch.recycle(stale);
    let under = HuffmanTable::build_limited_in(&counts, HUF_MAX_BITS, &mut scratch);
    let longest = (0..=255u8)
        .filter_map(|symbol| under.num_bits_for_symbol(symbol))
        .max()
        .map(usize::from)
        .unwrap();
    assert!(
        longest < HUF_MAX_BITS,
        "the code reaches the limit: {longest}"
    );
    assert_eq!(under.table_log() as usize, longest);
    let at = HuffmanTable::build_limited_in(&counts, longest, &mut WeightScratch::default());
    assert_eq!(at.table_log(), under.table_log());
    for symbol in 0..=255u8 {
        assert_eq!(
            under.num_bits_for_symbol(symbol),
            at.num_bits_for_symbol(symbol)
        );
    }
}

/// Random bytes, different for each seed.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// A block written raw teaches nothing: its bytes are no literals of any
/// compressed block. Samples averaging a line keep the blocks at 1 KiB, so an
/// 8 KiB sample of 4 KiB of log lines and 4 KiB of noise is four compressed
/// blocks and four raw ones; two versions differing only in the noise must
/// finalize to the same bytes.
#[test]
fn a_block_written_raw_is_not_counted() {
    let (mut data, mut sizes) = log_samples();
    let content: Vec<u8> = data[..4096].to_vec();
    let head = data[..4096].to_vec();
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
    let one = finalized(&noise(0x2545_F491_4F6C_DD1D, 4096), &mut data);
    let other = finalized(&noise(0x9E37_79B9_7F4A_7C15, 4096), &mut data);
    assert!(one == other, "a raw block's bytes reached the tables");
}

/// A block of text and noise still teaches its text. At level 19 a 128 KiB
/// sample of 16 KiB of log lines and 112 KiB of noise is one block for the
/// matcher; cut after matching, it went out as two compressed pieces and a raw
/// one, which could not be told apart from the one recorded block, so none of
/// the sample was counted.
#[test]
fn a_block_mixing_text_and_noise_is_counted() {
    const TEXT: usize = 16 << 10;
    let mut data = Vec::new();
    let mut line = 0u32;
    while data.len() < TEXT {
        data.extend_from_slice(
            std::format!(
                "ts={line} level=info msg=request served path=/api/v1/items/{}\n",
                line % 97
            )
            .as_bytes(),
        );
        line += 1;
    }
    data.truncate(TEXT);
    data.extend_from_slice(&noise(0x2545_F491_4F6C_DD1D, (128 << 10) - TEXT));
    let sizes = [data.len()];
    let samples = SampleSet::new(&data, &sizes).unwrap();
    let content = data[..4096].to_vec();
    let mut counts = EntropyCounts {
        literals: [1; 256],
        offset_codes: [1; 32],
        match_lengths: [1; 53],
        literal_lengths: [1; 36],
    };
    count_samples(
        &mut counts,
        &mut Analysis::default(),
        &content,
        &samples,
        1,
        19,
    )
    .unwrap();
    let matches: usize = counts.match_lengths.iter().sum();
    assert!(
        matches > 53 + 100,
        "the text half's sequences were counted: {matches}"
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
    // One analysis across both levels: the second must not run on the
    // compressor the first built for another level.
    let mut analysis = Analysis::default();
    let mut offset_code_one = |level| {
        let mut counts = EntropyCounts {
            literals: [1; 256],
            offset_codes: [1; 32],
            match_lengths: [1; 53],
            literal_lengths: [1; 36],
        };
        count_samples(
            &mut counts,
            &mut analysis,
            &content,
            &samples,
            samples.count(),
            level,
        )
        .unwrap();
        counts.offset_codes[1]
    };
    assert!(offset_code_one(3) > 1, "the input exercises deeper repeats");
    assert_eq!(offset_code_one(1), 1);
}

/// Each block's kind is read from the frame, one entry per block, and nothing
/// from bytes that are not a frame.
#[test]
fn block_kinds_are_read_from_the_frame() {
    let mut kinds = Vec::new();
    let text: String = core::iter::repeat_n("compressible text ", 200).collect();
    compressed_blocks(
        &compress_slice_to_vec(text.as_bytes(), CompressionLevel::Default),
        &mut kinds,
    );
    assert_eq!(kinds, [true]);
    // Past one full block of noise: two blocks, both written raw.
    compressed_blocks(
        &compress_slice_to_vec(&noise(1, 200_000), CompressionLevel::Default),
        &mut kinds,
    );
    assert_eq!(kinds, [false, false]);
    compressed_blocks(b"not a frame", &mut kinds);
    assert!(kinds.is_empty());
}

/// The id is derived from the content when none is given, inside the range
/// upstream reserves for them, and stays the same for the same content.
#[test]
fn derived_ids_are_stable_and_compliant() {
    let id = derive_dict_id(b"some content");
    assert_eq!(id, derive_dict_id(b"some content"));
    assert!((32_768..1 << 31).contains(&id));
}
