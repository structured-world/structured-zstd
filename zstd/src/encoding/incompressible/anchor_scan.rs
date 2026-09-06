//! Finding the anchor bytes in a block.
//!
//! The repeat grid keys on the positions carrying one chosen byte value, which
//! makes its anchors content-defined and therefore shift-invariant: a copy of a
//! block anchors at the same content wherever it lands. Nothing cheaper has
//! that property — sampling positions instead of content answers a block
//! aligned copy and misses every shifted one — so the whole block is read, and
//! the only question left is how fast a byte value can be found in it.
//!
//! One byte in 256 carries the value, so the search dominates: at a word at a
//! time it measured 119 microseconds per mebibyte, which on a fast level is
//! most of the encode of incompressible input.
//!
//! Anchors come back a BATCH at a time rather than one per call. A call that
//! returns the next anchor alone spends its dispatch and prologue on the ~256
//! bytes it scans before finding one, which is what a vector compare was
//! supposed to make cheap: measured that way the vector path was no faster than
//! the word trick. Filling a buffer puts the whole scan loop inside the
//! `#[target_feature]` body, where the compare inlines and the call happens
//! once per batch.
//!
//! Per the kernel rules: the tier is resolved at runtime by the caller and
//! passed in, each tier's body is expanded from one macro (so a
//! `#[target_feature]` function gets its own copy rather than an un-inlinable
//! call), and the scalar path is a complete implementation, not a stub.

use crate::encoding::fastpath::FastpathKernel;

/// How many anchors one call collects. One position in 256 carries the anchor
/// byte, so this covers about 32 KiB of block per call — few enough calls that
/// the dispatch and the prologue disappear, few enough offsets that the buffer
/// stays a handful of cache lines wide.
pub(crate) const ANCHOR_BATCH: usize = 128;

/// Emit the positions a compare mask marks, stopping when the buffer fills.
///
/// `$stride` is how many mask bits one input byte occupies — one for a
/// movemask, four for the NEON narrowing shift. All of a byte's bits are
/// cleared together, not just the lowest: clearing one at a time would report a
/// four-bit lane four times.
macro_rules! drain_mask {
    ($mask:expr, $chunk_at:expr, $stride:expr, $out:expr, $n:expr) => {{
        let mut bits: u64 = $mask;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            let lane = bit / $stride;
            let at = $chunk_at + lane;
            bits &= !(((1u64 << $stride) - 1) << (lane * $stride));
            $out[$n] = at as u32;
            $n += 1;
            if $n == $out.len() {
                return ($n, at + 1);
            }
        }
    }};
}

/// The word trick, and the body every tier falls back to for its tail: mark the
/// bytes equal to `needle` by making them the only zero bytes, then find them
/// with the classic has-zero-byte test.
macro_rules! scalar_fill {
    ($hay:expr, $needle:expr, $out:expr, $n:expr, $from:expr) => {{
        const ONES: u64 = 0x0101_0101_0101_0101;
        const HIGHS: u64 = 0x8080_8080_8080_8080;
        let hay: &[u8] = $hay;
        let spread = ONES * $needle as u64;
        let mut at = $from;
        while at + 8 <= hay.len() {
            // SAFETY: eight bytes are in range by the loop condition.
            let word = unsafe { hay.as_ptr().add(at).cast::<u64>().read_unaligned() };
            let marked = word ^ spread;
            let hits = (marked.wrapping_sub(ONES) & !marked & HIGHS) >> 7;
            drain_mask!(hits, at, 8, $out, $n);
            at += 8;
        }
        while at < hay.len() {
            if hay[at] == $needle {
                $out[$n] = at as u32;
                $n += 1;
                if $n == $out.len() {
                    return ($n, at + 1);
                }
            }
            at += 1;
        }
        ($n, hay.len())
    }};
}

/// Offsets of the next anchors in `hay`, written into `out`.
///
/// Returns how many were written and how far the scan got: everything before
/// that point has been examined, so the caller resumes there. The scan stops
/// early when `out` fills, which is why the second value is not always
/// `hay.len()`.
///
/// `kernel` is the tier the caller resolved once, ahead of its loop; it is a
/// value here, never a per-call detection.
#[inline]
pub(crate) fn fill_anchors(
    kernel: FastpathKernel,
    hay: &[u8],
    needle: u8,
    out: &mut [u32; ANCHOR_BATCH],
) -> (usize, usize) {
    match kernel {
        #[cfg(all(
            target_arch = "aarch64",
            target_endian = "little",
            feature = "kernel-neon"
        ))]
        // SAFETY: the tier value is only produced for a CPU that has NEON.
        FastpathKernel::Neon => unsafe { fill_anchors_neon(hay, needle, out) },
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-avx2"
        ))]
        // SAFETY: the tier value is only produced for a CPU that has AVX2.
        FastpathKernel::Avx2Bmi2 => unsafe { fill_anchors_avx2(hay, needle, out) },
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-sse"
        ))]
        // SAFETY: the tier value is only produced for a CPU that has SSE2.
        FastpathKernel::Sse2 | FastpathKernel::Sse42 => unsafe {
            fill_anchors_sse2(hay, needle, out)
        },
        _ => fill_anchors_scalar(hay, needle, out),
    }
}

fn fill_anchors_scalar(hay: &[u8], needle: u8, out: &mut [u32; ANCHOR_BATCH]) -> (usize, usize) {
    let mut n = 0usize;
    scalar_fill!(hay, needle, out, n, 0)
}

#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
#[target_feature(enable = "neon")]
unsafe fn fill_anchors_neon(
    hay: &[u8],
    needle: u8,
    out: &mut [u32; ANCHOR_BATCH],
) -> (usize, usize) {
    use core::arch::aarch64::*;
    let n_bytes = hay.len();
    let base = hay.as_ptr();
    let mut at = 0usize;
    let mut n = 0usize;
    // SAFETY: every load is bounded by the loop condition.
    unsafe {
        let want = vdupq_n_u8(needle);
        while at + 16 <= n_bytes {
            let eq = vceqq_u8(vld1q_u8(base.add(at)), want);
            // The common answer is "none of these sixteen", and `vmaxvq_u8`
            // settles that in one instruction; the narrowing below is only paid
            // when there is something to report.
            if vmaxvq_u8(eq) != 0 {
                // Four bits per lane, so a matching byte is a set nibble.
                let narrowed = vshrn_n_u16(vreinterpretq_u16_u8(eq), 4);
                let bits = vget_lane_u64(vreinterpret_u64_u8(narrowed), 0);
                drain_mask!(bits, at, 4, out, n);
            }
            at += 16;
        }
    }
    scalar_fill!(hay, needle, out, n, at)
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[target_feature(enable = "sse2")]
unsafe fn fill_anchors_sse2(
    hay: &[u8],
    needle: u8,
    out: &mut [u32; ANCHOR_BATCH],
) -> (usize, usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;
    let n_bytes = hay.len();
    let base = hay.as_ptr();
    let mut at = 0usize;
    let mut n = 0usize;
    // SAFETY: every load is bounded by the loop condition and is the unaligned
    // form.
    unsafe {
        let want = _mm_set1_epi8(needle as i8);
        while at + 16 <= n_bytes {
            let chunk = _mm_loadu_si128(base.add(at).cast());
            let mask = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, want)) as u32;
            if mask != 0 {
                drain_mask!(u64::from(mask), at, 1, out, n);
            }
            at += 16;
        }
    }
    scalar_fill!(hay, needle, out, n, at)
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[target_feature(enable = "avx2")]
unsafe fn fill_anchors_avx2(
    hay: &[u8],
    needle: u8,
    out: &mut [u32; ANCHOR_BATCH],
) -> (usize, usize) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;
    let n_bytes = hay.len();
    let base = hay.as_ptr();
    let mut at = 0usize;
    let mut n = 0usize;
    // SAFETY: every load is bounded by the loop condition and is the unaligned
    // form.
    unsafe {
        let want = _mm256_set1_epi8(needle as i8);
        while at + 32 <= n_bytes {
            let chunk = _mm256_loadu_si256(base.add(at).cast());
            let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, want)) as u32;
            if mask != 0 {
                drain_mask!(u64::from(mask), at, 1, out, n);
            }
            at += 32;
        }
    }
    scalar_fill!(hay, needle, out, n, at)
}

#[cfg(test)]
mod tests;
