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
//! - `rep[0..2]` is the functional repcode state: the kernel's
//!   two-deep stack, overwritten from `FastBlockResult.rep` after every
//!   `start_matching`, and what the NEXT block's kernel probes against.
//!   `offset_hist[0..3]` is NOT mutated by matching — the Fast backend
//!   drives repcodes off `rep`, and the wire-offset repcode coding is
//!   done downstream by `encode_raw_sequences_into` against the encode
//!   pipeline's OWN offset history, so a per-match
//!   `encode_offset_with_history` on the matcher would be redundant.
//!   `offset_hist` is therefore only seeded by `prime_offset_history`
//!   (which also sets `rep`) and otherwise stays at its `reset` default.
//!   Do NOT reintroduce per-match `offset_hist` rotation here: it is
//!   pure overhead on this backend (it was removed because the coded
//!   offset it produced was discarded).

use alloc::vec::Vec;

use crate::encoding::Sequence;
use crate::encoding::dict_attach::DictAttach;

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
///
/// Tradeoff: this rejects legitimate position-0 matches donor would
/// emit (rare — requires `read32(ip0)` to coincidentally equal
/// `read32(base)`), but cross-block isolation under
/// `skip_matching_with_hint(None)` depends on the sentinel — the
/// `skip_matching_with_none_hint_skips_hash_population` test
/// exercises that contract. Lowering to 0 breaks the test; the
/// position-0 emit rate is too small to be worth that breakage.
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
    /// One-shot borrowed match window: `(ptr, len)` into a caller-owned
    /// input buffer that holds the entire frame. When `Some`, all window
    /// *reads* ([`Self::history_bytes`] and the kernel match-slice) view
    /// this range instead of the owned `history` buffer, so the matcher
    /// never copies the input into `history`. `None` selects the owned
    /// streaming path (the default; the borrowed path is opt-in via
    /// [`Self::set_borrowed_window`]).
    ///
    /// A raw pointer (not a borrow) is required because this matcher is
    /// a persistent field of the driver / frame compressor; a borrowed
    /// lifetime would tie those structs to the input buffer. SAFETY
    /// contract (enforced by the caller, see `set_borrowed_window`): the
    /// pointed-to buffer must stay live and unmodified for as long as
    /// the window is set, and the window must be cleared before the
    /// buffer is dropped or the matcher is reused for another frame.
    borrowed: Option<(*const u8, usize)>,
    /// Absolute `[start, end)` range of the block most recently scanned
    /// by [`Self::start_matching_borrowed`], in the borrowed window's
    /// coordinate space. `Some` only while a borrowed window is active
    /// and at least one borrowed block has been scanned; lets
    /// [`Self::last_committed_space`] return that block's bytes
    /// (`borrowed[start..end]`, zero-copy) for the emit path — the
    /// borrowed analogue of `last_block_start` on the owned path.
    last_borrowed_block: Option<(usize, usize)>,
    /// Immutable dictionary hash table (donor `dictMatchState` Fast path) plus
    /// its CDict cache lifecycle, via the shared [`DictAttach`] level-1
    /// scaffolding. The table is built once during `prime_with_dictionary` over
    /// the dictionary region at the front of `history` (positions
    /// `[1, region_len)`), using the same `(hash_log, mls)` as
    /// [`Self::hash_table`] so a single hash keys both. Attached
    /// (`is_attached()`) activates the dual-probe [`compress_block_fast_dict`]
    /// kernel; invalidated on any history eviction (absolute dict positions
    /// would otherwise go stale) so the no-dict kernel takes over —
    /// correctness-safe, only the dict ratio benefit is lost when the input is
    /// large enough to slide the dictionary out of the window. `region_len()`
    /// is the dict/input boundary (`dict_end`).
    dict: DictAttach<FastHashTable>,
    /// High-water mark of any position storable into [`Self::hash_table`]
    /// since the last table clear / epoch advance: the largest history
    /// length seen by [`Self::extend_history_with_pending`] and the largest
    /// borrowed `block_end` scanned. `reset` feeds it to
    /// [`FastHashTable::advance_epoch`] as the span that makes every
    /// previously-stored entry stale, then rearms it at 0.
    table_pos_high_water: usize,
}

impl Clone for FastKernelMatcher {
    fn clone(&self) -> Self {
        Self {
            history: self.history.clone(),
            prefix_start_index: self.prefix_start_index,
            rep: self.rep,
            offset_hist: self.offset_hist,
            hash_table: self.hash_table.clone(),
            max_window_size: self.max_window_size,
            window_log: self.window_log,
            use_cmov: self.use_cmov,
            step_size: self.step_size,
            pending: self.pending.clone(),
            last_block_start: self.last_block_start,
            recycled_space: self.recycled_space.clone(),
            borrowed: self.borrowed,
            last_borrowed_block: self.last_borrowed_block,
            dict: self.dict.clone(),
            table_pos_high_water: self.table_pos_high_water,
        }
    }

    // The per-frame dictionary snapshot restore `clone_from`s this whole
    // matcher; reusing the retained `history` / hash-table / dict-table
    // buffers turns that restore into pure copies (no allocations), which
    // is what the donor's CDict table-copy regime pays.
    fn clone_from(&mut self, source: &Self) {
        self.history.clone_from(&source.history);
        self.prefix_start_index = source.prefix_start_index;
        self.rep = source.rep;
        self.offset_hist = source.offset_hist;
        self.hash_table.clone_from(&source.hash_table);
        self.max_window_size = source.max_window_size;
        self.window_log = source.window_log;
        self.use_cmov = source.use_cmov;
        self.step_size = source.step_size;
        self.pending.clone_from(&source.pending);
        self.last_block_start = source.last_block_start;
        self.recycled_space.clone_from(&source.recycled_space);
        self.borrowed = source.borrowed;
        self.last_borrowed_block = source.last_borrowed_block;
        self.dict.clone_from(&source.dict);
        self.table_pos_high_water = source.table_pos_high_water;
    }
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
        // Kernel indices are `u32`. `accept_data` lets history grow
        // up to `2 * max_window_size` before draining (donor parity
        // for the eager-eviction band), so `max_window_size` is
        // capped at 2^30 to keep that band ≤ 2^31 < `u32::MAX` and
        // prevent any `history.len()` from tripping the kernel's
        // `data.len() > u32::MAX` panic. Donor's
        // `ZSTD_WINDOWLOG_MAX_64` is 30 for the same reason — we
        // mirror it.
        assert!(
            window_log <= 30,
            "FastKernelMatcher requires window_log <= 30 (got {window_log}); \
             2 * (1 << 30) is the eviction-band ceiling that keeps history \
             length below the kernel's u32::MAX input bound"
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
            borrowed: None,
            last_borrowed_block: None,
            dict: DictAttach::new(),
            table_pos_high_water: 0,
        }
    }

    /// Reset for the next frame.
    ///
    /// Drops all history, clears the repcode and offset stacks, and
    /// either clears the existing hash table (if `(hash_log, mls)` are
    /// unchanged) or reallocates it. The window_log update redirects
    /// the soft-eviction bound and the decoder-side reported window.
    ///
    /// `dict_attach_epoch`: the upcoming frame re-primes the SAME
    /// dictionary in attach mode (separate cached dict table, dual-probe
    /// kernel). When the cached dict table is still primed, the main
    /// table is then invalidated via an epoch advance (donor
    /// `ZSTD_continueCCtx` cadence — stale entries filtered by the bias,
    /// no full-table memset); every other shape keeps the historical
    /// `clear()` so the raw-slice no-dict kernels always see a bias-0
    /// table.
    pub(crate) fn reset(
        &mut self,
        window_log: u8,
        hash_log: u32,
        mls: u32,
        step_size: usize,
        dict_attach_epoch: bool,
        // The caller (driver) has a primed-snapshot whose key matches this
        // exact reset shape and WILL `clone_from` it over this matcher
        // right after the reset (the copy-mode dictionary restore). The
        // table contents and epoch bias are about to be replaced
        // wholesale, so the full-table memset here would be pure waste.
        table_overwritten_by_restore: bool,
    ) {
        assert!(
            step_size >= 2,
            "FastKernelMatcher requires step_size >= 2 (got {step_size})"
        );
        // Same window_log cap as `with_params` — see there for why
        // the ceiling is 30, not 31.
        assert!(
            window_log <= 30,
            "FastKernelMatcher requires window_log <= 30 (got {window_log})"
        );
        if table_overwritten_by_restore
            && self.hash_table.hash_log() == hash_log
            && self.hash_table.mls() == mls
        {
            // Leave the table untouched: the snapshot restore copies the
            // primed contents (and bias) over it immediately after.
        } else if self.hash_table.hash_log() != hash_log || self.hash_table.mls() != mls {
            // Parameters changed — rebuild the table at the new size.
            // Cannot reuse the old allocation because the hash table
            // dimensions are baked in at construction. A reshape also
            // invalidates the cached dict table: its absolute positions
            // index a table whose shape no longer matches.
            self.hash_table = FastHashTable::new(hash_log, mls);
            self.dict.invalidate();
        } else if dict_attach_epoch && self.dict.is_primed() {
            // Dict-attach frame over the same primed dictionary: advance
            // the epoch bias past every position the previous frames could
            // have stored instead of memsetting the whole table (donor
            // `ZSTD_continueCCtx`). The dual-probe dict kernel reads the
            // main table only through `FastHashTable::get`, which maps
            // pre-advance entries to the empty sentinel. The cached dict
            // table is untouched (its own instance, bias 0): the dictionary
            // lands at the same absolute history positions every frame, so
            // its hashes stay valid and the per-frame re-hash is skipped
            // (CDict-equivalent).
            let span = u32::try_from(self.table_pos_high_water).unwrap_or(u32::MAX);
            self.hash_table.advance_epoch(span);
        } else {
            // Same shape — keep the allocation, zero the entries via
            // `memset` (ZSTD_window_clear cadence). A primed dict table
            // is retained (see the epoch branch above for why that is
            // sound).
            self.hash_table.clear();
        }
        self.table_pos_high_water = 0;
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
        // Drop any borrowed window: the next frame's input buffer is a
        // different allocation, so a stale (ptr, len) would dangle.
        self.borrowed = None;
        self.last_borrowed_block = None;
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

    /// Heap bytes this matcher owns: the history buffer, the hash table, the
    /// recycle/pending slots, and any attached dictionary hash table.
    pub(crate) fn heap_size(&self) -> usize {
        self.history.capacity()
            + self.hash_table.heap_size()
            + self.pending.as_ref().map_or(0, |v| v.capacity())
            + self.recycled_space.as_ref().map_or(0, |v| v.capacity())
            + self.dict.table().map_or(0, |t| t.heap_size())
    }

    /// Flat byte view of the match window the kernel scans against.
    ///
    /// Single read accessor for the window storage so the storage
    /// representation (owned buffer or borrowed one-shot view) is
    /// resolved in one place. The owned `window_low` length math and the
    /// last-committed-block peek (`last_committed_space`) call this.
    /// Tail-literal emission is NOT a caller — it happens inside
    /// `run_fast_kernel_block` against the `history` slice handed to it.
    /// The kernel match-slice in `start_matching` does NOT call this — it
    /// inlines the identical owned/borrowed selection so the immutable
    /// window borrow stays a disjoint field projection alongside the
    /// `&mut self.hash_table` borrow (a `&self` accessor call would
    /// borrow all of `self` and collide). Owned-only mutation paths
    /// (append, drain, rehash) keep accessing the backing buffer
    /// directly.
    #[inline(always)]
    fn history_bytes(&self) -> &[u8] {
        match self.borrowed {
            // SAFETY: the (ptr, len) pair was registered via
            // `set_borrowed_window`, whose contract guarantees the
            // buffer stays live and unmodified until the window is
            // cleared (and the window is cleared on `reset` / before the
            // buffer drops). `len` is the exact length passed in, so the
            // reconstructed slice never exceeds the original allocation.
            Some((ptr, len)) => unsafe { core::slice::from_raw_parts(ptr, len) },
            None => &self.history,
        }
    }

    /// Point the match window at a caller-owned input buffer instead of
    /// the owned `history` mirror, so the matcher reads the input in
    /// place without copying it block-by-block.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `buffer` stays live and is not
    /// moved or mutated for as long as the window is set, and that the
    /// window is cleared (via [`Self::clear_borrowed_window`] or
    /// [`Self::reset`]) before `buffer` is dropped or the matcher is
    /// reused for a different frame. The owned-buffer mutation paths
    /// (`accept_data`, `extend_history_with_pending`, drain, prime) must
    /// not run while a borrowed window is active.
    pub(crate) unsafe fn set_borrowed_window(&mut self, buffer: &[u8]) {
        // A staged owned `pending` block would make `last_committed_space`
        // return the pending buffer (it checks `pending` first) instead of
        // the borrowed range, breaking the borrowed/owned equivalence the
        // emit path relies on. The borrowed one-shot caller resets before
        // registering (so `pending` is None), but this is an unsafe mode
        // switch — make the precondition explicit and loud.
        assert!(
            self.pending.is_none(),
            "set_borrowed_window requires no staged owned block; reset before switching to a borrowed window",
        );
        self.borrowed = Some((buffer.as_ptr(), buffer.len()));
        self.last_borrowed_block = None;
        // Any hash-table entries left from a prior window are absolute
        // positions into THAT buffer; once we switch the window backing
        // to `buffer`, a `start_matching_borrowed` scan would read them
        // as indices into the new buffer — out of bounds / memory-unsafe.
        // Today the caller (`compress_oneshot_borrowed`) always resets
        // first, but make the borrowed-window registration self-contained:
        // clear the table and re-floor `prefix_start_index` so the window
        // starts from a clean match state regardless of prior history.
        self.hash_table.clear();
        self.prefix_start_index = INITIAL_PREFIX_START_INDEX;
    }

    /// Clear a borrowed window set by [`Self::set_borrowed_window`],
    /// returning the matcher to the owned `history` path.
    pub(crate) fn clear_borrowed_window(&mut self) {
        self.borrowed = None;
        self.last_borrowed_block = None;
        // The hash table still holds absolute positions into the
        // now-detached borrowed buffer. An owned-path scan would read
        // them as offsets into `self.history` — out of bounds once the
        // borrowed buffer is gone (memory-unsafe). Today every frame
        // begins with `reset` (which clears the table), so an owned
        // scan never observes the stale entries; but the docstring
        // promises a return to the owned path, so restore that
        // invariant here too rather than relying on the caller always
        // resetting first. Mirrors `reset` / `drain_real_prefix`:
        // clear the table and re-floor `prefix_start_index` at the
        // sentinel-0 baseline.
        self.hash_table.clear();
        self.prefix_start_index = INITIAL_PREFIX_START_INDEX;
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
        if let Some(slice) = self.pending.as_deref() {
            return slice;
        }
        // Borrowed one-shot path: the just-scanned block lives at
        // `[start, end)` of the borrowed window, which `history_bytes()`
        // views in place — return it zero-copy for the emit path.
        if let Some((start, end)) = self.last_borrowed_block {
            return &self.history_bytes()[start..end];
        }
        &self.history_bytes()[self.last_block_start..]
    }

    /// Accept a freshly-committed block from the driver.
    ///
    /// Donor's `ZSTD_window_update`: the new bytes are stashed for
    /// the next [`Self::start_matching`] / [`Self::skip_matching`]
    /// call but NOT yet appended to `history` — that delay lets the
    /// driver-side `get_last_space` peek at the still-pending buffer
    /// without committing it to the matcher's hot path.
    ///
    /// History budget is enforced EAGERLY in this function (not lazily
    /// inside [`Self::extend_history_with_pending`]) so the driver's
    /// `commit_space` can observe the eviction delta via a pre/post
    /// `history.len()` comparison. That delta feeds
    /// `retire_dictionary_budget`, which shrinks `max_window_size`
    /// back to the frame's contracted window after dictionary priming
    /// inflated it. Without commit-time visibility the dict-budget
    /// retire never runs and the matcher can emit offsets exceeding
    /// the frame header's reported window size (format-correctness
    /// risk).
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
        // Eviction operates on REAL data length. Post-M8 there is
        // no dummy prefix at the head of `history`, so `real_len` is
        // just `history.len()` minus the `HISTORY_DRAIN_BASE`
        // sentinel-slot offset — not a placeholder block subtraction.
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
    /// 5. Rehash retained tail starting at the sentinel-0 floor
    ///    ([`INITIAL_PREFIX_START_INDEX`] = 1) so block N+1 can find
    ///    matches against the kept bytes (without this they'd be
    ///    "dead history" — visible in the Vec but unlookupable).
    ///    Starting from index 1 instead of 0 avoids hashing a position
    ///    that the kernel's `match_idx >= prefix_start_index` filter
    ///    would reject anyway.
    fn drain_real_prefix(&mut self, drop_n: usize) {
        let drain_end = HISTORY_DRAIN_BASE + drop_n;
        self.history.drain(HISTORY_DRAIN_BASE..drain_end);
        self.prefix_start_index = INITIAL_PREFIX_START_INDEX;
        // Any drain rebases the retained tail to position 0, invalidating
        // the immutable dict table's absolute positions (and likely
        // sliding the dictionary out of the live window entirely). Drop it
        // so subsequent blocks fall back to the no-dict kernel — the only
        // cost is the dict ratio benefit on inputs large enough to evict
        // the dictionary, which is exactly when the dict is no longer
        // reachable anyway.
        self.dict.invalidate();
        self.hash_table.clear();
        self.last_block_start = self.last_block_start.saturating_sub(drop_n);
        // Skip position 0 — `prefix_start_index = 1` means the kernel
        // rejects any match resolving to index 0, so populating that
        // slot would just pollute the table with an unreachable entry.
        self.prime_hash_table_for_range(INITIAL_PREFIX_START_INDEX as usize);
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
        // Track the largest position any kernel scan over this history
        // could store into the hash table (consumed by `reset`'s epoch
        // advance).
        self.table_pos_high_water = self.table_pos_high_water.max(self.history.len());
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
    pub(crate) fn start_matching(&mut self, handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        // Owned scan path. A borrowed one-shot window (set via
        // `set_borrowed_window`) is mutually exclusive with this path:
        // `extend_history_with_pending` appends into `self.history` and
        // `block_start` indexes that owned buffer, so matching against a
        // borrowed window here would index it with an owned-history
        // offset, and the kernel would read `self.history` at hash-table
        // indices that were populated against the (possibly larger)
        // borrowed window — out-of-bounds / UB. Always-on (not
        // debug_assert): the guard must hold in release / `cargo test
        // --release` too, since the failure mode is memory-unsafe, not
        // merely wrong output. The borrowed window is scanned by
        // `start_matching_borrowed` instead.
        assert!(
            self.borrowed.is_none(),
            "start_matching is the owned path; clear the borrowed window first (use start_matching_borrowed)",
        );
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
        // Donor's `windowLow` analogue: the absolute floor of in-window
        // positions. Equals 0 at block 0 (no prior input retained) and
        // advances as the window slides. Drives the prologue's
        // `max_rep = ip0 - window_low` computation AND the backward-
        // extension `match_pos > window_low` bound — both paths that
        // donor expresses against `prefixStart` directly (NOT against
        // a sentinel-1 floor).
        let window_low = self.history_bytes().len().saturating_sub(advertised_window) as u32;
        // Sentinel-aware prefix for the hash-table filter — match_idx
        // == 0 (an uninitialized FastHashTable slot) must be rejected
        // by `match_found`, so we floor at `INITIAL_PREFIX_START_INDEX
        // = 1` when window_low is 0 (block 0 / pre-eviction blocks).
        // For later blocks (window_low >= 1) the two values coincide.
        //
        // This SPLIT is the donor-parity fix for issue #220: using
        // `prefix_start_index = 1` for the prologue's max_rep gave
        // `max_rep = 0` at ip0=1, zeroing donor's default
        // `rep_offset1 = 1` and disabling rep-at-ip2 for the entire
        // first block. With `window_low = 0` we match donor exactly
        // (`max_rep = 1`, rep_offset1 survives).
        let prefix_start_index = window_low.max(self.prefix_start_index);
        let rep_in = self.rep;
        let mls = self.hash_table.mls();
        let step_size = self.step_size;
        let use_cmov = self.use_cmov;

        // Donor `dictMatchState` Fast path, active whenever a dictionary is
        // primed (and not yet evicted — `drain_real_prefix` drops the table).
        // The 4-cursor search and its `prefix_start_index` / `window_low`
        // bounds are shared with the no-dict kernel (main matches are input
        // positions, all `>= dict_end >= prefix_start_index`); the dict probe
        // additionally requires `pos >= window_low` and `pos < dict_end` so
        // emitted offsets (`ip0 - pos`) stay within the advertised window,
        // including the pre-drain 1x..2x-window band where `window_low > 0`.
        let use_dict = self.dict.is_attached();
        let history: &[u8] = &self.history;
        let rep_out = if use_dict {
            use super::fast_kernel::kernel::PrefixBounds;
            let dict_end = self.dict.region_len() as u32;
            let bounds = PrefixBounds {
                prefix_start_index,
                window_low,
            };
            let main_table = &mut self.hash_table;
            let dict_table = self
                .dict
                .table()
                .expect("use_dict implies dict_table is Some");
            run_fast_kernel_block_dict(
                history,
                block_start,
                bounds,
                dict_end,
                main_table,
                dict_table,
                rep_in,
                step_size,
                mls,
                use_cmov,
                handle_sequence,
            )
        } else {
            // Owned scan reads `self.history` directly (the guard above
            // guarantees no borrowed window is active). Borrowing only the
            // `history` field keeps it a disjoint projection alongside the
            // `&mut self.hash_table` borrow handed to `run_fast_kernel_block`
            // (a `&self` accessor would borrow all of `self` and collide).
            run_fast_kernel_block(
                history,
                block_start,
                prefix_start_index,
                window_low,
                &mut self.hash_table,
                rep_in,
                step_size,
                mls,
                use_cmov,
                handle_sequence,
            )
        };
        // Persist the kernel's rep state for the next block.
        self.rep = rep_out;
    }

    /// Borrowed-window equivalent of [`Self::start_matching`]: scan the
    /// block spanning `[block_start, block_end)` of the registered
    /// borrowed window in place, without appending to or evicting from
    /// the owned `history`. Requires [`Self::set_borrowed_window`] to
    /// have registered the buffer; the caller supplies absolute block
    /// bounds (the one-shot frame path tracks them as it walks the
    /// input). `block_start <= block_end` and `block_end` <= the
    /// registered buffer length.
    ///
    /// Produces a byte-identical sequence stream to the owned path: a
    /// one-shot frame's blocks lie back-to-back in the input buffer
    /// exactly as the owned `history` accumulates them. Over-window inputs
    /// are supported: matches are bounded by `window_low = block_end -
    /// advertised_window` (the same bound the owned evicting path applies),
    /// and the per-position `put` during the scan keeps in-window hash
    /// slots current — so an out-of-window stale slot is rejected exactly
    /// where the owned rehash would have left the slot empty, giving
    /// identical match decisions with or without eviction.
    pub(crate) fn start_matching_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        handle_sequence: impl for<'a> FnMut(Sequence<'a>),
    ) {
        let (ptr, total_len) = self
            .borrowed
            .expect("start_matching_borrowed requires a registered borrowed window");
        // Always-on (not debug_assert): the bounds feed the unsafe
        // `from_raw_parts` below, so they must be validated even in
        // release / `cargo test --release` where debug_assert is
        // compiled out — otherwise an out-of-range block_end would build
        // a slice past the borrowed allocation (immediate UB).
        assert!(
            block_start <= block_end && block_end <= total_len,
            "borrowed block bounds out of range: start={block_start} end={block_end} total={total_len}",
        );
        // Borrowed scans store raw (bias-0) positions up to `block_end`
        // through the table's hot-state slice; record them for `reset`'s
        // epoch-advance span. (A borrowed window never coexists with a
        // primed dict, so the table bias is 0 here — see `hot_state` —
        // but the high-water must still cover these positions in case a
        // dictionary is attached on a later frame.)
        self.table_pos_high_water = self.table_pos_high_water.max(block_end);
        // Same window math as the owned path, but against the absolute
        // block end in the borrowed buffer rather than the accumulated
        // history length.
        let advertised_window = 1usize << self.window_log;
        let window_low = block_end.saturating_sub(advertised_window) as u32;
        let prefix_start_index = window_low.max(self.prefix_start_index);
        let rep_in = self.rep;
        let mls = self.hash_table.mls();
        // SAFETY: `block_end <= total_len` (the registered buffer length)
        // by the caller contract + the always-on `assert!` above, so the slice stays
        // within the borrowed allocation; `set_borrowed_window`'s
        // liveness contract guarantees the buffer is still live. `ptr` is
        // copied out of the `Copy` `borrowed` field, so `history` is not
        // tied to `&self` and stays disjoint from the `&mut` field
        // borrows passed to the kernel runner.
        let history: &[u8] = unsafe { core::slice::from_raw_parts(ptr, block_end) };
        let rep_out = run_fast_kernel_block(
            history,
            block_start,
            prefix_start_index,
            window_low,
            &mut self.hash_table,
            rep_in,
            self.step_size,
            mls,
            self.use_cmov,
            handle_sequence,
        );
        self.rep = rep_out;
        // Record the scanned range so `last_committed_space` can return
        // this block's bytes (`borrowed[start..end]`) for the emit path.
        self.last_borrowed_block = Some((block_start, block_end));
    }

    /// Make `[block_start, block_end)` the block `last_committed_space`
    /// reports BEFORE the scan runs. The emit pipeline reads
    /// `get_last_space().len()` in `collect_block_parts` *before* calling
    /// `start_matching`, so without this the first borrowed block would
    /// report the whole borrowed window (`last_borrowed_block` still
    /// `None` → `history_bytes()[0..]`), over-reserving the literal buffer
    /// and undercutting the peak-alloc win. Called by the driver when it
    /// stages the range; `start_matching_borrowed` / `skip_matching_borrowed`
    /// re-record the same value idempotently.
    pub(crate) fn stage_borrowed_block(&mut self, block_start: usize, block_end: usize) {
        let (_ptr, total_len) = self
            .borrowed
            .expect("stage_borrowed_block requires a registered borrowed window");
        // Always-on (not debug_assert): the staged range is later sliced by
        // `last_committed_space` as `history_bytes()[start..end]`, so an
        // out-of-range or inverted range would panic deep in the emit path
        // instead of at the staging call site. Validate here to match
        // `start_matching_borrowed` / `skip_matching_borrowed`.
        assert!(
            block_start <= block_end && block_end <= total_len,
            "staged borrowed block bounds out of range: start={block_start} end={block_end} total={total_len}",
        );
        self.last_borrowed_block = Some((block_start, block_end));
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

    /// Borrowed-window equivalent of [`Self::skip_matching_with_hint`]:
    /// the block `[block_start, block_end)` is emitted as RLE / raw
    /// without running the kernel, but its bytes are already resident in
    /// the borrowed window (no `history` append needed). Records the
    /// range for [`Self::last_committed_space`]; on the dict-priming hint
    /// (`Some(false)`) populates the hash table for the range so a later
    /// block can match into this skipped-but-compressible region, exactly
    /// as the owned path does.
    pub(crate) fn skip_matching_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        incompressible_hint: Option<bool>,
    ) {
        let (_ptr, total_len) = self
            .borrowed
            .expect("skip_matching_borrowed requires a registered borrowed window");
        // Always-on (not debug_assert): the recorded range is later sliced
        // by `last_committed_space` for the emit path, and the priming
        // path below does unsafe pointer reads up to `block_end - 8`. An
        // out-of-range `block_end` would be immediate UB even in release,
        // so validate it before storing the range or touching the table —
        // mirrors `start_matching_borrowed`.
        assert!(
            block_start <= block_end && block_end <= total_len,
            "borrowed block bounds out of range: start={block_start} end={block_end} total={total_len}",
        );
        self.last_borrowed_block = Some((block_start, block_end));
        if incompressible_hint == Some(false) {
            self.prime_hash_table_for_range_borrowed(block_start, block_end);
        }
    }

    /// Borrowed-window analogue of [`Self::prime_hash_table_for_range`].
    /// Hashes every position in `[range_start, block_end - HASH_READ_SIZE]`
    /// reading from the borrowed input buffer. Bounded by `block_end`
    /// (not the full buffer) so only the just-committed block's positions
    /// are indexed — future blocks aren't matchable yet, mirroring the
    /// owned path where `history` ends at `block_end`.
    fn prime_hash_table_for_range_borrowed(&mut self, range_start: usize, block_end: usize) {
        const HASH_READ_SIZE: usize = 8;
        if block_end < HASH_READ_SIZE {
            return;
        }
        let last_hashable = block_end - HASH_READ_SIZE;
        // Backfill the (HASH_READ_SIZE - 1) seam positions below
        // `range_start` (see `prime_hash_table_for_range`): the prior
        // block's trailing positions read across the block boundary, so
        // skipping them drops seam-spanning matches between blocks.
        let backfill_start = range_start.saturating_sub(HASH_READ_SIZE - 1);
        if backfill_start > last_hashable {
            return;
        }
        let (base, _len) = self
            .borrowed
            .expect("prime_hash_table_for_range_borrowed requires a registered borrowed window");
        match self.hash_table.mls() {
            4 => self.prime_hash_table_impl::<4>(base, backfill_start, last_hashable),
            5 => self.prime_hash_table_impl::<5>(base, backfill_start, last_hashable),
            6 => self.prime_hash_table_impl::<6>(base, backfill_start, last_hashable),
            7 => self.prime_hash_table_impl::<7>(base, backfill_start, last_hashable),
            8 => self.prime_hash_table_impl::<8>(base, backfill_start, last_hashable),
            _ => unreachable!("FastHashTable construction rejects mls outside 4..=8"),
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
        // Backfill the (HASH_READ_SIZE - 1) positions below `range_start`:
        // their 8-byte hash read straddles the seam into this slice, so
        // without re-hashing them a multi-slice history drops every
        // seam-spanning match. `saturating_sub` floors at HISTORY_DRAIN_BASE
        // (0); re-hashing already-indexed tail positions is idempotent.
        let backfill_start = range_start.saturating_sub(HASH_READ_SIZE - 1);
        if backfill_start > last_hashable {
            return;
        }

        let base = self.history.as_ptr();
        match self.hash_table.mls() {
            4 => self.prime_hash_table_impl::<4>(base, backfill_start, last_hashable),
            5 => self.prime_hash_table_impl::<5>(base, backfill_start, last_hashable),
            6 => self.prime_hash_table_impl::<6>(base, backfill_start, last_hashable),
            7 => self.prime_hash_table_impl::<7>(base, backfill_start, last_hashable),
            8 => self.prime_hash_table_impl::<8>(base, backfill_start, last_hashable),
            _ => unreachable!("FastHashTable construction rejects mls outside 4..=8"),
        }
    }

    /// Monomorphised per-MLS loop body shared by the owned
    /// [`Self::prime_hash_table_for_range`] and the borrowed
    /// [`Self::skip_matching_borrowed`] dict-priming paths. `base` is the
    /// window base pointer (owned `history` or the borrowed input
    /// buffer); positions are absolute window offsets in both.
    fn prime_hash_table_impl<const MLS: u32>(
        &mut self,
        base: *const u8,
        range_start: usize,
        last_hashable: usize,
    ) {
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

    /// Dictionary-priming entry for the donor `dictMatchState` Fast path.
    /// Appends the pending dict slice to `history` and indexes its positions
    /// into the SEPARATE immutable [`Self::dict_table`] — NOT the main hash
    /// table. Keeping dict positions out of the main table is what lets the
    /// dual-probe kernel prefer recent-input matches (main) over dictionary
    /// matches (dict fallback), matching the donor's `prefixStart`/dict split.
    /// Replaces the [`Self::skip_matching_with_hint`]`(Some(false))` call the
    /// driver used to make for Fast-backend priming.
    pub(crate) fn skip_matching_for_dict_prime(&mut self) {
        let block_start = self.extend_history_with_pending();
        self.prime_dict_table_for_range(block_start);
    }

    /// Mark the dict table as fully built (CDict-equivalent). Called by the
    /// driver after the final dictionary chunk has been primed, so the next
    /// frame's [`Self::prime_dict_table_for_range`] skips the re-hash while
    /// the dict bytes are still re-committed to history. Only marks when a
    /// table actually exists — a sub-8-byte dict builds no table and must
    /// re-run the (cheap, no-op) prime path each frame.
    pub(crate) fn mark_dict_primed(&mut self) {
        self.dict.mark_primed();
    }

    /// Drop the cached dict table and its primed flag. Called by the driver
    /// when the next frame carries no dictionary, so the kernel never probes
    /// a stale dict region whose bytes are no longer re-committed.
    pub(crate) fn invalidate_dict_cache(&mut self) {
        self.dict.invalidate();
    }

    /// Build (or extend) [`Self::dict_table`] over `history[range_start..]`,
    /// the freshly-appended dictionary bytes. Lazily allocates the dict table
    /// at the same `(hash_log, mls)` as the main table so one hash keys both.
    fn prime_dict_table_for_range(&mut self, range_start: usize) {
        const HASH_READ_SIZE: usize = 8;
        let history_len = self.history.len();
        // Record the dict/input boundary regardless of whether any position
        // is hashable (a sub-8-byte dict still bounds the input floor).
        self.dict.set_region_len(history_len);
        if self.dict.is_primed() {
            // CDict-equivalent fast path: `dict_table` was built over the
            // identical dictionary bytes on a prior frame, and those bytes
            // sit at the same absolute history positions now (the dict is
            // re-committed before this call). Skip the re-hash entirely.
            return;
        }
        if history_len < HASH_READ_SIZE {
            return;
        }
        let last_hashable = history_len - HASH_READ_SIZE;
        // Backfill the (HASH_READ_SIZE - 1) seam positions below
        // `range_start` (see `prime_hash_table_for_range`): a dictionary
        // fed in slice_size chunks otherwise drops every match that
        // straddles a chunk boundary.
        let backfill_start = range_start.saturating_sub(HASH_READ_SIZE - 1);
        if backfill_start > last_hashable {
            return;
        }
        let hash_log = self.hash_table.hash_log();
        let mls = self.hash_table.mls();
        self.dict
            .table_mut_or_init(|| FastHashTable::new(hash_log, mls));
        let base = self.history.as_ptr();
        match self.hash_table.mls() {
            4 => self.prime_dict_table_impl::<4>(base, backfill_start, last_hashable),
            5 => self.prime_dict_table_impl::<5>(base, backfill_start, last_hashable),
            6 => self.prime_dict_table_impl::<6>(base, backfill_start, last_hashable),
            7 => self.prime_dict_table_impl::<7>(base, backfill_start, last_hashable),
            8 => self.prime_dict_table_impl::<8>(base, backfill_start, last_hashable),
            _ => unreachable!("FastHashTable construction rejects mls outside 4..=8"),
        }
    }

    /// Monomorphised per-MLS dict-table fill. `base` is a raw pointer into
    /// `self.history` (no borrow held), so mutating `self.dict_table` in the
    /// loop is sound — the loop never touches `history`, which stays put.
    fn prime_dict_table_impl<const MLS: u32>(
        &mut self,
        base: *const u8,
        range_start: usize,
        last_hashable: usize,
    ) {
        let dict_table = self
            .dict
            .table_mut()
            .expect("prime_dict_table_for_range creates the table before this call");
        for pos in range_start..=last_hashable {
            // SAFETY: pos <= last_hashable = history_len - 8, so `base.add(pos)`
            // covers >= 8 readable bytes; MLS matches the table's mls.
            let ptr = unsafe { base.add(pos) };
            let hash = unsafe { dict_table.hash_ptr::<MLS>(ptr) };
            unsafe { dict_table.put(hash, pos as u32) };
        }
    }
}

/// Run the Fast kernel over `history[..]` for the block starting at
/// `block_start`, streaming emissions straight to `handle_sequence` and
/// emitting any terminal tail literals. Returns the kernel's two-deep
/// `rep` state for the caller to persist.
///
/// The Fast backend does NOT mutate the matcher's `offset_hist`: repcode
/// probes run off `rep`, and the wire-offset repcode coding is done
/// downstream by `encode_raw_sequences_into` against the encode
/// pipeline's own offset history. So emissions are forwarded verbatim,
/// with no per-match offset-history rotation here.
///
/// A free function (not a method) so the owned and borrowed
/// `start_matching` paths share one copy of the `(mls, use_cmov)`
/// dispatch: passing the disjoint `&mut` borrow of `hash_table` as an
/// explicit parameter sidesteps the `&self`-vs-`&mut self.hash_table`
/// conflict a `&self` accessor would create, the same reason the window
/// slice is selected by the caller and handed in.
#[allow(clippy::too_many_arguments)]
fn run_fast_kernel_block(
    history: &[u8],
    block_start: usize,
    prefix_start_index: u32,
    window_low: u32,
    hash_table: &mut FastHashTable,
    rep_in: [u32; 2],
    step_size: usize,
    mls: u32,
    use_cmov: bool,
    mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
) -> [u32; 2] {
    use super::fast_kernel::kernel::PrefixBounds;

    let bounds = PrefixBounds {
        prefix_start_index,
        window_low,
    };
    // Dispatch on (mls, use_cmov) — each pair monomorphises the kernel
    // hot loop independently. `_` is unreachable: `FastHashTable::new`
    // rejects mls outside 4..=8 at construction.
    let result = match (mls, use_cmov) {
        (4, false) => compress_block_fast::<4, false>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (4, true) => compress_block_fast::<4, true>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (5, false) => compress_block_fast::<5, false>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (5, true) => compress_block_fast::<5, true>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (6, false) => compress_block_fast::<6, false>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (6, true) => compress_block_fast::<6, true>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (7, false) => compress_block_fast::<7, false>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (7, true) => compress_block_fast::<7, true>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (8, false) => compress_block_fast::<8, false>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        (8, true) => compress_block_fast::<8, true>(
            history,
            block_start,
            bounds,
            hash_table,
            rep_in,
            step_size,
            &mut handle_sequence,
        ),
        _ => unreachable!(
            "FastHashTable construction rejects mls outside 4..=8 — \
             got mls={mls} which means the table was bypassed",
        ),
    };

    // Emit terminal literals if the kernel left a tail. `wrap_emit`'s
    // borrow of `handle_sequence` has ended (no use past the match), so
    // calling it directly here is allowed.
    if result.tail_literals_len > 0 {
        let tail_start = history.len() - result.tail_literals_len;
        handle_sequence(Sequence::Literals {
            literals: &history[tail_start..],
        });
    }

    result.rep
}

/// Dictionary-primed counterpart of [`run_fast_kernel_block`]: dispatches the
/// `(mls, use_cmov)` pair to [`compress_block_fast_dict`], threading the
/// immutable `dict_table` alongside the main table. Emits any terminal tail
/// literals exactly as the no-dict helper does.
#[allow(clippy::too_many_arguments)]
fn run_fast_kernel_block_dict(
    history: &[u8],
    block_start: usize,
    bounds: super::fast_kernel::kernel::PrefixBounds,
    dict_end: u32,
    main_table: &mut FastHashTable,
    dict_table: &FastHashTable,
    rep_in: [u32; 2],
    step_size: usize,
    mls: u32,
    use_cmov: bool,
    mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
) -> [u32; 2] {
    use super::fast_kernel::kernel::compress_block_fast_dict;

    macro_rules! run {
        ($mls:literal, $cmov:literal) => {
            compress_block_fast_dict::<$mls, $cmov>(
                history,
                block_start,
                bounds,
                main_table,
                dict_table,
                dict_end,
                rep_in,
                step_size,
                &mut handle_sequence,
            )
        };
    }
    let result = match (mls, use_cmov) {
        (4, false) => run!(4, false),
        (4, true) => run!(4, true),
        (5, false) => run!(5, false),
        (5, true) => run!(5, true),
        (6, false) => run!(6, false),
        (6, true) => run!(6, true),
        (7, false) => run!(7, false),
        (7, true) => run!(7, true),
        (8, false) => run!(8, false),
        (8, true) => run!(8, true),
        _ => unreachable!("FastHashTable construction rejects mls outside 4..=8 — got mls={mls}",),
    };

    if result.tail_literals_len > 0 {
        let tail_start = history.len() - result.tail_literals_len;
        handle_sequence(Sequence::Literals {
            literals: &history[tail_start..],
        });
    }

    result.rep
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
    fn borrowed_window_reads_match_owned_then_restores() {
        let mut m = FastKernelMatcher::new();
        // Owned path: history_bytes mirrors the owned buffer.
        m.history = b"owned-history-bytes".to_vec();
        assert_eq!(m.history_bytes(), b"owned-history-bytes");

        // Borrowed path: history_bytes views the caller's buffer, not
        // the owned one, and the owned buffer is left untouched.
        let external = b"a-different-borrowed-window".to_vec();
        // SAFETY: `external` outlives every history_bytes() call below
        // and is cleared before it drops at end of scope.
        unsafe { m.set_borrowed_window(&external) };
        assert_eq!(m.history_bytes(), &external[..]);
        assert_eq!(m.history, b"owned-history-bytes");

        // Clearing returns to the owned path with the original bytes.
        m.clear_borrowed_window();
        assert_eq!(m.history_bytes(), b"owned-history-bytes");
    }

    #[test]
    fn reset_clears_borrowed_window() {
        let mut m = FastKernelMatcher::new();
        let external = b"borrowed".to_vec();
        // SAFETY: `external` outlives the reset call below.
        unsafe { m.set_borrowed_window(&external) };
        m.reset(
            FAST_LEVEL_1_WINDOW_LOG,
            FAST_LEVEL_1_HASH_LOG,
            FAST_LEVEL_1_MLS,
            2,
            false,
            false,
        );
        // After reset the borrowed window is dropped — back to the
        // (now empty) owned buffer, never the dangling external range.
        assert!(m.borrowed.is_none());
        assert_eq!(m.history_bytes().len(), HISTORY_DRAIN_BASE);
    }

    /// The borrowed one-shot scan must emit the EXACT same sequence
    /// stream (and end with the same rep / offset_hist state) as the
    /// owned block-by-block path. This is the load-bearing correctness
    /// check for the borrowed window: the one-shot frame path relies on
    /// it producing identical output without the per-block history copy.
    #[test]
    fn borrowed_window_matches_owned_sequence_stream() {
        #[derive(PartialEq, Debug)]
        enum Seq {
            Triple(alloc::vec::Vec<u8>, usize, usize),
            Lits(alloc::vec::Vec<u8>),
        }

        // Repeating pattern so the matcher emits real matches, split into
        // two blocks. Window (1 << 15 = 32 KiB) far exceeds the input, so
        // the owned path never evicts — its accumulated `history` is
        // byte-identical to the borrowed buffer, the precondition for
        // stream equality.
        let mut whole = alloc::vec::Vec::with_capacity(320);
        for i in 0..320usize {
            whole.push((i % 64) as u8);
        }
        let split = 192usize;

        // Owned path: commit each block, then scan.
        let mut owned = FastKernelMatcher::with_params(15, 12, 5, 2);
        let mut owned_seqs: alloc::vec::Vec<Seq> = alloc::vec::Vec::new();
        owned.accept_data(whole[..split].to_vec());
        owned.start_matching(|seq| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => owned_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
            Sequence::Literals { literals } => owned_seqs.push(Seq::Lits(literals.to_vec())),
        });
        owned.accept_data(whole[split..].to_vec());
        owned.start_matching(|seq| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => owned_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
            Sequence::Literals { literals } => owned_seqs.push(Seq::Lits(literals.to_vec())),
        });

        // Borrowed path: same bytes, scanned in place by block range.
        let mut borrowed = FastKernelMatcher::with_params(15, 12, 5, 2);
        let mut borrowed_seqs: alloc::vec::Vec<Seq> = alloc::vec::Vec::new();
        // SAFETY: `whole` outlives both scans below; `borrowed` is dropped
        // at end of scope before `whole`, so the window never dangles.
        unsafe { borrowed.set_borrowed_window(&whole) };
        borrowed.start_matching_borrowed(0, split, |seq| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => borrowed_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
            Sequence::Literals { literals } => borrowed_seqs.push(Seq::Lits(literals.to_vec())),
        });
        borrowed.start_matching_borrowed(split, whole.len(), |seq| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => borrowed_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
            Sequence::Literals { literals } => borrowed_seqs.push(Seq::Lits(literals.to_vec())),
        });

        assert_eq!(
            owned_seqs, borrowed_seqs,
            "borrowed window must emit the identical sequence stream as the owned path",
        );
        assert_eq!(
            owned.rep, borrowed.rep,
            "rep state must match after both scans"
        );
        assert_eq!(
            owned.offset_hist, borrowed.offset_hist,
            "offset_hist must match after both scans",
        );
        // The borrowed path must have produced at least one match (else
        // the test would trivially pass on an all-literals stream).
        assert!(
            borrowed_seqs.iter().any(|s| matches!(s, Seq::Triple(..))),
            "pattern must yield at least one match to make the check meaningful",
        );
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
        // (4 MiB) confirms the shift width (`u64` head room).
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
            false,
            false,
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
        m.reset(16, 10, 4, 2, false, false);
        assert_eq!(m.hash_table.hash_log(), 10);
        assert_eq!(m.hash_table.mls(), 4);
        assert_eq!(m.window_log, 16);
        assert_eq!(m.max_window_size, 1usize << 16);
    }

    // The copy-mode restore fast path: a same-shape reset with
    // `table_overwritten_by_restore` must leave the table contents
    // untouched (the snapshot restore replaces them right after), while
    // the plain same-shape reset must memset them. Guards against a
    // refactor silently reintroducing the wasted per-frame clear — or,
    // worse, dropping the clear on the plain path.
    #[test]
    fn reset_keeps_table_when_overwritten_by_restore() {
        let mut m = FastKernelMatcher::new();
        m.reset(16, 10, 4, 2, false, false);
        let probe_hash = 7u32;
        // SAFETY: hash 7 < (1 << hash_log = 1024) table entries.
        unsafe { m.hash_table.put(probe_hash, 0xCAFE) };

        // Same shape + restore-pending: contents survive the reset.
        m.reset(16, 10, 4, 2, false, true);
        // SAFETY: same bounds as the put above.
        assert_eq!(
            unsafe { m.hash_table.get(probe_hash) },
            0xCAFE,
            "restore-pending reset must not clear the table"
        );

        // Plain same-shape reset: contents are memset back to empty.
        m.reset(16, 10, 4, 2, false, false);
        // SAFETY: same bounds as the put above.
        assert_eq!(
            unsafe { m.hash_table.get(probe_hash) },
            0,
            "plain reset must clear the table"
        );

        // Shape change overrides the flag: the table is rebuilt at the
        // new geometry even when a restore is claimed to be pending.
        unsafe { m.hash_table.put(probe_hash, 0xCAFE) };
        m.reset(16, 11, 4, 2, false, true);
        assert_eq!(m.hash_table.hash_log(), 11);
        // SAFETY: hash 7 < (1 << 11) table entries.
        assert_eq!(
            unsafe { m.hash_table.get(probe_hash) },
            0,
            "shape change must rebuild the table regardless of the flag"
        );
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
            "post-append history = retained prefix (≤ max_window_size) \
             + last block (200 bytes); got {}",
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
        // 8-byte real payload appended to history → history.len() =
        // 8 + HISTORY_DRAIN_BASE (= 8 under M8), last_hashable =
        // HISTORY_DRAIN_BASE (= 0), hashed range = [0..=0] (one
        // position).
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

    /// Regression: priming a dictionary in two chunks must hash the
    /// positions straddling the chunk seam. The second chunk's prime
    /// starts at `range_start = end-of-first-chunk`, but a hash read at
    /// any of the preceding `HASH_READ_SIZE - 1` positions spans the
    /// seam into the second chunk — those positions belong to neither
    /// chunk's `[range_start ..= last_hashable]` window unless the second
    /// prime backfills them. Without the backfill a dictionary fed in
    /// `slice_size` chunks silently drops every seam-spanning match.
    #[test]
    fn dict_prime_indexes_positions_across_chunk_seam() {
        // hash_log=20 (1 Mi slots) makes a hash collision among ~32
        // primed positions negligible, so `get(hash(p)) == p` is a
        // deterministic assertion on this fixed input.
        let mut m = FastKernelMatcher::with_params(20, 20, 4, 2);
        let chunk1: alloc::vec::Vec<u8> = (0..16u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(13))
            .collect();
        m.accept_data(chunk1);
        m.skip_matching_for_dict_prime();
        let seam = m.history.len(); // end of first chunk
        let chunk2: alloc::vec::Vec<u8> = (16..32u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(13))
            .collect();
        m.accept_data(chunk2);
        m.skip_matching_for_dict_prime();

        // A position in the (HASH_READ_SIZE - 1)-byte gap just below the
        // seam: its 8-byte hash read straddles the chunk boundary.
        let p = seam - 4;
        let dt = m.dict.table().expect("dict table primed");
        let h = unsafe { dt.hash_ptr::<4>(m.history.as_ptr().add(p)) };
        assert_eq!(
            unsafe { dt.get(h) },
            p as u32,
            "seam-spanning position {p} must be indexed in the dict table",
        );
    }

    /// Regression: same seam-gap defect for the MAIN hash table, primed
    /// per slice via `skip_matching_with_hint(Some(false))` (the dict
    /// COPY path and multi-slice history priming). Cross-slice matches
    /// at the seam are dropped unless the second prime backfills the
    /// trailing `HASH_READ_SIZE - 1` positions.
    #[test]
    fn main_table_prime_indexes_positions_across_slice_seam() {
        let mut m = FastKernelMatcher::with_params(20, 20, 4, 2);
        let chunk1: alloc::vec::Vec<u8> = (0..16u8)
            .map(|i| i.wrapping_mul(53).wrapping_add(7))
            .collect();
        m.accept_data(chunk1);
        m.skip_matching_with_hint(Some(false));
        let seam = m.history.len();
        let chunk2: alloc::vec::Vec<u8> = (16..32u8)
            .map(|i| i.wrapping_mul(53).wrapping_add(7))
            .collect();
        m.accept_data(chunk2);
        m.skip_matching_with_hint(Some(false));

        let p = seam - 4;
        let h = unsafe { m.hash_table.hash_ptr::<4>(m.history.as_ptr().add(p)) };
        assert_eq!(
            unsafe { m.hash_table.get(h) },
            p as u32,
            "seam-spanning position {p} must be indexed in the main table",
        );
    }

    /// After a single block emits matches, the matcher's `rep[0]`
    /// (kernel's `rep_offset1` post-block) must reflect the last emitted
    /// explicit offset — that is the state the next block's kernel probes
    /// against. The matcher's `offset_hist` is NOT updated by matching:
    /// the Fast backend drives repcode probes off `rep`, and the wire
    /// offset coding is done downstream against the encode pipeline's own
    /// offset history, so per-match `encode_offset_with_history` on the
    /// matcher would be redundant work. `offset_hist` therefore stays at
    /// whatever `reset` / `prime_offset_history` set it to.
    #[test]
    fn rep_tracks_last_explicit_offset_and_offset_hist_is_not_matched() {
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

        assert!(
            !emitted_offsets.is_empty(),
            "test setup must produce at least one explicit match",
        );
        let last_explicit = emitted_offsets[emitted_offsets.len() - 1];
        // rep[0] is the functional state: the kernel updates it from its
        // own result each block and the NEXT block probes against it.
        assert_eq!(
            m.rep[0] as usize, last_explicit,
            "kernel's rep[0] must reflect the last emitted explicit offset",
        );
        // offset_hist is NOT touched by matching on the Fast backend — it
        // stays at the construction default (FAST_INITIAL_OFFSET_HIST).
        assert_eq!(
            m.offset_hist, FAST_INITIAL_OFFSET_HIST,
            "Fast matching must not mutate offset_hist (it is seeded only \
             by reset / prime_offset_history; the wire offset coding runs \
             downstream against the encode pipeline's own history)",
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
        // `history_len_for_eviction_accounting` returns the real-data
        // length (history.len() minus HISTORY_DRAIN_BASE, which is 0
        // under M8), so pre/post compare cleanly in real-byte units.
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
        //
        // Pattern period 4 (`(i % 4)`) — dense enough that the donor-
        // parity `kSearchStrength = 8` (K_STEP_INCR = 128) step
        // doubling, which skips positions under step=3 in dict scan,
        // still leaves matches hittable from block2: every position
        // divisible by 4 inside the [in-window, hashed] subset writes
        // the same hash slot, so the slot at block2's first probe
        // contains a recent in-window dict position. Period 64 (the
        // original fixture) only had matches at positions {0, 64,
        // 128, 192} — positions 0, 64, 128 are below the sliding
        // floor and 192 falls in the step-skip gap, leaving the test
        // with zero emittable matches.
        let dict: alloc::vec::Vec<u8> = (0..200u8).map(|i| 0x40 + (i % 4)).collect();
        m.accept_data(dict.clone());
        m.start_matching(|_| {}); // populate hash table from dict
        m.max_window_size = m.max_window_size.saturating_add(200);

        // Add a block whose first 100 bytes match dict[0..100].
        // Without the fix, the kernel would emit offsets up to
        // ~history_len (200..300), since the inflated max_window
        // (328) keeps even the dict's earliest bytes inside the
        // sliding floor.
        let block: alloc::vec::Vec<u8> = (0..100u8).map(|i| 0x40 + (i % 4)).collect();
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

    /// Regression: at block 0 the kernel's prologue must NOT zero
    /// `rep_offset1 = 1` (donor's default initial rep state). Donor
    /// computes `max_rep = ip0 - windowLow` where `windowLow = 0` at
    /// block 0, giving `max_rep = 1` at ip0=1 → `rep_offset1 = 1`
    /// survives (`1 > 1` is false).
    ///
    /// Buggy prologue uses `max_rep = ip0 - prefix_start_index` with
    /// `prefix_start_index = 1` (sentinel-0 floor for hash-table
    /// filtering), giving `max_rep = 0` at ip0=1 → `rep_offset1 = 1 >
    /// 0` → stashed → rep-at-ip2 probe disabled for the ENTIRE first
    /// block.
    ///
    /// Symptom assertion: on a `[0x01, 0x42 × 199]` fixture, donor's
    /// rep-at-ip2 fires at iter 1 (ip2=3, both `read32` reads see
    /// `[42,42,42,42]`). The donor emit sequence is:
    /// `new_ip = ip2 = 3`, `match0 = ip2 - rep_offset1 = 2`, then the
    /// one-byte backward extension absorbs `data[2] == data[1]`
    /// (both `0x42`), giving `new_ip = 2, match_len ≈ 198`. Literal
    /// prefix is `data[0..new_ip] = [0x01, 0x42]` → length 2.
    ///
    /// The buggy path skips rep, walks the cursor via explicit-match
    /// shifts until matchIdx coincides further into the run, and
    /// emits a different literal prefix length. Asserting both
    /// `offset == 1` AND `literals.len() == 2` pins down the
    /// rep-at-ip2 path exactly — the explicit-match catch-up on
    /// uniform-byte data also finds offset=1 via slot collision, so
    /// an offset-only check passes both fixed and buggy paths.
    #[test]
    fn block_zero_prologue_preserves_default_rep_offset_one() {
        let mut data = alloc::vec::Vec::with_capacity(200);
        data.push(0x01);
        data.resize(200, 0x42);

        let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
        m.accept_data(data.clone());

        let mut first_literals_len: Option<usize> = None;
        let mut first_offset: Option<usize> = None;
        m.start_matching(|seq| {
            if first_literals_len.is_some() {
                return;
            }
            if let Sequence::Triple {
                literals, offset, ..
            } = seq
            {
                first_literals_len = Some(literals.len());
                first_offset = Some(offset);
            }
        });

        // Both `offset` AND `literals.len()` are asserted — together
        // they pin down EXACTLY which inner-loop path emitted the
        // first match. With the prologue bug (rep-at-ip2 disabled
        // because `max_rep` was computed against `prefix_start_index`
        // instead of `window_low`), the explicit-match path catches
        // up via hash-slot collision at ip0=3 and STILL emits
        // offset=1 — so an offset-only check would pass both fixed
        // and buggy paths on this uniform-byte fixture. The literal
        // prefix length is the actual discriminator: the rep-at-ip2
        // path consumes only the leading `0x01` literal (1 byte),
        // while the explicit-match catch-up walks past two more
        // `0x42` bytes before firing (3 bytes total). Asserting both
        // values keeps the regression locked to the exact path the
        // fix was meant to preserve.
        assert_eq!(
            first_offset,
            Some(1),
            "first emit must reference offset=1 — donor's default \
             rep_offset1=1 fires on rep-at-ip2 at iter 1, and the \
             prologue MUST NOT zero it (max_rep computed against \
             window_low=0 at block 0, NOT against the sentinel \
             prefix=1)",
        );
        assert_eq!(
            first_literals_len,
            Some(2),
            "first emit must have a 2-byte literal prefix \
             ([0x01, 0x42]) — the rep-at-ip2 probe lands at ip2=3, \
             then the one-byte backward extension drops new_ip to 2, \
             so literals = data[0..2]. A different prefix length \
             would indicate the explicit-match catch-up fired instead",
        );
    }
}
