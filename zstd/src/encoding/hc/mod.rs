//! Binary-tree match generator and optimal parser of the btopt / btultra /
//! btultra2 strategies.
//!
//! [`HcMatcher`] carries the search knobs the tree walk and the optimal
//! parser read; the tables live on the shared
//! [`super::match_table::storage::MatchTable`].
//!
//! Upstream zstd parity reference: `lib/compress/zstd_opt.c`.

#![allow(dead_code)]

pub(crate) mod generator;
pub(crate) mod optimal;
pub(crate) mod priceset;

use super::cost_model::{HC_FORMAT_MINMATCH, HC_OPT_NUM, HcOptimalCostProfile};

/// Key width the match tables hash a position on. Upstream zstd parity:
/// `MIN_MATCH` in `lib/compress/zstd_lazy.c`.
pub(crate) const HC_MIN_MATCH_LEN: usize = 4;

/// Capacity the optimal parser reserves for its per-position candidate buffer,
/// so the search writes into it without a capacity check. Every candidate it
/// keeps is strictly longer than the one before (repeats, hash3, both trees and
/// the long-distance producer all extend one ladder), starts at three bytes and
/// ends the walk once it passes [`HC_OPT_NUM`], whatever the compare budget:
/// upstream sizes its `matches` array the same way (`ZSTD_OPT_NUM + 1`).
pub(crate) const HC_OPT_CANDIDATE_CAPACITY: usize = HC_OPT_NUM + 8;

/// The per-frame search configuration of the tree walk.
#[derive(Clone)]
pub(crate) struct HcMatcher {
    /// Tree compares per position. Upstream zstd parity:
    /// `1 << params->cParams.searchLog`.
    pub(crate) search_depth: usize,
    /// "Sufficient" match length that ends a search. Upstream zstd parity:
    /// `params->cParams.targetLength`.
    pub(crate) target_len: usize,
}

impl HcMatcher {
    pub(crate) fn new(search_depth: usize, target_len: usize) -> Self {
        Self {
            search_depth,
            target_len,
        }
    }

    /// Upstream zstd parity: per-pass clamp of the "good enough — stop probing"
    /// threshold that the optimal parser passes to the tree walk.
    /// Reflects upstream zstd `ZSTD_compressBlock_opt_generic` which caps the
    /// profile's `sufficient_match_len` by the user-configured
    /// `targetLength` and the `HC_OPT_NUM` ceiling.
    pub(crate) fn sufficient_match_len_for_pass(&self, profile: HcOptimalCostProfile) -> usize {
        profile
            .sufficient_match_len
            .min(self.target_len)
            .clamp(HC_FORMAT_MINMATCH, HC_OPT_NUM - 1)
    }
}
