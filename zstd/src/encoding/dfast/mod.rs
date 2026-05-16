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
    DFAST_MAX_SKIP_STEP, DFAST_MIN_MATCH_LEN, DFAST_REBASE_GUARD_BAND, DFAST_SHORT_HASH_BITS_DELTA,
    DFAST_SHORT_HASH_LOOKAHEAD, DFAST_SKIP_STEP_GROWTH_INTERVAL, DFAST_TARGET_LEN, MIN_WINDOW_LOG,
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
    // Storage: single `u32` per bucket — donor-parity overwrite-on-
    // collision. Each slot holds a +1-biased relative position
    // (`(abs_pos - position_base + 1) as u32`); `DFAST_EMPTY_SLOT = 0`
    // is therefore never a real value. The two tables are sized
    // independently: `long_hash` (8-byte hash) uses `long_hash_bits`
    // (donor `hashTable`); `short_hash` (4-byte hash) uses
    // `short_hash_bits` = `long - 1` (donor `chainTable` for dfast).
    // Donor parity at Level 3: `2^17 × 4 + 2^16 × 4 = 768 KiB`. The
    // ratio loss from single-slot is compensated by donor's
    // `_search_next_long` retry — after a short-hash hit, the search
    // probes long_hash at `ip + 1` and picks the longer of the two
    // (see `find_best_match` for the retry logic).
    pub(crate) short_hash: Vec<u32>,
    pub(crate) long_hash: Vec<u32>,
    /// Absolute position whose `(abs_pos - position_base + 1)` slot
    /// encoding evaluates to `1`. Advances only via [`Self::reduce`]
    /// when an insert is about to overflow the u32 window — the
    /// frame-level `STREAM_ABS_HEADROOM` gate already bounds
    /// `history_abs_start` against `usize::MAX`, so a rebase trigger
    /// here only fires on encoder sessions that span more than
    /// `u32::MAX - DFAST_REBASE_GUARD_BAND ≈ 3 GiB` of input through
    /// a single matcher instance. Donor parity: `ZSTD_window_reduce`
    /// (`zstd_compress_internal.h`).
    pub(crate) position_base: usize,
    /// Long-hash table bit-width — `long_hash.len() == 1 <<
    /// long_hash_bits`. Donor parity with `cParams.hashLog` (17 for
    /// Level 3 large input, 16 for Level 2; see `clevels.h`).
    pub(crate) long_hash_bits: usize,
    /// Short-hash table bit-width — `short_hash.len() == 1 <<
    /// short_hash_bits`. Default is `long_hash_bits -
    /// DFAST_SHORT_HASH_BITS_DELTA`, donor parity with
    /// `cParams.chainLog` for dfast levels (one bit smaller than the
    /// long hash). Halves the short-table footprint without losing
    /// measurable ratio — the 4-byte short hash overwrites less
    /// frequently than the 8-byte long hash on average, so the
    /// smaller bucket count is the donor-correct sizing.
    pub(crate) short_hash_bits: usize,
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
            position_base: 0,
            long_hash_bits: DFAST_HASH_BITS,
            short_hash_bits: DFAST_HASH_BITS - DFAST_SHORT_HASH_BITS_DELTA,
            hash_kernel: crate::encoding::fastpath::select_kernel(),
            use_fast_loop: false,
            lazy_depth: 1,
        }
    }

    /// Set both hash table sizes. `bits` is the long-hash bit count
    /// (donor `cParams.hashLog`); the short hash is derived as
    /// `bits - DFAST_SHORT_HASH_BITS_DELTA`, donor-correct for dfast
    /// levels. Both clamps stay above `MIN_WINDOW_LOG` so very small
    /// windows don't underflow.
    pub(crate) fn set_hash_bits(&mut self, bits: usize) {
        let min_bits = MIN_WINDOW_LOG as usize;
        let long_clamped = bits.clamp(min_bits, DFAST_HASH_BITS);
        let short_clamped = long_clamped
            .saturating_sub(DFAST_SHORT_HASH_BITS_DELTA)
            .max(min_bits);
        if self.long_hash_bits != long_clamped {
            self.long_hash_bits = long_clamped;
            self.long_hash = Vec::new();
        }
        if self.short_hash_bits != short_clamped {
            self.short_hash_bits = short_clamped;
            self.short_hash = Vec::new();
        }
    }

    /// Encode an absolute position into a u32 slot value
    /// (`(abs_pos - position_base + 1) as u32`). Caller must have
    /// invoked [`Self::ensure_room_for`] earlier in the same frame so
    /// the relative offset is guaranteed to fit in `u32`.
    ///
    /// # Panics
    ///
    /// Panics if `abs_pos < position_base` (producer bug — a position
    /// before the current rebase base should have been filtered out
    /// before reaching the table) or if the relative offset exceeds
    /// `u32::MAX`. Runtime `assert!` rather than `debug_assert!`: a
    /// silent wrap would store a garbage relative offset and corrupt
    /// the bucket far from the bug's source.
    #[inline]
    pub(crate) fn pack_slot(&self, abs_pos: usize) -> u32 {
        let rel = abs_pos.checked_sub(self.position_base).unwrap_or_else(|| {
            panic!(
                "DfastMatchGenerator::pack_slot: abs_pos {abs_pos} below \
                 position_base {} — caller must filter pre-rebase positions",
                self.position_base
            )
        });
        assert!(
            rel < u32::MAX as usize,
            "DfastMatchGenerator::pack_slot: rel {rel} >= u32::MAX — \
             caller must invoke ensure_room_for before insert"
        );
        (rel as u32) + 1
    }

    /// Ensure that an absolute position `abs_pos` fits in the `u32`
    /// slot encoding when packed. If the relative offset would
    /// exceed `u32::MAX - DFAST_REBASE_GUARD_BAND`, advance the base
    /// by `DFAST_REBASE_GUARD_BAND` (in a loop, in case the caller
    /// jumped past multiple guard bands at once) and shift every
    /// stored slot down by the same amount. Mirrors
    /// `LdmHashTable::ensure_room_for` and the donor's
    /// `ZSTD_window_reduce` semantics.
    pub(crate) fn ensure_room_for(&mut self, abs_pos: usize) {
        if abs_pos < self.position_base {
            // Pre-base positions can't push us past the u32 ceiling.
            return;
        }
        let max_rel = u32::MAX as usize - DFAST_REBASE_GUARD_BAND as usize;
        while abs_pos - self.position_base > max_rel {
            self.reduce(DFAST_REBASE_GUARD_BAND);
        }
    }

    /// Subtract `reducer` from every stored slot value. Slots whose
    /// pre-shift value was `<= reducer` become the empty sentinel.
    /// Advance `position_base` by the same amount so future inserts
    /// continue from the rebased origin.
    fn reduce(&mut self, reducer: u32) {
        let shift_slots = |slots: &mut [u32]| {
            for slot in slots.iter_mut() {
                *slot = if *slot <= reducer {
                    DFAST_EMPTY_SLOT
                } else {
                    *slot - reducer
                };
            }
        };
        shift_slots(&mut self.short_hash);
        shift_slots(&mut self.long_hash);
        self.position_base += reducer as usize;
    }

    pub(crate) fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.position_base = 0;
        self.offset_hist = [1, 4, 8];
        if !self.short_hash.is_empty() {
            self.short_hash.fill(DFAST_EMPTY_SLOT);
            self.long_hash.fill(DFAST_EMPTY_SLOT);
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
                // Single-slot donor parity: donor inserts ONLY at the
                // current ip per iteration of its inner do-while loop
                // (`zstd_double_fast.c:187`). The earlier
                // ip0/ip1/ip2/ip3 fan-out only made sense with the
                // 4-slot bucket — under single-slot storage four inserts
                // per miss just overwrite the same buckets and discard
                // previously-stored positions before the producer can
                // walk past them, which costs ~30% compression ratio
                // on dfast-level fixtures.
                self.insert_position(abs_ip0);
                let _ = (ip1, ip2, ip3);
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
            // Sparse complementary insertion (donor parity,
            // `zstd_double_fast.c:300-304`): donor inserts ONLY at
            // `curr+2`, `ip-2`, `ip-1` after a match — three specific
            // positions, not the whole match range. The previous
            // `insert_positions(abs_pos, abs_pos + match_len)` made
            // sense only under the 4-slot bucket; with single-slot
            // donor parity it would just overwrite every bucket along
            // the match span and discard whichever positions the
            // producer was about to re-probe.
            let post_match_end = abs_pos + match_len;
            let insert_targets = [
                abs_pos + 2,                      // curr + 2
                post_match_end.saturating_sub(2), // ip - 2 (post-match cursor)
                post_match_end.saturating_sub(1), // ip - 1
            ];
            for &target in &insert_targets {
                if target > abs_pos && target < post_match_end {
                    self.insert_position(target);
                }
            }
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
        // Independent sizing per donor `clevels.h`: long-hash =
        // `hashLog`, short-hash = `chainLog`. Lazy allocation so
        // Fastest/Uncompressed never pay the dfast-level memory cost.
        let long_len = 1usize << self.long_hash_bits;
        let short_len = 1usize << self.short_hash_bits;
        if self.long_hash.len() != long_len {
            self.long_hash = alloc::vec![DFAST_EMPTY_SLOT; long_len];
        }
        if self.short_hash.len() != short_len {
            self.short_hash = alloc::vec![DFAST_EMPTY_SLOT; short_len];
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
        // Hoist the rebase base out of the bucket-walk loop so each
        // slot-to-absolute conversion is a single add instead of a
        // `&self` dereference per iteration. The base only changes
        // via `reduce`, which is called between match-finding calls.
        let position_base = self.position_base;
        let mut best = None;

        // Long-hash probe first (8-byte hash → longer matches more
        // likely). Single-slot per bucket — donor parity, no chain
        // walking. The retry policy below mirrors donor
        // `_search_next_long`: if the long-hash misses but the
        // short-hash hits, peek the long-hash at `abs_pos + 1` and
        // pick the longer of the two matches.
        let long_hit = if current_idx + 8 <= concat.len() {
            let long_hash = self.long_hash_index(&concat[current_idx..]);
            // SAFETY: `long_hash_index` masks to `long_hash_bits` and
            // `long_hash.len() == 1 << long_hash_bits` (`ensure_hash_tables`).
            debug_assert!(long_hash < self.long_hash.len());
            let slot = unsafe { *self.long_hash.get_unchecked(long_hash) };
            self.probe_slot_match(
                slot,
                position_base,
                history_abs_start,
                abs_pos,
                current_idx,
                concat,
                lit_len,
            )
        } else {
            None
        };
        if let Some(cand) = long_hit {
            best = best_len_offset_candidate(best, Some(cand));
            if best.is_some_and(|b| b.match_len >= DFAST_TARGET_LEN) {
                return best;
            }
        }

        if current_idx + 4 <= concat.len() {
            let short_hash = self.short_hash_index(&concat[current_idx..]);
            debug_assert!(short_hash < self.short_hash.len());
            let slot = unsafe { *self.short_hash.get_unchecked(short_hash) };
            if let Some(short_cand) = self.probe_slot_match(
                slot,
                position_base,
                history_abs_start,
                abs_pos,
                current_idx,
                concat,
                lit_len,
            ) {
                best = best_len_offset_candidate(best, Some(short_cand));
                if best.is_some_and(|b| b.match_len >= DFAST_TARGET_LEN) {
                    return best;
                }
                // Donor `_search_next_long` retry: short hit landed but
                // a long hit at `abs_pos + 1` could be even longer. The
                // donor inner loop precomputes `hashLong[hl1]` for
                // exactly this case (line 213 in `zstd_double_fast.c`);
                // we lift it inline here so the single-slot table
                // retains the compression-quality donor gets from its
                // overlapping probe pattern.
                let next_idx = current_idx + 1;
                if best.is_none_or(|b| b.match_len < DFAST_TARGET_LEN)
                    && next_idx + 8 <= concat.len()
                {
                    let next_long_hash = self.long_hash_index(&concat[next_idx..]);
                    debug_assert!(next_long_hash < self.long_hash.len());
                    let next_slot = unsafe { *self.long_hash.get_unchecked(next_long_hash) };
                    if let Some(retry) = self.probe_slot_match(
                        next_slot,
                        position_base,
                        history_abs_start,
                        abs_pos + 1,
                        next_idx,
                        concat,
                        lit_len.saturating_add(1),
                    ) && retry.match_len > short_cand.match_len
                    {
                        best = best_len_offset_candidate(best, Some(retry));
                    }
                }
            }
        }
        best
    }

    /// Resolve a single packed-slot value against the live history and
    /// return a backward-extended `MatchCandidate` if the bucket holds
    /// a valid in-range position whose forward extension reaches at
    /// least `DFAST_MIN_MATCH_LEN` bytes. Shared between the long-hash
    /// primary probe, the short-hash primary probe, and the
    /// `_search_next_long` retry — keeps the bounds-checking logic in
    /// one place so the three call sites can't drift.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn probe_slot_match(
        &self,
        slot: u32,
        position_base: usize,
        history_abs_start: usize,
        abs_pos: usize,
        current_idx: usize,
        concat: &[u8],
        lit_len: usize,
    ) -> Option<MatchCandidate> {
        if slot == DFAST_EMPTY_SLOT {
            return None;
        }
        let candidate_pos = position_base + (slot as usize) - 1;
        if candidate_pos < history_abs_start || candidate_pos >= abs_pos {
            return None;
        }
        let candidate_idx = candidate_pos - history_abs_start;
        // Cheap mismatch gate before the SIMD walk: if the first byte
        // doesn't match there's no way `common_prefix_len` reaches the
        // 6-byte minimum.
        if concat[candidate_idx] != concat[current_idx] {
            return None;
        }
        let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
        if match_len < DFAST_MIN_MATCH_LEN {
            return None;
        }
        Some(self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len))
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
        // Pre-rebase guard. The producer that walks `insert_positions*`
        // can sweep an arbitrary number of positions per block; running
        // `pack_slot` per-position would call `ensure_room_for` from a
        // tight inner loop. Hoisting the rebase trigger to the start of
        // `insert_position` keeps the per-byte hot path branch-free
        // when the relative window has plenty of headroom (the common
        // case) while still guaranteeing the slot value below fits in
        // `u32`. `ensure_room_for` is a single u32 comparison when no
        // rebase is needed.
        self.ensure_room_for(pos);
        let packed = self.pack_slot(pos);
        // SAFETY: the `*_hash_index` helpers mask the mixed hash to
        // `long_hash_bits` / `short_hash_bits`, and `ensure_hash_tables`
        // sizes the two tables to `1 << long_hash_bits` /
        // `1 << short_hash_bits` respectively, so every produced index
        // is provably below the table length. Eliding the bounds check
        // on this per-byte hot path saves ~4 instructions per call.
        //
        // Single-slot overwrite (donor parity): the previous 4-slot
        // bucket shift (`copy_within(..)`) is gone — donor
        // `ZSTD_compressBlock_doubleFast_*` writes a single `U32` per
        // hash position and relies on the dense `_search_next_long`
        // retry in `find_best_match` to preserve compression ratio.
        if idx + 4 <= concat_len {
            let concat = &self.history[self.history_start..];
            let short = self.short_hash_index(&concat[idx..]);
            debug_assert!(short < self.short_hash.len());
            unsafe { *self.short_hash.get_unchecked_mut(short) = packed };
        }

        if idx + 8 <= concat_len {
            let concat = &self.history[self.history_start..];
            let long = self.long_hash_index(&concat[idx..]);
            debug_assert!(long < self.long_hash.len());
            unsafe { *self.long_hash.get_unchecked_mut(long) = packed };
        }
    }

    pub(crate) fn short_hash_index(&self, data: &[u8]) -> usize {
        let value = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        self.hash_index_with_bits(value, self.short_hash_bits)
    }

    pub(crate) fn long_hash_index(&self, data: &[u8]) -> usize {
        let value = u64::from_le_bytes(data[..8].try_into().unwrap());
        self.hash_index_with_bits(value, self.long_hash_bits)
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

    fn hash_index_with_bits(&self, value: u64, bits: usize) -> usize {
        let mixed = crate::encoding::fastpath::hash_mix_u64_with_kernel(self.hash_kernel, value);
        (mixed >> (64 - bits)) as usize
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

    /// The helper accepts 4-byte rep extensions (donor `MINMATCH = 4`),
    /// not the main-loop `DFAST_MIN_MATCH_LEN = 6` floor. A regression
    /// back to 6 would still pass the constant-run / cross-block tests
    /// above (their rep matches extend much further), so this fixture
    /// is built so the rep matches EXACTLY 4 bytes before terminating:
    /// the byte at `pos + 4` differs from the byte at `pos + 4 - rep`.
    ///
    /// Fixture (32 bytes, indices `0..=31`):
    ///   `"ABCD????ABCD!??????????ABCDX????"`
    ///    01234567890123456789012345678901     (ones digit)
    ///              1111111111222222222233     (tens digit, aligned)
    ///
    /// Probe state:
    ///   * `offset_hist[1] = 8` → rep probe reads `concat[pos - 8..]`.
    ///   * `pos = 8` → `concat[8..12] = "ABCD"`, `concat[0..4] = "ABCD"`
    ///     → 4 bytes match.
    ///   * `concat[12] = '!'` vs `concat[4] = '?'` → 5th byte mismatch,
    ///     so the rep extension stops at exactly 4 bytes.
    #[test]
    fn dfast_repcode_extension_accepts_exactly_four_byte_rep() {
        // Block: "ABCD????" (8) + "ABCD!" (5) + "??????????" (10) +
        //        "ABCDX" (5) + "????" (4) = 32 bytes total. The
        //        important invariants are `concat[0..4] == "ABCD"`,
        //        `concat[8..12] == "ABCD"`, and `concat[12] = '!'`
        //        (so byte 12 ≠ byte 4 = '?', stopping the rep at
        //        length 4). The trailing bytes are irrelevant — we
        //        only iterate the helper at `pos = 8` and the rep
        //        chain terminates after one 4-byte emit because the
        //        next rep probe (post-swap) would need bytes at
        //        `pos + 4` to match a different offset.
        let data: Vec<u8> = b"ABCD????ABCD!??????????ABCDX????".to_vec();
        assert_eq!(data.len(), 32, "fixture invariant: 32 bytes");
        let mut dfast = DfastMatchGenerator::new(64);
        dfast.use_fast_loop = false;
        dfast.ensure_hash_tables();
        dfast.add_data(data.clone(), |_| {});

        dfast.offset_hist = [12, 8, 4];
        let current_abs_start = dfast.history_abs_start + dfast.window_size - data.len();
        let current_len = data.len();
        let pos = 8usize;
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

        // Helper must emit a single 4-byte rep, then stop because
        // the 5th byte mismatches.
        assert_eq!(seqs.len(), 1, "expected one 4-byte rep emit, got {seqs:?}");
        match &seqs[0] {
            CapturedSeq::Triple {
                lit_len,
                offset,
                match_len,
            } => {
                assert_eq!(*lit_len, 0, "rep emit must be zero-literal");
                assert_eq!(*offset, 8, "rep emit must use offset 8 (offset_hist[1])");
                assert_eq!(
                    *match_len, 4,
                    "rep emit must be exactly 4 bytes (donor MINMATCH floor). \
                     A regression back to DFAST_MIN_MATCH_LEN = 6 would skip \
                     this emission entirely and the test would fail with 0 seqs."
                );
            }
            other => panic!("expected Triple, got {other:?}"),
        }
        assert_eq!(new_pos, pos + 4, "pos must advance by exactly 4");
        assert_eq!(literals_start, new_pos, "literals_start must follow pos");
    }

    /// Integration coverage for the **fast-loop** call sites of
    /// `extend_with_repcode_after_match` inside
    /// `start_matching_fast_loop`. The direct-call tests above pin
    /// down the helper's contract; this test drives the full fast
    /// loop end-to-end through the production `compress_to_vec`
    /// pipeline on a fixture engineered to exercise the post-match
    /// rep chain on the fast-loop path.
    ///
    /// `CompressionLevel::Default` is the production config that
    /// enables `use_fast_loop = true` (see `Matcher::reset` in
    /// `match_generator.rs`). The fixture alternates 60-byte runs of
    /// `'A'` with single `'B'` break bytes and a short `'A'` tail
    /// per cycle — the breaks terminate the fast loop's primary match
    /// early, so subsequent iterations have runway for the helper to
    /// chain additional reps. A regression that broke either fast-
    /// loop helper call site surfaces as a roundtrip failure (decoded
    /// != input) or a ratio explosion. Constructing
    /// `DfastMatchGenerator` directly and asserting on captured
    /// sequences was attempted but the fixture engineering is
    /// brittle: the fast loop's primary match on simple constant
    /// fixtures consumes the entire remaining block in a single
    /// Triple, leaving no bytes for the helper to extend. The
    /// high-level roundtrip sidesteps that fragility while still
    /// routing through the same call site via the production driver.
    ///
    /// Gated on `feature = "std"`: the `Read::read_to_end` method
    /// used to drain `StreamingDecoder` resolves to `std::io::Read`
    /// only when std is enabled. Under no-std `StreamingDecoder`
    /// implements the crate's `io_nostd::Read` alias instead, and
    /// the call site has to be rewritten through that trait. The
    /// fast-loop helper itself is exercised under both
    /// configurations by the direct-call tests above plus the
    /// `cross_validation` Default-level roundtrip — gating this one
    /// integration test on std loses no coverage, only saves the
    /// dual-trait rewrite.
    #[cfg(feature = "std")]
    #[test]
    fn dfast_default_level_roundtrip_with_repetitive_breaks_exercises_fast_loop() {
        // ~4 KiB of input: 64 cycles of [60 'A's, 1 'B', 3 'A's].
        let mut data: Vec<u8> = Vec::with_capacity(64 * 64);
        for _ in 0..64 {
            data.extend_from_slice(&[b'A'; 60]);
            data.push(b'B');
            data.extend_from_slice(&[b'A'; 3]);
        }
        assert!(
            data.len() > 4000,
            "fixture invariant: long enough for fast loop"
        );

        let compressed = crate::encoding::compress_to_vec(
            data.as_slice(),
            crate::encoding::CompressionLevel::Default,
        );

        // Decompress and assert byte-for-byte parity. A regression
        // that broke the fast-loop helper call would either produce
        // invalid frames (decode error) or wrong bytes (mismatch).
        let mut decoder = crate::decoding::StreamingDecoder::new(compressed.as_slice())
            .expect("default-level frame must decode");
        let mut decoded = Vec::with_capacity(data.len());
        // Under `feature = "std"` (the gate above) `StreamingDecoder`
        // implements `std::io::Read`, so `Read::read_to_end` resolves
        // through the standard library's blanket implementation.
        std::io::Read::read_to_end(&mut decoder, &mut decoded)
            .expect("fast-loop output must round-trip cleanly");
        assert_eq!(
            decoded, data,
            "Default-level (use_fast_loop = true) roundtrip must be \
             byte-for-byte exact on the repetitive-breaks fixture"
        );

        // Ratio sanity: the post-match rep helper is what makes
        // repetitive runs compress aggressively on the fast-loop
        // path. A regression to a no-op helper would still produce
        // some compression via the primary match, but the ratio
        // would degrade. A 2:1 floor is conservative enough not to
        // flake on small fixture changes while still catching
        // structural failures of the fast loop.
        assert!(
            compressed.len() * 2 < data.len(),
            "fast loop must compress repetitive runs to at least 2:1, \
             got {} → {} bytes",
            data.len(),
            compressed.len()
        );
    }
}
