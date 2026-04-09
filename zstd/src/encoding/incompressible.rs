use super::CompressionLevel;

pub(crate) const RAW_FAST_PATH_MIN_BLOCK_LEN: usize = 512;
pub(crate) const RAW_FAST_PATH_MAX_SAMPLE_LEN: usize = 4096;
pub(crate) const RAW_FAST_PATH_MIN_SAMPLE_LEN: usize = 32;

const INCOMPRESSIBLE_REPEAT_TABLE_BITS: usize = 11;
const INCOMPRESSIBLE_REPEAT_TABLE_LEN: usize = 1 << INCOMPRESSIBLE_REPEAT_TABLE_BITS;
const INCOMPRESSIBLE_REPEAT_HASH_MULT: u32 = 0x9E37_79B1;

#[inline]
pub(crate) fn compression_level_allows_raw_fast_path(level: CompressionLevel) -> bool {
    match level {
        CompressionLevel::Fastest | CompressionLevel::Default => true,
        CompressionLevel::Level(level) => (0..=3).contains(&level),
        CompressionLevel::Uncompressed | CompressionLevel::Better | CompressionLevel::Best => false,
    }
}

#[inline]
fn update_sample_metrics(
    sample: &[u8],
    counts: &mut [u16; 256],
    repeat_table: &mut [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN],
    repeats: &mut usize,
    sampled_quads: &mut usize,
) {
    for &byte in sample {
        counts[byte as usize] += 1;
    }
    let mut idx = 0usize;
    while idx + 4 <= sample.len() {
        let quad = u32::from_le_bytes([
            sample[idx],
            sample[idx + 1],
            sample[idx + 2],
            sample[idx + 3],
        ]);
        let slot = ((quad.wrapping_mul(INCOMPRESSIBLE_REPEAT_HASH_MULT) as usize)
            >> (32 - INCOMPRESSIBLE_REPEAT_TABLE_BITS))
            & (INCOMPRESSIBLE_REPEAT_TABLE_LEN - 1);
        *sampled_quads += 1;
        if repeat_table[slot] == quad {
            *repeats += 1;
        } else {
            repeat_table[slot] = quad;
        }
        idx += 4;
    }
}

#[inline]
pub(crate) fn block_looks_incompressible(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }

    let sample_len = block.len().min(RAW_FAST_PATH_MAX_SAMPLE_LEN);
    if sample_len < RAW_FAST_PATH_MIN_SAMPLE_LEN {
        return false;
    }

    let mut counts = [0u16; 256];
    let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    let mut repeats = 0usize;
    let mut sampled_quads = 0usize;

    if sample_len == block.len() {
        update_sample_metrics(
            block,
            &mut counts,
            &mut repeat_table,
            &mut repeats,
            &mut sampled_quads,
        );
    } else {
        // Probe both ends to avoid classifying mixed-entropy blocks from prefix only.
        let head_len = sample_len / 2;
        let tail_len = sample_len - head_len;
        let head = &block[..head_len];
        let tail = &block[block.len() - tail_len..];
        update_sample_metrics(
            head,
            &mut counts,
            &mut repeat_table,
            &mut repeats,
            &mut sampled_quads,
        );
        update_sample_metrics(
            tail,
            &mut counts,
            &mut repeat_table,
            &mut repeats,
            &mut sampled_quads,
        );
    }

    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    let max_symbol_guard = sample_len / 24;
    let repeat_guard = sampled_quads / 64 + 1;
    distinct >= 200 && max_freq <= max_symbol_guard && repeats <= repeat_guard
}
