use super::*;
use crate::encoding::{
    HistoryBuf, Matcher, Sequence,
    block_input::BlockInput,
    frame_compressor::{CompressState, FseTables},
};
use alloc::vec;

#[derive(Default)]
struct HintProbeMatcher {
    input: BlockInput,
    skip_hints: Vec<Option<bool>>,
}

impl Matcher for HintProbeMatcher {
    fn get_last_space(&mut self) -> &[u8] {
        self.input.last_block()
    }

    fn fill_in_place(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        self.input.fill(capacity, fill)
    }

    fn uncommitted_input(&self) -> &[u8] {
        self.input.uncommitted()
    }

    fn commit_filled(&mut self, len: usize) {
        self.input.commit(len);
    }

    fn skip_matching(&mut self) {
        self.skip_hints.push(None);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.skip_hints.push(incompressible_hint);
    }

    fn start_matching(&mut self, _handle_sequence: impl FnMut(Sequence)) {
        panic!("start_matching must not run for early-exit paths");
    }

    fn reset(&mut self, _level: CompressionLevel) {}

    fn window_size(&self) -> u64 {
        128 * 1024
    }
}

#[test]
fn custom_matcher_dict_probe_defaults_to_false() {
    // `Matcher::block_samples_match_dict` defaults to `false`: a CUSTOM matcher
    // with no dict-table probe leaves the raw-fast-path on its content-only
    // verdict. NOTE this is the TRAIT DEFAULT, not the production wrapper: the
    // `MatchGeneratorDriver` overrides it to `true` for non-Simple backends so
    // a dict frame stays on the scan (covered by
    // `block_samples_match_dict_is_true_for_non_simple_backend`). Only the
    // Simple/Fast backend with an attached dictionary runs the precise probe.
    let m = HintProbeMatcher::default();
    assert!(!m.block_samples_match_dict(b"arbitrary block content, no dict probe"));
}

#[test]
fn rle_branch_passes_compressible_hint_to_skip_matching() {
    let mut state = CompressState {
        matcher: HintProbeMatcher::default(),
        sequence_capture: None,
        copy_kernel: crate::encoding::fastpath::select_kernel(),
        last_huff_table: None,
        huf_repeat_valid: false,
        huff_table_spare: None,
        huff_weights: Default::default(),
        seen_content: Default::default(),
        fse_tables: FseTables::new(),
        block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
        workspace: crate::encoding::workspace::Workspace::new(),
        offset_hist: [1, 4, 8],
        strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        pre_split: None,
        huf_optimal_search: true,
        literal_compression_disabled: false,
        post_split_allowed: true,
    };
    let mut output = Vec::new();
    let block = vec![0xAB; 1024];
    state.matcher.fill_in_place(block.len(), &mut |history| {
        history.extend_from_slice(&block);
        (block.len(), false)
    });

    let emitted = crate::encoding::fastpath::with_host_tier!(K => compress_block_encoded::<_, K>(
        &mut state,
        CompressionLevel::Fastest,
        true,
        block.len(),
        &mut output,
        false,
        true,
        true,
        #[cfg(feature = "lsm")]
        None,
        #[cfg(all(feature = "lsm", feature = "hash"))]
        None,
    ));
    assert_eq!(emitted, BlockType::RLE);

    assert_eq!(
        state.matcher.skip_hints,
        vec![Some(false)],
        "RLE is already known compressible; skip_matching should bypass incompressible sampling"
    );
}

/// Parses a block of one repeated byte as one literal and a match at offset 1,
/// counting the searches it runs.
#[derive(Default)]
struct UniformMatcher {
    input: BlockInput,
    searches: usize,
    skip_hints: Vec<Option<bool>>,
}

impl Matcher for UniformMatcher {
    fn get_last_space(&mut self) -> &[u8] {
        self.input.last_block()
    }

    fn fill_in_place(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        self.input.fill(capacity, fill)
    }

    fn uncommitted_input(&self) -> &[u8] {
        self.input.uncommitted()
    }

    fn commit_filled(&mut self, len: usize) {
        self.input.commit(len);
    }

    fn skip_matching(&mut self) {
        self.skip_hints.push(None);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.skip_hints.push(incompressible_hint);
    }

    fn start_matching(&mut self, mut handle_sequence: impl FnMut(Sequence)) {
        self.searches += 1;
        let len = self.input.last_block().len();
        handle_sequence(Sequence::Triple {
            literal_len: 1,
            offset: 1,
            match_len: len - 1,
        });
    }

    fn reset(&mut self, _level: CompressionLevel) {}

    fn window_size(&self) -> u64 {
        128 * 1024
    }
}

/// One block of a single repeated byte through the encoder with the raw skip
/// off: the block type written and the matcher that saw it.
fn uniform_block_with_raw_skip_off(first_block: bool) -> (BlockType, UniformMatcher) {
    let mut state = CompressState {
        matcher: UniformMatcher::default(),
        sequence_capture: None,
        copy_kernel: crate::encoding::fastpath::select_kernel(),
        last_huff_table: None,
        huf_repeat_valid: false,
        huff_table_spare: None,
        huff_weights: Default::default(),
        seen_content: Default::default(),
        fse_tables: FseTables::new(),
        block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
        workspace: crate::encoding::workspace::Workspace::new(),
        offset_hist: [1, 4, 8],
        strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        pre_split: None,
        huf_optimal_search: false,
        literal_compression_disabled: false,
        post_split_allowed: true,
    };
    let block = vec![0xAB; 4096];
    state.reset_for_frame(
        CompressionLevel::Fastest,
        block.len(),
        crate::encoding::workspace::IngestPlan::Stream,
    );
    state.finish_layout(block.len());
    state.matcher.fill_in_place(block.len(), &mut |history| {
        history.extend_from_slice(&block);
        (block.len(), false)
    });
    let mut output = Vec::new();
    let emitted = crate::encoding::fastpath::with_host_tier!(K => compress_block_encoded::<_, K>(
        &mut state,
        CompressionLevel::Fastest,
        true,
        block.len(),
        &mut output,
        false,
        false,
        first_block,
        #[cfg(feature = "lsm")]
        None,
        #[cfg(all(feature = "lsm", feature = "hash"))]
        None,
    ));
    (emitted, state.matcher)
}

/// With the raw skip off a block of one repeated byte is searched before it is
/// written as RLE, as upstream writes it (`ZSTD_compressBlock_internal`).
#[test]
fn a_uniform_block_is_searched_before_rle_with_the_raw_skip_off() {
    let (emitted, matcher) = uniform_block_with_raw_skip_off(false);
    assert_eq!(emitted, BlockType::RLE);
    assert_eq!(matcher.searches, 1, "the block is searched first");
    assert!(matcher.skip_hints.is_empty(), "and not indexed unsearched");
}

/// Upstream never rewrites a frame's first block as RLE, so with the raw skip
/// off a uniform first block stays the compressed block its search made.
#[test]
fn a_uniform_first_block_stays_compressed_with_the_raw_skip_off() {
    let (emitted, matcher) = uniform_block_with_raw_skip_off(true);
    assert_eq!(emitted, BlockType::Compressed);
    assert_eq!(matcher.searches, 1);
}

/// A single-block frame of noise that copies its own content, the copy beginning
/// between where fixed start and middle probe runs would sit, is searched and
/// coded as a match at every level rather than written out raw.
#[test]
fn a_copy_inside_one_block_of_noise_is_coded_as_a_match() {
    let mut state = 0x6C07_8965_u32;
    let mut noise = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    };
    let first = noise(16 * 1024 - 256);
    let mut input = first.clone();
    input.extend_from_slice(&first);
    input.extend(noise(32 * 1024 + 512));
    for level in [-7, -1, 1, 3, 5, 13, 19] {
        let compressed =
            crate::encoding::compress_to_vec(input.as_slice(), CompressionLevel::Level(level));
        assert!(
            compressed.len() < input.len() - 12 * 1024,
            "level {level}: {} bytes from {}, the copy went out raw",
            compressed.len(),
            input.len(),
        );
        let mut decoded = Vec::with_capacity(input.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&compressed, &mut decoded)
            .unwrap();
        assert_eq!(decoded, input, "level {level}");
    }
}

/// A one-shot frame at a negative level whose window cuts it into 16 KiB
/// blocks searches them without asking the classifier, and the search codes the
/// copy the first block makes of its own head.
#[test]
fn a_small_block_at_a_negative_level_is_searched_without_the_classifier() {
    use crate::encoding::{CompressionParameters, compress_with_parameters};
    let mut state = 0x6C07_8965_u32;
    let mut noise = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    };
    let head = noise(9 * 1024);
    let mut input = head.clone();
    input.extend_from_slice(&head[..5 * 1024]);
    input.extend(noise(2 * 1024));
    // A second block, so the first is not the frame's last and is recorded.
    input.extend(noise(16 * 1024));

    let params = CompressionParameters::builder(CompressionLevel::Level(-1))
        .window_log(14)
        .build()
        .expect("level -1 with a 16 KiB window is a valid configuration");
    let compressed = compress_with_parameters(&input, &params);
    assert!(
        compressed.len() < input.len() - 4 * 1024,
        "{} bytes from {}: the copy inside the first block was not coded as a match",
        compressed.len(),
        input.len(),
    );
    let mut decoded = Vec::with_capacity(input.len());
    crate::decoding::FrameDecoder::new()
        .decode_all_to_vec(&compressed, &mut decoded)
        .unwrap();
    assert_eq!(decoded, input);
}
