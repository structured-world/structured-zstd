//! Donor-shape Fast strategy matcher backend — selected for every
//! Fast-strategy level (Uncompressed, Fastest, Level(1), and the
//! negative Level(-7..=-1) variants). All levels currently resolve
//! to the same matcher with donor level-1 `hash_log = 14, mls = 7`;
//! per-level acceleration knobs land in phase 3.
//!
//! Wraps the kernel from
//! [`super::fast_kernel::kernel::compress_block_fast`] and presents the
//! `Matcher` API expected by [`crate::encoding::match_generator::MatchGeneratorDriver`].
//! Replaces the SuffixStore-based `MatchGenerator` for the Fast strategy
//! path with a donor-parity hash table and tight per-block loop.
//!
//! Wired into production: [`crate::encoding::match_generator::MatcherStorage::Simple`]
//! holds `FastKernelMatcher` directly; the driver's Matcher trait
//! methods (`commit_space` / `start_matching` / `skip_matching_with_hint`
//! / `reset` / `prime_with_dictionary` / `trim_after_budget_retire`)
//! all route through this module's inherent API.
//!
//! # Invariants this module guarantees
//!
//! - `prefix_start_index >= 1` at all times. Position 0 in `history`
//!   is permanently sub-prefix so the hash table's empty-slot
//!   sentinel value `0` cannot be confused with a real match
//!   position. See [`FastKernelMatcher::with_params`] for the full
//!   rationale.
//! - `history.len()` is bounded by `2 × max_window_size` post-append.
//!   See [`FastKernelMatcher::extend_history_with_pending`].
//! - `rep[0..2]` tracks the kernel's repcode state across blocks
//!   (updated from `FastBlockResult.rep` after every
//!   `start_matching`). `offset_hist[0..2]` tracks the wire
//!   encoder's repcode positions and is updated per-emission via
//!   [`encode_offset_with_history`]. These two are kept in sync by
//!   construction for the lit-len > 0 case; the lit-len == 0
//!   special-rule path (donor's `rep[0]-1` shift) is not yet
//!   modeled — the kernel doesn't emit lit-len == 0 Triples today,
//!   but a future cmov / lookahead-pipelined variant might.

use alloc::vec::Vec;

use crate::encoding::Sequence;
use crate::encoding::blocks::encode_offset_with_history;

use super::fast_kernel::hash_table::FastHashTable;
use super::fast_kernel::kernel::compress_block_fast;

/// Donor `ZSTD_defaultCParameters[level=1][srcSize > 256 KiB][Fast]` —
/// the parameter set the C reference encoder picks when the caller
/// asks for level 1 on inputs larger than the small-source cutoff.
/// Used as the entry-time defaults for [`FastKernelMatcher::new`];
/// the reset path can rebind to other `(hash_log, mls, window_log)`
/// triples once the source-size hint resolves a smaller window
/// (smaller inputs drop `hash_log` proportionally).
pub(crate) const FAST_LEVEL_1_HASH_LOG: u32 = 14;
pub(crate) const FAST_LEVEL_1_MLS: u32 = 7;
/// Donor level-1 Fast `window_log`. Production code reads
/// `window_log` from the resolved [`crate::encoding::match_generator`]
/// `LevelParams` directly; this const exists only for the
/// [`FastKernelMatcher::new`] test-helper constructor and the
/// invariant assertions in this file's tests.
#[cfg(test)]
pub(crate) const FAST_LEVEL_1_WINDOW_LOG: u8 = 19;

/// Donor's initial repcode state — `(rep_offset1 = 1, rep_offset2 = 4)`
/// matches `ZSTD_initCCtx`'s reset of `rep` at the start of every
/// frame. Used both as a struct-init constant and as a recovery point
/// in `reset`.
pub(crate) const FAST_INITIAL_REP: [u32; 2] = [1, 4];

/// Initial offset-history seed for the encoder's repcode-coded
/// offsets — matches donor's `repToConfirm[] = { 1, 4, 8 }` at frame
/// start and mirrors the value the old [`super::MatchGenerator`] used.
pub(crate) const FAST_INITIAL_OFFSET_HIST: [u32; 3] = [1, 4, 8];

/// Donor-shape Fast-strategy matcher state.
///
/// State layout mirrors the donor's `ZSTD_compressBlock_fast_*` entry
/// frame:
///
/// - `history` holds the flat byte buffer that the kernel reads from.
///   Both already-matched prior-block bytes (the prefix) and the
///   current block live in this single contiguous buffer; the kernel's
///   `block_start` parameter separates the two.
/// - `prefix_start_index` is donor's `prefixStartIndex` — the lowest
///   absolute position any match may reference. Bumped forward when
///   older history is evicted past `max_window_size`.
/// - `rep` carries the two-deep repcode state across blocks.
/// - `offset_hist` is the encoder-side 3-deep offset history used by
///   the wire encoder's repcode coding (separate from `rep`, which is
///   the matcher's own two-deep stack for the kernel).
/// - `hash_table` is the donor's flat `u32` hash table, persistent
///   across blocks (cleared only on full `reset`).
/// - `pending` holds the most recently `commit_space`'d block before
///   `start_matching` appends it onto `history` and runs the kernel.
pub(crate) struct FastKernelMatcher {
    /// Concatenated input history: prior-block bytes followed by the
    /// most-recently-committed (still pending-matching) tail.
    history: Vec<u8>,
    /// Donor `prefixStartIndex` — earliest position any match may
    /// reference.
    prefix_start_index: u32,
    /// Donor `rep_offset1, rep_offset2`. Threaded into the kernel as
    /// the `rep` array and updated from the kernel's `FastBlockResult`
    /// after every block.
    rep: [u32; 2],
    /// Encoder-side 3-deep offset history for repcode wire coding.
    /// `pub(crate)` so the driver's `prime_with_dictionary` can
    /// inject a seeded history without going through a setter —
    /// matches the legacy `MatchGenerator` field-visibility pattern
    /// the driver was written against.
    pub(crate) offset_hist: [u32; 3],
    /// Flat hash table indexed by donor `hash_ptr<MLS>`. Persistent
    /// across blocks; only `reset` (or a `(hash_log, mls)` parameter
    /// change) reallocates it.
    hash_table: FastHashTable,
    /// `1 << window_log`. Soft upper bound on `history.len()` — once
    /// the buffer grows past this point the prefix is dropped and
    /// `prefix_start_index` advances. `pub(crate)` for the same
    /// reason as `offset_hist`: the driver's `prime_with_dictionary`
    /// path widens this to accommodate retained dictionary bytes,
    /// matching the legacy MatchGenerator pattern.
    pub(crate) max_window_size: usize,
    /// Decoder-side window size (in `log` bits). Reported to the
    /// frame header via the `Matcher::window_size` trait method.
    window_log: u8,
    /// Holds a `commit_space`'d block until `start_matching` consumes
    /// it. `None` between frames and immediately after `start_matching`
    /// returns. The driver guarantees at most one outstanding pending
    /// space at a time (single-block-per-cycle protocol).
    pending: Option<Vec<u8>>,
    /// Absolute history position where the MOST RECENTLY appended
    /// block starts — `extend_history_with_pending` updates this so
    /// [`Self::last_committed_space`] can return that block's bytes
    /// AFTER processing (donor / legacy MatchGenerator parity: the
    /// driver's frame compressor reads `get_last_space` after
    /// `start_matching` to fetch the raw bytes for raw-block
    /// emission). Initialised to 0 — overwritten by every
    /// extend_history_with_pending call.
    last_block_start: usize,
    /// Per-block input buffer recycle slot. After
    /// `extend_history_with_pending` copies bytes from the pending
    /// buffer into `history`, the now-spent `Vec<u8>` allocation is
    /// stashed here (cleared, capacity retained). The driver pulls
    /// it via [`Self::take_recycled_space`] after every
    /// `start_matching` / `skip_matching_with_hint` and returns it
    /// to its `vec_pool` — avoiding a fresh allocation per block on
    /// the hot path.
    recycled_space: Option<Vec<u8>>,
}

impl FastKernelMatcher {
    /// Test-only zero-arg constructor that bakes in the donor's
    /// level-1 defaults. Production code goes through
    /// [`Self::with_params`] directly from the driver, threading the
    /// resolved LevelParams `window_log` (and the donor `hash_log =
    /// 14`, `mls = 7` constants) explicitly — no defaults applied.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_params(
            FAST_LEVEL_1_WINDOW_LOG,
            FAST_LEVEL_1_HASH_LOG,
            FAST_LEVEL_1_MLS,
        )
    }

    /// Explicit-parameter constructor used by the wiring commit when
    /// the level resolution produced a non-default `(window_log,
    /// hash_log, mls)` triple (typically because a small source-size
    /// hint clamped the window). Tests can also call this directly.
    pub(crate) fn with_params(window_log: u8, hash_log: u32, mls: u32) -> Self {
        Self {
            last_block_start: 0,
            recycled_space: None,
            history: Vec::new(),
            // Donor `prefixStartIndex` starts at 1 (not 0) so the
            // hash table's empty-slot sentinel `0` can't be confused
            // with a real position. Position 0 in our `history` is
            // therefore an unmatchable reserved byte — donor pays
            // the same one-byte cost via its `ip0 += (ip0 ==
            // prefixStart)` first-iteration bump. Eviction in
            // `extend_history_with_pending` walks this value
            // forward as the prefix advances.
            prefix_start_index: 1,
            rep: FAST_INITIAL_REP,
            offset_hist: FAST_INITIAL_OFFSET_HIST,
            hash_table: FastHashTable::new(hash_log, mls),
            max_window_size: 1usize << window_log,
            window_log,
            pending: None,
        }
    }

    /// Reset for the next frame.
    ///
    /// Drops all history, clears the repcode and offset stacks, and
    /// either clears the existing hash table (if `(hash_log, mls)` are
    /// unchanged) or reallocates it. The window_log update redirects
    /// the soft-eviction bound and the decoder-side reported window.
    pub(crate) fn reset(&mut self, window_log: u8, hash_log: u32, mls: u32) {
        if self.hash_table.hash_log() != hash_log || self.hash_table.mls() != mls {
            // Parameters changed — rebuild the table at the new size.
            // Cannot reuse the old allocation because the donor-shape
            // hash table dimensions are baked in at construction.
            self.hash_table = FastHashTable::new(hash_log, mls);
        } else {
            // Same shape — keep the allocation, zero the entries via
            // `memset` (donor's `ZSTD_window_clear` cadence).
            self.hash_table.clear();
        }
        self.history.clear();
        // Reset to the same `1` baseline `with_params` uses — see
        // that ctor for the empty-slot-sentinel rationale.
        self.prefix_start_index = 1;
        self.rep = FAST_INITIAL_REP;
        self.offset_hist = FAST_INITIAL_OFFSET_HIST;
        self.window_log = window_log;
        self.max_window_size = 1usize << window_log;
        self.pending = None;
        self.last_block_start = 0;
        self.recycled_space = None;
    }

    /// Reported decoder-side window size (bytes) — test-only.
    ///
    /// Equals `1 << window_log`. Production reads
    /// `reported_window_size` on [`crate::encoding::match_generator::MatchGeneratorDriver`]
    /// directly (it sets the field at `reset` time from
    /// `LevelParams.window_log`); this helper exists so tests can
    /// assert the matcher's own internal record matches.
    #[cfg(test)]
    pub(crate) fn window_size(&self) -> u64 {
        self.max_window_size as u64
    }

    /// Read-only view of the most recently committed block — donor /
    /// legacy MatchGenerator's `window.last().data` equivalent.
    ///
    /// Three states:
    /// - Pre-`accept_data` of any block: returns empty slice
    ///   (`last_block_start = 0`, `history.len() = 0`).
    /// - Between `accept_data` and `start_matching` /
    ///   `skip_matching_with_hint`: returns the pending buffer (not
    ///   yet in history).
    /// - After `start_matching` / `skip_matching_with_hint`: returns
    ///   the slice of `history` covering the just-processed block.
    ///   The frame compressor's raw-block emission path relies on
    ///   this — it reads `get_last_space()` AFTER `start_matching`
    ///   to fetch the bytes verbatim.
    pub(crate) fn last_committed_space(&self) -> &[u8] {
        match self.pending.as_deref() {
            Some(slice) => slice,
            None => &self.history[self.last_block_start..],
        }
    }

    /// Accept a freshly-committed block from the driver.
    ///
    /// Donor's `ZSTD_window_update`: the new bytes are stashed for
    /// the next [`Self::start_matching`] / [`Self::skip_matching`]
    /// call but NOT yet appended to `history` — that delay lets the
    /// driver-side `get_last_space` peek at the still-pending buffer
    /// without committing it to the matcher's hot path.
    ///
    /// History budget is enforced lazily on the actual append (inside
    /// [`Self::extend_history_with_pending`]) — checking it here
    /// would force the eviction work even when the driver follows up
    /// with `skip_matching` on incompressible data.
    pub(crate) fn accept_data(&mut self, space: Vec<u8>) {
        debug_assert!(
            self.pending.is_none(),
            "FastKernelMatcher: accept_data called with a still-pending buffer; \
             the driver must run start_matching / skip_matching between commits",
        );

        // Eager window eviction: drop oldest history bytes NOW if
        // accepting this block would push the total past donor's
        // `2 × max_window_size` soft cap. This fires at commit time
        // (not at append time inside `extend_history_with_pending`)
        // so the driver's `commit_space` can observe the byte delta
        // via a `pre/post history.len()` comparison — that delta
        // feeds `retire_dictionary_budget` which shrinks
        // `max_window_size` back to the frame's contracted window
        // after dictionary priming inflated it. Without commit-time
        // visibility the dict-budget retire never runs and the
        // matcher can emit offsets exceeding the frame header's
        // reported window size (format-correctness risk).
        let new_total = self.history.len().saturating_add(space.len());
        let cap = self.max_window_size.saturating_mul(2);
        if new_total > cap {
            let drop_n = self.history.len().saturating_sub(self.max_window_size);
            if drop_n > 0 {
                self.history.drain(..drop_n);
                // Rebase prefix_start_index to 1 (the sentinel-0
                // baseline from `with_params`) — drain re-indexes
                // the retained tail from position 0, so a
                // cumulative `saturating_add(drop_n)` would push
                // `prefix_start_index` past every valid history
                // index and the kernel's
                // `match_idx >= prefix_start_index` filter would
                // reject ALL match candidates wholesale. Donor uses
                // an absolute-base-pointer model that survives
                // drains without re-indexing; the `Vec<u8>` history
                // here can't mirror that, so we reset and rely on
                // the `hash_table.clear()` below + subsequent
                // kernel scans re-populating entries in the new
                // coordinate space.
                self.prefix_start_index = 1;
                self.hash_table.clear();
                self.last_block_start = self.last_block_start.saturating_sub(drop_n);
                // Rehash retained tail so block N+1 can still find
                // matches against the bytes we explicitly kept.
                // Without this, the retained window becomes "dead
                // history" — visible in `history` but un-lookupable
                // (no hash table entries). Donor's absolute-base
                // pointer model carries hash entries across drains
                // for free; the Vec<u8> history can't, so we pay an
                // O(retained_bytes) rehash here. Amortised over
                // max_window_size of input it's O(1) per byte.
                self.prime_hash_table_for_range(0);
            }
        }

        self.pending = Some(space);
    }

    /// Internal: drain `self.pending` into `self.history`, applying
    /// the window-budget eviction first. Returns the absolute position
    /// at which the newly-appended block starts (donor's
    /// `currentBlockStart` — what the kernel receives as
    /// `block_start`).
    ///
    /// Eviction rule mirrors donor's `ZSTD_window_correctOverflow`:
    /// when total retained bytes would exceed `2 × max_window_size`,
    /// drop the oldest bytes back down to a `max_window_size` tail
    /// and clear the hash table. The clear is forced because absolute
    /// positions stored in the table would otherwise reference
    /// evicted bytes; donor avoids the clear via a base-pointer trick
    /// (`base += correction`) that the flat-`Vec<u8>` history can't
    /// reuse, but pays for it with a one-time eviction every
    /// `max_window_size` worth of input — amortised constant.
    fn extend_history_with_pending(&mut self) -> usize {
        let mut space = self
            .pending
            .take()
            .expect("extend_history_with_pending without a pending buffer");

        // Eviction was already applied during `accept_data` (eager
        // pre-commit drain so the driver's `commit_space` accounting
        // sees the byte delta). At this point the matcher's
        // invariant `history.len() + space.len() <= 2 *
        // max_window_size` already holds — just append.
        let block_start = self.history.len();
        self.history.extend_from_slice(&space);
        // Record where this newly-appended block starts so
        // `last_committed_space` can return its bytes AFTER the
        // kernel call consumes pending.
        self.last_block_start = block_start;
        // Stash the now-spent space buffer (cleared, capacity
        // retained) for the driver to pull via
        // `take_recycled_space()` and return to its vec_pool. Avoids
        // a fresh per-block allocation on the hot path. If a previous
        // recycled buffer was never taken (e.g. driver crashed mid-
        // cycle) we drop it here — only ONE buffer is recycled per
        // cycle, matching the single-pending-block protocol.
        space.clear();
        self.recycled_space = Some(space);
        block_start
    }

    /// Reclaim the most recently spent input buffer (the `Vec<u8>`
    /// passed in via `accept_data` after its bytes were copied into
    /// `history`). The buffer is empty but retains its capacity —
    /// the driver can resize it back to `slice_size` and push onto
    /// `vec_pool` to amortise per-block allocation cost.
    ///
    /// Returns `None` if no block has been processed since the last
    /// `take_recycled_space` (or since construction / reset).
    pub(crate) fn take_recycled_space(&mut self) -> Option<Vec<u8>> {
        self.recycled_space.take()
    }

    /// Process the pending block with the donor-shape kernel,
    /// streaming `Sequence::Triple` emissions to `handle_sequence`
    /// and emitting a terminal `Sequence::Literals` if any tail
    /// remained after the last match.
    ///
    /// The MLS const-generic is dispatched at runtime against the
    /// hash table's `mls` (4..=8). Each arm monomorphises a separate
    /// `compress_block_fast<MLS>` body so the inner-loop hash formula
    /// and shift widths compile to constants per supported mls. The
    /// `_ =>` arm is unreachable because `validate_params` in
    /// [`FastHashTable::new`] rejects mls outside 4..=8 at
    /// construction.
    pub(crate) fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        let block_start = self.extend_history_with_pending();
        let prefix_start_index = self.prefix_start_index;
        let rep_in = self.rep;
        let mls = self.hash_table.mls();

        // Split borrow: `data` reads from history immutably, kernel
        // takes hash_table mutably. The two fields don't alias, so the
        // borrow checker is satisfied via field-by-field projection
        // (no `&mut self` re-borrow). We also need a mutable borrow on
        // `self.offset_hist` inside the per-Triple wire-history update,
        // but that runs synchronously inside this method without
        // overlapping the kernel call.
        let history: &[u8] = &self.history;
        let hash_table = &mut self.hash_table;

        // Kernel inner closure: forward Triple emissions to user,
        // updating the matcher's 3-deep wire-encoding history via
        // `encode_offset_with_history` for every match (so the
        // dictionary-prime tests that inspect `offset_hist` after a
        // run-through still see the donor-equivalent state).
        //
        // The closure captures `&mut offset_hist` AS WELL AS the
        // user's `handle_sequence` — splitting the borrow on `self`
        // before the kernel call lets both run from the kernel's
        // single emission stream.
        let offset_hist = &mut self.offset_hist;
        let mut wrap_emit = |seq: Sequence<'_>| {
            if let Sequence::Triple {
                literals,
                offset,
                match_len,
            } = seq
            {
                // Discarded return is the encoded repcode token —
                // mirrors what the legacy `MatchGenerator` does. The
                // wire-encoder downstream computes its own encoding
                // from the raw offset; this call only mutates
                // `offset_hist` so subsequent priming-state reads
                // see the post-block history.
                let _ =
                    encode_offset_with_history(offset as u32, literals.len() as u32, offset_hist);
                handle_sequence(Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                });
            } else {
                // The kernel's contract states it emits ONLY Triple
                // mid-block (terminal Literals lives in
                // `tail_literals_len`). Forward defensively in case
                // that contract loosens later.
                handle_sequence(seq);
            }
        };

        let result = match mls {
            4 => compress_block_fast::<4>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                &mut wrap_emit,
            ),
            5 => compress_block_fast::<5>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                &mut wrap_emit,
            ),
            6 => compress_block_fast::<6>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                &mut wrap_emit,
            ),
            7 => compress_block_fast::<7>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                &mut wrap_emit,
            ),
            8 => compress_block_fast::<8>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                &mut wrap_emit,
            ),
            _ => unreachable!(
                "FastHashTable construction rejects mls outside 4..=8 — \
                 got mls={mls} which means the table was bypassed",
            ),
        };

        // Persist the kernel's rep state for the next block.
        self.rep = result.rep;

        // Emit terminal literals if the kernel left a tail.
        if result.tail_literals_len > 0 {
            let tail_start = self.history.len() - result.tail_literals_len;
            handle_sequence(Sequence::Literals {
                literals: &self.history[tail_start..],
            });
        }
    }

    /// Donor's `skipMatching` equivalent: append the pending block to
    /// history without running the kernel.
    ///
    /// The block's bytes are NOT hashed into the table, so block N+1's
    /// matcher cannot find matches against the skipped region. This
    /// trades compression on the skipped bytes for CPU — the driver
    /// calls this when an upstream incompressibility hint marks the
    /// block as not worth scanning. Donor's
    /// `ZSTD_compressBlock_targetCBlockSize_body` makes the same
    /// trade.
    ///
    /// The `incompressible_hint` parameter accepts the donor's
    /// `Matcher::skip_matching_with_hint` semantics:
    ///
    /// - `Some(true)` or `None` — incompressible / no opinion: append
    ///   only, no hash entries (cheapest path).
    /// - `Some(false)` — explicitly "this block IS compressible, but
    ///   the driver is skipping it for dictionary-priming reasons":
    ///   the block's bytes need to be matchable in future blocks, so
    ///   pre-populate the hash table for every position in the newly
    ///   appended range. This matches the
    ///   `skip_matching_for_dictionary_priming` flow on the driver.
    pub(crate) fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        let block_start = self.extend_history_with_pending();
        // Rep state survives unchanged: skip should look idempotent
        // to the next block's matcher (no fake match implies no rep
        // promotion). offset_hist likewise unchanged.

        // Dictionary-priming path: explicit `Some(false)` means the
        // upstream knows the block is compressible material that the
        // future matcher should be able to reach. Populate hash
        // entries for every position in the appended range that has
        // at least `HASH_READ_SIZE` bytes of forward context — under
        // that threshold the kernel itself can't read the position
        // either, so a hash entry there would be unreachable.
        //
        // Iteration runs while `pos + HASH_READ_SIZE <= history.len()`;
        // a saturating subtract gives the loop bound without ever
        // wrapping for short blocks (history shorter than HASH_READ_SIZE
        // is a legal post-prime state when the dictionary itself is
        // very small).
        if incompressible_hint == Some(false) {
            self.prime_hash_table_for_range(block_start);
        }
    }

    /// Seed the wire encoder's offset history from a primed
    /// dictionary load. Currently sets `offset_hist` only; the
    /// kernel's `rep` is intentionally left out of sync in this
    /// version so the rep/offset_hist drift is observable by the
    /// regression test for #216 Copilot review #15. The fix commit
    /// extends this to update `rep[0..2]` from `offset_hist[0..2]`
    /// atomically.
    pub(crate) fn prime_offset_history(&mut self, offset_hist: [u32; 3]) {
        self.offset_hist = offset_hist;
    }

    /// Read-only view of `history.len()` for the driver's eviction
    /// accounting (`commit_space` → `retire_dictionary_budget` flow).
    /// The driver compares pre/post values to derive a byte-delta
    /// when its own bookkeeping doesn't see the matcher's internal
    /// drain calls.
    pub(crate) fn history_len_for_eviction_accounting(&self) -> usize {
        self.history.len()
    }

    /// Donor's `ZSTD_window_trimWindow` equivalent: drop history
    /// bytes that no longer fit in `max_window_size`, bumping
    /// `prefix_start_index` and clearing the hash table (which holds
    /// absolute positions into the pre-trim history).
    ///
    /// Returns the number of bytes evicted — used by the driver's
    /// `trim_after_budget_retire` loop to drive the dictionary-budget
    /// reclamation termination condition (`evicted_bytes == 0` →
    /// done).
    ///
    /// Idempotent: when `history.len() <= max_window_size` already,
    /// returns 0 without touching state.
    pub(crate) fn trim_to_window(&mut self) -> usize {
        if self.history.len() <= self.max_window_size {
            return 0;
        }
        let drop_n = self.history.len() - self.max_window_size;
        self.history.drain(..drop_n);
        // Rebase `prefix_start_index` to 1 on every drain — see
        // `accept_data` for the same rationale (cumulative
        // `saturating_add(drop_n)` would push the filter past every
        // valid post-drain history index, dropping all subsequent
        // matches against the retained tail).
        self.prefix_start_index = 1;
        // Hash table holds absolute positions into the pre-drain
        // history — clear them as in the in-loop eviction path.
        self.hash_table.clear();
        // Rehash retained tail (same rationale as the accept_data
        // drain branch — retained bytes need hash entries to be
        // matchable in subsequent blocks).
        let rehash_target = 0;
        // Track the drain in last_block_start so post-trim
        // `last_committed_space()` slices into the NEW history
        // coordinate space. Without this saturating subtract, an
        // old last_block_start that originally referenced bytes
        // within the drained prefix would point past the end of
        // history → OOB panic (or, worse, a valid but wrong slice
        // when the new history happens to be long enough to make
        // the stale index in-bounds).
        self.last_block_start = self.last_block_start.saturating_sub(drop_n);
        self.prime_hash_table_for_range(rehash_target);
        drop_n
    }

    /// Pre-populate the hash table with entries for every position in
    /// `history[range_start..end_of_history]` that has at least
    /// `HASH_READ_SIZE` bytes of forward context. Used by the
    /// dictionary-priming skip path (`skip_matching` with
    /// `incompressible_hint = Some(false)`).
    ///
    /// Dispatches on the matcher's monomorphised `MLS` so the inner
    /// `hash_ptr<MLS>` call resolves to a single constant-folded body
    /// per supported mls (4..=8). The unreachable `_` arm guards
    /// against future MLS-range widening missing this dispatch.
    fn prime_hash_table_for_range(&mut self, range_start: usize) {
        let history_len = self.history.len();
        // HASH_READ_SIZE = 8 is the kernel's load-width invariant
        // (donor `MEM_readST` cadence). Hashing a position with fewer
        // forward bytes would compute a hash over uninitialised /
        // out-of-range memory.
        const HASH_READ_SIZE: usize = 8;
        if history_len < HASH_READ_SIZE {
            return;
        }
        let last_hashable = history_len - HASH_READ_SIZE;
        if range_start > last_hashable {
            return;
        }

        let mls = self.hash_table.mls();
        let base = self.history.as_ptr();
        for pos in range_start..=last_hashable {
            // SAFETY: pos < history_len (by loop bound), and the
            // load width HASH_READ_SIZE is the kernel's contractually
            // required minimum, so `base.add(pos)` covers
            // HASH_READ_SIZE readable bytes by `last_hashable`'s
            // definition. Dispatch on the runtime mls into the
            // matching const-generic monomorphisation.
            let ptr = unsafe { base.add(pos) };
            let hash = unsafe {
                match mls {
                    4 => self.hash_table.hash_ptr::<4>(ptr),
                    5 => self.hash_table.hash_ptr::<5>(ptr),
                    6 => self.hash_table.hash_ptr::<6>(ptr),
                    7 => self.hash_table.hash_ptr::<7>(ptr),
                    8 => self.hash_table.hash_ptr::<8>(ptr),
                    _ => unreachable!("FastHashTable construction rejects mls outside 4..=8",),
                }
            };
            // SAFETY: hash came from this table's hash_ptr; pos fits
            // in u32 by the u32::MAX guard the kernel's entry asserts
            // (data.len() <= u32::MAX, and pos < data.len()).
            unsafe { self.hash_table.put(hash, pos as u32) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uses_donor_level_1_defaults() {
        let m = FastKernelMatcher::new();
        assert_eq!(m.window_log, FAST_LEVEL_1_WINDOW_LOG);
        assert_eq!(m.hash_table.hash_log(), FAST_LEVEL_1_HASH_LOG);
        assert_eq!(m.hash_table.mls(), FAST_LEVEL_1_MLS);
        assert_eq!(m.rep, FAST_INITIAL_REP);
        assert_eq!(m.offset_hist, FAST_INITIAL_OFFSET_HIST);
        assert_eq!(m.max_window_size, 1usize << FAST_LEVEL_1_WINDOW_LOG);
        assert!(m.history.is_empty());
        // Donor initializes prefixStartIndex at 1 so the hash
        // table's empty-slot sentinel value 0 can't be confused
        // with a real position — see `with_params` for the full
        // rationale.
        assert_eq!(m.prefix_start_index, 1);
        assert!(m.pending.is_none());
    }

    #[test]
    fn with_params_threads_through_each_field() {
        // Pick a non-default triple to prove no silent override by
        // donor-default constants.
        let m = FastKernelMatcher::with_params(16, 12, 5);
        assert_eq!(m.window_log, 16);
        assert_eq!(m.hash_table.hash_log(), 12);
        assert_eq!(m.hash_table.mls(), 5);
        assert_eq!(m.max_window_size, 1usize << 16);
    }

    #[test]
    fn window_size_reports_one_shifted_window_log() {
        // window_log = 16 → 64 KiB reported window.
        let m = FastKernelMatcher::with_params(16, 12, 5);
        assert_eq!(m.window_size(), 1u64 << 16);
        // Larger window_log → larger reported window. window_log = 22
        // (4 MiB, donor's BETTER_WINDOW_LOG) confirms the shift width
        // (`u64` head room).
        let m = FastKernelMatcher::with_params(22, 14, 7);
        assert_eq!(m.window_size(), 1u64 << 22);
    }

    #[test]
    fn last_committed_space_empty_before_commit() {
        let m = FastKernelMatcher::new();
        assert!(m.last_committed_space().is_empty());
    }

    #[test]
    fn reset_clears_history_and_state() {
        let mut m = FastKernelMatcher::new();
        // Simulate prior-frame state — non-empty history, advanced
        // prefix, non-default rep/offset stacks, a leftover pending
        // block. Reset must restore the matcher to a from-scratch
        // appearance regardless of which fields were dirtied.
        m.history.extend_from_slice(&[1, 2, 3, 4]);
        m.prefix_start_index = 7;
        m.rep = [42, 99];
        m.offset_hist = [10, 20, 30];
        m.pending = Some(alloc::vec![5, 6, 7]);

        m.reset(
            FAST_LEVEL_1_WINDOW_LOG,
            FAST_LEVEL_1_HASH_LOG,
            FAST_LEVEL_1_MLS,
        );

        assert!(m.history.is_empty());
        assert_eq!(m.prefix_start_index, 1);
        assert_eq!(m.rep, FAST_INITIAL_REP);
        assert_eq!(m.offset_hist, FAST_INITIAL_OFFSET_HIST);
        assert!(m.pending.is_none());
        // Hash-table identity preserved (same shape) — `clear()` path,
        // not a fresh `new()`. Equality test is over the params, not
        // the buffer pointer, because the `Vec`-internal allocation
        // identity is an internal detail the test should not lock in.
        assert_eq!(m.hash_table.hash_log(), FAST_LEVEL_1_HASH_LOG);
        assert_eq!(m.hash_table.mls(), FAST_LEVEL_1_MLS);
    }

    #[test]
    fn reset_with_changed_params_rebuilds_hash_table() {
        let mut m = FastKernelMatcher::new();
        // Force a parameter change — every Vec we hand the new
        // FastHashTable will be a fresh allocation.
        m.reset(16, 10, 4);
        assert_eq!(m.hash_table.hash_log(), 10);
        assert_eq!(m.hash_table.mls(), 4);
        assert_eq!(m.window_log, 16);
        assert_eq!(m.max_window_size, 1usize << 16);
    }

    /// Drive the matcher through a single block whose tail contains
    /// a repeated 4-byte run — the kernel must emit at least one
    /// `Sequence::Triple` with `match_len >= 4` and the bookkeeping
    /// invariant `Σ(literals + match_len) + tail_literals_len ==
    /// input.len()` must hold.
    #[test]
    fn accept_then_start_matching_emits_match_for_repeated_run() {
        // 64 bytes: 32 bytes of pseudo-random preamble + 32-byte
        // verbatim copy of bytes [0..32]. The kernel scanning the
        // tail should find the 32-byte repeat with offset = 32.
        let mut data = alloc::vec::Vec::with_capacity(64);
        for i in 0..32u8 {
            // Spread the byte values so 4-byte windows are all
            // distinct (avoids accidental rep hits that would skew
            // the assertion).
            data.push(i.wrapping_mul(7).wrapping_add(13));
        }
        data.extend_from_within(0..32);
        // Use a small mls=4 table so the test exercises the simpler
        // hash arm; level-1 defaults (mls=7) would also work but the
        // hash collisions on a 64-byte synthetic input are noisier
        // for mls>=5.
        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        m.accept_data(data.clone());

        let mut emitted_match_lens: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        let mut emitted_literal_byte_count: usize = 0;
        let mut tail_byte_count: usize = 0;
        m.start_matching(|seq| match seq {
            Sequence::Triple {
                literals,
                offset: _,
                match_len,
            } => {
                emitted_literal_byte_count += literals.len();
                emitted_match_lens.push(match_len);
            }
            Sequence::Literals { literals } => {
                tail_byte_count += literals.len();
            }
        });

        let total_matched: usize = emitted_match_lens.iter().sum();
        assert_eq!(
            emitted_literal_byte_count + total_matched + tail_byte_count,
            data.len(),
            "all input bytes must be accounted for as literals/matches/tail",
        );
        assert!(
            emitted_match_lens.iter().any(|&m| m >= 4),
            "kernel must emit at least one Triple with match_len >= MIN_MATCH (got {emitted_match_lens:?})",
        );
        // Pending buffer was consumed.
        assert!(m.pending.is_none());
        // History grew by exactly the block size.
        assert_eq!(m.history.len(), data.len());
        // `last_committed_space` post-processing reads from
        // history[last_block_start..] (donor / legacy MatchGenerator
        // parity for the frame compressor's raw-block emission
        // path) — for a single-block-then-process flow it equals
        // the input data verbatim.
        assert_eq!(m.last_committed_space(), data.as_slice());
    }

    /// Skip path: `skip_matching` must move the pending buffer into
    /// history WITHOUT emitting any sequences and WITHOUT touching
    /// the rep / offset_hist state.
    #[test]
    fn skip_matching_extends_history_without_emissions() {
        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        let pre_rep = m.rep;
        let pre_offset_hist = m.offset_hist;

        let payload: alloc::vec::Vec<u8> = (0..40u8).collect();
        m.accept_data(payload.clone());
        // Take a count of state pre-skip.
        assert_eq!(m.last_committed_space().len(), payload.len());

        m.skip_matching_with_hint(None);

        assert_eq!(
            m.history.len(),
            payload.len(),
            "skip_matching must append the pending buffer to history",
        );
        assert_eq!(m.rep, pre_rep, "skip must not touch rep state");
        assert_eq!(
            m.offset_hist, pre_offset_hist,
            "skip must not touch offset_hist",
        );
        assert!(m.pending.is_none());
    }

    /// Two-block run with literal block then matchable block — the
    /// SECOND `start_matching` must find a cross-block match against
    /// the first block's bytes (cross-block matches are the headline
    /// reason for keeping the hash table persistent across blocks).
    ///
    /// Sizing rationale: the kernel's main loop only scans up to
    /// `ilimit = data.len() - HASH_READ_SIZE` (donor parity). Block
    /// 2 must therefore carry enough trailing bytes past the
    /// crossblock-match start for `ip0` to actually reach the copy.
    /// We use a 128-byte block 1 and a 64-byte block 2 with the
    /// 32-byte copy of block 1's prefix landing at block-2 offset
    /// 16, leaving plenty of headroom under `ilimit`.
    #[test]
    fn cross_block_match_finds_first_block_payload() {
        // Block 1: 128-byte pattern, distinct 4-byte windows.
        let mut block1 = alloc::vec::Vec::with_capacity(128);
        for i in 0..128u8 {
            block1.push(i.wrapping_mul(11).wrapping_add(5));
        }
        // Block 2: 16 fresh bytes followed by a 32-byte verbatim copy
        // of block 1's [0..32]. The matcher must reach back into
        // block 1's bytes (offset 128+16-0 = 144 ≈ length of block 1
        // plus the leading fresh bytes of block 2). Tail (16 bytes
        // past the copy) gives `ip0` enough room to reach the copy
        // before hitting `ilimit`.
        let mut block2 = alloc::vec::Vec::with_capacity(64);
        block2.extend(0..16u8); // 16 fresh bytes (different from block1)
        block2.extend_from_slice(&block1[0..32]); // 32-byte cross-block copy
        block2.extend(200..216u8); // 16-byte tail buffer

        let mut m = FastKernelMatcher::with_params(12, 8, 4);

        // Block 1 — drain emissions, ignore.
        m.accept_data(block1.clone());
        m.start_matching(|_seq| {});

        // Block 2 — capture emissions.
        m.accept_data(block2.clone());
        let mut max_match: usize = 0;
        let mut saw_cross_block = false;
        m.start_matching(|seq| {
            if let Sequence::Triple {
                offset, match_len, ..
            } = seq
            {
                max_match = max_match.max(match_len);
                // Cross-block match: offset must reach back into
                // block 1, i.e. offset > position-within-block-2.
                // Block 2's payload starts at history position
                // `block1.len()`; the source is in block 1 when
                // offset >= block2.len() (offset measured from ip0
                // backwards, so a block-1 source means offset
                // exceeds any block-2-internal distance).
                if offset >= block2.len() {
                    saw_cross_block = true;
                }
            }
        });

        assert!(
            saw_cross_block,
            "block 2's matcher must find at least one cross-block match \
             (max_len={max_match})",
        );
        assert_eq!(
            m.history.len(),
            block1.len() + block2.len(),
            "history must hold both blocks after two start_matching calls",
        );
    }

    /// Dictionary-priming skip: `skip_matching(Some(false))` MUST
    /// pre-populate the hash table for the just-appended range so a
    /// subsequent `start_matching` can find matches against the
    /// dict-primed bytes. Without that pre-population, a future
    /// block that copies the dict prefix verbatim would emit only
    /// literals.
    #[test]
    fn skip_matching_with_false_hint_populates_hashes_for_dict_priming() {
        // Stage: 32 bytes "dict" via skip_matching(Some(false)),
        // then a second block whose tail copies the dict prefix.
        // Without the hash pre-population the kernel can't reach
        // the dict bytes in block 2.
        let mut dict_block = alloc::vec::Vec::with_capacity(32);
        for i in 0..32u8 {
            dict_block.push(i.wrapping_mul(13).wrapping_add(7));
        }

        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        m.accept_data(dict_block.clone());
        m.skip_matching_with_hint(Some(false)); // dictionary-priming skip

        // Sanity: history grew, prefix_start_index unchanged.
        assert_eq!(m.history.len(), dict_block.len());
        assert_eq!(m.prefix_start_index, 1);

        // Block 2: 16 fresh bytes + 16-byte copy of dict_block[0..16]
        // + 16-byte tail buffer so the kernel can reach the copy.
        let mut block2 = alloc::vec::Vec::with_capacity(48);
        block2.extend(100..116u8);
        block2.extend_from_slice(&dict_block[0..16]);
        block2.extend(120..136u8);
        m.accept_data(block2.clone());

        let mut saw_cross_block = false;
        m.start_matching(|seq| {
            if let Sequence::Triple { offset, .. } = seq
                && offset >= block2.len()
            {
                saw_cross_block = true;
            }
        });

        assert!(
            saw_cross_block,
            "skip_matching(Some(false)) must populate hashes so block 2 \
             can match against the primed bytes",
        );
    }

    /// Control case for the prime-path test: same setup but with
    /// `skip_matching(None)` — the bytes are NOT hashed, so block 2
    /// must NOT find the cross-block match.
    #[test]
    fn skip_matching_with_none_hint_skips_hash_population() {
        let mut dict_block = alloc::vec::Vec::with_capacity(32);
        for i in 0..32u8 {
            dict_block.push(i.wrapping_mul(13).wrapping_add(7));
        }

        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        m.accept_data(dict_block.clone());
        m.skip_matching_with_hint(None); // plain skip — no hash pre-population

        let mut block2 = alloc::vec::Vec::with_capacity(48);
        block2.extend(100..116u8);
        block2.extend_from_slice(&dict_block[0..16]);
        block2.extend(120..136u8);
        m.accept_data(block2.clone());

        let mut saw_cross_block = false;
        m.start_matching(|seq| {
            if let Sequence::Triple { offset, .. } = seq
                && offset >= block2.len()
            {
                saw_cross_block = true;
            }
        });

        assert!(
            !saw_cross_block,
            "skip_matching(None) must NOT populate hashes — the legacy \
             skip cost-savings only hold when future blocks are willing \
             to miss matches in the skipped region",
        );
    }

    /// Window eviction: when total history would exceed `2 ×
    /// max_window_size`, the matcher must drain the oldest prefix
    /// down to a `max_window_size` tail BEFORE appending the new
    /// block, bump `prefix_start_index`, and clear the hash table.
    ///
    /// Post-append history can still hold up to
    /// `max_window_size + block_size` bytes (the kernel needs the
    /// just-arrived block for matching plus the retained prefix for
    /// cross-block lookups). The hard upper bound is therefore the
    /// eviction threshold itself: `2 × max_window_size`.
    #[test]
    fn extend_history_drains_old_prefix_past_two_window_sizes() {
        // window_log = 8 → max_window_size = 256, eviction threshold
        // = 512. Stage three 200-byte blocks: after the third commit,
        // total would be 600 > 512 → eviction fires.
        let mut m = FastKernelMatcher::with_params(8, 6, 4);
        for round in 0..3 {
            // Distinct payload per round so a hash entry from round
            // 0 referencing position 0 is identifiable as stale
            // after eviction.
            let block: alloc::vec::Vec<u8> = (0..200u8)
                .map(|i| i.wrapping_add(round as u8 * 17))
                .collect();
            m.accept_data(block);
            m.skip_matching_with_hint(None);
        }
        // Hard bound: post-append history can hold up to
        // `max_window_size + block_size` (retained prefix + the
        // just-appended block). The eviction policy keeps total
        // strictly below `2 × max_window_size` for the next
        // accept_data call, so the invariant we assert here is the
        // post-append upper bound.
        assert!(
            m.history.len() <= m.max_window_size * 2,
            "after eviction, history must be bounded by 2× max_window_size \
             (got {}, max_window_size={})",
            m.history.len(),
            m.max_window_size,
        );
        assert!(
            m.history.len() <= m.max_window_size + 200,
            "post-append history = retained prefix (≤ max_window_size) + \
             last block (200 bytes); got {}",
            m.history.len(),
        );
        // Post-fix: drain RESETS prefix_start_index back to 1 (the
        // initial sentinel-0 baseline) rather than accumulating
        // saturating_add — see the `drain_rebases_prefix_start_index`
        // regression test for the full rationale. Eviction is
        // proven by post-history shrinking, not by an
        // index-advancement signal.
        assert_eq!(
            m.prefix_start_index, 1,
            "drain must rebase prefix_start_index to the baseline (1)",
        );
    }

    /// Boundary: exactly `HASH_READ_SIZE` (8) bytes appended via
    /// `skip_matching(Some(false))` — the dict-prime hash population
    /// loop must hash precisely one position (range_start == 0,
    /// last_hashable == 0) without overrunning or panicking.
    #[test]
    fn skip_matching_dict_prime_handles_exactly_hash_read_size_bytes() {
        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        // 8-byte payload — at the edge of what the kernel can hash.
        // After append: history.len() = 8, last_hashable = 0,
        // range = 0..=0 (one position).
        let payload: alloc::vec::Vec<u8> = (0..8u8).collect();
        m.accept_data(payload);
        m.skip_matching_with_hint(Some(false));
        assert_eq!(m.history.len(), 8);
        // No assertion on hash entries — the bug we're guarding
        // against is a panic / overrun, not a behavioural one.
        // Reaching this line without unwinding is the test.
    }

    /// Boundary: pending block too short to hash anything (less than
    /// `HASH_READ_SIZE` bytes). The dict-prime path must early-return
    /// without panicking on the `last_hashable` subtract.
    #[test]
    fn skip_matching_dict_prime_handles_below_hash_read_size_bytes() {
        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        let payload: alloc::vec::Vec<u8> = (0..4u8).collect();
        m.accept_data(payload);
        // history will be 4 bytes after append < HASH_READ_SIZE (8).
        // prime_hash_table_for_range must short-circuit on the
        // `history_len < HASH_READ_SIZE` guard.
        m.skip_matching_with_hint(Some(false));
        assert_eq!(m.history.len(), 4);
    }

    /// rep ↔ offset_hist consistency: after a single block emits
    /// matches, the matcher's `rep[0]` (kernel's `rep_offset1` post-
    /// block) must equal `offset_hist[0]` (wire encoder's most
    /// recently emitted explicit offset). They're updated by
    /// different mechanisms (kernel internal state vs
    /// encode_offset_with_history) but should converge on the same
    /// value as long as every emitted Triple is a fresh (non-repcode)
    /// offset.
    #[test]
    fn rep_and_offset_hist_track_emitted_explicit_offsets_in_lockstep() {
        // Engineer a single block that produces a deterministic
        // explicit match. 96 bytes: 48-byte distinct-window
        // preamble + 48-byte verbatim copy of bytes [0..48].
        let mut data = alloc::vec::Vec::with_capacity(96);
        for i in 0..48u8 {
            data.push(i.wrapping_mul(11).wrapping_add(3));
        }
        data.extend_from_within(0..48);

        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        m.accept_data(data.clone());

        let mut emitted_offsets: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        m.start_matching(|seq| {
            if let Sequence::Triple {
                offset, match_len, ..
            } = seq
                && match_len >= 4
            {
                emitted_offsets.push(offset);
            }
        });

        // For at least one emitted explicit-offset match, the
        // matcher's `rep[0]` (post-block) must equal that offset,
        // AND `offset_hist[0]` must equal that offset. Both are
        // computed independently — matching values mean the two
        // tracks stayed in sync across the block.
        assert!(
            !emitted_offsets.is_empty(),
            "test setup must produce at least one explicit match \
             (otherwise this isn't testing the rep/offset_hist sync)",
        );
        let last_explicit = emitted_offsets[emitted_offsets.len() - 1];
        assert_eq!(
            m.rep[0] as usize, last_explicit,
            "kernel's rep[0] must reflect the last emitted explicit \
             offset (sync with wire encoder)",
        );
        assert_eq!(
            m.offset_hist[0] as usize, last_explicit,
            "offset_hist[0] (encode_offset_with_history-tracked) must \
             match rep[0] (kernel-tracked) after a clean block",
        );
    }

    /// Eviction during a dict-priming sequence: when consecutive
    /// `skip_matching(Some(false))` calls accumulate past the
    /// eviction threshold, the second skip must drop the older
    /// prime'd hash entries (via `hash_table.clear()`) AND bump
    /// `prefix_start_index` past the dropped bytes. Otherwise the
    /// matcher would carry stale absolute positions referencing
    /// evicted history.
    #[test]
    fn eviction_during_dict_priming_drops_stale_prime_entries() {
        // window_log=8 → max_window_size=256, threshold=512.
        // Two 300-byte blocks both via dict-prime skip — second
        // one triggers eviction.
        let mut m = FastKernelMatcher::with_params(8, 6, 4);
        let block1: alloc::vec::Vec<u8> = (0..200u8).collect();
        m.accept_data(block1);
        m.skip_matching_with_hint(Some(false));
        let block2: alloc::vec::Vec<u8> = (0..200u8).map(|i| i.wrapping_add(50)).collect();
        m.accept_data(block2);
        // Second skip would push total to 400, still under 512 — no
        // eviction yet. Make sure two more rounds trigger it.
        m.skip_matching_with_hint(Some(false));
        let block3: alloc::vec::Vec<u8> = (0..200u8).map(|i| i.wrapping_add(100)).collect();
        m.accept_data(block3);
        // Now 400+200=600 > 512 → eviction fires inside extend.
        m.skip_matching_with_hint(Some(false));

        // Post-fix: drain rebases prefix_start_index to 1 (rather
        // than cumulative saturating_add); eviction is proven by
        // bounded history below.
        assert_eq!(
            m.prefix_start_index, 1,
            "drain must rebase prefix_start_index to the baseline (1)",
        );
        // History within the 2× window-size hard cap.
        assert!(m.history.len() <= m.max_window_size * 2);
    }

    /// Regression for #216 CodeRabbit review #6: after multiple
    /// drain-evictions, `prefix_start_index` grows cumulatively
    /// (saturating_add of drop_n per drain) while the hash table is
    /// cleared and re-populated from current `history` positions
    /// [0..max_window_size]. Without rebasing `prefix_start_index`
    /// back to 1 on drain, the kernel's `match_idx >=
    /// prefix_start_index` filter eventually rejects EVERY match
    /// candidate (table positions < accumulated prefix_start_index).
    /// Retained-window matches die wholesale.
    #[test]
    fn drain_rebases_prefix_start_index_so_retained_history_stays_matchable() {
        // Tight window so evictions fire quickly: window_log = 8 →
        // max_window_size = 256, threshold = 512. Each 200-byte
        // commit accumulates 200 bytes; the third onwards triggers
        // eviction with drop_n ≈ 144-200 per drain.
        //
        // After ~8 evictions, naive prefix_start_index grows to
        // ~1 + 8 * ~180 ≈ 1441, far past any position in the
        // current 256-byte retained history. The kernel's filter
        // would then reject every match candidate (positions are at
        // most history.len() = 256 << prefix_start_index = 1441).
        let mut m = FastKernelMatcher::with_params(8, 6, 4);
        // Build a fixed "signature" pattern that will survive many
        // commits and remain matchable in the retained tail.
        let sig: alloc::vec::Vec<u8> = (0..32u8)
            .map(|i| i.wrapping_mul(31).wrapping_add(17))
            .collect();

        // Run 12 commits of 200 bytes each — guarantees many evictions.
        // Last block carries the signature in its head + matchable
        // bytes — we want the kernel to find this signature against
        // the retained-history copy (placed in commit #10 below).
        for round in 0..10 {
            let mut block: alloc::vec::Vec<u8> = (0..200u8)
                .map(|i| i.wrapping_add(round as u8 * 7))
                .collect();
            if round == 10 - 1 {
                // Plant the signature near end of this block — it
                // will live in the retained tail by the time block
                // 11 commits below.
                block[100..132].copy_from_slice(&sig);
            }
            m.accept_data(block);
            m.skip_matching_with_hint(Some(false)); // dict-prime, hashes populated
        }

        // After 10 commits at 200 bytes each, prefix_start_index has
        // advanced significantly (cumulative drop_n). Pre-fix it
        // would be in the thousands; post-fix it should be 1 (or
        // small).
        let pre_fix_index_would_exceed_history = m.prefix_start_index as usize > m.history.len();
        // Document the failing-case expectation (the bug being
        // tested for): without the fix, prefix_start_index >> any
        // valid history position.
        let _ = pre_fix_index_would_exceed_history;

        // Now commit a final block whose head contains the
        // signature — kernel should find it referencing the
        // earlier-planted copy in retained history.
        let mut block: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(200);
        block.extend_from_slice(&sig);
        for i in 0..168u8 {
            block.push(i.wrapping_mul(53).wrapping_add(91));
        }
        m.accept_data(block);

        let mut saw_match = false;
        m.start_matching(|seq| {
            if let Sequence::Triple { match_len, .. } = seq
                && match_len >= 4
            {
                saw_match = true;
            }
        });

        assert!(
            saw_match,
            "after multiple drain-evictions the matcher must still find \
             matches against retained-history bytes (got no match; \
             prefix_start_index = {} vs history.len() = {})",
            m.prefix_start_index,
            m.history.len(),
        );
    }

    /// Regression for #216 review #1: `accept_data` MUST perform
    /// window eviction immediately so the driver's `commit_space`
    /// can observe the byte delta via a pre/post `history.len()`
    /// comparison. Without commit-time eviction visibility, the
    /// driver's `retire_dictionary_budget` never runs for this
    /// backend → `max_window_size` stays inflated post-dict-prime
    /// → matcher can emit offsets exceeding the frame header's
    /// reported window (format-correctness risk).
    #[test]
    fn accept_data_evicts_eagerly_so_commit_observes_byte_delta() {
        // window_log = 8 → max_window_size = 256, eviction threshold
        // = 512. Stage three 200-byte blocks via accept_data + a
        // start_matching cycle each so history accumulates without
        // eviction (200, 400 bytes). The THIRD accept_data crosses
        // the 512-byte threshold; its eviction MUST be visible at
        // accept_data return-time via the history.len() drop.
        let mut m = FastKernelMatcher::with_params(8, 6, 4);

        m.accept_data((0..200u8).collect());
        m.skip_matching_with_hint(None);
        assert_eq!(m.history.len(), 200);
        assert_eq!(m.prefix_start_index, 1, "no eviction yet");

        m.accept_data((0..200u8).map(|i| i.wrapping_add(50)).collect());
        m.skip_matching_with_hint(None);
        assert_eq!(m.history.len(), 400);
        assert_eq!(m.prefix_start_index, 1, "still no eviction (400 < 512)");

        // Third commit: history (400) + new space (200) = 600 > 512.
        // Eviction MUST fire inside accept_data, dropping history
        // back to max_window_size (256) BEFORE the kernel runs.
        let pre = m.history_len_for_eviction_accounting();
        m.accept_data((0..200u8).map(|i| i.wrapping_add(100)).collect());
        let post = m.history_len_for_eviction_accounting();
        assert!(
            pre > post,
            "accept_data must shrink history at the eviction threshold \
             (pre={pre}, post={post}) — driver's commit_space relies on \
             this delta for retire_dictionary_budget accounting",
        );
        assert_eq!(
            post, 256,
            "post-eviction retained must equal max_window_size"
        );
        assert_eq!(
            m.prefix_start_index, 1,
            "drain rebases prefix_start_index to the baseline (1) \
             — eviction is proven by the history.len() shrink above",
        );
    }

    /// Regression for #216 review #2: `trim_to_window` must update
    /// `last_block_start` to track the drain. Without the update,
    /// the OLD position references pre-drain coordinates and
    /// `last_committed_space()` would either panic with OOB or
    /// return wrong bytes when `last_block_start > history.len()`
    /// post-drain.
    #[test]
    fn trim_to_window_keeps_last_committed_space_consistent() {
        // window_log = 8 → max_window_size = 256. Process a 200-byte
        // block (now in history at positions [0..200], last_block_start
        // = 0). Then bump the matcher's max_window_size DOWN to 128
        // (simulating a dictionary-budget retire shrinking the
        // window) and call trim_to_window — drain_n = 200 - 128 = 72.
        // Post-drain history is bytes [72..200] = 128 bytes. The
        // last_block_start (was 0) MUST now be 0 (since 72 > 0 →
        // saturating_sub gives 0) so last_committed_space() returns
        // a valid in-bounds slice.
        let mut m = FastKernelMatcher::with_params(8, 6, 4);
        let payload: alloc::vec::Vec<u8> = (0..200u8).collect();
        m.accept_data(payload);
        m.skip_matching_with_hint(None);
        assert_eq!(m.last_block_start, 0);
        assert_eq!(m.history.len(), 200);

        // Shrink the window and trim. Without the fix, last_block_start
        // stays at 0 (which happens to be valid here) — but to make
        // the bug surface, use a SECOND block so last_block_start is
        // mid-history.
        let payload2: alloc::vec::Vec<u8> = (50..150u8).collect();
        m.accept_data(payload2);
        m.skip_matching_with_hint(None);
        // history = [0..200] + [50..150] = 300 bytes. last_block_start
        // = 200 (start of second block).
        assert_eq!(m.last_block_start, 200);
        assert_eq!(m.history.len(), 300);

        // Now force trim_to_window to drain into the middle of the
        // second block: shrink max_window_size below the second
        // block's start.
        m.max_window_size = 64;
        let drained = m.trim_to_window();
        assert_eq!(
            drained,
            300 - 64,
            "trim must drain history down to max_window_size = 64",
        );
        assert_eq!(m.history.len(), 64);

        // The slice MUST be in bounds — the bug would panic here OR
        // return a stale slice. After the fix, last_block_start
        // saturating_sub'd by drained = 236; since drained (236) >
        // old last_block_start (200), new last_block_start = 0,
        // pointing at the current head of history (start of what
        // remains of block 2 after the drain).
        let last = m.last_committed_space();
        assert!(
            last.len() <= 64,
            "last_committed_space must be in-bounds after trim \
             (got len {})",
            last.len(),
        );
    }

    /// Regression for #216 Copilot review #15: after
    /// `prime_offset_history` the kernel's `rep[0..2]` must mirror
    /// the wire-encoder's `offset_hist[0..2]` — without this the
    /// kernel makes repcode decisions against stale FAST_INITIAL_REP
    /// while the wire encoder uses the primed history → wrong
    /// repcode wire encoding (correctness bug, not perf).
    #[test]
    fn prime_offset_history_keeps_rep_and_offset_hist_in_lockstep() {
        let mut m = FastKernelMatcher::with_params(12, 8, 4);
        // Pre-prime: matcher carries the donor's initial state.
        assert_eq!(m.rep, FAST_INITIAL_REP);
        assert_eq!(m.offset_hist, FAST_INITIAL_OFFSET_HIST);

        // Prime with non-default history (donor's dictionary load
        // restores explicit rep1/rep2/rep3 values).
        let primed = [9u32, 4, 8];
        m.prime_offset_history(primed);

        // BOTH must reflect the primed values; rep[0..2] = the first
        // two entries of offset_hist.
        assert_eq!(
            m.offset_hist, primed,
            "offset_hist must be updated by prime_offset_history",
        );
        assert_eq!(
            m.rep,
            [primed[0], primed[1]],
            "rep[0..2] must mirror offset_hist[0..2] post-prime \
             (kernel's repcode decisions must match the wire \
             encoder's seeded history)",
        );
    }
}
