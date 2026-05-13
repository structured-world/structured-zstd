//! Binary-tree match finder used by `BtOpt` / `BtUltra` / `BtUltra2`.
//!
//! Hosts the BT-side per-frame state: the donor `optStatePtr_t` cost
//! model (`opt_state`), the optimal-parser scratch buffers
//! (`opt_*_scratch` / `opt_*_generation` / `opt_*_stamp`), and the
//! LDM long-distance match buffer (`ldm_sequences`). Method bodies
//! (BT walk, `bt_insert_step_no_rebase`, `bt_update_tree_until`,
//! `build_optimal_plan*`, `collect_optimal_candidates*`,
//! `emit_optimal_plan`, …) still live on `HcMatchGenerator` and will
//! move onto `impl BtMatcher` once Stage 3b threads
//! `&mut MatchTable` through them — this stage establishes the
//! ownership boundary mirror to Stage 1 (`MatchTable`) and Stage 2
//! (`HcMatcher`).
//!
//! Donor parity reference: `lib/compress/zstd_opt.c`,
//! `ZSTD_compressBlock_opt_generic` and friends.

#![allow(dead_code)]

use alloc::vec::Vec;

use super::cost_model::{HC_MAX_LIT, HcOptState, HcOptimalCostProfile};
use super::opt::ldm::{HcOptLdmState, HcRawSeq, HcRawSeqStore};
use super::opt::types::{HcOptimalNode, HcOptimalPlanBuffers, HcOptimalSequence, MatchCandidate};

/// Maximum offset reachable by the HC3 short-match probe. Donor
/// parity: keeps the 3-byte side table from emitting offsets that
/// the main BT/HC paths would address more efficiently. Used inside
/// the `hash3_candidate_body!` macro that the kernel-specific
/// variants below expand.
pub(crate) const HC3_MAX_OFFSET: usize = 1 << 18;

/// Binary-tree matcher state used by the `BtOpt` / `BtUltra` /
/// `BtUltra2` parse modes. Owns the cost model and the per-frame
/// scratch arenas; the actual BT pointer-pair table lives on the
/// shared [`super::match_table::storage::MatchTable`].
pub(crate) struct BtMatcher {
    /// Donor `optStatePtr_t` — Huffman / FSE-derived literal and
    /// sequence-symbol cost tables that drive the optimal parser.
    pub(crate) opt_state: HcOptState,
    /// Per-frame scratch for the optimal-parse node stream.
    pub(crate) opt_nodes_scratch: Vec<HcOptimalNode>,
    /// Per-frame scratch for collected match candidates.
    pub(crate) opt_candidates_scratch: Vec<MatchCandidate>,
    /// Per-frame scratch for the final emitted node stream.
    pub(crate) opt_store_scratch: Vec<HcOptimalNode>,
    /// Per-segment plan buffer (parse → encode hand-off).
    pub(crate) opt_segment_plan_scratch: Vec<HcOptimalSequence>,
    /// `btultra2` seed-pass plan buffer.
    pub(crate) opt_seed_plan_scratch: Vec<HcOptimalSequence>,
    /// Cached literal-length cost lookup; `generation` is a stale-tag
    /// vector and `stamp` is the current frame's generation counter.
    pub(crate) opt_ll_price_scratch: Vec<u32>,
    pub(crate) opt_ll_price_generation: Vec<u32>,
    pub(crate) opt_ll_price_stamp: u32,
    /// Cached literal-symbol cost lookup (per-symbol fixed array).
    pub(crate) opt_lit_price_scratch: [u32; HC_MAX_LIT + 1],
    pub(crate) opt_lit_price_generation: [u32; HC_MAX_LIT + 1],
    pub(crate) opt_lit_price_stamp: u32,
    /// Cached match-length cost lookup.
    pub(crate) opt_ml_price_scratch: Vec<u32>,
    pub(crate) opt_ml_price_generation: Vec<u32>,
    pub(crate) opt_ml_price_stamp: u32,
    /// Long-distance match (LDM) candidates seeded into the optimal
    /// parser. Built per-block during `start_matching_optimal` and
    /// drained as the parser advances.
    pub(crate) ldm_sequences: Vec<HcRawSeq>,
}

impl BtMatcher {
    /// BT/HC hash MLS (minimum-length-segment) parameter. Donor
    /// parity: even when `minMatch == 3` (btultra2), the main BT/HC
    /// hash still goes through `ZSTD_hashPtr(…, mls)` which falls
    /// back to the default `case 4` in
    /// `zstd_compress_internal.h`. The 3-byte path is a separate HC3
    /// side table only.
    pub(crate) const HASH_MLS: usize = 4;

    /// Append `candidate` to `out` if it's strictly longer than the
    /// best length seen so far (and at least `min_match_len`). Maintains
    /// `best_len_for_skip` so subsequent calls only keep strictly
    /// improving candidates. Pure associated function — no BtMatcher
    /// state needed, just the candidate ladder bookkeeping.
    pub(crate) fn push_candidate_ladder(
        out: &mut Vec<MatchCandidate>,
        best_len_for_skip: &mut usize,
        candidate: MatchCandidate,
        min_match_len: usize,
    ) -> bool {
        if candidate.match_len < min_match_len {
            return false;
        }
        if candidate.match_len > *best_len_for_skip {
            out.push(candidate);
            *best_len_for_skip = candidate.match_len;
            return true;
        }
        false
    }

    pub(crate) fn new() -> Self {
        Self {
            opt_state: HcOptState::new(),
            opt_nodes_scratch: Vec::new(),
            opt_candidates_scratch: Vec::new(),
            opt_store_scratch: Vec::new(),
            opt_segment_plan_scratch: Vec::new(),
            opt_seed_plan_scratch: Vec::new(),
            opt_ll_price_scratch: Vec::new(),
            opt_ll_price_generation: Vec::new(),
            opt_ll_price_stamp: 0,
            opt_lit_price_scratch: [0; HC_MAX_LIT + 1],
            opt_lit_price_generation: [0; HC_MAX_LIT + 1],
            opt_lit_price_stamp: 0,
            opt_ml_price_scratch: Vec::new(),
            opt_ml_price_generation: Vec::new(),
            opt_ml_price_stamp: 0,
            ldm_sequences: Vec::new(),
        }
    }

    /// Per-frame reset — clears scratch buffers, resets cost model,
    /// drops cached price stamps.
    pub(crate) fn reset(&mut self) {
        self.opt_state.reset();
        self.opt_nodes_scratch.clear();
        self.opt_candidates_scratch.clear();
        self.opt_store_scratch.clear();
        self.opt_segment_plan_scratch.clear();
        self.opt_seed_plan_scratch.clear();
        self.opt_ll_price_scratch.clear();
        self.opt_ll_price_generation.clear();
        self.opt_ll_price_stamp = 0;
        self.opt_lit_price_scratch = [0; HC_MAX_LIT + 1];
        self.opt_lit_price_generation = [0; HC_MAX_LIT + 1];
        self.opt_lit_price_stamp = 0;
        self.opt_ml_price_scratch.clear();
        self.opt_ml_price_generation.clear();
        self.opt_ml_price_stamp = 0;
        self.ldm_sequences.clear();
    }

    /// Donor parity: `ZSTD_optLdm_skipRawSeqStoreBytes`. Fast-forward the
    /// raw LDM seq store cursor by `nb_bytes`, consuming whole stored
    /// sequences and leaving a partial-sequence offset in `pos_in_sequence`.
    pub(crate) fn ldm_skip_raw_seq_store_bytes(
        &self,
        seq_store: &mut HcRawSeqStore,
        nb_bytes: usize,
    ) {
        let mut curr_pos = seq_store.pos_in_sequence.saturating_add(nb_bytes);
        while curr_pos > 0 && seq_store.pos < seq_store.size {
            let curr_seq = self.ldm_sequences[seq_store.pos];
            let seq_len = curr_seq.lit_length.saturating_add(curr_seq.match_length);
            if curr_pos >= seq_len {
                curr_pos -= seq_len;
                seq_store.pos += 1;
            } else {
                seq_store.pos_in_sequence = curr_pos;
                break;
            }
        }
        if curr_pos == 0 || seq_store.pos == seq_store.size {
            seq_store.pos_in_sequence = 0;
        }
    }

    /// Donor parity: `ZSTD_optLdm_maybeAddMatch` / its preamble in
    /// `ZSTD_optLdm_getNextMatch`. Advance the per-block LDM window
    /// markers to the next raw LDM sequence and skip its literals.
    pub(crate) fn ldm_get_next_match_and_update_seq_store(
        &self,
        opt_ldm: &mut HcOptLdmState,
        curr_pos_in_block: usize,
        block_bytes_remaining: usize,
    ) {
        if opt_ldm.seq_store.size == 0 || opt_ldm.seq_store.pos >= opt_ldm.seq_store.size {
            opt_ldm.start_pos_in_block = usize::MAX;
            opt_ldm.end_pos_in_block = usize::MAX;
            return;
        }
        let curr_seq = self.ldm_sequences[opt_ldm.seq_store.pos];
        let curr_block_end_pos = curr_pos_in_block.saturating_add(block_bytes_remaining);
        let literals_bytes_remaining = curr_seq
            .lit_length
            .saturating_sub(opt_ldm.seq_store.pos_in_sequence);
        let match_bytes_remaining = if literals_bytes_remaining == 0 {
            curr_seq.match_length.saturating_sub(
                opt_ldm
                    .seq_store
                    .pos_in_sequence
                    .saturating_sub(curr_seq.lit_length),
            )
        } else {
            curr_seq.match_length
        };
        if literals_bytes_remaining >= block_bytes_remaining {
            opt_ldm.start_pos_in_block = usize::MAX;
            opt_ldm.end_pos_in_block = usize::MAX;
            self.ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, block_bytes_remaining);
            return;
        }
        opt_ldm.start_pos_in_block = curr_pos_in_block.saturating_add(literals_bytes_remaining);
        opt_ldm.end_pos_in_block = opt_ldm
            .start_pos_in_block
            .saturating_add(match_bytes_remaining);
        opt_ldm.offset = curr_seq.offset;
        if opt_ldm.end_pos_in_block > curr_block_end_pos {
            opt_ldm.end_pos_in_block = curr_block_end_pos;
            self.ldm_skip_raw_seq_store_bytes(
                &mut opt_ldm.seq_store,
                curr_block_end_pos.saturating_sub(curr_pos_in_block),
            );
        } else {
            self.ldm_skip_raw_seq_store_bytes(
                &mut opt_ldm.seq_store,
                literals_bytes_remaining.saturating_add(match_bytes_remaining),
            );
        }
    }

    /// Donor parity: `ZSTD_optLdm_maybeAddMatch`. Convert the active LDM
    /// window (open/close cursors set by
    /// [`ldm_get_next_match_and_update_seq_store`]) into a usable
    /// `MatchCandidate` when the current position falls inside it.
    pub(crate) fn ldm_maybe_add_match(
        &self,
        opt_ldm: &HcOptLdmState,
        curr_pos_in_block: usize,
        min_match: usize,
    ) -> Option<MatchCandidate> {
        let _ = self;
        let pos_diff = curr_pos_in_block.saturating_sub(opt_ldm.start_pos_in_block);
        let candidate_match_length = opt_ldm
            .end_pos_in_block
            .saturating_sub(opt_ldm.start_pos_in_block)
            .saturating_sub(pos_diff);
        if curr_pos_in_block < opt_ldm.start_pos_in_block
            || curr_pos_in_block >= opt_ldm.end_pos_in_block
            || candidate_match_length < min_match
        {
            return None;
        }
        Some(MatchCandidate {
            start: curr_pos_in_block,
            offset: opt_ldm.offset,
            match_len: candidate_match_length,
        })
    }

    /// Donor parity: `ZSTD_optLdm_processMatchCandidate`. Wraps
    /// [`ldm_maybe_add_match`] with a re-seed step when the parser has
    /// stepped past the current LDM window.
    pub(crate) fn ldm_process_match_candidate(
        &self,
        opt_ldm: &mut HcOptLdmState,
        curr_pos_in_block: usize,
        remaining_bytes: usize,
        min_match: usize,
    ) -> Option<MatchCandidate> {
        if opt_ldm.seq_store.size == 0 || opt_ldm.seq_store.pos >= opt_ldm.seq_store.size {
            return None;
        }
        if curr_pos_in_block >= opt_ldm.end_pos_in_block {
            if curr_pos_in_block > opt_ldm.end_pos_in_block {
                let pos_overshoot = curr_pos_in_block.saturating_sub(opt_ldm.end_pos_in_block);
                self.ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, pos_overshoot);
            }
            self.ldm_get_next_match_and_update_seq_store(
                opt_ldm,
                curr_pos_in_block,
                remaining_bytes,
            );
        }
        self.ldm_maybe_add_match(opt_ldm, curr_pos_in_block, min_match)
    }

    /// Donor parity: restore the seven per-frame scratch buffers that
    /// `build_optimal_plan_impl!` borrowed via `core::mem::take`. The
    /// passed `result` tuple is the parser's `(offset, reps, litlen,
    /// match_len)` return value — kept untouched and returned so the
    /// macro chains the move-out in a single expression.
    pub(crate) fn finish_optimal_plan(
        &mut self,
        buffers: HcOptimalPlanBuffers,
        result: (u32, [u32; 3], usize, usize),
    ) -> (u32, [u32; 3], usize, usize) {
        let HcOptimalPlanBuffers {
            nodes,
            mut candidates,
            store,
            ll_prices,
            ll_price_generations,
            ml_prices,
            ml_price_generations,
        } = buffers;
        candidates.clear();
        self.opt_nodes_scratch = nodes;
        self.opt_candidates_scratch = candidates;
        self.opt_store_scratch = store;
        self.opt_ll_price_scratch = ll_prices;
        self.opt_ll_price_generation = ll_price_generations;
        self.opt_ml_price_scratch = ml_prices;
        self.opt_ml_price_generation = ml_price_generations;
        result
    }

    /// Donor parity: `ZSTD_ldm_blockCompress` would seed external
    /// long-distance match candidates here when `enableLdm ==
    /// ZSTD_ps_enable`. This Rust encoder does not expose the donor's
    /// LDM producer / runtime switch yet, so every level-22 frame
    /// starts with an empty `ldm_sequences` buffer — keep the clear
    /// to defend against carry-over if a producer is added later.
    pub(crate) fn prepare_ldm_candidates(&mut self, current_abs_start: usize, current_len: usize) {
        self.ldm_sequences.clear();
        let _ = (current_abs_start, current_len);
    }

    /// Donor parity: `ZSTD_storeSeq` — encode `actual_offset` into the
    /// donor's compact offset base (1/2/3 for rep slots, otherwise
    /// `actual_offset + 3`) and update the rolling `reps` window in
    /// lock-step. Returns `(off_base, next_reps)`.
    pub(crate) fn encode_offset_with_reps(
        actual_offset: u32,
        lit_len: usize,
        reps: [u32; 3],
    ) -> (u32, [u32; 3]) {
        let mut next_reps = reps;
        let encoded = if lit_len > 0 {
            if actual_offset == reps[0] {
                1
            } else if actual_offset == reps[1] {
                2
            } else if actual_offset == reps[2] {
                3
            } else {
                actual_offset.saturating_add(3)
            }
        } else if actual_offset == reps[1] {
            1
        } else if actual_offset == reps[2] {
            2
        } else if reps[0] > 1 && actual_offset == reps[0] - 1 {
            3
        } else {
            actual_offset.saturating_add(3)
        };

        if lit_len > 0 {
            match encoded {
                1 => {}
                2 => {
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
                _ => {
                    next_reps[2] = next_reps[1];
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
            }
        } else {
            match encoded {
                1 => {
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
                _ => {
                    next_reps[2] = next_reps[1];
                    next_reps[1] = next_reps[0];
                    next_reps[0] = actual_offset;
                }
            }
        }

        (encoded, next_reps)
    }

    /// `encode_offset_with_reps` minus the rep-history update — used in
    /// the optimal parser's per-candidate price probe where the rep
    /// window hasn't been committed yet.
    #[inline(always)]
    pub(crate) fn encode_offset_base_with_reps(
        actual_offset: u32,
        lit_len: usize,
        reps: [u32; 3],
    ) -> u32 {
        if lit_len > 0 {
            if actual_offset == reps[0] {
                1
            } else if actual_offset == reps[1] {
                2
            } else if actual_offset == reps[2] {
                3
            } else {
                actual_offset.saturating_add(3)
            }
        } else if actual_offset == reps[1] {
            1
        } else if actual_offset == reps[2] {
            2
        } else if reps[0] > 1 && actual_offset == reps[0] - 1 {
            3
        } else {
            actual_offset.saturating_add(3)
        }
    }

    /// Donor parity: replay an already-emitted plan segment through the
    /// `optStatePtr_t` stats updater so the next parse pass sees frozen
    /// counts. Pure static helper — only mutates the caller-owned
    /// `opt_state` / `reps` / `literals_start`.
    pub(crate) fn update_plan_stats_segment(
        current: &[u8],
        current_len: usize,
        plan: &[HcOptimalSequence],
        literals_start: &mut usize,
        reps: &mut [u32; 3],
        opt_state: &mut HcOptState,
        accurate: bool,
    ) {
        if plan.is_empty() {
            return;
        }
        for item in plan {
            let lit_len = item.lit_len as usize;
            let match_len = item.match_len as usize;
            let start = literals_start.saturating_add(lit_len);
            if start < *literals_start || start + match_len > current_len {
                continue;
            }
            let literals = &current[*literals_start..start];
            let (off_base, next_reps) =
                Self::encode_offset_with_reps(item.offset, literals.len(), *reps);
            opt_state.update_stats(literals.len(), literals, off_base, match_len);
            *reps = next_reps;
            *literals_start = start + match_len;
        }
        opt_state.set_base_prices(accurate);
    }

    #[inline(always)]
    pub(crate) fn reset_opt_nodes(nodes: &mut [HcOptimalNode], start: usize, end: usize) {
        for node in &mut nodes[start..=end] {
            Self::reset_opt_node(node);
        }
    }

    #[inline(always)]
    pub(crate) fn reset_opt_node(node: &mut HcOptimalNode) {
        node.price = u32::MAX;
        // Donor only marks the slot as unreachable and not end-of-match here;
        // stale mlen is ignored while price is MAX and litlen is non-zero.
        node.litlen = u32::MAX;
    }

    #[inline(always)]
    pub(crate) fn add_price_delta(price: u32, add: u32, delta: i32) -> u32 {
        #[cfg(debug_assertions)]
        {
            let sum = price as i64 + add as i64 + delta as i64;
            debug_assert!((0..=u32::MAX as i64).contains(&sum));
        }
        price.wrapping_add(add).wrapping_add_signed(delta)
    }

    #[inline(always)]
    pub(crate) fn add_prices(lhs: u32, rhs: u32) -> u32 {
        let sum = lhs + rhs;
        debug_assert!(sum >= lhs);
        sum
    }

    #[inline(always)]
    pub(crate) fn cached_literal_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        byte: u8,
        prices: &mut [u32; HC_MAX_LIT + 1],
        generations: &mut [u32; HC_MAX_LIT + 1],
        stamp: u32,
    ) -> u32 {
        // SAFETY: `byte as usize` is `0..256` and the fixed-size arrays are
        // `[u32; HC_MAX_LIT + 1 = 257]`, so the index is statically in bounds.
        // Each cached_*_price call sits inside the optimal parser per-byte
        // hot loop where these bounds checks are pure overhead.
        let idx = byte as usize;
        unsafe {
            if *generations.get_unchecked(idx) == stamp {
                return *prices.get_unchecked(idx);
            }
            let price = profile.literal_price(stats, byte);
            *prices.get_unchecked_mut(idx) = price;
            *generations.get_unchecked_mut(idx) = stamp;
            price
        }
    }

    #[inline(always)]
    pub(crate) fn cached_lit_length_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        lit_len: usize,
        prices: &mut [u32],
        generations: &mut [u32],
        stamp: u32,
    ) -> u32 {
        if lit_len >= prices.len() {
            return profile.lit_length_price(stats, lit_len);
        }
        // SAFETY: the early-return above proves `lit_len < prices.len()`. The
        // matching `generations` slice is sized identically by the caller in
        // `build_optimal_plan_impl` (`opt_ll_price_scratch` /
        // `opt_ll_price_generation` are `resize`d together), so the same
        // index is in bounds for both.
        unsafe {
            if *generations.get_unchecked(lit_len) == stamp {
                return *prices.get_unchecked(lit_len);
            }
            let price = profile.lit_length_price(stats, lit_len);
            *prices.get_unchecked_mut(lit_len) = price;
            *generations.get_unchecked_mut(lit_len) = stamp;
            price
        }
    }

    #[inline(always)]
    pub(crate) fn cached_lit_length_delta_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        lit_len: usize,
        prices: &mut [u32],
        generations: &mut [u32],
        stamp: u32,
    ) -> i32 {
        if lit_len == 0 {
            return profile.lit_length_price(stats, lit_len) as i32
                - profile.lit_length_price(stats, lit_len.saturating_sub(1)) as i32;
        }
        let price =
            Self::cached_lit_length_price(profile, stats, lit_len, prices, generations, stamp);
        let previous =
            Self::cached_lit_length_price(profile, stats, lit_len - 1, prices, generations, stamp);
        price as i32 - previous as i32
    }

    #[inline(always)]
    pub(crate) fn cached_match_length_price(
        profile: HcOptimalCostProfile,
        stats: &HcOptState,
        match_len: usize,
        prices: &mut [u32],
        generations: &mut [u32],
        stamp: u32,
    ) -> u32 {
        if match_len >= prices.len() {
            return profile.match_length_price(stats, match_len);
        }
        // SAFETY: see `cached_lit_length_price` — the caller co-sizes
        // `opt_ml_price_scratch` and `opt_ml_price_generation`, and the
        // early return proves `match_len < prices.len()`.
        unsafe {
            if *generations.get_unchecked(match_len) == stamp {
                return *prices.get_unchecked(match_len);
            }
            let price = profile.match_length_price(stats, match_len);
            *prices.get_unchecked_mut(match_len) = price;
            *generations.get_unchecked_mut(match_len) = stamp;
            price
        }
    }
}
