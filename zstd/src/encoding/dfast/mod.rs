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
use super::incompressible::block_looks_incompressible;
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

/// Donor `HASH_READ_SIZE` (`zstd_compress_internal.h`): the largest probe
/// width any hash / equality check in the dfast hot path reads at once.
/// Loop guards must stop scanning when fewer than `HASH_READ_SIZE` bytes
/// remain ahead of the probe cursor, matching donor `ilimit = iend -
/// HASH_READ_SIZE`. The `DFAST_MIN_MATCH_LEN = 5` floor is the match
/// acceptance threshold, NOT a safe loop bound — using it as the loop
/// guard reads up to 3 bytes past the live history end and is UB on a
/// raw pointer load.
const HASH_READ_SIZE: usize = 8;

/// Rep-extension minimum match length. The upstream zstd 1.5.7
/// reference (`zstd_double_fast.c:191`, rep1 emit
/// `ZSTD_count(ip+1+4, ip+1+4-offset_1, iend) + 4`) accepts 4-byte
/// rep hits even though hash-search matches require 5 bytes
/// (`DFAST_MIN_MATCH_LEN`) — rep coding has no on-wire offset cost,
/// so a 4-byte rep is a net win over re-running the hash probe. Both
/// the fast-loop inline rep1 peek and the post-match
/// `extend_with_repcode_after_match` chain must gate on this floor;
/// using the hash-search floor on either site silently drops 4-byte
/// rep emissions that upstream produces.
const DFAST_REP_MIN_MATCH_LEN: usize = 4;

pub(crate) struct DfastMatchGenerator {
    pub(crate) max_window_size: usize,
    /// Per-block length queue. Previously held the raw input
    /// `VecDeque<Vec<u8>>` for each appended block — that duplicated
    /// every byte in `history`, doubling the input footprint relative
    /// to donor on the hot path. Now stores only the lengths so the
    /// matcher can still pop blocks block-by-block on eviction
    /// (advancing `history_start` by each pop) while the actual byte
    /// storage lives once, in `history`.
    pub(crate) window_blocks: VecDeque<usize>,
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
    // (see `hash_candidate`, invoked via `best_match`, for the retry).
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
            window_blocks: VecDeque::new(),
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

    pub(crate) fn reset(&mut self) {
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
        // No Vec<u8> blocks to recycle: `add_data` returns each input
        // Vec to the caller eagerly via its own `reuse_space`, and the
        // history Vec is owned solely by the matcher. There is nothing
        // for an outer pool helper to do at reset time, so the dfast
        // signature does not take one (HC / Row do because they hold
        // per-block input Vecs internally; the dispatcher in
        // `match_generator.rs` resolves the per-backend shape).
        self.window_blocks.clear();
    }

    /// Slice of bytes from the most recently appended block. Returns
    /// the trailing `last_block_len` bytes of `history`.
    ///
    /// This is the public trait-dispatch entry point used by the
    /// streaming encoder / block compressor / per-level helpers via
    /// the `Matcher::get_last_space` trait. Internal callers inside
    /// this file deliberately use the inline pattern
    /// `let last_len = self.window_blocks.back().copied().unwrap_or(0);
    /// let current = &self.history[self.history.len() - last_len..];`
    /// so they share the same gate shape as the `current_len == 0`
    /// early returns in `skip_matching` / `start_matching`.
    ///
    /// # Panics
    /// Panics if `window_blocks` is empty (no block has been ingested
    /// yet). All trait-level callers reach this only on the
    /// `frame_compressor` post-`add_data` path, where at least one
    /// block has been appended. The panic is a contract-violation
    /// surface for a NEW external caller invoking before `add_data`,
    /// not a runtime data path: it fails loudly rather than silently
    /// returning an empty slice and letting downstream operate on
    /// zero-length data.
    pub(crate) fn get_last_space(&self) -> &[u8] {
        let last_len = *self
            .window_blocks
            .back()
            .expect("get_last_space: window_blocks empty — caller invoked before add_data");
        let start = self.history.len() - last_len;
        &self.history[start..]
    }

    pub(crate) fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        // Run the headroom check first so the safety invariant
        // (`history_abs_start + window_size + len + STREAM_ABS_HEADROOM
        // <= usize::MAX`) is enforced at the function boundary, not
        // hidden behind the empty-chunk short-circuit below. With
        // `data.len() == 0` the check is a cheap no-op on the cumulative
        // state today, but keeping the call here means the invariant
        // doesn't depend on "empty data implies nothing changes"
        // reasoning if a future change ever attaches side effects to
        // the empty path.
        check_stream_abs_headroom(self.history_abs_start, self.window_size, data.len());
        // Empty chunks have nothing to record: pushing a `0` into
        // `window_blocks` would let a streaming caller that flushes
        // empty chunks grow the deque without bound (`window_size`
        // stays unchanged so `trim_to_window` never has cause to
        // evict the zero-length entries). Hand the Vec straight back
        // to the pool and short-circuit.
        //
        // Side effect: this short-circuits BEFORE the eviction `while`
        // loop below. If a caller shrinks `max_window_size` and then
        // calls `add_data(vec![])` hoping to trigger trim, the trim
        // won't fire here. Use `trim_to_window` directly for that
        // case — it's the dedicated path for shedding retained bytes
        // and now actually frees the prefix (via `split_off`) instead
        // of leaving it pinned in the `history` allocation.
        if data.is_empty() {
            reuse_space(data);
            return;
        }
        while self.window_size + data.len() > self.max_window_size {
            let removed_len = self.window_blocks.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.window_blocks.push_back(data.len());
        // Eager Vec recycle: the only purpose of holding the input Vec
        // was to return it to the caller's pool on eviction. Now that
        // `history` owns the bytes, hand the Vec back immediately so
        // the pool grows on first add instead of waiting for window
        // overflow.
        reuse_space(data);
    }

    /// Trim retained blocks until the window fits `max_window_size`.
    ///
    /// Unlike `MatchGenerator::trim_to_window`,
    /// `RowMatchGenerator::trim_to_window`, and
    /// `HcMatchGenerator::trim_to_window`, this backend does NOT take a
    /// `reuse_space` callback because it doesn't retain per-block
    /// `Vec<u8>` storage to recycle (history is the sole byte buffer
    /// and `add_data` returns each input Vec eagerly). The dispatcher
    /// in `match_generator.rs` knows the variant and threads the right
    /// signature; callers needing the eviction byte count derive it
    /// from the `window_size` delta before/after this call.
    ///
    /// The explicit-trim-before-idle path is the reason this helper
    /// exists: a caller that trims to shed memory before a long
    /// quiescent period must see the resident size drop immediately,
    /// not "eventually, on the next ingest".
    ///
    /// `compact_history` is NOT the right tool for that — it uses
    /// `Vec::drain(..history_start)`, which moves elements down in
    /// the existing allocation and leaves capacity untouched (per
    /// `Vec` docs, only `shrink_to_fit` releases capacity). So even
    /// when compact ran, the original buffer stayed alive. Instead,
    /// rebuild `history` via `split_off`: it allocates a fresh
    /// buffer sized to the retained suffix, and the assignment
    /// drops the original (full-capacity) buffer. On a normal block
    /// loop `add_data` will compact again the next iter — the cost
    /// is one extra realloc on the trim boundary in exchange for
    /// actually shedding the prefix to the system allocator.
    /// Trim retained history down to `max_window_size`.
    ///
    /// Signature mirrors `HashChainTable::trim_to_window` /
    /// `RowMatcher::trim_to_window` so the [`Matcher`] backends share
    /// a uniform `trim_to_window(impl FnMut(Vec<u8>))` shape — the
    /// dispatcher in `match_generator.rs` can therefore call every
    /// backend through the same surface even though Dfast has no
    /// per-block `Vec<u8>` to recycle. The closure is invoked zero
    /// times here: Dfast stores its raw bytes in the single contiguous
    /// `history` buffer, releases the dead prefix via `split_off`
    /// (which drops the old full-capacity buffer to the system
    /// allocator), and surfaces eviction byte count to the dispatcher
    /// via the `window_size` delta rather than the callback.
    pub(crate) fn trim_to_window(&mut self, _reuse_space: impl FnMut(Vec<u8>)) {
        while self.window_size > self.max_window_size {
            let removed_len = self.window_blocks.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        if self.history_start != 0 {
            // `split_off` returns the suffix in a fresh allocation;
            // the original Vec (still owning [..history_start]) is
            // dropped on the assignment below, releasing the dead
            // prefix back to the allocator.
            self.history = self.history.split_off(self.history_start);
            self.history_start = 0;
        }
    }

    pub(crate) fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_hash_tables();
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            // `add_data` short-circuits on empty input and does NOT push
            // a zero-length entry onto `window_blocks`. A caller that
            // invokes skip-matching after a streaming flush of an empty
            // chunk would otherwise re-seed the previous block's
            // retained tail on every empty write. Mirror the gate that
            // `start_matching` already uses.
            return;
        }
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
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            // Same gate as `skip_matching` and `start_matching`: empty
            // chunks fed through `add_data` no longer push a block
            // entry, so a streaming caller that flushes empty chunks
            // would otherwise re-seed the retained tail on every
            // empty write.
            return;
        }
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

        let current_len = self.window_blocks.back().copied().unwrap_or(0);
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
        //    caller sized `max_window_size`. The general path's
        //    `best_match` → `hash_candidate` chain uses SAFE slicing
        //    (`data[..8].try_into()`) which panics on a short slice
        //    rather than reading uninitialised bytes — UB-free even
        //    when this guard lets `pos` land within 3 bytes of the
        //    block tail. The fast loop has stricter `+ HASH_READ_SIZE`
        //    guards because it does raw-pointer `read_unaligned` for
        //    speed; see `start_matching_fast_loop`. In production this
        //    also happens to be `≤ HC_BLOCKSIZE_MAX (128 KiB)` because
        //    the frame compressor never hands out larger blocks, but
        //    the safety argument above does not rely on that limit.
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
        // Behaviour change vs the pre-refactor `start_matching_general`:
        // this fast loop deliberately drops the strict-incompressible
        // early-skip path (the `block_looks_incompressible_strict` short
        // circuit + `miss_run` / `DFAST_LOCAL_SKIP_TRIGGER` thresholding).
        // The step ramp is now driven purely by distance traveled
        // (`DFAST_SKIP_STEP_GROWTH_INTERVAL = 64`, donor parity except
        // donor uses 256 — see "Donor-deviation audit" in the PR body),
        // so blocks the strict gate used to bail out of early now scan
        // through the standard ramp. `block_looks_incompressible_strict`
        // is still used by `levels/fastest.rs` for the Fastest preset
        // and by `incompressible.rs` unit tests, so the helper itself
        // stays.
        //
        // Donor outer/inner structure (`zstd_double_fast.c:167-322`):
        //   * outer `while(1)` runs once per match-found-and-stored;
        //   * inner `do { ... } while (ip1 <= ilimit)` carries `hl0`,
        //     `idxl0` between iterations, and precomputes `hl1` so the
        //     next iter's long hash is reused (one long-hash compute per
        //     two positions scanned, not per position).
        // We mirror that here. Per-frame invariants are hoisted before
        // the outer loop; mutable state (`position_base`, `history_abs_start`)
        // is re-snapshotted inside the outer loop because `emit_candidate`
        // → `insert_positions` → `ensure_room_for` can advance the rebase
        // base mid-frame.
        //
        // Rebase BEFORE any inner-loop slot pack. The hot loop computes
        // `packed_curr = (abs_ip0 - position_base) as u32 + 1` and writes
        // it straight into the hash tables, bypassing `insert_position`
        // (which is where `ensure_room_for` normally fires). On a long
        // stream of all-miss / non-hashable blocks the matcher can advance
        // `current_abs_start` arbitrarily far without any per-byte insert,
        // so `position_base` may be stale by `> u32::MAX`. The first
        // fast-loop block after that would silently truncate `packed_curr`
        // and poison both hash tables. The guard band in `ensure_room_for`
        // (`DFAST_REBASE_GUARD_BAND`) covers the entire current block's
        // worth of positions, so a single call at function entry suffices.
        //
        // `current_len > 0` is a precondition of this helper: every
        // caller (`start_matching`, `skip_matching`, `skip_matching_dense`)
        // returns early at `current_len == 0`. Encode it as a hard
        // assert so a future caller that forgets the gate fails loudly
        // in debug rather than wrap-underflowing the `- 1` below; the
        // rebase is unconditional in release builds since the
        // precondition holds.
        debug_assert!(current_len > 0, "fast_loop precondition: current_len > 0");
        self.ensure_room_for(current_abs_start + current_len - 1);
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        let short_shift = 64 - self.short_hash_bits;
        let long_shift = 64 - self.long_hash_bits;
        let mut pos = 1usize;
        let mut literals_start = 0usize;

        'outer: loop {
            // Outer-iter precondition: at least `HASH_READ_SIZE = 8` bytes
            // ahead of `pos` so the unconditional 8-byte `u64` load below
            // is in-bounds for the live history buffer. `DFAST_MIN_MATCH_LEN
            // = 5` is the match acceptance threshold and is NOT a safe
            // load bound — using it here read up to 3 bytes past
            // `history.len()` on tiny blocks (CI fuzz `interop` crash,
            // 7-byte input).
            // NOTE: when this guard fires on the very first outer-iter
            // (tiny block, `current_len < 9`), we `break 'outer` BEFORE
            // `tail_seed_anchor` is even declared. The post-loop
            // `seed_remaining_hashable_starts(.., pos)` then runs with
            // `pos` at its initial value (1 on first frame, or the
            // post-match cursor from a previous outer iter); that's the
            // correct tail seed for tiny blocks — `seed_pos = pos.min(
            // current_len).min(boundary_tail_start)` plus the
            // `seed_pos + DFAST_SHORT_HASH_LOOKAHEAD <= current_len`
            // guard inside the seeder keeps every insert in-bounds.
            if pos + HASH_READ_SIZE > current_len {
                break 'outer;
            }
            let mut step = 1usize;
            let mut next_step_pos = pos + DFAST_SKIP_STEP_GROWTH_INTERVAL;
            let mut ip0 = pos;
            let mut ip1 = ip0 + step;
            // Same `HASH_READ_SIZE` rationale for `ip1`: the inner loop
            // pre-loads 8 bytes at `concat_idx1` for the `hl1` precompute
            // and the `_search_next_long` retry, so `ip1 + 8 <= current_len`
            // must hold before we enter. If `ip0` is still hashable but
            // `ip1` is not (boundary case `pos == current_len - 8`), we
            // still want to probe `ip0` — skipping it would leave the
            // last hashable position to `seed_remaining_hashable_starts`,
            // which inserts but does NOT search, and that drops real
            // matches in the tail window vs the upstream reference
            // (whose single-cursor loop probes every position p with
            // `p + HASH_READ_SIZE <= iend`). Handle the boundary inline
            // before exiting: a single-cursor probe at `ip0` (rep peek
            // and `_search_next_long` retry both depend on `ip1` so
            // they're skipped — donor accepts that exact tradeoff at
            // the iend boundary).
            if ip1 + HASH_READ_SIZE > current_len {
                if let Some(committed) =
                    self.probe_tail_ip0_only(current_abs_start, current_len, ip0, literals_start)
                {
                    let start = self.emit_candidate(
                        current_abs_start,
                        &mut literals_start,
                        committed,
                        handle_sequence,
                    );
                    pos = start + committed.match_len;
                    pos = self.extend_with_repcode_after_match(
                        current_abs_start,
                        current_len,
                        pos,
                        &mut literals_start,
                        handle_sequence,
                    );
                    continue 'outer;
                }
                break 'outer;
            }

            // Re-read every per-frame-mutable cursor here — `emit_candidate`
            // in the previous outer iteration may have triggered a rebase.
            let history_abs_start = self.history_abs_start;
            let position_base = self.position_base;
            let history_start_offset = self.history_start;
            let history_base_ptr = self.history.as_ptr();
            let short_hash_ptr = self.short_hash.as_mut_ptr();
            let long_hash_ptr = self.long_hash.as_mut_ptr();
            let concat_len = self.history.len() - history_start_offset;

            // Pre-compute long hash at ip0 ONCE per outer iter.
            // `concat_idx = (current_abs_start + ip0) - history_abs_start`
            // is the byte offset within `live_history`.
            let mut hl0_idx;
            let mut idxl0;
            // SAFETY: `current_abs_start + ip0 >= history_abs_start`
            // (`ip` is inside the current block, which is part of live
            // history). The 8-byte unaligned load on
            // `concat[concat_idx..]` is safe because the outer loop
            // guards above enforce `ip0 + HASH_READ_SIZE <= current_len`,
            // and `current_abs_start + current_len - history_abs_start
            // <= concat_len` (live history contains the full current
            // block plus any retained earlier blocks), giving
            // `concat_idx + 8 <= concat_len`. The `debug_assert!` below
            // makes that invariant explicit so a future refactor that
            // touches eviction / `compact_history` / `trim_to_window`
            // semantics catches a violation in tests instead of leaking
            // through to ASan UB.
            unsafe {
                let concat_idx = (current_abs_start + ip0) - history_abs_start;
                debug_assert!(
                    concat_idx + HASH_READ_SIZE <= concat_len,
                    "fast-loop 8-byte load OOB: concat_idx={} HASH_READ_SIZE={} concat_len={}",
                    concat_idx,
                    HASH_READ_SIZE,
                    concat_len,
                );
                let v8 = (history_base_ptr.add(history_start_offset + concat_idx) as *const u64)
                    .read_unaligned();
                hl0_idx = (v8.wrapping_mul(PRIME) >> long_shift) as usize;
                idxl0 = *long_hash_ptr.add(hl0_idx);
            }

            // Inner-loop exit shape. Every `break 'inner` produces a
            // value of this type, so the type system enforces the
            // previously implicit pairing between "did we commit a
            // match?" and "where does the tail seeder pick up?":
            //
            //   * `Committed(c)` — rep1 / long / short(+next_long retry)
            //     paths chose `c`; outer arm runs emit + rep-extension.
            //   * `Tail(seed)` — inner ran out of safe scan room at
            //     `seed = ip0` (first un-scanned position); outer arm
            //     hands `seed` to `seed_remaining_hashable_starts` so
            //     the tail seeder does not redundantly re-pack
            //     positions the fast loop already wrote (which is what
            //     restarting from outer-entry `pos` would do on a
            //     miss-only block — throwing away the skip-step win on
            //     incompressible data).
            //
            // Adding a new `break 'inner` variant now forces the
            // author to pick a `InnerExit::…` variant explicitly; the
            // previous `Option<MatchCandidate>` + sibling `usize`
            // pairing relied on a comment-block to flag the coupling.
            enum InnerExit {
                Committed(MatchCandidate),
                Tail(usize),
            }

            let inner_exit: InnerExit = 'inner: loop {
                let abs_ip0 = current_abs_start + ip0;
                let abs_ip1 = current_abs_start + ip1;
                let lit_len_ip0 = ip0 - literals_start;
                let lit_len_ip1 = ip1 - literals_start;
                let packed_curr = ((abs_ip0 - position_base) as u32) + 1;

                let concat_idx0 = abs_ip0 - history_abs_start;
                let concat_idx1 = abs_ip1 - history_abs_start;

                // Load 8 bytes at ip0 for both short (low 4) and long
                // probe equality checks. We already used `v8_at_ip0` to
                // compute hl0/idxl0 in the outer init / previous iter's
                // carry; reload now (cheap unaligned read) so the
                // `read_unaligned` is from the same offset the
                // probe-eq below will use.
                let v8_0 = unsafe {
                    (history_base_ptr.add(history_start_offset + concat_idx0) as *const u64)
                        .read_unaligned()
                };
                let v4_0 = v8_0 & 0xFFFF_FFFF;
                let hs0_idx = (v4_0.wrapping_mul(PRIME) >> short_shift) as usize;
                let idxs0 = unsafe { *short_hash_ptr.add(hs0_idx) };

                // Donor parity (`zstd_double_fast.c:187`): update BOTH
                // tables at curr BEFORE checking matches. The benefit is
                // for hash-table consumers, NOT the rep peek (rep at ip+1
                // reads `offset_hist[0]`, never the hash tables). The
                // donor rationale is the long-hash retry path: the next
                // inner iter's `idxl1` lookup (`hashLong[hl1_idx]`) can
                // collide with this iter's `hl0_idx`, and writing curr
                // first means a self-collision still resolves to a real
                // match instead of the previous occupant. The short
                // probe of the same iter and the `_search_next_long`
                // retry at ip+1 are the other consumers that see the
                // fresh write.
                unsafe {
                    *long_hash_ptr.add(hl0_idx) = packed_curr;
                    *short_hash_ptr.add(hs0_idx) = packed_curr;
                }

                // Donor parity (`zstd_double_fast.c:190`): inline rep1
                // peek at ip+1, 4-byte gate. Donor's hot path checks ONLY
                // `offset_1` here (full 3-rep walk lives in lazy/btopt).
                // Since the peek is at `ip+1` with `pos >= literals_start`,
                // `lit_len_ip1 >= 1`, so `offset_hist[0]` is the donor's
                // `offset_1`. The `repcode_candidate_shared` helper we used
                // before walked all three offsets + did a full SIMD
                // `common_prefix_len` per probe, paying ~3× the work for
                // rep2/rep3 hits that the dfast fast path never benefits
                // from (those wins live in the lazy/btopt strategies).
                let rep1 = self.offset_hist[0] as usize;
                if rep1 != 0 && rep1 <= abs_ip1 {
                    let cand_pos_r = abs_ip1 - rep1;
                    if cand_pos_r >= history_abs_start
                        && cand_pos_r >= current_abs_start + literals_start
                    {
                        let cand_idx_r = cand_pos_r - history_abs_start;
                        // 4-byte gate; full forward count only if it passes.
                        let cand4 = unsafe {
                            (history_base_ptr.add(history_start_offset + cand_idx_r) as *const u32)
                                .read_unaligned()
                        };
                        let cur4 = unsafe {
                            (history_base_ptr.add(history_start_offset + concat_idx1) as *const u32)
                                .read_unaligned()
                        };
                        if cand4 == cur4 {
                            let mut match_len = 4usize;
                            let max_fwd = concat_len.saturating_sub(concat_idx1 + 4);
                            unsafe {
                                let lhs =
                                    history_base_ptr.add(history_start_offset + cand_idx_r + 4);
                                let rhs =
                                    history_base_ptr.add(history_start_offset + concat_idx1 + 4);
                                let ext = crate::encoding::fastpath::dispatch_common_prefix_len_ptr(
                                    lhs, rhs, max_fwd,
                                );
                                match_len += ext;
                            }
                            // Rep extensions use the 4-byte
                            // `DFAST_REP_MIN_MATCH_LEN` floor, NOT the
                            // hash-search `DFAST_MIN_MATCH_LEN = 5`. Rep
                            // coding has no on-wire offset cost, so the
                            // upstream reference accepts 4-byte rep
                            // hits; gating at 5 here would silently drop
                            // every 4-byte rep1 the upstream encoder
                            // produces, and leave the post-match
                            // `extend_with_repcode_after_match` chain
                            // (which uses 4) inconsistent with the peek.
                            if match_len >= DFAST_REP_MIN_MATCH_LEN {
                                let concat = &self.history[history_start_offset..];
                                let rep_cand = extend_backwards_shared(
                                    concat,
                                    history_abs_start,
                                    cand_pos_r,
                                    abs_ip1,
                                    match_len,
                                    lit_len_ip1,
                                );
                                break 'inner InnerExit::Committed(rep_cand);
                            }
                        }
                    }
                }

                // Precompute hl1 (donor `_search_next_long` carry, line 197).
                let v8_1 = unsafe {
                    (history_base_ptr.add(history_start_offset + concat_idx1) as *const u64)
                        .read_unaligned()
                };
                let hl1_idx = (v8_1.wrapping_mul(PRIME) >> long_shift) as usize;

                // Long match check at ip0 with idxl0. 8-byte equality
                // gate (`MEM_read64`) — if it passes, candidate is real.
                if idxl0 != DFAST_EMPTY_SLOT {
                    let cand_pos = position_base + (idxl0 as usize) - 1;
                    if cand_pos >= history_abs_start && cand_pos < abs_ip0 {
                        let cand_idx = cand_pos - history_abs_start;
                        // SAFETY: same buffer/length bounds as v8_0 above.
                        let cand_v8 = unsafe {
                            (history_base_ptr.add(history_start_offset + cand_idx) as *const u64)
                                .read_unaligned()
                        };
                        if cand_v8 == v8_0 {
                            // 8 bytes match; count forward + extend back.
                            let mut match_len = 8usize;
                            let max_fwd = concat_len.saturating_sub(concat_idx0 + 8);
                            // SAFETY: both ptrs at the same buffer; offsets
                            // verified above. `max_fwd` caps the scan to
                            // the live region.
                            unsafe {
                                let lhs = history_base_ptr.add(history_start_offset + cand_idx + 8);
                                let rhs =
                                    history_base_ptr.add(history_start_offset + concat_idx0 + 8);
                                let ext = crate::encoding::fastpath::dispatch_common_prefix_len_ptr(
                                    lhs, rhs, max_fwd,
                                );
                                match_len += ext;
                            }
                            let concat = &self.history[history_start_offset..];
                            let cand = extend_backwards_shared(
                                concat,
                                history_abs_start,
                                cand_pos,
                                abs_ip0,
                                match_len,
                                lit_len_ip0,
                            );
                            break 'inner InnerExit::Committed(cand);
                        }
                    }
                }

                let idxl1 = unsafe { *long_hash_ptr.add(hl1_idx) };

                // Short match check at ip0 with idxs0 — 4-byte gate
                // ONLY (donor `zstd_double_fast.c:220`). Forward count
                // and `_search_next_long` retry happen ONLY on hit.
                if idxs0 != DFAST_EMPTY_SLOT {
                    let cand_pos_s = position_base + (idxs0 as usize) - 1;
                    if cand_pos_s >= history_abs_start && cand_pos_s < abs_ip0 {
                        let cand_idx_s = cand_pos_s - history_abs_start;
                        let cand4 = unsafe {
                            (history_base_ptr.add(history_start_offset + cand_idx_s) as *const u32)
                                .read_unaligned()
                        };
                        if cand4 == v4_0 as u32 {
                            // Short hit: count forward from byte 4 onwards.
                            let mut s_match_len = 4usize;
                            let max_fwd = concat_len.saturating_sub(concat_idx0 + 4);
                            unsafe {
                                let lhs =
                                    history_base_ptr.add(history_start_offset + cand_idx_s + 4);
                                let rhs =
                                    history_base_ptr.add(history_start_offset + concat_idx0 + 4);
                                let ext = crate::encoding::fastpath::dispatch_common_prefix_len_ptr(
                                    lhs, rhs, max_fwd,
                                );
                                s_match_len += ext;
                            }
                            let concat = &self.history[history_start_offset..];
                            let short_cand = extend_backwards_shared(
                                concat,
                                history_abs_start,
                                cand_pos_s,
                                abs_ip0,
                                s_match_len,
                                lit_len_ip0,
                            );

                            // Enforce the hash-search floor BEFORE
                            // committing this short hit. The 4-byte
                            // equality gate plus forward-count extension
                            // can produce `short_cand.match_len < 5`
                            // (the literal 4-byte hit, no extension).
                            // `probe_slot_match` rejects below-floor
                            // long-hash hits at the same gate; the
                            // short-hash path must do the same so the
                            // fast loop never emits a sub-floor non-rep
                            // match. The retry below can still upgrade
                            // to a long hit (which has its own 8-byte
                            // floor, comfortably above `DFAST_MIN_MATCH_LEN`).
                            let short_hit_valid = short_cand.match_len >= DFAST_MIN_MATCH_LEN;

                            // Donor `_search_next_long` retry (line 260):
                            // try long match at ip1 with precomputed idxl1.
                            // If it produces a strictly longer match, use it.
                            let mut chosen = short_cand;
                            let mut retry_upgraded = false;
                            if idxl1 != DFAST_EMPTY_SLOT {
                                let cand_pos_l1 = position_base + (idxl1 as usize) - 1;
                                if cand_pos_l1 >= history_abs_start && cand_pos_l1 < abs_ip1 {
                                    let cand_idx_l1 = cand_pos_l1 - history_abs_start;
                                    let cand_v8_l1 = unsafe {
                                        (history_base_ptr.add(history_start_offset + cand_idx_l1)
                                            as *const u64)
                                            .read_unaligned()
                                    };
                                    if cand_v8_l1 == v8_1 {
                                        let mut l1_match_len = 8usize;
                                        let max_fwd_l1 = concat_len.saturating_sub(concat_idx1 + 8);
                                        unsafe {
                                            let lhs = history_base_ptr
                                                .add(history_start_offset + cand_idx_l1 + 8);
                                            let rhs = history_base_ptr
                                                .add(history_start_offset + concat_idx1 + 8);
                                            let ext = crate::encoding::fastpath::dispatch_common_prefix_len_ptr(
                                                lhs, rhs, max_fwd_l1,
                                            );
                                            l1_match_len += ext;
                                        }
                                        if l1_match_len > short_cand.match_len {
                                            let concat = &self.history[history_start_offset..];
                                            chosen = extend_backwards_shared(
                                                concat,
                                                history_abs_start,
                                                cand_pos_l1,
                                                abs_ip1,
                                                l1_match_len,
                                                lit_len_ip1,
                                            );
                                            // Long-hash hits start at 8
                                            // bytes (`MEM_read64` gate
                                            // above), well above
                                            // `DFAST_MIN_MATCH_LEN = 5`,
                                            // so the retry upgrade is
                                            // always valid even when
                                            // the raw short hit was
                                            // below the floor.
                                            retry_upgraded = true;
                                        }
                                    }
                                }
                            }
                            if short_hit_valid || retry_upgraded {
                                break 'inner InnerExit::Committed(chosen);
                            }
                            // Below-floor short hit with no retry
                            // upgrade — fall through to the step bump
                            // and keep scanning. Discarding here is
                            // correctness-relevant: emitting a 4-byte
                            // non-rep match would mint an offset on
                            // wire that costs more than the 4-byte
                            // payload buys (offsets at level 2/3 dfast
                            // are 13–17 bits depending on offset class
                            // and prior offset_hist state — strictly
                            // worse than emitting the 4 bytes as
                            // literals).
                        }
                    }
                }

                // Step bump on distance (donor `zstd_double_fast.c:224-228`).
                if ip1 >= next_step_pos {
                    step = (step + 1).min(DFAST_MAX_SKIP_STEP);
                    next_step_pos += DFAST_SKIP_STEP_GROWTH_INTERVAL;
                }

                // Advance: ip0 = ip1; ip1 += step; carry hl1 → hl0 / idxl1 → idxl0.
                ip0 = ip1;
                ip1 += step;
                hl0_idx = hl1_idx;
                idxl0 = idxl1;
                if ip1 + HASH_READ_SIZE > current_len {
                    // First position the fast loop did NOT pack into the
                    // hash tables. `seed_remaining_hashable_starts` will
                    // pick up from `ip0` instead of restarting at the
                    // outer-entry `pos`.
                    break 'inner InnerExit::Tail(ip0);
                }
            };

            match inner_exit {
                InnerExit::Committed(candidate) => {
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
                }
                InnerExit::Tail(seed) => {
                    // Inner loop ran out of safe scan room without
                    // committing. Hand the tail seeder the first
                    // un-scanned position so it does not redundantly
                    // re-pack everything the fast loop already wrote.
                    pos = seed;
                    break 'outer;
                }
            }
        }

        self.seed_remaining_hashable_starts(current_abs_start, current_len, pos);
        self.emit_trailing_literals(literals_start, handle_sequence);
    }

    /// Single-cursor probe at the last hashable position in the
    /// current block. Called from `start_matching_fast_loop` only when
    /// the outer iteration sees `ip0` still hashable but `ip1` past
    /// the end — the upstream reference's single-cursor loop probes
    /// every position with `p + HASH_READ_SIZE <= iend`, and skipping
    /// `ip0` here would leave a real match unsearched.
    ///
    /// Mirror the inner loop's long+short probe and the table update
    /// at `ip0`, but skip the rep1 peek (reads at `ip1`) and the
    /// `_search_next_long` retry (reads at `ip1`) — both depend on a
    /// hashable `ip1`. Upstream accepts this exact tradeoff at the
    /// `iend` boundary: the rep peek and retry only ever fire when a
    /// second hashable position exists.
    ///
    /// Returns `Some(MatchCandidate)` if a long or short hit at `ip0`
    /// meets the `DFAST_MIN_MATCH_LEN` floor, `None` otherwise (caller
    /// then breaks the outer loop). On a hit, the hash tables are NOT
    /// updated by this helper — the caller routes through
    /// `emit_candidate` which inserts via `insert_positions` over the
    /// emitted range, exactly like the inner loop's hit path.
    fn probe_tail_ip0_only(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        ip0: usize,
        literals_start: usize,
    ) -> Option<MatchCandidate> {
        debug_assert!(ip0 + HASH_READ_SIZE <= current_len);
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        let short_shift = 64 - self.short_hash_bits;
        let long_shift = 64 - self.long_hash_bits;

        let abs_ip0 = current_abs_start + ip0;
        let lit_len_ip0 = ip0 - literals_start;
        let history_abs_start = self.history_abs_start;
        let position_base = self.position_base;
        let history_start_offset = self.history_start;
        let concat_len = self.history.len() - history_start_offset;
        let history_base_ptr = self.history.as_ptr();

        let concat_idx0 = abs_ip0 - history_abs_start;
        let packed_curr = ((abs_ip0 - position_base) as u32) + 1;

        // SAFETY: `concat_idx0 + 8 <= concat_len` follows from the
        // caller's `ip0 + HASH_READ_SIZE <= current_len` precondition
        // (the live history contains the full current block).
        let v8_0 = unsafe {
            (history_base_ptr.add(history_start_offset + concat_idx0) as *const u64)
                .read_unaligned()
        };
        let v4_0 = v8_0 & 0xFFFF_FFFF;
        let hl0_idx = (v8_0.wrapping_mul(PRIME) >> long_shift) as usize;
        let hs0_idx = (v4_0.wrapping_mul(PRIME) >> short_shift) as usize;

        // Pre-fetch the candidate indices BEFORE writing `packed_curr`,
        // to mirror the inner loop (probe lookup precedes table
        // update). `ensure_room_for` already ran for the whole block at
        // the top of `start_matching_fast_loop`, so the raw pointers
        // into `short_hash` / `long_hash` are still valid here.
        let short_hash_ptr = self.short_hash.as_mut_ptr();
        let long_hash_ptr = self.long_hash.as_mut_ptr();
        let idxl0 = unsafe { *long_hash_ptr.add(hl0_idx) };
        let idxs0 = unsafe { *short_hash_ptr.add(hs0_idx) };
        unsafe {
            *long_hash_ptr.add(hl0_idx) = packed_curr;
            *short_hash_ptr.add(hs0_idx) = packed_curr;
        }

        // Long-hash probe first (upstream priority: an 8-byte hit
        // beats a 4-byte hit even before extension).
        if idxl0 != DFAST_EMPTY_SLOT {
            let cand_pos = position_base + (idxl0 as usize) - 1;
            if cand_pos >= history_abs_start && cand_pos < abs_ip0 {
                let cand_idx = cand_pos - history_abs_start;
                let cand_v8 = unsafe {
                    (history_base_ptr.add(history_start_offset + cand_idx) as *const u64)
                        .read_unaligned()
                };
                if cand_v8 == v8_0 {
                    let mut match_len = 8usize;
                    let max_fwd = concat_len.saturating_sub(concat_idx0 + 8);
                    unsafe {
                        let lhs = history_base_ptr.add(history_start_offset + cand_idx + 8);
                        let rhs = history_base_ptr.add(history_start_offset + concat_idx0 + 8);
                        let ext = crate::encoding::fastpath::dispatch_common_prefix_len_ptr(
                            lhs, rhs, max_fwd,
                        );
                        match_len += ext;
                    }
                    let concat = &self.history[history_start_offset..];
                    return Some(extend_backwards_shared(
                        concat,
                        history_abs_start,
                        cand_pos,
                        abs_ip0,
                        match_len,
                        lit_len_ip0,
                    ));
                }
            }
        }

        // Short-hash probe (4-byte gate, forward extension, same
        // floor-enforcement as the inner loop's short path — see
        // comment there).
        if idxs0 != DFAST_EMPTY_SLOT {
            let cand_pos_s = position_base + (idxs0 as usize) - 1;
            if cand_pos_s >= history_abs_start && cand_pos_s < abs_ip0 {
                let cand_idx_s = cand_pos_s - history_abs_start;
                let cand4 = unsafe {
                    (history_base_ptr.add(history_start_offset + cand_idx_s) as *const u32)
                        .read_unaligned()
                };
                if cand4 == v4_0 as u32 {
                    let mut s_match_len = 4usize;
                    let max_fwd = concat_len.saturating_sub(concat_idx0 + 4);
                    unsafe {
                        let lhs = history_base_ptr.add(history_start_offset + cand_idx_s + 4);
                        let rhs = history_base_ptr.add(history_start_offset + concat_idx0 + 4);
                        let ext = crate::encoding::fastpath::dispatch_common_prefix_len_ptr(
                            lhs, rhs, max_fwd,
                        );
                        s_match_len += ext;
                    }
                    let concat = &self.history[history_start_offset..];
                    let short_cand = extend_backwards_shared(
                        concat,
                        history_abs_start,
                        cand_pos_s,
                        abs_ip0,
                        s_match_len,
                        lit_len_ip0,
                    );
                    if short_cand.match_len >= DFAST_MIN_MATCH_LEN {
                        return Some(short_cand);
                    }
                }
            }
        }

        None
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
    /// `DFAST_MIN_MATCH_LEN = 5` enforced on the main search
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
        loop {
            // Need at least DFAST_REP_MIN_MATCH_LEN bytes of room past `pos`.
            if pos + DFAST_REP_MIN_MATCH_LEN > current_len {
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
            if cur_idx + DFAST_REP_MIN_MATCH_LEN > concat.len() {
                break;
            }
            // Cheap 4-byte gate before the SIMD `common_prefix_len`.
            if concat[cur_idx..cur_idx + 4] != concat[cand_idx..cand_idx + 4] {
                break;
            }
            let match_len = common_prefix_len(&concat[cand_idx..], &concat[cur_idx..]);
            if match_len < DFAST_REP_MIN_MATCH_LEN {
                break;
            }
            // Sparse complementary insertion (upstream parity,
            // `zstd_double_fast.c:300-304`): upstream inserts ONLY at
            // `curr+2`, `ip-2`, `ip-1` after a match — three specific
            // positions, not the whole match range. The previous
            // `insert_positions(abs_pos, abs_pos + match_len)` made
            // sense only under the 4-slot bucket; with single-slot
            // donor parity it would just overwrite every bucket along
            // the match span and discard whichever positions the
            // producer was about to re-probe.
            //
            // At the floor `match_len == DFAST_REP_MIN_MATCH_LEN (= 4)`
            // the three targets collapse to two distinct positions.
            // With `post_match_end = abs_pos + 4` the three offsets
            // resolve to:
            //   `curr + 2` = abs_pos + 2
            //   `ip - 2`   = abs_pos + 4 - 2 = abs_pos + 2  ← same as curr+2
            //   `ip - 1`   = abs_pos + 4 - 1 = abs_pos + 3  ← distinct
            // So `curr+2` and `ip-2` write the same slot twice;
            // single-slot overwrite is idempotent, so the duplicate
            // write is correctness-neutral. It's one wasted store on
            // the shortest rep extension and not worth a branch to
            // dedup. For `match_len >= 5` all three offsets are
            // distinct.
            //
            // Why `abs_pos` itself is NOT in the insert set, despite
            // not being written by the fast loop's pre-check insert
            // (the previous range form `insert_positions(abs_pos,
            // post_match_end)` did include it): upstream
            // `ZSTD_compressBlock_doubleFast_*` likewise does not
            // insert at the rep-extension start. After a rep match,
            // upstream just advances `ip` and reruns the outer fast
            // loop, which writes the new `curr` (the post-rep cursor)
            // — never the rep's own start. The three offsets above
            // are upstream's primary-match-emit insertion pattern,
            // mirrored here to preserve hit rate on the chains that
            // follow a rep extension. The hash-state delta vs the
            // prior range-insert behavior is intentional and tracked
            // by the sequence-stream comparator harness (the
            // per-sequence diff vs FFI mentioned in the PR body's
            // deferred section); the audit signed off on the current
            // sparse set as the closest faithful mirror of upstream.
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
        // Inline the trailing-block slice rather than calling
        // `get_last_space()` so this matches the gate pattern used by
        // `skip_matching` / `start_matching` (read `window_blocks.back()`
        // with `unwrap_or(0)`). `emit_candidate` runs only after a
        // successful match was found in the active block, so
        // `last_len > 0` is a structural precondition.
        let last_len = self.window_blocks.back().copied().unwrap_or(0);
        let current = &self.history[self.history.len() - last_len..];
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
        let last_len = self.window_blocks.back().copied().unwrap_or(0);
        if literals_start < last_len {
            // Inline rather than calling `get_last_space()` so the gate
            // pattern matches `skip_matching` / `start_matching` /
            // `emit_candidate`. `last_len > 0` is already established by
            // the `literals_start < last_len` check above.
            let current = &self.history[self.history.len() - last_len..];
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

    #[inline(always)]
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

    #[inline(always)]
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

    #[inline(always)]
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
                8, // long-hash equality gate
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
                4, // short-hash equality gate
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
                        8, // long-hash equality gate, `_search_next_long` retry
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
    #[inline(always)]
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
        gate_len: usize,
    ) -> Option<MatchCandidate> {
        if slot == DFAST_EMPTY_SLOT {
            return None;
        }
        let candidate_pos = position_base + (slot as usize) - 1;
        if candidate_pos < history_abs_start || candidate_pos >= abs_pos {
            return None;
        }
        let candidate_idx = candidate_pos - history_abs_start;
        // Probe-width equality gate before the SIMD walk. The long-hash
        // callers pass `gate_len = 8` (matches an `MEM_read64`-style
        // probe in the upstream reference); the short-hash caller
        // passes `gate_len = 4` (`MEM_read32`-style). A 1-byte
        // precheck would accept hash collisions whose actual common
        // prefix runs from 1 byte up to less than the probe width,
        // paying the full `common_prefix_len` walk for them on every
        // iteration. The wider gate rejects those collisions early
        // and matches what the upstream `_search_next_long` path does
        // after a hit.
        if candidate_idx + gate_len > concat.len() || current_idx + gate_len > concat.len() {
            return None;
        }
        if concat[candidate_idx..candidate_idx + gate_len]
            != concat[current_idx..current_idx + gate_len]
        {
            return None;
        }
        let match_len = common_prefix_len(&concat[candidate_idx..], &concat[current_idx..]);
        if match_len < DFAST_MIN_MATCH_LEN {
            return None;
        }
        Some(self.extend_backwards(candidate_pos, abs_pos, match_len, lit_len))
    }

    #[inline(always)]
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
        if start >= end {
            return;
        }
        // Hoist the rebase trigger out of the inner loop: a single
        // `ensure_room_for(end - 1)` covers every `pack_slot` in the
        // range. The per-position `ensure_room_for` call inside
        // `insert_position` would re-check on every byte and the
        // compiler cannot prove the call is idempotent through
        // `&mut self`.
        self.ensure_room_for(end - 1);

        // Snapshot every per-call invariant. `&mut self` blocks the
        // optimiser from hoisting these loads across the inner loop —
        // the bodies of `short_hash` / `long_hash` writes mutate
        // through `self`, so each iteration would otherwise reload
        // `history.len()`, `history_start`, `position_base`,
        // `*_hash_bits`. With ~1 input byte per
        // call on the dfast hot path that re-load shape was the
        // dominant cost in the per-position cluster.
        let history_abs_start = self.history_abs_start;
        let position_base = self.position_base;
        let history_start = self.history_start;
        let concat_len = self.history.len() - history_start;
        let history_base_ptr = self.history.as_ptr();
        let short_hash_bits = self.short_hash_bits;
        let long_hash_bits = self.long_hash_bits;
        let short_hash_ptr = self.short_hash.as_mut_ptr();
        let long_hash_ptr = self.long_hash.as_mut_ptr();
        let short_shift = 64 - short_hash_bits;
        let long_shift = 64 - long_hash_bits;

        // Two contiguous regions in the input range:
        // * `[start .. long_safe_end)` — every position has at least 8
        //   bytes of lookahead, so both short and long hashes get
        //   inserted from a single 8-byte unaligned load.
        // * `[long_safe_end .. short_safe_end)` — only 4..7 bytes
        //   remain, so only the short hash gets inserted (4-byte
        //   load).
        // Past `short_safe_end` neither hash has enough lookahead and
        // donor parity is "no insert" — skip entirely.
        let abs_concat_end = history_abs_start + concat_len;
        let long_safe_end = abs_concat_end.saturating_sub(7).min(end);
        let short_safe_end = abs_concat_end.saturating_sub(3).min(end);

        // SAFETY: `history_base_ptr.add(history_start + idx)` is
        // in-bounds for `idx + 8 <= concat_len`, which the two
        // `*_safe_end` cutoffs enforce. `short_hash_ptr.add(k)` /
        // `long_hash_ptr.add(k)` are in-bounds because
        // `ensure_hash_tables` sizes the two tables to `1 <<
        // *_hash_bits` and `k = mixed >> (64 - bits)` has at most
        // `bits` bits set. `position_base` and `history_abs_start`
        // are constant across the loop after the single `ensure_room_for`
        // call above. `packed` fits in `u32` by that same gate.
        let mut pos = start;
        while pos < long_safe_end {
            unsafe {
                let idx = pos - history_abs_start;
                let packed = ((pos - position_base) as u32) + 1;
                let load_ptr = history_base_ptr.add(history_start + idx);
                let v8 = (load_ptr as *const u64).read_unaligned();
                let v4 = v8 & 0xFFFF_FFFF;
                // Donor parity (`zstd_compress_internal.h:923-924`):
                // scalar `* prime8bytes` then shift to high bits. Drops
                // the CRC32d-based kernel dispatch (3-4 instructions) for
                // a single mul on the per-byte insert path.
                let mixed_short = v4.wrapping_mul(0xCF1BBCDCB7A56463_u64);
                let mixed_long = v8.wrapping_mul(0xCF1BBCDCB7A56463_u64);
                let short_idx = (mixed_short >> short_shift) as usize;
                let long_idx = (mixed_long >> long_shift) as usize;
                *short_hash_ptr.add(short_idx) = packed;
                *long_hash_ptr.add(long_idx) = packed;
            }
            pos += 1;
        }
        while pos < short_safe_end {
            unsafe {
                let idx = pos - history_abs_start;
                let packed = ((pos - position_base) as u32) + 1;
                let load_ptr = history_base_ptr.add(history_start + idx);
                let v4 = (load_ptr as *const u32).read_unaligned() as u64;
                let mixed_short = v4.wrapping_mul(0xCF1BBCDCB7A56463_u64);
                let short_idx = (mixed_short >> short_shift) as usize;
                *short_hash_ptr.add(short_idx) = packed;
            }
            pos += 1;
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
        // Single-slot overwrite (upstream parity): the previous 4-slot
        // bucket shift (`copy_within(..)`) is gone — upstream
        // `ZSTD_compressBlock_doubleFast_*` writes a single `U32` per
        // hash position and relies on the dense `_search_next_long`
        // retry in `hash_candidate` (via `best_match`) to preserve
        // compression ratio.
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

    #[inline(always)]
    pub(crate) fn short_hash_index(&self, data: &[u8]) -> usize {
        let value = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        self.hash_index_with_bits(value, self.short_hash_bits)
    }

    #[inline(always)]
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

    #[inline(always)]
    fn hash_index_with_bits(&self, value: u64, bits: usize) -> usize {
        // Donor parity (`zstd_compress_internal.h:923-924`, `ZSTD_hash8`):
        // a single 64-bit multiply by `prime8bytes` followed by a high-bits
        // shift. Drops the CRC32d + rotate + mul kernel dispatch the rest
        // of the crate uses — for dfast the donor's scalar hash is
        // distribution-equivalent and one instruction shorter on the hot
        // path.
        let mixed = value.wrapping_mul(0xCF1BBCDCB7A56463_u64);
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
    /// not the main-loop `DFAST_MIN_MATCH_LEN = 5` floor. A regression
    /// back above 4 would still pass the constant-run / cross-block tests
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
                     A regression back to DFAST_MIN_MATCH_LEN > 4 would skip \
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
