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
    INCOMPRESSIBLE_SKIP_STEP, MatchGenerator, ROW_EMPTY_SLOT, ROW_HASH_BITS, ROW_HASH_KEY_LEN,
    ROW_LOG, ROW_MIN_MATCH_LEN, ROW_SEARCH_DEPTH, ROW_TAG_BITS, ROW_TARGET_LEN, RowConfig,
};
use super::match_table::helpers::{
    LazyMatchConfig, best_len_offset_candidate, extend_backwards_shared, pick_lazy_match_shared,
    repcode_candidate_shared,
};
use super::opt::types::MatchCandidate;

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
        if incompressible_hint == Some(true) {
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
        } else {
            self.insert_positions(current_abs_start, current_abs_end);
        }
    }

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

    pub(crate) fn best_match(&self, abs_pos: usize, lit_len: usize) -> Option<MatchCandidate> {
        let rep = self.repcode_candidate(abs_pos, lit_len);
        let row = self.row_candidate(abs_pos, lit_len);
        best_len_offset_candidate(rep, row)
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
                target_len: self.target_len,
                min_match_len: ROW_MIN_MATCH_LEN,
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

        let mut best = None;
        for i in 0..max_walk {
            let slot = (head + i) & row_mask;
            let idx = row_base + slot;
            if self.row_tags[idx] != tag {
                continue;
            }
            let candidate_pos = self.row_positions[idx];
            if candidate_pos == ROW_EMPTY_SLOT
                || candidate_pos < self.history_abs_start
                || candidate_pos >= abs_pos
            {
                continue;
            }
            let candidate_idx = candidate_pos - self.history_abs_start;
            let match_len =
                MatchGenerator::common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
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
