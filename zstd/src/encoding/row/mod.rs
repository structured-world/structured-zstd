//! Row-based match finder (level 4 default backend).
//!
//! Donor parity: mirrors the `ZSTD_row_*` family in `zstd_lazy.c`. The
//! row hash splits each bucket into `1 << row_log` slots (16 / 32 / 64),
//! each tagged with a 1-byte hash so the search can skip most slots
//! without touching the position table.
//!
//! Extracted from `match_generator.rs` as part of #111 Phase 1b
//! (structural split). Mechanical move — names, fields, and bodies
//! are preserved; visibility on the relocated items was opened to
//! `pub(crate)` so `match_generator` can keep dispatching to
//! `RowMatchGenerator` through the `row::` import path.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::convert::TryInto;

use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::match_generator::{
    ROW_EMPTY_SLOT, ROW_HASH_BITS, ROW_HASH_KEY_LEN, ROW_LOG, ROW_MIN_MATCH_LEN, ROW_SEARCH_DEPTH,
    ROW_TAG_BITS, ROW_TARGET_LEN, RowConfig,
};
use super::match_table::helpers::{
    INCOMPRESSIBLE_SKIP_STEP, LazyMatchConfig, best_len_offset_candidate, common_prefix_len,
    extend_backwards_shared, pick_lazy_match_shared, repcode_candidate_shared,
};
use super::opt::types::MatchCandidate;

/// Which kernel computes the row tag-match mask. Resolved once per
/// `RowMatchGenerator` (CPU features don't change mid-process) and stored
/// so the per-position `row_candidate` hot path avoids a repeated runtime
/// feature query. `Sse2` covers the SSE2 baseline (always present on
/// x86_64); `Avx2` is used when the CPU has AVX2; everything else (other
/// architectures, x86 without SSE2) uses the scalar reference.
#[derive(Copy, Clone, Eq, PartialEq)]
enum RowTagKernel {
    Scalar,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Sse2,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    Avx2,
    // Little-endian only: the movemask packing reinterprets the lanes
    // through u16/u32/u64, which groups bytes by native order — correct only
    // on little-endian. Big-endian aarch64 falls back to the scalar kernel.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    Neon,
}

impl RowTagKernel {
    /// Resolve the best available tag-match kernel for this build/CPU.
    fn detect() -> Self {
        #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            use std::arch::is_x86_feature_detected;
            if is_x86_feature_detected!("avx2") {
                return RowTagKernel::Avx2;
            }
            if is_x86_feature_detected!("sse2") {
                return RowTagKernel::Sse2;
            }
        }
        #[cfg(all(
            not(feature = "std"),
            any(target_arch = "x86", target_arch = "x86_64"),
            target_feature = "avx2"
        ))]
        {
            return RowTagKernel::Avx2;
        }
        #[cfg(all(
            not(feature = "std"),
            any(target_arch = "x86", target_arch = "x86_64"),
            target_feature = "sse2"
        ))]
        {
            return RowTagKernel::Sse2;
        }
        #[cfg(all(feature = "std", target_arch = "aarch64", target_endian = "little"))]
        {
            if std::arch::is_aarch64_feature_detected!("neon") {
                return RowTagKernel::Neon;
            }
        }
        #[cfg(all(
            not(feature = "std"),
            target_arch = "aarch64",
            target_endian = "little",
            target_feature = "neon"
        ))]
        {
            return RowTagKernel::Neon;
        }
        RowTagKernel::Scalar
    }

    /// Bitmask of row slots whose tag byte equals `tag` (bit `j` set iff
    /// `tags[j] == tag`). `tags.len()` is the row width (16 / 32 / 64, all
    /// multiples of 16) so the result fits in a `u64`.
    #[inline]
    fn match_mask(self, tags: &[u8], tag: u8) -> u64 {
        match self {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: this variant is only produced by `detect()` after
            // confirming AVX2 (runtime) or `target_feature = "avx2"`.
            RowTagKernel::Avx2 => unsafe { row_tag_match_mask_avx2(tags, tag) },
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            // SAFETY: SSE2 confirmed by `detect()` (always true on x86_64).
            RowTagKernel::Sse2 => unsafe { row_tag_match_mask_sse2(tags, tag) },
            #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
            // SAFETY: NEON confirmed by `detect()` (baseline on aarch64).
            RowTagKernel::Neon => unsafe { row_tag_match_mask_neon(tags, tag) },
            RowTagKernel::Scalar => row_tag_match_mask_scalar(tags, tag),
        }
    }
}

/// Scalar reference: one byte compare per slot. SIMD kernels below compute
/// the identical mask with a vector compare + movemask (donor
/// `ZSTD_row_getMatchMask` shape).
#[inline]
fn row_tag_match_mask_scalar(tags: &[u8], tag: u8) -> u64 {
    let mut mask = 0u64;
    for (j, &t) in tags.iter().enumerate() {
        if t == tag {
            mask |= 1u64 << j;
        }
    }
    mask
}

/// SSE2 tag-match mask: `_mm_cmpeq_epi8` + `_mm_movemask_epi8` over each
/// 16-byte chunk of the row. `tags.len()` is a multiple of 16, so no scalar
/// tail is needed.
///
/// # Safety
/// Caller must ensure SSE2 is available (always true on x86_64; checked by
/// `RowTagKernel::detect`).
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn row_tag_match_mask_sse2(tags: &[u8], tag: u8) -> u64 {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{_mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8};
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{_mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8};

    let needle = _mm_set1_epi8(tag as i8);
    let mut mask = 0u64;
    let mut off = 0;
    while off + 16 <= tags.len() {
        // SAFETY: `off + 16 <= tags.len()`, so the 16-byte unaligned load is
        // in bounds.
        let v = unsafe { _mm_loadu_si128(tags.as_ptr().add(off) as *const _) };
        let eq = _mm_cmpeq_epi8(v, needle);
        let bits = _mm_movemask_epi8(eq) as u16 as u64;
        mask |= bits << off;
        off += 16;
    }
    mask
}

/// AVX2 tag-match mask: `_mm256_cmpeq_epi8` + `_mm256_movemask_epi8` over
/// each 32-byte chunk, falling back to a 16-byte SSE2 chunk for a 16-wide
/// row. `tags.len()` is 16 / 32 / 64.
///
/// # Safety
/// Caller must ensure AVX2 is available (checked by `RowTagKernel::detect`).
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn row_tag_match_mask_avx2(tags: &[u8], tag: u8) -> u64 {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{
        _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8, _mm256_cmpeq_epi8,
        _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
    };
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{
        _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8, _mm256_cmpeq_epi8,
        _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
    };

    let needle = _mm256_set1_epi8(tag as i8);
    let mut mask = 0u64;
    let mut off = 0;
    while off + 32 <= tags.len() {
        // SAFETY: `off + 32 <= tags.len()`, so the 32-byte load is in bounds.
        let v = unsafe { _mm256_loadu_si256(tags.as_ptr().add(off) as *const _) };
        let eq = _mm256_cmpeq_epi8(v, needle);
        let bits = _mm256_movemask_epi8(eq) as u32 as u64;
        mask |= bits << off;
        off += 32;
    }
    if off + 16 <= tags.len() {
        let needle16 = _mm_set1_epi8(tag as i8);
        // SAFETY: `off + 16 <= tags.len()`, so the 16-byte load is in bounds.
        let v = unsafe { _mm_loadu_si128(tags.as_ptr().add(off) as *const _) };
        let eq = _mm_cmpeq_epi8(v, needle16);
        let bits = _mm_movemask_epi8(eq) as u16 as u64;
        mask |= bits << off;
    }
    mask
}

/// NEON tag-match mask: `vceqq_u8` against the broadcast tag, then a
/// 16-byte-to-16-bit movemask (NEON has no direct equivalent, so pack the
/// per-lane high bits with the progressive shift-right-accumulate sequence).
/// `tags.len()` is a multiple of 16.
///
/// # Safety
/// Caller must ensure NEON is available (baseline on aarch64; checked by
/// `RowTagKernel::detect`).
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn row_tag_match_mask_neon(tags: &[u8], tag: u8) -> u64 {
    use core::arch::aarch64::{
        vceqq_u8, vdupq_n_u8, vgetq_lane_u8, vld1q_u8, vreinterpretq_u8_u64, vreinterpretq_u16_u8,
        vreinterpretq_u32_u16, vreinterpretq_u64_u32, vshrq_n_u8, vsraq_n_u16, vsraq_n_u32,
        vsraq_n_u64,
    };

    let needle = vdupq_n_u8(tag);
    let mut mask = 0u64;
    let mut off = 0;
    while off + 16 <= tags.len() {
        // SAFETY: `off + 16 <= tags.len()`, so the 16-byte load is in bounds.
        let v = unsafe { vld1q_u8(tags.as_ptr().add(off)) };
        let eq = vceqq_u8(v, needle); // 0xFF per lane where equal, else 0x00.
        // Collapse the 16 lanes' high bits into a 16-bit movemask, matching
        // `_mm_movemask_epi8` lane order. Each `vsra` shifts a lane right and
        // adds it into its neighbour, halving the live-lane count each step.
        let high = vshrq_n_u8(eq, 7); // lane -> 1 if matched, else 0.
        let paired16 = vreinterpretq_u32_u16(vsraq_n_u16(
            vreinterpretq_u16_u8(high),
            vreinterpretq_u16_u8(high),
            7,
        ));
        let paired32 = vreinterpretq_u64_u32(vsraq_n_u32(paired16, paired16, 14));
        let paired64 = vreinterpretq_u8_u64(vsraq_n_u64(paired32, paired32, 28));
        let bits = (vgetq_lane_u8(paired64, 0) as u64) | ((vgetq_lane_u8(paired64, 8) as u64) << 8);
        mask |= bits << off;
        off += 16;
    }
    mask
}

pub(crate) struct RowMatchGenerator {
    pub(crate) max_window_size: usize,
    pub(crate) window: VecDeque<Vec<u8>>,
    pub(crate) window_size: usize,
    pub(crate) history: Vec<u8>,
    pub(crate) history_start: usize,
    pub(crate) history_abs_start: usize,
    pub(crate) offset_hist: [u32; 3],
    pub(crate) row_hash_log: usize,
    pub(crate) row_log: usize,
    pub(crate) search_depth: usize,
    pub(crate) target_len: usize,
    pub(crate) lazy_depth: u8,
    /// Cached fastpath kernel for `hash_mix_u64`; see Dfast for rationale.
    pub(crate) hash_kernel: crate::encoding::fastpath::FastpathKernel,
    pub(crate) row_heads: Vec<u8>,
    pub(crate) row_positions: Vec<usize>,
    pub(crate) row_tags: Vec<u8>,
    /// Cached tag-match SIMD kernel; CPU features are fixed per process, so
    /// resolve once instead of querying per `row_candidate` call.
    tag_kernel: RowTagKernel,
}

impl RowMatchGenerator {
    pub(crate) fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            row_hash_log: ROW_HASH_BITS - ROW_LOG,
            row_log: ROW_LOG,
            search_depth: ROW_SEARCH_DEPTH,
            target_len: ROW_TARGET_LEN,
            lazy_depth: 1,
            hash_kernel: crate::encoding::fastpath::select_kernel(),
            row_heads: Vec::new(),
            row_positions: Vec::new(),
            row_tags: Vec::new(),
            tag_kernel: RowTagKernel::detect(),
        }
    }

    pub(crate) fn set_hash_bits(&mut self, bits: usize) {
        let clamped = bits.clamp(self.row_log + 1, ROW_HASH_BITS);
        let row_hash_log = clamped.saturating_sub(self.row_log);
        if self.row_hash_log != row_hash_log {
            self.row_hash_log = row_hash_log;
            self.row_heads.clear();
            self.row_positions.clear();
            self.row_tags.clear();
        }
    }

    pub(crate) fn configure(&mut self, config: RowConfig) {
        self.row_log = config.row_log.clamp(4, 6);
        self.search_depth = config.search_depth;
        self.target_len = config.target_len;
        self.set_hash_bits(config.hash_bits.max(self.row_log + 1));
    }

    pub(crate) fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.offset_hist = [1, 4, 8];
        self.row_heads.fill(0);
        self.row_positions.fill(ROW_EMPTY_SLOT);
        self.row_tags.fill(0);
        for mut data in self.window.drain(..) {
            data.resize(data.capacity(), 0);
            reuse_space(data);
        }
    }

    pub(crate) fn get_last_space(&self) -> &[u8] {
        self.window.back().unwrap().as_slice()
    }

    pub(crate) fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        super::match_table::storage::check_stream_abs_headroom(
            self.history_abs_start,
            self.window_size,
            data.len(),
        );
        while self.window_size + data.len() > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.window.push_back(data);
    }

    pub(crate) fn trim_to_window(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        while self.window_size > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
    }

    pub(crate) fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let backfill_start = self.backfill_start(current_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        match incompressible_hint {
            Some(true) => {
                // Sparse step + dense tail: caller declared the block
                // unlikely to compress, so we seed only every
                // `INCOMPRESSIBLE_SKIP_STEP` position plus a small tail to
                // keep cross-block continuity at the boundary.
                self.insert_positions_with_step(
                    current_abs_start,
                    current_abs_end,
                    INCOMPRESSIBLE_SKIP_STEP,
                );
                let dense_tail = ROW_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
                let tail_start = current_abs_end
                    .saturating_sub(dense_tail)
                    .max(current_abs_start);
                for pos in tail_start..current_abs_end {
                    if !(pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                        self.insert_position(pos);
                    }
                }
            }
            Some(false) => {
                // Dense seeding requested by the caller: the entire
                // skipped range must remain queryable so subsequent
                // blocks can match into it. Currently only used by the
                // dictionary-priming path (donor's
                // `ZSTD_loadDictionaryContent` does the same dense fill
                // via `ZSTD_row_update_internalImpl` over every dict
                // byte), but the semantic is "dense fill on demand" and
                // future fast-paths (e.g. an RLE / raw-block emitter
                // that still wants cross-block matches into the skipped
                // bytes) can reuse it without rewording the contract.
                self.insert_positions(current_abs_start, current_abs_end);
            }
            None => {
                // Donor parity: a plain `skip_matching` (no hint) leaves
                // the row table untouched for the skipped range. Donor's
                // `ZSTD_row_fillHashCache` only pre-fills the next-scan
                // cache (8 positions of lookahead for SIMD prefetch); it
                // does NOT retroactively insert every byte of a skipped
                // block.
                //
                // Boundary handling: the `backfill_start` insert above
                // covers the `ROW_HASH_KEY_LEN - 1` bytes immediately
                // BEFORE `current_abs_start` (i.e. the previous block's
                // tail), keeping the current block's start hashable as
                // a cross-block match target. The CURRENT skipped
                // block's tail (the `ROW_HASH_KEY_LEN - 1` bytes ending
                // at `current_abs_end`) is itself backfilled lazily —
                // by the NEXT call's own `backfill_start` insert when
                // that call's `current_abs_start` lands at
                // `current_abs_end`. So a parse of block N+1 sees
                // block N's tail in the row table but not its
                // interior, matching donor.
                //
                // Trade: cross-block matches into a skipped block's
                // interior are lost (rare in practice — `skip_matching`
                // is called on blocks the driver upstream identified as
                // not worth scanning), but the per-block O(block_size)
                // `insert_position` storm is gone. On the L4 large-log-
                // stream bench (~104 MB / 800 blocks) the prior dense
                // fill dominated ~25% of Rust self-time at 131K inserts
                // per block × 800 = ~104M inserts.
            }
        }
    }

    /// Lazy-style parse (depth >= 1) for the Row backend.
    ///
    /// Currently unused: the only strategy mapped to `BackendTag::Row`
    /// is `StrategyTag::Greedy` (level 4), which dispatches to
    /// [`Self::start_matching_greedy`] via the `debug_assert_eq!` in
    /// the `BackendTag::Row` arm of
    /// `MatchGeneratorDriver::compress_block`. This method is kept as
    /// scaffolding for the case where a future level routes a lazy
    /// strategy through the Row backend — extracting `pick_lazy_match`
    /// behavior to a fresh module then would mean re-deriving the
    /// row-hash machinery, which is wasteful. The `dead_code` allow is
    /// scoped to this method and its private helpers so any new
    /// caller will pick them up unmodified.
    #[allow(dead_code)]
    pub(crate) fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let backfill_start = self.backfill_start(current_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }

        let mut pos = 0usize;
        let mut literals_start = 0usize;
        while pos + ROW_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.best_match(abs_pos, lit_len);
            if let Some(candidate) = self.pick_lazy_match(abs_pos, lit_len, best) {
                self.insert_positions(abs_pos, candidate.start + candidate.match_len);
                let current = self.window.back().unwrap().as_slice();
                let start = candidate.start - current_abs_start;
                let literals = &current[literals_start..start];
                handle_sequence(Sequence::Triple {
                    literals,
                    offset: candidate.offset,
                    match_len: candidate.match_len,
                });
                let _ = encode_offset_with_history(
                    candidate.offset as u32,
                    literals.len() as u32,
                    &mut self.offset_hist,
                );
                pos = start + candidate.match_len;
                literals_start = pos;
            } else {
                self.insert_position(abs_pos);
                pos += 1;
            }
        }

        while pos + ROW_HASH_KEY_LEN <= current_len {
            self.insert_position(current_abs_start + pos);
            pos += 1;
        }

        if literals_start < current_len {
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    /// Donor-parity greedy parse for `lazy_depth == 0` (level 4).
    ///
    /// Mirrors `ZSTD_compressBlock_lazy_generic` (`zstd_lazy.c:1560`) with
    /// `depth == 0`, `dictMode == ZSTD_noDict`. The structural features
    /// that distinguish this greedy parse from the lazy parse in
    /// [`Self::start_matching`] (which `lazy_depth >= 1` strategies use):
    ///
    /// 1. **Default `start = pos + 1`**: each iteration first probes the
    ///    repcode bank at `abs_pos + 1` (treating one literal byte as
    ///    already committed). Donor's `start = ip + 1; matchLength = 0;
    ///    offBase = REPCODE1_TO_OFFBASE;` at the top of the loop body.
    ///    Only if a regular match at `abs_pos` is strictly longer does
    ///    `start` slide back to `abs_pos`. This trades one literal byte
    ///    for an unconditional repcode probe, which is the algorithmic
    ///    reason the strategy is called "greedy" — it greedily picks the
    ///    cheaper repcode encoding (4-5 bits) over a longer-offset
    ///    regular match (9-13 bits) whenever the rep hit is close to
    ///    matching the regular match's length.
    ///
    /// 2. **Hybrid commit, not donor's pure `goto _storeSequence`**:
    ///    donor's depth-0 path jumps to `_storeSequence` on the first
    ///    repcode hit and skips the regular search at `abs_pos`. We
    ///    deviate here — both the rep probe at `abs_pos + 1` *and* the
    ///    regular `row_candidate(abs_pos, ..)` are evaluated each
    ///    iteration, and the longer match wins (ties go to rep for
    ///    cheaper encoding via [`best_len_offset_candidate`]). Donor
    ///    can afford pure commit-on-first-rep because it recovers any
    ///    ratio loss via `minMatch = 5` and superblock-level entropy
    ///    sharing; we don't replicate those yet, so the hybrid form
    ///    avoids a measured ~3pp ratio cliff on decodecorpus while
    ///    still skipping the donor `lazy_depth == 1` lookahead probe
    ///    that [`start_matching`] above runs unconditionally — the
    ///    speed shape stays donor-like.
    ///
    /// 3. **Skip-step grows with literal-run length**: on a miss donor
    ///    advances `ip += ((ip - anchor) >> kSearchStrength) + 1` with
    ///    `kSearchStrength = 8`. The plain matcher steps by 1 — denser
    ///    hash inserts (mild ratio benefit), but the donor parity skip
    ///    halves the per-byte work on incompressible runs (the
    ///    `lazySkipping` mode in donor is an extension of the same idea).
    ///
    /// Donor has an immediate-rep loop after store that probes
    /// `offset_2` for back-to-back hits. It is omitted here: the
    /// main-loop rep probe at `abs_pos + 1` already evaluates all
    /// three rep slots (rep1, rep2, rep3 + the donor `ll0` fallback)
    /// via [`repcode_candidate_shared`], so the inner-loop slot
    /// donor's single-rep design would catch is already covered by
    /// the next main-loop iteration. Confirmed dead-on-arrival via a
    /// `panic!` probe across the full 528-test suite + benchmark
    /// matrix (never fires).
    ///
    /// Catch-up backwards extension is already absorbed into the
    /// `MatchCandidate.start` field by `extend_backwards_shared`
    /// (called from `row_candidate` and `repcode_candidate_shared`),
    /// so we don't redo it explicitly.
    ///
    /// `pick_lazy_match` is intentionally not called here — depth == 0
    /// means "no lookahead", emit the first viable hit.
    pub(crate) fn start_matching_greedy(
        &mut self,
        mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        self.ensure_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let backfill_start = self.backfill_start(current_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }

        // Donor mls for repcode probes is 4 (`MEM_read32` compare on
        // `ip+1` against `ip+1-offset_1`, length extended by
        // `ZSTD_count + 4`). The row matcher's `ROW_MIN_MATCH_LEN = 6`
        // gates the *regular* search via the row-table layout; rep
        // probes are independent of the row table and benefit from
        // the lower donor threshold (a 4-5 byte rep is cheap to
        // encode and frequently outperforms emitting the bytes as
        // literals).
        const REP_MIN_MATCH_LEN: usize = 4;
        // Outer-loop lookahead floor: at least `REP_MIN_MATCH_LEN + 1`
        // bytes left so the `abs_pos + 1` repcode probe can succeed
        // even in the block tail. Gating the loop on the stricter
        // `ROW_MIN_MATCH_LEN` (6) would miss the last 5-byte rep-only
        // case and let those bytes fall through as literals.
        const GREEDY_MIN_LOOKAHEAD: usize = REP_MIN_MATCH_LEN + 1;

        let mut pos = 0usize;
        let mut literals_start = 0usize;

        while pos + GREEDY_MIN_LOOKAHEAD <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            // (1) Default start = abs_pos + 1: probe the repcode bank
            //     at the next byte, treating one byte as already
            //     committed to the literal run. Donor probes only
            //     rep1 here; `repcode_candidate_shared` probes all three
            //     plus the `ll0` fallback because the donor "ll0" trick
            //     is already baked into our shared helper. The extra
            //     probes only add candidates that have repcode encoding
            //     costs (cheap), so the ratio direction is positive vs
            //     donor while still landing in the "greedy via repcode"
            //     algorithmic shape.
            let rep_probe_pos = abs_pos + 1;
            let rep_probe_lit_len = lit_len + 1;
            let rep_match = if rep_probe_pos + REP_MIN_MATCH_LEN <= self.history_abs_end() {
                repcode_candidate_shared(
                    self.live_history(),
                    self.history_abs_start,
                    self.offset_hist,
                    rep_probe_pos,
                    rep_probe_lit_len,
                    REP_MIN_MATCH_LEN,
                )
            } else {
                None
            };

            // (2) Donor at `depth == 0` does `goto _storeSequence` on a
            //     rep hit (commits without comparing against the regular
            //     search). That trade-off is ratio-negative for us
            //     because donor recovers the loss via other components
            //     we don't replicate (smaller `mls=5`, block splitter,
            //     better regular-search recall). To get the speed shape
            //     of donor's greedy *without* its ratio cliff, we
            //     compare both options and pick the longer match. On
            //     ties / near-ties the rep wins by being cheaper to
            //     encode (single-digit-bit offset code vs 9-13 bits for
            //     a regular offset).
            let regular_match = self.row_candidate(abs_pos, lit_len);
            let chosen = match (rep_match, regular_match) {
                (Some(rep), Some(reg)) => {
                    // Prefer the longer; tie-break to rep for cheaper
                    // encoding. `best_len_offset_candidate` ties on
                    // shorter offset which is the wrong direction for
                    // rep-vs-regular (regular offsets are always bigger
                    // than the corresponding rep, so it would always
                    // pick rep on ties — that's the right choice here
                    // but we want strict length preference too).
                    if reg.match_len > rep.match_len {
                        Some(reg)
                    } else {
                        Some(rep)
                    }
                }
                (Some(rep), None) => Some(rep),
                (None, reg) => reg,
            };

            let Some(candidate) = chosen else {
                // Donor `kSearchStrength = 8` shifts hard on miss
                // (step grows by `lit_len >> 8`). Empirically on our
                // corpus that recovers ~30% speed but costs ratio by
                // dropping hash inserts on long literal runs that
                // would have served future matches. Shift right by
                // `SKIP_STRENGTH = 10` instead — same shape, ~4×
                // rarer growth, so the step stays at 1 byte until the
                // literal run hits ~1 KiB and only then begins
                // skipping. Lets us keep most of donor's speed
                // characteristic without re-introducing the ratio
                // drain.
                const SKIP_STRENGTH: u32 = 10;
                let step = ((lit_len as u32) >> SKIP_STRENGTH) as usize + 1;
                self.insert_position(abs_pos);
                pos += step;
                continue;
            };

            // Emit sequence.
            let start = candidate.start - current_abs_start;
            // Index `[abs_pos, candidate.start + match_len)`, NOT
            // `[candidate.start, candidate.start + match_len)`.
            // `extend_backwards_shared` can move `candidate.start`
            // below `abs_pos` by absorbing literal bytes that the
            // outer loop already indexed on earlier miss iterations
            // via `insert_position(abs_pos)`. Re-indexing them here
            // would write the same `abs_pos -> position` mapping
            // into the row table a second time, evicting more recent
            // / more useful slot tenants from the same row's chain.
            // Measured on `decodecorpus-z000033`: the
            // `candidate.start` lower bound regresses `rust_bytes` by
            // ~+447 over `abs_pos` (537897 -> 538344), so the
            // narrower range is intentional.
            self.insert_positions(abs_pos, candidate.start + candidate.match_len);
            let current = self.window.back().unwrap().as_slice();
            let literals = &current[literals_start..start];
            handle_sequence(Sequence::Triple {
                literals,
                offset: candidate.offset,
                match_len: candidate.match_len,
            });
            let _ = encode_offset_with_history(
                candidate.offset as u32,
                literals.len() as u32,
                &mut self.offset_hist,
            );
            pos = start + candidate.match_len;
            literals_start = pos;

            // Donor's `lazy_generic` has an immediate-repcode loop here
            // (probing `offset_2` after each main emit and swapping
            // `offset_1 ↔ offset_2` on hit). It was implemented and
            // shipped in earlier iterations of this method but never
            // fired on any test or benchmark workload — the
            // `repcode_candidate_shared` probe at the top of the main
            // loop already evaluates all three rep slots (rep1, rep2,
            // rep3 + the `ll0` fallback), and the immediate-rep slot
            // (`offset_hist[1]` at `lit_len = 0`) is subsumed by the
            // next main-loop iteration's rep probe of the same slot.
            // Donor's version is single-rep, so the inner loop catches
            // hits its main-loop probe wouldn't; ours is three-rep, so
            // the inner loop is dead by construction. Removed to free
            // the per-iteration check and keep the parser body lean.
        }

        while pos + ROW_HASH_KEY_LEN <= current_len {
            self.insert_position(current_abs_start + pos);
            pos += 1;
        }

        if literals_start < current_len {
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    pub(crate) fn ensure_tables(&mut self) {
        let row_count = 1usize << self.row_hash_log;
        let row_entries = 1usize << self.row_log;
        let total = row_count * row_entries;
        if self.row_positions.len() != total {
            self.row_heads = alloc::vec![0; row_count];
            self.row_positions = alloc::vec![ROW_EMPTY_SLOT; total];
            self.row_tags = alloc::vec![0; total];
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        if self.history_start >= self.max_window_size
            || self.history_start * 2 >= self.history.len()
        {
            self.history.drain(..self.history_start);
            self.history_start = 0;
        }
    }

    pub(crate) fn live_history(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    pub(crate) fn hash_and_row(&self, abs_pos: usize) -> Option<(usize, u8)> {
        let idx = abs_pos - self.history_abs_start;
        let concat = self.live_history();
        if idx + ROW_HASH_KEY_LEN > concat.len() {
            return None;
        }
        let value =
            u32::from_le_bytes(concat[idx..idx + ROW_HASH_KEY_LEN].try_into().unwrap()) as u64;
        let hash = crate::encoding::fastpath::hash_mix_u64_with_kernel(self.hash_kernel, value);
        let total_bits = self.row_hash_log + ROW_TAG_BITS;
        let combined = hash >> (u64::BITS as usize - total_bits);
        let row_mask = (1usize << self.row_hash_log) - 1;
        let row = ((combined >> ROW_TAG_BITS) as usize) & row_mask;
        let tag = combined as u8;
        Some((row, tag))
    }

    fn backfill_start(&self, current_abs_start: usize) -> usize {
        current_abs_start
            .saturating_sub(ROW_HASH_KEY_LEN - 1)
            .max(self.history_abs_start)
    }

    /// Used only by the dead-code [`Self::start_matching`] (lazy-style
    /// row parse). Kept paired with that method so reviving the lazy
    /// path doesn't have to re-derive the rep+row best-of-two pick.
    #[allow(dead_code)]
    pub(crate) fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let row = self.row_candidate(abs_pos, lit_len);
        best_len_offset_candidate(rep, row)
    }

    #[allow(dead_code)]
    pub(crate) fn pick_lazy_match(
        &self,
        abs_pos: usize,
        lit_len: usize,
        best: Option<MatchCandidate>,
    ) -> Option<MatchCandidate> {
        pick_lazy_match_shared(
            abs_pos,
            lit_len,
            best,
            LazyMatchConfig {
                target_len: self.target_len,
                min_match_len: ROW_MIN_MATCH_LEN,
                lazy_depth: self.lazy_depth,
                history_abs_end: self.history_abs_end(),
            },
            |next_pos, next_lit_len| self.best_match(next_pos, next_lit_len),
        )
    }

    #[allow(dead_code)]
    pub(crate) fn repcode_candidate(
        &self,
        abs_pos: usize,
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        repcode_candidate_shared(
            self.live_history(),
            self.history_abs_start,
            self.offset_hist,
            abs_pos,
            lit_len,
            ROW_MIN_MATCH_LEN,
        )
    }

    pub(crate) fn row_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        if current_idx + ROW_MIN_MATCH_LEN > concat.len() {
            return None;
        }

        let (row, tag) = self.hash_and_row(abs_pos)?;
        let row_entries = 1usize << self.row_log;
        let row_mask = row_entries - 1;
        let row_base = row << self.row_log;
        let head = self.row_heads[row] as usize;
        let max_walk = self.search_depth.min(row_entries);

        // Compare all `row_entries` tags against `tag` in one pass, producing a
        // bitmask whose bit `j` is set iff `row_tags[row_base + j] == tag`. The
        // walk below then tests a single bit per slot instead of re-reading the
        // tag byte, which isolates the tag comparison into a form a SIMD kernel
        // can compute with one vector compare + movemask. `row_entries <= 64`
        // (row_log clamps to 4..=6) so the mask fits in a `u64`.
        let tag_match = self
            .tag_kernel
            .match_mask(&self.row_tags[row_base..row_base + row_entries], tag);

        let mut best = None;
        for i in 0..max_walk {
            let slot = (head + i) & row_mask;
            if (tag_match >> slot) & 1 == 0 {
                continue;
            }
            let idx = row_base + slot;
            let candidate_pos = self.row_positions[idx];
            if candidate_pos == ROW_EMPTY_SLOT
                || candidate_pos < self.history_abs_start
                || candidate_pos >= abs_pos
            {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
            if match_len >= ROW_MIN_MATCH_LEN {
                let candidate = self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                best = best_len_offset_candidate(best, Some(candidate));
                if best.is_some_and(|best| best.match_len >= self.target_len) {
                    return best;
                }
            }
        }
        best
    }

    fn extend_backwards(
        &self,
        candidate_pos: usize,
        abs_pos: usize,
        match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        extend_backwards_shared(
            self.live_history(),
            self.history_abs_start,
            candidate_pos,
            abs_pos,
            match_len,
            lit_len,
        )
    }

    fn insert_positions(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position(pos);
        }
    }

    fn insert_positions_with_step(&mut self, start: usize, end: usize, step: usize) {
        if step <= 1 {
            self.insert_positions(start, end);
            return;
        }
        let mut pos = start;
        while pos < end {
            self.insert_position(pos);
            let next = pos.saturating_add(step);
            if next <= pos {
                break;
            }
            pos = next;
        }
    }

    #[inline]
    fn insert_position(&mut self, abs_pos: usize) {
        let Some((row, tag)) = self.hash_and_row(abs_pos) else {
            return;
        };
        let row_entries = 1usize << self.row_log;
        let row_mask = row_entries - 1;
        let row_base = row << self.row_log;
        // SAFETY: `hash_and_row` masks `row` to `row_hash_log` bits and
        // `row_heads.len() == 1 << row_hash_log` by `ensure_tables`.
        // `row_base = row << row_log = row * row_entries` and
        // `next < row_entries`, so `row_base + next < row_count *
        // row_entries == row_positions.len() == row_tags.len()`. Both
        // index pairs are provably in bounds; per-byte hot path on
        // fast/dfast/row levels saves ~6 instructions and 3 branches.
        debug_assert!(row < self.row_heads.len());
        debug_assert!(row_base + row_entries <= self.row_positions.len());
        unsafe {
            let head = *self.row_heads.get_unchecked(row) as usize;
            let next = head.wrapping_sub(1) & row_mask;
            *self.row_heads.get_unchecked_mut(row) = next as u8;
            *self.row_tags.get_unchecked_mut(row_base + next) = tag;
            *self.row_positions.get_unchecked_mut(row_base + next) = abs_pos;
        }
    }
}

// Gated on `feature = "std"` because the runtime feature probe
// (`std::arch::is_x86_feature_detected!`) used to skip kernels the host CPU
// lacks is std-only, matching how `RowTagKernel::detect` gates the same probe.
#[cfg(all(
    test,
    feature = "std",
    any(target_arch = "x86", target_arch = "x86_64")
))]
mod tag_mask_tests {
    use super::{row_tag_match_mask_avx2, row_tag_match_mask_scalar, row_tag_match_mask_sse2};

    /// Deterministic LCG fill so the test exercises a realistic spread of
    /// matching / non-matching tag bytes without a RNG dependency.
    fn fill(buf: &mut [u8], mut state: u64) {
        for b in buf.iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 56) as u8;
        }
    }

    /// The SIMD kernels must produce byte-identical masks to the scalar
    /// reference for every supported row width (16 / 32 / 64) and tag, or
    /// the match selection diverges and the compressed output changes.
    #[test]
    fn simd_tag_mask_matches_scalar() {
        for &width in &[16usize, 32, 64] {
            let mut tags = alloc::vec![0u8; width];
            for seed in 0..32u64 {
                fill(&mut tags, 0x9e3779b97f4a7c15u64.wrapping_add(seed));
                // Cover both a tag that occurs in the row and arbitrary tags.
                for tag in [tags[seed as usize % width], 0u8, 0xFF, (seed as u8)] {
                    let expected = row_tag_match_mask_scalar(&tags, tag);
                    if std::arch::is_x86_feature_detected!("sse2") {
                        let got = unsafe { row_tag_match_mask_sse2(&tags, tag) };
                        assert_eq!(got, expected, "sse2 width={width} tag={tag}");
                    }
                    if std::arch::is_x86_feature_detected!("avx2") {
                        let got = unsafe { row_tag_match_mask_avx2(&tags, tag) };
                        assert_eq!(got, expected, "avx2 width={width} tag={tag}");
                    }
                }
            }
        }
    }
}

#[cfg(all(test, target_arch = "aarch64", target_endian = "little"))]
mod neon_tag_mask_tests {
    use super::{row_tag_match_mask_neon, row_tag_match_mask_scalar};

    fn fill(buf: &mut [u8], mut state: u64) {
        for b in buf.iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 56) as u8;
        }
    }

    /// The NEON kernel must produce byte-identical masks to the scalar
    /// reference for every supported row width (16 / 32 / 64) and tag, so
    /// match selection (and the compressed output) is unchanged on aarch64.
    #[test]
    fn neon_tag_mask_matches_scalar() {
        for &width in &[16usize, 32, 64] {
            let mut tags = alloc::vec![0u8; width];
            for seed in 0..32u64 {
                fill(&mut tags, 0x9e3779b97f4a7c15u64.wrapping_add(seed));
                for tag in [tags[seed as usize % width], 0u8, 0xFF, (seed as u8)] {
                    let expected = row_tag_match_mask_scalar(&tags, tag);
                    // SAFETY: NEON is baseline on aarch64.
                    let got = unsafe { row_tag_match_mask_neon(&tags, tag) };
                    assert_eq!(got, expected, "neon width={width} tag={tag}");
                }
            }
        }
    }
}
