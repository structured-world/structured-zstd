//! Plain-data types used by the optimal-parser DP and traceback.
//!
//! Extracted from `match_generator.rs` as part of #111 Phase 1
//! (structural split). Names, fields, derives, and `Default` impls are
//! preserved; the only intentional internal-API change is the
//! `pub(crate)` visibility on each item so the remaining methods on
//! `HcMatchGenerator` can import them through
//! `super::opt::types::{…}` (no re-exports are added at `opt::`).
//! Methods that operate on these types stay on `HcMatchGenerator` for
//! now and migrate in later phases.

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
    /// Block-relative byte offset of the SEGMENT this plan pass covers.
    /// The optimal parser runs per-segment with segment-relative
    /// positions, but the LDM `ldm_sequences` are block-relative, so the
    /// raw LDM seq-store is fast-forwarded by this many bytes at the
    /// start of each segment to land its windows at the right positions.
    /// `0` for the first segment / when LDM is inactive.
    pub(crate) block_offset: usize,
}

/// Bundle of scratch buffers the DP body owns for the duration of one
/// block. The matcher hands ownership over via `core::mem::take` and
/// receives them back through `finish_optimal_plan`, so the borrow
/// checker can split the matcher's fields without macro-level
/// scaffolding.
pub(crate) struct HcOptimalPlanBuffers {
    pub(crate) nodes: alloc::boxed::Box<[HcOptimalNode]>,
    pub(crate) candidates: Vec<MatchCandidate>,
    pub(crate) store: Vec<HcOptimalNode>,
    /// Single backing allocation for the four frontier-sized price/stale
    /// arrays (LL price, LL generation, ML price, ML generation), laid out
    /// as four fixed-stride `HC_OPT_NUM + 1` regions. One base pointer +
    /// fixed offsets, mirroring the donor's single-workspace opt arrays;
    /// the DP body splits it with `split_at_mut` into four disjoint slices.
    /// Fixed stride (not `frontier_limit`-dependent) so the generation
    /// stamps land in the same cell across calls with different frontiers.
    pub(crate) price_arena: alloc::boxed::Box<[u32]>,
}
