//! Shared byte-histogram helpers used by entropy-table builders.
//!
//! Follows the donor strategy from `zstd/lib/compress/hist.c`:
//! a scalar path for small inputs and a striped counting path for larger inputs.

const PARALLEL_COUNT_THRESHOLD: usize = 1500;
type CountBytesFn = fn(&[u8], &mut [usize; 256]) -> (usize, usize);

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
    if data.len() > u32::MAX as usize {
        // The striped counters are u32-based; preserve correctness for
        // oversized inputs by using the scalar usize accumulator.
        return count_bytes_scalar(data, counts);
    }

    let mut counting1 = [0u32; 256];
    let mut counting2 = [0u32; 256];
    let mut counting3 = [0u32; 256];
    let mut counting4 = [0u32; 256];
    let mut index = 0usize;

    while index + 16 <= data.len() {
        // SAFETY: loop condition guarantees we can read 16 bytes starting at
        // `index`; read_unaligned matches donor-style lane loads.
        let ptr = unsafe { data.as_ptr().add(index) };
        let lane0 = u32::from_le(unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) });
        let lane1 = u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(4).cast::<u32>()) });
        let lane2 = u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(8).cast::<u32>()) });
        let lane3 = u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(12).cast::<u32>()) });
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
        let value = merge_lane_counts(
            counting1[symbol],
            counting2[symbol],
            counting3[symbol],
            counting4[symbol],
        );
        counts[symbol] = value;
        if value > 0 {
            max_symbol = symbol;
            largest_count = largest_count.max(value);
        }
    }

    (max_symbol, largest_count)
}

#[inline(always)]
fn merge_lane_counts(c1: u32, c2: u32, c3: u32, c4: u32) -> usize {
    (c1 as usize) + (c2 as usize) + (c3 as usize) + (c4 as usize)
}

/// Counts byte frequencies in `data` and writes them into `counts`.
///
/// Returns `(max_symbol, largest_count)` where:
/// - `max_symbol` is the highest symbol index with non-zero count
/// - `largest_count` is the highest observed frequency
///
/// Uses a scalar path for small buffers and a striped-count path for larger
/// buffers. On AArch64 + `std`, dispatches through a cached SVE2-gated variant when
/// the runtime reports `sve2` support.
pub(crate) fn count_bytes(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    if data.len() < PARALLEL_COUNT_THRESHOLD {
        return count_bytes_scalar(data, counts);
    }

    count_bytes_dispatch()(data, counts)
}

#[cfg(all(feature = "std", target_arch = "aarch64"))]
#[inline]
fn count_bytes_dispatch() -> CountBytesFn {
    static DISPATCH: std::sync::OnceLock<CountBytesFn> = std::sync::OnceLock::new();

    *DISPATCH.get_or_init(|| {
        if std::arch::is_aarch64_feature_detected!("sve2") {
            count_bytes_sve2_wrapper
        } else {
            count_bytes_parallel
        }
    })
}

#[cfg(not(all(feature = "std", target_arch = "aarch64")))]
#[inline]
fn count_bytes_dispatch() -> CountBytesFn {
    count_bytes_parallel
}

#[cfg(all(feature = "std", target_arch = "aarch64"))]
#[inline]
fn count_bytes_sve2_wrapper(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    // SAFETY: dispatch cache selects this only when runtime detection reports
    // SVE2 support for the current process.
    unsafe { count_bytes_sve2(data, counts) }
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
    use super::{PARALLEL_COUNT_THRESHOLD, count_bytes, count_bytes_scalar, merge_lane_counts};

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

    #[test]
    fn merge_lane_counts_widens_before_sum() {
        let sum = merge_lane_counts(u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        let expected = 4u64 * (u32::MAX as u64);
        assert_eq!(sum as u64, expected);
    }
}
