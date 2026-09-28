//! Gathers a block's literal runs into the literals buffer after matching.
//!
//! The matcher reports each sequence's literal length, and the runs sit in the
//! block's source in order: run `i` starts where run `i - 1` and its match
//! ended. Copying them in one pass after the match loop keeps the copy out of
//! that loop, and lets the pass run under the CPU tier chosen once for the
//! block, so no copy below the dispatch asks which kernel to use.

use crate::decoding::simd_copy;
use crate::encoding::fastpath::FastpathKernel;
use crate::encoding::workspace::RegionVec;

use super::RawSequence;

/// Run length at or above which a run is handed to `memcpy`: the copy is
/// bandwidth-bound there and ERMS `rep movsb` beats a vector loop. Below it the
/// inline kernels win on the call and startup cost they avoid.
const LITERAL_INLINE_COPY_MAX: usize = 2048;

/// Append the literal runs of `sequences`, then `tail` trailing literal bytes,
/// from `block` to `dst`.
///
/// `kernel` is the tier resolved when the compressor was built; this is the
/// only place it is looked at, once per block.
///
/// # Panics
/// If the sequences and the tail reach past `block`, which a matcher that
/// reports its sequences correctly never produces.
pub(super) fn gather_literals(
    kernel: FastpathKernel,
    block: &[u8],
    sequences: &[RawSequence],
    tail: usize,
    dst: &mut RegionVec<u8>,
) {
    // The literals of a block are at most the block, and the buffer is sized
    // for the largest block, so this holds on every internal path. It is still
    // checked, once, because the loop below writes through a raw pointer.
    if dst.capacity() - dst.len() < block.len() {
        crate::encoding::workspace::capacity_exhausted();
    }
    match kernel {
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-avx2"
        ))]
        // SAFETY: the tier is only selected after confirming AVX2 (and BMI2).
        FastpathKernel::Avx2Bmi2 => unsafe { gather_avx2(block, sequences, tail, dst) },
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-sse"
        ))]
        // SAFETY: both tiers are only selected after confirming SSE2.
        FastpathKernel::Sse2 | FastpathKernel::Sse42 => unsafe {
            gather_sse2(block, sequences, tail, dst)
        },
        #[cfg(all(
            target_arch = "aarch64",
            target_endian = "little",
            feature = "kernel-neon"
        ))]
        FastpathKernel::Neon => gather_neon(block, sequences, tail, dst),
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ))]
        FastpathKernel::Simd128 => gather_simd128(block, sequences, tail, dst),
        _ => gather_scalar(block, sequences, tail, dst),
    }
}

/// Bytes of the block that must remain past a run for it to be copied in whole
/// chunks: the widest chunk rounds a run up by at most 31 bytes, read from the
/// block and written into the literals buffer, which is at least as long.
const WILDCOPY_SLACK: usize = 32;

/// One pass over the runs. A run with [`WILDCOPY_SLACK`] bytes of block after
/// it is copied in whole `$chunk`-byte chunks by `$wild`, overshooting into
/// bytes the next run overwrites (upstream zstd `ZSTD_storeSeq` +
/// `ZSTD_wildcopy`); the runs near the block's end take the exact ladder, with
/// `$medium` for `33..2048` bytes. A macro so each tier's loop is compiled
/// inside its own `#[target_feature]` function and its kernels inline into it.
macro_rules! gather_body {
    ($block:expr, $sequences:expr, $tail:expr, $dst:expr, $medium:path, $wild:path, $chunk:expr) => {{
        let block: &[u8] = $block;
        let dst: &mut RegionVec<u8> = $dst;
        let src = block.as_ptr();
        let src_len = block.len();
        let start = dst.len();
        let out = unsafe { dst.as_mut_ptr().add(start) };
        let mut written = 0usize;
        let mut pos = 0usize;
        macro_rules! copy_run {
            ($len:expr) => {{
                let len = $len;
                // `pos <= src_len` holds on every iteration.
                let room = src_len - pos;
                // SAFETY (both arms): `written <= pos`, and the capacity check in
                // `gather_literals` gives the buffer `src_len` bytes past
                // `start`. The chunked arm reads and writes at most
                // `len + $chunk - 1 < len + WILDCOPY_SLACK <= room` bytes from
                // `pos` / `written`; the exact arm, `len <= room` bytes.
                unsafe {
                    let s = src.add(pos);
                    let d = out.add(written);
                    if room >= WILDCOPY_SLACK && len <= room - WILDCOPY_SLACK {
                        // A repcode match right after the last one has no
                        // literals, and the wild copy would still run its loop
                        // checks for none (skipping it measured -1.0% at level
                        // 1 and -0.5% at level 4 on decodecorpus z000033).
                        if len < LITERAL_INLINE_COPY_MAX {
                            if len != 0 {
                                $wild(s, d, len.next_multiple_of($chunk));
                            }
                        } else {
                            core::ptr::copy_nonoverlapping(s, d, len);
                        }
                    } else {
                        assert!(len <= room, "sequences reach past the block they describe");
                        if len <= 32 {
                            if len != 0 {
                                simd_copy::copy_exact_small(s, d, len);
                            }
                        } else if len < LITERAL_INLINE_COPY_MAX {
                            $medium(s, d, len);
                        } else {
                            core::ptr::copy_nonoverlapping(s, d, len);
                        }
                    }
                }
                written += len;
            }};
        }
        for seq in $sequences {
            let ll = seq.ll as usize;
            copy_run!(ll);
            pos += ll;
            // The next run is read at `pos`, so the match is held to the block
            // as the literals are; `pos <= src_len` holds on every iteration.
            let ml = seq.ml as usize;
            assert!(
                ml <= src_len - pos,
                "sequences reach past the block they describe"
            );
            pos += ml;
        }
        copy_run!($tail);
        // Once per block, in every build: a custom matcher that stops short
        // would leave the block's end out of the frame.
        assert_eq!(pos + $tail, src_len, "sequences must cover the block");
        // SAFETY: `[start, start + written)` was written above, within capacity.
        unsafe { dst.set_len(start + written) };
    }};
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[target_feature(enable = "avx2")]
unsafe fn gather_avx2(
    block: &[u8],
    sequences: &[RawSequence],
    tail: usize,
    dst: &mut RegionVec<u8>,
) {
    gather_body!(
        block,
        sequences,
        tail,
        dst,
        simd_copy::copy_exact_avx2,
        simd_copy::copy_avx2,
        32
    )
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-sse"
))]
#[target_feature(enable = "sse2")]
unsafe fn gather_sse2(
    block: &[u8],
    sequences: &[RawSequence],
    tail: usize,
    dst: &mut RegionVec<u8>,
) {
    gather_body!(
        block,
        sequences,
        tail,
        dst,
        simd_copy::copy_exact_sse2,
        simd_copy::copy_sse2,
        16
    )
}

#[cfg(all(
    target_arch = "aarch64",
    target_endian = "little",
    feature = "kernel-neon"
))]
fn gather_neon(block: &[u8], sequences: &[RawSequence], tail: usize, dst: &mut RegionVec<u8>) {
    #[cfg(target_feature = "neon")]
    gather_body!(
        block,
        sequences,
        tail,
        dst,
        simd_copy::copy_exact_neon,
        simd_copy::copy_neon,
        16
    );
    #[cfg(not(target_feature = "neon"))]
    gather_body!(
        block,
        sequences,
        tail,
        dst,
        simd_copy::copy_exact_u64,
        simd_copy::copy_scalar,
        core::mem::size_of::<usize>()
    );
}

/// wasm SIMD is a compile-time feature, so the tier needs no
/// `#[target_feature]` wrapper; it takes its own loop all the same, with the
/// 16-byte `v128` kernels.
#[cfg(all(
    target_arch = "wasm32",
    target_feature = "simd128",
    feature = "kernel-simd128"
))]
fn gather_simd128(block: &[u8], sequences: &[RawSequence], tail: usize, dst: &mut RegionVec<u8>) {
    gather_body!(
        block,
        sequences,
        tail,
        dst,
        simd_copy::copy_exact_simd128,
        simd_copy::copy_simd128,
        16
    )
}

fn gather_scalar(block: &[u8], sequences: &[RawSequence], tail: usize, dst: &mut RegionVec<u8>) {
    gather_body!(
        block,
        sequences,
        tail,
        dst,
        simd_copy::copy_exact_u64,
        simd_copy::copy_scalar,
        core::mem::size_of::<usize>()
    )
}
