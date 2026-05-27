use crate::{
    blocks::block::BlockType,
    common::MAX_BLOCK_SIZE,
    encoding::{
        CompressionLevel, Matcher,
        block_header::BlockHeader,
        blocks::{compress_block, compress_block_with_post_split},
        frame_compressor::CompressState,
        incompressible::{
            block_looks_incompressible, block_looks_incompressible_strict,
            compression_level_allows_raw_fast_path,
        },
    },
};
use alloc::vec::Vec;

/// Compresses a single block using the shared compressed-block pipeline.
///
/// Used by all compressed levels (Fastest, Default, Better, Best, and numeric levels). The actual
/// compression quality is determined by the matcher backend in `state`,
/// not by this function.
///
/// # Parameters
/// - `state`: [`CompressState`] so the compressor can refer to data before
///   the start of this block
/// - `last_block`: Whether or not this block is going to be the last block in the frame
///   (needed because this info is written into the block header)
/// - `uncompressed_data`: A block's worth of uncompressed data, taken from the
///   larger input
/// - `output`: As `uncompressed_data` is compressed, it's appended to `output`.
#[inline]
pub(crate) fn compress_block_encoded<M: Matcher>(
    state: &mut CompressState<M>,
    compression_level: CompressionLevel,
    last_block: bool,
    uncompressed_data: Vec<u8>,
    output: &mut Vec<u8>,
    #[cfg(all(feature = "lsm", feature = "hash"))] block_checksums: Option<&mut Vec<u32>>,
) -> BlockType {
    let block_size = uncompressed_data.len() as u32;
    // First check to see if run length encoding can be used for the entire block
    if uncompressed_data.iter().all(|x| uncompressed_data[0].eq(x)) {
        let rle_byte = uncompressed_data[0];
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if let Some(sink) = block_checksums {
            sink.push(crate::encoding::frame_compressor::xxh64_block_low32(
                &uncompressed_data,
            ));
        }
        state.matcher.commit_space(uncompressed_data);
        state.matcher.skip_matching_with_hint(Some(false));
        let header = BlockHeader {
            last_block,
            block_type: BlockType::RLE,
            block_size,
        };
        // Write the header, then the block
        header.serialize(output);
        output.push(rle_byte);
        BlockType::RLE
    } else if should_emit_raw_fast_path(
        compression_level,
        state.matcher.window_size(),
        &uncompressed_data,
    ) {
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if let Some(sink) = block_checksums {
            sink.push(crate::encoding::frame_compressor::xxh64_block_low32(
                &uncompressed_data,
            ));
        }
        state.matcher.commit_space(uncompressed_data);
        state.matcher.skip_matching_with_hint(Some(true));
        let header = BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size,
        };
        header.serialize(output);
        output.extend_from_slice(state.matcher.get_last_space());
        BlockType::Raw
    } else {
        // Compress as a standard compressed block
        let mut compressed = Vec::new();
        let uncompressed_len = uncompressed_data.len();
        state.matcher.commit_space(uncompressed_data);
        if matches!(compression_level, CompressionLevel::Level(16..=22))
            && state.matcher.window_size() >= (1 << 17)
        {
            // This helper may emit multiple physical blocks (compressed or raw)
            // into `output`; checksums (if requested) are pushed per physical
            // block from inside the partition loop so the cardinality matches
            // the decoder's per-block hash count exactly.
            #[cfg(all(feature = "lsm", feature = "hash"))]
            compress_block_with_post_split(state, last_block, output, block_checksums);
            #[cfg(not(all(feature = "lsm", feature = "hash")))]
            compress_block_with_post_split(state, last_block, output);
            return BlockType::Compressed;
        }
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if let Some(sink) = block_checksums {
            // Pull the just-committed input back from the matcher so we can
            // hash the same bytes the decoder will see for this single block.
            let space = state.matcher.get_last_space();
            let start = space.len() - uncompressed_len;
            sink.push(crate::encoding::frame_compressor::xxh64_block_low32(
                &space[start..],
            ));
        }
        #[cfg(not(all(feature = "lsm", feature = "hash")))]
        let _ = uncompressed_len;

        // Keep rollback snapshots for the oversize fallback path below:
        // `compress_block` can mutate entropy/history state before we know
        // whether the compressed payload fits `MAX_BLOCK_SIZE`.
        let saved_offset_hist = state.offset_hist;
        let saved_huff_table = state.last_huff_table.clone();
        let saved_ll_previous = state.fse_tables.ll_previous.clone();
        let saved_ml_previous = state.fse_tables.ml_previous.clone();
        let saved_of_previous = state.fse_tables.of_previous.clone();
        compress_block(state, &mut compressed);
        // If the compressed data is larger than the maximum
        // allowable block size, instead store uncompressed
        if compressed.len() >= MAX_BLOCK_SIZE as usize {
            state.offset_hist = saved_offset_hist;
            state.last_huff_table = saved_huff_table;
            state.fse_tables.ll_previous = saved_ll_previous;
            state.fse_tables.ml_previous = saved_ml_previous;
            state.fse_tables.of_previous = saved_of_previous;
            let header = BlockHeader {
                last_block,
                block_type: BlockType::Raw,
                block_size,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend_from_slice(state.matcher.get_last_space());
            BlockType::Raw
        } else {
            let header = BlockHeader {
                last_block,
                block_type: BlockType::Compressed,
                block_size: compressed.len() as u32,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend(compressed);
            BlockType::Compressed
        }
    }
}

#[inline]
fn should_emit_raw_fast_path(level: CompressionLevel, window_size: u64, block: &[u8]) -> bool {
    if !compression_level_allows_raw_fast_path(level, window_size) {
        return false;
    }
    if matches!(level, CompressionLevel::Best) {
        return block_looks_incompressible_strict(block);
    }
    block_looks_incompressible(block)
}

#[cfg(test)]
mod tests {
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
    fn rle_branch_passes_compressible_hint_to_skip_matching() {
        let mut state = CompressState {
            matcher: HintProbeMatcher::default(),
            last_huff_table: None,
            fse_tables: FseTables::new(),
            block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
            offset_hist: [1, 4, 8],
            strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        };
        let mut output = Vec::new();

        let emitted = compress_block_encoded(
            &mut state,
            CompressionLevel::Fastest,
            true,
            vec![0xAB; 1024],
            &mut output,
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
            fse_tables: FseTables::new(),
            block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
            offset_hist: [1, 4, 8],
            strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
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
            !should_emit_raw_fast_path(CompressionLevel::Best, 16 * 1024 * 1024, &block),
            "Best should keep compressed path when large window can unlock long-distance matches"
        );
    }
}
