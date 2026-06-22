//! Hash-chain match finder used by `Lazy2`.
//!
//! Hosts the runtime knobs of the lazy parser — the lookahead depth
//! (`lazy_depth`), the chain-walk search budget (`search_depth`), and
//! the "sufficient match length" threshold (`target_len`). Method
//! bodies (chain walk, `insert_position`, `pick_lazy_match`,
//! `start_matching_lazy`) still live on `HcMatchGenerator` and will
//! move onto `impl HcMatcher` in Stage 2b alongside the
//! `&mut MatchTable` thread-through; this stage establishes the
//! ownership boundary so the BT-side extraction in Stage 3 has a
//! clean counterpart type to mirror.
//!
//! Upstream zstd parity reference: `lib/compress/zstd_lazy.c`,
//! `ZSTD_HcFindBestMatch` / `ZSTD_compressBlock_lazy2_generic`.

#![allow(dead_code)]

use super::cost_model::{HC_FORMAT_MINMATCH, HC_OPT_NUM, HcOptimalCostProfile};
use super::match_table::helpers::common_prefix_len;
use super::match_table::storage::{HC_EMPTY, MatchTable};
use super::opt::types::MatchCandidate;

/// Minimum match length emitted by the lazy / lazy2 chain walker.
/// Upstream zstd parity: `MIN_MATCH` in `lib/compress/zstd_lazy.c`.
pub(crate) const HC_MIN_MATCH_LEN: usize = 4;

/// Hard cap on chain-walk depth. Used to size the fixed-length
/// candidate buffer returned by [`HcMatcher::chain_candidates`].
pub(crate) const MAX_HC_SEARCH_DEPTH: usize = 512;

/// Hash-chain matcher state used by the `Lazy2` parse mode (and the
/// short-history fast path of the BT cascade's initial pass).
///
/// Owns only the per-frame *configuration* — the actual chain / hash
/// tables live on the shared
/// [`super::match_table::storage::MatchTable`] that this matcher
/// borrows when it runs.
#[derive(Clone)]
pub(crate) struct HcMatcher {
    /// Lookahead depth (1 = lazy, 2 = lazy2). Upstream zstd parity:
    /// `params->cParams.strategy >= ZSTD_lazy2`.
    pub(crate) lazy_depth: u8,
    /// Maximum number of chain entries inspected per `find_best_match`
    /// call. Upstream zstd parity: `params->cParams.searchLog` (clamped to
    /// [`MAX_HC_SEARCH_DEPTH`](super::match_generator::MAX_HC_SEARCH_DEPTH)
    /// for HC mode; BT modes use the unclamped value as their walk
    /// budget).
    pub(crate) search_depth: usize,
    /// "Sufficient" match length — once a candidate reaches this
    /// length, the lazy decision short-circuits without checking the
    /// next position. Upstream zstd parity:
    /// `params->cParams.targetLength`.
    pub(crate) target_len: usize,
}

impl HcMatcher {
    pub(crate) fn new(lazy_depth: u8, search_depth: usize, target_len: usize) -> Self {
        Self {
            lazy_depth,
            search_depth,
            target_len,
        }
    }

    /// Upstream zstd "match gain" heuristic: `match_len * 4 - offset_bits`.
    /// The lazy lookahead uses this to compare a candidate at the
    /// current position against one a byte (or two) ahead. Pure
    /// associated function — kept off `&self` so it can be called
    /// statically from inside `better_candidate`.
    #[inline]
    pub(crate) fn match_gain(match_len: usize, offset: usize) -> i32 {
        debug_assert!(
            offset > 0,
            "zstd offsets are 1-indexed, offset=0 is invalid"
        );
        let offset_bits = 32 - (offset as u32).leading_zeros() as i32;
        (match_len as i32) * 4 - offset_bits
    }

    /// Pick the better of two candidate matches by [`match_gain`].
    /// `None` arms pass the surviving `Some` through.
    pub(crate) fn better_candidate(
        lhs: Option<MatchCandidate>,
        rhs: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        match (lhs, rhs) {
            (None, other) | (other, None) => other,
            (Some(lhs), Some(rhs)) => {
                let lhs_gain = Self::match_gain(lhs.match_len, lhs.offset);
                let rhs_gain = Self::match_gain(rhs.match_len, rhs.offset);
                if rhs_gain > lhs_gain {
                    Some(rhs)
                } else {
                    Some(lhs)
                }
            }
        }
    }

    /// Walk the hash chain at `abs_pos` and collect up to
    /// [`HcMatcher::search_depth`] absolute positions of in-window
    /// candidates. Stale chain entries (positions evicted from the
    /// window) are skipped rather than terminating the walk; the
    /// chain is bounded by `search_depth` total iterations to keep
    /// pathological self-loops from spinning.
    pub(crate) fn chain_candidates(
        &self,
        table: &MatchTable,
        abs_pos: usize,
    ) -> [usize; MAX_HC_SEARCH_DEPTH] {
        let mut buf = [usize::MAX; MAX_HC_SEARCH_DEPTH];
        let idx = abs_pos - table.history_abs_start;
        let concat = table.live_history();
        if idx + 4 > concat.len() {
            return buf;
        }
        let hash = table.hash_position(&concat[idx..]);
        let chain_mask = (1 << table.chain_log) - 1;

        let mut cur = table.hash_table[hash];
        let mut filled = 0;
        let mut steps = 0;
        // Cap both the loop bound and the result-fill bound at
        // MAX_HC_SEARCH_DEPTH so a misconfigured `search_depth >
        // MAX_HC_SEARCH_DEPTH` (BT modes set it from the upstream zstd config,
        // which can exceed 64) cannot index past `buf`'s fixed size.
        let max_chain_steps = self.search_depth.min(MAX_HC_SEARCH_DEPTH);
        while filled < max_chain_steps && steps < max_chain_steps {
            if cur == HC_EMPTY {
                break;
            }
            let candidate_rel = cur.wrapping_sub(1) as usize;
            // Decode through `stored_abs_position_fast` so a non-zero
            // `index_shift` (set by future rebase variants) is honored;
            // raw `position_base + candidate_rel` would silently
            // misread rebased entries.
            let candidate_abs = super::match_table::storage::MatchTable::stored_abs_position_fast(
                cur,
                table.position_base,
                table.index_shift,
            );
            let next = table.chain_table[candidate_rel & chain_mask];
            steps += 1;
            if next == cur {
                // Self-loop: two positions share chain_idx, stop to
                // avoid spinning on the same candidate forever.
                if let Some(candidate_abs) = candidate_abs.filter(|&p| {
                    p >= table
                        .history_abs_start
                        .max(abs_pos.saturating_sub(table.max_window_size))
                        && p < abs_pos
                }) {
                    buf[filled] = candidate_abs;
                }
                break;
            }
            cur = next;
            let Some(candidate_abs) = candidate_abs else {
                continue;
            };
            if candidate_abs
                < table
                    .history_abs_start
                    .max(abs_pos.saturating_sub(table.max_window_size))
                || candidate_abs >= abs_pos
            {
                continue;
            }
            buf[filled] = candidate_abs;
            filled += 1;
        }
        buf
    }

    /// Probe the 3 rep-code offsets (with the upstream zstd `ll0 ↦ rep[0] − 1`
    /// fallback) and return the best in-range match. Pure helper —
    /// only reads from `MatchTable`, no HcMatcher state needed.
    pub(crate) fn repcode_candidate(
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        let reps = if lit_len == 0 {
            [
                Some(table.offset_hist[1] as usize),
                Some(table.offset_hist[2] as usize),
                (table.offset_hist[0] > 1).then_some((table.offset_hist[0] - 1) as usize),
            ]
        } else {
            [
                Some(table.offset_hist[0] as usize),
                Some(table.offset_hist[1] as usize),
                Some(table.offset_hist[2] as usize),
            ]
        };

        let concat = table.live_history();
        let current_idx = abs_pos - table.history_abs_start;
        if current_idx + HC_MIN_MATCH_LEN > concat.len() {
            return None;
        }
        // Raw base pointer for the upstream zstd-style `MEM_read32` 4-byte rep
        // gate below (single unaligned load each, no per-rep slice bounds check).
        let base_ptr = concat.as_ptr();

        let mut best = None;
        for rep in reps.into_iter().flatten() {
            if rep == 0 || rep > abs_pos {
                continue;
            }
            let candidate_pos = abs_pos - rep;
            if candidate_pos
                < table
                    .history_abs_start
                    .max(abs_pos.saturating_sub(table.max_window_size))
            {
                continue;
            }
            let candidate_idx = candidate_pos - table.history_abs_start;
            // Cheap 4-byte equality gate before the wider SIMD count (upstream
            // zstd `MEM_read32` repcode gate in `ZSTD_compressBlock_lazy`).
            // `HC_MIN_MATCH_LEN == 4` and `current_idx + 4 <= len` (the early
            // return above), so a first-4-byte mismatch can never reach the
            // match floor — reject without the vector load+count. Falls through
            // to the full count only when the candidate lacks a 4-byte lookahead
            // (short tail), so the accepted set is byte-identical to the
            // unconditional count. Mirrors the chain-walk gate.
            if candidate_idx + 4 <= concat.len()
                && unsafe {
                    MatchTable::read_le_u32_ptr(base_ptr.add(candidate_idx))
                        != MatchTable::read_le_u32_ptr(base_ptr.add(current_idx))
                }
            {
                continue;
            }
            let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= HC_MIN_MATCH_LEN {
                let candidate =
                    Self::extend_backwards(table, candidate_pos, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
            }
        }
        best
    }

    /// Best hash-chain match at `abs_pos`, in upstream zstd's
    /// `ZSTD_HcFindBestMatch` forward-length model: walk the live chain (and
    /// the dms) tracking the longest FORWARD match (`currentMl > ml`, ties
    /// keep the closest), gate each candidate on the self-tightening 4-byte
    /// tail probe, and extend the single winner backwards over the literal
    /// run ONCE at the end. No per-candidate `Option` / gain merge / backward
    /// extension — that per-attempt overhead is what the old gain-based walk
    /// paid over C.
    pub(crate) fn hash_chain_candidate<const DICT: bool>(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        let concat = table.live_history();
        let current_idx = abs_pos - table.history_abs_start;
        let history_tail = concat.len();
        if current_idx + HC_MIN_MATCH_LEN > history_tail {
            return None;
        }

        // Chain walk is inlined below — avoids the per-call 4 KiB
        // `[usize; MAX_HC_SEARCH_DEPTH]` array that
        // [`Self::chain_candidates`] materializes (zero-init on entry,
        // memcpy on return). At lazy_depth=2 (L7+) `pick_lazy_match`
        // triggers up to three chain walks per committed position, so
        // the array form costs ~12 KiB of stack traffic per accepted
        // match. Upstream zstd (`zstd_lazy.c` `ZSTD_HcFindBestMatch`) runs a
        // single fused loop with no intermediate buffer; mirror that.
        //
        // `chain_candidates` itself is still alive — the chain-walk
        // unit tests drive it directly, and the BT-optimal HC
        // candidate collector in `match_generator.rs` consumes it
        // through a macro pipeline that inherits the array form.
        // Inlining the array out of that BT-optimal callsite is a
        // separate, larger refactor; this commit only addresses the
        // lazy hot path.
        let hash = table.hash_position(&concat[current_idx..]);
        let chain_mask = (1usize << table.chain_log) - 1;
        let mut cur = table.hash_table[hash];
        // Cap loop at MAX_HC_SEARCH_DEPTH so a misconfigured
        // `search_depth > MAX_HC_SEARCH_DEPTH` (BT modes set it from the
        // upstream zstd config, which can exceed our cap) cannot run forever.
        let max_chain_steps = self.search_depth.min(MAX_HC_SEARCH_DEPTH);
        let mut steps = 0usize;
        let history_abs_start = table.history_abs_start;

        // Forward-length match state (upstream `ZSTD_HcFindBestMatch`): `ml` is
        // the FORWARD match length, seeded at `MIN_MATCH - 1` so the first
        // candidate's self-tightening gate (`match + ml - 3`) degenerates to a
        // first-4-byte compare. `best_idx` is the winning candidate's concat
        // index; `found` flips once `ml` reaches `MIN_MATCH`.
        let mut ml = HC_MIN_MATCH_LEN - 1;
        let mut best_idx = 0usize;
        let mut found = false;
        // Raw base pointer for the upstream zstd-style `MEM_read32` gate
        // (single unaligned load each, no per-candidate slice bounds check).
        let base_ptr = concat.as_ptr();
        // Loop-invariant precompute, hoisted out of the chain walk (upstream zstd:
        // `lowLimit` + base pointers are set once before the loop). Candidates
        // are tracked in history-RELATIVE index space so each step is a single
        // `add` + range check + 4-byte gate, mirroring `ZSTD_HcFindBestMatch`'s
        // tight body (`matchIndex >= lowLimit`; `NEXT_IN_CHAIN`) instead of an
        // `Option`-returning absolute-position decode plus a per-candidate
        // `.max()/.saturating_sub()` window-floor recompute.
        let floor_idx = current_idx.saturating_sub(table.max_window_size);
        // `candidate_idx = position_base + (cur-1) - index_shift -
        // history_abs_start`, folded into one wrapping bias so the per-step cost
        // is `cur + idx_bias`. Stale / sub-floor chain entries wrap to a huge
        // index and are rejected by the `< current_idx` upper bound — the
        // accepted set is identical to the `stored_abs_position_fast` decode
        // (which returned `None` for the same sub-floor entries).
        let idx_bias = table
            .position_base
            .wrapping_sub(1)
            .wrapping_sub(table.index_shift)
            .wrapping_sub(history_abs_start);
        while steps < max_chain_steps {
            if cur == HC_EMPTY {
                break;
            }
            let candidate_rel = cur.wrapping_sub(1) as usize;
            let next = table.chain_table[candidate_rel & chain_mask];
            steps += 1;
            // Self-loop: two positions share `candidate_rel & chain_mask`.
            let self_loop = next == cur;

            let candidate_idx = (cur as usize).wrapping_add(idx_bias);
            // A wrapped index (`>= current_idx`) means `abs < history_abs_start`:
            // a stale entry from a previous frame. The chain is head-insert
            // monotonic, so the rest of the tail is older and also stale -- stop.
            // This is the early-exit the no-memset floor-advance reset needs; in
            // the memset reset the chain has no wrapped entries, so it never
            // fires. Below-floor (`< floor_idx`) entries are NOT a break: a
            // chain-slot collision can put an in-window entry after one.
            if candidate_idx >= current_idx {
                break;
            }
            if candidate_idx >= floor_idx {
                // Upstream zstd's single self-tightening gate (`zstd_lazy.c:714`):
                //   MEM_read32(match + ml - 3) == MEM_read32(ip + ml - 3)
                // proves the candidate can possibly reach `ml + 1` before paying
                // for the full `common_prefix_len`. `ml >= MIN_MATCH - 1 = 3` so
                // `gate_off >= 0`. The iLimit break below keeps
                // `current_idx + ml < history_tail` on entry, so
                // `current_idx + gate_off + 4 = current_idx + ml + 1 <= history_tail`,
                // and `candidate_idx < current_idx` keeps the match-side read in
                // range too -- no per-iteration bounds check (matches C's
                // branchless gate).
                let gate_off = ml - 3;
                // SAFETY: both reads are `<= history_tail` per the bound above.
                let gate_ok = unsafe {
                    MatchTable::read_le_u32_ptr(base_ptr.add(candidate_idx + gate_off))
                        == MatchTable::read_le_u32_ptr(base_ptr.add(current_idx + gate_off))
                };
                if gate_ok {
                    let current_ml =
                        common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
                    // `currentMl > ml`: strictly longer forward match wins; ties
                    // keep the first (closest, the walk is newest-first).
                    if current_ml > ml {
                        ml = current_ml;
                        best_idx = candidate_idx;
                        found = true;
                        // Upstream `if (ip + currentMl == iLimit) break`: reached
                        // the input end (best possible); also keeps the next gate
                        // read in bounds.
                        if current_idx + ml >= history_tail {
                            break;
                        }
                    }
                }
            }

            if self_loop {
                break;
            }
            cur = next;
        }

        // Separate dictionary match state (upstream `ZSTD_dictMatchState`). The
        // walk is OUT-OF-LINE so the dict-only code never bloats this hot
        // function on no-dict frames. It shares `ml` / `steps` (upstream's
        // single `nbAttempts` across the live + dms loops). Skip it when the
        // live walk already reached iLimit (`ml` maximal, and the dms
        // self-tightening gate would read past `history_tail`).
        if DICT && table.dms.is_primed() && current_idx + ml < history_tail {
            let walk = self.dms_chain_walk(
                table,
                concat,
                current_idx,
                history_tail,
                base_ptr,
                max_chain_steps,
                steps,
                ml,
                best_idx,
                found,
            );
            ml = walk.0;
            best_idx = walk.1;
            found = walk.2;
        }

        if !found {
            return None;
        }
        // Single backward extension on the winner (upstream does this once in
        // the lazy loop after rep-vs-chain selection; folded into the find here
        // so `find_best_match` / the lazy loop keep the `MatchCandidate`
        // contract). The extension preserves the forward offset.
        Some(Self::extend_backwards(
            table,
            best_idx + history_abs_start,
            abs_pos,
            ml,
            lit_len,
        ))
    }

    /// Out-of-line dms HC4 walk (upstream `ZSTD_HcFindBestMatch` dms loop,
    /// `zstd_lazy.c:751-769`). Split from [`Self::hash_chain_candidate`] so the
    /// no-dict hot path keeps its small body / register budget. Shares the
    /// caller's forward-length state (`ml` / `best_idx` / `found`) and `steps`
    /// budget -- the live + dms loops are one bounded operation (upstream's
    /// single `nbAttempts`). The dict sits at the front of `concat`
    /// (`[0, region)`); a dms candidate is a concat index `< current_idx`, so
    /// the offset / gate / count logic matches the live walk. Returns the
    /// updated `(ml, best_idx, found)`.
    ///
    /// `#[inline]`: the `DICT = false` monomorph never references this (the
    /// `if DICT &&` gate is a compile-time false), so it is dropped from the
    /// no-dict path; the `DICT = true` monomorph inlines it into the dict hot
    /// path.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn dms_chain_walk(
        &self,
        table: &MatchTable,
        concat: &[u8],
        current_idx: usize,
        history_tail: usize,
        base_ptr: *const u8,
        max_chain_steps: usize,
        mut steps: usize,
        mut ml: usize,
        mut best_idx: usize,
        mut found: bool,
    ) -> (usize, usize, bool) {
        let dms = match table.dms.table() {
            Some(d) => d,
            None => return (ml, best_idx, found),
        };
        // Match upstream's effective dict search depth: the CDict path runs the
        // dedicated dict search deeper than the bare level searchLog (measured:
        // upstream needs nbAttempts >= 16 to surface the long dict match on the
        // per-label-dict fixtures). Floor the shared budget at 16.
        let dms_budget = max_chain_steps.max(16);
        let dms_hash = MatchTable::hash_position_at(concat, current_idx, dms.hash_log, dms.mls);
        let mut dcur = dms.hash_table[dms_hash];
        while steps < dms_budget {
            if dcur == 0 {
                break;
            }
            // Dict position is a concat index in `[0, region)`; the dict is at
            // the front so `dict_idx < current_idx` always.
            let dict_idx = (dcur - 1) as usize;
            let dnext = dms.chain_table[dict_idx];
            steps += 1;
            let new_offset = current_idx - dict_idx;
            // Out-of-window dict positions are unreachable to the decoder.
            if new_offset <= table.max_window_size {
                // Same self-tightening gate as the live walk; the caller's guard
                // (`current_idx + ml < history_tail`) + the iLimit break keep
                // `current_idx + (ml - 3) + 4 <= history_tail`, and
                // `dict_idx < current_idx` keeps the dict-side read in range.
                let gate_off = ml - 3;
                // SAFETY: both reads are `<= history_tail` per the bound above.
                let gate_ok = unsafe {
                    MatchTable::read_le_u32_ptr(base_ptr.add(dict_idx + gate_off))
                        == MatchTable::read_le_u32_ptr(base_ptr.add(current_idx + gate_off))
                };
                if gate_ok {
                    let current_ml =
                        common_prefix_len(&concat[dict_idx..], &concat[current_idx..]);
                    if current_ml > ml {
                        ml = current_ml;
                        best_idx = dict_idx;
                        found = true;
                        if current_idx + ml >= history_tail {
                            break;
                        }
                    }
                }
            }
            // Chain links are strictly decreasing (head insert); `dnext == dcur`
            // (or the `dcur == 0` at the top) ends the walk.
            if dnext == dcur {
                break;
            }
            dcur = dnext;
        }
        (ml, best_idx, found)
    }

    /// Combine the rep-code and chain-walk candidates and pick the
    /// better of the two. Monomorphised over `DICT` (upstream's compile-time
    /// `dictMode` template param): the `DICT = false` instance compiles WITHOUT
    /// any dms code so the no-dict hot path keeps its tight body, while the
    /// `DICT = true` instance dual-probes the separate dictMatchState. The
    /// dispatcher in `start_matching_lazy` picks the instance from whether a dms
    /// is primed.
    pub(crate) fn find_best_match<const DICT: bool>(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        let rep = Self::repcode_candidate(table, abs_pos, lit_len);
        let hash = self.hash_chain_candidate::<DICT>(table, abs_pos, lit_len);
        Self::better_candidate(rep, hash)
    }

    /// Upstream zstd `lazy` / `lazy2` lookahead: evaluate the match a byte
    /// (and optionally two) ahead before committing the current one.
    /// Returns `Some(best)` if the current match wins, `None` if the
    /// caller should defer.
    ///
    /// Lazy lookahead queries `pos + 1` / `pos + 2` before they are
    /// inserted into the hash tables — matching the C zstd ordering.
    /// Seeding before comparing would let a position match against
    /// itself, changing semantics.
    pub(crate) fn pick_lazy_match<const DICT: bool>(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        let best = best?;
        if best.match_len >= self.target_len
            || abs_pos + 1 + HC_MIN_MATCH_LEN > table.history_abs_end()
        {
            return Some(best);
        }

        let current_gain = Self::match_gain(best.match_len, best.offset) + 4;

        let next = self.find_best_match::<DICT>(table, abs_pos + 1, lit_len + 1);
        if let Some(next) = next {
            let next_gain = Self::match_gain(next.match_len, next.offset);
            if next_gain > current_gain {
                return None;
            }
        }

        if self.lazy_depth >= 2 && abs_pos + 2 + HC_MIN_MATCH_LEN <= table.history_abs_end() {
            let next2 = self.find_best_match::<DICT>(table, abs_pos + 2, lit_len + 2);
            if let Some(next2) = next2 {
                let next2_gain = Self::match_gain(next2.match_len, next2.offset);
                if next2_gain > current_gain + 4 {
                    return None;
                }
            }
        }

        Some(best)
    }

    /// Cross-platform dispatcher for the rep-code probe used by the
    /// optimal-parser pipeline. Routes to the kernel-specific variant
    /// so the per-rep `common_prefix_len_ptr` call inlines under the
    /// callee's `target_feature` umbrella. Test / external callers
    /// only — the on-encode hot path bypasses this dispatcher via the
    /// kernel-specific variants invoked from inside
    /// `collect_optimal_candidates_initialized_<kernel>`.
    #[allow(dead_code)]
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_each_repcode_candidate_with_reps(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
        reps: [u32; 3],
        current_abs_end: usize,
        min_match_len: usize,
        f: impl FnMut(MatchCandidate),
    ) {
        // SAFETY: each branch verifies the target_feature requirement of
        // the callee (same shape as the BT walk dispatchers).
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.for_each_repcode_candidate_with_reps_neon(
                table,
                abs_pos,
                lit_len,
                reps,
                current_abs_end,
                min_match_len,
                f,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.for_each_repcode_candidate_with_reps_avx2_bmi2(
                        table,
                        abs_pos,
                        lit_len,
                        reps,
                        current_abs_end,
                        min_match_len,
                        f,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.for_each_repcode_candidate_with_reps_sse42(
                        table,
                        abs_pos,
                        lit_len,
                        reps,
                        current_abs_end,
                        min_match_len,
                        f,
                    )
                },
                FastpathKernel::Scalar => self.for_each_repcode_candidate_with_reps_scalar(
                    table,
                    abs_pos,
                    lit_len,
                    reps,
                    current_abs_end,
                    min_match_len,
                    f,
                ),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.for_each_repcode_candidate_with_reps_scalar(
                table,
                abs_pos,
                lit_len,
                reps,
                current_abs_end,
                min_match_len,
                f,
            )
        }
    }

    /// NEON umbrella variant of the rep-code probe.
    ///
    /// # Safety
    /// Caller must be running on an AArch64 target with NEON
    /// available (baseline on AArch64).
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn for_each_repcode_candidate_with_reps_neon(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
        reps: [u32; 3],
        current_abs_end: usize,
        min_match_len: usize,
        mut f: impl FnMut(MatchCandidate),
    ) {
        let _ = self;
        super::match_generator::for_each_repcode_candidate_body!(
            table,
            abs_pos,
            lit_len,
            reps,
            current_abs_end,
            min_match_len,
            f,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
        )
    }

    /// SSE4.2 umbrella variant.
    ///
    /// # Safety
    /// Caller must be running on x86/x86_64 with SSE4.2 available.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn for_each_repcode_candidate_with_reps_sse42(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
        reps: [u32; 3],
        current_abs_end: usize,
        min_match_len: usize,
        mut f: impl FnMut(MatchCandidate),
    ) {
        let _ = self;
        super::match_generator::for_each_repcode_candidate_body!(
            table,
            abs_pos,
            lit_len,
            reps,
            current_abs_end,
            min_match_len,
            f,
            crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        )
    }

    /// AVX2+BMI2 umbrella variant.
    ///
    /// # Safety
    /// Caller must be running on x86/x86_64 with AVX2 + BMI2 available.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn for_each_repcode_candidate_with_reps_avx2_bmi2(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
        reps: [u32; 3],
        current_abs_end: usize,
        min_match_len: usize,
        mut f: impl FnMut(MatchCandidate),
    ) {
        let _ = self;
        super::match_generator::for_each_repcode_candidate_body!(
            table,
            abs_pos,
            lit_len,
            reps,
            current_abs_end,
            min_match_len,
            f,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        )
    }

    /// Scalar fallback used on non-AArch64 targets.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_each_repcode_candidate_with_reps_scalar(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
        reps: [u32; 3],
        current_abs_end: usize,
        min_match_len: usize,
        mut f: impl FnMut(MatchCandidate),
    ) {
        let _ = self;
        super::match_generator::for_each_repcode_candidate_body!(
            table,
            abs_pos,
            lit_len,
            reps,
            current_abs_end,
            min_match_len,
            f,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
        )
    }

    /// Walk a candidate match backwards over the literal run so the
    /// matcher can absorb literal bytes that happen to match the byte
    /// preceding the candidate. Upstream zstd parity: equivalent to the back
    /// extend inside `ZSTD_HcFindBestMatch` before committing a
    /// sequence.
    ///
    /// Takes `&MatchTable` because the only thing it needs is read
    /// access to the contiguous history mirror — kept off `&self` so
    /// callers don't have to hand it an HcMatcher reference they
    /// don't otherwise use.
    pub(crate) fn extend_backwards(
        table: &MatchTable,
        mut candidate_pos: usize,
        mut abs_pos: usize,
        mut match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        let concat = table.live_history();
        let min_abs_pos = abs_pos - lit_len;
        while abs_pos > min_abs_pos
            && candidate_pos > table.history_abs_start
            && concat[candidate_pos - table.history_abs_start - 1]
                == concat[abs_pos - table.history_abs_start - 1]
        {
            candidate_pos -= 1;
            abs_pos -= 1;
            match_len += 1;
        }
        MatchCandidate {
            start: abs_pos,
            offset: abs_pos - candidate_pos,
            match_len,
        }
    }

    /// Upstream zstd parity: per-pass clamp of the "good enough — stop probing"
    /// threshold that the optimal parser passes to the BT/HC walkers.
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

#[cfg(test)]
mod hc_tests {
    //! Unit coverage for `HcMatcher` paths the encode-level suite
    //! doesn't naturally hit: short-suffix early returns on probe
    //! helpers, chain-walk self-loop branch, and the lazy-pick
    //! "next match is better" decline paths.
    use super::*;
    use crate::encoding::match_table::storage::MatchTable;

    fn table_with_history(buf: &[u8]) -> MatchTable {
        let mut t = MatchTable::new(buf.len().max(8));
        t.history = buf.to_vec();
        t.history_start = 0;
        t.history_abs_start = 0;
        t.window_size = buf.len();
        t.position_base = 0;
        t.hash_log = 8;
        t.chain_log = 8;
        t.hash3_log = 0;
        t.ensure_tables();
        // `history` is set directly above; record one live chunk.
        t.chunk_lens.push_back(buf.len());
        t
    }

    #[test]
    fn chain_candidates_returns_sentinels_when_suffix_too_short() {
        let hc = HcMatcher::new(2, 4, 32);
        // History exactly at min-prefix - 1 → idx + 4 > concat.len() →
        // early return with all-sentinel buffer.
        let t = table_with_history(b"abc");
        let buf = hc.chain_candidates(&t, 0);
        assert!(buf.iter().all(|&v| v == usize::MAX));
    }

    #[test]
    fn chain_candidates_terminates_on_self_loop_with_in_range_pick() {
        // Construct a self-loop in the chain: hash_table → cur,
        // chain_table[cur_rel] = cur (points back to itself). The walker
        // must pick the position (in-range) and stop.
        let mut hc = HcMatcher::new(2, 4, 32);
        hc.search_depth = 4;
        let mut t = table_with_history(b"abcdef_abcdef_abcdef");
        let abs_pos = 10usize;
        // The walker hashes the suffix at `abs_pos`, not the prefix at 0.
        let concat = t.live_history();
        let hash = t.hash_position(&concat[abs_pos..]);
        // Stored = relative + 1 → stored=6 means candidate_rel=5.
        t.hash_table[hash] = 6;
        let chain_mask = (1 << t.chain_log) - 1;
        t.chain_table[5 & chain_mask] = 6; // self-loop

        let buf = hc.chain_candidates(&t, abs_pos);
        assert_eq!(
            buf[0], 5,
            "self-loop pick must surface the in-range candidate"
        );
        assert_eq!(buf[1], usize::MAX, "walker must stop after self-loop");
    }

    #[test]
    fn repcode_candidate_returns_none_when_suffix_too_short() {
        let mut t = table_with_history(b"abc");
        t.offset_hist = [1, 2, 3];
        // current_idx + HC_MIN_MATCH_LEN > concat.len() → early None.
        assert!(HcMatcher::repcode_candidate(&t, 0, 1).is_none());
    }

    #[test]
    fn repcode_candidate_skips_rep_at_history_boundary() {
        // rep=5 but abs_pos=4, so candidate_pos would underflow into
        // pre-history bytes; the `rep > abs_pos` guard must skip it.
        let mut t = table_with_history(b"abcdefgh");
        t.offset_hist = [5, 6, 7];
        // No match possible at abs_pos=4 because every rep aims past
        // history start.
        let result = HcMatcher::repcode_candidate(&t, 4, 1);
        assert!(result.is_none(), "no rep can land in-range");
    }

    #[test]
    fn find_best_match_returns_none_for_short_suffix() {
        let hc = HcMatcher::new(2, 4, 32);
        let t = table_with_history(b"abc");
        assert!(hc.find_best_match::<false>(&t, 0, 1).is_none());
    }

    /// Forward-length selection (upstream `ZSTD_HcFindBestMatch`): the chain
    /// walk keeps the longest FORWARD match (`currentMl > ml`) and applies the
    /// single backward extension to THAT winner — a shorter-forward candidate
    /// is NOT promoted just because it has more backward (`lit_len`) room.
    /// Backward "catch up" happens once, on the forward winner, after the walk;
    /// it never changes which candidate wins.
    ///
    /// Fixture (40 bytes): `"AAAabcdefZMQabcdefIJBAAAabcdefIJKKKKKKKK"`.
    /// Probing `abs_pos = 24`: the 4-byte hash at `idx 24` ("abcd") collides
    /// with `idx 3` and `idx 12`, so the walk visits `[12, 3]` (LIFO).
    ///   - `idx 12`: forward 8 (`"abcdefIJ"`), `concat[11] = 'Q'` !=
    ///     `concat[23] = 'A'` so no backward room. Total 8, offset 12.
    ///   - `idx 3`: forward only 6 (`"abcdef"`), but `concat[0..3] = "AAA"` ==
    ///     `concat[21..24]` so it could backward-extend 3 to a TOTAL of 9 — yet
    ///     its forward length (6) loses to candidate 12's (8), so it is never
    ///     selected. The forward winner (12) wins at both `lit_len`s.
    #[test]
    fn hash_chain_candidate_picks_longest_forward_over_shorter_with_backward_room() {
        let mut t = MatchTable::new(64);
        t.history = b"AAAabcdefZMQabcdefIJBAAAabcdefIJKKKKKKKK".to_vec();
        t.history_start = 0;
        t.history_abs_start = 0;
        t.window_size = t.history.len();
        t.position_base = 0;
        t.hash_log = 8;
        t.chain_log = 8;
        t.hash3_log = 0;
        t.ensure_tables();
        t.chunk_lens.push_back(t.history.len());
        t.insert_positions(0, 24);

        let hc = HcMatcher::new(2, 16, 64);

        // The forward winner (idx 12, forward 8) has no backward room.
        let c0 = hc
            .hash_chain_candidate::<false>(&t, 24, 0)
            .expect("forward match must be found");
        assert_eq!(c0.match_len, 8, "longest forward match is 8 (idx 12)");
        assert_eq!(
            c0.offset, 12,
            "winner is the forward-8 candidate at offset 12"
        );

        // With lit_len=3 the SHORTER-forward candidate (idx 3) could reach a
        // total of 9 via backward extension, but forward-length selection keeps
        // candidate 12 (forward 8) — backward room never promotes a shorter
        // forward match. Result stays 8, not 9.
        let c3 = hc
            .hash_chain_candidate::<false>(&t, 24, 3)
            .expect("forward match must be found");
        assert_eq!(
            c3.match_len, 8,
            "forward-length selection must keep the forward-8 winner (idx 12); \
             a value of 9 would mean a shorter-forward candidate was promoted \
             by its backward room (non-upstream gain-based selection)"
        );
        assert_eq!(c3.offset, 12, "winner unchanged by lit_len");
    }

    /// Forward-length ties keep the FIRST-visited candidate (upstream
    /// `ZSTD_HcFindBestMatch` uses `currentMl > ml`, strictly-longer, so an
    /// equal-length later candidate never displaces the earlier one). The walk
    /// is newest-first, so in organic chains "first visited" is the closest
    /// (smallest-offset) position anyway; this test hand-wires the chain into a
    /// non-monotonic order to pin the tie-break rule itself.
    ///
    /// Fixture: four 8-byte `"abcdefgh"` chunks at `0 / 9 / 18 / 27`, each
    /// followed by a unique terminator (`'A'/'B'/'C'/'D'`) capping cross-chunk
    /// forward matches at exactly 8. Probing `abs_pos = 27` with the chain
    /// hand-wired to visit pos 9 (offset 18) THEN pos 18 (offset 9): both have
    /// forward length 8, so the first-visited (pos 9, offset 18) wins. The
    /// self-tightening gate at `ml = 8` also rejects pos 18 (its tail byte is a
    /// different chunk terminator), so the equal-length later candidate is
    /// skipped before the count — consistent with ties-keep-first.
    #[test]
    fn hash_chain_candidate_forward_ties_keep_first_visited() {
        let mut t = MatchTable::new(64);
        t.history = b"abcdefghAabcdefghBabcdefghCabcdefghDZZZZ".to_vec();
        assert_eq!(t.history.len(), 40);
        t.history_start = 0;
        t.history_abs_start = 0;
        t.window_size = t.history.len();
        t.position_base = 0;
        t.hash_log = 8;
        t.chain_log = 8;
        t.hash3_log = 0;
        t.ensure_tables();
        t.chunk_lens.push_back(t.history.len());

        let abs_pos = 27usize;
        let concat = t.live_history();
        let probe_hash = t.hash_position(&concat[abs_pos..]);
        // Hand-wire the chain head + link so the walk surfaces pos 9 first
        // (offset 18) then pos 18 (offset 9). `stored = pos + 1`.
        t.hash_table[probe_hash] = 9 + 1;
        let chain_mask = (1usize << t.chain_log) - 1;
        t.chain_table[9 & chain_mask] = 18 + 1;
        t.chain_table[18 & chain_mask] = HC_EMPTY;

        let hc = HcMatcher::new(2, 16, 64);
        let cand = hc
            .hash_chain_candidate::<false>(&t, abs_pos, 0)
            .expect("walk must still produce a match");
        assert_eq!(
            cand.match_len, 8,
            "both candidates have an 8-byte forward prefix"
        );
        assert_eq!(
            cand.offset, 18,
            "forward-length ties keep the first-visited candidate (pos 9, \
             offset 18); a value of 9 would mean the equal-length later \
             candidate displaced it (non-upstream gain-based tie-break)"
        );
    }
}
