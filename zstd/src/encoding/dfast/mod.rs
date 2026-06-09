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
use super::dict_attach::DictAttach;
use super::fastpath::{FastpathKernel, select_kernel};
use super::incompressible::block_looks_incompressible;
use super::match_generator::{
    DFAST_EMPTY_SLOT, DFAST_HASH_BITS, DFAST_INCOMPRESSIBLE_SKIP_STEP, DFAST_MAX_SKIP_STEP,
    DFAST_MIN_MATCH_LEN, DFAST_REBASE_GUARD_BAND, DFAST_SHORT_HASH_BITS_DELTA,
    DFAST_SHORT_HASH_LOOKAHEAD, DFAST_SKIP_STEP_GROWTH_INTERVAL, MIN_WINDOW_LOG,
};
use super::match_table::helpers::{common_prefix_len_with_kernel, extend_backwards_shared};
use super::match_table::storage::{REBASE_RESET_FLOOR_CEILING, check_stream_abs_headroom};
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

#[derive(Clone)]
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
    /// Immutable dictionary long+short hash tables (donor `dictMatchState`)
    /// plus the CDict cache lifecycle, via the shared [`DictAttach`] level-1
    /// scaffolding. Built once over the dictionary region at the front of the
    /// contiguous history (flat `[dict][input]` model like the Fast backend:
    /// a dict match is `offset = ip - dict_pos` and counts cross the
    /// dict→input boundary, no `dictBase`/`dictIndexDelta`). Slots store the
    /// dict position as a +1-biased CONCAT index (offset into
    /// `history[history_start..]`), NOT the live tables' `position_base`-packed
    /// encoding — the dict table is never rebased, so it keys off the stable
    /// concat coordinate and is invalidated on any history eviction (which
    /// would slide the dict out of the concat) so positions never go stale.
    /// `is_attached()` activates the dual-probe kernel; `region_len()` is the
    /// dict/input boundary (`dict_end`, a concat index).
    pub(crate) dict: DictAttach<DfastDictTables>,
    /// CPU kernel for the `common_prefix_len` / repcode byte-compares,
    /// resolved once at construction so the per-byte match-finder scans skip
    /// the `select_kernel()` `OnceLock` atomic on every call.
    pub(crate) kernel: FastpathKernel,
    /// Borrowed one-shot scan window: `(input_base_ptr, current_block_end)`.
    /// When `Some`, the fast loop sources bytes from the caller's in-place
    /// input slice with absolute input positions (`abs_start = position_base
    /// = start_offset = 0`, readable length = `current_block_end`) instead of
    /// copying each block into the owned `history` concat — the dfast analog
    /// of the Fast backend's borrowed window. The pointer is the input start
    /// (constant for the frame); the second field is updated to the active
    /// block's end before each block scan. `None` = owned (history-copying)
    /// path. Cleared on `reset()`.
    pub(crate) borrowed: Option<(*const u8, usize)>,
}

/// The dfast backend's immutable dictionary tables — a long+short pair mirroring
/// the live [`DfastMatchGenerator::long_hash`] / [`DfastMatchGenerator::short_hash`]
/// shapes, sized to the same `(long_hash_bits, short_hash_bits)`. Held by the
/// shared [`DictAttach`] level-1 lifecycle; the per-tier dual-probe LOOKUP is
/// level-2 in this backend's kernel. Slots hold a +1-biased concat index
/// (`DFAST_EMPTY_SLOT = 0` is "no entry").
#[derive(Debug, Default, Clone)]
pub(crate) struct DfastDictTables {
    pub(crate) long: alloc::vec::Vec<u32>,
    pub(crate) short: alloc::vec::Vec<u32>,
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
            dict: DictAttach::new(),
            kernel: select_kernel(),
            borrowed: None,
        }
    }

    /// Set both hash table sizes from the per-level [`DfastConfig`]:
    /// `long_bits` = donor `cParams.hashLog`, `short_bits` = donor
    /// `cParams.chainLog`. Both clamps stay above `MIN_WINDOW_LOG` so very
    /// small windows don't underflow. The caller already caps `long_bits` by
    /// the source-size window when hinted, so no upper clamp is applied here.
    pub(crate) fn set_hash_bits(&mut self, long_bits: usize, short_bits: usize) {
        let min_bits = MIN_WINDOW_LOG as usize;
        let long_clamped = long_bits.max(min_bits);
        let short_clamped = short_bits.max(min_bits);
        let resized = self.long_hash_bits != long_clamped || self.short_hash_bits != short_clamped;
        if self.long_hash_bits != long_clamped {
            self.long_hash_bits = long_clamped;
            self.long_hash = Vec::new();
        }
        if self.short_hash_bits != short_clamped {
            self.short_hash_bits = short_clamped;
            self.short_hash = Vec::new();
        }
        if resized {
            // A table-size change makes the cached dict tables (sized to the old
            // bits) and their hash indices invalid — drop the attach so the next
            // prime rebuilds at the new shape.
            self.dict.invalidate();
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

    /// Heap bytes this matcher owns: history, the long/short hash tables, the
    /// window-block deque, and any attached dictionary tables.
    pub(crate) fn heap_size(&self) -> usize {
        let u32_sz = core::mem::size_of::<u32>();
        self.window_blocks.capacity() * core::mem::size_of::<usize>()
            + self.history.capacity()
            + (self.short_hash.capacity() + self.long_hash.capacity()) * u32_sz
            + self
                .dict
                .table()
                .map_or(0, |t| (t.long.capacity() + t.short.capacity()) * u32_sz)
    }

    pub(crate) fn reset(&mut self) {
        // Floor-advance reset (issue #337 technique, completing it for the
        // dfast backend — `MatchTable` already does this). Instead of
        // zeroing the long/short tables every frame (a memset proportional
        // to table size, ~25% of a small reused-context dfast frame),
        // advance the absolute-position floor past the previous frame's
        // end. Every slot-decode site rejects a candidate whose absolute
        // position is below `history_abs_start` before it dereferences a
        // history byte (audited: fast-loop long/short/long+1/repcode,
        // `probe_tail_ip0_only`, `probe_slot_match`), so the previous
        // frame's entries become unreachable without being cleared.
        // `position_base` is left untouched so stale slots still decode to
        // their (now sub-floor) absolute positions; `ensure_room_for` /
        // `reduce` keep the `u32` packing bounded as the cursor climbs.
        let next_floor = self.history_abs_start + (self.history.len() - self.history_start);
        self.window_size = 0;
        self.history.clear();
        self.history_start = 0;
        self.offset_hist = [1, 4, 8];
        if next_floor <= REBASE_RESET_FLOOR_CEILING {
            // Fast path: advance the floor; tables keep their contents (a
            // later `ensure_tables`/level change still reallocs them clean
            // if the dimensions changed). The dict tables key off stable
            // concat indices and are untouched here.
            self.history_abs_start = next_floor;
        } else {
            // Bounded fallback: rewind the cursor and zero the tables so
            // `history_abs_start` cannot climb toward `usize::MAX` (keeps
            // `check_stream_abs_headroom` satisfiable on 32-bit targets;
            // fires ~once per 2 GiB cumulative input there).
            self.history_abs_start = 0;
            self.position_base = 0;
            if !self.short_hash.is_empty() {
                self.short_hash.fill(DFAST_EMPTY_SLOT);
                self.long_hash.fill(DFAST_EMPTY_SLOT);
            }
        }
        // No Vec<u8> blocks to recycle: `add_data` returns each input
        // Vec to the caller eagerly via its own `reuse_space`, and the
        // history Vec is owned solely by the matcher. There is nothing
        // for an outer pool helper to do at reset time, so the dfast
        // signature does not take one (HC / Row do because they hold
        // per-block input Vecs internally; the dispatcher in
        // `match_generator.rs` resolves the per-backend shape).
        self.window_blocks.clear();
        // Drop any borrowed window: the input slice it pointed at does not
        // outlive the frame, and the next frame re-stages its own (or runs
        // the owned path). A stale pointer must never survive a reset.
        self.borrowed = None;
    }

    /// Slice of bytes from the most recently appended block. Returns
    /// the trailing `last_block_len` bytes of `history`, or an empty
    /// slice if no block has been ingested yet.
    ///
    /// Mirrors the inline gate pattern used by `skip_matching` /
    /// `skip_matching_dense` / `start_matching` / `emit_candidate` /
    /// `emit_trailing_literals`: read
    /// `window_blocks.back().copied().unwrap_or(0)` and slice the
    /// trailing `last_len` bytes (which is empty when `last_len == 0`).
    /// All current external callers — streaming encoder, block
    /// compressor, per-level helpers — invoke this only after at
    /// least one `add_data`, but returning an empty slice on the
    /// empty case keeps the trait surface aligned with the internal
    /// usage and avoids a panic-vs-gate divergence that would
    /// surprise a future refactor consolidating the call sites.
    pub(crate) fn get_last_space(&self) -> &[u8] {
        let last_len = self.window_blocks.back().copied().unwrap_or(0);
        &self.history[self.history.len() - last_len..]
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
        //
        // In-tree caller audit (`grep -rn '\.commit_space(' src/encoding/`):
        // every production path that reaches the driver's
        // `commit_space` → `add_data` chain originates in
        // `levels/fastest.rs`'s block emitter, which produces
        // non-empty blocks gated by `should_emit_raw_fast_path` /
        // RLE-detect on the source bytes — none of them pass
        // `Vec::new()`. The streaming encoder's block-sourcing loop
        // also filters empty reads before forwarding to the matcher.
        // Tests are the only callers that exercise the empty path
        // explicitly, and the regression covers eviction-driven trim
        // semantics through `trim_to_window` directly. So this
        // behaviour change is observable only by a hypothetical
        // future caller that relies on `add_data(empty)` as a
        // side-effecting trim trigger — and we deliberately want
        // such a caller to use `trim_to_window` instead.
        if data.is_empty() {
            reuse_space(data);
            return;
        }
        if self.window_size + data.len() > self.max_window_size {
            // Eviction advances `history_start`, so the dict tables' concat
            // indices (primed at `history_start == 0`) no longer address the
            // dict bytes — drop the attach (dict ratio benefit lost once the
            // dict slides out of the window, like the Fast backend).
            self.dict.invalidate();
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
    ///
    /// Unlike `HashChainTable::trim_to_window` /
    /// `RowMatcher::trim_to_window` this signature deliberately takes
    /// no `reuse_space` callback — Dfast stores its raw bytes in the
    /// single contiguous `history` buffer (no per-block `Vec<u8>` to
    /// recycle), releases the dead prefix via `split_off`, and
    /// surfaces eviction byte count to the dispatcher via the
    /// `window_size` delta rather than a callback. A uniform-signature
    /// trim helper would force every call site to monomorphize an
    /// `impl FnMut(Vec<u8>)` that is documented to never fire, paying
    /// codegen + inlining cost on the cold path for zero behaviour
    /// difference; the dispatcher in `match_generator.rs` already
    /// branches per backend (matchers diverge on `reset` and a few
    /// other lifecycle calls) so adding one more per-backend arm is
    /// free.
    pub(crate) fn trim_to_window(&mut self) {
        if self.window_size > self.max_window_size || self.history_start != 0 {
            // Any history shift slides the dictionary out of (or within) the
            // concat, staling the dict tables' concat indices — drop the attach.
            self.dict.invalidate();
        }
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

    /// Donor `ZSTD_dictMatchState` attach (dfast): hash the dictionary block at
    /// the front of history into the SEPARATE immutable [`Self::dict`] tables
    /// (long+short), once, instead of re-priming the live tables every frame
    /// (`skip_matching_dense`). The dual-probe kernel then searches live + dict
    /// without per-frame dict cost. Cached via the `DictAttach` primed flag.
    pub(crate) fn skip_matching_for_dict_attach(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            return;
        }
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        // Convert absolute → concat (history-relative) coordinates: the dict
        // table keys off the stable concat index, not `position_base`.
        let start_concat = current_abs_start - self.history_abs_start;
        let end_concat = current_abs_end - self.history_abs_start;
        self.prime_dict_tables_for_range(start_concat, end_concat);
    }

    /// Mark the dict tables fully built (CDict cache). The driver calls this
    /// after the final dictionary chunk so the next frame skips the re-hash.
    ///
    pub(crate) fn mark_dict_primed(&mut self) {
        self.dict.mark_primed();
    }

    /// Drop the cached dict tables (next frame carries no dict, or eviction /
    /// param change staled the concat positions).
    pub(crate) fn invalidate_dict_cache(&mut self) {
        self.dict.invalidate();
    }

    /// Build the immutable dict long+short tables over the contiguous-history
    /// concat range `[start_concat, end_concat)` (the dictionary bytes at the
    /// front of history). Mirrors [`Self::insert_positions`]' hash + lookahead
    /// gating, but writes a +1-biased CONCAT index (`idx + 1`, stable across
    /// `position_base` rebases) into `self.dict` rather than the live,
    /// `position_base`-packed tables. `DFAST_EMPTY_SLOT = 0` means "no entry".
    /// Skips the rehash when the CDict cache is already primed.
    fn prime_dict_tables_for_range(&mut self, start_concat: usize, end_concat: usize) {
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        // Record the dict/input boundary (concat index) regardless of whether
        // any position is hashable (a sub-min-match dict still bounds dict_end).
        self.dict.set_region_len(end_concat);
        if self.dict.is_primed() {
            return;
        }
        let history_start = self.history_start;
        let concat_len = self.history.len() - history_start;
        let long_bits = self.long_hash_bits;
        let short_bits = self.short_hash_bits;
        // Lookahead-safe cutoffs within the concat: long needs 8 readable
        // bytes, short needs 5 (donor `mls = 5` for the short hash).
        let long_safe_end = concat_len.saturating_sub(7).min(end_concat);
        let short_safe_end = concat_len.saturating_sub(4).min(end_concat);
        // The seam window `[backfill_floor, start_concat)` holds positions that
        // only became hashable when THIS chunk extended history (e.g. a `4+1`
        // or `7+1` dict chunking), so gate the early return on `backfill_floor`,
        // not `start_concat` — otherwise those seam inserts are dropped and the
        // attached dict tables stay incomplete.
        let backfill_floor = start_concat.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        if backfill_floor >= short_safe_end {
            return;
        }
        let dict = self.dict.table_mut_or_init(|| DfastDictTables {
            long: alloc::vec![DFAST_EMPTY_SLOT; 1usize << long_bits],
            short: alloc::vec![DFAST_EMPTY_SLOT; 1usize << short_bits],
        });
        let short_shift = 64 - short_bits;
        let long_shift = 64 - long_bits;
        let base = self.history.as_ptr();
        let long_ptr = dict.long.as_mut_ptr();
        let short_ptr = dict.short.as_mut_ptr();
        // SAFETY: `base.add(history_start + pos)` is in-bounds for
        // `pos + 8 <= concat_len` (long) / `pos + 5 <= concat_len` (short, the
        // donor 5-byte key), enforced by the `*_safe_end` cutoffs (the short
        // loop reads a 4-byte word + 1 byte, never past `concat_len`).
        // `*_idx = mixed >> (64 - bits)`
        // has at most `bits` bits set, in-bounds for the `1 << bits` tables.
        // `pos + 1` fits u32: concat indices are bounded by the u32 history
        // gate upstream (`check_stream_abs_headroom`).
        //
        // Backfill the previous chunk's last 7/3 bytes (the seam), which only
        // became hashable now that this chunk extended history — mirrors the
        // dense priming paths' backfill so multi-chunk dictionary priming
        // doesn't drop seam-spanning candidates.
        //
        // `saturating_sub` is a deliberate FLOOR clamp to concat position 0
        // (the dict start), NOT overflow-masking: `start_concat` is a valid
        // concat index, and the seam window `[start_concat - tail, start_concat)`
        // is clamped at 0 because there are no dict bytes before the front.
        // The first chunk (`start_concat == 0`) clamps to 0 → no seam, no-op.
        let mut pos = backfill_floor;
        while pos < long_safe_end {
            unsafe {
                let load_ptr = base.add(history_start + pos);
                let v8 = (load_ptr as *const u64).read_unaligned();
                // Donor 5-byte short hash (ZSTD_hash5 shape): low 5 bytes in
                // the high 40 bits (`v8 << 24`), matching `short_hash_index`.
                let short_idx = ((v8 << 24).wrapping_mul(PRIME) >> short_shift) as usize;
                let long_idx = (v8.wrapping_mul(PRIME) >> long_shift) as usize;
                let packed = (pos as u32) + 1;
                *short_ptr.add(short_idx) = packed;
                *long_ptr.add(long_idx) = packed;
            }
            pos += 1;
        }
        while pos < short_safe_end {
            unsafe {
                let load_ptr = base.add(history_start + pos);
                // 5-byte short key (donor `mls = 5`), assembled from a 4-byte
                // load + 1 byte so it never over-reads the <8-byte tail; the
                // low 5 bytes land in bits 24..63, matching `v8 << 24`.
                let lo4 = (load_ptr as *const u32).read_unaligned() as u64;
                let b5 = *load_ptr.add(4) as u64;
                let v5 = (lo4 | (b5 << 32)) << 24;
                let short_idx = (v5.wrapping_mul(PRIME) >> short_shift) as usize;
                *short_ptr.add(short_idx) = (pos as u32) + 1;
            }
            pos += 1;
        }
    }

    pub(crate) fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        self.ensure_hash_tables();

        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        // Re-seed the previous block's seam. With the donor 5-byte short hash,
        // a position within `mls - 1` bytes of the prior block end could not
        // form its full key when that block was processed (the trailing bytes
        // arrived with THIS block); re-hash that tail now that history spans
        // it, so cross-block matches anchored in the seam are found. Mirrors
        // `skip_matching_dense`'s backfill.
        let backfill_start = current_abs_start
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
            .max(self.history_abs_start);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        // dfast is the donor's greedy double-fast at every level (no lazy
        // variant exists — lazy parsing is the separate `ZSTD_lazy`/`lazy2`
        // strategy), so there is a single match path.
        self.start_matching_fast_loop(current_abs_start, current_len, &mut handle_sequence);
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
        &self,
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

        // SAFETY: `concat_idx0 + 8 <= concat_len` follows from the
        // caller's `ip0 + HASH_READ_SIZE <= current_len` precondition
        // (the live history contains the full current block).
        let v8_0 = unsafe {
            (history_base_ptr.add(history_start_offset + concat_idx0) as *const u64)
                .read_unaligned()
        };
        // `v4_0` (low 4 bytes) is the 4-byte equality-gate key below; the short
        // HASH keys on the donor 5-byte window (`v8_0 << 24`, ZSTD_hash5 shape).
        let v4_0 = v8_0 & 0xFFFF_FFFF;
        let hl0_idx = (v8_0.wrapping_mul(PRIME) >> long_shift) as usize;
        let hs0_idx = ((v8_0 << 24).wrapping_mul(PRIME) >> short_shift) as usize;

        // Read-only on the hash tables here — unlike the inner loop's
        // "update-before-check" pattern, the writes at `hl0_idx` /
        // `hs0_idx` would be dead in this helper:
        //
        //   * On a hit, the caller routes through `emit_candidate`
        //     which insert-positions the entire emitted range, then
        //     either advances `pos` past `current_len - HASH_READ_SIZE`
        //     and the outer guard breaks (so a future iter never
        //     re-uses these slots), or `continue 'outer` re-enters
        //     with `pos = start + match_len ≥ current_len - 3`, which
        //     fails the outer-entry `pos + HASH_READ_SIZE > current_len`
        //     guard immediately. Either way, no second probe sees the
        //     write.
        //   * On no hit, the caller `break 'outer`s directly. Same
        //     conclusion.
        //   * `seed_remaining_hashable_starts` inserts `ip0` itself
        //     during the post-loop tail seed pass, so even the "fresh
        //     entry for next block" rationale doesn't justify writing
        //     here — the seeder does that.
        //
        // Skipping the writes also removes a small amount of cache
        // dirtying on the tail boundary and keeps `probe_tail_ip0_only`
        // strictly cheaper than a full inner-loop iter.
        let idxl0 = unsafe { *self.long_hash.as_ptr().add(hl0_idx) };
        let idxs0 = unsafe { *self.short_hash.as_ptr().add(hs0_idx) };

        // Live tables only — no attached-dict probe here, by design. This
        // helper runs for exactly ONE position per block (the last hashable
        // `ip0` when `ip1` has fallen off the tail), so a missed dict match
        // costs at most one sequence per ~128 KiB block (negligible ratio).
        // `seed_remaining_hashable_starts` inserts this position so it is
        // dict-searchable in the next block; replicating the full dict
        // long+short dual-probe here would duplicate ~40 lines for that single
        // boundary position and defeat the helper's "strictly cheaper than a
        // full inner-loop iter" purpose.

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
            // Cheap 4-byte gate before the SIMD `common_prefix_len`. Read it as
            // one unaligned u32 rather than a slice `!=` (which lowers to a libc
            // `memcmp` CALL). Bounds: `cur_idx + 4 <= len` from the
            // `DFAST_REP_MIN_MATCH_LEN` (= 4) check above, and
            // `cand_idx = cur_idx - rep < cur_idx` so `cand_idx + 4 <= len` too.
            let gate_eq = unsafe {
                concat.as_ptr().add(cur_idx).cast::<u32>().read_unaligned()
                    == concat.as_ptr().add(cand_idx).cast::<u32>().read_unaligned()
            };
            if !gate_eq {
                break;
            }
            let match_len =
                common_prefix_len_with_kernel(self.kernel, &concat[cand_idx..], &concat[cur_idx..]);
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

    /// Per-outer-iteration byte-source descriptor for the fast loop:
    /// `(base_ptr, start_offset, abs_start, position_base, concat_len)`.
    ///
    /// Read once at the top of every outer iteration so the kernel body
    /// is agnostic to where its bytes live: the owned path returns the
    /// `history` concat's (rebased) fields, and a future borrowed
    /// one-shot window will return its own constant descriptor without
    /// the kernel body changing. Returns raw pointer + offsets (no borrow
    /// held), so the subsequent `&mut self` hash-table pointer snapshot in
    /// the loop stays sound.
    #[inline(always)]
    fn scan_source(&self) -> (*const u8, usize, usize, usize, usize) {
        if let Some((ptr, block_end)) = self.borrowed {
            // Borrowed one-shot window: positions are absolute input
            // offsets, so the rebase coordinates collapse to zero
            // (`start_offset = abs_start = position_base = 0`) and the
            // readable length is the active block's end. No history concat,
            // no `commit_space` copy. Candidate reads from earlier blocks
            // land at `ptr + earlier_abs_pos` (< block_end), in range.
            return (ptr, 0, 0, 0, block_end);
        }
        let start_offset = self.history_start;
        (
            self.history.as_ptr(),
            start_offset,
            self.history_abs_start,
            self.position_base,
            self.history.len() - start_offset,
        )
    }

    fn emit_candidate(
        &mut self,
        current_abs_start: usize,
        literals_start: &mut usize,
        candidate: MatchCandidate,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) -> usize {
        // Donor `zstd_double_fast.c` parity: the inner search loop already
        // inserts every position it VISITS (step-accelerated), so the literal
        // run is hashed exactly as densely as the donor's cursor swept it —
        // stepped-over and block-anchor (position 0) positions are NOT
        // re-inserted here (the donor skips them too via `ip += (ip ==
        // prefixStart)` + the growing `step`). Match interior: donor fills only
        // the sparse 3-target set (`curr+2`, `ip-2`, `ip-1`), each clamped to
        // the open match interval.
        let match_start = candidate.start;
        let post_match_end = candidate.start + candidate.match_len;
        let insert_targets = [
            match_start + 2,                  // curr + 2
            post_match_end.saturating_sub(2), // ip - 2 (post-match cursor)
            post_match_end.saturating_sub(1), // ip - 1
        ];
        for &target in &insert_targets {
            if target > match_start && target < post_match_end {
                self.insert_position(target);
            }
        }
        // Inline the trailing-block slice rather than calling
        // `get_last_space()` so this matches the gate pattern used by
        // `skip_matching` / `start_matching` (read `window_blocks.back()`
        // with `unwrap_or(0)`). `emit_candidate` runs only after a
        // successful match was found in the active block, so
        // `last_len > 0` is a structural precondition — the
        // `debug_assert!` makes that precondition fail at the source
        // in tests rather than silently produce an empty slice and
        // panic on the literals subslice below.
        let last_len = self.window_blocks.back().copied().unwrap_or(0);
        debug_assert!(
            last_len > 0,
            "emit_candidate precondition: active block must be non-empty"
        );
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
        let short_safe_end = abs_concat_end.saturating_sub(4).min(end);

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
                // Donor parity (`zstd_compress_internal.h:923-924`):
                // scalar `* prime8bytes` then shift to high bits. Drops
                // the CRC32d-based kernel dispatch (3-4 instructions) for
                // a single mul on the per-byte insert path. Short hash keys
                // on the donor 5-byte window (`v8 << 24`, ZSTD_hash5 shape).
                let mixed_short = (v8 << 24).wrapping_mul(0xCF1BBCDCB7A56463_u64);
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
                // 5-byte short key (donor `mls = 5`): 4-byte load + 1 byte so
                // the <8-byte tail is never over-read; low 5 bytes in bits
                // 24..63 to match `v8 << 24`.
                let lo4 = (load_ptr as *const u32).read_unaligned() as u64;
                let b5 = *load_ptr.add(4) as u64;
                let mixed_short = ((lo4 | (b5 << 32)) << 24).wrapping_mul(0xCF1BBCDCB7A56463_u64);
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
        // Short key needs 5 readable bytes (donor `mls = 5`). A position
        // within 4 bytes of the current history end is not inserted here; the
        // `start_matching` seam re-seed picks it up once the next block extends
        // history far enough to form its full 5-byte key.
        if idx + 5 <= concat_len {
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

    /// 5-byte short-hash index (donor `ZSTD_hashPtr(ip, hBitsS, mls=5)`). A
    /// 4-byte key collides more on repetitive / log-stream data, so the
    /// single-slot table overwrites useful positions the donor's 5-byte key
    /// keeps. `data` MUST hold at least 5 bytes — every call site gates on a
    /// 5-byte lookahead, so no zero-padded synthetic key is ever formed (a
    /// padded short key would populate buckets for starts the donor skips).
    #[inline(always)]
    pub(crate) fn short_hash_index(&self, data: &[u8]) -> usize {
        debug_assert!(data.len() >= 5, "short hash needs a full 5-byte key");
        // Low 5 bytes (ZSTD_hash5 shape) shifted into bits 24..63, matching the
        // raw `v8 << 24` form used by the fast-loop probe / insert sites.
        let lo4 = u32::from_le_bytes(data[..4].try_into().unwrap()) as u64;
        let b5 = data[4] as u64;
        let value = (lo4 | (b5 << 32)) << 24;
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

/// Per-kernel body of the dfast fast match loop. Mirrors the BT/opt
/// `*_body!` macros: the wrapper carries the `#[target_feature]` umbrella and
/// passes its tier `common_prefix_len_ptr` as `$cpl`, so the 7 match-extension
/// cpl calls inline under one umbrella and `select_kernel()` is resolved ONCE
/// per block in the bare dispatcher, never per cpl call.
macro_rules! start_matching_fast_loop_body {
    ($self:ident, $current_abs_start:ident, $current_len:ident, $handle_sequence:ident, $cpl:path) => {{
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
        // `$current_abs_start` arbitrarily far without any per-byte insert,
        // so `position_base` may be stale by `> u32::MAX`. The first
        // fast-loop block after that would silently truncate `packed_curr`
        // and poison both hash tables. The guard band in `ensure_room_for`
        // (`DFAST_REBASE_GUARD_BAND`) covers the entire current block's
        // worth of positions, so a single call at function entry suffices.
        //
        // Raw-pointer aliasing invariant. The inner loop caches
        // `short_hash_ptr = $self.short_hash.as_mut_ptr()` and
        // `long_hash_ptr = $self.long_hash.as_mut_ptr()` (and a
        // `history_base_ptr` for the byte-buffer reads), then over
        // the rest of the function does:
        //
        //   * raw writes / reads via `*short_hash_ptr.add(...) =
        //     packed_curr`, `*long_hash_ptr.add(...) = packed_curr`,
        //     and `*long_hash_ptr.add(hl1_idx)`;
        //   * `&$self`-shared reads of `$self.offset_hist[0]` for the
        //     rep1 peek;
        //   * `&$self`-shared slice reads of `$self.history` (`let
        //     concat = &$self.history[history_start_offset..]`) for
        //     `extend_backwards_shared` invocations.
        //
        // This is sound under both Stacked Borrows and Tree Borrows
        // because the three fields touched (`short_hash`, `long_hash`,
        // `history`, `offset_hist`) are physically disjoint
        // allocations — `Vec<u32>`/`Vec<u8>` each own their own heap
        // buffer, and `offset_hist` is an inline `[u32; 3]` inside
        // the struct. A raw pointer derived from one field's Vec data
        // has provenance over that field only, so the subsequent
        // `&$self` reads of sibling fields don't reborrow through the
        // raw pointer's provenance tree.
        //
        // CRITICAL: this invariant must be preserved by any future
        // refactor that adds method calls inside the loop body. A
        // call that takes `&mut $self` (e.g.,
        // `$self.ensure_hash_tables()`, `$self.insert_position(...)`)
        // would reborrow the tables and invalidate the cached raw
        // pointers — every such call must happen OUTSIDE the
        // `'outer: loop` (as `ensure_room_for` does, hoisted to the
        // function preamble above) or be followed by a fresh
        // `as_mut_ptr()` reload of both `short_hash_ptr` /
        // `long_hash_ptr`. The outer-loop body already re-snapshots
        // `history_base_ptr` per iteration for exactly this reason
        // (`emit_candidate` → `insert_positions` may grow `history`
        // and trigger a realloc), so the same re-snapshot discipline
        // applies to the hash-table pointers.
        //
        // `$current_len > 0` is a precondition of this helper: every
        // caller (`start_matching`, `skip_matching`, `skip_matching_dense`)
        // returns early at `$current_len == 0`. Encode it as a hard
        // assert so a future caller that forgets the gate fails loudly
        // in debug rather than wrap-underflowing the `- 1` below; the
        // rebase is unconditional in release builds since the
        // precondition holds.
        debug_assert!($current_len > 0, "fast_loop precondition: $current_len > 0");
        $self.ensure_room_for($current_abs_start + $current_len - 1);
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        let short_shift = 64 - $self.short_hash_bits;
        let long_shift = 64 - $self.long_hash_bits;
        let mut pos = 1usize;
        let mut literals_start = 0usize;

        // Dict dual-probe (donor `ZSTD_dictMatchState`): snapshot the immutable
        // dict tables ONCE. Unlike the live tables (which `emit_candidate` may
        // grow / rebase), the dict tables are never mutated during matching, so
        // one snapshot before the outer loop stays valid every iteration. The
        // raw pointers hold no borrow, so the per-iter `&$self`/`&mut $self`
        // accesses below coexist. `use_dict` gates every probe so the no-dict
        // hot path keeps its exact instruction shape. `dict_end` is the
        // dict/input boundary as a CONCAT index (history_start-relative); the
        // dict is invalidated on any history eviction, so concat indices stay
        // valid for the snapshot's lifetime.
        // `use_dict` MUST track table presence, NOT `is_attached()`:
        // `prime_dict_tables_for_range` records the dict region (so
        // `is_attached()` is true) but returns before allocating the
        // tables when the hashable region is shorter than the short-hash
        // lookahead. Gating on `table().is_some()` keeps the null dict
        // pointers out of the probe below, which dereferences them before
        // the `dict_end` bound is consulted.
        let (use_dict, dict_long_ptr, dict_short_ptr, dict_end): (
            bool,
            *const u32,
            *const u32,
            usize,
        ) = match $self.dict.table() {
            Some(d) => (
                true,
                d.long.as_ptr(),
                d.short.as_ptr(),
                $self.dict.region_len(),
            ),
            None => (false, core::ptr::null(), core::ptr::null(), 0),
        };

        'outer: loop {
            // Outer-iter precondition: at least `HASH_READ_SIZE = 8` bytes
            // ahead of `pos` so the unconditional 8-byte `u64` load below
            // is in-bounds for the live history buffer. `DFAST_MIN_MATCH_LEN
            // = 5` is the match acceptance threshold and is NOT a safe
            // load bound — using it here read up to 3 bytes past
            // `history.len()` on tiny blocks (CI fuzz `interop` crash,
            // 7-byte input).
            // NOTE: when this guard fires on the very first outer-iter
            // (tiny block, `$current_len < 9`), we `break 'outer` BEFORE
            // `tail_seed_anchor` is even declared. The post-loop
            // `seed_remaining_hashable_starts(.., pos)` then runs with
            // `pos` at its initial value (1 on first frame, or the
            // post-match cursor from a previous outer iter); that's the
            // correct tail seed for tiny blocks — `seed_pos = pos.min(
            // $current_len).min(boundary_tail_start)` plus the
            // `seed_pos + DFAST_SHORT_HASH_LOOKAHEAD <= $current_len`
            // guard inside the seeder keeps every insert in-bounds.
            if pos + HASH_READ_SIZE > $current_len {
                break 'outer;
            }
            let mut step = 1usize;
            let mut next_step_pos = pos + DFAST_SKIP_STEP_GROWTH_INTERVAL;
            let mut ip0 = pos;
            let mut ip1 = ip0 + step;
            // Same `HASH_READ_SIZE` rationale for `ip1`: the inner loop
            // pre-loads 8 bytes at `concat_idx1` for the `hl1` precompute
            // and the `_search_next_long` retry, so `ip1 + 8 <= $current_len`
            // must hold before we enter. If `ip0` is still hashable but
            // `ip1` is not (boundary case `pos == $current_len - 8`), we
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
            if ip1 + HASH_READ_SIZE > $current_len {
                if let Some(committed) =
                    $self.probe_tail_ip0_only($current_abs_start, $current_len, ip0, literals_start)
                {
                    let start = $self.emit_candidate(
                        $current_abs_start,
                        &mut literals_start,
                        committed,
                        $handle_sequence,
                    );
                    pos = start + committed.match_len;
                    pos = $self.extend_with_repcode_after_match(
                        $current_abs_start,
                        $current_len,
                        pos,
                        &mut literals_start,
                        $handle_sequence,
                    );
                    continue 'outer;
                }
                break 'outer;
            }

            // Re-read every per-frame-mutable cursor here — `emit_candidate`
            // in the previous outer iteration may have triggered a rebase.
            // The byte-source quintet comes through `scan_source()` so a
            // borrowed one-shot window can substitute the owned `history`
            // concat without touching this kernel body: the owned path
            // returns its rebased fields, a borrowed window its constant
            // descriptor. The hash-table pointers are mode-invariant (the
            // tables persist across owned/borrowed) so they stay direct.
            let (history_base_ptr, history_start_offset, history_abs_start, position_base, concat_len) =
                $self.scan_source();
            let short_hash_ptr = $self.short_hash.as_mut_ptr();
            let long_hash_ptr = $self.long_hash.as_mut_ptr();

            // Pre-compute long hash at ip0 ONCE per outer iter.
            // `concat_idx = ($current_abs_start + ip0) - history_abs_start`
            // is the byte offset within `live_history`.
            let mut hl0_idx;
            let mut idxl0;
            // SAFETY: `$current_abs_start + ip0 >= history_abs_start`
            // (`ip` is inside the current block, which is part of live
            // history). The 8-byte unaligned load on
            // `concat[concat_idx..]` is safe because the outer loop
            // guards above enforce `ip0 + HASH_READ_SIZE <= $current_len`,
            // and `$current_abs_start + $current_len - history_abs_start
            // <= concat_len` (live history contains the full current
            // block plus any retained earlier blocks), giving
            // `concat_idx + 8 <= concat_len`. The `debug_assert!` below
            // makes that invariant explicit so a future refactor that
            // touches eviction / `compact_history` / `trim_to_window`
            // semantics catches a violation in tests instead of leaking
            // through to ASan UB.
            unsafe {
                let concat_idx = ($current_abs_start + ip0) - history_abs_start;
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
                let abs_ip0 = $current_abs_start + ip0;
                let abs_ip1 = $current_abs_start + ip1;
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
                // `v4_0` (low 4 bytes) is the cheap 4-byte equality-gate key
                // below; the short HASH keys on the donor 5-byte window
                // (`v8_0 << 24`, ZSTD_hash5 shape) to match `short_hash_index`.
                let v4_0 = v8_0 & 0xFFFF_FFFF;
                let hs0_idx = ((v8_0 << 24).wrapping_mul(PRIME) >> short_shift) as usize;
                let idxs0 = unsafe { *short_hash_ptr.add(hs0_idx) };

                // Donor parity (`zstd_double_fast.c:187`): update BOTH
                // tables at curr BEFORE checking matches. The benefit is
                // for hash-table consumers, NOT the rep peek (rep at ip+1
                // reads `offset_hist[0]`, never the hash tables). The
                // donor rationale is the long-hash retry path: the next
                // inner iter's `idxl1` lookup (`hashLong[hl1_idx]`) can
                // collide with this iter's `hl0_idx`, and writing curr
                // first means a $self-collision still resolves to a real
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
                let rep1 = $self.offset_hist[0] as usize;
                if rep1 != 0 && rep1 <= abs_ip1 {
                    let cand_pos_r = abs_ip1 - rep1;
                    if cand_pos_r >= history_abs_start
                        && cand_pos_r >= $current_abs_start + literals_start
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
                                let ext = $cpl(
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
                                let concat = &$self.history[history_start_offset..];
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
                {
                    // Branchless validity (donor `ZSTD_selectAddr`/cmov): fold
                    // the slot-empty / out-of-window / past-cursor checks into a
                    // bitwise `in_long` mask, then mask the candidate index to 0
                    // (history start, always readable) when invalid so the 8-byte
                    // load never faults and is never a real candidate. The only
                    // remaining branch is the rare actual-match `& in_long`,
                    // which is predictable — this removes the per-position
                    // validity mispredict that dominated the hot loop.
                    let cand_pos = position_base.wrapping_add((idxl0 as usize).wrapping_sub(1));
                    let in_long = (idxl0 != DFAST_EMPTY_SLOT)
                        & (cand_pos >= history_abs_start)
                        & (cand_pos < abs_ip0);
                    let cand_idx =
                        cand_pos.wrapping_sub(history_abs_start) & (in_long as usize).wrapping_neg();
                    // SAFETY: `cand_idx` is either a valid in-window concat index
                    // (when `in_long`) or 0; both leave the 8-byte load inside
                    // live history (same buffer/length bounds as `v8_0`).
                    let cand_v8 = unsafe {
                        (history_base_ptr.add(history_start_offset + cand_idx) as *const u64)
                            .read_unaligned()
                    };
                    if cand_v8 == v8_0 && in_long {
                        {
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
                                let ext = $cpl(
                                    lhs, rhs, max_fwd,
                                );
                                match_len += ext;
                            }
                            let concat = &$self.history[history_start_offset..];
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

                // Dict long fallback (donor `dictMatchState`): the live long
                // missed (empty / out-of-window / 8-byte mismatch). Probe the
                // immutable dict long table at the SAME `hl0_idx`. Flat model:
                // the dict sits in the contiguous history before the input, so
                // a dict match is `offset = abs_ip0 - dict_abs` and the forward
                // count crosses the dict→input boundary like any in-window
                // match (no `dictBase`/`count_2segments`).
                if use_dict {
                    // SAFETY: when `use_dict`, `dict_long_ptr` is non-null and
                    // sized `1 << long_hash_bits`; `hl0_idx < 1 << long_hash_bits`.
                    let dl = unsafe { *dict_long_ptr.add(hl0_idx) };
                    if dl != DFAST_EMPTY_SLOT {
                        let dp = (dl as usize) - 1;
                        // Dict long slots were only written for positions with
                        // 8-byte lookahead, so `dp + 8 <= dict_len <= concat_len`;
                        // `dp < dict_end` keeps the match inside the dict region.
                        if dp < dict_end {
                            debug_assert!(
                                dp + HASH_READ_SIZE <= concat_len,
                                "dict long load OOB: dp={dp} concat_len={concat_len}",
                            );
                            // SAFETY: `dp + 8 <= concat_len` (above) ⇒ the 8-byte
                            // load at concat `dp` is in-bounds for live history.
                            let dcand_v8 = unsafe {
                                (history_base_ptr.add(history_start_offset + dp) as *const u64)
                                    .read_unaligned()
                            };
                            if dcand_v8 == v8_0 {
                                let mut match_len = 8usize;
                                let max_fwd = concat_len.saturating_sub(concat_idx0 + 8);
                                // SAFETY: both ptrs in the same buffer; `max_fwd`
                                // caps the scan to the live region.
                                unsafe {
                                    let lhs = history_base_ptr.add(history_start_offset + dp + 8);
                                    let rhs = history_base_ptr
                                        .add(history_start_offset + concat_idx0 + 8);
                                    let ext =
                                        $cpl(
                                            lhs, rhs, max_fwd,
                                        );
                                    match_len += ext;
                                }
                                let cand_pos = history_abs_start + dp;
                                let concat = &$self.history[history_start_offset..];
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
                }

                let idxl1 = unsafe { *long_hash_ptr.add(hl1_idx) };

                // Short match check at ip0 with idxs0 — 4-byte gate
                // ONLY (donor `zstd_double_fast.c:220`). Forward count
                // and `_search_next_long` retry happen ONLY on hit.
                {
                    // Branchless validity (same `ZSTD_selectAddr`/cmov shape as
                    // the long check above): fold into `in_short`, mask the index
                    // to 0 when invalid so the 4-byte load never faults, reject
                    // the dummy read with `& in_short`.
                    let cand_pos_s = position_base.wrapping_add((idxs0 as usize).wrapping_sub(1));
                    let in_short = (idxs0 != DFAST_EMPTY_SLOT)
                        & (cand_pos_s >= history_abs_start)
                        & (cand_pos_s < abs_ip0);
                    let cand_idx_s = cand_pos_s.wrapping_sub(history_abs_start)
                        & (in_short as usize).wrapping_neg();
                    let cand4 = unsafe {
                        (history_base_ptr.add(history_start_offset + cand_idx_s) as *const u32)
                            .read_unaligned()
                    };
                    if cand4 == v4_0 as u32 && in_short {
                        {
                            // Short hit: count forward from byte 4 onwards.
                            let mut s_match_len = 4usize;
                            let max_fwd = concat_len.saturating_sub(concat_idx0 + 4);
                            unsafe {
                                let lhs =
                                    history_base_ptr.add(history_start_offset + cand_idx_s + 4);
                                let rhs =
                                    history_base_ptr.add(history_start_offset + concat_idx0 + 4);
                                let ext = $cpl(
                                    lhs, rhs, max_fwd,
                                );
                                s_match_len += ext;
                            }
                            let concat = &$self.history[history_start_offset..];
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
                            let mut live_l1_hit = false;
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
                                        live_l1_hit = true;
                                        let mut l1_match_len = 8usize;
                                        let max_fwd_l1 = concat_len.saturating_sub(concat_idx1 + 8);
                                        unsafe {
                                            let lhs = history_base_ptr
                                                .add(history_start_offset + cand_idx_l1 + 8);
                                            let rhs = history_base_ptr
                                                .add(history_start_offset + concat_idx1 + 8);
                                            let ext = $cpl(
                                                lhs, rhs, max_fwd_l1,
                                            );
                                            l1_match_len += ext;
                                        }
                                        if l1_match_len > short_cand.match_len {
                                            let concat = &$self.history[history_start_offset..];
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
                            // Dict long match at ip1 (donor `_search_next_long`
                            // dict arm, zstd_double_fast.c:472-483): probed ONLY
                            // when the live long+1 missed, mirroring donor's
                            // `else if dictTagsMatchL3`. Attach-mode keeps the
                            // dict in a SEPARATE table, so the live long+1 probe
                            // above never sees dict positions; without this the
                            // dict-long upgrade the old dense-reprime path got
                            // for free (dict positions lived in the live table)
                            // is lost and the loop emits the shorter short match.
                            if !live_l1_hit && use_dict {
                                // SAFETY: `use_dict` ⇒ `dict_long_ptr` non-null,
                                // sized `1 << long_hash_bits`; `hl1_idx` is in range.
                                let dl1 = unsafe { *dict_long_ptr.add(hl1_idx) };
                                if dl1 != DFAST_EMPTY_SLOT {
                                    let dp1 = (dl1 as usize) - 1;
                                    if dp1 < dict_end {
                                        debug_assert!(
                                            dp1 + HASH_READ_SIZE <= concat_len,
                                            "dict long+1 load OOB: dp1={dp1} concat_len={concat_len}",
                                        );
                                        // SAFETY: `dp1 + 8 <= concat_len` ⇒ the
                                        // 8-byte load at concat `dp1` is in-bounds.
                                        let dcand_v8_l1 = unsafe {
                                            (history_base_ptr.add(history_start_offset + dp1)
                                                as *const u64)
                                                .read_unaligned()
                                        };
                                        if dcand_v8_l1 == v8_1 {
                                            let mut dl1_match_len = 8usize;
                                            let max_fwd =
                                                concat_len.saturating_sub(concat_idx1 + 8);
                                            // SAFETY: same buffer; `max_fwd` caps
                                            // the scan to the live region.
                                            unsafe {
                                                let lhs = history_base_ptr
                                                    .add(history_start_offset + dp1 + 8);
                                                let rhs = history_base_ptr
                                                    .add(history_start_offset + concat_idx1 + 8);
                                                let ext = $cpl(
                                                    lhs, rhs, max_fwd,
                                                );
                                                dl1_match_len += ext;
                                            }
                                            if dl1_match_len > short_cand.match_len {
                                                let cand_pos = history_abs_start + dp1;
                                                let concat = &$self.history[history_start_offset..];
                                                chosen = extend_backwards_shared(
                                                    concat,
                                                    history_abs_start,
                                                    cand_pos,
                                                    abs_ip1,
                                                    dl1_match_len,
                                                    lit_len_ip1,
                                                );
                                                retry_upgraded = true;
                                            }
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

                // Dict short fallback (donor `dictMatchState`): the live short
                // missed / was below floor. Probe the immutable dict short
                // table at the SAME `hs0_idx`, 4-byte gate, forward count, then
                // enforce the same `DFAST_MIN_MATCH_LEN` floor the live short
                // path uses (a sub-floor non-rep match mints a wire offset that
                // costs more than emitting the bytes as literals). No
                // `_search_next_long` retry: the dict long fallback already
                // covers the long-upgrade case at `ip0`.
                if use_dict {
                    // SAFETY: `use_dict` ⇒ `dict_short_ptr` non-null, sized
                    // `1 << short_hash_bits`; `hs0_idx < 1 << short_hash_bits`.
                    let ds = unsafe { *dict_short_ptr.add(hs0_idx) };
                    if ds != DFAST_EMPTY_SLOT {
                        let dp = (ds as usize) - 1;
                        if dp < dict_end {
                            debug_assert!(
                                dp + 4 <= concat_len,
                                "dict short load OOB: dp={dp} concat_len={concat_len}",
                            );
                            // SAFETY: short slots were only written with 4 bytes
                            // of lookahead ⇒ `dp + 4 <= dict_len <= concat_len`.
                            let dcand4 = unsafe {
                                (history_base_ptr.add(history_start_offset + dp) as *const u32)
                                    .read_unaligned()
                            };
                            if dcand4 == v4_0 as u32 {
                                let mut s_match_len = 4usize;
                                let max_fwd = concat_len.saturating_sub(concat_idx0 + 4);
                                unsafe {
                                    let lhs = history_base_ptr.add(history_start_offset + dp + 4);
                                    let rhs = history_base_ptr
                                        .add(history_start_offset + concat_idx0 + 4);
                                    let ext =
                                        $cpl(
                                            lhs, rhs, max_fwd,
                                        );
                                    s_match_len += ext;
                                }
                                let cand_pos = history_abs_start + dp;
                                let concat = &$self.history[history_start_offset..];
                                let dcand = extend_backwards_shared(
                                    concat,
                                    history_abs_start,
                                    cand_pos,
                                    abs_ip0,
                                    s_match_len,
                                    lit_len_ip0,
                                );
                                if dcand.match_len >= DFAST_MIN_MATCH_LEN {
                                    break 'inner InnerExit::Committed(dcand);
                                }
                            }
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
                if ip1 + HASH_READ_SIZE > $current_len {
                    // First position the fast loop did NOT pack into the
                    // hash tables. `seed_remaining_hashable_starts` will
                    // pick up from `ip0` instead of restarting at the
                    // outer-entry `pos`.
                    break 'inner InnerExit::Tail(ip0);
                }
            };

            match inner_exit {
                InnerExit::Committed(candidate) => {
                    let start = $self.emit_candidate(
                        $current_abs_start,
                        &mut literals_start,
                        candidate,
                        $handle_sequence,
                    );
                    pos = start + candidate.match_len;
                    pos = $self.extend_with_repcode_after_match(
                        $current_abs_start,
                        $current_len,
                        pos,
                        &mut literals_start,
                        $handle_sequence,
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

        $self.seed_remaining_hashable_starts($current_abs_start, $current_len, pos);
        $self.emit_trailing_literals(literals_start, $handle_sequence);
    }};
}

impl DfastMatchGenerator {
    /// Dispatcher for the per-kernel dfast fast loop: resolve the tier ONCE
    /// per block via `select_kernel()` and call the matching
    /// `start_matching_fast_loop_<kernel>` wrapper, so every per-position
    /// `common_prefix_len_ptr` inlines under one `#[target_feature]` umbrella
    /// (mirrors `build_optimal_plan_impl`). Replaces the prior per-cpl
    /// `dispatch_common_prefix_len_ptr` runtime dispatch.
    fn start_matching_fast_loop(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.start_matching_fast_loop_neon(current_abs_start, current_len, handle_sequence)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::FastpathKernel;
            // Use the matcher-level cached kernel (resolved once in `new()`),
            // not a per-block `select_kernel()` — the cache only helps if the
            // hot path actually reads it. Mirrors the Row backend.
            match self.kernel {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.start_matching_fast_loop_avx2_bmi2(
                        current_abs_start,
                        current_len,
                        handle_sequence,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.start_matching_fast_loop_sse42(
                        current_abs_start,
                        current_len,
                        handle_sequence,
                    )
                },
                FastpathKernel::Scalar => self.start_matching_fast_loop_scalar(
                    current_abs_start,
                    current_len,
                    handle_sequence,
                ),
            }
        }
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel_simd128"
        ))]
        unsafe {
            self.start_matching_fast_loop_simd128(current_abs_start, current_len, handle_sequence)
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64",
            all(
                target_arch = "wasm32",
                target_feature = "simd128",
                feature = "kernel_simd128"
            )
        )))]
        {
            self.start_matching_fast_loop_scalar(current_abs_start, current_len, handle_sequence)
        }
    }

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    unsafe fn start_matching_fast_loop_neon(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::neon::common_prefix_len_ptr
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    unsafe fn start_matching_fast_loop_sse42(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::sse42::common_prefix_len_ptr
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn start_matching_fast_loop_avx2_bmi2(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr
        )
    }

    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel_simd128"
    ))]
    #[target_feature(enable = "simd128")]
    unsafe fn start_matching_fast_loop_simd128(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::simd128::common_prefix_len_ptr
        )
    }

    #[cfg(not(any(
        all(target_arch = "aarch64", target_endian = "little"),
        all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel_simd128"
        )
    )))]
    #[allow(unused_unsafe)]
    fn start_matching_fast_loop_scalar(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr
        )
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
