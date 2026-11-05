//! Shared byte-histogram helpers used by entropy-table builders.
//!
//! Follows the upstream zstd strategy from `zstd/lib/compress/hist.c`:
//! a scalar path for small inputs and a striped counting path for larger inputs.

use core::mem::MaybeUninit;

const PARALLEL_COUNT_THRESHOLD: usize = 1500;
type CountBytesFn = fn(&[u8], &mut MaybeUninit<[usize; 256]>) -> (usize, usize);

#[inline]
fn count_bytes_scalar(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    counts.fill(0);
    count_bytes_scalar_zeroed(data, counts)
}

#[inline]
fn count_bytes_scalar_zeroed(data: &[u8], counts: &mut [usize; 256]) -> (usize, usize) {
    if data.is_empty() {
        return (0, 0);
    }

    for &byte in data {
        counts[byte as usize] += 1;
    }

    max_symbol_and_largest(counts)
}

/// Writes every one of the 256 counts, so `counts` needs no clearing first.
#[inline(always)]
fn count_bytes_parallel(data: &[u8], counts: &mut MaybeUninit<[usize; 256]>) -> (usize, usize) {
    if data.len() > u32::MAX as usize {
        // The striped counters are u32-based; preserve correctness for
        // oversized inputs by using the scalar usize accumulator.
        return count_bytes_scalar_zeroed(data, counts.write([0; 256]));
    }

    let mut counting1 = [0u32; 256];
    let mut counting2 = [0u32; 256];
    let mut counting3 = [0u32; 256];
    let mut counting4 = [0u32; 256];
    let mut index = 0usize;

    if data.len() >= 16 {
        let end = data.len() - 16;
        while index <= end {
            // SAFETY: loop condition guarantees we can read 16 bytes starting at
            // `index`; read_unaligned matches upstream zstd-style lane loads.
            let ptr = unsafe { data.as_ptr().add(index) };
            let lane0 = u32::from_le(unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) });
            let lane1 =
                u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(4).cast::<u32>()) });
            let lane2 =
                u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(8).cast::<u32>()) });
            let lane3 =
                u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(12).cast::<u32>()) });
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
    }

    while index < data.len() {
        counting1[data[index] as usize] += 1;
        index += 1;
    }

    // A symbol's count is at most `data.len()`, at most `u32::MAX` by the guard
    // above, so its lanes sum in `u32`, and the largest is taken in the same
    // pass, as upstream merges (`HIST_count_parallel_wksp`): an unsigned
    // maximum over the widened counts is several instructions a lane on x86.
    let out = counts.as_mut_ptr().cast::<usize>();
    let mut largest = 0u32;
    for symbol in 0..256 {
        let count = counting1[symbol] + counting2[symbol] + counting3[symbol] + counting4[symbol];
        largest = largest.max(count);
        // SAFETY: `symbol < 256`, inside the array `counts` points at.
        unsafe { out.add(symbol).write(count as usize) };
    }
    // SAFETY: the loop above wrote all 256 counts.
    let counts = unsafe { counts.assume_init_ref() };
    let max_symbol = counts.iter().rposition(|&count| count != 0).unwrap_or(0);
    (max_symbol, largest as usize)
}

/// Read off the finished counts (upstream zstd `HIST_count_simple`): tracked per
/// symbol, the highest one is a branch per symbol on the alphabet's own gaps.
#[inline(always)]
fn max_symbol_and_largest(counts: &[usize; 256]) -> (usize, usize) {
    let max_symbol = counts.iter().rposition(|&count| count != 0).unwrap_or(0);
    let largest_count = counts[..=max_symbol]
        .iter()
        .fold(0, |largest, &c| largest.max(c));
    (max_symbol, largest_count)
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

    // SAFETY: the striped count only ever writes initialised counts, so the
    // array stays initialised through the cast.
    let storage = unsafe { &mut *core::ptr::from_mut(counts).cast::<MaybeUninit<[usize; 256]>>() };
    count_bytes_dispatch()(data, storage)
}

/// [`count_bytes`] into storage nobody cleared, returned initialised: only the
/// scalar path, which counts by increment, zeroes it first (as upstream
/// `HIST_count_simple`); the striped path writes every count anyway.
#[inline]
pub(crate) fn count_bytes_uninit<'a>(
    data: &[u8],
    counts: &'a mut MaybeUninit<[usize; 256]>,
) -> (&'a mut [usize; 256], usize, usize) {
    if data.len() < PARALLEL_COUNT_THRESHOLD {
        let counts = counts.write([0; 256]);
        let (max_symbol, largest) = count_bytes_scalar_zeroed(data, counts);
        return (counts, max_symbol, largest);
    }

    let (max_symbol, largest) = count_bytes_dispatch()(data, counts);
    // SAFETY: the striped count writes all 256 counts.
    (unsafe { counts.assume_init_mut() }, max_symbol, largest)
}

#[cfg(all(feature = "std", target_arch = "aarch64"))]
#[inline]
fn count_bytes_dispatch() -> CountBytesFn {
    static DISPATCH: std::sync::OnceLock<CountBytesFn> = std::sync::OnceLock::new();

    *DISPATCH.get_or_init(|| {
        use crate::cpu_kernel::{CpuLevel, cpu_allows};
        if cpu_allows(CpuLevel::Sve) && std::arch::is_aarch64_feature_detected!("sve2") {
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
fn count_bytes_sve2_wrapper(data: &[u8], counts: &mut MaybeUninit<[usize; 256]>) -> (usize, usize) {
    // SAFETY: dispatch cache selects this only when runtime detection reports
    // SVE2 support for the current process.
    unsafe { count_bytes_sve2(data, counts) }
}

#[cfg(all(feature = "std", target_arch = "aarch64"))]
#[target_feature(enable = "sve2")]
unsafe fn count_bytes_sve2(data: &[u8], counts: &mut MaybeUninit<[usize; 256]>) -> (usize, usize) {
    // Rust stable does not expose HISTCNT intrinsics yet, so we keep the same
    // striped algorithm while compiling this variant with SVE2 enabled.
    count_bytes_parallel(data, counts)
}

#[cfg(test)]
mod tests;
