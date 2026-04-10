use crate::{
    common::MAX_BLOCK_SIZE,
    encoding::{
        CompressionLevel, Matcher,
        block_header::BlockHeader,
        blocks::compress_block,
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
) {
    let block_size = uncompressed_data.len() as u32;
    // First check to see if run length encoding can be used for the entire block
    if uncompressed_data.iter().all(|x| uncompressed_data[0].eq(x)) {
        let rle_byte = uncompressed_data[0];
        state.matcher.commit_space(uncompressed_data);
        state.matcher.skip_matching(Some(false));
        let header = BlockHeader {
            last_block,
            block_type: crate::blocks::block::BlockType::RLE,
            block_size,
        };
        // Write the header, then the block
        header.serialize(output);
        output.push(rle_byte);
    } else if should_emit_raw_fast_path(
        compression_level,
        state.matcher.window_size(),
        &uncompressed_data,
    ) {
        state.matcher.commit_space(uncompressed_data);
        state.matcher.skip_matching(Some(true));
        let header = BlockHeader {
            last_block,
            block_type: crate::blocks::block::BlockType::Raw,
            block_size,
        };
        header.serialize(output);
        output.extend_from_slice(state.matcher.get_last_space());
    } else {
        // Compress as a standard compressed block
        let mut compressed = Vec::new();
        state.matcher.commit_space(uncompressed_data);
        compress_block(state, &mut compressed);
        // If the compressed data is larger than the maximum
        // allowable block size, instead store uncompressed
        if compressed.len() >= MAX_BLOCK_SIZE as usize {
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend_from_slice(state.matcher.get_last_space());
        } else {
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Compressed,
                block_size: compressed.len() as u32,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend(compressed);
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

        fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
            self.skip_hints.push(incompressible_hint);
        }

        fn start_matching(&mut self, _handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
            panic!("start_matching must not run for RLE path");
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
            offset_hist: [1, 4, 8],
        };
        let mut output = Vec::new();

        compress_block_encoded(
            &mut state,
            CompressionLevel::Fastest,
            true,
            vec![0xAB; 1024],
            &mut output,
        );

        assert_eq!(
            state.matcher.skip_hints,
            vec![Some(false)],
            "RLE is already known compressible; skip_matching should bypass incompressible sampling"
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
