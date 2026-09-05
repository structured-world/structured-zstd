use super::CompressionLevel;
use alloc::vec;
use alloc::vec::Vec;

/// What the block's own bytes cannot answer: has this content been seen
/// earlier in the frame?
///
/// The classifier below decides from the block in hand, so a block that is
/// incompressible within itself but duplicates an earlier one looks exactly
/// like noise and, taken alone, would be written off unsearched — throwing
/// away a match the size of the duplicate. This records a fingerprint at every
/// grid position of every block that reaches the decision, and reports whether
/// the block in hand collides with one; a collision sends it to the search.
///
/// Which positions are sampled is decided by the CONTENT at them, not by where
/// they fall: a position is an anchor when its eight bytes hash into a chosen
/// slice of the hash space. The same content therefore anchors at the same
/// places wherever it appears, so a duplicate is recognised however it is
/// shifted — sampling on a position grid instead would see a repeat only at
/// distances that happen to be a multiple of the step, and miss, say, a block
/// that repeats the previous one after two inserted bytes.
///
/// Each slot carries the frame offset it was recorded at, so a fingerprint is
/// consulted only while the matcher could still reach that far back and expires
/// on its own. Clearing the whole table on a window boundary instead would
/// forget content that is still in reach — a block-sized window would drop the
/// block a duplicate is about to match against.
///
/// A collision that is merely a hash coincidence costs a search, never
/// correctness, and at a 32-bit fingerprint over a few thousand live entries
/// that is rarer than one frame in a million.
#[derive(Debug, Default)]
pub(crate) struct SeenContentGrid {
    /// `fingerprint == 0` marks an empty slot, so a fingerprint is forced
    /// non-zero. `at` is the frame offset the sample was taken at.
    slots: Vec<SeenSample>,
    /// Bytes of this frame recorded so far. `u64` on every target: a frame can
    /// be longer than a 16- or 32-bit address space, and only this counter
    /// would notice.
    frame_offset: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeenSample {
    fingerprint: u32,
    at: u64,
}

impl SeenContentGrid {
    const SLOTS: usize = 4096;
    /// Bytes read per sample.
    const KEY_LEN: usize = 8;
    /// One position in this many is an anchor, on average: 256 per 128 KiB
    /// block, dense enough that a duplicate collides hundreds of times over and
    /// sparse enough that the table holds many blocks' worth.
    const ANCHOR_IN: u64 = 512;

    pub(crate) fn reset_for_frame(&mut self) {
        if self.slots.is_empty() {
            self.slots = vec![SeenSample::default(); Self::SLOTS];
        } else {
            self.slots.fill(SeenSample::default());
        }
        self.frame_offset = 0;
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.slots.capacity() * core::mem::size_of::<SeenSample>()
    }

    /// Record `block`'s anchors and report whether any was already there.
    ///
    /// Recording happens whatever the answer: a block that goes out raw is
    /// still content a later block may duplicate, and one that gets searched
    /// is indexed by the matcher but only for as long as the window holds it.
    /// `window_size` is that reach; pass `0` when it is unknown, which keeps
    /// every fingerprint for the frame.
    pub(crate) fn record_and_report_repeat(&mut self, block: &[u8], window_size: usize) -> bool {
        // Too short to key on. The offset still advances, so the ages of what
        // follows stay true distances in the stream.
        if block.len() < Self::KEY_LEN {
            self.frame_offset += block.len() as u64;
            return false;
        }
        if self.slots.is_empty() {
            self.reset_for_frame();
        }
        let reach = if window_size == 0 {
            u64::MAX
        } else {
            window_size as u64
        };
        let mut repeat = false;
        let mut at = 0usize;
        let last = block.len().saturating_sub(Self::KEY_LEN);
        while at <= last {
            let key = u64::from_le_bytes(
                block[at..at + Self::KEY_LEN]
                    .try_into()
                    .expect("the slice is KEY_LEN bytes"),
            );
            // One mixing step feeds all three questions from disjoint bits:
            // whether this position anchors at all, which slot it lands in, and
            // what is compared there — so a slot collision alone does not read
            // as a repeat.
            let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            if (mixed >> 55) % Self::ANCHOR_IN == 0 {
                let slot = (mixed >> 43) as usize & (Self::SLOTS - 1);
                let fingerprint = ((mixed >> 11) as u32) | 1;
                let here = self.frame_offset + at as u64;
                let held = self.slots[slot];
                if held.fingerprint == fingerprint && here - held.at <= reach {
                    repeat = true;
                } else {
                    self.slots[slot] = SeenSample {
                        fingerprint,
                        at: here,
                    };
                }
            }
            at += 1;
        }
        self.frame_offset += block.len() as u64;
        repeat
    }
}

pub(crate) const RAW_FAST_PATH_MIN_BLOCK_LEN: usize = 512;
pub(crate) const RAW_FAST_PATH_MAX_SAMPLE_LEN: usize = 4096;
pub(crate) const RAW_FAST_PATH_MIN_SAMPLE_LEN: usize = 32;
/// How densely a block written off unsearched is still indexed.
///
/// It has to be findable at all, or a later block duplicating it has nothing
/// to match against; it does not have to be findable at every position. The
/// duplicate is recognised on the [`SeenContentGrid`] grid and then searched,
/// and the search sweeps positions, so an entry every `STEP` bytes is hit
/// within `STEP` bytes of scanning — immaterial against a block-sized match.
/// Indexing more finely is what the skip exists to avoid: at one entry per
/// eight bytes a megabyte of incompressible input costs 131,000 stores it had
/// no use for, which measured as a four-fold slowdown on the fast levels.
pub(crate) const RAW_SKIP_INDEX_STEP: usize = 512;

/// Window-size ceiling (8 MiB) above which a numeric level does not take the
/// skip whatever its number: the further back a match may reach, the more a
/// block written off unsearched can be throwing away. `Best` reads the same
/// ceiling; the three named levels below it always may.
const RAW_FAST_PATH_MAX_WINDOW_LOG: u8 = 23;
const RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES: u64 = 1u64 << RAW_FAST_PATH_MAX_WINDOW_LOG;

// Keep classifier scratch modest for no_std/small-stack targets: 1024 slots
// cuts per-call stack for repeat tracking from ~8 KiB to ~4 KiB.
const INCOMPRESSIBLE_REPEAT_TABLE_BITS: usize = 10;
const INCOMPRESSIBLE_REPEAT_TABLE_LEN: usize = 1 << INCOMPRESSIBLE_REPEAT_TABLE_BITS;
const INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS: usize = INCOMPRESSIBLE_REPEAT_TABLE_LEN / 64;
const INCOMPRESSIBLE_REPEAT_HASH_MULT: u32 = 0x9E37_79B1;
const INCOMPRESSIBLE_MIN_DISTINCT_BYTES: usize = 200;
// Allow at most ~4.2% concentration for the most frequent symbol in sampled data.
// This guards against low-entropy text-like inputs being misclassified as random.
const INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR: usize = 24;
// Allow limited 4-byte hash-bucket repeats before treating the sample as structured.
const INCOMPRESSIBLE_REPEAT_DIVISOR: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StrictProbeSelection {
    probe_len: usize,
    tail_start: Option<usize>,
    mid_start: Option<usize>,
}

impl StrictProbeSelection {
    #[inline]
    const fn reuses_full_block_classification(self) -> bool {
        self.tail_start.is_none()
    }
}

#[inline]
fn select_strict_probes(block_len: usize) -> StrictProbeSelection {
    let probe_len = RAW_FAST_PATH_MIN_BLOCK_LEN.min(block_len);
    if probe_len == block_len {
        StrictProbeSelection {
            probe_len,
            tail_start: None,
            mid_start: None,
        }
    } else {
        let tail_start = block_len - probe_len;
        if tail_start < probe_len {
            // For [probe_len + 1, 2 * probe_len), head/tail would heavily overlap.
            // Reuse the full-block classification computed by the caller.
            StrictProbeSelection {
                probe_len,
                tail_start: None,
                mid_start: None,
            }
        } else if tail_start < 2 * probe_len {
            // For [2 * probe_len, 3 * probe_len), head/tail are separable but a
            // distinct non-overlapping middle probe is not.
            StrictProbeSelection {
                probe_len,
                tail_start: Some(tail_start),
                mid_start: None,
            }
        } else {
            // Once we can separate all windows, use head/mid/tail probing.
            StrictProbeSelection {
                probe_len,
                tail_start: Some(tail_start),
                mid_start: Some(tail_start / 2),
            }
        }
    }
}

#[inline]
pub(crate) fn compression_level_allows_raw_fast_path(
    level: CompressionLevel,
    window_size: u64,
) -> bool {
    match level {
        // The named variants resolve to levels 1 / 3 / 7, all inside the band
        // where a misjudged block is cheap; `Best` is level 13, which is not.
        CompressionLevel::Fastest | CompressionLevel::Default | CompressionLevel::Better => true,
        CompressionLevel::Best => window_size <= RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES,
        CompressionLevel::Level(_) => window_size <= RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES,
        CompressionLevel::Uncompressed => false,
    }
}

/// Accumulate byte counts and 4-byte repeat hits for one sample region in a
/// single pass.
///
/// Returns `true` as soon as the running repeat count passes `repeat_guard`.
/// That guard is the FINAL threshold (fixed before any region is scanned)
/// and the repeat count only grows, so an early `true` is exactly the
/// verdict (`false` = compressible) that a full scan would have produced —
/// the `repeats <= repeat_guard` term of the final verdict is already
/// settled. On repetitive data (structured text, a single long match) the
/// quad repeats pass the guard within the first few hundred bytes; on random
/// data the guard is never reached and the whole region is counted, with no
/// per-byte branch in the hot path (the byte counts and the quad hashing
/// share one pass instead of the previous two).
#[inline]
fn scan_sample_region(
    sample: &[u8],
    // Wide enough for a whole block, not just a sample: the dictionary-aware
    // classifier scans the full 128 KiB, and a byte can appear more than 65,535
    // times there without the quad-repeat guard firing first (distinct quads
    // sharing one byte value do exactly that), which a narrower counter would
    // wrap or panic on.
    counts: &mut [u32; 256],
    repeat_table: &mut [u32; INCOMPRESSIBLE_REPEAT_TABLE_LEN],
    repeat_occupied: &mut [u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS],
    repeats: &mut usize,
    repeat_guard: usize,
) -> bool {
    let mut idx = 0usize;
    let len = sample.len();
    while idx + 4 <= len {
        counts[sample[idx] as usize] += 1;
        counts[sample[idx + 1] as usize] += 1;
        counts[sample[idx + 2] as usize] += 1;
        counts[sample[idx + 3] as usize] += 1;
        let quad = u32::from_le_bytes([
            sample[idx],
            sample[idx + 1],
            sample[idx + 2],
            sample[idx + 3],
        ]);
        // Top `INCOMPRESSIBLE_REPEAT_TABLE_BITS` bits of the 32-bit hash give
        // the slot directly: the `as usize` value is `< 2^32`, so the shift
        // by `32 - BITS` already yields an index in `0..TABLE_LEN`. No mask
        // needed (upstream zstd `ZSTD_hashPtr` shape).
        let slot = (quad.wrapping_mul(INCOMPRESSIBLE_REPEAT_HASH_MULT) as usize)
            >> (32 - INCOMPRESSIBLE_REPEAT_TABLE_BITS);
        let word = slot / 64;
        let bit = 1_u64 << (slot % 64);
        let occupied = (repeat_occupied[word] & bit) != 0;
        if occupied && repeat_table[slot] == quad {
            *repeats += 1;
            if *repeats > repeat_guard {
                return true;
            }
        } else {
            repeat_table[slot] = quad;
            repeat_occupied[word] |= bit;
        }
        idx += 4;
    }
    // Tail bytes that don't form a full quad still count toward the symbol
    // histogram used by the final distinct / max-frequency verdict.
    while idx < len {
        counts[sample[idx] as usize] += 1;
        idx += 1;
    }
    false
}

#[inline]
pub(crate) fn block_looks_incompressible(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    sample_looks_incompressible(block)
}

/// Dict-aware incompressibility check: stricter than the plain no-dict
/// heuristic. With a dictionary attached, a block that LOOKS high-entropy in a
/// small fixed sample can still compress — either against the dict, or via a
/// long-range internal repeat the capped sample never spans. So sample the WHOLE
/// block, which surfaces those repeats; only blocks that stay high-entropy
/// across their full length are skipped to raw. Truly random data is still
/// classified incompressible (no repeats anywhere), so the no-dict-quality
/// rejection of incompressible input is preserved — it is only harder to trip on
/// the dict path, never weaker.
///
/// This covers INTERNAL repeats only. EXTERNAL dict matches (a dict segment
/// embedded in otherwise-incompressible input — content this content-only sample
/// can never see) are caught by a SEPARATE layer at the call site: the raw skip
/// fires only when this returns `true` AND `Matcher::block_samples_match_dict`
/// finds no extendable dict match. So a block that matches the dictionary is
/// never emitted raw, even though this function, by design, does not probe the
/// dict itself.
#[inline]
pub(crate) fn block_looks_incompressible_dict(block: &[u8]) -> bool {
    if block.len() < RAW_FAST_PATH_MIN_BLOCK_LEN {
        return false;
    }
    sample_looks_incompressible_capped(block, block.len())
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
    let selection = select_strict_probes(block.len());
    if selection.reuses_full_block_classification() {
        // The full-block sample above already classified this input. For
        // minimum and near-min blocks, split probes would overlap too heavily.
        return true;
    }
    let probe_len = selection.probe_len;
    let tail_start = selection
        .tail_start
        .expect("strict probe tail_start should be present for split probes");
    let head = &block[..probe_len];
    let tail = &block[tail_start..tail_start + probe_len];
    if let Some(mid_start) = selection.mid_start {
        let mid = &block[mid_start..mid_start + probe_len];
        sample_looks_incompressible(head)
            && sample_looks_incompressible(mid)
            && sample_looks_incompressible(tail)
    } else {
        sample_looks_incompressible(head) && sample_looks_incompressible(tail)
    }
}

#[inline]
fn sample_looks_incompressible(block: &[u8]) -> bool {
    sample_looks_incompressible_capped(block, RAW_FAST_PATH_MAX_SAMPLE_LEN)
}

/// As [`sample_looks_incompressible`] but with an explicit sample cap. A larger
/// cap scans more of the block, so it detects LONG-RANGE repeats (a region that
/// re-occurs far away — e.g. a record drawn from a dictionary, or a block whose
/// second half repeats its first) that the small fixed sample misses by only
/// looking at disjoint head/mid/tail windows. Used by the dict-aware check,
/// which samples the whole block: a high-entropy-LOOKING block that actually
/// repeats (and so will compress, dict or not) must not be skipped to raw.
fn sample_looks_incompressible_capped(block: &[u8], max_sample_len: usize) -> bool {
    let sample_len = block.len().min(max_sample_len);
    if sample_len < RAW_FAST_PATH_MIN_SAMPLE_LEN {
        return false;
    }

    // Select the sampled regions: the whole block when it fits the cap, or
    // head / middle / tail probes so capped samples still reject
    // mixed-entropy blocks whose center is compressible.
    let mut regions: [&[u8]; 3] = [&[], &[], &[]];
    let region_count = if sample_len == block.len() {
        regions[0] = block;
        1
    } else {
        let head_len = sample_len / 3;
        let mid_len = sample_len / 3;
        let tail_len = sample_len - head_len - mid_len;
        let mid_start = (block.len() - mid_len) / 2;
        regions[0] = &block[..head_len];
        regions[1] = &block[mid_start..mid_start + mid_len];
        regions[2] = &block[block.len() - tail_len..];
        3
    };

    // `repeat_guard` is the FINAL verdict threshold, fixed before scanning.
    // It needs the total 4-byte-quad count up front (one quad per 4 bytes of
    // each region) so `scan_sample_region` can bail the moment the running
    // repeat count passes it.
    let max_symbol_guard = sample_len / INCOMPRESSIBLE_MAX_SYMBOL_DIVISOR;
    let total_quads: usize = regions[..region_count].iter().map(|r| r.len() / 4).sum();
    let repeat_guard = total_quads / INCOMPRESSIBLE_REPEAT_DIVISOR + 1;

    let mut counts = [0u32; 256];
    let mut repeat_table = [u32::MAX; INCOMPRESSIBLE_REPEAT_TABLE_LEN];
    // Bitset occupancy keeps this path no_std-friendly while avoiding the
    // larger per-slot bool map (and extra matcher-level scratch state).
    let mut repeat_occupied = [0_u64; INCOMPRESSIBLE_REPEAT_OCCUPANCY_WORDS];
    let mut repeats = 0usize;

    for region in &regions[..region_count] {
        if scan_sample_region(
            region,
            &mut counts,
            &mut repeat_table,
            &mut repeat_occupied,
            &mut repeats,
            repeat_guard,
        ) {
            // The repeat guard was passed — the block is compressible. This
            // is exactly the verdict a full scan would have produced.
            return false;
        }
    }

    let distinct = counts.iter().filter(|&&count| count != 0).count();
    let max_freq = counts.iter().copied().max().unwrap_or(0) as usize;
    distinct >= INCOMPRESSIBLE_MIN_DISTINCT_BYTES
        && max_freq <= max_symbol_guard
        && repeats <= repeat_guard
}

#[cfg(test)]
mod tests;
