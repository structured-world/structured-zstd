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
use super::match_table::storage::check_stream_abs_headroom;
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
        check_stream_abs_headroom(self.history_abs_start, self.window_size, data.len());
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
        // Loop invariants:
        //
        // 1. Block-local arithmetic (`pos`, `skip_step`, `start +
        //    candidate.match_len`, `DFAST_SKIP_STEP_GROWTH_INTERVAL`):
        //    the dynamic bound is `current_len` itself — the `while
        //    pos + DFAST_MIN_MATCH_LEN <= current_len` guard keeps
        //    every offset within that bound regardless of how the
        //    caller sized `max_window_size`. In production this also
        //    happens to be `≤ HC_BLOCKSIZE_MAX (128 KiB)` because the
        //    frame compressor never hands out larger blocks, but the
        //    safety argument above does not rely on that limit.
        // 2. Absolute-position arithmetic (`current_abs_start + pos`):
        //    `current_abs_start` is the frame-lifetime cursor and
        //    advances with total bytes processed, NOT with the
        //    retained window size. A long streaming encode on i686
        //    can therefore push `current_abs_start + pos` past
        //    `usize::MAX` even though memory usage stays bounded by
        //    `window_size`. The runtime enforcement lives in
        //    `DfastMatchGenerator::add_data`, which routes through
        //    `check_stream_abs_headroom` (the same gate used by
        //    `MatchTable::add_data`): every ingest fails fast with a
        //    clear panic if cumulative input would push the cursor
        //    within `STREAM_ABS_HEADROOM` (`= HC_OPT_NUM + 16`) of
        //    `usize::MAX`. Raw `+` here is donor parity and is
        //    correct precisely because that upstream gate runs before
        //    this loop sees any new bytes.
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
                // Donor's opportunistic rep-0 extension after every emit.
                pos = self.extend_with_repcode_after_match(
                    current_abs_start,
                    current_len,
                    pos,
                    &mut literals_start,
                    handle_sequence,
                );
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
        // Loop invariants (two distinct bounds):
        //
        // 1. Block-local arithmetic (`ip0..ip3 = pos + N` for small
        //    `N`, `pos + skip_step` with `skip_step <=
        //    DFAST_MAX_SKIP_STEP`): the dynamic bound is `current_len`
        //    itself — the `while pos + DFAST_MIN_MATCH_LEN <=
        //    current_len` guard keeps every offset within that bound
        //    regardless of how the caller sized `max_window_size`.
        //    In production this also happens to be `≤
        //    HC_BLOCKSIZE_MAX (128 KiB)` because the frame compressor
        //    never hands out larger blocks, but the safety argument
        //    above does not rely on that limit.
        // 2. Absolute-position arithmetic (`current_abs_start + ip0`,
        //    `current_abs_start + ip2`, etc.): `current_abs_start` is
        //    the frame-lifetime cursor and advances with total bytes
        //    processed, NOT with the retained window size. A long
        //    streaming encode on i686 can push `current_abs_start +
        //    ipN` past `usize::MAX` even though memory stays bounded
        //    by `window_size`. The runtime enforcement lives in
        //    `DfastMatchGenerator::add_data` via
        //    `check_stream_abs_headroom`: every ingest fails fast if
        //    cumulative input would advance the cursor within
        //    `STREAM_ABS_HEADROOM` of `usize::MAX`, which keeps the
        //    raw `current_abs_start + ipN` arithmetic below
        //    `usize::MAX` for every position
        //    this loop ever observes.
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
                    pos = self.extend_with_repcode_after_match(
                        current_abs_start,
                        current_len,
                        pos,
                        &mut literals_start,
                        handle_sequence,
                    );
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
                pos = self.extend_with_repcode_after_match(
                    current_abs_start,
                    current_len,
                    pos,
                    &mut literals_start,
                    handle_sequence,
                );
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

    /// Donor `zstd_double_fast.c` post-match rep-0 extension. After the
    /// primary match has been emitted and `pos` advanced past it, donor
    /// opportunistically chains additional `rep_2`-coded matches at the
    /// new cursor as long as 4 bytes at `ip` keep matching the bytes at
    /// `ip - offset_2` (in donor naming; in Rust offset terms this is
    /// `offset_hist[1]` once `lit_len == 0` after the just-emitted
    /// primary). Each iteration:
    ///
    ///   * emits one zero-literal sequence with the old `offset_hist[1]`,
    ///   * swaps `offset_hist[0]` ↔ `offset_hist[1]` via
    ///     [`encode_offset_with_history`] (the donor `offset_2 = offset_1;
    ///     offset_1 = old_offset_2;` swap),
    ///   * skips the hash-table probe entirely on every extra match.
    ///
    /// Critically uses donor's `MINMATCH = 4` here rather than the
    /// stricter `DFAST_MIN_MATCH_LEN = 6` enforced on the main search
    /// loop. The donor accepts any 4-byte rep extension; we mirror that
    /// because the rep emission carries no offset cost — even a 4-byte
    /// rep is a net win over re-running the full hash search. Returns
    /// the new value of `pos` and updates `literals_start` in place to
    /// the post-rep-chain anchor.
    fn extend_with_repcode_after_match(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        mut pos: usize,
        literals_start: &mut usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) -> usize {
        const DONOR_REP_MIN_MATCH_LEN: usize = 4;
        loop {
            // Need at least DONOR_REP_MIN_MATCH_LEN bytes of room past `pos`.
            if pos + DONOR_REP_MIN_MATCH_LEN > current_len {
                break;
            }
            // After a primary emit `literals_start == pos`, so `lit_len`
            // on the next sequence is zero — donor's rep probe uses
            // `offset_2` (== `offset_hist[1]` under our encoding).
            let rep = self.offset_hist[1] as usize;
            if rep == 0 {
                break;
            }
            let abs_pos = current_abs_start + pos;
            let cur_idx = abs_pos - self.history_abs_start;
            // `checked_sub` is the authoritative bound here: a valid rep
            // can reach beyond the current block into retained history
            // (the contiguous `live_history()` buffer covers
            // `history_abs_start..history_abs_end`), so the only hard
            // constraint is `cur_idx >= rep` (i.e. the candidate is in
            // the addressable history range). A previous draft also
            // gated on `rep > pos`, which over-rejected valid offsets
            // that point into retained history near block boundaries —
            // exactly the donor-style chain win this helper is meant to
            // recover.
            let cand_idx = match cur_idx.checked_sub(rep) {
                Some(idx) => idx,
                None => break,
            };
            let concat = &self.history[self.history_start..];
            if cur_idx + DONOR_REP_MIN_MATCH_LEN > concat.len() {
                break;
            }
            // Cheap 4-byte gate before the SIMD `common_prefix_len`.
            if concat[cur_idx..cur_idx + 4] != concat[cand_idx..cand_idx + 4] {
                break;
            }
            let match_len = common_prefix_len(&concat[cand_idx..], &concat[cur_idx..]);
            if match_len < DONOR_REP_MIN_MATCH_LEN {
                break;
            }
            // Insert the rep range into hash tables so future positions
            // hashing into this area find these candidates.
            self.insert_positions(abs_pos, abs_pos + match_len);
            // Emit zero-literal rep sequence.
            handle_sequence(Sequence::Triple {
                literals: &[],
                offset: rep,
                match_len,
            });
            let _ = encode_offset_with_history(rep as u32, 0, &mut self.offset_hist);
            pos += match_len;
            *literals_start = pos;
        }
        pos
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
        // The raw `pos += step` below is correct only while `step` is
        // bounded by `DFAST_INCOMPRESSIBLE_SKIP_STEP` (the only value
        // any in-tree caller passes here). Asserting it locally keeps
        // a future caller from quietly reintroducing the overflow risk
        // that the upstream `check_stream_abs_headroom` gate is sized
        // for.
        assert!(
            step <= DFAST_INCOMPRESSIBLE_SKIP_STEP,
            "insert_positions_with_step: step ({step}) exceeds \
             DFAST_INCOMPRESSIBLE_SKIP_STEP — raw `pos += step` would \
             eat into the STREAM_ABS_HEADROOM reserve"
        );
        let start = start.max(self.history_abs_start);
        let end = end.min(self.history_abs_end());
        if step <= 1 {
            self.insert_positions(start, end);
            return;
        }
        let mut pos = start;
        while pos < end {
            self.insert_position(pos);
            // `pos + step` is safe: `pos < end <= history_abs_end()` and
            // `history_abs_end <= usize::MAX - STREAM_ABS_HEADROOM` by
            // the upstream `check_stream_abs_headroom` gate, while
            // `step` is bounded above by the assertion at function
            // entry.
            pos += step;
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

#[cfg(test)]
mod extend_with_repcode_tests {
    //! Targeted regression coverage for `extend_with_repcode_after_match`.
    //!
    //! These tests intentionally bypass the higher-level
    //! `compress_to_vec` roundtrip path used by `cross_validation` so
    //! that a failure pinpoints the post-match rep helper rather than
    //! firing somewhere downstream (block writer / huff0 / FSE / decode).
    //! The capture closure records the exact sequence stream the matcher
    //! emits, which is what the assertions check.
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    /// Capture every sequence the matcher emits into an owned record,
    /// so the assertions can match on `lit_len` / `offset` / `match_len`
    /// shape directly. `Sequence::Triple` carries borrowed literals; we
    /// take their length and discard the bytes (the test only cares
    /// about the structural shape, not the literal content).
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CapturedSeq {
        Triple {
            lit_len: usize,
            offset: usize,
            match_len: usize,
        },
        Literals {
            lit_len: usize,
        },
    }

    fn record_seq<'a>(out: &'a mut Vec<CapturedSeq>) -> impl FnMut(Sequence<'_>) + 'a {
        move |seq| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => out.push(CapturedSeq::Triple {
                lit_len: literals.len(),
                offset,
                match_len,
            }),
            Sequence::Literals { literals } => out.push(CapturedSeq::Literals {
                lit_len: literals.len(),
            }),
        }
    }

    fn build_dfast_with(data: &[u8]) -> DfastMatchGenerator {
        // Window sized to the block so the matcher does not start
        // trimming history mid-test.
        let mut dfast = DfastMatchGenerator::new(data.len().next_power_of_two().max(64));
        dfast.use_fast_loop = false; // exercise `start_matching_general`
        dfast.ensure_hash_tables();
        dfast.add_data(data.to_vec(), |_| {});
        dfast
    }

    /// Direct call into [`DfastMatchGenerator::extend_with_repcode_after_match`]
    /// with a hand-built post-primary-match state. Going through
    /// `start_matching` is unreliable for this assertion because the
    /// primary `best_match` greedily consumes a constant run in a
    /// single `Triple` (offset 1, match_len = block - 1), leaving the
    /// helper nothing to extend. Instead we set up the state the
    /// helper expects after a primary emit and verify it chains
    /// rep-0 sequences for as many bytes as the rep predicate
    /// matches.
    #[test]
    fn dfast_repcode_extension_emits_zero_literal_rep_on_constant_run() {
        let data: Vec<u8> = vec![b'A'; 64];
        let mut dfast = build_dfast_with(&data);

        // Post-primary-match state: pretend a previous sequence emitted
        // with offset = 4 (`offset_hist[0]`). Under the donor swap the
        // post-match rep probe consults `offset_hist[1]`, here set to
        // 1 so every subsequent byte (constant 'A') matches its
        // predecessor.
        dfast.offset_hist = [4, 1, 8];
        let current_abs_start = dfast.history_abs_start + dfast.window_size - data.len();
        let current_len = data.len();
        // Start the helper mid-block; the leading bytes are the
        // "literals + match" the (simulated) primary would have
        // covered. `literals_start == pos` is the post-emit invariant
        // — `lit_len` for the next sequence is zero.
        let pos = 10usize;
        let mut literals_start = pos;

        let mut seqs = Vec::new();
        let new_pos = {
            let mut rec = record_seq(&mut seqs);
            dfast.extend_with_repcode_after_match(
                current_abs_start,
                current_len,
                pos,
                &mut literals_start,
                &mut rec,
            )
        };

        assert!(
            new_pos > pos,
            "helper must advance pos past at least one rep match \
             (pos={pos}, new_pos={new_pos})"
        );
        assert_eq!(
            literals_start, new_pos,
            "helper must keep literals_start == new_pos so the caller's main \
             loop sees zero pending literals after the rep chain"
        );
        assert!(!seqs.is_empty(), "helper must emit at least one Triple");
        for seq in &seqs {
            match seq {
                CapturedSeq::Triple {
                    lit_len,
                    offset,
                    match_len: _,
                } => {
                    assert_eq!(
                        *lit_len, 0,
                        "rep emission must be zero-literal (got {seq:?})"
                    );
                    assert_eq!(
                        *offset, 1,
                        "rep emission must use the swapped-in offset_hist[1] = 1 \
                         (got {seq:?})"
                    );
                }
                CapturedSeq::Literals { .. } => {
                    panic!("rep extension must not emit a Literals tail: {seq:?}");
                }
            }
        }
    }

    /// Cross-block / retained-history case: probe with `offset > pos`
    /// (where `pos` is block-local) so the candidate lives in retained
    /// history from a previously committed block. The
    /// CodeRabbit-flagged `rep > pos` guard would have rejected
    /// exactly this path — the current implementation only gates on
    /// `cur_idx.checked_sub(rep)` so the helper accepts the cross-
    /// block offset and emits the rep sequence.
    #[test]
    fn dfast_repcode_extension_walks_into_retained_history() {
        let block_a: Vec<u8> = vec![b'C'; 64];
        let block_b: Vec<u8> = vec![b'C'; 32];
        let mut dfast = DfastMatchGenerator::new(256);
        dfast.use_fast_loop = false;
        dfast.ensure_hash_tables();
        dfast.add_data(block_a, |_| {});
        dfast.add_data(block_b.clone(), |_| {});

        // Post-primary-match state targeting cross-block rep: probe
        // offset = 40 (a candidate inside block A bytes), block-local
        // cursor = 5 (so `rep > pos` under the rejected guard).
        dfast.offset_hist = [4, 40, 8];
        let current_len = block_b.len();
        let current_abs_start = dfast.history_abs_start + dfast.window_size - current_len;
        let pos = 5usize;
        let mut literals_start = pos;

        let mut seqs = Vec::new();
        let new_pos = {
            let mut rec = record_seq(&mut seqs);
            dfast.extend_with_repcode_after_match(
                current_abs_start,
                current_len,
                pos,
                &mut literals_start,
                &mut rec,
            )
        };

        assert!(
            new_pos > pos,
            "rep with offset > block-local pos must still emit a match when the \
             candidate lives in retained history (pos={pos}, new_pos={new_pos})"
        );
        assert_eq!(seqs.len(), 1, "expected one rep emit, got {seqs:?}");
        match &seqs[0] {
            CapturedSeq::Triple {
                lit_len,
                offset,
                match_len: _,
            } => {
                assert_eq!(*lit_len, 0, "rep emit must be zero-literal");
                assert_eq!(*offset, 40, "rep emit must use the cross-block offset 40");
            }
            other => panic!("expected Triple, got {other:?}"),
        }
    }
}
