//! Plain-data types used by the optimal-parser DP and traceback.
//!
//! Extracted from `match_generator.rs` as part of #111 Phase 1
//! (structural split). All types are mechanical moves — names,
//! signatures, derives, and `Default` impls match the pre-extraction
//! monolith byte-for-byte. Methods that operate on these types remain
//! on `HcMatchGenerator` for now; they migrate in later phases.

use alloc::vec::Vec;

use super::super::cost_model::HcOptimalCostProfile;

/// One candidate match produced by a match-finder probe. Carries the
/// absolute starting position and the donor-style offset/length pair.
#[derive(Copy, Clone, Debug)]
pub(crate) struct MatchCandidate {
    pub(crate) start: usize,
    pub(crate) offset: usize,
    pub(crate) match_len: usize,
}

/// DP cell in the optimal parser table. Stores the running price, the
/// chosen offset/match-length, the literal-run length, and the rep-code
/// history that would be active after committing this cell.
#[derive(Copy, Clone, Debug)]
pub(crate) struct HcOptimalNode {
    pub(crate) price: u32,
    pub(crate) off: u32,
    pub(crate) mlen: u32,
    pub(crate) litlen: u32,
    pub(crate) reps: [u32; 3],
}

impl Default for HcOptimalNode {
    fn default() -> Self {
        Self {
            price: u32::MAX,
            off: 0,
            mlen: 0,
            // Donor parity: uninitialized DP slots use litlen != 0
            // (C code uses !0) so they are never treated as end-of-match.
            litlen: u32::MAX,
            reps: [1, 4, 8],
        }
    }
}

/// Single sequence emitted by the optimal-parser traceback.
#[derive(Copy, Clone)]
pub(crate) struct HcOptimalSequence {
    pub(crate) offset: u32,
    pub(crate) match_len: u32,
    pub(crate) lit_len: u32,
}

/// Inputs to the per-position candidate collection step. Bundled so the
/// `collect_optimal_candidates_initialized_body!` macro can hand-roll the
/// argument list once.
#[derive(Copy, Clone)]
pub(crate) struct HcCandidateQuery {
    pub(crate) reps: [u32; 3],
    pub(crate) lit_len: usize,
    pub(crate) ldm_candidate: Option<MatchCandidate>,
}

/// Per-frame DP state captured at the start of `build_optimal_plan_impl`:
/// the rep history to use as opt[0], the initial literal-run length, and
/// the cost-model profile picked for the current strategy.
#[derive(Copy, Clone)]
pub(crate) struct HcOptimalPlanState {
    pub(crate) reps: [u32; 3],
    pub(crate) litlen: usize,
    pub(crate) profile: HcOptimalCostProfile,
}

/// Bundle of scratch buffers the DP body owns for the duration of one
/// block. The matcher hands ownership over via `core::mem::take` and
/// receives them back through `finish_optimal_plan`, so the borrow
/// checker can split the matcher's fields without macro-level
/// scaffolding.
pub(crate) struct HcOptimalPlanBuffers {
    pub(crate) nodes: Vec<HcOptimalNode>,
    pub(crate) candidates: Vec<MatchCandidate>,
    pub(crate) store: Vec<HcOptimalNode>,
    pub(crate) ll_prices: Vec<u32>,
    pub(crate) ll_price_generations: Vec<u32>,
    pub(crate) ml_prices: Vec<u32>,
    pub(crate) ml_price_generations: Vec<u32>,
}
