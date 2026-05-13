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
//! Donor parity reference: `lib/compress/zstd_lazy.c`,
//! `ZSTD_HcFindBestMatch` / `ZSTD_compressBlock_lazy2_generic`.

#![allow(dead_code)]

use super::match_table::helpers::common_prefix_len;
use super::match_table::storage::{HC_EMPTY, MatchTable};
use super::opt::types::MatchCandidate;

/// Minimum match length emitted by the lazy / lazy2 chain walker.
/// Donor parity: `MIN_MATCH` in `lib/compress/zstd_lazy.c`.
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
pub(crate) struct HcMatcher {
    /// Lookahead depth (1 = lazy, 2 = lazy2). Donor parity:
    /// `params->cParams.strategy >= ZSTD_lazy2`.
    pub(crate) lazy_depth: u8,
    /// Maximum number of chain entries inspected per `find_best_match`
    /// call. Donor parity: `params->cParams.searchLog` (clamped to
    /// [`MAX_HC_SEARCH_DEPTH`](super::match_generator::MAX_HC_SEARCH_DEPTH)
    /// for HC mode; BT modes use the unclamped value as their walk
    /// budget).
    pub(crate) search_depth: usize,
    /// "Sufficient" match length — once a candidate reaches this
    /// length, the lazy decision short-circuits without checking the
    /// next position. Donor parity:
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

    /// Donor "match gain" heuristic: `match_len * 4 - offset_bits`.
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
        let max_chain_steps = self.search_depth;
        while filled < self.search_depth && steps < max_chain_steps {
            if cur == HC_EMPTY {
                break;
            }
            let candidate_rel = cur.wrapping_sub(1) as usize;
            let candidate_abs = table.position_base + candidate_rel;
            let next = table.chain_table[candidate_rel & chain_mask];
            steps += 1;
            if next == cur {
                // Self-loop: two positions share chain_idx, stop to
                // avoid spinning on the same candidate forever.
                if candidate_abs >= table.history_abs_start && candidate_abs < abs_pos {
                    buf[filled] = candidate_abs;
                }
                break;
            }
            cur = next;
            if candidate_abs < table.history_abs_start || candidate_abs >= abs_pos {
                continue;
            }
            buf[filled] = candidate_abs;
            filled += 1;
        }
        buf
    }

    /// Probe the 3 rep-code offsets (with the donor `ll0 ↦ rep[0] − 1`
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

        let mut best = None;
        for rep in reps.into_iter().flatten() {
            if rep == 0 || rep > abs_pos {
                continue;
            }
            let candidate_pos = abs_pos - rep;
            if candidate_pos < table.history_abs_start {
                continue;
            }
            let candidate_idx = candidate_pos - table.history_abs_start;
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
    pub(crate) fn hash_chain_candidate(
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

        let mut best: Option<MatchCandidate> = None;
        for candidate_abs in self.chain_candidates(table, abs_pos) {
            if candidate_abs == usize::MAX {
                break;
            }
            let candidate_idx = candidate_abs - table.history_abs_start;
            let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= HC_MIN_MATCH_LEN {
                let candidate =
                    Self::extend_backwards(table, candidate_abs, abs_pos, match_len, lit_len);
                best = Self::better_candidate(best, Some(candidate));
                if best.is_some_and(|b| b.match_len >= self.target_len) {
                    return best;
                }
            }
        }
        best
    }

    /// Combine the rep-code and chain-walk candidates and pick the
    /// better of the two.
    pub(crate) fn find_best_match(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        let rep = Self::repcode_candidate(table, abs_pos, lit_len);
        let hash = self.hash_chain_candidate(table, abs_pos, lit_len);
        Self::better_candidate(rep, hash)
    }

    /// Donor `lazy` / `lazy2` lookahead: evaluate the match a byte
    /// (and optionally two) ahead before committing the current one.
    /// Returns `Some(best)` if the current match wins, `None` if the
    /// caller should defer.
    ///
    /// Lazy lookahead queries `pos + 1` / `pos + 2` before they are
    /// inserted into the hash tables — matching the C zstd ordering.
    /// Seeding before comparing would let a position match against
    /// itself, changing semantics.
    pub(crate) fn pick_lazy_match(
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

        let next = self.find_best_match(table, abs_pos + 1, lit_len + 1);
        if let Some(next) = next {
            let next_gain = Self::match_gain(next.match_len, next.offset);
            if next_gain > current_gain {
                return None;
            }
        }

        if self.lazy_depth >= 2 && abs_pos + 2 + HC_MIN_MATCH_LEN <= table.history_abs_end() {
            let next2 = self.find_best_match(table, abs_pos + 2, lit_len + 2);
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
        crate::for_each_repcode_candidate_body!(
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
        crate::for_each_repcode_candidate_body!(
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
        crate::for_each_repcode_candidate_body!(
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
        crate::for_each_repcode_candidate_body!(
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
    /// preceding the candidate. Donor parity: equivalent to the back
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
}
