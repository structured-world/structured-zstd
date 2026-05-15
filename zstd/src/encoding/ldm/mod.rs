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
//! 3. [`search`] — verify + extend a candidate forward and
//!    backward across the LDM window (prefix-only path; the
//!    two-segment `extDict` variant is deferred).
//! 4. [`params`] — [`LdmParams`] derived from `windowLog` /
//!    `strategy` (`ZSTD_ldm_adjustParameters` parity).
//!
//! Aggregate [`LdmProducer`] holds the rolling-hash state, the
//! bucket table, and the per-call scratch buffers. The downstream
//! consumer (`bt::ldm_sequences`) was plumbed during Phase 1
//! (#119) — Phase 5 swaps the `prepare_ldm_candidates` no-op stub
//! for a real producer that fills that buffer.
//!
//! # Activation policy (distro-grade parity)
//!
//! `LdmProducer` is **never activated automatically by a
//! [`super::CompressionLevel`] preset.** This mirrors upstream
//! `libzstd.so.1` behaviour: `ZSTD_compress(..., level)` keeps
//! LDM off at every standard level (1..22). Activation is an
//! explicit user opt-in:
//!
//! * upstream — `ZSTD_CCtx_setParameter(cctx,
//!   ZSTD_c_enableLongDistanceMatching, ZSTD_ps_enable)`;
//! * `zstd` CLI — `zstd --long[=N]`;
//! * Rust port (this crate) — the Phase-5 surface ships an
//!   `ldm_producer: Option<LdmProducer>` field on `BtMatcher` that
//!   stays `None` by default. The forthcoming parameter-API issue
//!   (#27) plugs into that field; the C ABI work in #126 / #127
//!   wires `ZSTD_c_enableLdm` through the same surface.
//!
//! Therefore the `level22_sequences_match_donor_on_corpus_proxy`
//! ratio gate continues to compare two LDM-OFF outputs (ours vs
//! upstream `ZSTD_compress(..., 22)`) — byte-parity invariant.
//!
//! Donor parity anchors:
//! * `lib/compress/zstd_ldm.c` v1.5.7
//! * `lib/compress/zstd_ldm.h`
//! * `lib/compress/zstd_ldm_geartab.h` — the 256 × `u64` permutation
//!   table reproduced verbatim in [`gear_hash::GEAR_TAB`] to preserve
//!   byte-for-byte split-point compatibility.

// Phase 5 of #111 ships the full LDM producer (gear hash + params
// + bucket table + verify/extend search + aggregate driver) in
// two commits. By distro-parity design (see the module docs
// above), no `CompressionLevel` preset activates the producer —
// `BtMatcher::ldm_producer` stays `None` until #27 (Rust
// parameter API) or #126/#127 (C ABI) plug the user opt-in.
// Several `pub(crate)` accessors (`LDM_HASHLOG_MIN/MAX`,
// `params::bounded`, `bucket_mask`) are therefore reachable only
// via the unit test suite until the parameter API lands; the
// `#![allow(dead_code)]` matches the same transitional marker in
// `bt/mod.rs` introduced during Phase 1.
#![allow(dead_code)]

use alloc::vec;
use alloc::vec::Vec;

use core::hash::Hasher;
use twox_hash::XxHash64;

use super::opt::ldm::HcRawSeq;

pub(crate) mod gear_hash;
pub(crate) mod params;
pub(crate) mod search;
pub(crate) mod table;

use gear_hash::{GearHashState, LDM_BATCH_SIZE};
use params::LdmParams;
use search::{FindBestMatchInputs, find_best_match};
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
    /// `history` is the full per-frame byte slice — back
    /// references reach all the way back to byte 0 (the
    /// prefix-only path described in [`search`]). `block_start`
    /// / `block_end` mark the bounds of the input chunk this
    /// call is responsible for; the producer never emits a
    /// sequence whose `split + forward` reaches past `block_end`.
    ///
    /// Implements the donor pipeline from
    /// `ZSTD_ldm_generateSequences_internal`
    /// (`zstd_ldm.c:346-515`) in three phases:
    ///
    /// 1. **Init** — re-seed the rolling hash and prime it with
    ///    the first `min_match_length` bytes of the block
    ///    (donor `zstd_ldm.c:381-383`). Donor resets the hash
    ///    state at every `generateSequences_internal` call; the
    ///    bucket table is preserved across blocks within a frame
    ///    so long-range candidates accumulated by earlier blocks
    ///    remain reachable.
    /// 2. **Feed batch** — call [`gear_hash::feed`] to fill the
    ///    `splits_scratch` buffer with up to [`LDM_BATCH_SIZE`]
    ///    split positions (donor `zstd_ldm.c:390-391`).
    /// 3. **Per-split verify + emit** — for every split, hash the
    ///    preceding `min_match_length`-byte window (donor seed
    ///    0, XXH64), look up the bucket, run [`find_best_match`]
    ///    to score every entry by forward + backward match
    ///    length, and either emit an [`HcRawSeq`] + insert the
    ///    new entry (match found) or insert-only (donor
    ///    `zstd_ldm.c:393-510`). When a match overlaps the
    ///    already-hashed range donor re-resets the rolling hash
    ///    and re-enters the outer loop from `anchor` (donor
    ///    `zstd_ldm.c:497-508` — the "all-zeros repetition speed
    ///    boost").
    ///
    /// The producer covers the **prefix-only** path; the
    /// two-segment `extDict` variant is documented and deferred
    /// in [`search`].
    pub(crate) fn generate_into(
        &mut self,
        history: &[u8],
        block_start: usize,
        block_end: usize,
        out: &mut Vec<HcRawSeq>,
    ) {
        debug_assert!(block_start <= block_end);
        debug_assert!(block_end <= history.len());

        let min_match = self.params.min_match_length as usize;
        // Donor `zstd_ldm.c:377-378`: nothing to do if the block
        // can't fit a single LDM window.
        if block_end.saturating_sub(block_start) < min_match {
            return;
        }

        // hBits = hashLog - bucketSizeLog. Donor `zstd_ldm.c:354`.
        let h_bits = self
            .params
            .hash_log
            .saturating_sub(self.params.bucket_size_log);
        let hash_id_mask: u32 = if h_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << h_bits).wrapping_sub(1)
        };

        // Re-init + reset against the first min_match bytes —
        // donor `zstd_ldm.c:381-383`. The table itself is
        // preserved across blocks; only the rolling hash is
        // wound back.
        self.hash_state = GearHashState::new(min_match, self.params.hash_rate_log);
        gear_hash::reset(
            &mut self.hash_state,
            &history[block_start..block_start + min_match],
        );

        // Anchor: leftmost byte the producer can still emit as
        // literal. Donor `BYTE const* anchor = istart;`.
        let mut anchor = block_start;
        // `ip` (current input cursor): we start AFTER the reset
        // window. Donor `ip += minMatchLength;`.
        let mut ip = block_start + min_match;
        // Donor caps the outer walk at `iend - HASH_READ_SIZE`
        // (`zstd_ldm.c:366`). HASH_READ_SIZE = 8 in donor.
        const HASH_READ_SIZE: usize = 8;
        let ilimit = block_end.saturating_sub(HASH_READ_SIZE);

        while ip < ilimit {
            let chunk = &history[ip..ilimit];
            let (hashed, num_splits) =
                gear_hash::feed(&mut self.hash_state, chunk, &mut self.splits_scratch);

            // Two-pass over the batch like donor. The first pass
            // prepares (hash, checksum) for every split (donor
            // does it for PREFETCH_L1; we leave the entries in
            // `splits_scratch` and recompute the per-split state
            // in pass two — Rust's optimiser folds the duplicated
            // work). Pass two is the verify + emit + insert loop.
            let mut split_idx = 0usize;
            while split_idx < num_splits {
                let s = self.splits_scratch[split_idx];
                split_idx += 1;
                // Donor `zstd_ldm.c:313`:
                //   if (ip + splits[n] >= istart + minMatchLength)
                // Since `ip - block_start >= min_match` after the
                // reset, the window-start subtraction never
                // underflows. Belt-and-braces defensive guard:
                if s + ip < block_start + min_match {
                    continue;
                }
                let split_abs = ip + s - min_match;
                let window_end = split_abs + min_match;
                if window_end > block_end {
                    continue;
                }

                let mut hasher = XxHash64::with_seed(LDM_XXH64_SEED);
                hasher.write(&history[split_abs..window_end]);
                let xxhash = hasher.finish();
                let hash_id = (xxhash as u32) & hash_id_mask;
                let checksum = (xxhash >> 32) as u32;

                let new_entry = LdmEntry {
                    offset: split_abs as u32,
                    checksum,
                };

                // Donor `zstd_ldm.c:420-426`: if this split would
                // emit a sequence overlapping the previous one,
                // just record it and move on.
                if split_abs < anchor {
                    self.hash_table.insert(hash_id, new_entry);
                    continue;
                }

                // Search bucket for the best forward+backward
                // match. `lowest_index_abs = 0` matches our
                // prefix-only invariant (every entry with
                // `offset > 0` is in-window).
                let best = find_best_match(
                    &self.hash_table,
                    hash_id,
                    checksum,
                    FindBestMatchInputs {
                        history,
                        split_abs,
                        anchor_abs: anchor,
                        lowest_index_abs: 0,
                        min_match_length: min_match,
                    },
                );

                let Some(best) = best else {
                    // Donor `zstd_ldm.c:468-473`: no match → just
                    // insert the new entry and continue.
                    self.hash_table.insert(hash_id, new_entry);
                    continue;
                };

                // Match found. Donor `zstd_ldm.c:475-488`.
                let offset = (split_abs as u32) - best.match_pos;
                let lit_length = split_abs - best.backward_len - anchor;
                let match_length = best.total_len();
                out.push(HcRawSeq {
                    lit_length,
                    offset: offset as usize,
                    match_length,
                });

                // Insert AFTER finding the match so the lookup
                // doesn't clobber `bestEntry` (donor `zstd_ldm.c:
                // 490-492`).
                self.hash_table.insert(hash_id, new_entry);

                // Advance anchor past the matched bytes (donor
                // `zstd_ldm.c:494`).
                anchor = split_abs + best.forward_len;

                // Donor `zstd_ldm.c:496-508`: when the emitted
                // match extends past the already-hashed window,
                // skip ahead by re-resetting the rolling hash on
                // the bytes preceding the new anchor.
                if anchor > ip + hashed {
                    let reset_start = anchor.saturating_sub(min_match);
                    if reset_start + min_match <= block_end {
                        self.hash_state = GearHashState::new(min_match, self.params.hash_rate_log);
                        gear_hash::reset(
                            &mut self.hash_state,
                            &history[reset_start..reset_start + min_match],
                        );
                        // Continue the outer `while (ip < ilimit)`
                        // loop at `anchor`. Donor: `ip = anchor -
                        // hashed;` so the upcoming `ip += hashed`
                        // lands exactly at `anchor`.
                        ip = anchor.saturating_sub(hashed);
                    }
                    break;
                }
            }

            ip = ip.saturating_add(hashed).max(ip + 1);
        }

        // Donor returns `iend - anchor` (the "leftover" tail),
        // which our caller doesn't currently need; the optimal
        // parser drains `out` based on the sequences alone.
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

    /// End-to-end producer pipeline: a long-range repetition
    /// (two copies of a 4 KiB random-like payload separated by
    /// 64 KiB of unique filler) must emit at least one
    /// [`HcRawSeq`] whose `offset` equals the distance between
    /// the copies and whose `match_length` reaches at least the
    /// donor `min_match_length` floor. Validates that
    /// gear-hash → bucket-insert → checksum-filter → forward /
    /// backward extend → emit all hold together.
    #[test]
    fn generate_into_emits_long_range_match_on_repeated_payload() {
        // Use level 22 / btultra2 / windowLog 27 to get the
        // halved min_match (32) — the default 64 would require a
        // 128 KiB+ fixture to comfortably trigger a split inside
        // the payload, which slows the test without adding
        // coverage.
        let mut producer = LdmProducer::with_window_and_strategy(27, 9);
        let p = producer.params();
        assert_eq!(p.min_match_length, 32);

        // Build a fixture: payload, gap, payload, sized so the
        // second payload sits comfortably past the gear hash's
        // `2 ^ hash_rate_log` ≈ 16-byte expected split spacing.
        // 4 KiB payload + 64 KiB gap + 4 KiB payload ⇒ ~72 KiB
        // total, plenty of bytes for multiple split points inside
        // each copy.
        const PAYLOAD: usize = 4096;
        const GAP: usize = 64 * 1024;
        let mut history = Vec::with_capacity(2 * PAYLOAD + GAP);
        // Deterministic non-trivial payload: a simple LCG so the
        // bytes look random to the gear hash but the fixture
        // remains reproducible across runs.
        let mut prng: u32 = 0x1234_5678;
        let payload: alloc::vec::Vec<u8> = (0..PAYLOAD)
            .map(|_| {
                prng = prng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (prng >> 16) as u8
            })
            .collect();
        history.extend_from_slice(&payload);
        // Unique-byte gap: counter mod 251 keeps the gap
        // statistically distinct from the payload (251 prime so
        // it doesn't accidentally align with payload bytes).
        history.extend((0..GAP).map(|i| (i % 251) as u8));
        history.extend_from_slice(&payload);

        let mut out = Vec::new();
        producer.generate_into(&history, 0, history.len(), &mut out);

        assert!(
            !out.is_empty(),
            "long-range repetition must produce at least one LDM sequence"
        );
        // Every emitted sequence must satisfy the donor floor.
        for seq in &out {
            assert!(
                seq.match_length >= p.min_match_length as usize,
                "every emitted match must reach min_match_length \
                 (got match_length = {})",
                seq.match_length
            );
        }
        // At least one emitted sequence must reach across the
        // gap into the first payload copy — this is the
        // long-range match LDM exists to capture. Short-distance
        // matches found inside the first payload (statistically
        // possible on random-like content) are allowed too, but
        // the long-range one must show up.
        let crossing_gap = out.iter().any(|s| s.offset >= GAP);
        assert!(
            crossing_gap,
            "at least one emitted sequence must have offset >= GAP \
             ({GAP}); offsets observed: {:?}",
            out.iter().map(|s| s.offset).collect::<alloc::vec::Vec<_>>()
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
