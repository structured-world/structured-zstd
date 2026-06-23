//! HashChain / binary-tree match generator (`HcMatchGenerator`).
//!
//! The HC chain matcher plus the BT-backed optimal-parse machinery the lazy /
//! btopt / btultra strategies run on: the `HcBackend` discriminator, the
//! `HcMatchGenerator` storage + methods, the `btlazy2` cost helpers, the
//! optimal-plan body macros, and the `build_optimal_plan` driver. Moved
//! verbatim from `match_generator.rs` (no behaviour change); encoding-level
//! paths are absolute (`crate::encoding::…`) for the deeper module.

use alloc::vec::Vec;

use crate::encoding::Sequence;
use crate::encoding::blocks::encode_offset_with_history;
use crate::encoding::bt::BtMatcher;
use crate::encoding::cost_model::{
    HC_BITCOST_MULTIPLIER, HC_OPT_NODE_LEN, HC_OPT_NUM, HC_OPT_PRICE_ARENA_LEN,
    HC_OPT_PRICE_STRIDE, HC_PREDEF_THRESHOLD, HcOptState, HcOptimalCostProfile,
};
use crate::encoding::hc::{HC_MIN_MATCH_LEN, MAX_HC_SEARCH_DEPTH};
use crate::encoding::levels::config::HcConfig;
use crate::encoding::match_generator::{
    HC_OPT_MIN_MATCH_LEN, HC_SEARCH_DEPTH, HC_TARGET_LEN, HcBackend,
};
use crate::encoding::match_table::storage::HC3_HASH_LOG;
use crate::encoding::opt::ldm::{HcOptLdmState, HcRawSeqStore};
use crate::encoding::opt::types::{
    HcCandidateQuery, HcOptimalNode, HcOptimalPlanBuffers, HcOptimalPlanState, HcOptimalSequence,
    MatchCandidate,
};
// Driver / sibling-backend types the moved `#[cfg(test)]` parse helpers exercise.
#[cfg(test)]
use crate::encoding::CompressionLevel;
#[cfg(test)]
use crate::encoding::Matcher;
#[cfg(test)]
use crate::encoding::dfast::DfastMatchGenerator;
#[cfg(test)]
use crate::encoding::match_generator::MatchGeneratorDriver;

impl HcBackend {
    /// Heap bytes held by the backend. `Hc` is zero-sized; `Bt` boxes a
    /// `BtMatcher`, so count the boxed payload plus its own scratch heap.
    pub(crate) fn heap_size(&self) -> usize {
        match self {
            Self::Hc => 0,
            Self::Bt(bt) => core::mem::size_of::<crate::encoding::bt::BtMatcher>() + bt.heap_size(),
        }
    }

    /// Mutable accessor on the BT matcher; panics if the active
    /// backend is `Hc`. The HC-or-Bt branches in orchestrator code use
    /// `let HcBackend::Bt(bt) = &self.backend` directly for readonly
    /// access — this helper exists so macro bodies that already drive
    /// a mutable BT update through the optimal parser can write
    /// `$self.backend.bt_mut().X` without an outer `match` ladder.
    #[inline(always)]
    pub(crate) fn bt_mut(&mut self) -> &mut crate::encoding::bt::BtMatcher {
        match self {
            Self::Bt(bt) => bt,
            Self::Hc => unreachable!("BT-only accessor called in HC mode"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct HcMatchGenerator {
    /// Shared match-finder storage (window, history, hash / chain /
    /// hash3 tables, dictionary-priming flags). Used identically by HC
    /// and BT modes; backend-specific table interpretation lives in the
    /// matcher methods on this struct.
    pub(crate) table: crate::encoding::match_table::storage::MatchTable,
    /// HC runtime knobs (lazy_depth, search_depth, target_len). Always
    /// present — BT modes still consult `hc.search_depth` for repcode
    /// probing and chain candidate enumeration.
    pub(crate) hc: crate::encoding::hc::HcMatcher,
    /// Backend discriminator. [`HcBackend::Hc`] is zero-sized for the
    /// lazy / lazy2 path so HC-only generators don't carry the BT
    /// optimal-parser scratch buffers. [`HcBackend::Bt`] holds the
    /// `BtMatcher` when an optimal mode is configured.
    pub(crate) backend: HcBackend,
    /// Compile-time strategy tag mirrored from
    /// [`MatchGeneratorDriver::strategy_tag`] during `configure()`.
    /// The driver hot path never reads this — it dispatches to
    /// `compress_block::<S>` from its own tag — but the
    /// `#[cfg(test)] start_matching` helper consumes it so artificial
    /// test setups still pick the correct concrete `S` for the
    /// const-generic optimal parser (BtOpt vs BtUltra vs BtUltra2).
    /// Without this field the test path would have to collapse
    /// `BtOpt` and `BtUltra` onto the same monomorphisation since
    /// `table.uses_bt` / `table.is_btultra2` alone can't tell them
    /// apart.
    pub(crate) strategy_tag: crate::encoding::strategy::StrategyTag,
}

// Plain-data types relocated to [`crate::encoding::opt::types`] and
// [`crate::encoding::opt::ldm`] by #111 Phase 1. The use statements at
// the top of this file bring them back into scope so the existing
// methods on `HcMatchGenerator` compile unchanged.

/// `bt_insert_step_no_rebase` body parameterized over the per-CPU
/// `count_match_from_indices` symbol. Each kernel-specific wrapper invokes
/// the macro with its own `fastpath::<kernel>::count_match_from_indices`
/// path so the call resolves inside the wrapper's `#[target_feature]`
/// umbrella and inlines instead of paying the function-call ABI per BT walk
/// iteration. Used only by `HcMatchGenerator` BT walk wrappers below.
///
/// Crate-private: the macro body references private `encoding::*`
/// modules via `$crate::...`, so it is unusable downstream and is
/// re-exported only inside this crate via `pub(crate) use` below.
macro_rules! bt_insert_step_no_rebase_body {
    ($table:expr, $search_depth:expr, $abs_pos:ident, $current_abs_end:ident, $target_abs:ident, $cmf:path) => {{
        let idx = $abs_pos - $table.history_abs_start;
        // Borrowed-aware live region (owned: `history[history_start..]`;
        // borrowed: the in-place input `[0, block_end)`). Reborrow-then-raw-ptr
        // so the slice holds NO borrow and coexists with the `&mut $table`
        // binary-tree writes below. Owned is byte-identical (same bytes).
        let concat: &[u8] = unsafe {
            let lh = $table.live_history();
            core::slice::from_raw_parts(lh.as_ptr(), lh.len())
        };
        if idx + 8 > concat.len() {
            return 1;
        }
        debug_assert!(
            $abs_pos <= $current_abs_end,
            "BT walker called past current block end"
        );
        let tail_limit = $current_abs_end - $abs_pos;
        let hash = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $table.hash_log,
            $table.search_mls,
        );
        // Prefetch the hash bucket now. For the large L16+ hash table over
        // high-entropy input the bucket is L3/DRAM-cold, and unlike upstream's
        // monolithic ZSTD_btGetAllMatches (which overlaps this miss with its
        // inline rep/hash3 prologue) the read+write of `hash_table[hash]`
        // below is reached with nothing to hide it behind — it stalled a large
        // share of this function's cycles. Issuing the hint here lets the miss
        // overlap the address setup that follows.
        #[cfg(all(
            target_feature = "sse",
            any(target_arch = "x86", target_arch = "x86_64")
        ))]
        {
            #[cfg(target_arch = "x86")]
            use core::arch::x86::{_MM_HINT_T0, _mm_prefetch};
            #[cfg(target_arch = "x86_64")]
            use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            // SAFETY: prefetch is a hint that never faults; `hash` indexes
            // `hash_table` directly below, so it is in bounds.
            unsafe {
                _mm_prefetch($table.hash_table.as_ptr().add(hash).cast(), _MM_HINT_T0);
            }
            // Prefetch the NEXT position's bucket too. The optimal-parser DP
            // advances one position per iteration, so this miss is issued a
            // full BT walk plus the next iteration's pre-collect work ahead of
            // the collect that will read it — far more lead than the same-call
            // hint above, enough to hide the full DRAM latency.
            if idx + 1 + 8 <= concat.len() {
                let hash_next =
                    $crate::encoding::match_table::storage::MatchTable::hash_position_at(
                        concat,
                        idx + 1,
                        $table.hash_log,
                        $table.search_mls,
                    );
                // SAFETY: prefetch never faults; an out-of-range index is a
                // harmless no-op hint.
                unsafe {
                    _mm_prefetch(
                        $table.hash_table.as_ptr().add(hash_next).cast(),
                        _MM_HINT_T0,
                    );
                }
            }
        }
        let Some(relative_pos) = $table.relative_position($abs_pos) else {
            return 1;
        };
        let stored = relative_pos + 1;
        let bt_mask = $table.bt_mask();
        // `abs_pos < bt_mask` legitimately happens for the first BT walk of
        // a fresh frame (bt_low effectively "no floor"). Saturating keeps
        // the floor at 0 so the `candidate_abs <= bt_low` check never
        // triggers early; raw subtraction would underflow into a huge
        // sentinel that ALWAYS triggers.
        let bt_low = $abs_pos.saturating_sub(bt_mask);
        // Hoist the BT pointer-pair base out of `self` once — see the
        // collect-matches body for the full rationale (per-step Vec reload +
        // bounds check through `&mut self` vs the upstream zstd's raw `U32*` walk).
        let chain_ptr = $table.chain_table.as_mut_ptr();
        debug_assert_eq!($table.chain_table.len(), 2 << $table.bt_log());
        let window_low = $table.window_low_abs_for_target($target_abs);
        // `abs_pos + 9` is safe in raw form: `MatchTable::add_data` caps
        // total input at `usize::MAX - STREAM_ABS_HEADROOM` (where
        // `STREAM_ABS_HEADROOM = HC_OPT_NUM + 16`), so every
        // frame-lifetime absolute cursor passed to the BT walker stays
        // below `usize::MAX - 9` regardless of stream length or
        // pointer width. The guard is hoisted to the data-ingest
        // boundary so this per-position site pays zero arithmetic
        // overhead in the hot loop.
        let mut match_end_abs = $abs_pos + 9;
        let mut best_len = 8usize;
        let mut compares_left = $search_depth;
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = $table.bt_pair_index_for_abs($abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = $table.hash_table[hash];
        $table.hash_table[hash] = stored;

        while compares_left > 0 {
            if match_stored == $crate::encoding::match_table::storage::HC_EMPTY {
                break;
            }
            // Reject stale post-rebase slots whose pre-shift position is below
            // `index_shift` explicitly. A `wrapping_sub` maps such a slot to a
            // near-`usize::MAX` value that the `>= abs_pos` test only rejects
            // while `abs_pos` is far from the integer ceiling; on a
            // long-running rebased stream (reachable on 32-bit) `abs_pos` can
            // approach the ceiling and the wrapped value can land back inside
            // `[window_low, abs_pos)`. `checked_sub` ends the walk on the
            // underflow instead. `match_stored != HC_EMPTY` here, so the `- 1`
            // cannot underflow.
            let Some(candidate_abs) = ($table.position_base + (match_stored as usize - 1))
                .checked_sub($table.index_shift)
            else {
                break;
            };
            if candidate_abs < window_low || candidate_abs >= $abs_pos {
                break;
            }
            compares_left -= 1;

            let next_pair_idx = $table.bt_pair_index_for_abs(candidate_abs);
            // SAFETY: `next_pair_idx (+1)` = `2*(candidate_abs & bt_mask) (+1)`
            // ≤ `chain_table.len()-1`; `chain_ptr` is the hoisted live base,
            // table not realloc'd during the walk.
            let next_smaller = unsafe { *chain_ptr.add(next_pair_idx) };
            let next_larger = unsafe { *chain_ptr.add(next_pair_idx + 1) };
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - $table.history_abs_start;
            // SAFETY: BT walk invariant — `candidate_idx + tail_limit ≤
            // concat.len()` since the candidate is within
            // `[history_abs_start, abs_pos)` and `tail_limit ≤
            // current_abs_end - abs_pos`.
            let match_len = unsafe { $cmf(concat, idx, candidate_idx, tail_limit, seed_len) };

            if match_len > best_len {
                best_len = match_len;
                // `candidate_abs + match_len <= current_abs_end` by BT walk
                // invariant — `match_len <= tail_limit = current_abs_end -
                // abs_pos` and `candidate_abs < abs_pos`.
                let candidate_end = candidate_abs + match_len;
                if candidate_end > match_end_abs {
                    match_end_abs = candidate_end;
                }
            }

            if match_len >= tail_limit {
                break;
            }

            let candidate_next = candidate_idx + match_len;
            let current_next = idx + match_len;
            // SAFETY: first-differing positions after a match_len-long prefix;
            // match_len < tail_limit (break above) + BT-walk bound
            // idx/candidate_idx + tail_limit <= concat.len() keep both in range.
            if unsafe {
                *concat.get_unchecked(candidate_next) < *concat.get_unchecked(current_next)
            } {
                // SAFETY: `smaller_slot` holds a valid pair index (init
                // `pair_idx`, updated to `next_pair_idx + 1`); the `usize::MAX`
                // sentinel is set only just before `break`, never written here.
                unsafe { *chain_ptr.add(smaller_slot) = match_stored };
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                // SAFETY: as above for `larger_slot`.
                unsafe { *chain_ptr.add(larger_slot) = match_stored };
                common_length_larger = match_len;
                if candidate_abs <= bt_low {
                    larger_slot = usize::MAX;
                    break;
                }
                larger_slot = next_pair_idx;
                match_stored = next_smaller;
            }
        }

        // SAFETY: both slots, when not the `usize::MAX` sentinel, hold valid
        // pair indices into the hoisted `chain_table` base.
        if smaller_slot != usize::MAX {
            unsafe {
                *chain_ptr.add(smaller_slot) = $crate::encoding::match_table::storage::HC_EMPTY
            };
        }
        if larger_slot != usize::MAX {
            unsafe {
                *chain_ptr.add(larger_slot) = $crate::encoding::match_table::storage::HC_EMPTY
            };
        }

        let speed_positions = if best_len > 384 {
            (best_len - 384).min(192)
        } else {
            0
        };
        // `match_end_abs` is initialized to `abs_pos + 9` and is only
        // reassigned inside the `candidate_end > match_end_abs` branch
        // above. So even though an individual `candidate_end =
        // candidate_abs + match_len` can land below `abs_pos` (the
        // candidate sits earlier in history and the match runs short),
        // the variable itself never drops below its initial value.
        // That gives `match_end_abs ≥ abs_pos + 9 > abs_pos + 8` as a
        // loop-wide invariant, so the raw subtraction below cannot
        // underflow.
        speed_positions.max(match_end_abs - ($abs_pos + 8))
    }};
}
pub(crate) use bt_insert_step_no_rebase_body;

/// `build_optimal_plan_impl` body parameterized over the per-CPU
/// `collect_optimal_candidates_initialized_<kernel>` method name. Caller
/// passes its `&mut self`, the seven DP entry-point arguments, and the
/// kernel-specific collect method. Each per-kernel wrapper invokes this
/// macro inside its own `#[target_feature]` umbrella so the per-position
/// `$collect` call inlines and the entire DP loop runs as one straight-line
/// hot path without an ABI barrier between the DP and the match-gathering
/// pipeline.
///
/// Body is ~730 lines but mechanically identical across kernels — the macro
/// keeps a single source of truth. The two const generics
/// (`ACCURATE_PRICE`, `FAVOR_SMALL_OFFSETS`) come from the wrapper's
/// generic parameter list and are referenced as bare identifiers; macro
/// hygiene resolves them at the expansion site.
/// Upstream zstd `offBase` for the btlazy2 lazy gain heuristic: a match whose offset
/// equals one of the three active repeat offsets prices as the cheap repcode
/// code (1/2/3); any other offset prices as `offset + 3`. So an equal-length
/// repeat-offset match always out-gains an explicit-offset one
/// (`zstd_lazy.c` `ZSTD_storeSeq` offBase convention).
#[inline]
fn btlazy2_offbase(offset: usize, reps: [u32; 3], ll0: bool) -> u32 {
    let o = offset as u32;
    // Upstream zstd repcode mapping shifts by `ll0` (zero-literal position): the cheap
    // codes become rep1 / rep2 / (rep0 - 1) instead of rep0 / rep1 / rep2,
    // because at ll0 an offset equal to rep0 is the special rep0-1 case, not
    // repcode 1. Scoring offsets against the wrong code at ll0 over-rewards a
    // rep0-distance match that does not actually encode as the cheapest code.
    if ll0 {
        if o == reps[1] {
            1
        } else if o == reps[2] {
            2
        } else if reps[0] > 1 && o == reps[0] - 1 {
            3
        } else {
            // Offsets are < window (<= 2^27), so `+ 3` never overflows u32.
            o + 3
        }
    } else if o == reps[0] {
        1
    } else if o == reps[1] {
        2
    } else if o == reps[2] {
        3
    } else {
        // Offsets are < window (<= 2^27), so `+ 3` never overflows u32.
        o + 3
    }
}

/// Upstream zstd lazy match gain (`matchLength * 4 - ZSTD_highbit32(offBase)`): the
/// selection metric that lets a shorter repeat-offset match beat a longer
/// explicit-offset one. `offBase >= 1`, so `highbit` is well-defined.
#[inline]
fn btlazy2_gain(match_len: usize, offset: usize, reps: [u32; 3], ll0: bool) -> i64 {
    let offbase = btlazy2_offbase(offset, reps, ll0);
    (match_len as i64) * 4 - (31 - offbase.leading_zeros()) as i64
}

/// Per-kernel body of the `btlazy2` (levels 13-15) greedy/lazy parse over
/// the binary-tree match finder. Mirrors `build_optimal_plan_impl_body!`'s
/// kernel-dispatch discipline: the wrapper carries the `#[target_feature]`
/// umbrella and passes its tier-specific `collect_optimal_candidates_initialized_<kernel>`
/// as `$collect`, so the per-position BT collect (and its inlined cpl)
/// stays under one umbrella — the runtime `select_kernel()` dispatch happens
/// ONCE per block in the bare `start_matching_btlazy2`, never per position.
macro_rules! start_matching_btlazy2_body {
    ($self:ident, $handle_sequence:ident, $collect:ident, $cmf:path $(,)?) => {{
        $self.table.ensure_tables();
        // Borrowed-aware: owned → last committed chunk; borrowed → staged block.
        let (current_abs_start, current_len) = $self.table.current_block_range();
        if current_len == 0 {
            return;
        }
        let current_ptr = $self.table.get_last_space().as_ptr();
        // Mutates tables but never reallocates `history`, so this tail slice
        // stays valid for the routine's duration (same as the other parsers).
        let current: &[u8] = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };
        // Full contiguous live region (owned: dict + prior blocks + current
        // block in `history`; borrowed: `[0, block_end)` of the in-place
        // input) as a raw slice, for the explicit repcode probe: a rep offset
        // can point before the current block, which `current` can't reach.
        // `live_history()` is borrowed-aware; reborrow-then-raw-ptr so the
        // slice holds NO borrow and coexists with the `&mut self` collector
        // calls below. Same no-realloc validity contract as `current`.
        let history_abs_start = $self.table.history_abs_start;
        let concat_full: &[u8] = unsafe {
            let lh = $self.table.live_history();
            core::slice::from_raw_parts(lh.as_ptr(), lh.len())
        };
        let current_abs_end = current_abs_start + current_len;
        $self
            .table
            .apply_limited_update_after_long_match(current_abs_start);
        $self
            .table
            .backfill_boundary_positions(current_abs_start, current_abs_end);

        let profile =
            HcOptimalCostProfile::const_for_strategy::<crate::encoding::strategy::Btlazy2>();
        let mut candidates = core::mem::take(&mut $self.backend.bt_mut().opt_candidates_scratch);

        let depth = $self.hc.lazy_depth as usize;
        let mut pos = 0usize;
        let mut literals_start = 0usize;

        // Collect + select the highest-GAIN match at a position (upstream zstd
        // `ZSTD_searchMax` plus the explicit offset_1 repcode check): scan the
        // length-sorted BT/dms ladder by gain, then probe rep0 directly since
        // the ladder's strictly-increasing-length filter drops short cheap
        // reps. Expands to `(match_len, offset)`; `match_len == 0` = no match.
        macro_rules! bt_select {
            ($p:expr) => {{
                let sel_pos: usize = $p;
                // `ll0` (upstream zstd): zero literals pending before this position, so
                // the repcode set is shifted (see `btlazy2_offbase`).
                let ll0 = sel_pos == literals_start;
                let sel_abs = current_abs_start + sel_pos;
                candidates.clear();
                let query = HcCandidateQuery {
                    reps: $self.table.offset_hist,
                    lit_len: sel_pos - literals_start,
                    // No LDM seed: L13-15 run at windowLog 22, below upstream zstd's
                    // LDM auto-enable threshold (windowLog >= 27).
                    ldm_candidate: None,
                };
                // SAFETY: called inside the wrapper's `#[target_feature]`
                // umbrella (the scalar wrapper's `$collect` is a safe fn).
                unsafe {
                    $self.$collect::<crate::encoding::strategy::Btlazy2, true>(
                        sel_abs,
                        current_abs_end,
                        profile,
                        query,
                        &mut candidates,
                    );
                }
                let reps = $self.table.offset_hist;
                let mut sel_ml = 0usize;
                let mut sel_off = 0usize;
                let mut sel_gain = i64::MIN;
                for c in candidates.iter() {
                    let ml = c.match_len.min(current_len - sel_pos);
                    if ml < HC_OPT_MIN_MATCH_LEN {
                        continue;
                    }
                    let g = btlazy2_gain(ml, c.offset, reps, ll0);
                    if g > sel_gain {
                        sel_gain = g;
                        sel_ml = ml;
                        sel_off = c.offset;
                    }
                }
                let sel_idx = sel_abs - history_abs_start;
                // Upstream zstd probes `rep[0 + ll0]` directly (the length-sorted ladder
                // drops short cheap reps): rep0 normally, rep1 at a zero-literal
                // position where rep0 is not the cheapest code.
                let probe_rep = if ll0 {
                    reps[1] as usize
                } else {
                    reps[0] as usize
                };
                if probe_rep != 0 && sel_idx >= probe_rep {
                    let tail = current_len - sel_pos;
                    // SAFETY: `sel_idx - probe_rep < sel_idx`, `sel_idx + tail <=
                    // concat_full.len()`; same overshoot slack the collector
                    // relies on for this block.
                    let rep_ml =
                        unsafe { $cmf(concat_full, sel_idx, sel_idx - probe_rep, tail, 0) };
                    if rep_ml >= HC_OPT_MIN_MATCH_LEN
                        && btlazy2_gain(rep_ml, probe_rep, reps, ll0) > sel_gain
                    {
                        sel_ml = rep_ml;
                        sel_off = probe_rep;
                    }
                }
                (sel_ml, sel_off)
            }};
        }

        while pos + HC_OPT_MIN_MATCH_LEN <= current_len {
            let (mut best_ml, mut best_off) = bt_select!(pos);
            if best_ml < HC_OPT_MIN_MATCH_LEN {
                pos += 1;
                continue;
            }
            // Lazy lookahead (upstream zstd depth 1/2): advance one byte and accept the
            // later match only if it out-gains the current one by the upstream zstd
            // margin (deferring costs an extra literal — `+4` at depth 1, `+7`
            // at depth 2). `start` tracks where the chosen match begins.
            let mut start = pos;
            let mut d = 0usize;
            while d < depth && start + 1 + HC_OPT_MIN_MATCH_LEN <= current_len {
                let look = start + 1;
                let (ml2, off2) = bt_select!(look);
                if ml2 < HC_OPT_MIN_MATCH_LEN {
                    break;
                }
                let reps = $self.table.offset_hist;
                let margin = if d == 0 { 4 } else { 7 };
                // `best` sits at `start` (ll0 iff no literals precede it); the
                // lookahead match at `start + 1` always has a pending literal.
                let gain1 = btlazy2_gain(best_ml, best_off, reps, start == literals_start) + margin;
                let gain2 = btlazy2_gain(ml2, off2, reps, false);
                if gain2 > gain1 {
                    best_ml = ml2;
                    best_off = off2;
                    start = look;
                    d += 1;
                } else {
                    break;
                }
            }
            // Commit the chosen match at `start`; [literals_start, start) is
            // emitted as literals. `best_ml` was bounded to `current_len -
            // start` at selection, so `start + best_ml <= current_len`.
            let lit_len = start - literals_start;
            let literals = &current[literals_start..start];
            $handle_sequence(Sequence::Triple {
                literals,
                offset: best_off,
                match_len: best_ml,
            });
            let _ = encode_offset_with_history(
                best_off as u32,
                lit_len as u32,
                &mut $self.table.offset_hist,
            );
            pos = start + best_ml;
            literals_start = pos;
        }

        if literals_start < current_len {
            $handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
        $self.backend.bt_mut().opt_candidates_scratch = candidates;
    }};
}

macro_rules! build_optimal_plan_impl_body {
    (
        $self:expr,
        $strategy_ty:ty,
        $current:ident,
        $current_abs_start:ident,
        $current_len:ident,
        $initial_state:ident,
        $stats:ident,
        $out:ident,
        $collect:ident,
        $priceset:path $(,)?
    ) => {{
        let current_abs_end = $current_abs_start + $current_len;
        let min_match_len = HC_OPT_MIN_MATCH_LEN;
        // `HC_OPT_NUM > 0` by const definition, so `HC_OPT_NUM - 1` is safe.
        let frontier_limit = $current_len.min(HC_OPT_NUM - 1);
        let initial_reps = $initial_state.reps;
        let initial_litlen = $initial_state.litlen;
        let ldm_block_offset = $initial_state.block_offset;
        let mut profile = $initial_state.profile;
        profile.sufficient_match_len = $self.hc.sufficient_match_len_for_pass(profile);
        // Const-fold from the strategy's associated `OPT_LEVEL`
        // (upstream zstd `optLevel`): BtOpt = 0, BtUltra / BtUltra2 = 2.
        // The two flags below are the only places the inner DP loop
        // used to consult `parse_mode`; lifting them into const
        // expressions drops one indirect read + one branch on every
        // candidate insertion and every traceback step.
        // `let` (not `const`) — nested `const` items inside a
        // generic fn cannot project through the outer fn's type
        // parameter, but a `let` binding from a const expression
        // does get folded by the optimiser per monomorphisation,
        // which is what we actually want here.
        debug_assert!(
            <$strategy_ty as crate::encoding::strategy::Strategy>::USE_BT,
            "build_optimal_plan_impl_body called on non-BT strategy"
        );
        let abort_on_worse_match: bool =
            <$strategy_ty as crate::encoding::strategy::Strategy>::OPT_LEVEL == 0;
        let opt_level: bool = <$strategy_ty as crate::encoding::strategy::Strategy>::OPT_LEVEL >= 2;
        let mut nodes = core::mem::take(&mut $self.backend.bt_mut().opt_nodes_scratch);
        let mut node_prices = core::mem::take(&mut $self.backend.bt_mut().opt_node_prices_scratch);
        // `frontier_limit + 2 <= HC_OPT_NODE_LEN` — bounded by const.
        let frontier_buffer_size = frontier_limit + 2;
        if nodes.len() < HC_OPT_NODE_LEN {
            // First optimal-parse use (empty boxed slice) or an undersized
            // buffer: allocate the fixed upstream-zstd-sized frontier once. The DP
            // overwrites the active prefix before reading it.
            nodes = alloc::vec![HcOptimalNode::default(); HC_OPT_NODE_LEN].into_boxed_slice();
        }
        // The DP price array, same fixed length as `nodes`. This is the SOLE
        // home of each position's price (the node struct carries no price), so
        // the SIMD price-set vector-loads it directly. Initialised to u32::MAX
        // so unwritten frontier cells compare as "unreachable".
        if node_prices.len() < HC_OPT_NODE_LEN {
            node_prices = alloc::vec![u32::MAX; HC_OPT_NODE_LEN].into_boxed_slice();
        }
        let mut candidates = core::mem::take(&mut $self.backend.bt_mut().opt_candidates_scratch);
        candidates.clear();
        if candidates.capacity() < MAX_HC_SEARCH_DEPTH {
            candidates.reserve_exact(MAX_HC_SEARCH_DEPTH - candidates.capacity());
        }
        let mut store = core::mem::take(&mut $self.backend.bt_mut().opt_store_scratch);
        store.clear();
        let mut price_arena = core::mem::take(&mut $self.backend.bt_mut().opt_price_arena);
        if price_arena.len() < HC_OPT_PRICE_ARENA_LEN {
            price_arena = alloc::vec![[0u32; 2]; HC_OPT_PRICE_ARENA_LEN].into_boxed_slice();
        }
        // Single arena → two disjoint fixed-stride regions of `[price,
        // generation]` pairs (LL cache, ML cache): one base pointer + fixed
        // offsets, mirroring upstream zstd's single opt workspace. Pairing
        // price+generation per code keeps the optimal parser's cache probe
        // on ONE line instead of two strided regions.
        // SAFETY: `price_arena` is exactly `HC_OPT_PRICE_ARENA_LEN =
        // 2 * HC_OPT_PRICE_STRIDE` pairs long (just ensured), so the two
        // STRIDE-wide regions are in bounds and disjoint. The slices alias
        // the heap buffer `price_arena` owns; that heap address is stable
        // across the later move of the `price_arena` box into the result
        // bundle (a `Box` move relocates only the pointer, not the heap
        // data), and the slices are never used after the bundle is
        // constructed. The fixed STRIDE (independent of `frontier_limit`)
        // keeps every code's cell at a constant offset so the monotonic
        // stamps stay valid across calls with different frontiers.
        let arena_base = price_arena.as_mut_ptr();
        let mut ll_cache: &mut [[u32; 2]] =
            unsafe { core::slice::from_raw_parts_mut(arena_base, HC_OPT_PRICE_STRIDE) };
        let mut ml_cache: &mut [[u32; 2]] = unsafe {
            core::slice::from_raw_parts_mut(arena_base.add(HC_OPT_PRICE_STRIDE), HC_OPT_PRICE_STRIDE)
        };
        $self.backend.bt_mut().opt_ll_price_stamp = $self
            .backend
            .bt_mut()
            .opt_ll_price_stamp
            .wrapping_add(1)
            .max(1);
        let ll_price_stamp = $self.backend.bt_mut().opt_ll_price_stamp;
        $self.backend.bt_mut().opt_lit_price_stamp = $self
            .backend
            .bt_mut()
            .opt_lit_price_stamp
            .wrapping_add(1)
            .max(1);
        let lit_price_stamp = $self.backend.bt_mut().opt_lit_price_stamp;
        $self.backend.bt_mut().opt_ml_price_stamp = $self
            .backend
            .bt_mut()
            .opt_ml_price_stamp
            .wrapping_add(1)
            .max(1);
        let ml_price_stamp = $self.backend.bt_mut().opt_ml_price_stamp;
        let node0_price = BtMatcher::cached_lit_length_price(
            profile,
            $stats,
            initial_litlen,
            &mut ll_cache,
            ll_price_stamp,
        );
        nodes[0] = HcOptimalNode {
            litlen: initial_litlen as u32,
            reps: initial_reps,
            ..HcOptimalNode::default()
        };
        node_prices[0] = node0_price;
        let sufficient_len = profile.sufficient_match_len;
        let ll0_price = BtMatcher::cached_lit_length_price(
            profile,
            $stats,
            0,
            &mut ll_cache,
            ll_price_stamp,
        );
        let ll1_price = BtMatcher::cached_lit_length_price(
            profile,
            $stats,
            1,
            &mut ll_cache,
            ll_price_stamp,
        );
        let mut pos = 1usize;
        let mut last_pos = 0usize;
        let mut forced_end: Option<usize> = None;
        let mut forced_end_state: Option<HcOptimalNode> = None;
        // Price companion of `forced_end_state` (price no longer lives in the
        // node struct; tracked alongside the forced-seed node).
        let mut forced_end_price: Option<u32> = None;
        let mut seed_forced_shortest_path = false;
        let mut opt_ldm = HcOptLdmState {
            seq_store: HcRawSeqStore {
                pos: 0,
                pos_in_sequence: 0,
                size: $self.backend.bt_mut().ldm_sequences.len(),
            },
            ..HcOptLdmState::default()
        };
        let has_ldm = !$self.backend.bt_mut().ldm_sequences.is_empty();
        if has_ldm {
            // `ldm_sequences` are emitted in BLOCK-relative coordinates,
            // but this optimal-parser pass runs over a SEGMENT of the
            // block starting at block-offset `$block_offset` and uses
            // segment-relative positions throughout. Fast-forward the raw
            // seq-store cursor past the bytes covered by earlier segments
            // so the (segment-relative) LDM windows below land at the
            // correct positions. Idempotent: `ldm_skip_raw_seq_store_bytes`
            // recomputes from `pos = 0`, so re-running it per segment is
            // safe. Without this, every segment after the first re-applied
            // the block's leading LDM windows at the wrong offset, emitting
            // matches that copy the wrong bytes (undecodable frame).
            if ldm_block_offset > 0 {
                $self
                    .backend
                    .bt_mut()
                    .ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, ldm_block_offset);
            }
            $self
                .backend
                .bt_mut()
                .ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 0, $current_len);
        }

        // Upstream zstd-like seed at rPos=0: initialize frontier with matches starting
        // at current position before entering the generic forward DP loop.
        if $current_len >= min_match_len {
            let seed_ldm = if has_ldm {
                $self.backend.bt_mut().ldm_process_match_candidate(
                    &mut opt_ldm,
                    0,
                    $current_len,
                    min_match_len,
                )
            } else {
                None
            };
            candidates.clear();
            // SAFETY: wrapper is in the same target_feature umbrella as the
            // `$collect` kernel variant; the runtime kernel detector already
            // gated entry into the wrapper.
            unsafe {
                $self.$collect::<$strategy_ty, true>(
                    $current_abs_start,
                    current_abs_end,
                    profile,
                    HcCandidateQuery {
                        reps: initial_reps,
                        lit_len: initial_litlen,
                        ldm_candidate: seed_ldm,
                    },
                    &mut candidates,
                )
            };
            if !candidates.is_empty() {
                // `min_match_len >= HC_FORMAT_MINMATCH (3)` by invariant.
                last_pos = (min_match_len - 1).min(frontier_limit);
                for p in 1..min_match_len.min(frontier_buffer_size) {
                    BtMatcher::reset_opt_node(&mut nodes[p]);
                    // Reset the price (sole home; the node carries none).
                    node_prices[p] = u32::MAX;
                    // `initial_litlen` is the litlen carried from prior
                    // optimal-plan segments — its real bound is the
                    // current block length (the frame compressor caps
                    // block scan at `HC_BLOCKSIZE_MAX`), not the segment
                    // `current_len`. `p < min_match_len` (small constant),
                    // so the sum stays well within `u32::MAX`. Use
                    // `checked_add` FIRST so the `usize` addition itself
                    // cannot overflow on i686 (where `usize` is 32-bit
                    // and a wrapping `+` would slip past `try_from`).
                    let seed_litlen = initial_litlen
                        .checked_add(p)
                        .and_then(|s| u32::try_from(s).ok())
                        .expect("optimal parser seed litlen out of u32 range");
                    nodes[p].litlen = seed_litlen;
                }
            }

            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min($current_len);
                if longest_len > sufficient_len {
                    let off_base = BtMatcher::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    let ml_price = BtMatcher::cached_match_length_price(
                        profile,
                        $stats,
                        longest_len,
                        &mut ml_cache,
                        ml_price_stamp,
                    );
                    let seq_cost = BtMatcher::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let forced_price = BtMatcher::add_prices(node_prices[0], seq_cost);
                    let forced_state = HcOptimalNode {
                        off: candidate.offset as u32,
                        mlen: longest_len as u32,
                        litlen: 0,
                        reps: initial_reps,
                    };
                    if longest_len < frontier_buffer_size && forced_price < node_prices[longest_len] {
                        nodes[longest_len] = forced_state;
                        node_prices[longest_len] = forced_price;
                    }
                    forced_end = Some(longest_len);
                    forced_end_state = Some(forced_state);
                    forced_end_price = Some(forced_price);
                    seed_forced_shortest_path = true;
                }
            }
            if !seed_forced_shortest_path {
                let mut prev_max_len = min_match_len - 1;
                for candidate in candidates.iter() {
                    let max_match_len = candidate.match_len.min(frontier_limit);
                    if max_match_len < min_match_len {
                        continue;
                    }
                    let start_len = (prev_max_len + 1).max(min_match_len);
                    if start_len > max_match_len {
                        prev_max_len = prev_max_len.max(max_match_len);
                        continue;
                    }
                    if max_match_len > last_pos {
                        BtMatcher::reset_opt_nodes(
                            &mut nodes,
                            &mut node_prices,
                            last_pos + 1,
                            max_match_len,
                        );
                    }
                    let off_base = BtMatcher::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        initial_litlen,
                        initial_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    debug_assert!(max_match_len < frontier_buffer_size);
                    let nodes0_price = node_prices[0];
                    for match_len in (start_len..=max_match_len).rev() {
                        let ml_price = BtMatcher::cached_match_length_price(
                            profile,
                            $stats,
                            match_len,
                            &mut ml_cache,
                            ml_price_stamp,
                        );
                        let seq_cost = BtMatcher::add_prices(
                            ll0_price,
                            profile.match_price_from_parts(off_price, ml_price, $stats),
                        );
                        let next_cost = BtMatcher::add_prices(nodes0_price, seq_cost);
                        let node_price = unsafe { *node_prices.get_unchecked(match_len) };
                        if match_len > last_pos || next_cost < node_price {
                            let slot = unsafe { nodes.get_unchecked_mut(match_len) };
                            *slot = HcOptimalNode {
                                off: candidate.offset as u32,
                                mlen: match_len as u32,
                                litlen: 0,
                                reps: initial_reps,
                            };
                            unsafe { *node_prices.get_unchecked_mut(match_len) = next_cost };
                            if match_len > last_pos {
                                last_pos = match_len;
                            }
                        } else if abort_on_worse_match {
                            break;
                        }
                    }
                    prev_max_len = prev_max_len.max(max_match_len);
                }
                if last_pos + 1 < frontier_buffer_size {
                    node_prices[last_pos + 1] = u32::MAX;
                }
            }
        }
        while !seed_forced_shortest_path && pos <= last_pos && pos <= frontier_limit {
            debug_assert!(pos + 1 < frontier_buffer_size);
            let prev_node = unsafe { *nodes.get_unchecked(pos - 1) };
            let prev_node_price = unsafe { *node_prices.get_unchecked(pos - 1) };
            if prev_node_price != u32::MAX {
                let lit_len = prev_node.litlen as usize + 1;
                let lit_price = {
                    let bt = $self.backend.bt_mut();
                    BtMatcher::cached_literal_price(
                        profile,
                        $stats,
                        $current[pos - 1],
                        &mut bt.opt_lit_price_scratch,
                        &mut bt.opt_lit_price_generation,
                        lit_price_stamp,
                    )
                };
                let ll_delta = BtMatcher::cached_lit_length_delta_price(
                    profile,
                    $stats,
                    lit_len,
                    &mut ll_cache,
                    ll_price_stamp,
                );
                let lit_cost = BtMatcher::add_price_delta(prev_node_price, lit_price, ll_delta);
                // `node_pos_price` is the OLD price at `pos` (before the write
                // below) — also the price of `prev_match`, the pre-overwrite copy.
                let node_pos_price = unsafe { *node_prices.get_unchecked(pos) };
                if lit_cost <= node_pos_price {
                    let prev_match = unsafe { *nodes.get_unchecked(pos) };
                    let slot = unsafe { nodes.get_unchecked_mut(pos) };
                    *slot = prev_node;
                    slot.litlen = lit_len as u32;
                    node_prices[pos] = lit_cost;
                    #[allow(clippy::collapsible_if)]
                    if opt_level
                        && prev_match.mlen > 0
                        && prev_match.litlen == 0
                        && pos < $current_len
                    {
                        if ll1_price < ll0_price {
                            let next_lit_price = {
                                let bt = $self.backend.bt_mut();
                                BtMatcher::cached_literal_price(
                                    profile,
                                    $stats,
                                    $current[pos],
                                    &mut bt.opt_lit_price_scratch,
                                    &mut bt.opt_lit_price_generation,
                                    lit_price_stamp,
                                )
                            };
                            let with1literal = BtMatcher::add_price_delta(
                                node_pos_price,
                                next_lit_price,
                                ll1_price as i32 - ll0_price as i32,
                            );
                            let ll_delta_next = BtMatcher::cached_lit_length_delta_price(
                                profile,
                                $stats,
                                lit_len + 1,
                                &mut ll_cache,
                                ll_price_stamp,
                            );
                            let with_more_literals =
                                BtMatcher::add_price_delta(lit_cost, next_lit_price, ll_delta_next);
                            let next = pos + 1;
                            let next_price = unsafe { *node_prices.get_unchecked(next) };
                            if with1literal < with_more_literals && with1literal < next_price {
                                // Upstream zstd parity (zstd_opt.c:1232): `cur >= prevMatch.mlen`.
                                debug_assert!(pos >= prev_match.mlen as usize);
                                let prev_pos = pos - prev_match.mlen as usize;
                                {
                                    let prev_state = unsafe { *nodes.get_unchecked(prev_pos) };
                                    let (_, reps_after_match) = BtMatcher::encode_offset_with_reps(
                                        prev_match.off,
                                        prev_state.litlen as usize,
                                        prev_state.reps,
                                    );
                                    let slot = unsafe { nodes.get_unchecked_mut(next) };
                                    *slot = prev_match;
                                    slot.reps = reps_after_match;
                                    slot.litlen = 1;
                                    node_prices[next] = with1literal;
                                    if next > last_pos {
                                        last_pos = next;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Memory-resident DP (upstream zstd parity): read opt[cur] fields on
            // demand instead of holding a 28-byte node copy live across the
            // per-position `$collect` call below. The held copy forced LLVM
            // to spill reps[3] + litlen around the (non-inlinable) call;
            // reading the fields fresh on each side keeps them out of the
            // cross-call live set. `nodes[pos]` is stable across `$collect`
            // (it only fills `candidates`), so post-call reads are identical.
            let base_cost = unsafe { *node_prices.get_unchecked(pos) };
            if base_cost == u32::MAX {
                pos += 1;
                continue;
            }
            {
                let base_node = unsafe { *nodes.get_unchecked(pos) };
                if base_node.mlen > 0 && base_node.litlen == 0 {
                    // Upstream zstd parity (zstd_opt.c:1255): `cur >= opt[cur].mlen`.
                    debug_assert!(pos >= base_node.mlen as usize);
                    let prev_pos = pos - base_node.mlen as usize;
                    let prev_state = unsafe { *nodes.get_unchecked(prev_pos) };
                    let (_, reps_after_match) = BtMatcher::encode_offset_with_reps(
                        base_node.off,
                        prev_state.litlen as usize,
                        prev_state.reps,
                    );
                    unsafe { nodes.get_unchecked_mut(pos).reps = reps_after_match };
                }
            }

            if pos + 8 > $current_len {
                pos += 1;
                continue;
            }

            if pos == last_pos {
                break;
            }

            let next_price = unsafe { *node_prices.get_unchecked(pos + 1) };
            // `saturating_add` is REQUIRED here, not a masked bug: `base_cost`
            // is a node price that can be the `u32::MAX` "unreachable" sentinel,
            // and saturating keeps `base_cost + margin` pinned at MAX so the
            // comparison stays correct. Plain `+` would wrap the sentinel and
            // flip the abort decision (a ratio bug / debug overflow panic).
            if abort_on_worse_match
                && next_price <= base_cost.saturating_add(HC_BITCOST_MULTIPLIER / 2)
            {
                pos += 1;
                continue;
            }

            let abs_pos = $current_abs_start + pos;
            let ldm_candidate = if has_ldm {
                $self.backend.bt_mut().ldm_process_match_candidate(
                    &mut opt_ldm,
                    pos,
                    $current_len - pos,
                    min_match_len,
                )
            } else {
                None
            };
            candidates.clear();
            // SAFETY: same umbrella as `$collect`. Query fields are read
            // fresh here (consumed into the call's argument) so they do not
            // stay live across the call; the post-call reads below are a
            // separate, fresh load of the same stable `nodes[pos]`.
            unsafe {
                $self.$collect::<$strategy_ty, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    HcCandidateQuery {
                        reps: nodes.get_unchecked(pos).reps,
                        lit_len: nodes.get_unchecked(pos).litlen as usize,
                        ldm_candidate,
                    },
                    &mut candidates,
                )
            };
            // Post-call reads of opt[cur]: fresh, born after `$collect`, so
            // never part of the cross-call live set (see memory-resident note
            // above). `nodes[pos]` is untouched by `$collect`.
            let base_reps = unsafe { nodes.get_unchecked(pos).reps };
            let base_litlen = unsafe { nodes.get_unchecked(pos).litlen as usize };
            if let Some(candidate) = candidates.last() {
                let longest_len = candidate.match_len.min($current_len - pos);
                if longest_len > sufficient_len
                    || pos + longest_len >= HC_OPT_NUM
                    || pos + longest_len >= $current_len
                {
                    let lit_len = base_litlen;
                    let off_base = BtMatcher::encode_offset_base_with_reps(
                        candidate.offset as u32,
                        lit_len,
                        base_reps,
                    );
                    let off_price = profile
                        .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                    let ml_price = BtMatcher::cached_match_length_price(
                        profile,
                        $stats,
                        longest_len,
                        &mut ml_cache,
                        ml_price_stamp,
                    );
                    let seq_cost = BtMatcher::add_prices(
                        ll0_price,
                        profile.match_price_from_parts(off_price, ml_price, $stats),
                    );
                    let forced_price = BtMatcher::add_prices(base_cost, seq_cost);
                    let end_pos = (pos + longest_len).min($current_len);
                    forced_end = Some(end_pos);
                    forced_end_state = Some(HcOptimalNode {
                        off: candidate.offset as u32,
                        mlen: longest_len as u32,
                        litlen: 0,
                        reps: base_reps,
                    });
                    forced_end_price = Some(forced_price);
                    break;
                }
            }
            let mut prev_max_len = min_match_len - 1;
            for candidate in candidates.iter() {
                // Outer loop guards `pos <= frontier_limit` (see the
                // `while ... pos <= frontier_limit` condition); the
                // subtraction below is therefore safe.
                debug_assert!(pos <= frontier_limit);
                let max_match_len = candidate
                    .match_len
                    .min($current_len - pos)
                    .min(frontier_limit - pos);
                let min_len = min_match_len;
                if max_match_len < min_len {
                    continue;
                }
                let start_len = (prev_max_len + 1).max(min_len);
                if start_len > max_match_len {
                    prev_max_len = prev_max_len.max(max_match_len);
                    continue;
                }
                let max_next = pos + max_match_len;
                if max_next > last_pos {
                    BtMatcher::reset_opt_nodes(
                        &mut nodes,
                        &mut node_prices,
                        last_pos + 1,
                        max_next,
                    );
                }
                let lit_len = base_litlen;
                let off_base = BtMatcher::encode_offset_base_with_reps(
                    candidate.offset as u32,
                    lit_len,
                    base_reps,
                );
                let off_price = profile
                    .offset_price_for::<ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>($stats, off_base);
                debug_assert!(pos + max_match_len < frontier_buffer_size);
                if abort_on_worse_match {
                    // btopt (OPT_LEVEL == 0): reverse-iterate with early break —
                    // once a longer match stops improving, shorter ones are
                    // skipped. Order-dependent, stays scalar.
                    for match_len in (start_len..=max_match_len).rev() {
                        let next = pos + match_len;
                        let ml_price = BtMatcher::cached_match_length_price(
                            profile,
                            $stats,
                            match_len,
                            &mut ml_cache,
                            ml_price_stamp,
                        );
                        let seq_cost = BtMatcher::add_prices(
                            ll0_price,
                            profile.match_price_from_parts(off_price, ml_price, $stats),
                        );
                        let next_cost = BtMatcher::add_prices(base_cost, seq_cost);
                        let node_next_price = unsafe { *node_prices.get_unchecked(next) };
                        if next > last_pos || next_cost < node_next_price {
                            let slot = unsafe { nodes.get_unchecked_mut(next) };
                            *slot = HcOptimalNode {
                                off: candidate.offset as u32,
                                mlen: match_len as u32,
                                litlen: 0,
                                reps: base_reps,
                            };
                            unsafe { *node_prices.get_unchecked_mut(next) = next_cost };
                            if next > last_pos {
                                last_pos = next;
                            }
                        } else {
                            break;
                        }
                    }
                } else {
                    // btultra / btultra2 (OPT_LEVEL >= 2): no abort, each
                    // match_len writes a distinct node => order-independent.
                    // Dispatch to the per-tier price-set ($priceset is the
                    // tier's fn: AVX2 SoA-vector compare for the avx2 wrapper,
                    // inline scalar otherwise) — it folds into this wrapper's
                    // monomorphisation, so no call ABI / runtime feature check.
                    #[allow(unused_unsafe)]
                    {
                        last_pos = last_pos.max(unsafe {
                            $priceset(
                                &mut node_prices,
                                &mut nodes,
                                ml_cache,
                                ml_price_stamp,
                                profile,
                                $stats,
                                pos,
                                start_len,
                                max_match_len,
                                ll0_price,
                                off_price,
                                base_cost,
                                candidate.offset as u32,
                                base_reps,
                                last_pos,
                            )
                        });
                    }
                }
                prev_max_len = prev_max_len.max(max_match_len);
            }

            if last_pos + 1 < frontier_buffer_size {
                unsafe {
                    *node_prices.get_unchecked_mut(last_pos + 1) = u32::MAX;
                }
            }
            pos += 1;
        }

        if last_pos == 0 {
            if $current_len == 0 {
                let price = node_prices[0];
                return $self.backend.bt_mut().finish_optimal_plan(
                    HcOptimalPlanBuffers {
                        nodes,
                        node_prices,
                        candidates,
                        store,
                        price_arena,
                    },
                    (price, initial_reps, initial_litlen, 0),
                );
            }
            let lit_price = {
                let bt = $self.backend.bt_mut();
                BtMatcher::cached_literal_price(
                    profile,
                    $stats,
                    $current[0],
                    &mut bt.opt_lit_price_scratch,
                    &mut bt.opt_lit_price_generation,
                    lit_price_stamp,
                )
            };
            // `initial_litlen` is carried across optimal-plan segments;
            // its real bound is the current block length, not
            // `current_len`. On i686 (32-bit `usize`) `+ 1` could
            // theoretically wrap if the invariant ever broke. Catch
            // that explicitly via `checked_add` rather than letting a
            // wrapping sum slip into the price lookup.
            let next_litlen = initial_litlen
                .checked_add(1)
                .expect("optimal parser next litlen out of usize range");
            let ll_delta = BtMatcher::cached_lit_length_delta_price(
                profile,
                $stats,
                next_litlen,
                &mut ll_cache,
                ll_price_stamp,
            );
            let price = BtMatcher::add_price_delta(node_prices[0], lit_price, ll_delta);
            return $self.backend.bt_mut().finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    node_prices,
                    candidates,
                    store,
                    price_arena,
                },
                (price, initial_reps, next_litlen, 1),
            );
        }

        let target_pos = forced_end.unwrap_or(last_pos.min(frontier_limit));
        // Price lives in `node_prices`, not the node struct, so carry the
        // final-stretch price alongside its node (forced-seed companion or the
        // frontier price at `target_pos`).
        let (last_stretch, last_stretch_price) = if let Some(forced_state) = forced_end_state {
            (forced_state, forced_end_price.expect("forced state has a price"))
        } else {
            (nodes[target_pos], node_prices[target_pos])
        };
        if last_stretch_price == u32::MAX {
            return $self.backend.bt_mut().finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    node_prices,
                    candidates,
                    store,
                    price_arena,
                },
                (u32::MAX, initial_reps, initial_litlen, $current_len),
            );
        }

        if last_stretch.mlen == 0 {
            return $self.backend.bt_mut().finish_optimal_plan(
                HcOptimalPlanBuffers {
                    nodes,
                    node_prices,
                    candidates,
                    store,
                    price_arena,
                },
                (
                    last_stretch_price,
                    last_stretch.reps,
                    last_stretch.litlen as usize,
                    target_pos.min($current_len),
                ),
            );
        }

        let mut cur = target_pos.saturating_sub(last_stretch.mlen as usize);
        let end_reps = if last_stretch.litlen == 0 {
            let prev_state = nodes[cur];
            let (_, reps_after_match) = BtMatcher::encode_offset_with_reps(
                last_stretch.off,
                prev_state.litlen as usize,
                prev_state.reps,
            );
            reps_after_match
        } else {
            let tail_literals = last_stretch.litlen as usize;
            if cur < tail_literals {
                return $self.backend.bt_mut().finish_optimal_plan(
                    HcOptimalPlanBuffers {
                        nodes,
                        node_prices,
                        candidates,
                        store,
                        price_arena,
                    },
                    (
                        last_stretch_price,
                        last_stretch.reps,
                        tail_literals,
                        target_pos.min($current_len),
                    ),
                );
            }
            cur -= tail_literals;
            last_stretch.reps
        };
        let store_end = cur + 2;
        if store.len() <= store_end {
            store.resize(store_end + 1, HcOptimalNode::default());
        }
        let mut store_start;
        let mut stretch_pos = cur;

        if last_stretch.litlen > 0 {
            store[store_end] = HcOptimalNode {
                litlen: last_stretch.litlen,
                mlen: 0,
                ..HcOptimalNode::default()
            };
            store_start = store_end.saturating_sub(1);
            store[store_start] = last_stretch;
        }
        store[store_end] = last_stretch;
        store_start = store_end;

        loop {
            let next_stretch = nodes[stretch_pos];
            store[store_start].litlen = next_stretch.litlen;
            if next_stretch.mlen == 0 {
                break;
            }
            if store_start == 0 {
                break;
            }
            store_start -= 1;
            store[store_start] = next_stretch;
            // Parser invariant: every emitted stretch is bounded by the
            // current block, so `litlen + mlen <= current_len <=
            // HC_BLOCKSIZE_MAX (128 KiB)`. The `as usize` widening + raw
            // `+` is safe on 32-bit targets — two u32 values do NOT
            // automatically fit in `usize` on i686, the block bound is
            // what makes this addition safe.
            let litlen = next_stretch.litlen as usize;
            let mlen = next_stretch.mlen as usize;
            debug_assert!(litlen + mlen <= $current_len);
            let step = litlen + mlen;
            if step == 0 || stretch_pos < step {
                break;
            }
            stretch_pos -= step;
        }

        let mut tail_literals = initial_litlen;
        let mut store_pos = store_start;
        while store_pos <= store_end {
            let stretch = store[store_pos];
            let llen = stretch.litlen as usize;
            let mlen = stretch.mlen as usize;
            if mlen == 0 {
                tail_literals = llen;
                store_pos += 1;
                continue;
            }
            $out.push(HcOptimalSequence {
                offset: stretch.off,
                match_len: mlen as u32,
                lit_len: llen as u32,
            });
            tail_literals = 0;
            store_pos += 1;
        }
        let result = (
            last_stretch_price,
            end_reps,
            if last_stretch.litlen > 0 {
                last_stretch.litlen as usize
            } else {
                tail_literals
            },
            target_pos.min($current_len),
        );
        $self.backend.bt_mut().finish_optimal_plan(
            HcOptimalPlanBuffers {
                nodes,
                node_prices,
                candidates,
                store,
                price_arena,
            },
            result,
        )
    }};
}

/// `collect_optimal_candidates_initialized` body parameterized over the per-CPU
/// kernel: the `$cpl` path is the kernel's `common_prefix_len_ptr` (used in
/// the HC chain walk fallback), and the four method-name substitutions
/// (`$bt_update`, `$bt_insert`, `$for_each_rep`, `$hash3`) route to the
/// kernel-specific wrappers of the inner helpers. With every helper under
/// the same `target_feature` umbrella, the entire per-position pipeline
/// (BT-tree fill + rep probing + hash3 probing + BT match collection /
/// HC chain walk) inlines without ABI barriers on the level22 hot path.
macro_rules! collect_optimal_candidates_initialized_body {
    (
        $self:expr,
        $strategy_ty:ty,
        $abs_pos:ident,
        $current_abs_end:ident,
        $profile:ident,
        $query:ident,
        $out:ident,
        $bt_matchfinder:ident,
        $bt_update:ident,
        $bt_insert:ident,
        $for_each_rep:ident,
        $hash3:ident,
        $cpl:path $(,)?
    ) => {{
        // Per-strategy compile-time const: only BtUltra2 drives the
        // hash3 short-match table. All other monomorphisations drop
        // the entire hash3 lookup block at codegen time. The relaxed
        // implication enforces only the direction we depend on:
        // if the strategy declares hash3, the table must be live.
        // The reverse (`hash3_log != 0` without `USE_HASH3`) is OK —
        // a future caller may pre-allocate hash3 storage without
        // wiring the BtUltra2 path through.
        let use_hash3: bool = <$strategy_ty as crate::encoding::strategy::Strategy>::USE_HASH3;
        debug_assert!(!$self.table.hash_table.is_empty());
        debug_assert!($self.table.hash3_log == 0 || !$self.table.hash3_table.is_empty());
        debug_assert!(
            !use_hash3 || $self.table.hash3_log != 0,
            "Strategy::USE_HASH3 = true but runtime hash3_log is 0 — call configure() first",
        );
        debug_assert!(!$self.table.chain_table.is_empty());
        let min_match_len = HC_OPT_MIN_MATCH_LEN;
        let reps = $query.reps;
        let lit_len = $query.lit_len;
        let ldm_candidate = $query.ldm_candidate;
        $out.clear();
        if $abs_pos < $self.table.skip_insert_until_abs {
            if let Some(ldm) = ldm_candidate {
                let mut best_len_for_skip = 0usize;
                let _ = crate::encoding::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    ldm,
                    min_match_len,
                );
            }
            return;
        }
        if $bt_matchfinder {
            // SAFETY: caller is in the same target_feature umbrella as
            // `$bt_update`; the runtime kernel detector already gated entry.
            unsafe { $self.table.$bt_update($abs_pos, $current_abs_end) };
        }
        let current_idx = $abs_pos - $self.table.history_abs_start;
        if current_idx + 4 > $self.table.live_history().len() {
            if let Some(ldm) = ldm_candidate {
                let mut best_len_for_skip = 0usize;
                let _ = crate::encoding::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    ldm,
                    min_match_len,
                );
            }
            return;
        }
        let mut best_len_for_skip = 0usize;
        let mut skip_further_match_search = false;
        let mut rep_len_candidate_found = false;
        // SAFETY: same umbrella; closure capture is monomorphized per call.
        unsafe {
            $self.hc.$for_each_rep(
                &$self.table,
                $abs_pos,
                lit_len,
                reps,
                $current_abs_end,
                min_match_len,
                |rep| {
                    if rep.match_len >= min_match_len {
                        rep_len_candidate_found = true;
                    }
                    let _ = crate::encoding::bt::BtMatcher::push_candidate_ladder(
                        $out,
                        &mut best_len_for_skip,
                        rep,
                        min_match_len,
                    );
                    if rep.match_len > $profile.sufficient_match_len {
                        skip_further_match_search = true;
                    }
                    // `for_each_repcode_candidate_with_reps` caps
                    // `rep.match_len` at the per-call `tail_limit =
                    // current_abs_end - abs_pos`, so `abs_pos +
                    // rep.match_len <= current_abs_end`. The raw sum
                    // therefore stays in `usize` on every supported
                    // target.
                    if $abs_pos + rep.match_len >= $current_abs_end {
                        skip_further_match_search = true;
                    }
                },
            )
        };
        // Hash3 lookup runs only when the strategy enables it. The
        // `use_hash3` binding above is a per-monomorphisation const,
        // so non-BtUltra2 instances drop this entire block.
        if use_hash3 && !skip_further_match_search && best_len_for_skip < min_match_len {
            $self.table.update_hash3_until($abs_pos);
            // SAFETY: same umbrella for hash3_candidate.
            if let Some(h3) = unsafe {
                $self
                    .table
                    .$hash3($abs_pos, $current_abs_end, min_match_len)
            } {
                let _ = crate::encoding::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    &mut best_len_for_skip,
                    h3,
                    min_match_len,
                );
                if !rep_len_candidate_found
                    && (h3.match_len > $profile.sufficient_match_len
                        || $abs_pos + h3.match_len >= $current_abs_end)
                {
                    $self.table.skip_insert_until_abs = $abs_pos + 1;
                    skip_further_match_search = true;
                }
            }
        }
        if !skip_further_match_search && $bt_matchfinder {
            // SAFETY: same umbrella for bt_insert_and_collect_matches.
            unsafe {
                $self.table.$bt_insert(
                    $abs_pos,
                    $current_abs_end,
                    $profile,
                    min_match_len,
                    &mut best_len_for_skip,
                    $out,
                )
            };
        } else if !skip_further_match_search {
            $self.table.insert_position($abs_pos);
            let max_chain_depth = $profile.max_chain_depth.min($self.hc.search_depth);
            let concat = $self.table.live_history();
            // Raw `+ 9` is safe here — see `bt_insert_step_no_rebase_body!`
            // for the full discussion of the upstream `STREAM_ABS_HEADROOM`
            // cap in `MatchTable::add_data`.
            let mut match_end_abs = $abs_pos + 9;
            if max_chain_depth > 0 {
                for (visited, candidate_abs) in $self
                    .hc
                    .chain_candidates(&$self.table, $abs_pos)
                    .into_iter()
                    .enumerate()
                {
                    if visited >= max_chain_depth {
                        break;
                    }
                    if candidate_abs == usize::MAX {
                        break;
                    }
                    if candidate_abs < $self.table.window_low_abs_for_target($abs_pos)
                        || candidate_abs >= $abs_pos
                    {
                        continue;
                    }
                    let candidate_idx = candidate_abs - $self.table.history_abs_start;
                    debug_assert!(
                        $abs_pos <= $current_abs_end,
                        "HC chain walker called past current block end"
                    );
                    let tail_limit = $current_abs_end - $abs_pos;
                    let base = concat.as_ptr();
                    // SAFETY: history-relative indices; `tail_limit` bounds
                    // the scan within `concat`. `$cpl` is the kernel-specific
                    // common_prefix_len_ptr — call inlines because the
                    // surrounding wrapper carries the same target_feature.
                    let match_len =
                        unsafe { $cpl(base.add(candidate_idx), base.add(current_idx), tail_limit) };
                    if match_len < min_match_len {
                        continue;
                    }
                    let offset = $abs_pos - candidate_abs;
                    if crate::encoding::bt::BtMatcher::push_candidate_ladder(
                        $out,
                        &mut best_len_for_skip,
                        MatchCandidate {
                            start: $abs_pos,
                            offset,
                            match_len,
                        },
                        min_match_len,
                    ) {
                        let candidate_end = candidate_abs + match_len;
                        if candidate_end > match_end_abs {
                            match_end_abs = candidate_end;
                        }
                    }
                    if match_len > HC_OPT_NUM || $abs_pos + match_len >= $current_abs_end {
                        break;
                    }
                }
            }
            // `match_end_abs` initialized to `abs_pos + 9`; monotonic
            // updates only ever extend it, so `match_end_abs - 8 >= 1`.
            $self.table.skip_insert_until_abs =
                $self.table.skip_insert_until_abs.max(match_end_abs - 8);
        }
        if let Some(ldm) = ldm_candidate {
            let _ = crate::encoding::bt::BtMatcher::push_candidate_ladder(
                $out,
                &mut best_len_for_skip,
                ldm,
                min_match_len,
            );
        }
    }};
}

/// `hash3_candidate` body parameterized over the per-CPU
/// `common_prefix_len_ptr` symbol. The hash3 probe checks one candidate per
/// position when invoked, so the per-call ABI savings compound across the
/// segment. Crate-private (see `bt_insert_step_no_rebase_body!`).
macro_rules! hash3_candidate_body {
    (
        $table:expr,
        $abs_pos:ident,
        $current_abs_end:ident,
        $min_match_len:ident,
        $cpl:path $(,)?
    ) => {{
        if $table.hash3_log == 0 {
            return None;
        }
        let idx = $abs_pos.checked_sub($table.history_abs_start)?;
        let concat = $table.live_history();
        if idx + 4 > concat.len() {
            return None;
        }
        let hash3 = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $table.hash3_log,
            3,
        );
        let entry = $table
            .hash3_table
            .get(hash3)
            .copied()
            .unwrap_or($crate::encoding::match_table::storage::HC_EMPTY);
        let candidate_abs =
            $crate::encoding::match_table::storage::MatchTable::stored_abs_position_fast(
                entry,
                $table.position_base,
                $table.index_shift,
            )?;
        if candidate_abs < $table.history_abs_start || candidate_abs >= $abs_pos {
            return None;
        }
        let offset = $abs_pos - candidate_abs;
        if offset >= $crate::encoding::bt::HC3_MAX_OFFSET {
            return None;
        }
        let candidate_idx = candidate_abs - $table.history_abs_start;
        let tail_limit = $current_abs_end.saturating_sub($abs_pos);
        let base = concat.as_ptr();
        // SAFETY: candidate/idx are within history range; tail_limit
        // bounds the scan within `concat`.
        let match_len = unsafe { $cpl(base.add(candidate_idx), base.add(idx), tail_limit) };
        (match_len >= $min_match_len).then_some($crate::encoding::opt::types::MatchCandidate {
            start: $abs_pos,
            offset,
            match_len,
        })
    }};
}
pub(crate) use hash3_candidate_body;

/// `for_each_repcode_candidate_with_reps` body parameterized over the per-CPU
/// `common_prefix_len_ptr` symbol so the per-rep prefix probe inlines under
/// the wrapper's `target_feature` umbrella instead of crossing the ABI
/// boundary through the dispatcher. Three rep probes per encoded position →
/// thousands per segment, so the per-call barrier was non-trivial.
///
/// The callback `f` runs in the wrapper's umbrella context too, so closures
/// that capture mutable state still work (FnMut). Crate-private
/// (see `bt_insert_step_no_rebase_body!`).
macro_rules! for_each_repcode_candidate_body {
    (
        $table:expr,
        $abs_pos:ident,
        $lit_len:ident,
        $reps:ident,
        $current_abs_end:ident,
        $min_match_len:ident,
        $f:ident,
        $cpl:path $(,)?
    ) => {{
        let rep_offsets: [Option<usize>; 3] = if $lit_len == 0 {
            [
                Some($reps[1] as usize),
                Some($reps[2] as usize),
                ($reps[0] > 1).then_some(($reps[0] - 1) as usize),
            ]
        } else {
            [
                Some($reps[0] as usize),
                Some($reps[1] as usize),
                Some($reps[2] as usize),
            ]
        };
        let concat = $table.live_history();
        let current_idx = $abs_pos - $table.history_abs_start;
        if current_idx + 4 > concat.len() {
            return;
        }
        let tail_limit = $current_abs_end.saturating_sub($abs_pos);
        let base = concat.as_ptr();
        let concat_len = concat.len();
        for rep in rep_offsets.into_iter().flatten() {
            if rep == 0 || rep > $abs_pos {
                continue;
            }
            let candidate_pos = $abs_pos - rep;
            if candidate_pos < $table.history_abs_start {
                continue;
            }
            let candidate_idx = candidate_pos - $table.history_abs_start;
            // Upstream zstd `ZSTD_readMINMATCH` gate (zstd_opt.c:657-674): a
            // 4-byte (3-byte when min_match_len == 3) equality probe
            // before the full prefix scan. Equivalent filtering — a
            // mismatch here means `match_len < min_match_len`, which
            // the post-scan check rejects anyway — but it skips the
            // prefix-kernel call for the common no-match case (rep
            // offsets rarely hit on low-redundancy input).
            //
            // SAFETY: `current_idx + 4 <= concat_len` (early return
            // above) and `candidate_idx < current_idx` (rep >= 1), so
            // both 4-byte reads stay inside `concat`.
            let gate_matches = unsafe {
                let cand = base.add(candidate_idx).cast::<u32>().read_unaligned();
                let cur = base.add(current_idx).cast::<u32>().read_unaligned();
                if $min_match_len == 3 {
                    // Compare the low-address 3 bytes regardless of
                    // endianness: byte-shift on LE, mask via to_le.
                    (cand.to_le() & 0x00FF_FFFF) == (cur.to_le() & 0x00FF_FFFF)
                } else {
                    cand == cur
                }
            };
            if !gate_matches {
                continue;
            }
            // SAFETY: `candidate_idx ≤ current_idx < concat_len` (since
            // candidate_pos ≤ abs_pos and we early-returned on
            // `current_idx + 4 > concat_len`). `max` clamps to the shorter
            // remaining run so neither pointer overruns `concat`.
            let max = (concat_len - candidate_idx)
                .min(concat_len - current_idx)
                .min(tail_limit);
            let match_len = unsafe { $cpl(base.add(candidate_idx), base.add(current_idx), max) };
            if match_len < $min_match_len {
                continue;
            }
            $f(MatchCandidate {
                start: $abs_pos,
                offset: rep,
                match_len,
            });
        }
    }};
}
pub(crate) use for_each_repcode_candidate_body;

/// `bt_insert_and_collect_matches` body parameterized over the per-CPU
/// `count_match_from_indices` symbol. Same shape as
/// [`bt_insert_step_no_rebase_body`] — picks up the matching kernel through
/// `$cmf` so the per-iteration vector probe inlines under the wrapper's
/// `target_feature` umbrella. Returns nothing (matches the original method).
/// Crate-private (see `bt_insert_step_no_rebase_body!`).
macro_rules! bt_insert_and_collect_matches_body {
    (
        $table:expr,
        $search_depth:expr,
        $abs_pos:ident,
        $current_abs_end:ident,
        $profile:ident,
        $min_match_len:ident,
        $best_len_for_skip:ident,
        $out:ident,
        $cmf:path $(,)?
    ) => {{
        let idx = $abs_pos - $table.history_abs_start;
        // Borrowed-aware live region (owned: `history[history_start..]`;
        // borrowed: the in-place input `[0, block_end)`). Reborrow-then-raw-ptr
        // so the slice holds NO borrow and coexists with the `&mut $table`
        // binary-tree writes below. Owned is byte-identical (same bytes).
        let concat: &[u8] = unsafe {
            let lh = $table.live_history();
            core::slice::from_raw_parts(lh.as_ptr(), lh.len())
        };
        if idx + 8 > concat.len() {
            return;
        }
        debug_assert!(
            $abs_pos <= $current_abs_end,
            "BT collect called past current block end"
        );
        let tail_limit = $current_abs_end - $abs_pos;
        let hash = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
            concat,
            idx,
            $table.hash_log,
            $table.search_mls,
        );
        // Prefetch the hash bucket now. For the large L16+ hash table over
        // high-entropy input the bucket is L3/DRAM-cold, and unlike upstream's
        // monolithic ZSTD_btGetAllMatches (which overlaps this miss with its
        // inline rep/hash3 prologue) the read+write of `hash_table[hash]`
        // below is reached with nothing to hide it behind — it stalled a large
        // share of this function's cycles. Issuing the hint here lets the miss
        // overlap the address setup that follows.
        #[cfg(all(
            target_feature = "sse",
            any(target_arch = "x86", target_arch = "x86_64")
        ))]
        {
            #[cfg(target_arch = "x86")]
            use core::arch::x86::{_MM_HINT_T0, _mm_prefetch};
            #[cfg(target_arch = "x86_64")]
            use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
            // SAFETY: prefetch is a hint that never faults; `hash` indexes
            // `hash_table` directly below, so it is in bounds.
            unsafe {
                _mm_prefetch($table.hash_table.as_ptr().add(hash).cast(), _MM_HINT_T0);
            }
            // Prefetch the NEXT position's bucket too. The optimal-parser DP
            // advances one position per iteration, so this miss is issued a
            // full BT walk plus the next iteration's pre-collect work ahead of
            // the collect that will read it — far more lead than the same-call
            // hint above, enough to hide the full DRAM latency.
            if idx + 1 + 8 <= concat.len() {
                let hash_next =
                    $crate::encoding::match_table::storage::MatchTable::hash_position_at(
                        concat,
                        idx + 1,
                        $table.hash_log,
                        $table.search_mls,
                    );
                // SAFETY: prefetch never faults; an out-of-range index is a
                // harmless no-op hint.
                unsafe {
                    _mm_prefetch(
                        $table.hash_table.as_ptr().add(hash_next).cast(),
                        _MM_HINT_T0,
                    );
                }
            }
        }
        let Some(relative_pos) = $table.relative_position($abs_pos) else {
            return;
        };
        let stored = relative_pos + 1;
        let bt_mask = $table.bt_mask();
        // Hoist the BT pointer-pair table's base out of `self` once: every
        // access below is `chain_table[computed_index]` through `&mut self`,
        // which the optimizer cannot prove loop-invariant, so it reloads the
        // Vec's (ptr,len) from the struct AND bounds-checks on every tree
        // step (the upstream zstd walks a raw `U32* btable`, zstd_opt.c). The raw
        // base carries no borrow, so the `&self` helper calls in the loop
        // (`bt_pair_index_for_abs`, `window_low_abs_for_target`,
        // `relative_position`) coexist — they read other fields, never
        // `chain_table`. Indices are in bounds by the BT invariants:
        // `bt_pair_index_for_abs` returns `2*(abs & bt_mask) (+1)` ≤
        // `chain_table.len()-1`, and the slots only ever hold those values.
        let chain_ptr = $table.chain_table.as_mut_ptr();
        debug_assert_eq!($table.chain_table.len(), 2 << $table.bt_log());
        // See `bt_insert_step_no_rebase_body!`: saturating is needed for the
        // first BT walk of a fresh frame where `abs_pos < bt_mask`.
        let bt_low = $abs_pos.saturating_sub(bt_mask);
        let window_low = $table.window_low_abs_for_target($abs_pos);
        // Upstream zstd-style window bound in stored space so the BT-walk loop
        // condition rejects out-of-window / HC_EMPTY candidates WITHOUT
        // decoding them (mirrors upstream `while ... matchIndex >= matchLow`):
        // one range check on `match_stored` instead of decode-then-break,
        // dropping the wasted candidate_abs decode on every walk's terminating
        // step. candidate_abs(s) = (position_base + s - 1) - index_shift =
        // base + s (wrapping); in-window ⟺ candidate_abs - window_low <
        // abs_pos - window_low ⟺ s.wrapping_add(win_off) < win_range.
        // HC_EMPTY (s = 0) maps to base = (lowest representable abs) - 1 <
        // window_low, so it falls out of range and ends the walk.
        let win_off = $table
            .position_base
            .wrapping_sub(1)
            .wrapping_sub($table.index_shift)
            .wrapping_sub(window_low);
        let win_range = $abs_pos - window_low;
        // Raw `+ 9` is safe here — see `bt_insert_step_no_rebase_body!`
        // for the full discussion of the upstream `STREAM_ABS_HEADROOM`
        // cap in `MatchTable::add_data`.
        let mut match_end_abs = $abs_pos + 9;
        let mut compares_left = $profile.max_chain_depth.min($search_depth);
        let mut common_length_smaller = 0usize;
        let mut common_length_larger = 0usize;
        let pair_idx = $table.bt_pair_index_for_abs($abs_pos);
        let mut smaller_slot = pair_idx;
        let mut larger_slot = pair_idx + 1;
        let mut match_stored = $table.hash_table[hash];
        $table.hash_table[hash] = stored;
        // Upstream zstd semantics: `bestLength` starts at `lengthToBeat - 1`; rep/hash3
        // probing may raise it; BT then only reports strictly longer matches.
        // `min_match_len >= HC_FORMAT_MINMATCH (3)` by configure invariant,
        // so `min_match_len - 1 >= 2` cannot underflow.
        debug_assert!(
            $min_match_len >= $crate::encoding::cost_model::HC_FORMAT_MINMATCH,
            "min_match_len must be at least HC_FORMAT_MINMATCH"
        );
        let mut best_len = (*$best_len_for_skip).max($min_match_len - 1);

        // Upstream zstd-form loop condition: the stored-space window range check
        // (`s.wrapping_add(win_off) < win_range`) rejects out-of-window and
        // HC_EMPTY candidates here, so the terminating step never enters the
        // body — no wasted candidate_abs decode, matching upstream's
        // `while ... matchIndex >= matchLow`.
        while compares_left > 0 && (match_stored as usize).wrapping_add(win_off) < win_range {
            compares_left -= 1;
            // The condition proved this candidate is in `[window_low,
            // abs_pos)`, so `match_stored >= 1` (HC_EMPTY is out of range) and
            // the `- 1` cannot underflow; candidate_abs == base + match_stored.
            let candidate_abs = ($table.position_base + (match_stored as usize - 1))
                .wrapping_sub($table.index_shift);

            let next_pair_idx = $table.bt_pair_index_for_abs(candidate_abs);
            // SAFETY: `next_pair_idx (+1)` = `2*(candidate_abs & bt_mask) (+1)`
            // ≤ `chain_table.len()-1`; `chain_ptr` is the hoisted live base,
            // table not realloc'd during the walk.
            let next_smaller = unsafe { *chain_ptr.add(next_pair_idx) };
            let next_larger = unsafe { *chain_ptr.add(next_pair_idx + 1) };
            let seed_len = common_length_smaller.min(common_length_larger);
            let candidate_idx = candidate_abs - $table.history_abs_start;
            // SAFETY: BT walk invariant — `candidate_idx + tail_limit ≤
            // concat.len()`.
            let match_len = unsafe { $cmf(concat, idx, candidate_idx, tail_limit, seed_len) };

            if match_len > best_len {
                let offset = $abs_pos - candidate_abs;
                let accepted = $crate::encoding::bt::BtMatcher::push_candidate_ladder(
                    $out,
                    $best_len_for_skip,
                    $crate::encoding::opt::types::MatchCandidate {
                        start: $abs_pos,
                        offset,
                        match_len,
                    },
                    $min_match_len,
                );
                if accepted {
                    best_len = match_len;
                    // BT walker invariants: `candidate_abs < abs_pos`
                    // and `match_len <= tail_limit = current_abs_end -
                    // abs_pos`. So `candidate_abs + match_len <
                    // abs_pos + tail_limit = current_abs_end`, which
                    // fits in `usize` on every supported target (32-bit
                    // i686 included) — the addition stays within the
                    // current block.
                    let candidate_end = candidate_abs + match_len;
                    if candidate_end > match_end_abs {
                        match_end_abs = candidate_end;
                    }
                    if match_len >= tail_limit
                        || match_len > $crate::encoding::cost_model::HC_OPT_NUM
                    {
                        break;
                    }
                }
            }

            if match_len >= tail_limit {
                break;
            }

            let candidate_next = candidate_idx + match_len;
            let current_next = idx + match_len;
            // SAFETY: first-differing positions after a match_len-long prefix;
            // match_len < tail_limit (break above) + BT-walk bound
            // idx/candidate_idx + tail_limit <= concat.len() keep both in range.
            if unsafe {
                *concat.get_unchecked(candidate_next) < *concat.get_unchecked(current_next)
            } {
                // SAFETY: `smaller_slot` holds a valid pair index (init
                // `pair_idx`, updated to `next_pair_idx + 1`); the `usize::MAX`
                // sentinel is set only just before `break`, never written here.
                unsafe { *chain_ptr.add(smaller_slot) = match_stored };
                common_length_smaller = match_len;
                if candidate_abs <= bt_low {
                    smaller_slot = usize::MAX;
                    break;
                }
                smaller_slot = next_pair_idx + 1;
                match_stored = next_larger;
            } else {
                // SAFETY: as above for `larger_slot`.
                unsafe { *chain_ptr.add(larger_slot) = match_stored };
                common_length_larger = match_len;
                if candidate_abs <= bt_low {
                    larger_slot = usize::MAX;
                    break;
                }
                larger_slot = next_pair_idx;
                match_stored = next_smaller;
            }
        }

        // SAFETY: both slots, when not the `usize::MAX` sentinel, hold valid
        // pair indices into the hoisted `chain_table` base.
        if smaller_slot != usize::MAX {
            unsafe {
                *chain_ptr.add(smaller_slot) = $crate::encoding::match_table::storage::HC_EMPTY
            };
        }
        if larger_slot != usize::MAX {
            unsafe {
                *chain_ptr.add(larger_slot) = $crate::encoding::match_table::storage::HC_EMPTY
            };
        }

        // Dict dual-probe (upstream zstd `ZSTD_dictMatchState`, zstd_opt.c:777-813):
        // after the live tree, descend the immutable dictionary BINARY TREE
        // (built in `prime_dms_bt`) with its OWN compare budget and push any
        // dict match longer than the live best into the ladder. The DUBT
        // descent reaches the longest dict match efficiently (a hash-chain
        // surfaced only the few same-bucket candidates and left most of the
        // dict savings unrealised at btlazy2 / btopt). Dict positions are
        // dictionary-relative concat indices in `[0, region)`, pinned at the
        // front of history, so a dict candidate at `dict_idx` sits at offset
        // `idx - dict_idx` (no upstream zstd `dmsIndexDelta`). The optimal parser
        // prices these (its DP lookahead values the repcode chain a dict match
        // seeds); the greedy/lazy parser commits the longest.
        if let Some(dms) = $table.dms.table() {
            let region = $table.dms.region_len();
            let dh = $crate::encoding::match_table::storage::MatchTable::hash_position_at(
                concat,
                idx,
                dms.hash_log,
                dms.mls,
            );
            let mut dcur = dms.hash_table[dh];
            // DUBT seed lengths: bytes already known common on each side, so
            // `$cmf` resumes from there (upstream zstd commonLengthSmaller/Larger).
            let mut common_smaller = 0usize;
            let mut common_larger = 0usize;
            let mut dms_compares = $profile.max_chain_depth.min($search_depth);
            while dms_compares > 0 && dcur != $crate::encoding::match_table::storage::HC_EMPTY {
                let dict_idx = (dcur - 1) as usize;
                // The dict tree holds only dict positions (`< region <= idx`).
                if dict_idx >= region || dict_idx >= idx {
                    break;
                }
                dms_compares -= 1;
                let pair = 2 * dict_idx;
                let seed = common_smaller.min(common_larger);
                // SAFETY: `dict_idx < idx` and `idx + tail_limit <=
                // concat.len()` (checked at entry); same umbrella as the live
                // walk's `$cmf`. `seed <= prior match_len <= tail_limit`.
                let match_len = unsafe { $cmf(concat, idx, dict_idx, tail_limit, seed) };
                if match_len > best_len {
                    let offset = idx - dict_idx;
                    let accepted = $crate::encoding::bt::BtMatcher::push_candidate_ladder(
                        $out,
                        $best_len_for_skip,
                        $crate::encoding::opt::types::MatchCandidate {
                            start: $abs_pos,
                            offset,
                            match_len,
                        },
                        $min_match_len,
                    );
                    if accepted {
                        best_len = match_len;
                        let candidate_end = $abs_pos + match_len;
                        if candidate_end > match_end_abs {
                            match_end_abs = candidate_end;
                        }
                        if match_len > $crate::encoding::cost_model::HC_OPT_NUM {
                            break;
                        }
                    }
                }
                // Match reached the block tail: can't order the pair (upstream zstd
                // `ip+matchLength == iLimit`), and indexing `concat[idx +
                // match_len]` below would step past the searchable region.
                if match_len >= tail_limit {
                    break;
                }
                // Descend the DUBT (upstream zstd zstd_opt.c:806-811): dict candidate
                // smaller than input → its larger child is closer to `idx`.
                if concat[dict_idx + match_len] < concat[idx + match_len] {
                    common_smaller = match_len;
                    dcur = dms.chain_table[pair + 1];
                } else {
                    common_larger = match_len;
                    dcur = dms.chain_table[pair];
                }
            }
        }

        // `match_end_abs >= abs_pos + 9 >= 9` (initialized and monotonic),
        // so `match_end_abs - 8 >= 1` cannot underflow.
        $table.skip_insert_until_abs = match_end_abs - 8;
    }};
}
pub(crate) use bt_insert_and_collect_matches_body;

impl HcMatchGenerator {
    /// Heap bytes this generator owns: the shared match table plus the BT
    /// backend's optimal-parser / LDM scratch (the HC knobs are inline).
    pub(crate) fn heap_size(&self) -> usize {
        self.table.heap_size() + self.backend.heap_size()
    }

    pub(crate) fn should_run_btultra2_seed_pass<S: crate::encoding::strategy::Strategy>(
        &self,
        current_len: usize,
    ) -> bool {
        // The in-block two-pass dynamic-stats seed (`initStats_ultra`)
        // is btultra2-only. `TWO_PASS_SEED` is `false` for every other
        // strategy — including btultra, which now shares the hash3
        // short-match probe but stays single-pass — so the seed call and
        // its body drop at codegen time for all non-btultra2 kernels.
        if !S::TWO_PASS_SEED {
            return false;
        }
        let HcBackend::Bt(bt) = &self.backend else {
            return false;
        };
        bt.opt_state.lit_length_sum == 0
            && bt.opt_state.dictionary_seed.is_none()
            && !self.table.dictionary_primed_for_frame
            && bt.ldm_sequences.is_empty()
            && self.table.window_size == current_len
            && self.table.history_abs_start == 0
            && self.table.chunk_lens.len() == 1
            && current_len > HC_PREDEF_THRESHOLD
    }

    pub(crate) fn new(max_window_size: usize) -> Self {
        Self {
            table: crate::encoding::match_table::storage::MatchTable::new(max_window_size),
            hc: crate::encoding::hc::HcMatcher::new(2, HC_SEARCH_DEPTH, HC_TARGET_LEN),
            // Default to the zero-sized HC backend; `configure()` swaps
            // in a `BtMatcher` only when an optimal strategy lands.
            backend: HcBackend::Hc,
            // Lazy is the per-construct default — every production
            // caller calls `configure()` before the first encode and
            // overwrites this. Tests that drive `HcMatchGenerator`
            // without calling `configure()` end up in the
            // `start_matching_lazy` arm of the test dispatcher, which
            // matches the previous default behaviour.
            strategy_tag: crate::encoding::strategy::StrategyTag::Lazy,
        }
    }

    pub(crate) fn configure(
        &mut self,
        config: HcConfig,
        tag: crate::encoding::strategy::StrategyTag,
        window_log: u8,
    ) {
        use crate::encoding::strategy::StrategyTag;
        // Mirror the driver-resolved strategy tag so the
        // `#[cfg(test)] start_matching` dispatcher can route
        // BtOpt / BtUltra / BtUltra2 to distinct monomorphisations.
        self.strategy_tag = tag;
        let is_btultra2 = tag == StrategyTag::BtUltra2;
        let uses_bt = matches!(
            tag,
            StrategyTag::Btlazy2
                | StrategyTag::BtOpt
                | StrategyTag::BtUltra
                | StrategyTag::BtUltra2
        );
        // btultra and btultra2 both run the mls=3 hash3 short-match probe
        // (clevels.h minMatch 3). The `is_btultra2` flag below stays
        // exclusive to btultra2 because it tweaks the BT rebase boundary,
        // not match finding.
        let wants_hash3 = matches!(tag, StrategyTag::BtUltra | StrategyTag::BtUltra2);
        let next_hash3_log = if wants_hash3 {
            HC3_HASH_LOG.min(window_log as usize)
        } else {
            0
        };
        let resize = self.table.hash_log != config.hash_log
            || self.table.chain_log != config.chain_log
            || self.table.hash3_log != next_hash3_log;
        // Capture the layout flip BEFORE `uses_bt` is overwritten below — it
        // feeds the dms invalidation (the dms is keyed by layout too).
        let uses_bt_changed = self.table.uses_bt != uses_bt;
        self.table.hash_log = config.hash_log;
        self.table.chain_log = config.chain_log;
        self.table.hash3_log = next_hash3_log;
        self.hc.search_depth = if uses_bt {
            config.search_depth
        } else {
            config.search_depth.min(MAX_HC_SEARCH_DEPTH)
        };
        self.hc.target_len = config.target_len;
        // Mirror strategy-derived flags + HC search depth onto MatchTable
        // so the BT walker and rebase machinery can read them directly
        // without dispatching back through HcMatchGenerator.
        self.table.search_depth = self.hc.search_depth;
        self.table.is_btultra2 = is_btultra2;
        self.table.uses_bt = uses_bt;
        // BT finder hash width, upstream zstd `mls = BOUNDED(4, cParams.minMatch, 6)`,
        // carried explicitly in the level config so a `target_length` override
        // cannot silently flip the finder between 5- and 4-byte hashing. Only
        // the BT body reads it; HC/lazy levels leave it at 4. clevels.h
        // (srcSize > 256 KiB tier): btlazy2 L13-15 + btopt L16 are minMatch=5,
        // btopt L17 is minMatch=4, btultra/btultra2 are minMatch=3 (4-byte main
        // hash + the hash3 short-match probe).
        // The cached dms is keyed by the full (region, layout, mls, hash_log)
        // shape that `build_dms!` validates on the normal prime path, but the
        // reborrow fast path in `MatchTable::reset` reuses it on `dms.is_primed()`
        // ALONE. A reused-compressor level switch can change the search mls (e.g.
        // btlazy2 -> lazy), the table geometry (hash_log / chain_log / hash3,
        // captured in `resize`), OR the HC<->BT layout (`uses_bt_changed`)
        // independently of each other, and any of them leaves the dms hashed for
        // a different shape. Invalidate on ANY so the next dict frame re-primes at
        // the new shape (configure runs before reset) instead of probing a
        // mismatched dms and silently degrading match quality. Over-invalidation
        // only costs a re-prime, which a real shape change needs anyway.
        let mls_changed = self.table.search_mls != config.search_mls;
        if resize || mls_changed || uses_bt_changed {
            self.table.dms.invalidate();
        }
        self.table.search_mls = config.search_mls;
        // Stage D: promote the backend discriminator. HC modes drop the
        // BT scratch buffers entirely; switching back into a BT mode
        // allocates a fresh `BtMatcher` on demand.
        match (&self.backend, self.table.uses_bt) {
            (HcBackend::Hc, true) => {
                self.backend =
                    HcBackend::Bt(alloc::boxed::Box::new(crate::encoding::bt::BtMatcher::new()));
            }
            (HcBackend::Bt(_), false) => {
                self.backend = HcBackend::Hc;
            }
            _ => {}
        }
        if resize && !self.table.hash_table.is_empty() {
            // Force reallocation on next ensure_tables() call.
            self.table.hash_table.clear();
            self.table.hash3_table.clear();
            self.table.chain_table.clear();
        }
    }

    pub(crate) fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.opt_state.seed_dictionary_entropy(huff, ll, ml, of);
        }
    }

    /// Install (or clear) the long-distance-match producer (#27). Only
    /// the BT backend owns an `ldm_producer` slot; on the HC (lazy)
    /// backend the producer is dropped because there is no optimal-parser
    /// candidate buffer to seed. Call after [`Self::reset`].
    #[cfg(feature = "hash")]
    pub(crate) fn set_ldm_producer(&mut self, producer: Option<crate::encoding::ldm::LdmProducer>) {
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.ldm_producer = producer;
        }
    }

    /// Move the LDM producer out of the BT backend, leaving `None`. Used by the
    /// dictionary snapshot path: the producer carries no dictionary state (LDM
    /// is not dict-primed; its hash table is empty at capture), so it is not
    /// retained in the snapshot — the working frame's freshly-reset producer is
    /// reinstated on restore instead.
    #[cfg(feature = "hash")]
    pub(crate) fn take_ldm_producer(&mut self) -> Option<crate::encoding::ldm::LdmProducer> {
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.ldm_producer.take()
        } else {
            None
        }
    }

    pub(crate) fn reset(&mut self, reuse_space: impl FnMut(Vec<u8>)) {
        self.table.reset(reuse_space);
        if let HcBackend::Bt(bt) = &mut self.backend {
            bt.reset();
        }
    }

    /// Backfill positions from the tail of the previous slice that couldn't be
    /// hashed at the time (insert_position needs 4 bytes of lookahead).
    pub(crate) fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.table.skip_matching(incompressible_hint);
    }

    /// Runtime-dispatched entry kept only for in-crate tests. Production
    /// callers reach the inner loops through
    /// [`Self::start_matching_strategy`] / [`MatchGeneratorDriver::compress_block`]
    /// which pick the lazy / optimal arm from `S::USE_BT` at
    /// monomorphisation time.
    #[cfg(test)]
    pub(crate) fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        use crate::encoding::strategy::{self, StrategyTag};
        // Dispatch on the mirrored `strategy_tag` so each test runs
        // under the same monomorphisation production would pick.
        // `BtOpt` / `BtUltra` / `BtUltra2` remain distinct here even
        // though `table.uses_bt` / `is_btultra2` alone can't separate
        // BtOpt from BtUltra.
        match self.strategy_tag {
            StrategyTag::Fast | StrategyTag::Dfast | StrategyTag::Greedy | StrategyTag::Lazy => {
                self.start_matching_lazy(&mut handle_sequence)
            }
            StrategyTag::Btlazy2 => self.start_matching_btlazy2(&mut handle_sequence),
            StrategyTag::BtOpt => {
                self.start_matching_optimal::<strategy::BtOpt>(&mut handle_sequence)
            }
            StrategyTag::BtUltra => {
                self.start_matching_optimal::<strategy::BtUltra>(&mut handle_sequence)
            }
            StrategyTag::BtUltra2 => {
                self.start_matching_optimal::<strategy::BtUltra2>(&mut handle_sequence)
            }
        }
    }

    /// Strategy-aware entry point used by
    /// [`MatchGeneratorDriver::compress_block`]. Branches on
    /// `S::USE_BT` — a compile-time `const` — so each
    /// monomorphisation keeps exactly one arm: `Lazy` /
    /// `Fast` / `Dfast` / `Greedy` see only `start_matching_lazy`,
    /// `BtOpt` / `BtUltra` / `BtUltra2` see only
    /// `start_matching_optimal`. The inherent test-only
    /// [`HcMatchGenerator::start_matching`] reaches the same arms by
    /// runtime-matching on `self.strategy_tag` (the parse-mode field
    /// has been removed); production never invokes that path.
    pub(crate) fn start_matching_strategy<S: crate::encoding::strategy::Strategy>(
        &mut self,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        debug_assert_eq!(
            self.table.uses_bt,
            S::USE_BT,
            "Strategy::USE_BT disagrees with runtime table.uses_bt at HC dispatch"
        );
        if S::USE_BT {
            self.start_matching_optimal::<S>(handle_sequence)
        } else {
            self.start_matching_lazy(handle_sequence)
        }
    }

    /// Dispatcher: pick the dict-aware monomorph when a separate dms is primed
    /// (attach-mode dictionary), else the no-dict monomorph. Mirrors upstream's
    /// compile-time `dictMode` split — the `DICT = false` body carries no dms
    /// code at all, so the no-dict hot path is unaffected by the dict search.
    pub(crate) fn start_matching_lazy(
        &mut self,
        handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        if self.table.dms.is_primed() {
            self.start_matching_lazy_impl::<true>(handle_sequence);
        } else {
            self.start_matching_lazy_impl::<false>(handle_sequence);
        }
    }

    pub(crate) fn start_matching_lazy_impl<const DICT: bool>(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.table.ensure_tables();

        // `current_block_range()` is borrowed-aware: owned → last committed
        // chunk; borrowed → the staged in-place block range.
        let (current_abs_start, current_len) = self.table.current_block_range();
        if current_len == 0 {
            return;
        }
        // The current block is the tail of `history` (owned) or the staged
        // borrowed range (`get_last_space()` resolves both). Hoist it as a raw
        // slice: the routine mutates the hash/chain tables + `offset_hist` but
        // never reallocates `history`, so the slice stays valid and we avoid
        // re-borrowing `self.table` (which would conflict with the
        // `offset_hist` write).
        let current_ptr = self.table.get_last_space().as_ptr();
        let current: &[u8] = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };

        // Full live history (dict + committed blocks + current block), hoisted
        // ONCE for the whole position scan and threaded into every
        // `find_best_match` / `pick_lazy_match` call. `live_history()` is
        // loop-invariant here (the scan mutates the hash/chain tables +
        // `offset_hist` but never the history bytes or length), so re-fetching
        // it per find — inside `hash_chain_candidate` + the rep probe, plus
        // again for each lazy lookahead at pos+1 / pos+2 — was pure
        // per-position overhead. Same raw-slice detach as `current` so the
        // loop's `&mut self.table` inserts coexist with this `&[u8]`.
        let concat: &[u8] = {
            let lh = self.table.live_history();
            unsafe { core::slice::from_raw_parts(lh.as_ptr(), lh.len()) }
        };
        // Dict-match-state primed flag, hoisted ONCE for the scan: it is
        // block-invariant (the dict is primed before the block) and lives on the
        // cold `dms` cacheline, so the per-find `dms.is_primed()` load was a
        // measurable hot-path cost (~8% of `hash_chain_candidate` on the
        // dict-over-random fixture). The `DICT = false` monomorph ignores it.
        let dms_primed = self.table.dms.is_primed();

        let current_abs_end = current_abs_start + current_len;
        self.table
            .backfill_boundary_positions(current_abs_start, current_abs_end);

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + HC_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            // `find_best_match` returns the forward `(offset, length)` in
            // registers (`HcMatch`, 16 bytes) — no 24-byte `MatchCandidate` /
            // 32-byte `Option` spilled-and-copied per position. The backward
            // extension that yields `start` runs ONCE here, after the lazy
            // decision settles, exactly like upstream's lazy loop.
            let best =
                self.hc
                    .find_best_match::<DICT>(concat, dms_primed, &self.table, abs_pos, lit_len);
            if best.is_match() {
                if self.hc.pick_lazy_match::<DICT>(
                    concat,
                    dms_primed,
                    &self.table,
                    abs_pos,
                    lit_len,
                    best,
                ) {
                    // Backward-extend over the literal run (upstream `zstd_lazy.c`
                    // after rep-vs-chain selection). The offset is preserved;
                    // `start` and `match_len` grow by the same amount, bounded by
                    // `literals_start` (the `min_abs` floor) so it never crosses
                    // an already-emitted sequence.
                    let history_abs_start = self.table.history_abs_start;
                    let min_abs = abs_pos - lit_len;
                    let mut start_abs = abs_pos;
                    let mut cand_abs = abs_pos - best.offset;
                    let mut match_len = best.match_len;
                    while start_abs > min_abs
                        && cand_abs > history_abs_start
                        && concat[cand_abs - history_abs_start - 1]
                            == concat[start_abs - history_abs_start - 1]
                    {
                        start_abs -= 1;
                        cand_abs -= 1;
                        match_len += 1;
                    }
                    self.table.insert_match_span(abs_pos, start_abs + match_len);
                    let start = start_abs - current_abs_start;
                    let literals = &current[literals_start..start];
                    handle_sequence(Sequence::Triple {
                        literals,
                        offset: best.offset,
                        match_len,
                    });
                    let _ = encode_offset_with_history(
                        best.offset as u32,
                        literals.len() as u32,
                        &mut self.table.offset_hist,
                    );
                    pos = start + match_len;
                    literals_start = pos;
                    continue;
                }
                // Lazy lookahead found a better match at `abs_pos + 1` / `+ 2`
                // (defer): advance exactly ONE byte (upstream
                // `ZSTD_compressBlock_lazy_generic`) so the deferred candidate is
                // re-evaluated at its own position; the no-match skip below could
                // jump past it once the literal run reaches 256+ bytes.
                self.table.insert_position(abs_pos);
                pos += 1;
                continue;
            }
            // No match found.
            self.table.insert_position(abs_pos);
            // Lazy skipping (upstream zstd `ZSTD_compressBlock_lazy_generic`,
            // zstd_lazy.c:1614): advance faster over runs with no match.
            // `step = ((ip - anchor) >> kSearchStrength) + 1` with
            // kSearchStrength = 8, where `ip - anchor` is the current
            // literal-run length. On compressible input the run stays short
            // (step == 1, identical to a 1-byte advance); on incompressible
            // / dict-over-random input the run grows so the parser skips
            // ahead (one search per `step` positions) instead of searching
            // every byte. Skipped positions are not inserted, mirroring
            // upstream (it inserts only searched positions during a no-match
            // run). Ratio follows upstream (not byte-identical).
            let step = ((pos - literals_start) >> 8) + 1;
            pos += step;
            // No clamp needed before the tail loop: the search bound and the
            // hashable bound are both `pos + HC_MIN_MATCH_LEN <= current_len`
            // (HC_MIN_MATCH_LEN == 4 == the insert width), so there is no
            // non-searchable-but-hashable anchor to miss. Positions the skip
            // jumps over inside the searchable region are intentionally not
            // inserted — same as upstream zstd, which advances past them via
            // the identical `ip += step` and never hashes them either.
        }

        // Insert remaining hashable positions in the tail (the matching loop
        // stops at HC_MIN_MATCH_LEN but insert_position only needs 4 bytes).
        while pos + 4 <= current_len {
            self.table.insert_position(current_abs_start + pos);
            pos += 1;
        }

        if literals_start < current_len {
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    /// Register the borrowed input window for the no-copy one-shot path.
    /// # Safety
    /// `buffer` must outlive the borrowed scans (see `MatchTable`).
    pub(crate) unsafe fn set_borrowed_window(&mut self, buffer: &[u8]) {
        // SAFETY: forwarded liveness contract.
        unsafe { self.table.set_borrowed_window(buffer) };
    }

    pub(crate) fn clear_borrowed_window(&mut self) {
        self.table.clear_borrowed_window();
    }

    /// Borrowed (no-copy) equivalent of [`Self::start_matching_lazy`]: stage
    /// the in-place block range, then run the same lazy chain parse. The
    /// parse reads its range via `current_block_range()` and its bytes via
    /// `get_last_space()` / `live_history()`, all borrowed-aware, so the block
    /// is scanned in place with the per-position window_low offset cap.
    pub(crate) fn start_matching_lazy_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.table.stage_borrowed_block(block_start, block_end);
        self.start_matching_lazy(handle_sequence);
    }

    /// Borrowed (no-copy) equivalent of the lazy `skip_matching`: stage the
    /// in-place block, then seed positions without an owned-history append.
    pub(crate) fn skip_matching_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        incompressible_hint: Option<bool>,
    ) {
        self.table.stage_borrowed_block(block_start, block_end);
        self.table.skip_matching(incompressible_hint);
    }

    /// Upstream zstd `ZSTD_btlazy2` (levels 13-15): binary-tree match finder with a
    /// greedy/lazy parse. Bare dispatcher — resolves the runtime tier ONCE
    /// per block via `select_kernel()` and calls the matching
    /// `start_matching_btlazy2_<kernel>` wrapper, so the per-position BT
    /// collect runs under a single `#[target_feature]` umbrella (mirrors
    /// `build_optimal_plan_impl`). See `start_matching_btlazy2_body!` for the
    /// shared loop.
    pub(crate) fn start_matching_btlazy2(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.start_matching_btlazy2_neon(&mut handle_sequence)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.start_matching_btlazy2_avx2_bmi2(&mut handle_sequence)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.start_matching_btlazy2_sse42(&mut handle_sequence)
                },
                FastpathKernel::Scalar => self.start_matching_btlazy2_scalar(&mut handle_sequence),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.start_matching_btlazy2_scalar(&mut handle_sequence)
        }
    }

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn start_matching_btlazy2_neon(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_neon,
            crate::encoding::fastpath::neon::count_match_from_indices
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn start_matching_btlazy2_sse42(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_sse42,
            crate::encoding::fastpath::sse42::count_match_from_indices
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn start_matching_btlazy2_avx2_bmi2(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_avx2_bmi2,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices
        )
    }

    // Scalar wrapper: no `#[target_feature]`; `$collect` (the scalar collect)
    // is a safe fn, so the body macro's `unsafe` block is inert here. Same cfg
    // as `collect_optimal_candidates_initialized_scalar` (absent on
    // aarch64-little, where NEON is the baseline tier).
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    #[allow(unused_unsafe)]
    pub(crate) fn start_matching_btlazy2_scalar(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_btlazy2_body!(
            self,
            handle_sequence,
            collect_optimal_candidates_initialized_scalar,
            crate::encoding::fastpath::scalar::count_match_from_indices
        )
    }

    pub(crate) fn start_matching_optimal<S: crate::encoding::strategy::Strategy>(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.table.ensure_tables();
        // Borrowed-aware: owned → last committed chunk; borrowed → staged
        // in-place block range.
        let (current_abs_start, current_len) = self.table.current_block_range();
        if current_len == 0 {
            return;
        }
        let current_ptr = self.table.get_last_space().as_ptr();
        // `start_matching_optimal()` mutates tables/state but never mutates or
        // reallocates `self.table.history`, so this tail slice remains valid for
        // the duration of the routine and avoids cloning the full block.
        let current = unsafe { core::slice::from_raw_parts(current_ptr, current_len) };

        let current_abs_end = current_abs_start + current_len;
        self.table
            .apply_limited_update_after_long_match(current_abs_start);
        let hash3_start_cursor = self
            .table
            .skip_insert_until_abs
            .max(self.table.history_abs_start);
        self.table
            .backfill_boundary_positions(current_abs_start, current_abs_end);
        self.table.next_to_update3 = hash3_start_cursor;
        // Borrow split: `prepare_ldm_candidates` needs immutable
        // access to the live history (the post-`history_start`
        // slice of `self.table.history`) while it mutates the LDM
        // bucket table owned by `self.backend.bt_mut()`. Both live
        // in disjoint fields of `Self`, so we capture the slice +
        // its base before reaching for `bt_mut()`.
        //
        // The producer operates in absolute stream coordinates
        // throughout; `live_history[0]` corresponds to absolute
        // `history_abs_start` (upstream zstd `base + dictLimit`), and the
        // abs→slice translation happens inside the producer at
        // each `live_history[..]` access. Passing the full
        // `history` Vec would index into the dead prefix (the
        // bytes already retired past `history_start`).
        let live_history = self.table.live_history();
        let history_abs_start = self.table.history_abs_start;
        self.backend.bt_mut().prepare_ldm_candidates(
            live_history,
            history_abs_start,
            current_abs_start,
            current_len,
        );

        if self.should_run_btultra2_seed_pass::<S>(current_len) {
            self.run_btultra2_seed_pass(current, current_abs_start, current_len);
        }

        // Const-generic profile selection: every field is folded from
        // S's associated consts (MAX_CHAIN_DEPTH /
        // SUFFICIENT_MATCH_LEN / ACCURATE_PRICE / FAVOR_SMALL_OFFSETS),
        // so the optimiser produces the literal at codegen time
        // without a runtime match.
        let profile = HcOptimalCostProfile::const_for_strategy::<S>();
        let mut opt_state =
            core::mem::replace(&mut self.backend.bt_mut().opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, profile);
        let mut best_plan = core::mem::take(&mut self.backend.bt_mut().opt_segment_plan_scratch);
        best_plan.clear();
        let mut plan_reps = self.table.offset_hist;
        let (mut cursor, mut plan_litlen) =
            self.table.opt_start_cursor_and_litlen(current_abs_start);
        let mut plan_literals_cursor = 0usize;
        let match_loop_limit = current_len.saturating_sub(8);
        while cursor < match_loop_limit {
            let remaining_len = current_len - cursor;
            let segment_abs_start = current_abs_start + cursor;
            let segment_start = best_plan.len();
            let (_, end_reps, end_litlen, consumed_len) = self.build_optimal_plan::<S>(
                &current[cursor..],
                segment_abs_start,
                remaining_len,
                HcOptimalPlanState {
                    block_offset: cursor,
                    reps: plan_reps,
                    litlen: plan_litlen,
                    profile,
                },
                &opt_state,
                &mut best_plan,
            );
            BtMatcher::update_plan_stats_segment(
                current,
                current_len,
                &best_plan[segment_start..],
                &mut plan_literals_cursor,
                &mut plan_reps,
                &mut opt_state,
                profile.accurate,
            );
            plan_reps = end_reps;
            plan_litlen = end_litlen;
            cursor += consumed_len;
        }

        self.table
            .emit_optimal_plan(current_len, &best_plan, &mut handle_sequence);
        best_plan.clear();
        self.backend.bt_mut().opt_segment_plan_scratch = best_plan;
        self.backend.bt_mut().opt_state = opt_state;
    }

    fn run_btultra2_seed_pass(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
    ) {
        // The seed pass is BtUltra2-exclusive by name (the only
        // caller is `should_run_btultra2_seed_pass`), so pin `S` to
        // `BtUltra2` for both the cost-profile lookup and the
        // `build_optimal_plan::<S>` call below.
        type S = crate::encoding::strategy::BtUltra2;
        let seed_profile = HcOptimalCostProfile::const_for_strategy::<S>();
        let mut opt_state =
            core::mem::replace(&mut self.backend.bt_mut().opt_state, HcOptState::new());
        opt_state.rescale_freqs(current, seed_profile);
        let mut seed_reps = self.table.offset_hist;
        let (mut cursor, mut seed_litlen) =
            self.table.opt_start_cursor_and_litlen(current_abs_start);
        let mut seed_literals_cursor = 0usize;
        let mut seed_plan = core::mem::take(&mut self.backend.bt_mut().opt_seed_plan_scratch);
        seed_plan.clear();
        let match_loop_limit = current_len.saturating_sub(8);
        while cursor < match_loop_limit {
            let remaining_len = current_len - cursor;
            let segment_abs_start = current_abs_start + cursor;
            let segment_start = seed_plan.len();
            let (_, end_reps, end_litlen, consumed_len) = self.build_optimal_plan::<S>(
                &current[cursor..],
                segment_abs_start,
                remaining_len,
                HcOptimalPlanState {
                    block_offset: cursor,
                    reps: seed_reps,
                    litlen: seed_litlen,
                    profile: seed_profile,
                },
                &opt_state,
                &mut seed_plan,
            );
            BtMatcher::update_plan_stats_segment(
                current,
                current_len,
                &seed_plan[segment_start..],
                &mut seed_literals_cursor,
                &mut seed_reps,
                &mut opt_state,
                seed_profile.accurate,
            );
            seed_plan.truncate(segment_start);
            seed_reps = end_reps;
            seed_litlen = end_litlen;
            cursor += consumed_len;
        }
        seed_plan.clear();
        self.backend.bt_mut().opt_seed_plan_scratch = seed_plan;
        self.backend.bt_mut().opt_state = opt_state;

        // Upstream zstd initStats_ultra keeps the collected entropy statistics but
        // invalidates the first-pass matchfinder history before the real pass.
        self.table.position_base = self.table.history_abs_start;
        self.table.index_shift = current_len;
        self.table.next_to_update3 = current_abs_start;
        self.table.skip_insert_until_abs = current_abs_start;
        // Upstream zstd `ZSTD_initStats_ultra()` invalidates the first scan by moving
        // `window.base` back by `srcSize`, making the real pass start at
        // `curr == srcSize` instead of 0. Position 0 is therefore a valid
        // table entry in the second pass even though raw C tables reserve
        // value 0 as empty during an unshifted first pass.
        self.table.allow_zero_relative_position = true;
    }

    fn build_optimal_plan<S: crate::encoding::strategy::Strategy>(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        debug_assert!(S::USE_BT, "build_optimal_plan called on non-BT strategy");
        debug_assert_eq!(initial_state.profile.accurate, S::ACCURATE_PRICE);
        debug_assert_eq!(
            initial_state.profile.favor_small_offsets,
            S::FAVOR_SMALL_OFFSETS
        );
        // `S::ACCURATE_PRICE` / `S::FAVOR_SMALL_OFFSETS` cannot appear
        // as const-generic arguments yet (`generic_const_exprs` is
        // still unstable), so dispatch over a 4-arm match — but on the
        // strategy's ASSOCIATED CONSTS, not the runtime profile (the
        // `debug_assert_eq`s above pin the runtime profile to those
        // consts). A const scrutinee folds the three dead arms at
        // monomorphisation; matching the runtime profile instead kept
        // all four `#[inline(always)]` DP bodies (~16 KB each) alive in
        // EVERY `S` instantiation — ~360 KB of the wasm payload.
        match (S::ACCURATE_PRICE, S::FAVOR_SMALL_OFFSETS) {
            (true, false) => self.build_optimal_plan_impl::<S, true, false>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (true, true) => self.build_optimal_plan_impl::<S, true, true>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (false, false) => self.build_optimal_plan_impl::<S, false, false>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
            (false, true) => self.build_optimal_plan_impl::<S, false, true>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            ),
        }
    }

    /// Cross-platform DP entry. Picks the kernel-specific variant so the
    /// entire optimal-parser DP body (per-position match gathering, price
    /// updates, traceback) runs inside a single `target_feature` umbrella
    /// alongside the per-position `collect_optimal_candidates_initialized_
    /// <kernel>`. This eliminates the final ABI barrier on the hot per-
    /// position match-collection call — the level22 critical path is now
    /// one straight-line inline chain from DP body down through BT walk
    /// and match-length probes.
    #[inline(always)]
    fn build_optimal_plan_impl<
        S: crate::encoding::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.build_optimal_plan_impl_neon::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.build_optimal_plan_impl_avx2_bmi2::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.build_optimal_plan_impl_sse42::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    )
                },
                FastpathKernel::Scalar => self
                    .build_optimal_plan_impl_scalar::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                        current,
                        current_abs_start,
                        current_len,
                        initial_state,
                        stats,
                        out,
                    ),
            }
        }
        // wasm with simd128: route through the simd128 DP body (4-lane price-set).
        #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
        unsafe {
            self.build_optimal_plan_impl_simd128::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            )
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64",
            all(target_arch = "wasm32", target_feature = "simd128")
        )))]
        {
            self.build_optimal_plan_impl_scalar::<S, ACCURATE_PRICE, FAVOR_SMALL_OFFSETS>(
                current,
                current_abs_start,
                current_len,
                initial_state,
                stats,
                out,
            )
        }
    }

    /// NEON-umbrella DP body. Inlines
    /// `collect_optimal_candidates_initialized_neon` (and its entire
    /// per-position pipeline) directly into the DP loop.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn build_optimal_plan_impl_neon<
        S: crate::encoding::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_neon,
            crate::encoding::hc::priceset::priceset_range_nonabort_neon,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn build_optimal_plan_impl_sse42<
        S: crate::encoding::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_sse42,
            crate::encoding::hc::priceset::priceset_range_nonabort_sse41,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn build_optimal_plan_impl_avx2_bmi2<
        S: crate::encoding::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_avx2_bmi2,
            crate::encoding::hc::priceset::priceset_range_nonabort_avx2,
        )
    }

    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    // Body macros wrap callees in `unsafe { }` for the NEON/AVX/SSE
    // variants where callees are `unsafe fn`. The scalar wrappers route
    // through safe fns, so those blocks are redundant on this path.
    #[allow(unused_unsafe)]
    // The dispatch reaches this only on non-SIMD x86 (Scalar tier) and the
    // portable fallback; on wasm+simd128 the simd128 wrapper is selected, so
    // this is cfg-dead there.
    #[cfg_attr(
        all(target_arch = "wasm32", target_feature = "simd128"),
        allow(dead_code)
    )]
    fn build_optimal_plan_impl_scalar<
        S: crate::encoding::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_scalar,
            crate::encoding::hc::priceset::priceset_range_nonabort_scalar,
        )
    }

    /// wasm `simd128`-umbrella DP body: scalar candidate collection (no wasm
    /// collect kernel) but the simd128 4-lane price-set.
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    #[target_feature(enable = "simd128")]
    // With `+simd128` in the wasm baseline the shared body macro's `unsafe`
    // blocks (needed by the safe scalar wrapper) are redundant inside this
    // target_feature fn.
    #[allow(unused_unsafe)]
    unsafe fn build_optimal_plan_impl_simd128<
        S: crate::encoding::strategy::Strategy,
        const ACCURATE_PRICE: bool,
        const FAVOR_SMALL_OFFSETS: bool,
    >(
        &mut self,
        current: &[u8],
        current_abs_start: usize,
        current_len: usize,
        initial_state: HcOptimalPlanState,
        stats: &HcOptState,
        out: &mut Vec<HcOptimalSequence>,
    ) -> (u32, [u32; 3], usize, usize) {
        build_optimal_plan_impl_body!(
            self,
            S,
            current,
            current_abs_start,
            current_len,
            initial_state,
            stats,
            out,
            collect_optimal_candidates_initialized_scalar,
            crate::encoding::hc::priceset::priceset_range_nonabort_simd128,
        )
    }

    #[cfg(test)]
    pub(crate) fn collect_optimal_candidates(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        use crate::encoding::strategy::{self, StrategyTag};
        self.table.ensure_tables();
        // Dispatch purely from `self.strategy_tag` (set by
        // `configure()`). Tests must configure the matcher the same
        // way production does — wiring up `table.hash3_log` directly
        // without setting a matching `strategy_tag` is no longer
        // allowed.
        match self.strategy_tag {
            StrategyTag::BtUltra2 => self
                .collect_optimal_candidates_initialized::<strategy::BtUltra2, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::BtUltra => self
                .collect_optimal_candidates_initialized::<strategy::BtUltra, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::Btlazy2 => self
                .collect_optimal_candidates_initialized::<strategy::Btlazy2, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::BtOpt => self
                .collect_optimal_candidates_initialized::<strategy::BtOpt, true>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                ),
            StrategyTag::Fast | StrategyTag::Dfast | StrategyTag::Greedy | StrategyTag::Lazy => {
                self.collect_optimal_candidates_initialized::<strategy::Lazy, false>(
                    abs_pos,
                    current_abs_end,
                    profile,
                    query,
                    out,
                )
            }
        }
    }

    /// Cross-platform entry. Picks the kernel-specific variant so the per-
    /// position pipeline (BT-tree fill, rep probing, hash3 probing, BT
    /// collect / HC chain walk) runs inside a single `target_feature`
    /// umbrella — all inner SIMD probes inline without ABI barriers.
    ///
    /// The on-encode hot path bypasses this dispatcher: `build_optimal_plan_impl_<kernel>`
    /// calls the matching `_<kernel>` variant directly. This entry is kept
    /// for the cfg(test)-only `collect_optimal_candidates` shim and any
    /// future caller that isn't already inside a kernel umbrella.
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn collect_optimal_candidates_initialized<
        S: crate::encoding::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.collect_optimal_candidates_initialized_neon::<S, USE_BT_MATCHFINDER>(
                abs_pos,
                current_abs_end,
                profile,
                query,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.collect_optimal_candidates_initialized_avx2_bmi2::<S, USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.collect_optimal_candidates_initialized_sse42::<S, USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    )
                },
                FastpathKernel::Scalar => self
                    .collect_optimal_candidates_initialized_scalar::<S, USE_BT_MATCHFINDER>(
                        abs_pos,
                        current_abs_end,
                        profile,
                        query,
                        out,
                    ),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.collect_optimal_candidates_initialized_scalar::<S, USE_BT_MATCHFINDER>(
                abs_pos,
                current_abs_end,
                profile,
                query,
                out,
            )
        }
    }

    /// NEON-umbrella variant. Every inner helper (`bt_update_tree_until_neon`,
    /// `for_each_repcode_candidate_with_reps_neon`, `hash3_candidate_neon`,
    /// `bt_insert_and_collect_matches_neon`, `fastpath::neon::
    /// common_prefix_len_ptr`) shares the NEON umbrella so the per-position
    /// pipeline executes as a single straight-line inline sequence.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn collect_optimal_candidates_initialized_neon<
        S: crate::encoding::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_neon,
            bt_insert_and_collect_matches_neon,
            for_each_repcode_candidate_with_reps_neon,
            hash3_candidate_neon,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn collect_optimal_candidates_initialized_sse42<
        S: crate::encoding::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_sse42,
            bt_insert_and_collect_matches_sse42,
            for_each_repcode_candidate_with_reps_sse42,
            hash3_candidate_sse42,
            crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn collect_optimal_candidates_initialized_avx2_bmi2<
        S: crate::encoding::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_avx2_bmi2,
            bt_insert_and_collect_matches_avx2_bmi2,
            for_each_repcode_candidate_with_reps_avx2_bmi2,
            hash3_candidate_avx2_bmi2,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        )
    }

    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    // Macro emits `unsafe { }` wrappers for NEON/AVX/SSE variants; scalar
    // callees are safe so the blocks are redundant here only.
    #[allow(unused_unsafe)]
    pub(crate) fn collect_optimal_candidates_initialized_scalar<
        S: crate::encoding::strategy::Strategy,
        const USE_BT_MATCHFINDER: bool,
    >(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        query: HcCandidateQuery,
        out: &mut Vec<MatchCandidate>,
    ) {
        collect_optimal_candidates_initialized_body!(
            self,
            S,
            abs_pos,
            current_abs_end,
            profile,
            query,
            out,
            USE_BT_MATCHFINDER,
            bt_update_tree_until_scalar,
            bt_insert_and_collect_matches_scalar,
            for_each_repcode_candidate_with_reps_scalar,
            hash3_candidate_scalar,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
        )
    }
}

#[cfg(any())] // disabled: tested legacy MatchGenerator/SuffixStore behavior removed in phase 1b
#[test]
fn matches() {
    let mut matcher = MatchGenerator::new(1000);
    let mut original_data = Vec::new();
    let mut reconstructed = Vec::new();

    let replay_sequence = |seq: Sequence<'_>, reconstructed: &mut Vec<u8>| match seq {
        Sequence::Literals { literals } => {
            assert!(!literals.is_empty());
            reconstructed.extend_from_slice(literals);
        }
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            assert!(offset > 0);
            assert!(match_len >= MIN_MATCH_LEN);
            reconstructed.extend_from_slice(literals);
            assert!(offset <= reconstructed.len());
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    };

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));

    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![
            1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
        ],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[
        1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
    ]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![7, 8, 9, 10, 11],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[7, 8, 9, 10, 11]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    matcher.skip_matching();
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);
    reconstructed.extend_from_slice(&[1, 3, 5, 7, 9]);
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23]);

    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    matcher.next_sequence(|seq| replay_sequence(seq, &mut reconstructed));
    assert!(!matcher.next_sequence(|_| {}));

    assert_eq!(reconstructed, original_data);
}

#[test]
fn dfast_matches_roundtrip_multi_block_pattern() {
    let pattern = [9, 21, 44, 184, 19, 96, 171, 109, 141, 251];
    let first_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();
    let second_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();

    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let replay_sequence = |decoded: &mut Vec<u8>, seq: Sequence<'_>| match seq {
        Sequence::Literals { literals } => decoded.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            decoded.extend_from_slice(literals);
            let start = decoded.len() - offset;
            for i in 0..match_len {
                let byte = decoded[start + i];
                decoded.push(byte);
            }
        }
    };

    matcher.add_data(first_block.clone(), |_| {});
    let mut history = Vec::new();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));
    assert_eq!(history, first_block);

    matcher.add_data(second_block.clone(), |_| {});
    let prefix_len = history.len();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));

    assert_eq!(&history[prefix_len..], second_block.as_slice());
}

/// Regression for the `DFAST_MIN_MATCH_LEN: 6 -> 5` drop. The fixture
/// is built so the longest available match is EXACTLY 5 bytes — a
/// matcher that still effectively requires a 6-byte floor would emit
/// only literals here and the assertion would catch the silent
/// 5-byte miss.
///
/// Fixture layout (34 B):
///   bytes 0..5    `"ABCDE"`  — match source
///   bytes 5..28   `'!'` × 23 — filler that does NOT start with 'A'
///   bytes 28..33  `"ABCDE"`  — match site (repeats the prefix)
///   byte  33      `'F'`      — terminator: differs from byte 5 (`'!'`),
///                              so the forward extension at the match
///                              site stops at exactly length 5.
///
/// A 5-byte match at offset 28 must be emitted; a 6-byte+ match at the
/// same offset must NOT.
#[test]
fn dfast_accepts_exact_five_byte_match() {
    // Layout the input so that:
    //   byte  0      = 'Z'            (lead byte — keeps the match SOURCE off
    //                                  position 0, which the greedy loop never
    //                                  inserts: like the upstream zstd it starts the
    //                                  cursor at ip+1 and hashes only visited
    //                                  positions)
    //   bytes 1..6   = "ABCDE"        (the match source — position 1 IS visited)
    //   bytes 6..29  = 23 filler bytes that do NOT start with 'A'
    //   bytes 29..34 = "ABCDE"        (the 5-byte match site)
    //   byte  34     = 'F'            (differs from byte 6 = '!')
    // The longest available copy at position 29 is exactly 5 bytes:
    // the byte at position 34 ('F') differs from the byte at position 6
    // ('!'), so the forward extension stops at length 5.
    let mut data = Vec::new();
    data.push(b'Z'); // 0
    data.extend_from_slice(b"ABCDE"); // 1..6
    data.extend_from_slice(b"!!!!!!!!!!!!!!!!!!!!!!!"); // 6..29 (23 bytes)
    data.extend_from_slice(b"ABCDE"); // 29..34
    data.push(b'F'); // 34: forces forward extension to stop at length 5
    // Trailing filler so the match site (29) sits at least HASH_READ_SIZE (8)
    // bytes before the block end. The greedy double-fast — like the upstream zstd —
    // stops probing at `ilimit = iend - HASH_READ_SIZE`, so a match in the
    // final 8 bytes is never searched (upstream zstd parity, not a regression).
    data.extend_from_slice(b"GHIJKLMNOPQRSTUVWXYZ"); // 35..55
    assert_eq!(data.len(), 55);

    let mut matcher = DfastMatchGenerator::new(1 << 22);
    matcher.add_data(data.clone(), |_| {});

    let mut saw_five_byte_match = false;
    let mut saw_longer_match = false;
    matcher.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
        {
            if offset == 28 && match_len == 5 {
                saw_five_byte_match = true;
            } else if offset == 28 && match_len > 5 {
                saw_longer_match = true;
            }
        }
    });

    assert!(
        saw_five_byte_match,
        "dfast must accept the exact-5-byte match — a 6-byte floor would skip it"
    );
    assert!(
        !saw_longer_match,
        "fixture pinned to length 5 — byte 33 ('F') must terminate the extension"
    );
}

#[test]
fn driver_switches_backends_and_initializes_dfast_via_reset() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Default);
    assert_eq!(
        driver.active_backend(),
        crate::encoding::strategy::BackendTag::Dfast
    );
    assert_eq!(driver.window_size(), (1u64 << 21));

    let mut first = driver.get_next_space();
    first[..12].copy_from_slice(b"abcabcabcabc");
    first.truncate(12);
    driver.commit_space(first);
    assert_eq!(driver.get_last_space(), b"abcabcabcabc");
    driver.skip_matching_with_hint(None);

    let mut second = driver.get_next_space();
    second[..12].copy_from_slice(b"abcabcabcabc");
    second.truncate(12);
    driver.commit_space(second);

    let mut reconstructed = b"abcabcabcabc".to_vec();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(reconstructed, b"abcabcabcabcabcabcabcabc");

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.window_size(), (1u64 << 19));
}

#[test]
fn driver_level5_selects_row_backend() {
    let mut driver = MatchGeneratorDriver::new(32, 2);
    driver.reset(CompressionLevel::Level(5));
    assert_eq!(
        driver.active_backend(),
        crate::encoding::strategy::BackendTag::Row
    );
    // Greedy-specific routing assertion: `MatchGeneratorDriver::start_matching`
    // dispatches the Row backend into `start_matching_greedy` iff
    // `self.parse == ParseMode::Greedy`, so assert that actual selector —
    // round-trip alone passes on the lazy parser too. `row_matcher().lazy_depth`
    // is a secondary corroboration of the same routing decision (a mirror of
    // the parse mode); checking `parse` directly catches a regression even if
    // the two ever drift apart.
    assert_eq!(
        driver.parse,
        crate::encoding::strategy::ParseMode::Greedy,
        "L5 must route to start_matching_greedy (parse == Greedy)",
    );
    assert_eq!(
        driver.row_matcher().lazy_depth,
        0,
        "row matcher lazy_depth must mirror the greedy parse mode",
    );
}

/// Level 4 maps to `StrategyTag::Dfast` (the greedy double-fast, upstream zstd
/// `ZSTD_dfast` — "greedy" is the parse discipline, not the Row/Greedy
/// strategy at Level 5). Round-trip alone doesn't pin match quality (a lazy
/// parser would also reconstruct the input correctly), so this test guards the
/// parse output itself: a small repeating pattern must produce at least one
/// `Sequence::Triple`, so a future regression that emits literals-only (e.g. a
/// `min_match` or rep-probe guard regression) is caught.
#[test]
fn driver_level4_greedy_round_trip_single_slice() {
    let mut driver = MatchGeneratorDriver::new(64, 2);
    driver.reset(CompressionLevel::Level(4));
    let input = b"abcdefgh_abcdefgh_abcdefgh_abcdefgh";
    let mut space = driver.get_next_space();
    space[..input.len()].copy_from_slice(input);
    space.truncate(input.len());
    driver.commit_space(space);

    let mut reconstructed: Vec<u8> = Vec::new();
    let mut saw_triple = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            saw_triple = true;
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(
        reconstructed,
        input.to_vec(),
        "L4 greedy parse failed to reconstruct repeating-pattern input",
    );
    assert!(
        saw_triple,
        "L4 greedy parse on a repeating pattern must emit at least one match (Triple)",
    );
}

#[test]
fn driver_level4_greedy_round_trip_cross_slice() {
    // Verifies that the greedy parse carries repcode / hash-table state
    // across slice boundaries: the second slice repeats the first byte
    // for byte, so the parse must pick up matches reaching back into
    // the previous slice's history.
    let mut driver = MatchGeneratorDriver::new(32, 4);
    driver.reset(CompressionLevel::Level(4));
    let chunk = b"the quick brown fox jumps over!!";
    assert_eq!(chunk.len(), 32);

    let mut first = driver.get_next_space();
    first[..chunk.len()].copy_from_slice(chunk);
    first.truncate(chunk.len());
    driver.commit_space(first);

    let mut first_recon: Vec<u8> = Vec::new();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => first_recon.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            first_recon.extend_from_slice(literals);
            let start = first_recon.len() - offset;
            for i in 0..match_len {
                let byte = first_recon[start + i];
                first_recon.push(byte);
            }
        }
    });
    assert_eq!(
        first_recon,
        chunk.to_vec(),
        "first slice failed to round-trip"
    );

    let mut second = driver.get_next_space();
    second[..chunk.len()].copy_from_slice(chunk);
    second.truncate(chunk.len());
    driver.commit_space(second);

    let mut full = first_recon.clone();
    let mut saw_cross_slice_match = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => full.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            // A match whose offset reaches >= the current slice's literal
            // run plus the second slice's index means we matched into the
            // first slice — exactly the cross-slice behavior under test.
            if offset >= chunk.len() {
                saw_cross_slice_match = true;
            }
            full.extend_from_slice(literals);
            let start = full.len() - offset;
            for i in 0..match_len {
                let byte = full[start + i];
                full.push(byte);
            }
        }
    });
    let mut expected = chunk.to_vec();
    expected.extend_from_slice(chunk);
    assert_eq!(
        full, expected,
        "cross-slice L4 greedy parse failed to reconstruct"
    );
    assert!(
        saw_cross_slice_match,
        "L4 greedy parse must match across slice boundaries (history is shared)",
    );
}
