//! Donor-shape Fast strategy matcher backend (level 1).
//!
//! Wraps the kernel from
//! [`super::fast_kernel::kernel::compress_block_fast`] and presents the
//! `Matcher` API expected by [`crate::encoding::match_generator::MatchGeneratorDriver`].
//! Replaces the SuffixStore-based `MatchGenerator` for the Fast strategy
//! path with a donor-parity hash table and tight per-block loop.
//!
//! Phase 1b scaffold: this file currently defines the matcher's state
//! and lifecycle hooks (`new` / `reset`); the `Matcher` trait
//! implementation and dispatch wiring land in the follow-up commits
//! on the same PR. The struct is `pub(crate)` so the wiring commit
//! can hook it into [`crate::encoding::match_generator::MatcherStorage`]
//! without churn here.
//!
//! The narrowly-scoped `#![allow(dead_code)]` covers exactly the
//! items the next commit consumes — leaving `unused_imports` active
//! so any stray import in sibling code is still flagged. The allow
//! is removed in the wiring commit that hooks the matcher into the
//! driver.
#![allow(dead_code)]

use alloc::vec::Vec;

use super::fast_kernel::hash_table::FastHashTable;

/// Donor `ZSTD_defaultCParameters[level=1][srcSize > 256 KiB][Fast]` —
/// the parameter set the C reference encoder picks when the caller
/// asks for level 1 on inputs larger than the small-source cutoff.
/// Used as the entry-time defaults for [`FastKernelMatcher::new`];
/// the reset path can rebind to other `(hash_log, mls, window_log)`
/// triples once the source-size hint resolves a smaller window
/// (smaller inputs drop `hash_log` proportionally).
pub(crate) const FAST_LEVEL_1_HASH_LOG: u32 = 14;
pub(crate) const FAST_LEVEL_1_MLS: u32 = 7;
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
    offset_hist: [u32; 3],
    /// Flat hash table indexed by donor `hash_ptr<MLS>`. Persistent
    /// across blocks; only `reset` (or a `(hash_log, mls)` parameter
    /// change) reallocates it.
    hash_table: FastHashTable,
    /// `1 << window_log`. Soft upper bound on `history.len()` — once
    /// the buffer grows past this point the prefix is dropped and
    /// `prefix_start_index` advances.
    max_window_size: usize,
    /// Decoder-side window size (in `log` bits). Reported to the
    /// frame header via the `Matcher::window_size` trait method.
    window_log: u8,
    /// Holds a `commit_space`'d block until `start_matching` consumes
    /// it. `None` between frames and immediately after `start_matching`
    /// returns. The driver guarantees at most one outstanding pending
    /// space at a time (single-block-per-cycle protocol).
    pending: Option<Vec<u8>>,
}

impl FastKernelMatcher {
    /// Build a fresh matcher with the donor's level-1 defaults baked
    /// in. The driver re-invokes [`Self::reset`] on every frame, so
    /// these defaults are only what the matcher carries until the
    /// first `reset` call — they exist so `MatchGeneratorDriver::new`
    /// can construct the matcher without committing to a level yet.
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
            history: Vec::new(),
            prefix_start_index: 0,
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
        self.prefix_start_index = 0;
        self.rep = FAST_INITIAL_REP;
        self.offset_hist = FAST_INITIAL_OFFSET_HIST;
        self.window_log = window_log;
        self.max_window_size = 1usize << window_log;
        self.pending = None;
    }

    /// Reported decoder-side window size (bytes).
    ///
    /// Equals `1 << window_log`. The driver forwards this through the
    /// `Matcher::window_size` trait method into the frame header.
    pub(crate) fn window_size(&self) -> u64 {
        self.max_window_size as u64
    }

    /// Read-only view of the most recently committed (pending) space.
    /// Returns the empty slice between `start_matching` calls. Used by
    /// the driver's `get_last_space` trait method.
    pub(crate) fn last_committed_space(&self) -> &[u8] {
        match self.pending.as_deref() {
            Some(slice) => slice,
            None => &[],
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
        assert_eq!(m.prefix_start_index, 0);
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
        assert_eq!(m.prefix_start_index, 0);
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
}
