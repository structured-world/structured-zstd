use alloc::vec::Vec;

use super::*;
use crate::encoding::cost_model::{HC_OPT_NODE_LEN, HC_OPT_NUM, HC_OPT_PRICE_ARENA_LEN};
use crate::encoding::hc::MAX_HC_SEARCH_DEPTH;

/// The workspace estimate must equal what a matcher retains once every
/// scratch buffer sits at the size its allocation site grows it to. The price
/// arena is allocated at `HC_OPT_PRICE_ARENA_LEN` pairs; an estimate still
/// budgeting frontier-sized regions over-reports every optimal-level context.
#[test]
fn estimated_workspace_bytes_matches_retained_scratch_at_growth_bounds() {
    let frontier = HC_OPT_NUM + 1;
    let mut bt = BtMatcher::new();
    bt.opt_nodes_scratch = Box::new_uninit_slice(HC_OPT_NODE_LEN);
    bt.opt_node_prices_scratch = Box::new_uninit_slice(HC_OPT_NODE_LEN);
    bt.opt_candidates_scratch = Vec::with_capacity(MAX_HC_SEARCH_DEPTH);
    bt.opt_store_scratch = Vec::with_capacity(HC_OPT_NODE_LEN);
    bt.opt_segment_plan_scratch = Vec::with_capacity(frontier);
    bt.opt_seed_plan_scratch = Vec::with_capacity(frontier);
    bt.opt_price_arena = alloc::vec![[0u32; 2]; HC_OPT_PRICE_ARENA_LEN].into_boxed_slice();
    assert_eq!(
        BtMatcher::estimated_workspace_bytes(),
        core::mem::size_of::<BtMatcher>() + bt.heap_size()
    );
}
