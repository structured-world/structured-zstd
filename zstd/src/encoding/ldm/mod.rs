//! Long-distance-match (LDM) producer.
//!
//! Implements the donor's `lib/compress/zstd_ldm.c` pipeline:
//!
//! 1. [`gear_hash`] — gear rolling hash over a 256-entry random
//!    permutation table picks content-defined split points
//!    (`(hash & stopMask) == 0`).
//! 2. [`table`] — bucket-based hash table indexed by the gear-hash
//!    checksum, holding `1 << bucket_size_log` candidate positions
//!    per bucket.
//! 3. *(planned)* `search` — verify + extend a candidate forward
//!    and backward across the LDM window.
//! 4. [`params`] — [`LdmParams`] derived from `windowLog` /
//!    `strategy` (`ZSTD_ldm_adjustParameters` parity).
//!
//! Aggregate [`LdmProducer`] holds the rolling-hash state, the
//! bucket table, and the per-call scratch buffers. The downstream
//! consumer (`bt::ldm_sequences`) was plumbed during Phase 1 (#119)
//! — Phase 5 swaps the `prepare_ldm_candidates` no-op stub for a
//! real producer that fills that buffer.
//!
//! Current state (Phase 5 foundations commit):
//! * `LdmProducer::new` allocates the table + initialises the
//!   rolling-hash state from caller params;
//! * `LdmProducer::clear` resets the bucket cursors + the rolling
//!   hash so a fresh frame starts clean;
//! * `LdmProducer::generate_into` is the planned entry point that
//!   walks the input, finds splits, inserts new candidates, and
//!   emits verified matches as [`HcRawSeq`] entries. The verify +
//!   extend logic lands in the next commit of #111 Phase 5; this
//!   commit ships the structural skeleton and parameter plumbing.
//!
//! Donor parity anchors:
//! * `lib/compress/zstd_ldm.c` v1.5.7
//! * `lib/compress/zstd_ldm.h`
//! * `lib/compress/zstd_ldm_geartab.h` — the 256 × `u64` permutation
//!   table reproduced verbatim in [`gear_hash::GEAR_TAB`] to preserve
//!   byte-for-byte split-point compatibility.

// Phase 5 of #111 lands in two PR-sized chunks. This first chunk
// ships the gear-hash primitive, parameter derivation, bucket
// table, and the `LdmProducer` fill path (gear walk + XXH64 +
// bucket insert) — all donor-cited with full unit coverage. The
// verify + extend pass that drains the bucket into `HcRawSeq`
// entries and the activation gate (`window_log >= 27` à la donor
// `ZSTD_window_size > maxDistance`) land in the second chunk; in
// the meantime several `pub(crate)` items (the `bucket` lookup,
// the `bucket_mask` accessor, the `LDM_HASHLOG_MIN/MAX` bounds,
// the `params::bounded` helper) are reachable only through tests.
// `bt/mod.rs` carries the same `#![allow(dead_code)]` marker for
// the same Phase-1 transitional reason.
#![allow(dead_code)]

use alloc::vec;
use alloc::vec::Vec;

use core::hash::Hasher;
use twox_hash::XxHash64;

use super::opt::ldm::HcRawSeq;

pub(crate) mod gear_hash;
pub(crate) mod params;
pub(crate) mod table;

use gear_hash::{GearHashState, LDM_BATCH_SIZE};
use params::LdmParams;
use table::{LdmEntry, LdmHashTable};

/// Donor `XXH64` seed for the per-window LDM hash
/// (`zstd_ldm.c:315`: `XXH64(split, minMatchLength, 0)`).
const LDM_XXH64_SEED: u64 = 0;

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
/// within a frame, mirroring donor's `ldmState_t` lifecycle).
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
    /// entries per donor pre-condition). Kept in the producer so
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
    /// satisfying the donor pre-condition without re-allocation.
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

    /// Re-derive parameters from a `(window_log, strategy)` pair
    /// using [`LdmParams::adjust_for`]. Convenience wrapper.
    pub(crate) fn with_window_and_strategy(window_log: u32, strategy: u32) -> Self {
        Self::new(LdmParams::adjust_for(window_log, strategy))
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

    /// Read-only view of the parameter set for diagnostics / tests.
    pub(crate) fn params(&self) -> LdmParams {
        self.params
    }

    /// Scan `history[block_start..block_end]` against accumulated
    /// long-range candidates and append every accepted match into
    /// `out` as an [`HcRawSeq`].
    ///
    /// `history` is the full per-frame byte slice (so back
    /// references can be resolved up to `block_start`).
    /// `block_start` / `block_end` mark the bounds of the input
    /// chunk this call is responsible for; the producer never
    /// emits sequences whose match references bytes at or after
    /// `block_end`.
    ///
    /// **Current implementation status (Phase 5 foundations):**
    /// The fill / insert half of the donor pipeline is live in
    /// this commit:
    ///
    /// 1. walk `history[block_start..block_end]` through the gear
    ///    rolling hash;
    /// 2. for every split point, hash the preceding
    ///    `min_match_length`-byte window with XXH64 (donor
    ///    `zstd_ldm.c:315`) and insert the `(offset, high32)`
    ///    pair into the bucket table.
    ///
    /// The verify + extend step that reads back the bucket and
    /// emits [`HcRawSeq`] entries lands in the next Phase 5
    /// commit. For now `out` is left untouched — call behaviour
    /// matches the previous no-op stub, but the bucket table is
    /// populated so the follow-up emit pass already has long-range
    /// candidates to read.
    pub(crate) fn generate_into(
        &mut self,
        history: &[u8],
        block_start: usize,
        block_end: usize,
        _out: &mut Vec<HcRawSeq>,
    ) {
        debug_assert!(block_start <= block_end);
        debug_assert!(block_end <= history.len());
        if block_end <= block_start {
            return;
        }
        let min_match = self.params.min_match_length as usize;
        // hBits: donor `zstd_ldm.c:295`
        // (`hBits = params.hashLog - bucketSizeLog`). Pre-compute
        // so the hot loop is a mask-and-shift rather than a per-
        // iteration subtract.
        let h_bits = self
            .params
            .hash_log
            .saturating_sub(self.params.bucket_size_log);
        let hash_id_mask: u32 = if h_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << h_bits).wrapping_sub(1)
        };

        let mut cursor = block_start;
        while cursor < block_end {
            let chunk = &history[cursor..block_end];
            let (consumed, num_splits) =
                gear_hash::feed(&mut self.hash_state, chunk, &mut self.splits_scratch);

            // Insert pass — donor `ZSTD_ldm_fillHashTable`
            // (`zstd_ldm.c:289-325`). Each split index `s` is the
            // post-byte offset INSIDE `chunk`; the matched window
            // starts at `chunk_abs + s - min_match_length`. If
            // `s < min_match_length` we have no window yet (donor
            // `if (ip + splits[n] >= istart + minMatchLength)`).
            for &s in &self.splits_scratch[..num_splits] {
                if s < min_match {
                    continue;
                }
                let window_start = cursor + s - min_match;
                let window_end = window_start + min_match;
                if window_end > block_end {
                    // Defensive — donor's loop walks `ip..iend` so
                    // `window_end <= iend` is guaranteed; the cap
                    // here is paranoid against caller bugs.
                    continue;
                }
                let mut hasher = XxHash64::with_seed(LDM_XXH64_SEED);
                hasher.write(&history[window_start..window_end]);
                let xxhash = hasher.finish();
                let hash_id = (xxhash as u32) & hash_id_mask;
                let checksum = (xxhash >> 32) as u32;
                self.hash_table.insert(
                    hash_id,
                    LdmEntry {
                        offset: window_start as u32,
                        checksum,
                    },
                );
            }

            // Donor's outer `while (ip < iend)` advances by the
            // number of bytes the gear hash consumed; if the batch
            // filled up before reaching the end of the chunk the
            // loop re-enters with the new cursor. `max(1)` defends
            // against a pathological `consumed == 0` (impossible by
            // construction — `gear_feed` always advances by at
            // least one byte if `chunk` is non-empty — but keeps
            // the loop monotonic).
            cursor += consumed.max(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LdmProducer::new` must allocate without panic for a
    /// representative parameter set (level 22 / btultra2 / window
    /// 27 — the level the project's ratio gate targets).
    #[test]
    fn producer_constructs_with_donor_default_params() {
        let producer = LdmProducer::with_window_and_strategy(27, 9);
        let p = producer.params();
        // Donor defaults at btultra2: minMatch halved, hash_rate_log
        // = 4, bucket_size_log clamps to 8. See params::tests for
        // the per-knob derivations.
        assert_eq!(p.window_log, 27);
        assert_eq!(p.min_match_length, 32);
        assert_eq!(p.hash_rate_log, 4);
        assert_eq!(p.bucket_size_log, 8);
    }

    /// `clear` after `generate_into` rewinds the rolling hash to
    /// the canonical init value — guards the frame-boundary
    /// contract.
    #[test]
    fn clear_resets_rolling_hash_state() {
        let mut producer = LdmProducer::with_window_and_strategy(27, 3);
        let mut out = Vec::new();
        // Feed a non-empty chunk so the rolling hash advances.
        let data = [0xAAu8; 256];
        producer.generate_into(&data, 0, data.len(), &mut out);
        let advanced = producer.hash_state.rolling;
        assert_ne!(
            advanced,
            gear_hash::GEAR_HASH_INIT,
            "rolling hash should have moved after generate_into"
        );
        producer.clear();
        assert_eq!(
            producer.hash_state.rolling,
            gear_hash::GEAR_HASH_INIT,
            "clear must rewind to GEAR_HASH_INIT"
        );
    }

    /// `generate_into` with an empty range is a no-op — emits
    /// nothing and leaves the rolling hash untouched. Guards
    /// against an off-by-one in the bounds check.
    #[test]
    fn generate_into_empty_range_is_noop() {
        let mut producer = LdmProducer::with_window_and_strategy(27, 3);
        let mut out = Vec::new();
        let data = [0u8; 128];
        let pre = producer.hash_state.rolling;
        producer.generate_into(&data, 64, 64, &mut out);
        assert!(out.is_empty());
        assert_eq!(producer.hash_state.rolling, pre);
    }
}
