//! Shared byte-histogram helpers used by entropy-table builders.
//!
//! Follows the donor strategy from `zstd/lib/compress/hist.c`:
//! a scalar path for small inputs and a striped counting path for larger inputs.

const PARALLEL_COUNT_THRESHOLD: usize = 1500;

#[inline]
fn count_bytes_scalar(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    counts.fill(0);
    if data.is_empty() {
        return (0, 0);
    }

    let mut max_symbol = 0usize;
    let mut largest_count = 0usize;
    for &byte in data {
        let symbol = byte as usize;
        let next = counts[symbol] + 1;
        counts[symbol] = next;
        max_symbol = max_symbol.max(symbol);
        largest_count = largest_count.max(next);
    }

    (max_symbol, largest_count)
}

#[inline]
fn count_bytes_parallel(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    let mut counting1 = [0u32; 256];
    let mut counting2 = [0u32; 256];
    let mut counting3 = [0u32; 256];
    let mut counting4 = [0u32; 256];
    let mut index = 0usize;

    while index + 16 <= data.len() {
        let lane0 = u32::from_le_bytes(data[index..index + 4].try_into().unwrap());
        let lane1 = u32::from_le_bytes(data[index + 4..index + 8].try_into().unwrap());
        let lane2 = u32::from_le_bytes(data[index + 8..index + 12].try_into().unwrap());
        let lane3 = u32::from_le_bytes(data[index + 12..index + 16].try_into().unwrap());
        index += 16;

        counting1[(lane0 & 0xFF) as usize] += 1;
        counting2[((lane0 >> 8) & 0xFF) as usize] += 1;
        counting3[((lane0 >> 16) & 0xFF) as usize] += 1;
        counting4[(lane0 >> 24) as usize] += 1;

        counting1[(lane1 & 0xFF) as usize] += 1;
        counting2[((lane1 >> 8) & 0xFF) as usize] += 1;
        counting3[((lane1 >> 16) & 0xFF) as usize] += 1;
        counting4[(lane1 >> 24) as usize] += 1;

        counting1[(lane2 & 0xFF) as usize] += 1;
        counting2[((lane2 >> 8) & 0xFF) as usize] += 1;
        counting3[((lane2 >> 16) & 0xFF) as usize] += 1;
        counting4[(lane2 >> 24) as usize] += 1;

        counting1[(lane3 & 0xFF) as usize] += 1;
        counting2[((lane3 >> 8) & 0xFF) as usize] += 1;
        counting3[((lane3 >> 16) & 0xFF) as usize] += 1;
        counting4[(lane3 >> 24) as usize] += 1;
    }

    while index < data.len() {
        counting1[data[index] as usize] += 1;
        index += 1;
    }

    let mut max_symbol = 0usize;
    let mut largest_count = 0usize;
    for symbol in 0..256 {
        let value = (counting1[symbol] + counting2[symbol] + counting3[symbol] + counting4[symbol])
            as usize;
        counts[symbol] = value;
        if value > 0 {
            max_symbol = symbol;
            largest_count = largest_count.max(value);
        }
    }

    (max_symbol, largest_count)
}

/// Counts byte frequencies in `data` and writes them into `counts`.
///
/// Returns `(max_symbol, largest_count)` where:
/// - `max_symbol` is the highest symbol index with non-zero count
/// - `largest_count` is the highest observed frequency
///
/// Uses a scalar path for small buffers and a striped-count path for larger
/// buffers. On AArch64 + `std`, dispatches through an SVE2-gated variant when
/// the runtime reports `sve2` support.
pub(crate) fn count_bytes(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    if data.len() < PARALLEL_COUNT_THRESHOLD {
        return count_bytes_scalar(data, counts);
    }

    #[cfg(all(feature = "std", target_arch = "aarch64"))]
    if std::arch::is_aarch64_feature_detected!("sve2") {
        // SAFETY: runtime detection guarantees SVE2 support for this call site.
        return unsafe { count_bytes_sve2(data, counts) };
    }

    count_bytes_parallel(data, counts)
}

#[cfg(all(feature = "std", target_arch = "aarch64"))]
#[target_feature(enable = "sve2")]
unsafe fn count_bytes_sve2(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    // Rust stable does not expose HISTCNT intrinsics yet, so we keep the same
    // striped algorithm while compiling this variant with SVE2 enabled.
    count_bytes_parallel(data, counts)
}

#[cfg(test)]
mod tests {
    use super::{PARALLEL_COUNT_THRESHOLD, count_bytes, count_bytes_scalar};

    fn make_data(len: usize, seed: u64) -> alloc::vec::Vec<u8> {
        let mut state = seed;
        let mut out = alloc::vec![0u8; len];
        for byte in out.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 32) as u8;
        }
        out
    }

    #[test]
    fn count_bytes_matches_scalar_for_large_input() {
        let data = make_data(8192, 0xDEADBEEF);
        let mut fast = [0usize; 256];
        let mut scalar = [0usize; 256];

        let fast_meta = count_bytes(&data, &mut fast);
        let scalar_meta = count_bytes_scalar(&data, &mut scalar);

        assert_eq!(fast, scalar);
        assert_eq!(fast_meta, scalar_meta);
    }

    #[test]
    fn count_bytes_handles_empty_input() {
        let mut counts = [123usize; 256];
        let meta = count_bytes(&[], &mut counts);

        assert_eq!(meta, (0, 0));
        assert!(counts.iter().all(|value| *value == 0));
    }

    #[test]
    fn count_bytes_parallel_handles_tail() {
        let data = make_data(PARALLEL_COUNT_THRESHOLD + 7, 42);
        let mut fast = [0usize; 256];
        let mut scalar = [0usize; 256];

        let fast_meta = count_bytes(&data, &mut fast);
        let scalar_meta = count_bytes_scalar(&data, &mut scalar);

        assert_eq!(fast, scalar);
        assert_eq!(fast_meta, scalar_meta);
    }
}
