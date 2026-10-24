//! Long-distance-match (LDM) producer.
//!
//! Implements the upstream zstd's `lib/compress/zstd_ldm.c` pipeline:
//!
//! 1. [`gear_hash`] — gear rolling hash over a 256-entry random
//!    permutation table picks content-defined split points
//!    (`(hash & stopMask) == 0`).
//! 2. [`table`] — bucket-based hash table indexed by the gear-hash
//!    checksum, holding `1 << bucket_size_log` candidate positions
//!    per bucket.
//! 3. [`search`] — verify + extend a candidate forward and
//!    backward across the LDM window (prefix-only path; the
//!    two-segment `extDict` variant is deferred).
//! 4. [`params`] — [`LdmParams`] derived from `windowLog` /
//!    `strategy` (`ZSTD_ldm_adjustParameters` parity).
//!
//! Aggregate [`LdmProducer`] holds the rolling-hash state, the
//! bucket table, and the per-call scratch buffers. The match driver owns
//! one per frame, as upstream keeps `ldmState` in the context: the optimal
//! parser takes its sequences as candidates, every other strategy takes them
//! outright and searches only the literals between them
//! (`ZSTD_ldm_blockCompress`).
//!
//! # Activation policy
//!
//! No [`super::CompressionLevel`] preset turns it on, as upstream
//! `ZSTD_compress(..., level)` keeps LDM off at every level. It is an
//! explicit opt-in: the parameter API
//! (`enable_long_distance_matching`), `ZSTD_c_enableLongDistanceMatching`
//! through the C ABI, and `zstd --long[=N]`.
//!
//! Upstream zstd parity anchors:
//! * `lib/compress/zstd_ldm.c` v1.5.7
//! * `lib/compress/zstd_ldm.h`
//! * `lib/compress/zstd_ldm_geartab.h` — the 256 × `u64` permutation
//!   table reproduced verbatim in [`gear_hash::GEAR_TAB`] to preserve
//!   byte-for-byte split-point compatibility.

use alloc::vec;
use alloc::vec::Vec;

use core::hash::Hasher;
use twox_hash::XxHash64;

use super::opt::ldm::HcRawSeq;

pub(crate) mod gear_hash;
pub(crate) mod params;
pub(crate) mod search;
pub(crate) mod store;
pub(crate) mod table;

use gear_hash::{GearHashState, LDM_BATCH_SIZE};
use params::LdmParams;
use search::{FindBestMatchInputs, find_best_match};
use table::LdmHashTable;

/// Upstream zstd `XXH64` seed for the per-window LDM hash
/// (`zstd_ldm.c:315`: `XXH64(split, minMatchLength, 0)`).
const LDM_XXH64_SEED: u64 = 0;

/// How much of a long-distance match's body the strategy's own tables index:
/// the match is taken without being searched, and upstream catches its tables
/// up on no more than this much of it (`ZSTD_ldm_limitTableUpdate`,
/// `zstd_ldm.c:335`, brings `nextToUpdate` to within 512 of the match end).
pub(crate) const LONG_MATCH_INDEXED_TAIL: usize = 512;

/// Bytes a frame's long-distance matching holds under `params`: its table and
/// the sequence vectors around it. Each vector keeps at most a block's matches
/// at `min_match_length` apiece (upstream's `ZSTD_ldm_getMaxNbSeq`), and five
/// are retained: what a search found, the store, which keeps the matches a
/// block took until they are half of it, the block's own, and the optimal
/// parser's copy of those.
pub(crate) fn estimated_workspace_bytes(params: &LdmParams) -> usize {
    const RETAINED_SEQUENCE_VECTORS: usize = 5;
    let block = crate::common::MAX_BLOCK_SIZE as usize;
    let sequences = block / params.min_match_length.max(1) as usize + 1;
    let vectors = sequences * core::mem::size_of::<HcRawSeq>() * RETAINED_SEQUENCE_VECTORS;
    LdmHashTable::estimated_workspace_bytes(params.hash_log, params.bucket_size_log)
        .saturating_add(vectors)
}

/// LDM sequence producer — owns the rolling-hash state, bucket
/// table, and scratch buffers needed to scan an input block and
/// emit a stream of [`HcRawSeq`] candidates consumed by the
/// optimal parser.
///
/// Construction allocates the table (sized by [`LdmParams`]); the
/// per-call work is dominated by the hash walk and bucket lookups.
/// Designed to be re-used across blocks within a frame — call
/// [`Self::clear`] only when starting a new frame (so the
/// long-range history accumulated across blocks is preserved
/// within a frame, mirroring upstream zstd's `ldmState_t` lifecycle).
pub(crate) struct LdmProducer {
    /// Parameter set this producer was built with. Used by the
    /// split walker (next commit) to honour `min_match_length` /
    /// `hash_rate_log` / bucket sizing.
    params: LdmParams,
    /// Rolling-hash state. Re-initialised on [`Self::clear`].
    hash_state: GearHashState,
    /// Bucket table indexed by the high bits of the per-window
    /// XXH64. See [`table`] for layout details.
    hash_table: LdmHashTable,
    /// Scratch buffer for `gear_hash::feed` (`LDM_BATCH_SIZE`
    /// entries per upstream zstd pre-condition). Kept in the producer so
    /// hot calls don't re-allocate.
    splits_scratch: Vec<usize>,
}

impl LdmProducer {
    /// Build a fresh producer for the given parameter set.
    ///
    /// Allocates the bucket hash table (`1 << params.hash_log`
    /// entries) and seeds the rolling-hash state from
    /// `params.min_match_length` / `params.hash_rate_log`. The
    /// `splits_scratch` buffer is sized to [`LDM_BATCH_SIZE`] so
    /// every subsequent `gear_hash::feed` call sees a buffer
    /// satisfying the upstream zstd pre-condition without re-allocation.
    pub(crate) fn new(params: LdmParams) -> Self {
        let hash_state = GearHashState::new(params.min_match_length as usize, params.hash_rate_log);
        let hash_table = LdmHashTable::new(params.hash_log, params.bucket_size_log);
        Self {
            params,
            hash_state,
            hash_table,
            splits_scratch: vec![0usize; LDM_BATCH_SIZE],
        }
    }

    /// Heap bytes this producer owns: the LDM hash table and the split-scratch.
    pub(crate) fn heap_size(&self) -> usize {
        self.hash_table.heap_size() + self.splits_scratch.capacity() * core::mem::size_of::<usize>()
    }

    /// Reset bucket cursors, zero the hash entries, and re-seed
    /// the rolling-hash state. Use at frame boundaries.
    pub(crate) fn clear(&mut self) {
        self.hash_table.clear();
        self.hash_state = GearHashState::new(
            self.params.min_match_length as usize,
            self.params.hash_rate_log,
        );
    }

    /// Index `[start_abs, end_abs)` of `live_history` (whose first byte sits
    /// at `history_abs_start`) without searching it: a dictionary loaded under
    /// long-distance matching (upstream zstd `ZSTD_ldm_fillHashTable`,
    /// `zstd_ldm.c:289-327`). Leaves the rolling hash for the next
    /// [`Self::generate_into`], which re-seeds it anyway.
    pub(crate) fn fill_table(
        &mut self,
        live_history: &[u8],
        history_abs_start: usize,
        start_abs: usize,
        end_abs: usize,
    ) {
        debug_assert!(history_abs_start <= start_abs && start_abs <= end_abs);
        debug_assert!(end_abs <= history_abs_start + live_history.len());
        let min_match = self.params.min_match_length as usize;
        let hash_id_mask = self.hash_table.bucket_mask();
        let mut hash_state = GearHashState::new(min_match, self.params.hash_rate_log);
        let mut ip_abs = start_abs;
        while ip_abs < end_abs {
            let chunk = &live_history[ip_abs - history_abs_start..end_abs - history_abs_start];
            let (hashed, num_splits) =
                gear_hash::feed(&mut hash_state, chunk, &mut self.splits_scratch);
            for &split in &self.splits_scratch[..num_splits] {
                // A split closes a `min_match`-byte window; one that would open
                // before the range is not a window of it.
                if ip_abs + split < start_abs + min_match {
                    continue;
                }
                let split_abs = ip_abs + split - min_match;
                let split_idx = split_abs - history_abs_start;
                let mut hasher = XxHash64::with_seed(LDM_XXH64_SEED);
                hasher.write(&live_history[split_idx..split_idx + min_match]);
                let xxhash = hasher.finish();
                self.hash_table.ensure_room_for(split_abs);
                self.hash_table.insert_absolute(
                    (xxhash as u32) & hash_id_mask,
                    split_abs,
                    (xxhash >> 32) as u32,
                );
            }
            // `feed` consumes at least one byte of a non-empty chunk.
            ip_abs += hashed.max(1);
        }
    }

    /// Read-only view of the parameter set for diagnostics / tests.
    pub(crate) fn params(&self) -> LdmParams {
        self.params
    }

    /// Scan the absolute range `[block_start_abs, block_end_abs)`
    /// inside `live_history` against accumulated long-range
    /// candidates and append every accepted match into `out` as
    /// an [`HcRawSeq`].
    ///
    /// `live_history` is the per-frame *live* byte slice — `live_
    /// history[0]` is the byte at absolute stream position
    /// `history_abs_start`. Every cross-block invariant
    /// (`anchor`, `ip`, `entry.offset`) is maintained in
    /// **absolute stream coordinates** so the bucket table stays
    /// valid across window evictions: after a slide,
    /// `history_abs_start` advances and any entry inserted by an
    /// earlier window is filtered by the inclusive lower-bound
    /// staleness check `entry.offset < lowest_index_abs` in
    /// [`find_best_match`] (entries at exactly
    /// `lowest_index_abs == history_abs_start` survive).
    ///
    /// Implements the upstream zstd pipeline from
    /// `ZSTD_ldm_generateSequences_internal`
    /// (`zstd_ldm.c:346-515`) in three phases:
    ///
    /// 1. **Init** — re-seed the rolling hash and prime it with
    ///    the first `min_match_length` bytes of the block
    ///    (upstream zstd `zstd_ldm.c:381-383`). Upstream zstd resets the hash
    ///    state at every `generateSequences_internal` call; the
    ///    bucket table is preserved across blocks within a frame
    ///    so long-range candidates accumulated by earlier blocks
    ///    remain reachable.
    /// 2. **Feed batch** — call [`gear_hash::feed`] to fill the
    ///    `splits_scratch` buffer with up to [`LDM_BATCH_SIZE`]
    ///    split positions (upstream zstd `zstd_ldm.c:390-391`).
    /// 3. **Per-split verify + emit** — for every split, hash the
    ///    preceding `min_match_length`-byte window (upstream zstd seed
    ///    0, XXH64), look up the bucket, run [`find_best_match`]
    ///    to score every entry by forward + backward match
    ///    length, and either emit an [`HcRawSeq`] + insert the
    ///    new entry (match found) or insert-only (upstream zstd
    ///    `zstd_ldm.c:393-510`). When a match overlaps the
    ///    already-hashed range upstream zstd re-resets the rolling hash
    ///    and re-enters the outer loop from `anchor` (upstream zstd
    ///    `zstd_ldm.c:497-508` — the "all-zeros repetition speed
    ///    boost").
    ///
    /// The producer covers the **prefix-only** path; the
    /// two-segment `extDict` variant is documented and deferred
    /// in [`search`].
    pub(crate) fn generate_into<K: crate::encoding::fastpath::EncodeTier>(
        &mut self,
        live_history: &[u8],
        history_abs_start: usize,
        block_start_abs: usize,
        block_end_abs: usize,
        out: &mut Vec<HcRawSeq>,
    ) {
        debug_assert!(history_abs_start <= block_start_abs);
        debug_assert!(block_start_abs <= block_end_abs);
        debug_assert!(block_end_abs <= history_abs_start + live_history.len());

        let min_match = self.params.min_match_length as usize;
        // Upstream zstd `zstd_ldm.c:377-378`: nothing to do if the block
        // can't fit a single LDM window.
        if block_end_abs.saturating_sub(block_start_abs) < min_match {
            return;
        }

        // hBits = hashLog - bucketSizeLog (upstream zstd `zstd_ldm.c:354`).
        // Pull the mask from the table rather than re-deriving from
        // `params.hash_log / params.bucket_size_log`: the table
        // applies a `min(bucket_size_log, hash_log)` clamp
        // (`zstd_ldm.c:176`) which the producer-side recomputation
        // does not, so any caller that constructs the table with a
        // `bucket_size_log >= hash_log` would see drift between
        // `hash_id_mask` and the actual bucket count.
        let hash_id_mask: u32 = self.hash_table.bucket_mask();

        // abs→slice helper. The closure body folds to a single
        // sub instruction at the call site.
        let to_idx = |abs: usize| abs - history_abs_start;

        // Re-init + reset against the first min_match bytes —
        // upstream zstd `zstd_ldm.c:381-383`. The table itself is
        // preserved across blocks; only the rolling hash is
        // wound back.
        self.hash_state = GearHashState::new(min_match, self.params.hash_rate_log);
        let reset_start_idx = to_idx(block_start_abs);
        gear_hash::reset(
            &mut self.hash_state,
            &live_history[reset_start_idx..reset_start_idx + min_match],
        );

        // Anchor: leftmost byte the producer can still emit as
        // literal. Upstream zstd `BYTE const* anchor = istart;` — kept in
        // absolute coordinates so emitted seq positions stay
        // consistent across blocks within a frame.
        let mut anchor_abs = block_start_abs;
        // `ip` (current input cursor): we start AFTER the reset
        // window. Upstream zstd `ip += minMatchLength;`.
        let mut ip_abs = block_start_abs + min_match;
        // Upstream zstd caps the outer walk at `iend - HASH_READ_SIZE`
        // (`zstd_ldm.c:366`). HASH_READ_SIZE = 8 in upstream zstd.
        const HASH_READ_SIZE: usize = 8;
        let ilimit_abs = block_end_abs.saturating_sub(HASH_READ_SIZE);

        while ip_abs < ilimit_abs {
            let chunk_idx = to_idx(ip_abs);
            let chunk_end_idx = to_idx(ilimit_abs);
            let chunk = &live_history[chunk_idx..chunk_end_idx];
            let (hashed, num_splits) =
                gear_hash::feed(&mut self.hash_state, chunk, &mut self.splits_scratch);

            // Two-pass over the batch like upstream zstd. The first pass
            // prepares (hash, checksum) for every split (upstream zstd
            // does it for PREFETCH_L1; we leave the entries in
            // `splits_scratch` and recompute the per-split state
            // in pass two — Rust's optimiser folds the duplicated
            // work). Pass two is the verify + emit + insert loop.
            let mut split_n = 0usize;
            while split_n < num_splits {
                let s = self.splits_scratch[split_n];
                split_n += 1;
                // Upstream zstd `zstd_ldm.c:313`:
                //   if (ip + splits[n] >= istart + minMatchLength)
                // Since `ip_abs - block_start_abs >= min_match`
                // after the reset, the window-start subtraction
                // never underflows. Belt-and-braces defensive
                // guard:
                if s + ip_abs < block_start_abs + min_match {
                    continue;
                }
                let split_abs = ip_abs + s - min_match;
                let window_end_abs = split_abs + min_match;
                if window_end_abs > block_end_abs {
                    continue;
                }

                let split_idx = to_idx(split_abs);
                let window_end_idx = to_idx(window_end_abs);
                let mut hasher = XxHash64::with_seed(LDM_XXH64_SEED);
                hasher.write(&live_history[split_idx..window_end_idx]);
                let xxhash = hasher.finish();
                let hash_id = (xxhash as u32) & hash_id_mask;
                let checksum = (xxhash >> 32) as u32;

                // The table stores positions as `u32` offsets
                // relative to its internal `position_base` (upstream zstd
                // rebase scheme — see `table.rs`); ensure room
                // before every insert so streams beyond
                // `u32::MAX` rebase transparently.
                self.hash_table.ensure_room_for(split_abs);

                // Upstream zstd `zstd_ldm.c:420-426`: if this split would
                // emit a sequence overlapping the previous one,
                // just record it and move on.
                if split_abs < anchor_abs {
                    self.hash_table
                        .insert_absolute(hash_id, split_abs, checksum);
                    continue;
                }

                // Search bucket for the best forward+backward
                // match. `lowest_index_abs = history_abs_start`
                // — entries inserted before the current window
                // (i.e. by a previous, now-evicted frame view)
                // are stale and filtered. Within a single frame
                // `history_abs_start` stays constant so all
                // entries inserted by this call are kept; the
                // mechanism becomes load-bearing only when the
                // caller drives multiple compress_block windows
                // through the same producer.
                let best = find_best_match::<K>(
                    &self.hash_table,
                    hash_id,
                    checksum,
                    FindBestMatchInputs {
                        live_history,
                        history_abs_start,
                        split_abs,
                        anchor_abs,
                        lowest_index_abs: history_abs_start,
                        // Cap the forward search at the current
                        // block's end — upstream zstd `iend`. Without
                        // this bound a match could extend past
                        // `block_end_abs` into bytes the
                        // producer hasn't scanned yet, breaking
                        // the `anchor <= split + forward <=
                        // block_end_abs` invariant downstream.
                        iend_abs: block_end_abs,
                        min_match_length: min_match,
                    },
                );

                let Some(best) = best else {
                    // Upstream zstd `zstd_ldm.c:468-473`: no match → just
                    // insert the new entry and continue.
                    self.hash_table
                        .insert_absolute(hash_id, split_abs, checksum);
                    continue;
                };

                // Match found. Upstream zstd `zstd_ldm.c:475-488`.
                // `best.match_pos` is absolute; `split_abs` is
                // absolute; their difference is a true
                // back-reference distance immune to window
                // sliding and to the table's internal `u32`
                // rebase shifts.
                let offset = split_abs - best.match_pos;
                let lit_length = split_abs - best.backward_len - anchor_abs;
                let match_length = best.total_len();
                out.push(HcRawSeq {
                    lit_length,
                    offset,
                    match_length,
                });

                // Insert AFTER finding the match so the lookup
                // doesn't clobber `bestEntry` (upstream zstd `zstd_ldm.c:
                // 490-492`).
                self.hash_table
                    .insert_absolute(hash_id, split_abs, checksum);

                // Advance anchor past the matched bytes (upstream zstd
                // `zstd_ldm.c:494`).
                anchor_abs = split_abs + best.forward_len;

                // Upstream zstd `zstd_ldm.c:496-508`: when the emitted
                // match extends past the already-hashed window,
                // skip ahead by re-resetting the rolling hash on
                // the bytes preceding the new anchor.
                if anchor_abs > ip_abs + hashed {
                    let reset_start_abs = anchor_abs.saturating_sub(min_match);
                    if reset_start_abs + min_match <= block_end_abs
                        && reset_start_abs >= history_abs_start
                    {
                        let reset_idx = to_idx(reset_start_abs);
                        self.hash_state = GearHashState::new(min_match, self.params.hash_rate_log);
                        gear_hash::reset(
                            &mut self.hash_state,
                            &live_history[reset_idx..reset_idx + min_match],
                        );
                        // Continue the outer `while (ip < ilimit)`
                        // loop at `anchor`. Upstream zstd: `ip = anchor -
                        // hashed;` so the upcoming `ip += hashed`
                        // lands exactly at `anchor`.
                        ip_abs = anchor_abs.saturating_sub(hashed).max(history_abs_start);
                    }
                    break;
                }
            }

            // Guarantee forward progress even when `hashed == 0`:
            // `gear_feed` always advances by at least one byte
            // over a non-empty chunk, but `hashed.max(1)` keeps
            // the outer `while` loop monotonic against any
            // future regression of that invariant.
            //
            // Raw `+` is safe — every callsite reaches LDM via
            // `bt::prepare_ldm_candidates`, whose
            // `current_abs_start` / `block_end_abs` are derived
            // from a `MatchTable` that has already passed
            // `check_stream_abs_headroom`
            // (`match_table/storage.rs:50`). That frame-level
            // guard guarantees `history_abs_start + window_size +
            // STREAM_ABS_HEADROOM ≤ usize::MAX`, a much stronger
            // bound than this single-byte add needs. Within the
            // `while ip_abs < ilimit_abs` loop body the
            // post-update value also stays ≤ `ilimit_abs ≤
            // history_abs_start + live_history.len()`, so neither
            // operand can overflow `usize`.
            ip_abs += hashed.max(1);
        }

        // Upstream zstd returns `iend - anchor` (the "leftover" tail),
        // which our caller doesn't currently need; the optimal
        // parser drains `out` based on the sequences alone.
    }
}

#[cfg(test)]
mod tests;
