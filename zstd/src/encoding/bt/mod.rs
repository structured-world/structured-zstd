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

use super::cost_model::{HC_MAX_LIT, HcOptState, HcOptimalCostProfile};
use super::match_table::storage::MatchTable;
use super::opt::ldm::{HcOptLdmState, HcRawSeq, HcRawSeqStore};
use super::opt::types::{HcOptimalNode, HcOptimalPlanBuffers, HcOptimalSequence, MatchCandidate};

/// Maximum offset reachable by the HC3 short-match probe. Donor
/// parity: keeps the 3-byte side table from emitting offsets that
/// the main BT/HC paths would address more efficiently. Used inside
/// the `hash3_candidate_body!` macro that the kernel-specific
/// variants below expand.
pub(crate) const HC3_MAX_OFFSET: usize = 1 << 18;

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

    /// Cross-platform dispatcher for the HC3 short-match probe.
    /// Routes to the kernel-specific variant so the per-position
    /// `common_prefix_len_ptr` call inlines under the callee's
    /// `target_feature` umbrella. Test / external callers only — the
    /// on-encode hot path bypasses this dispatcher via the
    /// kernel-specific variants invoked from inside
    /// `collect_optimal_candidates_initialized_<kernel>`.
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn hash3_candidate(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.hash3_candidate_neon(table, abs_pos, current_abs_end, min_match_len)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.hash3_candidate_avx2_bmi2(table, abs_pos, current_abs_end, min_match_len)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.hash3_candidate_sse42(table, abs_pos, current_abs_end, min_match_len)
                },
                FastpathKernel::Scalar => {
                    self.hash3_candidate_scalar(table, abs_pos, current_abs_end, min_match_len)
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.hash3_candidate_scalar(table, abs_pos, current_abs_end, min_match_len)
        }
    }

    /// NEON umbrella HC3 probe.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline). Body inlines via macro.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn hash3_candidate_neon(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        let _ = self;
        crate::hash3_candidate_body!(
            table,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
        )
    }

    /// SSE4.2 umbrella HC3 probe.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    pub(crate) unsafe fn hash3_candidate_sse42(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        let _ = self;
        crate::hash3_candidate_body!(
            table,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        )
    }

    /// AVX2+BMI2 umbrella HC3 probe.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 + BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    pub(crate) unsafe fn hash3_candidate_avx2_bmi2(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        let _ = self;
        crate::hash3_candidate_body!(
            table,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        )
    }

    /// Scalar fallback HC3 probe (used on non-AArch64 targets).
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    pub(crate) fn hash3_candidate_scalar(
        &self,
        table: &MatchTable,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        let _ = self;
        crate::hash3_candidate_body!(
            table,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
        )
    }

    /// Cross-platform dispatcher for the per-position BT walker. The
    /// per-iteration `count_match_from_indices` symbol inlines under
    /// the kernel-specific `target_feature` umbrella so the entire
    /// walk runs as one straight-line hot path.
    #[inline(always)]
    pub(crate) fn bt_insert_step_no_rebase(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        // SAFETY: each branch verifies the target_feature requirement
        // of the callee — aarch64 NEON is baseline; x86 AVX2/BMI2 and
        // SSE4.2 are selected only when the runtime detector reports
        // them present.
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_insert_step_no_rebase_neon(
                table,
                search_depth,
                abs_pos,
                current_abs_end,
                target_abs,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_insert_step_no_rebase_avx2_bmi2(
                        table,
                        search_depth,
                        abs_pos,
                        current_abs_end,
                        target_abs,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_insert_step_no_rebase_sse42(
                        table,
                        search_depth,
                        abs_pos,
                        current_abs_end,
                        target_abs,
                    )
                },
                FastpathKernel::Scalar => self.bt_insert_step_no_rebase_scalar(
                    table,
                    search_depth,
                    abs_pos,
                    current_abs_end,
                    target_abs,
                ),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.bt_insert_step_no_rebase_scalar(
                table,
                search_depth,
                abs_pos,
                current_abs_end,
                target_abs,
            )
        }
    }

    /// NEON umbrella variant of the BT walker. Body inlines via macro.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline).
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn bt_insert_step_no_rebase_neon(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let _ = self;
        crate::bt_insert_step_no_rebase_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::neon::count_match_from_indices
        )
    }

    /// SSE4.2 umbrella BT walker.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    pub(crate) unsafe fn bt_insert_step_no_rebase_sse42(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let _ = self;
        crate::bt_insert_step_no_rebase_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::sse42::count_match_from_indices
        )
    }

    /// AVX2+BMI2 umbrella BT walker.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 + BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    pub(crate) unsafe fn bt_insert_step_no_rebase_avx2_bmi2(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let _ = self;
        crate::bt_insert_step_no_rebase_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices
        )
    }

    /// Scalar fallback BT walker (used on non-AArch64 targets).
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    pub(crate) fn bt_insert_step_no_rebase_scalar(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let _ = self;
        crate::bt_insert_step_no_rebase_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::scalar::count_match_from_indices
        )
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

    /// Cross-platform entry. Picks the kernel-specific variant so the BT walk
    /// body executes inside one `target_feature` umbrella and inlines the
    /// vectorized `count_match_from_indices` directly. See
    /// `bt_insert_step_no_rebase` for the same dispatcher pattern.
    ///
    /// The on-encode hot path bypasses this dispatcher: when invoked from
    /// `collect_optimal_candidates_initialized_<kernel>` the per-kernel
    /// variant is called directly so the BT match collection inlines under
    /// the surrounding umbrella. This entry is kept for external / future
    /// callers that aren't yet under an umbrella.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub(crate) fn bt_insert_and_collect_matches(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        // SAFETY: each branch verifies the target_feature requirement of the
        // callee (see `bt_insert_step_no_rebase` dispatcher).
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_insert_and_collect_matches_neon(
                table,
                search_depth,
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                best_len_for_skip,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_insert_and_collect_matches_avx2_bmi2(
                        table,
                        search_depth,
                        abs_pos,
                        current_abs_end,
                        profile,
                        min_match_len,
                        best_len_for_skip,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_insert_and_collect_matches_sse42(
                        table,
                        search_depth,
                        abs_pos,
                        current_abs_end,
                        profile,
                        min_match_len,
                        best_len_for_skip,
                        out,
                    )
                },
                FastpathKernel::Scalar => self.bt_insert_and_collect_matches_scalar(
                    table,
                    search_depth,
                    abs_pos,
                    current_abs_end,
                    profile,
                    min_match_len,
                    best_len_for_skip,
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
            self.bt_insert_and_collect_matches_scalar(
                table,
                search_depth,
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                best_len_for_skip,
                out,
            )
        }
    }

    /// NEON-umbrella variant of `bt_insert_and_collect_matches`. Inlines
    /// `fastpath::neon::count_match_from_indices` via the shared body macro.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline).
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bt_insert_and_collect_matches_neon(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let _ = self;
        crate::bt_insert_and_collect_matches_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::neon::count_match_from_indices,
        )
    }

    /// SSE4.2 umbrella variant of `bt_insert_and_collect_matches`.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bt_insert_and_collect_matches_sse42(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let _ = self;
        crate::bt_insert_and_collect_matches_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::sse42::count_match_from_indices,
        )
    }

    /// AVX2+BMI2 umbrella variant of `bt_insert_and_collect_matches`.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 and BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bt_insert_and_collect_matches_avx2_bmi2(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let _ = self;
        crate::bt_insert_and_collect_matches_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices,
        )
    }

    /// Scalar fallback used on non-AArch64 targets.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bt_insert_and_collect_matches_scalar(
        &self,
        table: &mut MatchTable,
        search_depth: usize,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let _ = self;
        crate::bt_insert_and_collect_matches_body!(
            table,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::scalar::count_match_from_indices,
        )
    }

    /// Donor parity: `ZSTD_optLdm_skipRawSeqStoreBytes`. Fast-forward the
    /// raw LDM seq store cursor by `nb_bytes`, consuming whole stored
    /// sequences and leaving a partial-sequence offset in `pos_in_sequence`.
    pub(crate) fn ldm_skip_raw_seq_store_bytes(
        &self,
        seq_store: &mut HcRawSeqStore,
        nb_bytes: usize,
    ) {
        let mut curr_pos = seq_store.pos_in_sequence.saturating_add(nb_bytes);
        while curr_pos > 0 && seq_store.pos < seq_store.size {
            let curr_seq = self.ldm_sequences[seq_store.pos];
            let seq_len = curr_seq.lit_length.saturating_add(curr_seq.match_length);
            if curr_pos >= seq_len {
                curr_pos -= seq_len;
                seq_store.pos += 1;
            } else {
                seq_store.pos_in_sequence = curr_pos;
                break;
            }
        }
        if curr_pos == 0 || seq_store.pos == seq_store.size {
            seq_store.pos_in_sequence = 0;
        }
    }

    /// Donor parity: `ZSTD_optLdm_maybeAddMatch` / its preamble in
    /// `ZSTD_optLdm_getNextMatch`. Advance the per-block LDM window
    /// markers to the next raw LDM sequence and skip its literals.
    pub(crate) fn ldm_get_next_match_and_update_seq_store(
        &self,
        opt_ldm: &mut HcOptLdmState,
        curr_pos_in_block: usize,
        block_bytes_remaining: usize,
    ) {
        if opt_ldm.seq_store.size == 0 || opt_ldm.seq_store.pos >= opt_ldm.seq_store.size {
            opt_ldm.start_pos_in_block = usize::MAX;
            opt_ldm.end_pos_in_block = usize::MAX;
            return;
        }
        let curr_seq = self.ldm_sequences[opt_ldm.seq_store.pos];
        let curr_block_end_pos = curr_pos_in_block.saturating_add(block_bytes_remaining);
        let literals_bytes_remaining = curr_seq
            .lit_length
            .saturating_sub(opt_ldm.seq_store.pos_in_sequence);
        let match_bytes_remaining = if literals_bytes_remaining == 0 {
            curr_seq.match_length.saturating_sub(
                opt_ldm
                    .seq_store
                    .pos_in_sequence
                    .saturating_sub(curr_seq.lit_length),
            )
        } else {
            curr_seq.match_length
        };
        if literals_bytes_remaining >= block_bytes_remaining {
            opt_ldm.start_pos_in_block = usize::MAX;
            opt_ldm.end_pos_in_block = usize::MAX;
            self.ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, block_bytes_remaining);
            return;
        }
        opt_ldm.start_pos_in_block = curr_pos_in_block.saturating_add(literals_bytes_remaining);
        opt_ldm.end_pos_in_block = opt_ldm
            .start_pos_in_block
            .saturating_add(match_bytes_remaining);
        opt_ldm.offset = curr_seq.offset;
        if opt_ldm.end_pos_in_block > curr_block_end_pos {
            opt_ldm.end_pos_in_block = curr_block_end_pos;
            self.ldm_skip_raw_seq_store_bytes(
                &mut opt_ldm.seq_store,
                curr_block_end_pos.saturating_sub(curr_pos_in_block),
            );
        } else {
            self.ldm_skip_raw_seq_store_bytes(
                &mut opt_ldm.seq_store,
                literals_bytes_remaining.saturating_add(match_bytes_remaining),
            );
        }
    }

    /// Donor parity: `ZSTD_optLdm_maybeAddMatch`. Convert the active LDM
    /// window (open/close cursors set by
    /// [`ldm_get_next_match_and_update_seq_store`]) into a usable
    /// `MatchCandidate` when the current position falls inside it.
    pub(crate) fn ldm_maybe_add_match(
        &self,
        opt_ldm: &HcOptLdmState,
        curr_pos_in_block: usize,
        min_match: usize,
    ) -> Option<MatchCandidate> {
        let _ = self;
        let pos_diff = curr_pos_in_block.saturating_sub(opt_ldm.start_pos_in_block);
        let candidate_match_length = opt_ldm
            .end_pos_in_block
            .saturating_sub(opt_ldm.start_pos_in_block)
            .saturating_sub(pos_diff);
        if curr_pos_in_block < opt_ldm.start_pos_in_block
            || curr_pos_in_block >= opt_ldm.end_pos_in_block
            || candidate_match_length < min_match
        {
            return None;
        }
        Some(MatchCandidate {
            start: curr_pos_in_block,
            offset: opt_ldm.offset,
            match_len: candidate_match_length,
        })
    }

    /// Donor parity: `ZSTD_optLdm_processMatchCandidate`. Wraps
    /// [`ldm_maybe_add_match`] with a re-seed step when the parser has
    /// stepped past the current LDM window.
    pub(crate) fn ldm_process_match_candidate(
        &self,
        opt_ldm: &mut HcOptLdmState,
        curr_pos_in_block: usize,
        remaining_bytes: usize,
        min_match: usize,
    ) -> Option<MatchCandidate> {
        if opt_ldm.seq_store.size == 0 || opt_ldm.seq_store.pos >= opt_ldm.seq_store.size {
            return None;
        }
        if curr_pos_in_block >= opt_ldm.end_pos_in_block {
            if curr_pos_in_block > opt_ldm.end_pos_in_block {
                let pos_overshoot = curr_pos_in_block.saturating_sub(opt_ldm.end_pos_in_block);
                self.ldm_skip_raw_seq_store_bytes(&mut opt_ldm.seq_store, pos_overshoot);
            }
            self.ldm_get_next_match_and_update_seq_store(
                opt_ldm,
                curr_pos_in_block,
                remaining_bytes,
            );
        }
        self.ldm_maybe_add_match(opt_ldm, curr_pos_in_block, min_match)
    }

    /// Donor parity: restore the seven per-frame scratch buffers that
    /// `build_optimal_plan_impl!` borrowed via `core::mem::take`. The
    /// passed `result` tuple is the parser's `(offset, reps, litlen,
    /// match_len)` return value — kept untouched and returned so the
    /// macro chains the move-out in a single expression.
    pub(crate) fn finish_optimal_plan(
        &mut self,
        buffers: HcOptimalPlanBuffers,
        result: (u32, [u32; 3], usize, usize),
    ) -> (u32, [u32; 3], usize, usize) {
        let HcOptimalPlanBuffers {
            nodes,
            mut candidates,
            store,
            ll_prices,
            ll_price_generations,
            ml_prices,
            ml_price_generations,
        } = buffers;
        candidates.clear();
        self.opt_nodes_scratch = nodes;
        self.opt_candidates_scratch = candidates;
        self.opt_store_scratch = store;
        self.opt_ll_price_scratch = ll_prices;
        self.opt_ll_price_generation = ll_price_generations;
        self.opt_ml_price_scratch = ml_prices;
        self.opt_ml_price_generation = ml_price_generations;
        result
    }

    /// Donor parity: `ZSTD_ldm_blockCompress` would seed external
    /// long-distance match candidates here when `enableLdm ==
    /// ZSTD_ps_enable`. This Rust encoder does not expose the donor's
    /// LDM producer / runtime switch yet, so every level-22 frame
    /// starts with an empty `ldm_sequences` buffer — keep the clear
    /// to defend against carry-over if a producer is added later.
    pub(crate) fn prepare_ldm_candidates(&mut self, current_abs_start: usize, current_len: usize) {
        self.ldm_sequences.clear();
        let _ = (current_abs_start, current_len);
    }
}
