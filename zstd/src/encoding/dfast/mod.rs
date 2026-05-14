//! Double-fast match finder (default-level backend, donor parity for
//! `ZSTD_dfast.c`). Two parallel hash chains — a 4-byte short hash and
//! an 8-byte long hash — feed an adaptive sparse search that bails out
//! when consecutive misses suggest an incompressible region.
//!
//! Extracted from `match_generator.rs` as part of #111 Phase 1b
//! (structural split). Mechanical move — names, fields, method bodies,
//! constants, and the `#[inline]` annotations are preserved; the
//! visibility on the relocated items was opened to `pub(crate)` so
//! `match_generator` can keep dispatching to `DfastMatchGenerator`
//! through the `dfast::` import path.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::convert::TryInto;

use super::Sequence;
use super::blocks::encode_offset_with_history;
use super::incompressible::{block_looks_incompressible, block_looks_incompressible_strict};
use super::match_generator::{
    DFAST_EMPTY_SLOT, DFAST_HASH_BITS, DFAST_INCOMPRESSIBLE_SKIP_STEP, DFAST_LOCAL_SKIP_TRIGGER,
    DFAST_MAX_SKIP_STEP, DFAST_MIN_MATCH_LEN, DFAST_SEARCH_DEPTH, DFAST_SHORT_HASH_LOOKAHEAD,
    DFAST_SKIP_STEP_GROWTH_INTERVAL, DFAST_TARGET_LEN, MIN_WINDOW_LOG,
};
use super::match_table::helpers::{
    LazyMatchConfig, best_len_offset_candidate, common_prefix_len, extend_backwards_shared,
    pick_lazy_match_shared, repcode_candidate_shared,
};
use super::opt::types::MatchCandidate;

pub(crate) struct DfastMatchGenerator {
    pub(crate) max_window_size: usize,
    pub(crate) window: VecDeque<Vec<u8>>,
    pub(crate) window_size: usize,
    // We keep a contiguous searchable history to avoid rebuilding and reseeding
    // the matcher state from disjoint block buffers on every block.
    pub(crate) history: Vec<u8>,
    pub(crate) history_start: usize,
    pub(crate) history_abs_start: usize,
    pub(crate) offset_hist: [u32; 3],
    pub(crate) short_hash: Vec<[usize; DFAST_SEARCH_DEPTH]>,
    pub(crate) long_hash: Vec<[usize; DFAST_SEARCH_DEPTH]>,
    pub(crate) hash_bits: usize,
    /// Cached fastpath kernel for `hash_mix_u64`. Resolved once at `new()`
    /// and reused on every `hash_index` call so we skip the per-call
    /// `OnceLock` atomic load that `dispatch_hash_mix_u64` would pay.
    pub(crate) hash_kernel: crate::encoding::fastpath::FastpathKernel,
    pub(crate) use_fast_loop: bool,
    // Lazy match lookahead depth (internal tuning parameter).
    pub(crate) lazy_depth: u8,
}

impl DfastMatchGenerator {
    // Keep a short dense tail at block boundaries for two related reasons:
    // 1) insert_position() needs short (4-byte) and long (8-byte) lookahead,
    //    so appending a new block can make starts from the previous block newly
    //    hashable and require backfill;
    // 2) we also need enough trailing bytes from the previous block to preserve
    //    cross-block matching for the minimum match length.
    pub(crate) const BOUNDARY_DENSE_TAIL_LEN: usize = DFAST_MIN_MATCH_LEN + 3;

    pub(crate) fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            short_hash: Vec::new(),
            long_hash: Vec::new(),
            hash_bits: DFAST_HASH_BITS,
            hash_kernel: crate::encoding::fastpath::select_kernel(),
            use_fast_loop: false,
            lazy_depth: 1,
        }
    }

    pub(crate) fn set_hash_bits(&mut self, bits: usize) {
        let clamped = bits.clamp(MIN_WINDOW_LOG as usize, DFAST_HASH_BITS);
        if self.hash_bits != clamped {
            self.hash_bits = clamped;
            self.short_hash = Vec::new();
            self.long_hash = Vec::new();
        }
    }

    pub(crate) fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.offset_hist = [1, 4, 8];
        if !self.short_hash.is_empty() {
            self.short_hash.fill([DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]);
            self.long_hash.fill([DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]);
        }
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

    pub(crate) fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_hash_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let tail_start = current_abs_start.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        if tail_start < current_abs_start {
            self.insert_positions(tail_start, current_abs_start);
        }

        let used_sparse = incompressible_hint
            .unwrap_or_else(|| self.block_looks_incompressible(current_abs_start, current_abs_end));
        if used_sparse {
            self.insert_positions_with_step(
                current_abs_start,
                current_abs_end,
                DFAST_INCOMPRESSIBLE_SKIP_STEP,
            );
        } else {
            self.insert_positions(current_abs_start, current_abs_end);
        }

        // Seed the tail densely only after sparse insertion so the next block
        // can match across the boundary without rehashing the full block twice.
        if used_sparse {
            let tail_start = current_abs_end
                .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
                .max(current_abs_start);
            if tail_start < current_abs_end {
                self.insert_positions(tail_start, current_abs_end);
            }
        }
    }

    pub(crate) fn skip_matching_dense(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window.back().unwrap().len();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let backfill_start = current_abs_start
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
            .max(self.history_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        self.insert_positions(current_abs_start, current_abs_end);
    }

    pub(crate) fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_hash_tables();

        let current_len = self.window.back().unwrap().len();
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        if self.use_fast_loop {
            self.start_matching_fast_loop(current_abs_start, current_len, &mut handle_sequence);
            return;
        }
        self.start_matching_general(current_abs_start, current_len, &mut handle_sequence);
    }

    fn start_matching_general(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        let use_adaptive_skip =
            self.block_looks_incompressible(current_abs_start, current_abs_start + current_len);
        let mut pos = 1usize;
        let mut literals_start = 0usize;
        let mut skip_step = 1usize;
        let mut next_skip_growth_pos = DFAST_SKIP_STEP_GROWTH_INTERVAL;
        let mut miss_run = 0usize;
        while pos + DFAST_MIN_MATCH_LEN <= current_len {
            let abs_pos = current_abs_start + pos;
            let lit_len = pos - literals_start;

            let best = self.best_match(abs_pos, lit_len);
            if let Some(candidate) = self.pick_lazy_match(abs_pos, lit_len, best) {
                let start = self.emit_candidate(
                    current_abs_start,
                    &mut literals_start,
                    candidate,
                    handle_sequence,
                );
                pos = start + candidate.match_len;
                skip_step = 1;
                next_skip_growth_pos = pos + DFAST_SKIP_STEP_GROWTH_INTERVAL;
                miss_run = 0;
            } else {
                self.insert_position(abs_pos);
                miss_run += 1;
                let use_local_adaptive_skip = miss_run >= DFAST_LOCAL_SKIP_TRIGGER;
                if use_adaptive_skip || use_local_adaptive_skip {
                    let skip_cap = if use_adaptive_skip {
                        DFAST_MAX_SKIP_STEP
                    } else {
                        2
                    };
                    if pos >= next_skip_growth_pos {
                        skip_step = (skip_step + 1).min(skip_cap);
                        next_skip_growth_pos += DFAST_SKIP_STEP_GROWTH_INTERVAL;
                    }
                    pos += skip_step;
                } else {
                    pos += 1;
                }
            }
        }

        self.seed_remaining_hashable_starts(current_abs_start, current_len, pos);
        self.emit_trailing_literals(literals_start, handle_sequence);
    }

    fn start_matching_fast_loop(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        let block_is_strict_incompressible = self
            .block_looks_incompressible_strict(current_abs_start, current_abs_start + current_len);
        let mut pos = 1usize;
        let mut literals_start = 0usize;
        let mut skip_step = 1usize;
        let mut next_skip_growth_pos = DFAST_SKIP_STEP_GROWTH_INTERVAL;
        let mut miss_run = 0usize;
        while pos + DFAST_MIN_MATCH_LEN <= current_len {
            let ip0 = pos;
            let ip1 = ip0 + 1;
            let ip2 = ip0 + 2;
            let ip3 = ip0 + 3;

            let abs_ip0 = current_abs_start + ip0;
            let lit_len_ip0 = ip0 - literals_start;

            if ip2 + DFAST_MIN_MATCH_LEN <= current_len {
                let abs_ip2 = current_abs_start + ip2;
                let lit_len_ip2 = ip2 - literals_start;
                if let Some(rep) = self.repcode_candidate(abs_ip2, lit_len_ip2)
                    && rep.start >= current_abs_start + literals_start
                    && rep.start <= abs_ip2
                {
                    let start = self.emit_candidate(
                        current_abs_start,
                        &mut literals_start,
                        rep,
                        handle_sequence,
                    );
                    pos = start + rep.match_len;
                    skip_step = 1;
                    next_skip_growth_pos = pos + DFAST_SKIP_STEP_GROWTH_INTERVAL;
                    miss_run = 0;
                    continue;
                }
            }

            let best = self.best_match(abs_ip0, lit_len_ip0);
            if let Some(candidate) = best {
                let start = self.emit_candidate(
                    current_abs_start,
                    &mut literals_start,
                    candidate,
                    handle_sequence,
                );
                pos = start + candidate.match_len;
                skip_step = 1;
                next_skip_growth_pos = pos + DFAST_SKIP_STEP_GROWTH_INTERVAL;
                miss_run = 0;
            } else {
                self.insert_position(abs_ip0);
                if ip1 + 4 <= current_len {
                    self.insert_position(current_abs_start + ip1);
                }
                if ip2 + 4 <= current_len {
                    self.insert_position(current_abs_start + ip2);
                }
                if ip3 + 4 <= current_len {
                    self.insert_position(current_abs_start + ip3);
                }
                miss_run += 1;
                if block_is_strict_incompressible || miss_run >= DFAST_LOCAL_SKIP_TRIGGER {
                    let skip_cap = DFAST_MAX_SKIP_STEP;
                    if pos >= next_skip_growth_pos {
                        skip_step = (skip_step + 1).min(skip_cap);
                        next_skip_growth_pos += DFAST_SKIP_STEP_GROWTH_INTERVAL;
                    }
                    pos += skip_step;
                } else {
                    skip_step = 1;
                    next_skip_growth_pos = pos + DFAST_SKIP_STEP_GROWTH_INTERVAL;
                    pos += 1;
                }
            }
        }

        self.seed_remaining_hashable_starts(current_abs_start, current_len, pos);
        self.emit_trailing_literals(literals_start, handle_sequence);
    }

    pub(crate) fn seed_remaining_hashable_starts(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        pos: usize,
    ) {
        let boundary_tail_start = current_len.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        let mut seed_pos = pos.min(current_len).min(boundary_tail_start);
        while seed_pos + DFAST_SHORT_HASH_LOOKAHEAD <= current_len {
            self.insert_position(current_abs_start + seed_pos);
            seed_pos += 1;
        }
    }

    fn emit_candidate(
        &mut self,
        current_abs_start: usize,
        literals_start: &mut usize,
        candidate: MatchCandidate,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) -> usize {
        self.insert_positions(
            current_abs_start + *literals_start,
            candidate.start + candidate.match_len,
        );
        let current = self.window.back().unwrap().as_slice();
        let start = candidate.start - current_abs_start;
        let literals = &current[*literals_start..start];
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
        *literals_start = start + candidate.match_len;
        start
    }

    fn emit_trailing_literals(
        &self,
        literals_start: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        if literals_start < self.window.back().unwrap().len() {
            let current = self.window.back().unwrap().as_slice();
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }

    pub(crate) fn ensure_hash_tables(&mut self) {
        let table_len = 1usize << self.hash_bits;
        if self.short_hash.len() != table_len {
            // This is intentionally lazy so Fastest/Uncompressed never pay the
            // ~dfast-level memory cost. The current size tracks the issue's
            // zstd level-3 style parameters rather than a generic low-memory preset.
            self.short_hash = alloc::vec![[DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]; table_len];
            self.long_hash = alloc::vec![[DFAST_EMPTY_SLOT; DFAST_SEARCH_DEPTH]; table_len];
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

    pub(crate) fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    pub(crate) fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let hash = self.hash_candidate(abs_pos, lit_len);
        best_len_offset_candidate(rep, hash)
    }

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
                target_len: DFAST_TARGET_LEN,
                min_match_len: DFAST_MIN_MATCH_LEN,
                lazy_depth: self.lazy_depth,
                history_abs_end: self.history_abs_end(),
            },
            |next_pos, next_lit_len| self.best_match(next_pos, next_lit_len),
        )
    }

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
            DFAST_MIN_MATCH_LEN,
        )
    }

    pub(crate) fn hash_candidate(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        // Hoist all the per-loop invariants out of the combinator chains.
        // `short_candidates`/`long_candidates` each re-fetch `live_history`
        // and recompute `idx` from scratch inside their Option/flatten/filter
        // adapters; on a per-byte hot path (32% exclusive on default-level
        // profile) that's measurable Option/Iterator scaffolding the
        // compiler can't always erase.
        let concat = self.live_history();
        let current_idx = abs_pos - self.history_abs_start;
        let history_abs_start = self.history_abs_start;
        let mut best = None;

        // Long-hash probes first (8-byte hash → longer matches more likely).
        if current_idx + 8 <= concat.len() {
            let long_hash = self.hash8(&concat[current_idx..]);
            // SAFETY: `hash_index` masks to `hash_bits` and `long_hash.len()
            // == 1 << hash_bits` (`ensure_hash_tables`).
            debug_assert!(long_hash < self.long_hash.len());
            let bucket = unsafe { self.long_hash.get_unchecked(long_hash) };
            for &candidate_pos in bucket {
                if candidate_pos == DFAST_EMPTY_SLOT
                    || candidate_pos < history_abs_start
                    || candidate_pos >= abs_pos
                {
                    continue;
                }
                let candidate_idx = candidate_pos - history_abs_start;
                let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
                if match_len >= DFAST_MIN_MATCH_LEN {
                    let candidate =
                        self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                    best = best_len_offset_candidate(best, Some(candidate));
                    if best.is_some_and(|b| b.match_len >= DFAST_TARGET_LEN) {
                        return best;
                    }
                }
            }
        }

        if current_idx + 4 <= concat.len() {
            let short_hash = self.hash4(&concat[current_idx..]);
            debug_assert!(short_hash < self.short_hash.len());
            let bucket = unsafe { self.short_hash.get_unchecked(short_hash) };
            for &candidate_pos in bucket {
                if candidate_pos == DFAST_EMPTY_SLOT
                    || candidate_pos < history_abs_start
                    || candidate_pos >= abs_pos
                {
                    continue;
                }
                let candidate_idx = candidate_pos - history_abs_start;
                let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
                if match_len >= DFAST_MIN_MATCH_LEN {
                    let candidate =
                        self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len);
                    best = best_len_offset_candidate(best, Some(candidate));
                    if best.is_some_and(|b| b.match_len >= DFAST_TARGET_LEN) {
                        return best;
                    }
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

    pub(crate) fn insert_positions(&mut self, start: usize, end: usize) {
        let start = start.max(self.history_abs_start);
        let end = end.min(self.history_abs_end());
        for pos in start..end {
            self.insert_position(pos);
        }
    }

    pub(crate) fn insert_positions_with_step(&mut self, start: usize, end: usize, step: usize) {
        let start = start.max(self.history_abs_start);
        let end = end.min(self.history_abs_end());
        if step <= 1 {
            self.insert_positions(start, end);
            return;
        }
        let mut pos = start;
        while pos < end {
            self.insert_position(pos);
            pos = pos.saturating_add(step);
        }
    }

    #[inline]
    pub(crate) fn insert_position(&mut self, pos: usize) {
        let idx = pos.wrapping_sub(self.history_abs_start);
        let concat_len = self.history.len() - self.history_start;
        // SAFETY: `hash_index` masks the mixed hash to `hash_bits` bits and
        // both tables are sized to `1 << hash_bits` in `ensure_hash_tables`,
        // so every index produced here is provably below the table length.
        // Eliding the bounds check on this per-byte hot path saves ~4
        // instructions and one branch per call.
        if idx + 4 <= concat_len {
            let concat = &self.history[self.history_start..];
            let short = self.hash4(&concat[idx..]);
            debug_assert!(short < self.short_hash.len());
            let bucket = unsafe { self.short_hash.get_unchecked_mut(short) };
            if bucket[0] != pos {
                bucket.copy_within(0..DFAST_SEARCH_DEPTH - 1, 1);
                bucket[0] = pos;
            }
        }

        if idx + 8 <= concat_len {
            let concat = &self.history[self.history_start..];
            let long = self.hash8(&concat[idx..]);
            debug_assert!(long < self.long_hash.len());
            let bucket = unsafe { self.long_hash.get_unchecked_mut(long) };
            if bucket[0] != pos {
                bucket.copy_within(0..DFAST_SEARCH_DEPTH - 1, 1);
                bucket[0] = pos;
            }
        }
    }

    pub(crate) fn hash4(&self, data: &[u8]) -> usize {
        let value = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        self.hash_index(value)
    }

    pub(crate) fn hash8(&self, data: &[u8]) -> usize {
        let value = u64::from_le_bytes(data[..8].try_into().unwrap());
        self.hash_index(value)
    }

    fn block_looks_incompressible(&self, start: usize, end: usize) -> bool {
        let live = self.live_history();
        if start >= end || start < self.history_abs_start {
            return false;
        }
        let start_idx = start - self.history_abs_start;
        let end_idx = end - self.history_abs_start;
        if end_idx > live.len() {
            return false;
        }
        let block = &live[start_idx..end_idx];
        block_looks_incompressible(block)
    }

    fn block_looks_incompressible_strict(&self, start: usize, end: usize) -> bool {
        let live = self.live_history();
        if start >= end || start < self.history_abs_start {
            return false;
        }
        let start_idx = start - self.history_abs_start;
        let end_idx = end - self.history_abs_start;
        if end_idx > live.len() {
            return false;
        }
        let block = &live[start_idx..end_idx];
        block_looks_incompressible_strict(block)
    }

    fn hash_index(&self, value: u64) -> usize {
        let mixed = crate::encoding::fastpath::hash_mix_u64_with_kernel(self.hash_kernel, value);
        (mixed >> (64 - self.hash_bits)) as usize
    }
}
