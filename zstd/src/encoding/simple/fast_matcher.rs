//! Donor-shape Fast strategy matcher backend — selected for every
//! Fast-strategy level (Uncompressed, Fastest, Level(1), and the
//! negative Level(-7..=-1) variants). Per-level dispatch on
//! `(fast_hash_log, fast_mls, fast_step_size)` is wired through
//! `LevelParams` → `with_params` / `reset` (step_size is part of
//! the construction/reset signature, not a separate setter).
//! Level(1) uses `(hash_log=14, mls=7, step_size=2)`; Fastest /
//! Uncompressed / Level(-1..=-7) use `(hash_log=14, mls=6)` with
//! `step_size` 2..8 driving donor's acceleration gradient on
//! negative levels.
//!
//! `use_cmov` is derived directly from `window_log` inside the
//! matcher (donor heuristic `windowLog < 19`) — NOT a
//! `LevelParams` field.
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
//! - `prefix_start_index >= INITIAL_PREFIX_START_INDEX = 1` at all
//!   times. `history` holds real input bytes from position 0
//!   onward (no dummy region — M8 dropped the seeded sentinel byte
//!   for donor byte-range parity). Sentinel-0 protection comes from
//!   the kernel's `match_idx >= prefix_start_index` filter rejecting
//!   the hash table's empty-slot value `0`. After eviction / drain
//!   the buffer is rebased to position 0 and `prefix_start_index`
//!   resets to 1, making the first retained byte (`history[0]`)
//!   unmatchable — small ratio cost, accepted for sentinel safety.
//! - `history.len()` is bounded by `2 × max_window_size` post-append.
//!   See [`FastKernelMatcher::extend_history_with_pending`].
//! - `rep[0..2]` tracks the kernel's two-deep repcode state
//!   (overwritten from `FastBlockResult.rep` after every
//!   `start_matching`). `offset_hist[0..3]` tracks the wire
//!   encoder's three-deep history (rotated per Triple via
//!   `encode_offset_with_history`). They reflect DIFFERENT state
//!   and may diverge — e.g. on lit_len == 0 emits the kernel's
//!   `rep` stays put while `offset_hist` rotates per RFC 8878
//!   §3.1.2.5. Both halves are self-consistent within their own
//!   domain (kernel uses `rep` for next-block repcode probes,
//!   downstream wire encoder uses its own offset_hist).

use alloc::vec::Vec;

use crate::encoding::Sequence;
use crate::encoding::blocks::encode_offset_with_history;

use super::fast_kernel::hash_table::FastHashTable;
use super::fast_kernel::kernel::compress_block_fast;

/// Donor `ZSTD_defaultCParameters[level=1][srcSize > 256 KiB][Fast]`
/// constants. Kept for `MatchGeneratorDriver::new`'s initial-state
/// matcher (which runs BEFORE any `reset` from a resolved
/// `LevelParams`). Production calls thread per-level values
/// (`fast_hash_log`, `fast_mls`, `fast_step_size`) through
/// `LevelParams` instead.
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

/// Drain start offset used by eviction / drain paths. Set to 0:
/// `history` holds real input bytes from position 0 onward,
/// donor-parity layout, no dummy region. Sentinel-0 protection
/// (hash table's empty-slot value `0` would otherwise be
/// indistinguishable from a real match at position 0) is provided
/// by [`INITIAL_PREFIX_START_INDEX`] = 1 via the kernel's
/// `match_idx >= prefix_start_index` filter.
///
/// Kept as a named constant so the drain math reads consistently
/// against future changes.
pub(crate) const HISTORY_DRAIN_BASE: usize = 0;

/// Donor's `prefixStartIndex` floor on fresh frames. Set to 1 (not 0)
/// so the kernel's `match_idx >= prefix_start_index` filter rejects
/// stale empty-slot lookups (value 0 in FastHashTable's all-zero
/// initial state). Donor relies on its `ip0 += (ip0 == prefixStart)`
/// bump to skip position 0 instead — both approaches match the same
/// 0..N-1 byte ranges for the hash table.
const INITIAL_PREFIX_START_INDEX: u32 = 1;

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
///   position any match may reference. Pinned to
///   `INITIAL_PREFIX_START_INDEX` (= 1) at construction and after every
///   drain (drain re-indexes the retained tail; the `1` floor rejects
///   the hash table's all-zero empty-slot value from being read as a
///   valid match at position 0).
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
    /// Donor heuristic: prefer cmov match-found when
    /// `windowLog < 19` (`ZSTD_compressBlock_fast` line 449). Small-
    /// window encoders have less predictable in-range filtering, so
    /// the branchless variant beats the branchful one on those
    /// levels. Set during `reset` / `with_params` from `window_log`.
    /// Reachable in production via source-size hints (when the
    /// caller passes a small `source_size` to a streaming encoder,
    /// `adjust_params_for_source_size` clamps `window_log` below
    /// the donor default of 19, flipping `use_cmov` on).
    use_cmov: bool,
    /// Initial step the kernel uses for the 4-cursor body's skip
    /// schedule. Donor `stepSize = targetLength + !(targetLength) +
    /// 1` (min 2). Negative-level frames set this to 2..8 to
    /// recreate donor's acceleration gradient; Level(1) and other
    /// Fast levels keep step_size=2.
    step_size: usize,
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
            2,
        )
    }

    /// Current per-frame `step_size` (set at construction / reset).
    /// Test-only crate helper for verifying driver wiring.
    #[cfg(test)]
    pub(crate) fn step_size(&self) -> usize {
        self.step_size
    }

    /// Hash table `hash_log` (delegates to the inner table). Test-only
    /// crate helper for verifying driver wiring.
    #[cfg(test)]
    pub(crate) fn hash_log(&self) -> u32 {
        self.hash_table.hash_log()
    }

    /// Hash table `mls` (delegates to the inner table). Test-only
    /// crate helper for verifying driver wiring.
    #[cfg(test)]
    pub(crate) fn mls(&self) -> u32 {
        self.hash_table.mls()
    }

    /// Explicit-parameter constructor used by the wiring commit when
    /// the level resolution produced a non-default `(window_log,
    /// hash_log, mls, step_size)` tuple (typically because a small
    /// source-size hint clamped the window). Tests can also call this
    /// directly.
    pub(crate) fn with_params(window_log: u8, hash_log: u32, mls: u32, step_size: usize) -> Self {
        assert!(
            step_size >= 2,
            "FastKernelMatcher requires step_size >= 2 (got {step_size})"
        );
        // M8: history starts empty (HISTORY_DRAIN_BASE = 0).
        // Sentinel-0 protection comes from prefix_start_index =
        // INITIAL_PREFIX_START_INDEX = 1, which filters hash table
        // lookups returning the empty-slot value 0.
        let history = alloc::vec![0u8; HISTORY_DRAIN_BASE];
        Self {
            last_block_start: HISTORY_DRAIN_BASE,
            recycled_space: None,
            history,
            // Filter `match_idx >= prefix_start_index` rejects the
            // hash table's empty-slot value 0. Eviction in
            // `extend_history_with_pending` rebases the retained
            // tail and resets prefix_start_index back to 1.
            prefix_start_index: INITIAL_PREFIX_START_INDEX,
            rep: FAST_INITIAL_REP,
            offset_hist: FAST_INITIAL_OFFSET_HIST,
            hash_table: FastHashTable::new(hash_log, mls),
            max_window_size: 1usize << window_log,
            window_log,
            use_cmov: window_log < 19,
            step_size,
            pending: None,
        }
    }

    /// Reset for the next frame.
    ///
    /// Drops all history, clears the repcode and offset stacks, and
    /// either clears the existing hash table (if `(hash_log, mls)` are
    /// unchanged) or reallocates it. The window_log update redirects
    /// the soft-eviction bound and the decoder-side reported window.
    pub(crate) fn reset(&mut self, window_log: u8, hash_log: u32, mls: u32, step_size: usize) {
        assert!(
            step_size >= 2,
            "FastKernelMatcher requires step_size >= 2 (got {step_size})"
        );
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
        // M8: history starts empty (HISTORY_DRAIN_BASE = 0).
        self.history.clear();
        self.history.resize(HISTORY_DRAIN_BASE, 0);
        // Sentinel-0 protection via prefix_start_index >= 1 filter
        // — see `with_params` for the full rationale.
        self.prefix_start_index = INITIAL_PREFIX_START_INDEX;
        self.rep = FAST_INITIAL_REP;
        self.offset_hist = FAST_INITIAL_OFFSET_HIST;
        self.window_log = window_log;
        self.use_cmov = window_log < 19;
        self.step_size = step_size;
        self.max_window_size = 1usize << window_log;
        self.pending = None;
        self.last_block_start = HISTORY_DRAIN_BASE;
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
        1u64 << self.window_log
    }

    /// Read-only view of the most recently committed block — donor /
    /// legacy MatchGenerator's `window.last().data` equivalent.
    ///
    /// Three states:
    /// - Pre-`accept_data`: empty slice — `history` is empty and
    ///   `last_block_start` is 0, so `history[last_block_start..]`
    ///   degenerates to a zero-length slice.
    /// - Between `accept_data` and processing: the pending buffer.
    /// - Post-processing: `history` slice of the just-processed
    ///   block — frame compressor's raw-block emission reads this.
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
        assert!(
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
        // Eviction operates on REAL data length (excluding the
        // no dummy at the head of history post-M8).
        let real_len = self.history.len().saturating_sub(HISTORY_DRAIN_BASE);
        let new_real_total = real_len.saturating_add(space.len());
        let cap = self.max_window_size.saturating_mul(2);
        // Hard precondition: caller must split blocks into pieces no
        // larger than `2 × max_window_size`. Without this, the
        // eviction math below can't keep post-append history under
        // the advertised cap (retain_real saturates to 0 but the
        // full block still appends, violating the invariant).
        assert!(
            space.len() <= cap,
            "FastKernelMatcher requires block_size <= 2 × max_window_size \
             (block={}, cap={})",
            space.len(),
            cap,
        );
        if new_real_total > cap {
            // Compute how many real bytes to KEEP, then drop the
            // delta. Pre-fix code naively kept `max_window_size`
            // regardless of incoming block size — for a committed
            // block larger than `max_window_size` that left
            // `real_len + space.len() > 2 × max_window_size`,
            // violating the docstring invariant.
            //
            // Post-fix: retained = (cap - space.len()) clamped to
            // [0, max_window_size]. When the incoming block alone
            // exceeds cap, retained = 0 (no historical context kept,
            // but the cap is still as close as we can get without
            // truncating the caller's block).
            let retain_real = cap.saturating_sub(space.len()).min(self.max_window_size);
            let drop_n = real_len.saturating_sub(retain_real);
            if drop_n > 0 {
                self.drain_real_prefix(drop_n);
            }
        }

        self.pending = Some(space);
    }

    /// Drop the OLDEST `drop_n` real bytes from history and rebase
    /// the retained tail to start at position 0 (M8 layout: no
    /// dummy region). Used by both the eager commit-time eviction
    /// in [`Self::accept_data`] and the dictionary-budget retire
    /// loop's [`Self::trim_to_window`].
    ///
    /// Side effects:
    ///
    /// 1. Drain `history[0..drop_n)`.
    /// 2. Reset `prefix_start_index` to `INITIAL_PREFIX_START_INDEX = 1`
    ///    — drain re-indexes the retained tail; the sentinel-0
    ///    filter restores via this fixed baseline.
    /// 3. Clear the hash table — entries hold pre-drain absolute
    ///    positions that no longer reference live bytes.
    /// 4. `saturating_sub` `last_block_start` by `drop_n`.
    /// 5. Rehash retained tail starting at position 0 so block N+1
    ///    can find matches against the kept bytes (without this
    ///    they'd be "dead history" — visible in the Vec but
    ///    unlookupable).
    fn drain_real_prefix(&mut self, drop_n: usize) {
        let drain_end = HISTORY_DRAIN_BASE + drop_n;
        self.history.drain(HISTORY_DRAIN_BASE..drain_end);
        self.prefix_start_index = INITIAL_PREFIX_START_INDEX;
        self.hash_table.clear();
        self.last_block_start = self.last_block_start.saturating_sub(drop_n);
        self.prime_hash_table_for_range(HISTORY_DRAIN_BASE);
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
        // Compute the EFFECTIVE prefix floor for this scan against
        // the ADVERTISED frame window (`1 << window_log`), NOT
        // `max_window_size` — the driver may temporarily inflate
        // `max_window_size` by the retained dictionary budget
        // during `prime_with_dictionary`. The frame header still
        // reports `1 << window_log`, so any emitted offset older
        // than `history.len() - (1 << window_log)` would exceed
        // the decoder's reserved window and produce a format-
        // invalid sequence. Donor's
        // `ZSTD_getLowestPrefixIndex(ms, endIndex, windowLog)`
        // uses `windowLog` for the same reason.
        let advertised_window = 1usize << self.window_log;
        let effective_prefix = self
            .history
            .len()
            .saturating_sub(advertised_window)
            .max(self.prefix_start_index as usize) as u32;
        let prefix_start_index = effective_prefix;
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
                // Track wire encoder's offset_hist unconditionally —
                // matches what Dfast/Row/HashChain matchers do.
                // `matcher.offset_hist` and `matcher.rep` track
                // DIFFERENT state (wire encoder vs kernel); they're
                // not meant to stay in lockstep on lit_len == 0 emits.
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

        // Dispatch on (mls, use_cmov) — donor's 8 specialised
        // `ZSTD_GEN_FAST_FN` expansions (we also cover mls=8 for
        // future use). Each (mls, cmov) pair monomorphises the
        // kernel hot loop independently.
        //
        // The 10 arms are intentionally expanded inline rather
        // than wrapped in a macro: the kernel signature is short
        // enough that the duplication doesn't obscure intent, and
        // an explicit match makes it obvious which monomorphisations
        // exist. If the kernel ever grows additional const-generic
        // parameters (e.g. per-level `hash_fill_step`), revisit.
        let result = match (mls, self.use_cmov) {
            (4, false) => compress_block_fast::<4, false>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (4, true) => compress_block_fast::<4, true>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (5, false) => compress_block_fast::<5, false>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (5, true) => compress_block_fast::<5, true>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (6, false) => compress_block_fast::<6, false>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (6, true) => compress_block_fast::<6, true>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (7, false) => compress_block_fast::<7, false>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (7, true) => compress_block_fast::<7, true>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (8, false) => compress_block_fast::<8, false>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
                &mut wrap_emit,
            ),
            (8, true) => compress_block_fast::<8, true>(
                history,
                block_start,
                prefix_start_index,
                hash_table,
                rep_in,
                self.step_size,
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

    /// Seed both the wire encoder's offset history AND the kernel's
    /// repcode state from a primed dictionary load. Donor's
    /// `ZSTD_dictAndWindowLoad` restores `rep[0..2]` to the
    /// dictionary's stored `repToConfirm[0..2]`; the wire encoder
    /// uses the same triple as its 3-deep offset history. Setting
    /// only one side leaves the kernel making repcode decisions
    /// against stale FAST_INITIAL_REP while the wire encoder uses
    /// the primed values — divergent wire encoding.
    ///
    /// This setter writes both fields atomically. `rep[0..2]`
    /// mirrors `offset_hist[0..2]`; `offset_hist[2]` (the
    /// rep3 slot) lives only on the wire encoder side since the
    /// kernel's `rep` is two-deep.
    pub(crate) fn prime_offset_history(&mut self, offset_hist: [u32; 3]) {
        self.offset_hist = offset_hist;
        self.rep = [offset_hist[0], offset_hist[1]];
    }

    /// Read-only view of history's real-data length for the driver's
    /// eviction accounting (`commit_space` →
    /// `retire_dictionary_budget` flow). The driver compares pre/post
    /// values to derive a byte-delta; under M8 history holds only
    /// real bytes from position 0 onward (HISTORY_DRAIN_BASE is 0),
    /// so this is just the history length — the `saturating_sub` is
    /// kept symmetric with `trim_to_window` below in case the drain
    /// base ever moves off 0.
    pub(crate) fn history_len_for_eviction_accounting(&self) -> usize {
        self.history.len().saturating_sub(HISTORY_DRAIN_BASE)
    }

    /// Drop history bytes past `max_window_size` via
    /// [`Self::drain_real_prefix`] (resets `prefix_start_index` to
    /// `INITIAL_PREFIX_START_INDEX` = 1 — the sentinel-0 floor — and
    /// clears + rehashes the table). Returns evicted byte count;
    /// idempotent when `real_len <= max_window_size`.
    pub(crate) fn trim_to_window(&mut self) -> usize {
        let real_len = self.history.len().saturating_sub(HISTORY_DRAIN_BASE);
        if real_len <= self.max_window_size {
            return 0;
        }
        let drop_n = real_len - self.max_window_size;
        // Front-drain bookkeeping shared with `accept_data`'s
        // eager-eviction branch — see `drain_real_prefix` for the
        // full invariant list. Keeping the two sites in lockstep
        // (rather than inlined-and-duplicated) prevents the next
        // drain-related fix from landing in only one of them.
        self.drain_real_prefix(drop_n);
        drop_n
    }

    /// Pre-populate the hash table with entries for every position in
    /// `history[range_start..end_of_history]` that has at least
    /// `HASH_READ_SIZE` bytes of forward context. Used by the
    /// dictionary-priming skip path (`skip_matching` with
    /// `incompressible_hint = Some(false)`).
    ///
    /// `mls` dispatch is hoisted OUTSIDE the per-position loop so
    /// the inner body is monomorphised per matcher instance (no
    /// branch / mispredict in the hot path).
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

        match self.hash_table.mls() {
            4 => self.prime_hash_table_impl::<4>(range_start, last_hashable),
            5 => self.prime_hash_table_impl::<5>(range_start, last_hashable),
            6 => self.prime_hash_table_impl::<6>(range_start, last_hashable),
            7 => self.prime_hash_table_impl::<7>(range_start, last_hashable),
            8 => self.prime_hash_table_impl::<8>(range_start, last_hashable),
            _ => unreachable!("FastHashTable construction rejects mls outside 4..=8"),
        }
    }

    /// Monomorphised per-MLS loop body called by
    /// [`Self::prime_hash_table_for_range`] after the outer dispatch.
    fn prime_hash_table_impl<const MLS: u32>(&mut self, range_start: usize, last_hashable: usize) {
        let base = self.history.as_ptr();
        for pos in range_start..=last_hashable {
            // SAFETY: pos < history_len (by loop bound), and the load
            // width HASH_READ_SIZE is the kernel's contractually
            // required minimum, so `base.add(pos)` covers
            // HASH_READ_SIZE readable bytes by `last_hashable`'s
            // definition. The MLS const-generic is bound at the
            // caller's match arm — `hash_ptr<MLS>` and `put` are
            // constant-folded per MLS.
            let ptr = unsafe { base.add(pos) };
            let hash = unsafe { self.hash_table.hash_ptr::<MLS>(ptr) };
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
        // M8: history starts empty (HISTORY_DRAIN_BASE = 0).
        // First input byte will live at position 0; sentinel-0
        // protection comes from the prefix filter, not from a
        // dummy region.
        assert_eq!(m.history.len(), HISTORY_DRAIN_BASE);
        // prefix_start_index = 1 makes position 0 unmatchable so
        // the hash table's empty-slot value 0 can't be confused
        // with a real match.
        assert_eq!(m.prefix_start_index, INITIAL_PREFIX_START_INDEX);
        assert!(m.pending.is_none());
    }

    #[test]
    fn with_params_threads_through_each_field() {
        // Pick a non-default triple to prove no silent override by
        // donor-default constants.
        let m = FastKernelMatcher::with_params(16, 12, 5, 2);
        assert_eq!(m.window_log, 16);
        assert_eq!(m.hash_table.hash_log(), 12);
        assert_eq!(m.hash_table.mls(), 5);
        assert_eq!(m.max_window_size, 1usize << 16);
    }

    #[test]
    fn window_size_reports_one_shifted_window_log() {
        // window_log = 16 → 64 KiB reported window.
        let m = FastKernelMatcher::with_params(16, 12, 5, 2);
        assert_eq!(m.window_size(), 1u64 << 16);
        // Larger window_log → larger reported window. window_log = 22
        // (4 MiB, donor's BETTER_WINDOW_LOG) confirms the shift width
        // (`u64` head room).
        let m = FastKernelMatcher::with_params(22, 14, 7, 2);
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
            2,
        );

        // Post-reset: history empty (HISTORY_DRAIN_BASE=0; no
        // dummy only; prefix_start_index pinned to that baseline.
        assert_eq!(m.history.len(), HISTORY_DRAIN_BASE);
        assert_eq!(m.prefix_start_index, INITIAL_PREFIX_START_INDEX);
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
        m.reset(16, 10, 4, 2);
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
        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
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
        // History grew by exactly the block size (plus the
        // no dummy carried since construction — M8).
        assert_eq!(m.history.len(), data.len() + HISTORY_DRAIN_BASE);
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
        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
        let pre_rep = m.rep;
        let pre_offset_hist = m.offset_hist;

        let payload: alloc::vec::Vec<u8> = (0..40u8).collect();
        m.accept_data(payload.clone());
        // Take a count of state pre-skip.
        assert_eq!(m.last_committed_space().len(), payload.len());

        m.skip_matching_with_hint(None);

        assert_eq!(
            m.history.len(),
            payload.len() + HISTORY_DRAIN_BASE,
            "skip_matching must append the pending buffer to history \
             (above the HISTORY_DRAIN_BASE dummy)",
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

        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);

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
            block1.len() + block2.len() + HISTORY_DRAIN_BASE,
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

        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
        m.accept_data(dict_block.clone());
        m.skip_matching_with_hint(Some(false)); // dictionary-priming skip

        // Sanity: history grew, prefix_start_index unchanged.
        assert_eq!(m.history.len(), dict_block.len() + HISTORY_DRAIN_BASE);
        assert_eq!(m.prefix_start_index, INITIAL_PREFIX_START_INDEX);

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

        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
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
        let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
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
            m.history.len() <= m.max_window_size * 2 + HISTORY_DRAIN_BASE,
            "after eviction, REAL history must be bounded by 2× \
             max_window_size (got history.len()={}, max_window_size={})",
            m.history.len(),
            m.max_window_size,
        );
        assert!(
            m.history.len() <= m.max_window_size + 200 + HISTORY_DRAIN_BASE,
            "post-append history = RESERVED dummy + retained prefix \
             (≤ max_window_size) + last block (200 bytes); got {}",
            m.history.len(),
        );
        // Post-fix: drain RESETS prefix_start_index back to 1 (the
        // initial sentinel-0 baseline) rather than accumulating
        // saturating_add — see the `drain_rebases_prefix_start_index`
        // regression test for the full rationale. Eviction is
        // proven by post-history shrinking, not by an
        // index-advancement signal.
        assert_eq!(
            m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
            "drain must rebase prefix_start_index to the baseline (1)",
        );
    }

    /// Boundary: exactly `HASH_READ_SIZE` (8) real bytes appended
    /// via `skip_matching(Some(false))` — must hash one position
    /// (`range_start == HISTORY_DRAIN_BASE`, `last_hashable ==
    /// HISTORY_DRAIN_BASE`) without overrun.
    #[test]
    fn skip_matching_dict_prime_handles_exactly_hash_read_size_bytes() {
        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
        // 8-byte real payload appends above the RESERVED dummy →
        // history.len() = 8 + HISTORY_DRAIN_BASE, last_hashable =
        // HISTORY_DRAIN_BASE, hashed range = [RESERVED..=RESERVED]
        // (one position).
        let payload: alloc::vec::Vec<u8> = (0..8u8).collect();
        m.accept_data(payload);
        m.skip_matching_with_hint(Some(false));
        assert_eq!(m.history.len(), 8 + HISTORY_DRAIN_BASE);
        // No assertion on hash entries — the bug we're guarding
        // against is a panic / overrun, not a behavioural one.
        // Reaching this line without unwinding is the test.
    }

    /// Boundary: pending block too short to hash anything (less than
    /// `HASH_READ_SIZE` bytes). The dict-prime path must early-return
    /// without panicking on the `last_hashable` subtract.
    #[test]
    fn skip_matching_dict_prime_handles_below_hash_read_size_bytes() {
        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
        let payload: alloc::vec::Vec<u8> = (0..4u8).collect();
        m.accept_data(payload);
        // history will be 4 bytes after append < HASH_READ_SIZE (8).
        // prime_hash_table_for_range must short-circuit on the
        // `history_len < HASH_READ_SIZE` guard.
        m.skip_matching_with_hint(Some(false));
        assert_eq!(m.history.len(), 4 + HISTORY_DRAIN_BASE);
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

        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
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
        let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
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
            m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
            "drain must rebase prefix_start_index to the baseline (1)",
        );
        // History within the 2× window-size hard cap.
        assert!(m.history.len() <= m.max_window_size * 2);
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
        let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);

        m.accept_data((0..200u8).collect());
        m.skip_matching_with_hint(None);
        assert_eq!(m.history.len(), 200 + HISTORY_DRAIN_BASE);
        assert_eq!(
            m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
            "no eviction yet"
        );

        m.accept_data((0..200u8).map(|i| i.wrapping_add(50)).collect());
        m.skip_matching_with_hint(None);
        assert_eq!(m.history.len(), 400 + HISTORY_DRAIN_BASE);
        assert_eq!(
            m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
            "still no eviction (400 < 512)",
        );

        // Third commit: real history (400) + new space (200) = 600 > 512.
        // Eviction MUST fire inside accept_data, dropping history
        // back to max_window_size (256) BEFORE the kernel runs.
        // `history_len_for_eviction_accounting` returns REAL data
        // length (excluding the RESERVED dummy), so pre/post compare
        // cleanly in real-byte units.
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
            "post-eviction retained must equal max_window_size",
        );
        assert_eq!(
            m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
            "drain rebases prefix_start_index to INITIAL_PREFIX_START_INDEX \
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
        let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
        let payload: alloc::vec::Vec<u8> = (0..200u8).collect();
        m.accept_data(payload);
        m.skip_matching_with_hint(None);
        // First block lands at history[RESERVED..RESERVED+200] after
        // the seed dummy at [0..RESERVED). last_block_start tracks
        // that absolute index.
        assert_eq!(m.last_block_start, HISTORY_DRAIN_BASE);
        assert_eq!(m.history.len(), 200 + HISTORY_DRAIN_BASE);

        // Shrink the window and trim. Without the fix, last_block_start
        // stays mid-history past the post-drain end — to make the
        // bug surface, use a SECOND block so last_block_start is
        // somewhere AFTER the dummy + first block.
        let payload2: alloc::vec::Vec<u8> = (50..150u8).collect();
        m.accept_data(payload2);
        m.skip_matching_with_hint(None);
        // history = [dummy] + [0..200] + [50..150] = 1 + 200 + 100 = 301.
        // last_block_start = RESERVED + 200 = start of second block.
        assert_eq!(m.last_block_start, HISTORY_DRAIN_BASE + 200);
        assert_eq!(m.history.len(), 300 + HISTORY_DRAIN_BASE);

        // Now force trim_to_window to drain into the middle of the
        // second block: shrink max_window_size below the second
        // block's start. trim_to_window operates on REAL data so
        // the drain target is 300 REAL bytes → 64.
        m.max_window_size = 64;
        let drained = m.trim_to_window();
        assert_eq!(
            drained,
            300 - 64,
            "trim must drain REAL history down to max_window_size = 64",
        );
        // history.len() = RESERVED (dummy preserved) + 64.
        assert_eq!(m.history.len(), 64 + HISTORY_DRAIN_BASE);

        // The slice MUST be in bounds — the bug would panic here OR
        // return a stale slice. After the fix, last_block_start is
        // saturating_sub'd by drained (236) AND clamped to >= RESERVED
        // — since drained > old last_block_start - RESERVED, new
        // last_block_start = RESERVED, pointing at the current first
        // real byte of history (post-drain start of what remains of
        // block 2).
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
        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
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

    /// Regression for #216 CodeRabbit #19: when a committed block
    /// is larger than `max_window_size`, the pre-fix eviction math
    /// always retained a full `max_window_size` of historical real
    /// data and then appended the (oversized) block, blowing past
    /// the documented `2 × max_window_size` post-append bound.
    /// Fix retains a SMALLER prefix (or none) so the bound holds.
    #[test]
    fn accept_data_evicts_more_aggressively_when_block_larger_than_window() {
        let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
        // max_window_size = 256 (1 << 8). Threshold = 512.

        // Pre-fill history to full max_window_size of real data via
        // skip_matching_with_hint (no kernel-side trimming). u8 range
        // tops at 255, so the 256-byte preamble cycles via modulo.
        let preamble: alloc::vec::Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        m.accept_data(preamble);
        m.skip_matching_with_hint(None);
        assert_eq!(
            m.history_len_for_eviction_accounting(),
            256,
            "history pre-filled to one full window of real bytes",
        );

        // Commit a block larger than max_window_size but under 2×
        // (so its append should still be allowed without rejecting
        // outright). 400 > 256 but < 512.
        let oversize: alloc::vec::Vec<u8> = (0..400u32)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(11))
            .collect();
        m.accept_data(oversize);

        // Post-append real_len + space.len() MUST stay within 2×
        // max_window_size (the documented invariant). Pre-fix it
        // jumped to 256 + 400 = 656 = 2.56× — bound violated.
        let real_len_after = m.history_len_for_eviction_accounting();
        // accept_data stashes pending without appending, so the
        // 400-byte block is in pending. real_len reflects post-
        // drain retained real bytes. To verify the bound, run
        // start_matching to commit the append.
        m.start_matching(|_| {});
        let real_total = m.history_len_for_eviction_accounting();
        assert!(
            real_total <= m.max_window_size * 2,
            "post-append history MUST stay within 2 × max_window_size \
             (got real_total={real_total}, cap={})",
            m.max_window_size * 2,
        );
        // Sanity: pre-append eviction did drain something (we had
        // 256 real bytes, can't accept 400 more while staying under
        // 512 unless we drop at least 144).
        assert!(
            real_len_after < 256,
            "pre-append drain must have shed historical bytes \
             (got real_len_after_drain={real_len_after}, was 256 \
             before accept)",
        );
    }

    /// Regression for PR #219 round 6 (CR Critical): `start_matching`
    /// computes an effective sliding prefix floor of
    /// `history.len() - max_window_size` so emitted offsets never
    /// exceed the advertised `max_window_size`. Without this floor,
    /// after history grows past one window (commit's
    /// eager-eviction keeps up to 2× max_window_size before
    /// draining), the kernel could match against positions older
    /// than the frame window — invalid for decoder buffer
    /// reservation.
    #[test]
    fn start_matching_enforces_max_window_size_offset_bound() {
        // Tight window: window_log=7 → max_window_size=128.
        // Block lengths 200 + 200 = 400 bytes total, comfortably
        // past 128 so without the sliding floor the kernel would
        // emit matches at offsets > max_window.
        let mut m = FastKernelMatcher::with_params(7, 8, 4, 2);
        let max_window = m.max_window_size;
        assert_eq!(max_window, 128, "test assumes window_log=7");

        // Block 1: 200 bytes of a distinct ASCII pattern that
        // populates the hash table at positions 0..200.
        let block1: alloc::vec::Vec<u8> = (0..200u8).map(|i| 0x30 + (i % 64)).collect();
        m.accept_data(block1.clone());
        m.start_matching(|_| {});

        // Block 2: 200 bytes that RE-USE block1's content from
        // position 0 onwards. Without the sliding floor, the
        // kernel would happily emit Triples with offset
        // ≈ history_len (~200..400) — well past max_window=128.
        let block2 = block1.clone();
        m.accept_data(block2);

        let mut max_emitted_offset = 0usize;
        let mut emitted_match_count = 0usize;
        m.start_matching(|seq| {
            if let Sequence::Triple {
                offset, match_len, ..
            } = seq
                && match_len > 0
            {
                emitted_match_count += 1;
                if offset > max_emitted_offset {
                    max_emitted_offset = offset;
                }
            }
        });

        // GUARANTEE 1: the kernel actually scanned and matched
        // something — otherwise the offset bound below passes
        // trivially with max_emitted_offset = 0.
        assert!(
            emitted_match_count > 0,
            "fixture must produce at least one Triple match — \
             history.len()=~400, max_window=128, block2 is identical to block1",
        );

        // GUARANTEE 2: every emitted offset stays within the
        // advertised max_window_size. Without the sliding floor,
        // matches against early block1 positions (e.g. position
        // 0..72) would yield offsets > 128.
        assert!(
            max_emitted_offset <= max_window,
            "sliding floor MUST cap emitted offsets at max_window_size; \
             got max emitted offset {} vs max_window_size {}",
            max_emitted_offset,
            max_window,
        );
    }

    /// Regression for PR #219 round 9 (Copilot Critical): the
    /// sliding prefix floor in `start_matching` MUST use the
    /// advertised frame window (`1 << window_log`), NOT the
    /// dynamically inflated `max_window_size` (which the driver
    /// adds dictionary-budget bytes to during
    /// `prime_with_dictionary`). With the inflated value,
    /// offsets could exceed the advertised window during
    /// dictionary-primed compression — format-invalid sequences.
    #[test]
    fn start_matching_caps_offsets_at_window_log_not_inflated_max() {
        // Advertised frame window = 1 << 7 = 128 bytes.
        let mut m = FastKernelMatcher::with_params(7, 8, 4, 2);
        let advertised_window: usize = 1 << m.window_log;
        assert_eq!(advertised_window, 128, "test assumes window_log=7");

        // Simulate dictionary priming: driver inflates
        // max_window_size by retained_dict_budget. We add 200
        // bytes of "dictionary" content to history first, then
        // bump max_window_size to reflect the dict-retention
        // budget (mirrors MatchGeneratorDriver::prime_with_dictionary
        // for the Simple backend).
        let dict: alloc::vec::Vec<u8> = (0..200u8).map(|i| 0x40 + (i % 64)).collect();
        m.accept_data(dict.clone());
        m.start_matching(|_| {}); // populate hash table from dict
        m.max_window_size = m.max_window_size.saturating_add(200);

        // Add a block whose first 100 bytes match dict[0..100].
        // Without the fix, the kernel would emit offsets up to
        // ~history_len (200..300), since the inflated max_window
        // (328) keeps even the dict's earliest bytes inside the
        // sliding floor.
        let block: alloc::vec::Vec<u8> = (0..100u8).map(|i| 0x40 + (i % 64)).collect();
        m.accept_data(block);

        let mut max_emitted_offset = 0usize;
        let mut emitted_match_count = 0usize;
        m.start_matching(|seq| {
            if let Sequence::Triple {
                offset, match_len, ..
            } = seq
                && match_len > 0
            {
                emitted_match_count += 1;
                if offset > max_emitted_offset {
                    max_emitted_offset = offset;
                }
            }
        });

        assert!(
            emitted_match_count > 0,
            "fixture must produce at least one match — block content \
             repeats dict, history.len() ≈ 300, scan should find at \
             least one Triple",
        );
        assert!(
            max_emitted_offset <= advertised_window,
            "sliding floor MUST cap emitted offsets at the ADVERTISED \
             frame window (1 << window_log = {}), NOT the inflated \
             max_window_size; got max emitted offset {}",
            advertised_window,
            max_emitted_offset,
        );
    }
}
