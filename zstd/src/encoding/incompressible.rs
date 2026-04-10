use super::CompressionLevel;

pub(crate) const RAW_FAST_PATH_MIN_BLOCK_LEN: usize = 512;
pub(crate) const RAW_FAST_PATH_MAX_SAMPLE_LEN: usize = 4096;
pub(crate) const RAW_FAST_PATH_MIN_SAMPLE_LEN: usize = 32;

const INCOMPRESSIBLE_REPEAT_TABLE_BITS: usize = 11;
const INCOMPRESSIBLE_REPEAT_TABLE_LEN: usize = 1 << INCOMPRESSIBLE_REPEAT_TABLE_BITS;
const INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS: usize = INCOMPRESSIBLE_REPEAT_TABLE_LEN / 64;
const INCOMPRESSIBLE_REPEAT_HASH_MULT: u32 = 0x9E37_79B1;
const INCOMPRESSIBLE_MIN_DISTINCT_BYTES: usize = 200;
// Allow at most ~4.2% concentration for the most frequent symbol in sampled data.
// This guards against low-entropy text-like inputs being misclassified as random.
const INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR: usize = 24;
// Allow limited 4-byte hash-bucket repeats before treating the sample as structured.
const INCOMPRESSIBLE_REPEAT_DIVISOR: usize = 64;

#[inline]
pub(crate) fn compression_level_allows_raw_fast_path(level: CompressionLevel) -> bool {
    match level {
        CompressionLevel::Fastest | CompressionLevel::Default | CompressionLevel::Best => true,
        CompressionLevel::Level(level) => (0..=3).contains(&level),
        CompressionLevel::Uncompressed | CompressionLevel::Better => false,
    }
}

#[inline]
fn update_sample_metrics(
    sample: &[u8],
    counts: &mut [u16; 256],
    repeat_table: &mut [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN],
    repeat_occupied: &mut [u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS],
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
        let word = slot / 64;
        let bit = 1_u64 << (slot % 64);
        let occupied = (repeat_occupied[word] & bit) != 0;
        *sampled_quads += 1;
        if occupied && repeat_table[slot] == quad {
            *repeats += 1;
        } else {
            repeat_table[slot] = quad;
            repeat_occupied[word] |= bit;
        }
        idx += 4;
    }
}

#[inline]
pub(crate) fn block_looks_incompressible(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    sample_looks_incompressible(block)
}

#[inline]
pub(crate) fn block_looks_incompressible_strict(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    if !sample_looks_incompressible(block) {
        return false;
    }
    // Best level should only early-exit on strongly random data. Probe head,
    // middle, and tail so mixed-entropy blocks do not get misclassified.
    let probe_len = RAW_FAST_PATH_MIN_BLOCK_LEN.min(block.len());
    let mid_start = (block.len() - probe_len) / 2;
    let head = &block[..probe_len];
    let mid = &block[mid_start..mid_start + probe_len];
    let tail = &block[block.len() - probe_len..];
    sample_looks_incompressible(head)
        && sample_looks_incompressible(mid)
        && sample_looks_incompressible(tail)
}

#[inline]
fn sample_looks_incompressible(block: &[u8]) -> bool {
    let sample_len = block.len().min(RAW_FAST_PATH_MAX_SAMPLE_LEN);
    if sample_len < RAW_FAST_PATH_MIN_SAMPLE_LEN {
        return false;
    }

    let mut counts = [0u16; 256];
    let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
    let mut repeats = 0usize;
    let mut sampled_quads = 0usize;

    if sample_len == block.len() {
        update_sample_metrics(
            block,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
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
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );
        update_sample_metrics(
            tail,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );
    }

    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    let max_symbol_guard = sample_len / INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR;
    let repeat_guard = sampled_quads / INCOMPRESSIBLE_REPEAT_DIVISOR + 1;
    distinct >= INCOMPRESSIBLE_MIN_DISTINCT_BYTES
        && max_freq <= max_symbol_guard
        && repeats <= repeat_guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_metrics_do_not_count_first_u32_max_as_repeat() {
        let sample = [0xFF_u8; 4];
        let mut counts = [0u16; 256];
        let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
        let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
        let mut repeats = 0usize;
        let mut sampled_quads = 0usize;

        update_sample_metrics(
            &sample,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            &mut sampled_quads,
        );

        assert_eq!(sampled_quads, 1);
        assert_eq!(repeats, 0, "first quad must not be miscounted as a repeat");
    }
}
