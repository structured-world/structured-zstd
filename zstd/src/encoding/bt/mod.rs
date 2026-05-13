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

use super::cost_model::{HC_MAX_LIT, HcOptState};
use super::opt::ldm::HcRawSeq;
use super::opt::types::{HcOptimalNode, HcOptimalSequence, MatchCandidate};

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
}
