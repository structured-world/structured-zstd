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
#[cfg(feature = "hash")]
use super::ldm::LdmProducer;
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
#[derive(Clone)]
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
    /// LDM producer — `None` while LDM is opt-out (current
    /// default) so the table allocation only happens for callers
    /// that opt in. The producer is **never auto-constructed**:
    /// every `CompressionLevel` preset leaves this field as
    /// `None`, matching upstream `libzstd.so.1` where
    /// `ZSTD_compress(..., level)` never enables LDM. The
    /// opt-in surface (Rust parameter API, see #27) builds an
    /// `LdmProducer` from caller-supplied params and assigns it
    /// here at frame setup time; `prepare_ldm_candidates` only
    /// consumes the field, it does not construct it. See
    /// [`super::ldm::LdmProducer`].
    ///
    /// Gated behind the `hash` feature because the producer's
    /// per-window XXH64 hashing depends on the optional
    /// `twox-hash` dependency; under `default-features = false`
    /// the field disappears and the `prepare_ldm_candidates`
    /// body shrinks to the legacy `ldm_sequences.clear()` stub.
    #[cfg(feature = "hash")]
    pub(crate) ldm_producer: Option<LdmProducer>,
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
            #[cfg(feature = "hash")]
            ldm_producer: None,
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
        #[cfg(feature = "hash")]
        if let Some(producer) = self.ldm_producer.as_mut() {
            producer.clear();
        }
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

    /// Donor parity: `ZSTD_ldm_blockCompress` seeds external
    /// long-distance match candidates here when `enableLdm ==
    /// ZSTD_ps_enable`. The default Rust encoder still keeps LDM
    /// disabled (`ldm_producer = None`); when an external caller
    /// opts in (#18 Phase 5 wiring — see #27 for the parameter
    /// surface), the producer is delegated to via
    /// [`LdmProducer::generate_into`].
    ///
    /// While `ldm_producer.is_none()` the behaviour matches the
    /// pre-Phase-5 stub: a defensive clear of [`Self::ldm_sequences`]
    /// so cross-frame carry-over is impossible if a producer is
    /// activated mid-session.
    ///
    /// # Coordinate convention (PR #139 review feedback)
    ///
    /// The producer operates entirely in **absolute stream
    /// coordinates** (so its bucket-table entries remain valid
    /// across window evictions — entries inserted by an earlier
    /// view of the frame are filtered by the staleness check
    /// `entry.offset < history_abs_start`, i.e. inclusive lower
    /// bound: entries at exactly `history_abs_start` survive,
    /// see `ldm::search::FindBestMatchInputs::lowest_index_abs`).
    /// The caller is expected to pass:
    ///
    /// * `live_history` — the contiguous *live* slice of the
    ///   per-frame `MatchTable::history` (`&history[history_start..]`).
    ///   `live_history[0]` corresponds to absolute position
    ///   `history_abs_start`.
    /// * `history_abs_start` — absolute stream position of
    ///   `live_history[0]`.
    /// * `current_abs_start` / `current_len` — absolute span of
    ///   the block to scan.
    ///
    /// `prepare_ldm_candidates` forwards these absolute
    /// coordinates straight to [`LdmProducer::generate_into`]; the
    /// abs→slice translation happens inside the producer only at
    /// the moment of `live_history[..]` indexing.
    pub(crate) fn prepare_ldm_candidates(
        &mut self,
        live_history: &[u8],
        history_abs_start: usize,
        current_abs_start: usize,
        current_len: usize,
    ) {
        self.ldm_sequences.clear();
        #[cfg(feature = "hash")]
        if let Some(producer) = self.ldm_producer.as_mut() {
            debug_assert!(current_abs_start >= history_abs_start);
            // MatchTable invariant: `live_history.len() ==
            // window_size`, and `current_abs_start =
            // history_abs_start + window_size − current_len`
            // (match_generator.rs around line 1330). The two
            // sides below must coincide; using `min(...)` would
            // silently truncate the scanned range and mask an
            // invariant violation. Raw `+` is safe — the frame-
            // level `check_stream_abs_headroom`
            // (`match_table/storage.rs:50`) guarantees
            // `history_abs_start + window_size +
            // STREAM_ABS_HEADROOM ≤ usize::MAX`.
            debug_assert_eq!(
                current_abs_start + current_len,
                history_abs_start + live_history.len(),
                "MatchTable invariant violation: current block range \
                 `[current_abs_start, +current_len)` must coincide with the \
                 live-history end (window_size == live_history.len())"
            );
            let block_end_abs = current_abs_start + current_len;
            producer.generate_into(
                live_history,
                history_abs_start,
                current_abs_start,
                block_end_abs,
                &mut self.ldm_sequences,
            );
        }
        #[cfg(not(feature = "hash"))]
        {
            // Under `default-features = false` (no `hash`),
            // `LdmProducer` is not compiled — `live_history` /
            // `history_abs_start` / `current_abs_start` /
            // `current_len` would otherwise be unused.
            let _ = (
                live_history,
                history_abs_start,
                current_abs_start,
                current_len,
            );
        }
    }

    /// Donor parity: `ZSTD_storeSeq` — encode `actual_offset` into the
    /// donor's compact offset base (1/2/3 for rep slots, otherwise
    /// `actual_offset + 3`) and update the rolling `reps` window in
    /// lock-step. Returns `(off_base, next_reps)`. The non-rep branch
    /// uses `saturating_add` so a `u32` near `u32::MAX` (only possible
    /// via malformed external input) clamps to `u32::MAX` rather than
    /// wrapping into a small rep-code value that would silently corrupt
    /// the encoded stream.
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
            // `checked_add` on both edges so a malformed / partially-built
            // plan can't overflow `usize` arithmetic before the
            // bounds guard fires. `saturating_add` would have masked
            // overflow as "clamp to usize::MAX" which then bypasses the
            // `> current_len` check.
            let Some(start) = literals_start.checked_add(lit_len) else {
                continue;
            };
            let Some(end) = start.checked_add(match_len) else {
                continue;
            };
            if end > current_len {
                continue;
            }
            let literals = &current[*literals_start..start];
            let (off_base, next_reps) =
                Self::encode_offset_with_reps(item.offset, literals.len(), *reps);
            opt_state.update_stats(literals.len(), literals, off_base, match_len);
            *reps = next_reps;
            *literals_start = end;
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
            // The `lit_len == 0` branch is the rare case where computing
            // `lit_len - 1` would underflow; we feed `0` to both
            // `lit_length_price` calls so the delta is 0 by construction.
            // No need to compute `0_usize - 1`.
            return 0;
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

#[cfg(test)]
mod ldm_helper_tests {
    //! Unit coverage for the LDM helper cluster that batch 14 lifted
    //! onto `impl BtMatcher`. The helpers walk the raw-LDM seq store
    //! and translate its (literals, match) sequences into the optimal
    //! parser's `MatchCandidate` shape. They are pure logic on
    //! [`HcRawSeqStore`] / [`HcOptLdmState`] — no SIMD, no `MatchTable`
    //! contact — so they exercise on every CI target.
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;
    use crate::encoding::opt::ldm::{HcOptLdmState, HcRawSeq, HcRawSeqStore};

    fn raw(lit: usize, mat: usize, off: usize) -> HcRawSeq {
        HcRawSeq {
            lit_length: lit,
            offset: off,
            match_length: mat,
        }
    }

    fn bt_with_seqs(seqs: Vec<HcRawSeq>) -> BtMatcher {
        let mut bt = BtMatcher::new();
        bt.ldm_sequences = seqs;
        bt
    }

    fn store(size: usize) -> HcRawSeqStore {
        HcRawSeqStore {
            pos: 0,
            pos_in_sequence: 0,
            size,
        }
    }

    /// Regression test for PR #139 Copilot review — threads #1/#2:
    /// `prepare_ldm_candidates` must translate the absolute
    /// `current_abs_start` (`history_abs_start + window_size -
    /// current_len`) into a slice index inside the supplied
    /// `history` slice. Before the fix, the code passed
    /// `current_abs_start` directly as `block_start` to
    /// `LdmProducer::generate_into`, which would index out of
    /// bounds (or into the wrong byte range) as soon as
    /// `history_abs_start != 0` after window eviction.
    ///
    /// We construct a `BtMatcher` with an active `LdmProducer`
    /// whose table starts empty, hand it a small `history` slice,
    /// and call `prepare_ldm_candidates` with absolute coordinates
    /// shifted by a non-zero `history_abs_start`. The call must
    /// complete without panic — under the pre-fix code it would
    /// panic on the `history[block_start..block_end]` slice access
    /// inside `generate_into`.
    #[test]
    #[cfg(feature = "hash")]
    fn prepare_ldm_candidates_translates_absolute_positions_to_slice_indices() {
        use crate::encoding::ldm::{LdmProducer, params::LdmParams};

        let mut bt = BtMatcher::new();
        // Activate the producer with hand-tuned small params —
        // the donor btultra2 defaults (`with_window_and_strategy(27, 9)`)
        // would allocate `1 << 23 = 8M` entries × 8 bytes = 64 MiB
        // for a table this test never actually populates beyond a
        // handful of entries. Build a 10-bit hash log instead so
        // the table is ~8 KiB — keeps the suite stable under
        // parallel nextest on 32-bit / memory-tight CI shards.
        bt.ldm_producer = Some(LdmProducer::new(LdmParams {
            window_log: 27,
            hash_log: 10,
            hash_rate_log: 4,
            min_match_length: 32,
            bucket_size_log: 4,
        }));

        // 256 bytes of arbitrary content — enough for the gear
        // hash to make progress against the donor btultra2
        // `min_match_length = 32`.
        let history: alloc::vec::Vec<u8> = (0u8..=255).collect();

        // Simulate a frame whose window has already evicted some
        // bytes: `history_abs_start = 1024`, and the current block
        // covers absolute `[1024, 1280)` which maps to slice
        // `[0, 256)` inside `history`.
        let history_abs_start = 1024usize;
        let current_abs_start = 1024usize;
        let current_len = history.len();

        // Pre-fix: would panic with `slice index out of bounds`
        // on `history[1024..1280]`. Post-fix: translates to
        // `[0..256]` and runs cleanly.
        bt.prepare_ldm_candidates(&history, history_abs_start, current_abs_start, current_len);
        // Cross-block fixture isn't engineered to emit a match —
        // the assertion is simply that the call succeeded.
    }

    #[test]
    fn ldm_skip_raw_seq_store_bytes_walks_whole_sequences() {
        let bt = bt_with_seqs(vec![raw(2, 3, 100), raw(4, 1, 200), raw(0, 5, 50)]);
        let mut s = store(3);
        // 5 = first seq (lit+match=5) → advance to seq 1 with pos_in_sequence=0
        bt.ldm_skip_raw_seq_store_bytes(&mut s, 5);
        assert_eq!(s.pos, 1);
        assert_eq!(s.pos_in_sequence, 0);
    }

    #[test]
    fn ldm_skip_raw_seq_store_bytes_leaves_partial_offset() {
        let bt = bt_with_seqs(vec![raw(2, 3, 100), raw(4, 1, 200)]);
        let mut s = store(2);
        // 4 < first seq's 5 bytes → stay on seq 0, pos_in_sequence=4
        bt.ldm_skip_raw_seq_store_bytes(&mut s, 4);
        assert_eq!(s.pos, 0);
        assert_eq!(s.pos_in_sequence, 4);
    }

    #[test]
    fn ldm_skip_raw_seq_store_bytes_exhausts_store_when_distance_too_large() {
        let bt = bt_with_seqs(vec![raw(1, 1, 1), raw(1, 1, 1)]);
        let mut s = store(2);
        // 10 bytes total but store only holds 4 (2 seqs × 2 bytes each)
        bt.ldm_skip_raw_seq_store_bytes(&mut s, 10);
        assert_eq!(s.pos, 2);
        assert_eq!(s.pos_in_sequence, 0);
    }

    #[test]
    fn ldm_get_next_match_empty_store_clears_window() {
        let bt = bt_with_seqs(vec![]);
        let mut opt_ldm = HcOptLdmState::default();
        opt_ldm.seq_store.size = 0;
        bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 10, 100);
        assert_eq!(opt_ldm.start_pos_in_block, usize::MAX);
        assert_eq!(opt_ldm.end_pos_in_block, usize::MAX);
    }

    #[test]
    fn ldm_get_next_match_marks_window_inside_block() {
        let bt = bt_with_seqs(vec![raw(2, 5, 17)]);
        let mut opt_ldm = HcOptLdmState::default();
        opt_ldm.seq_store.size = 1;
        // 100 bytes remain in block, current pos 10 → window starts at
        // 10 + lit_length (2) = 12 and ends 12 + match_length (5) = 17.
        bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 10, 100);
        assert_eq!(opt_ldm.start_pos_in_block, 12);
        assert_eq!(opt_ldm.end_pos_in_block, 17);
        assert_eq!(opt_ldm.offset, 17);
    }

    #[test]
    fn ldm_get_next_match_skips_when_literals_exceed_remaining_block() {
        let bt = bt_with_seqs(vec![raw(50, 5, 17)]);
        let mut opt_ldm = HcOptLdmState::default();
        opt_ldm.seq_store.size = 1;
        // Only 30 bytes left in block but seq starts with 50 literals →
        // window stays "no LDM in flight" and cursor advances past the
        // remaining bytes.
        bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 0, 30);
        assert_eq!(opt_ldm.start_pos_in_block, usize::MAX);
        assert_eq!(opt_ldm.end_pos_in_block, usize::MAX);
        assert_eq!(opt_ldm.seq_store.pos_in_sequence, 30);
    }

    #[test]
    fn ldm_maybe_add_match_returns_none_before_window() {
        let bt = bt_with_seqs(vec![]);
        let opt_ldm = HcOptLdmState {
            seq_store: store(0),
            start_pos_in_block: 20,
            end_pos_in_block: 30,
            offset: 7,
        };
        assert!(bt.ldm_maybe_add_match(&opt_ldm, 10, 4).is_none());
    }

    #[test]
    fn ldm_maybe_add_match_returns_none_past_window() {
        let bt = bt_with_seqs(vec![]);
        let opt_ldm = HcOptLdmState {
            seq_store: store(0),
            start_pos_in_block: 20,
            end_pos_in_block: 30,
            offset: 7,
        };
        assert!(bt.ldm_maybe_add_match(&opt_ldm, 30, 4).is_none());
        assert!(bt.ldm_maybe_add_match(&opt_ldm, 35, 4).is_none());
    }

    #[test]
    fn ldm_maybe_add_match_returns_none_below_min_match() {
        let bt = bt_with_seqs(vec![]);
        let opt_ldm = HcOptLdmState {
            seq_store: store(0),
            start_pos_in_block: 20,
            end_pos_in_block: 23,
            offset: 7,
        };
        // window length is 3 < min_match (4) → reject
        assert!(bt.ldm_maybe_add_match(&opt_ldm, 20, 4).is_none());
    }

    #[test]
    fn ldm_maybe_add_match_emits_candidate_inside_window() {
        let bt = bt_with_seqs(vec![]);
        let opt_ldm = HcOptLdmState {
            seq_store: store(0),
            start_pos_in_block: 20,
            end_pos_in_block: 30,
            offset: 7,
        };
        let cand = bt
            .ldm_maybe_add_match(&opt_ldm, 22, 4)
            .expect("should emit");
        assert_eq!(cand.start, 22);
        assert_eq!(cand.offset, 7);
        // candidate length = window - (pos - start) = 10 - 2 = 8
        assert_eq!(cand.match_len, 8);
    }

    #[test]
    fn ldm_process_match_candidate_returns_none_when_store_exhausted() {
        let mut bt = bt_with_seqs(vec![raw(2, 5, 17)]);
        bt.ldm_sequences.clear();
        let mut opt_ldm = HcOptLdmState::default();
        opt_ldm.seq_store.size = 0;
        assert!(
            bt.ldm_process_match_candidate(&mut opt_ldm, 0, 100, 4)
                .is_none()
        );
    }

    #[test]
    fn push_candidate_ladder_rejects_below_min_match() {
        let mut out: Vec<MatchCandidate> = vec![];
        let mut best = 0usize;
        let accepted = BtMatcher::push_candidate_ladder(
            &mut out,
            &mut best,
            MatchCandidate {
                start: 10,
                offset: 5,
                match_len: 2,
            },
            4,
        );
        assert!(!accepted);
        assert!(out.is_empty());
        assert_eq!(best, 0);
    }

    #[test]
    fn push_candidate_ladder_rejects_when_not_strictly_better() {
        let mut out: Vec<MatchCandidate> = vec![];
        let mut best = 8usize;
        let accepted = BtMatcher::push_candidate_ladder(
            &mut out,
            &mut best,
            MatchCandidate {
                start: 10,
                offset: 5,
                match_len: 8,
            },
            4,
        );
        // match_len == best_len_for_skip → not strictly greater → rejected
        assert!(!accepted);
        assert!(out.is_empty());
        assert_eq!(best, 8);
    }

    #[test]
    fn ldm_get_next_match_clamps_window_to_block_end() {
        // Seq match window extends past the block boundary — the
        // function clamps `end_pos_in_block` to `curr_block_end_pos`
        // and skips through the seq store accordingly.
        let bt = bt_with_seqs(vec![raw(0, 100, 99)]);
        let mut opt_ldm = HcOptLdmState::default();
        opt_ldm.seq_store.size = 1;
        bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 10, 30);
        // Window would naturally be [10, 110); clamped to [10, 40)
        // because remaining block bytes only allow that span.
        assert_eq!(opt_ldm.start_pos_in_block, 10);
        assert_eq!(opt_ldm.end_pos_in_block, 40);
        assert_eq!(opt_ldm.offset, 99);
    }

    #[test]
    fn ldm_process_match_candidate_handles_overshoot_into_next_seq() {
        // Parser jumped past the active LDM window — `pos_overshoot`
        // forces the seq-store cursor forward before re-seeding.
        let bt = bt_with_seqs(vec![raw(0, 5, 17), raw(0, 5, 33)]);
        let mut opt_ldm = HcOptLdmState {
            seq_store: HcRawSeqStore {
                pos: 1,
                pos_in_sequence: 0,
                size: 2,
            },
            start_pos_in_block: 0,
            end_pos_in_block: 5,
            offset: 17,
        };
        // curr_pos = 12 (overshot the previous window's end by 7) → must
        // consume the overshoot from the next seq, then re-seed.
        let _ = bt.ldm_process_match_candidate(&mut opt_ldm, 12, 100, 4);
        // After overshoot consumes 7 bytes from seq 1 (which only had
        // 5), cursor advances to pos=2 (store exhausted).
        assert_eq!(opt_ldm.seq_store.pos, 2);
    }

    #[test]
    fn ldm_process_match_candidate_reseeds_after_overshoot() {
        // Two sequences. Seq 0 was just emitted (cursor advanced past it),
        // so the active window points at seq 1's offset (33). The parser
        // arrives at position 5 — past the seq 0 window — and must
        // re-seed onto seq 1's window without overshoot logic re-reading
        // seq 0.
        let bt = bt_with_seqs(vec![raw(0, 5, 17), raw(0, 5, 33)]);
        let mut opt_ldm = HcOptLdmState {
            seq_store: HcRawSeqStore {
                pos: 1,
                pos_in_sequence: 0,
                size: 2,
            },
            start_pos_in_block: 0,
            end_pos_in_block: 5,
            offset: 17,
        };
        let cand = bt
            .ldm_process_match_candidate(&mut opt_ldm, 5, 100, 4)
            .expect("should emit reseeded candidate");
        assert_eq!(cand.start, 5);
        assert_eq!(cand.offset, 33);
        assert_eq!(cand.match_len, 5);
    }
}
