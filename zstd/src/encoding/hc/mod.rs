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

    /// Best hash-chain match at `abs_pos`. Walks the chain via
    /// [`Self::chain_candidates`], extends each survivor backwards
    /// over the literal run, and short-circuits as soon as a
    /// candidate crosses `target_len`.
    pub(crate) fn hash_chain_candidate<const DICT: bool>(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        let concat = table.live_history();
        let current_idx = abs_pos - table.history_abs_start;
        if current_idx + HC_MIN_MATCH_LEN > concat.len() {
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

        let mut best: Option<MatchCandidate> = None;
        // Upstream zstd speculative tail check (`zstd_lazy.c:714`,
        // `ZSTD_HcFindBestMatch`): once `best` is set, gate the
        // expensive `common_prefix_len` walk on a 4-byte tail compare
        // proving the new candidate can possibly reach the *forward*
        // length required to outscore `best` under
        // [`Self::better_candidate`] (gain = `len*4 - offset_bits`).
        //
        // Correctness — backward-extension–aware bound:
        //   `best.match_len` is the *total* length stored by
        //   [`Self::extend_backwards`]: forward bytes from
        //   `current_idx` plus up to `lit_len` backward bytes
        //   (`B_best = abs_pos − best.start`, capped by `lit_len`).
        //   A new candidate can in principle replace `B_best` of its
        //   own length with up to `lit_len` backward bytes, so the
        //   worst-case forward length it needs to outscore `best`
        //   is `best.match_len − lit_len + 1`. The 4-byte tail probe
        //   at offset `best.match_len − lit_len − 3` covers exactly
        //   that boundary (the read includes the byte at the
        //   required-forward-length position itself). A mismatch
        //   there is a proof the candidate cannot win regardless of
        //   how much it later extends backwards.
        //   When `best.match_len ≤ lit_len + 3` the worst-case
        //   forward target is so close to `current_idx` that the
        //   `common_prefix_len` cost is already trivial — the gate
        //   is skipped via the `checked_sub`.
        //
        // Walk-order argument (offset monotonicity — REQUIRED gate
        // precondition, enforced per-iteration):
        //   Chain walks are LIFO in their dense form (newest first →
        //   strictly increasing offset). But the chain table is
        //   `chain_log`-bits wide; when a position is re-inserted at
        //   the same masked chain index after the cycle wraps, an
        //   older chain link can point into a slot that has since
        //   been OVERWRITTEN with a newer (closer) position. The
        //   walker then surfaces a candidate with a SMALLER offset
        //   than ones it has already returned, breaking monotonicity.
        //
        //   The gate's bound (`tail_off = best.match_len − lit_len −
        //   3` covers exactly the "new forward must reach
        //   best.match_len − lit_len + 1" requirement) is only sound
        //   when `new.offset_bits ≥ best.offset_bits`. Otherwise a
        //   smaller-offset candidate can outscore `best` by gain at
        //   *equal* total length — and the gate would skip it because
        //   it only proves `match_len > best.match_len`. The
        //   `new_offset >= best.offset` per-iteration check below
        //   enforces the monotonicity precondition; on non-monotonic
        //   walks we fall through to the full `common_prefix_len` so
        //   the offset-bits advantage is given a chance to win.
        let history_tail = concat.len();
        // Raw base pointer for the upstream zstd-style `MEM_read32` 4-byte gates below
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
            // Self-loop: two positions share `candidate_rel & chain_mask`;
            // stop after processing this slot.
            let self_loop = next == cur;

            // Only process candidates in the live window, in relative space:
            // `floor_idx <= candidate_idx < current_idx`.
            let candidate_idx = (cur as usize).wrapping_add(idx_bias);
            if candidate_idx >= floor_idx && candidate_idx < current_idx {
                // `candidate_idx < current_idx` (checked above), so the
                // subtraction never underflows. `new_offset == abs_pos -
                // candidate_abs`.
                let new_offset = current_idx - candidate_idx;
                // Speculative tail gate — full rationale (backward-extension
                // bound + walk-order/offset-monotonicity precondition) is in
                // the comment block right above this loop.
                let mut skip = false;
                if let Some(best_ref) = best
                    && new_offset >= best_ref.offset
                    && let Some(tail_off) = best_ref.match_len.checked_sub(lit_len + 3)
                {
                    let m_end = candidate_idx + tail_off + 4;
                    let i_end = current_idx + tail_off + 4;
                    // Bounds-fail is a SAFE skip — see longer rationale in the
                    // gate's git history. Briefly: under the monotonicity
                    // precondition above, bounds-fail proves no in-range
                    // candidate at this `current_idx` can outscore `best`.
                    // Raw unaligned `u32` load+compare (upstream zstd `MEM_read32`)
                    // instead of a bounds-checked slice equality — same 4-byte
                    // tail test, one load each, no `[a..b]` bounds panic path.
                    if i_end > history_tail || m_end > history_tail {
                        skip = true;
                    } else {
                        // SAFETY: both `+ tail_off` reads are `<= history_tail`
                        // (checked above), so 4 bytes are in range.
                        let m = unsafe {
                            MatchTable::read_le_u32_ptr(base_ptr.add(candidate_idx + tail_off))
                        };
                        let i = unsafe {
                            MatchTable::read_le_u32_ptr(base_ptr.add(current_idx + tail_off))
                        };
                        skip = m != i;
                    }
                }

                // Cheap 4-byte equality gate before the wider SIMD count
                // (upstream zstd `MEM_read32` gate in `ZSTD_HcFindBestMatch`).
                // `HC_MIN_MATCH_LEN == 4`, so a first-4-byte mismatch can never
                // reach the match floor — reject without the vector
                // load+count+tzcnt. On dict-primed chains over low-match
                // (random) input, where `best` stays `None` so the speculative
                // tail gate above never fires, this rejects the bulk of chain
                // candidates on a scalar compare instead of a full
                // `common_prefix_len`. Raw unaligned `u32` load+compare (upstream zstd
                // `MEM_read32`), not a bounds-checked slice equality. Falls
                // through to the full count when either side lacks a 4-byte
                // lookahead (the count handles short tails), so the accepted
                // set is byte-identical.
                let four_byte_ok = candidate_idx + 4 > history_tail
                    || current_idx + 4 > history_tail
                    || unsafe {
                        // SAFETY: both `+ 4` reads are `<= history_tail` (checked
                        // above), so 4 bytes are in range.
                        MatchTable::read_le_u32_ptr(base_ptr.add(candidate_idx))
                            == MatchTable::read_le_u32_ptr(base_ptr.add(current_idx))
                    };
                if !skip && four_byte_ok {
                    let match_len =
                        common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
                    if match_len >= HC_MIN_MATCH_LEN {
                        // Rare accept path: recover the absolute position only
                        // here (the hot per-candidate body stays in relative
                        // space). `candidate_abs == candidate_idx +
                        // history_abs_start`.
                        let candidate_abs = candidate_idx + history_abs_start;
                        let candidate = Self::extend_backwards(
                            table,
                            candidate_abs,
                            abs_pos,
                            match_len,
                            lit_len,
                        );
                        best = Self::better_candidate(best, Some(candidate));
                        if best.is_some_and(|b| b.match_len >= self.target_len) {
                            return best;
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
        // walk is OUT-OF-LINE (`dms_chain_walk`, `#[inline(never)]`) so the
        // dict-only code never bloats this hot function on no-dict frames, where
        // the `is_some` guard is a single not-taken branch. `dms_chain_walk`
        // continues the SAME `steps` budget, so live + dms stay one bounded
        // operation (upstream zstd's single `nbAttempts`).
        if DICT && table.dms.is_primed() {
            best = self.dms_chain_walk(
                table,
                concat,
                abs_pos,
                lit_len,
                current_idx,
                history_abs_start,
                history_tail,
                max_chain_steps,
                steps,
                best,
            );
        }
        best
    }

    /// Out-of-line dms HC4 walk (upstream `ZSTD_HcFindBestMatch` dms loop,
    /// `zstd_lazy.c:748-770`). Split from [`Self::hash_chain_candidate`] so the
    /// no-dict hot path keeps its small body / register budget. Continues the
    /// caller's `steps` against the shared `max_chain_steps` so the live + dms
    /// search is one bounded operation. The dict sits at the front of `concat`
    /// (`[0, region)`); a dms candidate is a concat index `< current_idx`, so
    /// the offset / extension logic matches the live walk.
    ///
    /// `#[inline]`: with the `DICT` split the `DICT = false` monomorph never
    /// references this (the `if DICT &&` gate is a compile-time false), so it is
    /// dropped from the no-dict path entirely; the `DICT = true` monomorph
    /// inlines it into the dict hot path.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn dms_chain_walk(
        &self,
        table: &MatchTable,
        concat: &[u8],
        abs_pos: usize,
        lit_len: usize,
        current_idx: usize,
        history_abs_start: usize,
        history_tail: usize,
        max_chain_steps: usize,
        mut steps: usize,
        mut best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        let dms = match table.dms.table() {
            Some(d) => d,
            None => return best,
        };
        // Match upstream zstd's effective dict search depth: ZSTD's CDict path
        // adjusts cParams for the dedicated dict search so it runs deeper than
        // the bare level searchLog (measured: upstream needs nbAttempts >= 16 to
        // surface the long dict match on the per-label-dict fixtures, vs the 8
        // from L6's searchLog=3). Floor the dict-walk budget at 16.
        let dms_budget = max_chain_steps.max(16);
        // Dedicated dms step budget, independent of the live-chain walk's
        // consumption. Upstream's per-frame index reset keeps the live chain
        // short so the shared budget still reached the dms; a no-memset
        // floor-advance lets the live chain accumulate across frames, and the
        // shared counter would then be exhausted on stale live entries before
        // the dms loop runs (0 dict attempts -> dict matches silently dropped,
        // the gradual cross-frame decay). Resetting the counter gives the dict
        // search its full >= 16 attempts in both the memset and floor-advance
        // resets; the live chain is short enough in the memset case that the
        // effective dms attempt count is unchanged.
        steps = 0;
        let base_ptr = concat.as_ptr();
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
                let mut skip = false;
                if let Some(best_ref) = best
                    && new_offset >= best_ref.offset
                    && let Some(tail_off) = best_ref.match_len.checked_sub(lit_len + 3)
                {
                    let m_end = dict_idx + tail_off + 4;
                    let i_end = current_idx + tail_off + 4;
                    if i_end > history_tail || m_end > history_tail {
                        skip = true;
                    } else {
                        let m = unsafe {
                            MatchTable::read_le_u32_ptr(base_ptr.add(dict_idx + tail_off))
                        };
                        let i = unsafe {
                            MatchTable::read_le_u32_ptr(base_ptr.add(current_idx + tail_off))
                        };
                        skip = m != i;
                    }
                }
                let four_byte_ok = dict_idx + 4 > history_tail
                    || current_idx + 4 > history_tail
                    || unsafe {
                        MatchTable::read_le_u32_ptr(base_ptr.add(dict_idx))
                            == MatchTable::read_le_u32_ptr(base_ptr.add(current_idx))
                    };
                if !skip && four_byte_ok {
                    let match_len = common_prefix_len(&concat[dict_idx..], &concat[current_idx..]);
                    if match_len >= HC_MIN_MATCH_LEN {
                        let candidate = Self::extend_backwards(
                            table,
                            dict_idx + history_abs_start,
                            abs_pos,
                            match_len,
                            lit_len,
                        );
                        best = Self::better_candidate(best, Some(candidate));
                        // Match upstream zstd `ZSTD_HcFindBestMatch`: the find
                        // breaks only when the match reaches the input end
                        // (`ip+ml == iLimit`), NOT on a `target_len` threshold —
                        // `target_len` gates the lazy lookahead, not the find.
                        if best.is_some_and(|b| current_idx + b.match_len >= history_tail) {
                            return best;
                        }
                    }
                }
            }
            // Chain links are strictly decreasing (head insert); `dnext == 0`
            // ends the walk at the loop top.
            if dnext == dcur {
                break;
            }
            dcur = dnext;
        }
        best
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

    /// Regression test for the speculative tail check's
    /// backward-extension bound. The pre-fix gate used `tail_off =
    /// best.match_len − 3` and was unaware that `extend_backwards`
    /// could have added up to `lit_len` backward bytes to
    /// `best.match_len`. The post-fix formula subtracts `lit_len`
    /// via `checked_sub(lit_len + 3)`.
    ///
    /// To actually fail for the pre-fix gate (Copilot review on
    /// `c16ca32b` flagged that an earlier round of this test did
    /// not), the fixture is constructed so the first LIFO candidate
    /// cannot extend backward but a later candidate can — only the
    /// later candidate's *total* match length (`forward +
    /// backward_extension`) reaches the new best.
    ///
    /// Fixture (40 bytes, indices `0..=39`):
    ///   `"AAAabcdefZMQabcdefIJBAAAabcdefIJKKKKKKKK"`
    ///    0123456789012345678901234567890123456789   (ones digit)
    ///              1111111111222222222233333333     (tens digit, aligned)
    ///
    /// Probing `abs_pos = 24, lit_len = 3`:
    ///   - The 4-byte hash at `idx 24` ("abcd") collides with the
    ///     hashes at `idx 3` and `idx 12` (also "abcd"). All other
    ///     positions in `0..24` hash to other buckets, so the chain
    ///     walker visits exactly `[12, 3]` in LIFO order.
    ///   - Candidate at `idx 12`: forward 8 bytes (`"abcdefIJ"`
    ///     matches the probe `"abcdefIJ"` at `24..32` exactly), but
    ///     the byte right before — `concat[11] = 'Q'` — does NOT
    ///     equal `concat[23] = 'A'`, so `extend_backwards` cannot
    ///     extend even one byte despite `lit_len = 3` of available
    ///     headroom. Total `match_len = 8`, offset = 12.
    ///   - Candidate at `idx 3`: forward only 6 bytes (`"abcdef"`
    ///     matches, byte 7 at `idx 9 = 'Z'` differs from probe byte
    ///     7 at `idx 30 = 'I'`). But the 3 bytes before it —
    ///     `concat[0..3] = "AAA"` — exactly match `concat[21..24] =
    ///     "AAA"`, so `extend_backwards` adds 3 backward bytes.
    ///     Total `match_len = 6 + 3 = 9`, offset = 21.
    ///
    /// Gate behaviour at `lit_len = 3`:
    ///   - Pre-fix: `tail_off = best.match_len - 3 = 5`. For
    ///     candidate at `idx 3` the gate reads `concat[3+5..3+5+4]
    ///     = concat[8..12] = "fZMQ"` and compares to probe
    ///     `concat[29..33] = "fIJK"`. Mismatch → gate SKIPS. The
    ///     helper never runs `common_prefix_len` on candidate `3`,
    ///     never extends backwards, and returns `match_len = 8`
    ///     (the first candidate's match) — losing the 9-byte
    ///     backward-extended win.
    ///   - Post-fix: `tail_off = best.match_len - lit_len - 3 = 2`.
    ///     The gate reads `concat[3+2..3+2+4] = concat[5..9] =
    ///     "cdef"` and compares to probe `concat[26..30] = "cdef"`.
    ///     Match → gate PASSES → full count runs, finds forward 6,
    ///     extends backwards 3, returns `match_len = 9`.
    #[test]
    fn hash_chain_candidate_speculative_gate_handles_lit_len_backward_extension() {
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

        // `lit_len = 0`: no backward extension headroom — neither
        // candidate can grow past its forward match length, so the
        // best wins at forward-only length 8. Both pre-fix and
        // post-fix gates produce the same answer here.
        let result_lit0 = hc.hash_chain_candidate::<false>(&t, 24, 0);
        let len0 = result_lit0.map(|c| c.match_len).unwrap_or(0);
        assert_eq!(
            len0, 8,
            "lit_len=0 must return the forward-only 8-byte match at offset 12, \
             got {len0}"
        );

        // `lit_len = 3`: the second candidate (`idx 3`) can extend 3
        // bytes backwards, giving a total length of 9. Pre-fix gate
        // would skip it; post-fix gate lets it through.
        let result_lit3 = hc.hash_chain_candidate::<false>(&t, 24, 3);
        let len3 = result_lit3.map(|c| c.match_len).unwrap_or(0);
        assert_eq!(
            len3, 9,
            "lit_len=3 must return the backward-extended 9-byte match \
             (forward 6 + backward 3); a value of 8 means the gate over-rejected \
             the second LIFO candidate and the helper missed the backward-extension \
             win (pre-fix regression). Got {len3}"
        );

        // Strict-increase between `lit_len=0` and `lit_len=3` is the
        // signal the pre-fix gate would NOT have produced. Keep this
        // assertion explicitly so the test's failure message points
        // at exactly the regression it guards.
        assert!(
            len3 > len0,
            "speculative gate must allow `lit_len`-dependent strict gains: \
             lit_len=0 → {len0}, lit_len=3 → {len3}. Equal values means the \
             gate skipped the backward-extending candidate."
        );
    }

    /// Regression test for the non-monotonic-walk fallback. When the
    /// cyclic `chain_table & chain_mask` mask overwrites a slot, the
    /// chain walker can surface a candidate with a SMALLER offset than
    /// ones it has already returned. The speculative gate's
    /// monotonicity precondition (`new.offset_bits ≥ best.offset_bits`)
    /// is enforced per-iteration via the `new_offset ≥ best.offset`
    /// check: when monotonicity breaks the gate falls through to
    /// `common_prefix_len` so the offset-bits advantage is given a
    /// chance to win.
    ///
    /// Construction: organic LIFO insertion order would never produce
    /// this layout — when positions are inserted in monotonic order
    /// the chain links naturally point at strictly older positions
    /// (the previous `hash_table[hash]`). To force the bug-prone
    /// scenario this test reaches into `MatchTable` and hand-wires the
    /// chain so the walker visits `pos 9` first (offset 18) and then
    /// `pos 18` second (offset 9). The fixture sits four 8-byte
    /// `"abcdefgh"` chunks at positions `0 / 9 / 18 / 27` (each chunk
    /// followed by a unique terminator byte that caps cross-chunk
    /// forward matches at exactly 8); the probe at `abs_pos = 27`
    /// hashes the same prefix as the earlier chunks, so all chain
    /// candidates produce an 8-byte forward match and only the
    /// offset-bits difference can decide the winner.
    ///
    /// With the new `new_offset >= best.offset` precondition:
    ///   * Iter 1: cand_abs 9, offset 18. `best = None` → no gate,
    ///     full count, `best = (len 8, offset 18)`.
    ///   * Iter 2: cand_abs 18, offset 9. `new_offset = 9 <
    ///     best.offset = 18` → gate skipped → full count runs →
    ///     `better_candidate` picks the smaller-offset winner (equal
    ///     length, smaller offset_bits → strictly higher gain).
    ///
    /// Final `best.offset` must be `9` (the smaller-offset winner).
    /// Pre-fix code (gate applied unconditionally) would have
    /// inspected the tail at `tail_off = 8 − 0 − 3 = 5` and the
    /// 4-byte read at offsets `5..9` covers the chunk-terminator byte
    /// (different `'A' / 'B' / 'C' / 'D'` per chunk) — gate fails on
    /// the mismatching terminator and the second candidate gets
    /// skipped, leaving `best.offset = 18`.
    #[test]
    fn hash_chain_candidate_non_monotonic_walk_accepts_smaller_offset() {
        // Four 8-byte `"abcdefgh"` chunks, each followed by a unique
        // terminator byte (`'A' / 'B' / 'C' / 'D'`). The terminators
        // are part of the same 40-byte stream, so each chunk start
        // sits 9 bytes after the previous one — chunk starts are at
        // `0`, `9`, `18`, `27` (not `0/8/16/24` as a naive
        // chunk-width calculation would suggest). The cross-chunk
        // forward match between any two chunks caps at exactly 8
        // because the byte right after each chunk is unique.
        let mut t = MatchTable::new(64);
        t.history = b"abcdefghAabcdefghBabcdefghCabcdefghDZZZZ".to_vec();
        assert_eq!(t.history.len(), 40);
        // After each "abcdefgh" the next byte is unique ('A'/'B'/'C'
        // /'D'), capping cross-chunk forward matches at length 8.
        t.history_start = 0;
        t.history_abs_start = 0;
        t.window_size = t.history.len();
        t.position_base = 0;
        t.hash_log = 8;
        t.chain_log = 8;
        t.hash3_log = 0;
        t.ensure_tables();
        t.chunk_lens.push_back(t.history.len());

        // The probe is at abs_pos 27 (start of the fourth
        // "abcdefgh" chunk). Hand-wire the chain so the walker
        // visits pos 9 first and pos 18 second.
        //
        // Layout: chunks at 0..8, 9..17, 18..26, 27..35. The byte
        // BEFORE each chunk is the terminator from the previous
        // chunk's match. Probing at abs_pos 27:
        //   * candidate 9 → offset 27 − 9 = 18
        //   * candidate 18 → offset 27 − 18 = 9 (SMALLER — second
        //     visit, non-monotonic)
        let abs_pos = 27usize;
        let concat = t.live_history();
        let probe_hash = t.hash_position(&concat[abs_pos..]);
        // `stored = pos + 1` per `MatchTable::stored_abs_position_fast`.
        // Hand-wire the chain head and the link OUT of pos 9 so the
        // walk surfaces 9 first, then 18.
        t.hash_table[probe_hash] = 9 + 1;
        let chain_mask = (1usize << t.chain_log) - 1;
        t.chain_table[9 & chain_mask] = 18 + 1;
        // Terminate the walk after pos 18.
        t.chain_table[18 & chain_mask] = HC_EMPTY;

        let hc = HcMatcher::new(2, 16, 64);
        let result = hc.hash_chain_candidate::<false>(&t, abs_pos, 0);
        let cand = result.expect("non-monotonic walk must still produce a match");
        assert_eq!(
            cand.match_len, 8,
            "both chain candidates have an 8-byte forward prefix match — \
             expected match_len = 8, got {}",
            cand.match_len
        );
        assert_eq!(
            cand.offset, 9,
            "non-monotonic fallback must surface the smaller-offset winner: \
             expected offset = 9 (cand_abs 18), got offset = {}. \
             A regression in the `new_offset >= best.offset` check would \
             keep the gate active for the smaller-offset second candidate, \
             skip its full count, and leave best.offset at 18 (the \
             larger-offset first-visited candidate).",
            cand.offset
        );
    }
}
