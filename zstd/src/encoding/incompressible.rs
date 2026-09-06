use super::CompressionLevel;
use crate::common::MAX_BLOCK_SIZE;
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
/// Recording lands on a fixed grid of stream offsets and probing sweeps a run
/// of CONSECUTIVE positions, which is what makes a shifted duplicate findable
/// without reading the whole block: a copy sits some distance from its
/// original, the probed positions map to original positions that far lower, and
/// among [`Self::PROBE_RUN`] consecutive values exactly one is a multiple of
/// [`Self::RECORD_STEP`] — so exactly one probe meets a recorded key, whatever
/// the distance is. Probing on the same grid it records on would instead see a
/// repeat only at distances that happen to be a multiple of the step, and miss,
/// say, a block that repeats the previous one after two inserted bytes.
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
    /// A slot belongs to the frame whose `epoch` it carries; one from an
    /// earlier frame reads as empty, which is what makes starting a frame free.
    slots: Vec<SeenSample>,
    /// One byte of each slot's key, in a table of its own. A probe run is 512
    /// lookups and nearly all of them miss, so the miss has to be cheap: a byte
    /// per slot keeps a window's worth of them in cache where the same number of
    /// sixteen-byte samples does not, and only a byte that matches is worth
    /// reading the sample for. Never zero for a live record, so a slot no frame
    /// has written cannot match.
    tags: Vec<u8>,
    /// Which frame the live slots belong to. Starts at 0 with a freshly
    /// allocated (all-zero) table, and every frame increments it first, so no
    /// frame ever runs under the epoch a zeroed slot carries.
    epoch: u16,
    /// Bytes of this frame recorded so far. `u64` on every target: a frame can
    /// be longer than a 16- or 32-bit address space, and only this counter
    /// would notice.
    frame_offset: u64,
    /// Frame offset up to which the search stays on after a hit.
    repeat_until: u64,
}

/// Sixteen bytes, which is what the three fields pack into with no padding:
/// widening any of them costs a byte of table for every slot.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SeenSample {
    /// Sixteen bits here and eight more in the tag table: a coincidence costs a
    /// search that finds nothing, never a wrong answer, and at twenty-four bits
    /// over the few thousand probes a frame makes that is one frame in
    /// thousands.
    fingerprint: u16,
    epoch: u16,
    /// The record's offset in units of [`SeenContentGrid::RECORD_STEP`], which
    /// is what every record sits on. A frame would have to run past two
    /// tebibytes for this to be too narrow.
    at_step: u32,
}

impl SeenContentGrid {
    /// Ceiling on the table. Sized so a window's worth of records fits without
    /// evicting: a probe run meets exactly ONE recorded key, so an evicted
    /// record is a repeat missed outright, where the previous scheme had a
    /// couple of hundred chances per block and could afford to lose most of
    /// them. At sixteen bytes a slot this is a megabyte, and only a frame whose
    /// window is that large ever allocates it.
    const SLOTS: usize = 64 * 1024;
    /// Floor on the table, so a tiny window still has room for a few anchors
    /// without a slot collision reading as a repeat on every one.
    const MIN_SLOTS: usize = 64;
    /// Bytes read per sample.
    const KEY_LEN: usize = 8;
    /// Stream offsets that get recorded: every one that is a multiple of this,
    /// in the frame's own coordinates rather than the block's, so the same
    /// content lands on the same offsets however the blocks are cut. 256 records
    /// per 128 KiB block, and a 4 MiB window's worth fits the table without
    /// evicting most of itself.
    const RECORD_STEP: usize = 512;
    /// Consecutive positions probed per run. Equal to [`Self::RECORD_STEP`] by
    /// construction, not by coincidence: among that many consecutive stream
    /// offsets exactly one is a multiple of the step, so a copy at ANY distance
    /// from its original has exactly one probe that meets a recorded key.
    const PROBE_RUN: usize = Self::RECORD_STEP;
    /// How far past a hit the search stays on: two maximum blocks, so an
    /// isolated miss inside repeating content cannot cost a block, while a frame
    /// that stops repeating returns to skipping within a block or two.
    const STICKY_REACH: u64 = 2 * MAX_BLOCK_SIZE as u64;
    /// Start a frame. Every frame calls this, most never consult the table, and
    /// clearing it here cost a 64 KiB fill per frame — a fixed few microseconds
    /// that a small frame pays in full. Stepping the epoch retires the previous
    /// frame's slots instead, and the table is allocated only once a frame
    /// actually records into it.
    pub(crate) fn reset_for_frame(&mut self) {
        // The wrap lands on 0, which is the epoch a freshly allocated slot
        // carries, so the table is cleared on that one frame in sixty-five
        // thousand rather than letting a stale slot read as live.
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.slots.fill(SeenSample::default());
            self.tags.fill(0);
            self.epoch = 1;
        }
        self.frame_offset = 0;
        self.repeat_until = 0;
    }

    pub(crate) fn heap_size(&self) -> usize {
        self.slots.capacity() * core::mem::size_of::<SeenSample>() + self.tags.capacity()
    }

    /// Account for a block the grid does not scan. Ages here are distances in
    /// the stream, and the reach test compares them against the matcher's
    /// window, so a block that skips the scan — RLE, compressible, or ruled out
    /// before the grid is asked — must still move the offset. Leaving it behind
    /// makes an old fingerprint read as in reach long after the matcher has
    /// dropped it, and every later block then pays for a search that cannot
    /// find anything.
    #[inline]
    pub(crate) fn advance_past(&mut self, block_len: usize) {
        self.frame_offset += block_len as u64;
    }

    /// The mixed key at `at`, and the slot it belongs in.
    #[inline]
    fn key_at(&self, block: &[u8], at: usize, mask: usize) -> (usize, u16, u8) {
        let key = u64::from_le_bytes(
            block[at..at + Self::KEY_LEN]
                .try_into()
                .expect("the slice is KEY_LEN bytes"),
        );
        let mixed = Self::avalanche(key);
        // Neither the fingerprint nor the tag is ever zero, so a slot no frame
        // has written cannot read as a match.
        (
            (mixed >> 32) as usize & mask,
            (mixed as u16) | 1,
            (mixed >> 16) as u8 | 1,
        )
    }

    /// Whether the content at `at` was recorded within `reach`. Reads only: the
    /// probe run sweeps consecutive positions, and recording every one of them
    /// would fill the table with keys no later probe can align with.
    #[inline]
    fn probe_key(&self, block: &[u8], at: usize, reach: u64, mask: usize) -> bool {
        let (slot, fingerprint, tag) = self.key_at(block, at, mask);
        // The byte first: this is the only line nearly every probe executes.
        if self.tags[slot] != tag {
            return false;
        }
        let held = self.slots[slot];
        let here = self.frame_offset + at as u64;
        if held.epoch != self.epoch || held.fingerprint != fingerprint {
            return false;
        }
        let recorded = u64::from(held.at_step) * Self::RECORD_STEP as u64;
        // A record is never ahead of a probe that meets it: records for a run go
        // in before the run, and both walk the block forwards. The subtraction
        // is checked all the same — the alternative is a wrap in release that
        // reads as a repeat at a distance of eighteen exabytes.
        debug_assert!(recorded <= here, "a record ahead of the probe that met it");
        here.checked_sub(recorded)
            .is_some_and(|apart| apart <= reach)
    }

    /// Put the content at `at` in the table, dated here.
    ///
    /// Written whether or not the slot was occupied by the same content: leaving
    /// a slot dated to the FIRST occurrence of a repeating run makes the third
    /// block measure its distance from there, which a one-block window reads as
    /// out of reach even though the block right behind it is exactly what the
    /// matcher would find — every other block of the run would go out raw.
    #[inline]
    fn record_key(&mut self, block: &[u8], at: usize, mask: usize) {
        let (slot, fingerprint, tag) = self.key_at(block, at, mask);
        self.tags[slot] = tag;
        let here = self.frame_offset + at as u64;
        debug_assert!(
            here.is_multiple_of(Self::RECORD_STEP as u64),
            "records sit on the grid, which is what lets the offset be stored in steps",
        );
        self.slots[slot] = SeenSample {
            fingerprint,
            epoch: self.epoch,
            at_step: (here / Self::RECORD_STEP as u64) as u32,
        };
    }

    /// Record this block on the grid and report whether it duplicates content
    /// still within `window_size` of here.
    ///
    /// Recording happens whatever the answer: a block that goes out raw is still
    /// content a later block may duplicate, and one that gets searched is
    /// indexed by the matcher but only for as long as the window holds it.
    /// `window_size` is that reach; pass `0` when it is unknown, which keeps
    /// every record for the frame.
    pub(crate) fn record_and_report_repeat(&mut self, block: &[u8], window_size: usize) -> bool {
        // Too short to key on. The offset still advances, so the ages of what
        // follows stay true distances in the stream.
        if block.len() < Self::KEY_LEN {
            self.frame_offset += block.len() as u64;
            return false;
        }
        let wanted = Self::slots_for(window_size);
        if self.slots.len() < wanted {
            self.slots = vec![SeenSample::default(); wanted];
            self.tags = vec![0u8; wanted];
            // Zeroed slots carry epoch 0, so a frame must never run under it.
            self.epoch = self.epoch.max(1);
        }
        let mask = self.slots.len() - 1;
        let reach = if window_size == 0 {
            u64::MAX
        } else {
            window_size as u64
        };
        let mut repeat = false;
        let last = block.len() - Self::KEY_LEN;
        // Neither side reads the whole block. Recording lands on a FIXED grid in
        // the frame's own coordinates — every `RECORD_STEP` bytes of stream, so
        // the same content recorded once is recorded at the same stream offsets
        // however the blocks around it are cut. Probing takes `RECORD_STEP`
        // CONSECUTIVE positions, and that is what makes any shift work: a copy
        // sits at some distance D from its original, the probed positions map to
        // original positions D lower, and among `RECORD_STEP` consecutive values
        // exactly one is a multiple of `RECORD_STEP` — so exactly one probe
        // meets a recorded key, whatever D is.
        //
        // The pass this replaces read every byte looking for content-defined
        // anchors. It was correct and it was the cost: on a fast level a whole
        // extra pass over the block doubles the encode of incompressible input,
        // where the raw path is little more than a copy. This touches about
        // eight kilobytes of a hundred-and-twenty-eight-kilobyte block.
        //
        // Two probe runs, not one: the run at the start answers a duplicate of
        // anything recorded earlier, and the run at the midpoint answers a block
        // whose own first half is the original — a hundred and twenty-eight
        // kilobytes of two identical halves reads as incompressible by any
        // sample of it and halves if the search runs.
        let step = Self::RECORD_STEP as u64;
        // A full run covers every distance a copy could sit at, and on a block
        // of any size it is a rounding error. On a block of a couple of
        // kilobytes it is half the block, and the grid then costs more than the
        // duplicate it could find is worth — a missed one there is bounded by
        // the block. So the run is capped at a probe per sixteen bytes, which
        // reaches the full width by eight kilobytes and stays whole above it.
        let run = Self::PROBE_RUN.min((block.len() / 16).max(8));
        let mut abs = self.frame_offset.next_multiple_of(step);
        let mut halves = [0usize, block.len() / 2].into_iter().peekable();
        while let Some(start) = halves.next() {
            // Records for everything before this run go in FIRST, because the
            // midpoint run's whole job is to meet them: a block whose own first
            // half is the original is invisible to a run that probes before that
            // half has been recorded.
            // Strictly before the run: a grid point recorded at a position the
            // run then probes answers itself, and every block reads as a repeat
            // of itself.
            let until = self.frame_offset + start as u64;
            while abs < until && abs <= self.frame_offset + last as u64 {
                let at = (abs - self.frame_offset) as usize;
                self.record_key(block, at, mask);
                abs += step;
            }
            // Nothing recorded yet, nothing this run could meet: the first
            // block of a frame opens with an empty table, and a single-block
            // frame — every frame of a few kilobytes — would otherwise spend
            // half its probes on a run that cannot hit.
            if self.frame_offset != 0 || start != 0 {
                let end = (start + run).min(last + 1);
                for at in start..end {
                    repeat |= self.probe_key(block, at, reach, mask);
                }
            }
            if halves.peek().is_none() {
                // The rest of the block, once no run is left to probe it.
                while abs <= self.frame_offset + last as u64 {
                    let at = (abs - self.frame_offset) as usize;
                    self.record_key(block, at, mask);
                    abs += step;
                }
            }
        }
        self.frame_offset += block.len() as u64;
        // Content that repeats does not repeat in every block, and the sampling
        // can miss one the matcher would have found — a miss costs a whole block
        // written out raw, so a hit keeps the search on for a stretch after it.
        // The case the skip exists for, a frame of noise, never enters this: it
        // never hits.
        if repeat {
            self.repeat_until = self.frame_offset + Self::STICKY_REACH;
        }
        repeat || self.frame_offset <= self.repeat_until
    }

    /// How many slots a window's worth of anchors needs, rounded up to a power
    /// of two and held to [`Self::SLOTS`].
    ///
    /// The window is the reach: a fingerprint older than that is never
    /// consulted, so a table wider than the anchors the window can hold is
    /// memory a frame allocates and faults for nothing. A kibibyte frame was
    /// taking a 64 KiB table for the handful of anchors it could ever record,
    /// which on the cheapest levels was a third of the encode. Four slots per
    /// anchor keeps eviction rare.
    fn slots_for(window_size: usize) -> usize {
        if window_size == 0 {
            return Self::SLOTS;
        }
        let records = (window_size / Self::RECORD_STEP).max(1) as u64;
        let wanted = (records * 4).next_power_of_two();
        (wanted as usize).clamp(Self::MIN_SLOTS, Self::SLOTS)
    }

    /// Full 64-bit avalanche (splitmix64's finalizer): every output bit depends
    /// on every input bit, which a single multiply does not give — its low half
    /// is barely mixed and its top bits are spoken for by the anchor test.
    fn avalanche(key: u64) -> u64 {
        let mut z = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
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
/// block written off unsearched can be throwing away. The three named levels
/// below `Best` resolve inside the band and always may.
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
        // The window is the whole question, at every level. A level ceiling was
        // tried here — the high levels are where a block written off unsearched
        // costs the most — and it cost 12 to 32 times the encode on
        // incompressible input at levels 16 and up while buying nothing: what
        // the ceiling was guarding against is a repeat written off, and the
        // grid catches those on its own. Four megabytes repeated at nearly the
        // window distance comes out at 4,129,240 bytes against the reference's
        // 4,129,258 with the skip live at level 17.
        CompressionLevel::Fastest | CompressionLevel::Default | CompressionLevel::Better => true,
        CompressionLevel::Best | CompressionLevel::Level(_) => {
            window_size <= RAW_FAST_PATH_MAX_WINDOW_SIZE_BYTES
        }
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
