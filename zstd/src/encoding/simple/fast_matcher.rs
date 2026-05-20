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
        let space = self
            .pending
            .take()
            .expect("extend_history_with_pending without a pending buffer");

        // Lazy eviction: only fires when retained bytes would actually
        // exceed the donor's 2× soft cap. For typical inputs that fit
        // in a single window the branch is cold.
        let new_total = self.history.len().saturating_add(space.len());
        let cap = self.max_window_size.saturating_mul(2);
        if new_total > cap {
            let target_retained = self.max_window_size;
            let drop_n = self.history.len().saturating_sub(target_retained);
            if drop_n > 0 {
                self.history.drain(..drop_n);
                self.prefix_start_index = self.prefix_start_index.saturating_add(drop_n as u32);
                // The hash table holds ABSOLUTE positions into the
                // pre-drain history; after draining, those positions
                // point at evicted bytes. Clearing forces a fresh
                // population from the new block onward — donor pays
                // the same cost on `ZSTD_window_clear` after a stale
                // reset.
                self.hash_table.clear();
            }
        }

        let block_start = self.history.len();
        self.history.extend_from_slice(&space);
        block_start
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
    /// The `_incompressible_hint` parameter is accepted for the
    /// `Matcher::skip_matching_with_hint` trait method's signature
    /// compatibility; the Fast matcher doesn't make decisions from it
    /// today (the driver has already decided to skip by the time it
    /// calls this), but keeping the parameter avoids re-shaping the
    /// trait when the heuristic moves into the matcher in a future
    /// commit.
    pub(crate) fn skip_matching(&mut self, _incompressible_hint: Option<bool>) {
        let _block_start = self.extend_history_with_pending();
        // Rep state survives unchanged: skip should look idempotent
        // to the next block's matcher (no fake match implies no rep
        // promotion). offset_hist likewise unchanged.
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
        assert!(m.last_committed_space().is_empty());
        assert!(m.pending.is_none());
        // History grew by exactly the block size.
        assert_eq!(m.history.len(), data.len());
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

        m.skip_matching(None);

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
            m.skip_matching(None);
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
        assert!(
            m.prefix_start_index > 0,
            "prefix_start_index must have advanced after an eviction",
        );
    }
}
