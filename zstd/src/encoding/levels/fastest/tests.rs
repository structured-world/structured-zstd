use super::*;
use crate::encoding::{
    Matcher, Sequence,
    frame_compressor::{CompressState, FseTables},
};
use alloc::vec;

#[derive(Default)]
struct HintProbeMatcher {
    last_space: Vec<u8>,
    skip_hints: Vec<Option<bool>>,
}

impl Matcher for HintProbeMatcher {
    fn get_next_space(&mut self) -> Vec<u8> {
        vec![0; 1024]
    }

    fn get_last_space(&mut self) -> &[u8] {
        &self.last_space
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        self.last_space = space;
    }

    fn skip_matching(&mut self) {
        self.skip_hints.push(None);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.skip_hints.push(incompressible_hint);
    }

    fn start_matching(&mut self, _handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
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
        last_huff_table: None,
        huff_table_spare: None,
        fse_tables: FseTables::new(),
        block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
        offset_hist: [1, 4, 8],
        strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        huf_optimal_search: true,
        literal_compression_disabled: false,
    };
    let mut output = Vec::new();

    let emitted = compress_block_encoded(
        &mut state,
        CompressionLevel::Fastest,
        true,
        vec![0xAB; 1024],
        &mut output,
        false,
        #[cfg(feature = "lsm")]
        None,
        #[cfg(all(feature = "lsm", feature = "hash"))]
        None,
    );
    assert_eq!(emitted, BlockType::RLE);

    assert_eq!(
        state.matcher.skip_hints,
        vec![Some(false)],
        "RLE is already known compressible; skip_matching should bypass incompressible sampling"
    );
}

#[test]
fn raw_fast_path_emits_raw_block_and_passes_incompressible_hint() {
    let mut state = CompressState {
        matcher: HintProbeMatcher::default(),
        last_huff_table: None,
        huff_table_spare: None,
        fse_tables: FseTables::new(),
        block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
        offset_hist: [1, 4, 8],
        strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        huf_optimal_search: true,
        literal_compression_disabled: false,
    };
    let mut output = Vec::new();

    let mut block = vec![0u8; 4096];
    let mut x = 0x1234_5678u32;
    for byte in &mut block {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *byte = x as u8;
    }
    assert!(
        block_looks_incompressible(&block),
        "fixture must look incompressible to hit raw fast-path success branch"
    );

    let emitted = compress_block_encoded(
        &mut state,
        CompressionLevel::Fastest,
        true,
        block.clone(),
        &mut output,
        false,
        #[cfg(feature = "lsm")]
        None,
        #[cfg(all(feature = "lsm", feature = "hash"))]
        None,
    );
    assert_eq!(emitted, BlockType::Raw);

    assert_eq!(state.matcher.skip_hints, vec![Some(true)]);
    assert_eq!(state.matcher.get_last_space(), block.as_slice());
    assert_eq!(
        (output[0] >> 1) & 0b11,
        0,
        "raw fast-path should emit BlockType::Raw header"
    );
}

#[test]
fn best_raw_fast_path_disabled_when_window_exceeds_better_reach() {
    let mut block = vec![0u8; 4096];
    let mut x = 0x1234_5678u32;
    for byte in &mut block {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *byte = x as u8;
    }
    assert!(
        block_looks_incompressible_strict(&block),
        "fixture must look incompressible to exercise Best window guard"
    );
    assert!(
        !should_emit_raw_fast_path(CompressionLevel::Best, 16 * 1024 * 1024, &block, false),
        "Best should keep compressed path when large window can unlock long-distance matches"
    );
}
