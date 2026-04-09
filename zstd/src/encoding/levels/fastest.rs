use crate::{
    common::MAX_BLOCK_SIZE,
    encoding::{
        CompressionLevel, Matcher, block_header::BlockHeader, blocks::compress_block,
        frame_compressor::CompressState,
    },
};
use alloc::vec::Vec;

/// Compresses a single block using the shared compressed-block pipeline.
///
/// Used by all compressed levels (Fastest, Default, Better). The actual
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
pub fn compress_block_encoded<M: Matcher>(
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
        state.matcher.skip_matching();
        let header = BlockHeader {
            last_block,
            block_type: crate::blocks::block::BlockType::RLE,
            block_size,
        };
        // Write the header, then the block
        header.serialize(output);
        output.push(rle_byte);
    } else if should_emit_raw_fast_path(compression_level, &uncompressed_data) {
        state.matcher.commit_space(uncompressed_data);
        state.matcher.skip_matching();
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
fn should_emit_raw_fast_path(level: CompressionLevel, block: &[u8]) -> bool {
    let level_allows_fast_path = match level {
        CompressionLevel::Fastest | CompressionLevel::Default => true,
        CompressionLevel::Level(level) => (0..=3).contains(&level),
        CompressionLevel::Uncompressed | CompressionLevel::Better | CompressionLevel::Best => false,
    };
    if !level_allows_fast_path {
        return false;
    }

    // Tiny payloads are already cheap; avoid adding heuristic overhead/noise.
    if block.len() < 512 {
        return false;
    }

    let sample_len = block.len().min(4096);
    if sample_len < 32 {
        return false;
    }
    let sample = &block[..sample_len];

    // Fast entropy proxy: random/incompressible data tends to have a wide byte
    // spread and no dominant symbol.
    let mut counts = [0u16; 256];
    for &byte in sample {
        counts[byte as usize] += 1;
    }
    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;

    // Exact 4-byte repeat signal on sampled positions: random payloads should
    // almost never repeat the same 4-byte chunk, while compressible inputs do.
    const REPEAT_TABLE_BITS: usize = 11;
    const REPEAT_TABLE_LEN: usize = 1 << REPEAT_TABLE_BITS;
    let mut repeat_table = [u32::MAX; REPEAT_TABLE_LEN];
    let mut repeats = 0usize;
    let mut sampled_quads = 0usize;
    let mut idx = 0usize;
    while idx + 4 <= sample.len() {
        let quad = u32::from_le_bytes([
            sample[idx],
            sample[idx + 1],
            sample[idx + 2],
            sample[idx + 3],
        ]);
        let slot = ((quad.wrapping_mul(0x9E37_79B1) as usize) >> (32 - REPEAT_TABLE_BITS))
            & (REPEAT_TABLE_LEN - 1);
        sampled_quads += 1;
        if repeat_table[slot] == quad {
            repeats += 1;
        } else {
            repeat_table[slot] = quad;
        }
        idx += 4;
    }

    // Guardrails tuned to classify high-entropy blocks only.
    let max_symbol_guard = sample_len / 24; // ~4.1%
    let repeat_guard = sampled_quads / 64 + 1; // allow tiny accidental repeats
    distinct >= 200 && max_freq <= max_symbol_guard && repeats <= repeat_guard
}
