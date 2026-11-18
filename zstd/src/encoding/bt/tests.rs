use super::*;
use crate::encoding::cost_model::{HC_OPT_NODE_LEN, HC_OPT_PRICE_ARENA_LEN};
use crate::encoding::hc::HC_OPT_CANDIDATE_CAPACITY;

/// The workspace estimate must equal what a matcher retains once every
/// scratch buffer sits at the size its allocation site grows it to. The price
/// arena is allocated at `HC_OPT_PRICE_ARENA_LEN` pairs; an estimate still
/// budgeting frontier-sized regions over-reports every optimal-level context.
#[test]
fn estimated_workspace_bytes_matches_retained_scratch_at_growth_bounds() {
    use crate::encoding::workspace::region_bytes;
    // A 128 KiB block: the estimate rounds each region to the workspace's
    // alignment, the owned buffers here are exact, so the two are compared
    // through the same rounding.
    let block = 128 * 1024;
    let plan = BtMatcher::plan_room_len(block);
    let mut bt = BtMatcher::new();
    bt.opt_nodes_scratch = Table::owned_unwritten(HC_OPT_NODE_LEN);
    bt.opt_node_prices_scratch = Table::owned_unwritten(HC_OPT_NODE_LEN);
    bt.opt_candidates_scratch = Table::owned_unwritten(HC_OPT_CANDIDATE_CAPACITY);
    bt.opt_segment_plan_scratch = Table::owned_unwritten(plan);
    bt.opt_price_arena = Table::owned(alloc::vec![[0u32; 2]; HC_OPT_PRICE_ARENA_LEN]);
    let rounded = region_bytes::<HcOptimalNode>(HC_OPT_NODE_LEN)
        + region_bytes::<u32>(HC_OPT_NODE_LEN)
        + region_bytes::<OptMatch>(HC_OPT_CANDIDATE_CAPACITY)
        + region_bytes::<HcOptimalSequence>(plan)
        + region_bytes::<[u32; 2]>(HC_OPT_PRICE_ARENA_LEN);
    assert_eq!(
        BtMatcher::estimated_workspace_bytes(block),
        core::mem::size_of::<BtMatcher>() + rounded
    );
    // Every byte the estimate charges is a byte the scratch holds, up to the
    // rounding of five regions.
    assert!(rounded - bt.heap_size() < 5 * crate::encoding::workspace::ALIGN);
}

/// The repeat-code probe selects instead of branching; it must return what
/// the RFC 8878 3.1.2.5 compare chain returns for every offset and history,
/// including colliding repeats (the first match wins), `rep[0] <= 1` with no
/// literals (no `rep[0] - 1` code), and an offset at the saturation edge.
#[test]
fn offset_base_probe_matches_the_repeat_compare_chain() {
    fn chain(offset: u32, lit_len: usize, reps: [u32; 3]) -> u32 {
        if lit_len > 0 {
            if offset == reps[0] {
                1
            } else if offset == reps[1] {
                2
            } else if offset == reps[2] {
                3
            } else {
                offset.saturating_add(3)
            }
        } else if offset == reps[1] {
            1
        } else if offset == reps[2] {
            2
        } else if reps[0] > 1 && offset == reps[0] - 1 {
            3
        } else {
            offset.saturating_add(3)
        }
    }
    let values = [0u32, 1, 2, 3, 4, 5, u32::MAX - 3, u32::MAX - 2, u32::MAX];
    for &r0 in &values {
        for &r1 in &values {
            for &r2 in &values {
                for &offset in &values {
                    for lit_len in [0usize, 1, 7] {
                        let reps = [r0, r1, r2];
                        assert_eq!(
                            BtMatcher::encode_offset_base_with_reps(offset, lit_len, reps),
                            chain(offset, lit_len, reps),
                            "offset={offset} lit_len={lit_len} reps={reps:?}"
                        );
                    }
                }
            }
        }
    }
}
