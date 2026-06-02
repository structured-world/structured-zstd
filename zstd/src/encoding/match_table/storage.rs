//! Shared match-finder storage.
//!
//! `MatchTable` owns every byte of state that both the hash-chain (HC /
//! lazy / lazy2) and binary-tree (BT / optimal parser) backends touch:
//! the rolling window, the contiguous `history` mirror, the absolute
//! position cursors, the hash / hash3 / chain (or BT pointer-pair)
//! tables, and the dictionary-priming flags. Both backends operate on
//! the same physical buffers; the only difference is the semantics of
//! `chain_table` entries — HC mode threads single-link chain pointers
//! through it, BT mode lays out pairs of pointers per node — and that
//! interpretation is the matcher's concern, not the table's.
//!
//! Extracted from `HcMatchGenerator` in #111 Phase 1d Stage 1 so the
//! follow-up stages can pull the HC and BT matchers into their own
//! modules without dragging this shared storage around as a forest of
//! `&mut Vec<u32>` arguments.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::super::Sequence;
use super::super::blocks::encode_offset_with_history;
use super::super::cost_model::{HC_OPT_NUM, HcOptimalCostProfile};
use super::super::hc::HC_MIN_MATCH_LEN;
use super::super::opt::types::{HcOptimalSequence, MatchCandidate};
use super::helpers::INCOMPRESSIBLE_SKIP_STEP;

/// Lookahead bytes the BT walker / optimal parser will read past the
/// per-position absolute cursor (`abs_pos + 9` for match-end sentinels,
/// `abs_pos + HC_OPT_NUM` for the optimal-parser match-length cap, etc.).
/// `MatchTable::add_data` rejects input that would advance
/// `history_abs_end` past `usize::MAX - STREAM_ABS_HEADROOM`, which lets
/// every downstream raw `abs_pos + N` arithmetic stay within `usize` on
/// 32-bit targets without per-call saturating guards.
pub(crate) const STREAM_ABS_HEADROOM: usize = HC_OPT_NUM + 16;

/// Frame-level overflow gate shared by `MatchTable`,
/// `DfastMatchGenerator`, and `RowMatchGenerator`.
///
/// Each backend owns its own rolling window and `history_abs_start`
/// cursor but they all hand absolute positions to inner loops that add
/// small constants without per-iteration overflow checks. This helper
/// enforces a single contract: the next `data.len()` bytes — together
/// with the still-resident `window_size` — must leave at least
/// `STREAM_ABS_HEADROOM` slack below `usize::MAX`. Failing fast here
/// lets every downstream `abs_pos + N` site stay raw and keeps i686
/// streams correct. New match-finder backends with their own
/// `add_data` path must route through this helper.
#[inline]
pub(crate) fn check_stream_abs_headroom(
    history_abs_start: usize,
    window_size: usize,
    data_len: usize,
) {
    let _future_abs_end = history_abs_start
        .checked_add(window_size)
        .and_then(|p| p.checked_add(data_len))
        .and_then(|p| p.checked_add(STREAM_ABS_HEADROOM))
        .expect(
            "structured-zstd: cumulative input would leave less than \
             STREAM_ABS_HEADROOM (HC_OPT_NUM + 16) slack below \
             `usize::MAX` for the encoder's absolute-position \
             lookahead; on 32-bit targets, split the input into \
             smaller frames or use a 64-bit build",
        );
}

#[cfg(test)]
mod bt_pair_index_wrap_tests {
    use super::MatchTable;

    /// `bt_pair_index_for_abs` switched from `+` to `wrapping_add` so the
    /// release-mode overflow branch and the debug-mode panic stay off
    /// the hot path on rare 32-bit streams where `abs_pos + index_shift`
    /// overflows `usize`. This test forces that overflow and verifies
    /// the returned BT slot still equals what the modulo-ring identity
    /// promises: `(abs_pos + index_shift) mod 2^bt_log`, doubled
    /// because the table stores pointer pairs.
    #[test]
    fn bt_pair_index_matches_modulo_ring_after_wraparound() {
        let mut table = MatchTable::new(1 << 20);
        // Small BT ring (`bt_log = chain_log - 1 = 3` → mask = 0b0111) so
        // the modular identity is easy to read at a glance.
        table.chain_log = 4;
        // Use a wide `index_shift` so the addition wraps for many of
        // the `abs_pos` values we probe below.
        table.index_shift = usize::MAX - 5;

        let bt_mask = table.bt_mask();
        assert_eq!(bt_mask, 0b0111);

        for abs_pos in [0usize, 1, 4, 5, 6, 7, 8, 12, 17] {
            let got = table.bt_pair_index_for_abs(abs_pos);
            let expected = 2 * (abs_pos.wrapping_add(table.index_shift) & bt_mask);
            assert_eq!(
                got, expected,
                "abs_pos={abs_pos}: wrapping_add ring slot must match the masked sum"
            );
        }

        // Spot-check one value where overflow is certain (abs_pos > 5)
        // and the ring index has a stable closed form.
        // abs_pos=7, index_shift=usize::MAX-5: sum wraps to 1, mask -> 1, doubled -> 2.
        assert_eq!(table.bt_pair_index_for_abs(7), 2);
        // abs_pos=14: sum wraps to 8, masked -> 0, doubled -> 0.
        assert_eq!(table.bt_pair_index_for_abs(14), 0);
    }

    /// Sanity check the non-overflow path keeps the same identity so a
    /// future refactor cannot regress the common case while leaving the
    /// overflow case green.
    #[test]
    fn bt_pair_index_matches_modulo_ring_without_overflow() {
        let mut table = MatchTable::new(1 << 20);
        table.chain_log = 8; // bt_log = 7, mask = 0x7f
        table.index_shift = 17;

        let bt_mask = table.bt_mask();
        for abs_pos in [0usize, 1, 16, 32, 64, 127, 128, 255, 1 << 20] {
            let got = table.bt_pair_index_for_abs(abs_pos);
            let expected = 2 * ((abs_pos + table.index_shift) & bt_mask);
            assert_eq!(got, expected, "abs_pos={abs_pos}");
        }
    }
}

#[cfg(test)]
mod stream_abs_headroom_tests {
    use super::{STREAM_ABS_HEADROOM, check_stream_abs_headroom};

    #[test]
    fn accepts_exactly_at_the_boundary() {
        // `history_abs_start + window_size + data_len + STREAM_ABS_HEADROOM == usize::MAX`.
        let history_abs_start = usize::MAX - STREAM_ABS_HEADROOM - 2;
        check_stream_abs_headroom(history_abs_start, 1, 1);
    }

    #[test]
    fn accepts_well_below_the_boundary() {
        check_stream_abs_headroom(0, 1 << 20, 1 << 20);
    }

    #[test]
    #[should_panic(expected = "STREAM_ABS_HEADROOM")]
    fn rejects_one_byte_past_the_boundary() {
        // One byte over: sum = usize::MAX + 1 → checked_add returns None.
        let history_abs_start = usize::MAX - STREAM_ABS_HEADROOM - 1;
        check_stream_abs_headroom(history_abs_start, 1, 1);
    }

    #[test]
    #[should_panic(expected = "STREAM_ABS_HEADROOM")]
    fn rejects_history_abs_start_already_too_high() {
        check_stream_abs_headroom(usize::MAX - 10, 0, 0);
    }
}

/// Knuth-style 3-byte hash multiplier. Donor parity:
/// `ZSTD_HASH3PRIME` in `lib/compress/zstd_compress_internal.h`. Used
/// by the HC3 short-match side table and by the 3-byte branch of the
/// generic `hash_value_with_mls`.
pub(crate) const HC_PRIME3BYTES: u32 = 506_832_829;
/// Knuth-style 4-byte hash multiplier. Donor parity: `ZSTD_HASHPRIME`.
pub(crate) const HC_PRIME4BYTES: u32 = 2_654_435_761;

/// Hash / chain / hash3 sentinel marking an empty slot.
///
/// The donor uses position `0` as the sentinel because absolute
/// positions are stored as `relative_position + 1`, so a stored zero
/// never collides with a real position. Kept here so storage helpers
/// don't have to pull it from the matcher modules.
pub(crate) const HC_EMPTY: u32 = 0;

// Default table-log constants — the canonical (and only) definitions.
// `match_generator.rs` re-imports the names so existing macros / configs
// can keep referring to them unqualified; do NOT shadow these values
// there with a second `const HC_*_LOG = ...;` declaration. Drift between
// the two copies caused Phase 1d review feedback that this comment
// guards against re-introducing.

/// Default `hash_log` for the level-7 hash-chain matcher. Real values
/// are written directly into [`MatchTable::hash_log`] by the matcher's
/// `configure()` call once the driver resolves the compression level;
/// this constant only seeds the field for matchers that haven't been
/// configured yet.
pub(crate) const HC_HASH_LOG: usize = 20;
/// Default `chain_log` for HC mode (also the pointer-pair log for BT
/// mode — same table reused).
pub(crate) const HC_CHAIN_LOG: usize = 19;
/// Default `hash3_log` for the HC3 short-match side table. Only
/// allocated when the `btultra2` / `btopt` cascade asks for it; HC
/// modes leave it sized to zero.
pub(crate) const HC3_HASH_LOG: usize = 17;

/// Shared storage backing every match finder. Holds the contiguous
/// history buffer, the rolling window, and the hash / chain / hash3
/// tables. Methods on this struct contain only logic that's identical
/// between HC and BT modes — backend-specific table interpretation
/// lives in the matcher modules.
pub(crate) struct MatchTable {
    pub(crate) max_window_size: usize,
    /// Per-chunk lengths of the live window, in add order. The bytes
    /// themselves live only in `history` (the contiguous mirror); this
    /// deque tracks chunk boundaries for eviction and locates the
    /// current block (the back chunk) as a tail slice of `history`,
    /// so the live region is never duplicated.
    pub(crate) chunk_lens: VecDeque<usize>,
    pub(crate) window_size: usize,
    pub(crate) history: Vec<u8>,
    pub(crate) history_start: usize,
    pub(crate) history_abs_start: usize,
    pub(crate) position_base: usize,
    pub(crate) index_shift: usize,
    pub(crate) offset_hist: [u32; 3],
    pub(crate) hash_table: Vec<u32>,
    pub(crate) hash3_table: Vec<u32>,
    pub(crate) chain_table: Vec<u32>,
    pub(crate) hash_log: usize,
    pub(crate) chain_log: usize,
    pub(crate) hash3_log: usize,
    pub(crate) next_to_update3: usize,
    pub(crate) skip_insert_until_abs: usize,
    pub(crate) dictionary_limit_abs: Option<usize>,
    pub(crate) dictionary_primed_for_frame: bool,
    pub(crate) allow_zero_relative_position: bool,
    /// HC chain-walk depth, mirrored from `HcMatcher::search_depth` during
    /// `configure()`. Stage D moves the BT walker onto this struct, and the
    /// walker macros read the depth from `$table.search_depth` directly so
    /// the call sites don't have to plumb it through.
    pub(crate) search_depth: usize,
    /// Whether the active parser is `btultra2`. Mirrored from the
    /// current [`super::super::strategy::StrategyTag`] during
    /// `configure()` so the BT walker and rebase machinery can stay
    /// on `MatchTable` without consulting the outer generator.
    pub(crate) is_btultra2: bool,
    /// Whether the active backend is one of the BT parsers (`btopt`,
    /// `btultra`, `btultra2`). Mirrored from the active strategy for
    /// the same reason as `is_btultra2`.
    pub(crate) uses_bt: bool,
}

impl MatchTable {
    pub(crate) fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            chunk_lens: VecDeque::new(),
            window_size: 0,
            history: Vec::new(),
            history_start: 0,
            history_abs_start: 0,
            position_base: 0,
            index_shift: 0,
            offset_hist: [1, 4, 8],
            hash_table: Vec::new(),
            hash3_table: Vec::new(),
            chain_table: Vec::new(),
            hash_log: HC_HASH_LOG,
            chain_log: HC_CHAIN_LOG,
            hash3_log: HC3_HASH_LOG,
            next_to_update3: 0,
            skip_insert_until_abs: 0,
            dictionary_limit_abs: None,
            dictionary_primed_for_frame: false,
            allow_zero_relative_position: false,
            search_depth: 0,
            is_btultra2: false,
            uses_bt: false,
        }
    }

    /// Cheap precondition check: can the rebase guard for `abs_pos`
    /// (against the eventual `max_abs_pos`) be skipped because every
    /// involved position is already trivially representable as a
    /// `(rel + 1)` u32? The `is_btultra2` flag tweaks the boundary
    /// rule: BtUltra2 allows `abs_pos == history_abs_start` even when
    /// `allow_zero_relative_position` is `false`, matching the donor
    /// btultra2 seed-pass behaviour.
    #[inline(always)]
    pub(crate) fn can_skip_rebase_check_at(
        &self,
        abs_pos: usize,
        max_abs_pos: usize,
        is_btultra2: bool,
    ) -> bool {
        let max_rel_no_rebase = (u32::MAX as usize).saturating_sub(2);
        self.position_base == 0
            && self.index_shift == 0
            && max_abs_pos <= max_rel_no_rebase
            && (self.allow_zero_relative_position
                || abs_pos > self.history_abs_start
                || (is_btultra2 && abs_pos == self.history_abs_start))
    }

    /// Decide whether the table needs a cold rebase before `abs_pos`
    /// can be inserted. Pure predicate — does **not** perform the
    /// rebase. The caller (whichever backend owns the BT walk path)
    /// is responsible for invoking `rebase_positions_cold` when this
    /// returns `true`. Hot path: ~once per byte, so the function is
    /// kept tight and `#[inline]`.
    #[inline]
    pub(crate) fn needs_rebase(&self, abs_pos: usize, is_btultra2: bool) -> bool {
        if is_btultra2
            && !self.allow_zero_relative_position
            && self.position_base == 0
            && abs_pos == 0
        {
            return false;
        }
        self.relative_position(abs_pos)
            .is_none_or(|relative| relative >= u32::MAX - 1)
    }

    /// Insert a position into the HC3 short-match side table without
    /// running the rebase check. Caller is responsible for ensuring
    /// the position is already representable (or that the rebase
    /// guard upstream already cleared it). Donor parity: the inner
    /// `ZSTD_insertAndFindFirstIndexHash3` body.
    pub(crate) fn insert_hash3_only_no_rebase(&mut self, abs_pos: usize) {
        if self.hash3_log == 0 {
            return;
        }
        let idx = abs_pos - self.history_abs_start;
        let concat = &self.history[self.history_start..];
        if idx + 4 > concat.len() {
            return;
        }
        let Some(relative_pos) = self.relative_position(abs_pos) else {
            return;
        };
        let hash3 = Self::hash_position_at(concat, idx, self.hash3_log, 3);
        self.hash3_table[hash3] = relative_pos + 1;
    }

    /// Insert a position into the main hash / chain table without
    /// running the rebase check. Caller pre-validates that the
    /// position is representable as a `(rel + 1)` u32, either via
    /// `maybe_rebase_positions` (HC) or `bt_update_tree_until` (BT).
    /// Donor parity: `ZSTD_insertAndFindFirstIndex` inner body.
    #[inline]
    pub(crate) fn insert_position_no_rebase(&mut self, abs_pos: usize) {
        let idx = abs_pos.wrapping_sub(self.history_abs_start);
        let concat = &self.history[self.history_start..];
        if idx + 4 > concat.len() {
            return;
        }
        let hash = Self::hash_position_at(concat, idx, self.hash_log, 4);
        let Some(relative_pos) = self.relative_position(abs_pos) else {
            return;
        };
        let stored = relative_pos + 1;
        let chain_mask = (1usize << self.chain_log) - 1;
        let chain_idx = relative_pos as usize & chain_mask;
        // SAFETY: `hash` is produced by `hash_value_with_mls` which masks
        // the result down to `hash_log` bits, and `hash_table.len() == 1 <<
        // hash_log` (`ensure_tables`). `chain_idx` is `& chain_mask` so
        // `< chain_table.len() == 1 << chain_log`. Both indices are
        // provably in bounds, so the elided bounds checks save ~4
        // instructions per call on this per-byte-of-input hot path.
        debug_assert!(hash < self.hash_table.len());
        debug_assert!(chain_idx < self.chain_table.len());
        unsafe {
            let prev = *self.hash_table.get_unchecked(hash);
            *self.chain_table.get_unchecked_mut(chain_idx) = prev;
            *self.hash_table.get_unchecked_mut(hash) = stored;
        }
    }

    /// Allocate the hash / chain / hash3 tables sized to the current
    /// `hash_log` / `chain_log` / `hash3_log` configuration. Each table
    /// is reallocated whenever its current length doesn't match the
    /// width implied by its `*_log` field — `insert_position_no_rebase`
    /// reaches via `get_unchecked`, so a stale-width table after a
    /// level change would index out of bounds (UB) on the next encode.
    pub(crate) fn ensure_tables(&mut self) {
        let hash_size = 1 << self.hash_log;
        if self.hash_table.len() != hash_size {
            self.hash_table = alloc::vec![HC_EMPTY; hash_size];
        }

        let chain_size = 1 << self.chain_log;
        if self.chain_table.len() != chain_size {
            self.chain_table = alloc::vec![HC_EMPTY; chain_size];
        }

        let hash3_size = if self.hash3_log == 0 {
            0
        } else {
            1 << self.hash3_log
        };
        if self.hash3_table.len() != hash3_size {
            self.hash3_table = alloc::vec![HC_EMPTY; hash3_size];
        }
    }

    /// Unaligned little-endian `u32` load. Hot helper for every
    /// `hash_position*` site. Donor parity: `MEM_readLE32`.
    #[inline(always)]
    pub(crate) fn read_le_u32(data: &[u8]) -> u32 {
        debug_assert!(data.len() >= 4);
        unsafe { Self::read_le_u32_ptr(data.as_ptr()) }
    }

    /// Pointer variant of [`read_le_u32`]. Used from macros that
    /// already hold a raw pointer.
    ///
    /// # Safety
    /// `ptr` must be valid for a `u32` read.
    #[inline(always)]
    pub(crate) unsafe fn read_le_u32_ptr(ptr: *const u8) -> u32 {
        unsafe { u32::from_le(core::ptr::read_unaligned(ptr as *const u32)) }
    }

    /// MLS-parameterised hash of a 32-bit value into a `hash_log`-bit
    /// index. Donor parity: the `mls`-switch in `ZSTD_hashPtr`.
    #[inline(always)]
    pub(crate) fn hash_value_with_mls(value: u32, hash_log: usize, mls: usize) -> usize {
        match mls {
            3 => (((value << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - hash_log)) as usize,
            _ => ((value.wrapping_mul(HC_PRIME4BYTES)) >> (32 - hash_log)) as usize,
        }
    }

    /// Hash a 4-byte window at the head of `data`.
    #[inline(always)]
    pub(crate) fn hash_position_with_mls(data: &[u8], hash_log: usize, mls: usize) -> usize {
        let value = Self::read_le_u32(data);
        Self::hash_value_with_mls(value, hash_log, mls)
    }

    /// Hash a 4-byte window starting at `idx` inside `data`. Skips the
    /// slice subrange to keep the bounds check off the per-byte hot
    /// path.
    #[inline(always)]
    pub(crate) fn hash_position_at(data: &[u8], idx: usize, hash_log: usize, mls: usize) -> usize {
        debug_assert!(idx + 4 <= data.len());
        let value = unsafe { Self::read_le_u32_ptr(data.as_ptr().add(idx)) };
        Self::hash_value_with_mls(value, hash_log, mls)
    }

    /// Main hash for the current matcher (4-byte MLS, `hash_log` from
    /// the table's configuration).
    #[inline(always)]
    pub(crate) fn hash_position(&self, data: &[u8]) -> usize {
        Self::hash_position_with_mls(data, self.hash_log, 4)
    }

    /// 3-byte hash used by the HC3 side table. Test-only — the
    /// production path uses inlined per-kernel variants.
    #[cfg(test)]
    pub(crate) fn hash3_position(data: &[u8], hash_log: usize) -> usize {
        let value = Self::read_le_u32(data);
        (((value << 8).wrapping_mul(HC_PRIME3BYTES)) >> (32 - hash_log)) as usize
    }

    /// Mark this frame as dictionary-primed so the HC / BT seed paths
    /// know to honour the dictionary boundary.
    pub(crate) fn mark_dictionary_primed(&mut self) {
        self.dictionary_primed_for_frame = true;
    }

    /// Set the per-frame dictionary boundary in absolute coordinates.
    /// `primed_len == 0` clears the limit.
    pub(crate) fn set_dictionary_limit_from_primed_bytes(&mut self, primed_len: usize) {
        self.dictionary_limit_abs = if primed_len == 0 {
            None
        } else {
            Some(self.history_abs_start.saturating_add(primed_len))
        };
    }

    /// Append a freshly committed buffer to the rolling window. Drops
    /// chunk-length entries for the oldest slices until the new total
    /// fits inside `max_window_size`, extends the contiguous `history`
    /// mirror with the new bytes, then hands the *input* buffer back
    /// through `reuse_space` for pool reuse — `history` now owns the
    /// bytes, so the input buffer carries no live data. (Callers must
    /// therefore treat the callback as recycle-only, not as an eviction
    /// report; eviction bytes come from the `window_size` delta.)
    pub(crate) fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        check_stream_abs_headroom(self.history_abs_start, self.window_size, data.len());
        while self.window_size + data.len() > self.max_window_size {
            let removed_len = self.chunk_lens.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        self.compact_history();
        let added = data.len();
        self.history.extend_from_slice(&data);
        self.next_to_update3 = self.next_to_update3.max(self.history_abs_start);
        self.window_size += added;
        self.chunk_lens.push_back(added);
        // The input buffer's bytes now live in `history`; hand the buffer
        // straight back to the caller's pool instead of holding a second
        // copy in the window (the source of the per-compress duplicate).
        reuse_space(data);
    }

    /// Drop window slices that have rolled past `max_window_size`.
    /// Used after `max_window_size` shrinks (dictionary release path).
    pub(crate) fn trim_to_window(&mut self) {
        while self.window_size > self.max_window_size {
            let removed_len = self.chunk_lens.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
    }

    /// Drain the dead prefix of `history` (already-rolled-out bytes)
    /// when it has grown to at least half the live region. Keeps the
    /// contiguous mirror compact so reallocation costs stay amortised.
    pub(crate) fn compact_history(&mut self) {
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

    /// The live (post-`history_start`) slice of the contiguous history
    /// mirror. Match finders operate on this slice rather than the raw
    /// `history` Vec.
    pub(crate) fn live_history(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    /// Absolute position one past the end of the live history.
    pub(crate) fn history_abs_end(&self) -> usize {
        self.history_abs_start + self.live_history().len()
    }

    /// Get a reference to the last committed window slice. Returns
    /// the most recent buffer in the rolling window — panics if no
    /// data has been committed yet.
    pub(crate) fn get_last_space(&self) -> &[u8] {
        let last = *self.chunk_lens.back().unwrap();
        &self.history[self.history.len() - last..]
    }

    /// Test-only: append a chunk to the live window without eviction /
    /// compaction, mirroring the storage side of [`add_data`]. Replaces
    /// the old `window.push_back(vec)` setup now that chunk bytes live
    /// only in `history`.
    #[cfg(test)]
    pub(crate) fn push_test_chunk(&mut self, data: Vec<u8>) {
        self.history.extend_from_slice(&data);
        self.window_size += data.len();
        self.chunk_lens.push_back(data.len());
    }

    /// Convert an absolute position into the (relative_pos + 1) form
    /// stored in the hash / chain tables. Returns `None` for positions
    /// outside the current window's representable range. Donor parity:
    /// matches the `relIdx` arithmetic in `ZSTD_HcFindBestMatch`.
    pub(crate) fn relative_position(&self, abs_pos: usize) -> Option<u32> {
        let shifted_abs = abs_pos.checked_add(self.index_shift)?;
        let rel = shifted_abs.checked_sub(self.position_base)?;
        let rel_u32 = u32::try_from(rel).ok()?;
        // Donor parity: raw BT/HC tables use 0 as the empty sentinel, so
        // the very first absolute position in the first block
        // (curr == 0) is not a representable candidate index.
        if !self.allow_zero_relative_position && self.position_base == 0 && rel_u32 == 0 {
            return None;
        }
        // Positions are stored as (relative_pos + 1), with 0 reserved
        // as the empty sentinel. So the raw relative position itself
        // must stay strictly below u32::MAX.
        (rel_u32 < u32::MAX).then_some(rel_u32)
    }

    /// Lower bound (in absolute positions) of the window that's still
    /// reachable from `target_abs`. Donor parity: `windowLow` in
    /// `ZSTD_compressBlock_*`.
    pub(crate) fn window_low_abs_for_target(&self, target_abs: usize) -> usize {
        let history_low = self.history_abs_start;
        let window_low = target_abs.saturating_sub(self.max_window_size);
        history_low.max(window_low)
    }

    /// BT pointer-pair log: chain_log minus one because the table
    /// stores pairs of pointers (smaller / larger) per node.
    #[inline(always)]
    pub(crate) fn bt_log(&self) -> usize {
        self.chain_log.saturating_sub(1)
    }

    /// BT pointer-pair address mask. Donor parity: `(1 << btLog) - 1`.
    #[inline(always)]
    pub(crate) fn bt_mask(&self) -> usize {
        (1usize << self.bt_log()) - 1
    }

    /// Convert an absolute position into a BT pair index in
    /// `chain_table`. Each node occupies two consecutive slots
    /// (smaller, larger) so the result is doubled. Donor parity:
    /// `2 * (curr & btMask)` from `ZSTD_insertBt1`.
    #[inline(always)]
    pub(crate) fn bt_pair_index_for_abs(&self, abs_pos: usize) -> usize {
        // Hot per-iteration BT walker entry. `abs_pos` is a
        // frame-lifetime absolute stream cursor (capped by
        // `check_stream_abs_headroom`); `index_shift` is block-local
        // (`current_len` during the btultra2 seed pass, `0` otherwise).
        // The result is immediately masked down to the BT ring width
        // by `& bt_mask()`, so what matters here is only the modulo-
        // ring identity `(a + b) mod m == ((a mod 2^bits) + (b mod
        // 2^bits)) mod m` for `m | 2^bits`: `wrapping_add` preserves
        // that identity even on the rare i686 streams where the raw
        // `usize` sum overflows. Using `wrapping_add` instead of `+`
        // (or `saturating_add`) also keeps the release-mode overflow
        // branch and the debug-mode overflow panic off this hot path.
        let bt_pos = abs_pos.wrapping_add(self.index_shift);
        2 * (bt_pos & self.bt_mask())
    }

    /// Decode a stored hash / chain table entry back into its absolute
    /// position. Returns `None` for the `HC_EMPTY` sentinel or for
    /// entries that underflowed after `index_shift` was applied. Pure
    /// associated function — kept off `&self` so macros can pass the
    /// constituent fields directly when partial-borrow shenanigans
    /// would block a `&self` call.
    #[inline(always)]
    pub(crate) fn stored_abs_position_fast(
        stored: u32,
        position_base: usize,
        index_shift: usize,
    ) -> Option<usize> {
        if stored == HC_EMPTY {
            return None;
        }
        let shifted = position_base + (stored as usize - 1);
        if shifted < index_shift {
            return None;
        }
        Some(shifted - index_shift)
    }

    /// Reset the per-frame portion of the storage. The hash / chain /
    /// hash3 tables themselves are zeroed in place (via
    /// `Vec::fill(HC_EMPTY)`) if they're already sized; otherwise
    /// they're left empty so the next `ensure_tables()` call resizes
    /// them. The window is now just `chunk_lens` (the bytes live in
    /// `history`, which this method clears below), so there are no
    /// per-block buffers to drain; `_reuse_space` is retained only for
    /// caller signature compatibility and is intentionally unused.
    pub(crate) fn reset(&mut self, _reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        self.chunk_lens.clear();
        self.history.clear();
        self.history_start = 0;
        self.history_abs_start = 0;
        self.position_base = 0;
        self.index_shift = 0;
        self.offset_hist = [1, 4, 8];
        self.next_to_update3 = 0;
        self.skip_insert_until_abs = 0;
        self.dictionary_limit_abs = None;
        self.dictionary_primed_for_frame = false;
        self.allow_zero_relative_position = false;
        // Clear each table independently — `Vec::fill` on an empty Vec
        // is a no-op, so unconditional fills are safe even when a table
        // hasn't been allocated yet (HC mode keeps hash3_table empty,
        // and the backend-switch path swaps every table for Vec::new()
        // to release oversized allocations).
        self.hash_table.fill(HC_EMPTY);
        self.hash3_table.fill(HC_EMPTY);
        self.chain_table.fill(HC_EMPTY);
    }

    /// Donor parity: `ZSTD_compressBlock_btopt_generic` starts its main
    /// match loop at cursor `1` (not `0`) whenever the current block sits
    /// at the absolute history origin — the byte at offset `0` is
    /// reserved for the seed literal so the parser never reports a
    /// zero-offset match. The same flag governs the initial `litlen`
    /// because the seed literal counts as one pending literal byte.
    pub(crate) fn donor_opt_start_cursor_and_litlen(
        &self,
        current_abs_start: usize,
    ) -> (usize, usize) {
        let start_cursor = usize::from(current_abs_start == self.history_abs_start);
        (start_cursor, start_cursor)
    }

    /// Stage D: BT walker step. Cross-platform dispatcher that picks
    /// the per-kernel variant so the per-iteration
    /// `count_match_from_indices` symbol inlines under the kernel's
    /// `target_feature` umbrella. Previously lived on `BtMatcher`
    /// but the body uses only table state plus `self.search_depth`,
    /// so it migrates onto `MatchTable` and clears the cross-struct
    /// borrow that blocked the rest of the BT update chain.
    #[inline(always)]
    pub(crate) fn bt_insert_step_no_rebase(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_insert_step_no_rebase_neon(abs_pos, current_abs_end, target_abs)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_insert_step_no_rebase_avx2_bmi2(abs_pos, current_abs_end, target_abs)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_insert_step_no_rebase_sse42(abs_pos, current_abs_end, target_abs)
                },
                FastpathKernel::Scalar => {
                    self.bt_insert_step_no_rebase_scalar(abs_pos, current_abs_end, target_abs)
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.bt_insert_step_no_rebase_scalar(abs_pos, current_abs_end, target_abs)
        }
    }

    /// NEON umbrella BT walker step.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline).
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn bt_insert_step_no_rebase_neon(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_step_no_rebase_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::neon::count_match_from_indices
        )
    }

    /// SSE4.2 umbrella BT walker step.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    pub(crate) unsafe fn bt_insert_step_no_rebase_sse42(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_step_no_rebase_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::sse42::count_match_from_indices
        )
    }

    /// AVX2+BMI2 umbrella BT walker step.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 + BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    pub(crate) unsafe fn bt_insert_step_no_rebase_avx2_bmi2(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_step_no_rebase_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices
        )
    }

    /// Scalar fallback BT walker step (used on non-AArch64 targets).
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    pub(crate) fn bt_insert_step_no_rebase_scalar(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        target_abs: usize,
    ) -> usize {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_step_no_rebase_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            target_abs,
            crate::encoding::fastpath::scalar::count_match_from_indices
        )
    }

    /// Stage D: cross-platform dispatcher for the BT collect-matches walker.
    /// External / test entry — the hot path bypasses this and calls the
    /// per-kernel variant from inside the surrounding
    /// `collect_optimal_candidates_initialized_<kernel>` umbrella.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    pub(crate) fn bt_insert_and_collect_matches(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_insert_and_collect_matches_neon(
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                best_len_for_skip,
                out,
            )
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_insert_and_collect_matches_avx2_bmi2(
                        abs_pos,
                        current_abs_end,
                        profile,
                        min_match_len,
                        best_len_for_skip,
                        out,
                    )
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_insert_and_collect_matches_sse42(
                        abs_pos,
                        current_abs_end,
                        profile,
                        min_match_len,
                        best_len_for_skip,
                        out,
                    )
                },
                FastpathKernel::Scalar => self.bt_insert_and_collect_matches_scalar(
                    abs_pos,
                    current_abs_end,
                    profile,
                    min_match_len,
                    best_len_for_skip,
                    out,
                ),
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.bt_insert_and_collect_matches_scalar(
                abs_pos,
                current_abs_end,
                profile,
                min_match_len,
                best_len_for_skip,
                out,
            )
        }
    }

    /// NEON-umbrella variant of `bt_insert_and_collect_matches`.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline).
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bt_insert_and_collect_matches_neon(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_and_collect_matches_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::neon::count_match_from_indices,
        )
    }

    /// SSE4.2 umbrella variant of `bt_insert_and_collect_matches`.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bt_insert_and_collect_matches_sse42(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_and_collect_matches_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::sse42::count_match_from_indices,
        )
    }

    /// AVX2+BMI2 umbrella variant of `bt_insert_and_collect_matches`.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 + BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn bt_insert_and_collect_matches_avx2_bmi2(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_and_collect_matches_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::avx2_bmi2::count_match_from_indices,
        )
    }

    /// Scalar fallback BT collect-matches walker.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bt_insert_and_collect_matches_scalar(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
        profile: HcOptimalCostProfile,
        min_match_len: usize,
        best_len_for_skip: &mut usize,
        out: &mut Vec<MatchCandidate>,
    ) {
        let search_depth = self.search_depth;
        super::super::match_generator::bt_insert_and_collect_matches_body!(
            self,
            search_depth,
            abs_pos,
            current_abs_end,
            profile,
            min_match_len,
            best_len_for_skip,
            out,
            crate::encoding::fastpath::scalar::count_match_from_indices,
        )
    }

    /// BT-side history replay after [`Self::begin_rebase`]. Re-walks
    /// `history_start..abs_pos` through the BT step so the pointer-pair
    /// table is consistent with the freshly reset `position_base`.
    pub(crate) fn replay_history_for_rebase_bt(&mut self, history_start: usize, abs_pos: usize) {
        let rebuild_end = self.history_abs_end();
        let mut pos = history_start;
        while pos < abs_pos {
            let forward = self.bt_insert_step_no_rebase(pos, rebuild_end, abs_pos);
            // `pos` is a frame-lifetime absolute cursor that can approach
            // `usize::MAX` on long 32-bit streams. Cap the step at the
            // remaining distance to `abs_pos` so the addition stays
            // within `usize` even when the BT walker returns a large
            // `forward` near the stream end.
            let step = forward.max(1).min(abs_pos - pos);
            pos += step;
        }
    }

    /// Stage D: BT-tree update dispatcher. Picks the kernel-specific
    /// variant so the per-iteration BT walker inlines under the
    /// surrounding `target_feature` umbrella.
    #[inline(always)]
    pub(crate) fn bt_update_tree_until(&mut self, abs_pos: usize, current_abs_end: usize) {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.bt_update_tree_until_neon(abs_pos, current_abs_end)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.bt_update_tree_until_avx2_bmi2(abs_pos, current_abs_end)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.bt_update_tree_until_sse42(abs_pos, current_abs_end)
                },
                FastpathKernel::Scalar => {
                    self.bt_update_tree_until_scalar(abs_pos, current_abs_end)
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.bt_update_tree_until_scalar(abs_pos, current_abs_end)
        }
    }

    /// NEON-umbrella variant: per-iteration `bt_insert_step_no_rebase_neon`
    /// inlines into the body because both share the
    /// `target_feature = "neon"` umbrella.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline).
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn bt_update_tree_until_neon(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
    ) {
        if self.skip_insert_until_abs < self.history_abs_start {
            self.skip_insert_until_abs = self.history_abs_start;
        }
        let mut update_abs = self.skip_insert_until_abs;
        let is_btultra2 = self.is_btultra2;
        while update_abs < abs_pos {
            if !self.can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2) {
                self.maybe_rebase_positions(update_abs);
            }
            // SAFETY: same NEON umbrella; direct call inlines the BT-walk body.
            let forward =
                unsafe { self.bt_insert_step_no_rebase_neon(update_abs, current_abs_end, abs_pos) };
            update_abs += forward.max(1).min(abs_pos - update_abs);
        }
        self.skip_insert_until_abs = abs_pos;
    }

    /// SSE4.2 umbrella variant.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    pub(crate) unsafe fn bt_update_tree_until_sse42(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
    ) {
        if self.skip_insert_until_abs < self.history_abs_start {
            self.skip_insert_until_abs = self.history_abs_start;
        }
        let mut update_abs = self.skip_insert_until_abs;
        let is_btultra2 = self.is_btultra2;
        while update_abs < abs_pos {
            if !self.can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2) {
                self.maybe_rebase_positions(update_abs);
            }
            let forward = unsafe {
                self.bt_insert_step_no_rebase_sse42(update_abs, current_abs_end, abs_pos)
            };
            update_abs += forward.max(1).min(abs_pos - update_abs);
        }
        self.skip_insert_until_abs = abs_pos;
    }

    /// AVX2+BMI2 umbrella variant.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 + BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    pub(crate) unsafe fn bt_update_tree_until_avx2_bmi2(
        &mut self,
        abs_pos: usize,
        current_abs_end: usize,
    ) {
        if self.skip_insert_until_abs < self.history_abs_start {
            self.skip_insert_until_abs = self.history_abs_start;
        }
        let mut update_abs = self.skip_insert_until_abs;
        let is_btultra2 = self.is_btultra2;
        while update_abs < abs_pos {
            if !self.can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2) {
                self.maybe_rebase_positions(update_abs);
            }
            let forward = unsafe {
                self.bt_insert_step_no_rebase_avx2_bmi2(update_abs, current_abs_end, abs_pos)
            };
            update_abs += forward.max(1).min(abs_pos - update_abs);
        }
        self.skip_insert_until_abs = abs_pos;
    }

    /// Scalar fallback used on non-AArch64 targets.
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    pub(crate) fn bt_update_tree_until_scalar(&mut self, abs_pos: usize, current_abs_end: usize) {
        if self.skip_insert_until_abs < self.history_abs_start {
            self.skip_insert_until_abs = self.history_abs_start;
        }
        let mut update_abs = self.skip_insert_until_abs;
        let is_btultra2 = self.is_btultra2;
        while update_abs < abs_pos {
            if !self.can_skip_rebase_check_at(update_abs, abs_pos, is_btultra2) {
                self.maybe_rebase_positions(update_abs);
            }
            let forward =
                self.bt_insert_step_no_rebase_scalar(update_abs, current_abs_end, abs_pos);
            update_abs += forward.max(1).min(abs_pos - update_abs);
        }
        self.skip_insert_until_abs = abs_pos;
    }

    /// Hash3-only fill up to (but not including) `abs_pos`. Rebase
    /// guard fires only when `can_skip_rebase_check_at` says we can't
    /// trivially skip — the fast path is a tight loop over `hash3_table`
    /// writes.
    pub(crate) fn update_hash3_until(&mut self, abs_pos: usize) {
        let is_btultra2 = self.is_btultra2;
        if self.next_to_update3 < self.history_abs_start {
            self.next_to_update3 = self.history_abs_start;
        }
        if self.next_to_update3 >= abs_pos {
            return;
        }
        while self.next_to_update3 < abs_pos {
            if !self.can_skip_rebase_check_at(self.next_to_update3, abs_pos, is_btultra2) {
                self.maybe_rebase_positions(self.next_to_update3);
            }
            self.insert_hash3_only_no_rebase(self.next_to_update3);
            // hash3 cursor strictly less than `abs_pos` here (loop guard);
            // `+ 1` cannot overflow within encode block sizes.
            self.next_to_update3 += 1;
        }
    }

    /// Hot wrapper for the rebase guard. Fast path is a single
    /// [`Self::needs_rebase`] check; the cold rebuild is a separate
    /// `#[cold]` function so the i-cache stays warm on the common
    /// "no rebase needed" branch.
    #[inline]
    pub(crate) fn maybe_rebase_positions(&mut self, abs_pos: usize) {
        let is_btultra2 = self.is_btultra2;
        if self.needs_rebase(abs_pos, is_btultra2) {
            self.rebase_positions_cold(abs_pos);
        }
    }

    /// Cold rebase: clear the hash / hash3 / chain tables and replay
    /// the inserted history prefix through the active backend's walker
    /// so the new `position_base` is consistent. The `uses_bt` flag
    /// (mirrored from `HcParseMode`) selects between the HC and BT
    /// replay variants.
    #[cold]
    #[inline(never)]
    pub(crate) fn rebase_positions_cold(&mut self, abs_pos: usize) {
        self.begin_rebase();
        let history_start = self.history_abs_start;
        // Rebuild only the already-inserted prefix. The caller inserts abs_pos
        // immediately after this, and later positions are added in-order.
        if self.uses_bt {
            self.replay_history_for_rebase_bt(history_start, abs_pos);
        } else {
            self.replay_history_for_rebase_hc(history_start, abs_pos);
        }
        // begin_rebase() also clears `hash3_table`. Rewind `next_to_update3`
        // to the inserted prefix start so `update_hash3_until` re-fills the
        // HC3 side table — without this, every HC3 probe up to `abs_pos`
        // returns "empty" until the encoder catches up, which silently
        // changes btultra2 short-match selection on long-running streams.
        self.next_to_update3 = history_start;
        if self.hash3_log != 0 {
            self.update_hash3_until(abs_pos);
        }
    }

    /// Insert a single position into the hash / chain tables, rebasing
    /// first if required.
    #[inline]
    pub(crate) fn insert_position(&mut self, abs_pos: usize) {
        self.maybe_rebase_positions(abs_pos);
        self.insert_position_no_rebase(abs_pos);
    }

    /// Insert every position in `[start, end)` into the hash / chain
    /// tables and advance the hash3 fill cursor past `end`.
    pub(crate) fn insert_positions(&mut self, start: usize, end: usize) {
        for pos in start..end {
            self.insert_position(pos);
        }
        self.next_to_update3 = self.next_to_update3.max(end);
    }

    /// Insert every `step`-th position in `[start, end)` — the sparse
    /// counterpart to [`Self::insert_positions`]. Skipped positions are
    /// *not* advanced through `next_to_update3` (the donor's behaviour
    /// for the "incompressible block" skip path).
    pub(crate) fn insert_positions_with_step(&mut self, start: usize, end: usize, step: usize) {
        if step == 0 {
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

    /// Backfill the last `< 4` bytes of the previous slice (which couldn't
    /// be hashed at the time because `insert_position` needs 4 bytes of
    /// lookahead) before the next slice's matching pass.
    pub(crate) fn backfill_boundary_positions(
        &mut self,
        current_abs_start: usize,
        current_abs_end: usize,
    ) {
        let backfill_start = current_abs_start
            .saturating_sub(3)
            .max(self.history_abs_start);
        if backfill_start < current_abs_start {
            if self.uses_bt {
                self.bt_update_tree_until(current_abs_start, current_abs_end);
            } else {
                self.insert_positions(backfill_start, current_abs_start);
            }
        }
    }

    /// After a long BT match the optimal parser may have skipped a huge
    /// stretch of `skip_insert_until_abs`. Cap the gap at 384 to bound
    /// the worst-case BT tree update on the next match search.
    pub(crate) fn apply_limited_update_after_long_match(&mut self, current_abs_start: usize) {
        if !self.uses_bt {
            return;
        }
        let gap = current_abs_start.saturating_sub(self.skip_insert_until_abs);
        if gap > 384 {
            self.skip_insert_until_abs = current_abs_start - (gap - 384).min(192);
        }
    }

    /// BT-mode counterpart of the HC sparse skip path. Inserts every
    /// `INCOMPRESSIBLE_SKIP_STEP`-th position through the BT walker
    /// (which threads the binary tree) and then densely inserts the
    /// final `HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP` positions so
    /// the very last hashable starts are present for the next slice.
    pub(crate) fn bt_insert_sparse_incompressible_block(
        &mut self,
        current_abs_start: usize,
        current_abs_end: usize,
    ) {
        let mut pos = current_abs_start;
        while pos < current_abs_end {
            self.maybe_rebase_positions(pos);
            let _ = self.bt_insert_step_no_rebase(pos, current_abs_end, current_abs_end);
            self.insert_hash3_only_no_rebase(pos);
            let next = pos.saturating_add(INCOMPRESSIBLE_SKIP_STEP);
            if next <= pos {
                break;
            }
            pos = next;
        }

        let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
        let tail_start = current_abs_end
            .saturating_sub(dense_tail)
            .max(self.history_abs_start)
            .max(current_abs_start);
        for pos in tail_start..current_abs_end {
            if (pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                continue;
            }
            self.maybe_rebase_positions(pos);
            let _ = self.bt_insert_step_no_rebase(pos, current_abs_end, current_abs_end);
            self.insert_hash3_only_no_rebase(pos);
        }

        self.skip_insert_until_abs = self.skip_insert_until_abs.max(current_abs_end);
        self.next_to_update3 = self.next_to_update3.max(current_abs_end);
    }

    /// `skip_matching` body — backfill the slice boundary and then walk
    /// the current slice in either dense or sparse mode (driven by the
    /// `incompressible_hint`). BT and HC modes branch via `uses_bt`.
    pub(crate) fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_tables();
        let current_len = *self.chunk_lens.back().unwrap();
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        self.backfill_boundary_positions(current_abs_start, current_abs_end);
        if self.uses_bt {
            if incompressible_hint == Some(true) {
                self.bt_insert_sparse_incompressible_block(current_abs_start, current_abs_end);
                return;
            }
            self.bt_update_tree_until(current_abs_end, current_abs_end);
            return;
        }
        if incompressible_hint == Some(true) {
            self.insert_positions_with_step(
                current_abs_start,
                current_abs_end,
                INCOMPRESSIBLE_SKIP_STEP,
            );
            let dense_tail = HC_MIN_MATCH_LEN + INCOMPRESSIBLE_SKIP_STEP;
            let tail_start = current_abs_end
                .saturating_sub(dense_tail)
                .max(self.history_abs_start);
            let tail_start = tail_start.max(current_abs_start);
            for pos in tail_start..current_abs_end {
                if !(pos - current_abs_start).is_multiple_of(INCOMPRESSIBLE_SKIP_STEP) {
                    self.insert_position(pos);
                }
            }
        } else {
            self.insert_positions(current_abs_start, current_abs_end);
        }
    }

    /// Stage D: HC3 short-match probe. Cross-platform dispatcher.
    /// External / test callers only — the on-encode hot path bypasses
    /// this via the per-kernel variant from inside the surrounding
    /// `collect_optimal_candidates_initialized_<kernel>` umbrella.
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn hash3_candidate(
        &self,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        unsafe {
            self.hash3_candidate_neon(abs_pos, current_abs_end, min_match_len)
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::{FastpathKernel, select_kernel};
            match select_kernel() {
                FastpathKernel::Avx2Bmi2 => unsafe {
                    self.hash3_candidate_avx2_bmi2(abs_pos, current_abs_end, min_match_len)
                },
                FastpathKernel::Sse42 => unsafe {
                    self.hash3_candidate_sse42(abs_pos, current_abs_end, min_match_len)
                },
                FastpathKernel::Scalar => {
                    self.hash3_candidate_scalar(abs_pos, current_abs_end, min_match_len)
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_endian = "little"),
            target_arch = "x86",
            target_arch = "x86_64"
        )))]
        {
            self.hash3_candidate_scalar(abs_pos, current_abs_end, min_match_len)
        }
    }

    /// NEON umbrella HC3 probe.
    ///
    /// # Safety
    /// AArch64 with NEON (baseline). Body inlines via macro.
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn hash3_candidate_neon(
        &self,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        super::super::match_generator::hash3_candidate_body!(
            self,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
        )
    }

    /// SSE4.2 umbrella HC3 probe.
    ///
    /// # Safety
    /// x86/x86_64 with SSE4.2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse4.2")]
    pub(crate) unsafe fn hash3_candidate_sse42(
        &self,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        super::super::match_generator::hash3_candidate_body!(
            self,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::sse42::common_prefix_len_ptr,
        )
    }

    /// AVX2+BMI2 umbrella HC3 probe.
    ///
    /// # Safety
    /// x86/x86_64 with AVX2 + BMI2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2,bmi2")]
    pub(crate) unsafe fn hash3_candidate_avx2_bmi2(
        &self,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        super::super::match_generator::hash3_candidate_body!(
            self,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
        )
    }

    /// Scalar fallback HC3 probe (used on non-AArch64 targets).
    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    pub(crate) fn hash3_candidate_scalar(
        &self,
        abs_pos: usize,
        current_abs_end: usize,
        min_match_len: usize,
    ) -> Option<MatchCandidate> {
        super::super::match_generator::hash3_candidate_body!(
            self,
            abs_pos,
            current_abs_end,
            min_match_len,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
        )
    }

    /// Reset the rebase-derived bookkeeping (rolling `position_base` /
    /// `index_shift`) so every stored position re-encodes from
    /// `history_abs_start`, then clear the three index tables. Hot
    /// path for [`Self::rebase_positions_cold`]; the caller is
    /// responsible for re-inserting any positions the active
    /// matchfinder still needs.
    pub(crate) fn begin_rebase(&mut self) {
        self.position_base = self.history_abs_start;
        self.index_shift = 0;
        self.allow_zero_relative_position = true;
        self.hash_table.fill(HC_EMPTY);
        self.hash3_table.fill(HC_EMPTY);
        self.chain_table.fill(HC_EMPTY);
    }

    /// HC-side history replay after [`begin_rebase`]. Re-inserts every
    /// position from `history_start` (inclusive) to `abs_pos`
    /// (exclusive) into the HC chain/hash tables without re-checking
    /// the rebase guard — the caller has just rebased, so positions
    /// are by construction representable.
    pub(crate) fn replay_history_for_rebase_hc(&mut self, history_start: usize, abs_pos: usize) {
        for pos in history_start..abs_pos {
            self.insert_position_no_rebase(pos);
        }
    }

    /// Donor parity: replay an optimal-parser plan into the consumer's
    /// sequence sink. Reads the current input frame off `window` and
    /// advances `offset_hist` exactly like the donor block-store walker.
    pub(crate) fn emit_optimal_plan(
        &mut self,
        current_len: usize,
        plan: &[HcOptimalSequence],
        handle_sequence: &mut impl for<'a> FnMut(Sequence<'a>),
    ) {
        // Direct field borrow of `history` (not `get_last_space()`, which
        // borrows all of `self`) so the per-sequence `&mut self.offset_hist`
        // write below stays a disjoint-field borrow.
        let current = &self.history[self.history.len() - current_len..];
        if plan.is_empty() {
            handle_sequence(Sequence::Literals { literals: current });
            return;
        }

        let mut literals_start = 0usize;
        for item in plan {
            let lit_len = item.lit_len as usize;
            let match_len = item.match_len as usize;
            // checked_add on both edges — mirrors
            // `BtMatcher::update_plan_stats_segment`. A malformed plan
            // entry can't overflow `usize` arithmetic before the
            // `> current_len` guard fires.
            let Some(start) = literals_start.checked_add(lit_len) else {
                continue;
            };
            let Some(end) = start.checked_add(match_len) else {
                continue;
            };
            if end > current_len {
                continue;
            }
            let literals = &current[literals_start..start];
            handle_sequence(Sequence::Triple {
                literals,
                offset: item.offset as usize,
                match_len,
            });
            encode_offset_with_history(item.offset, literals.len() as u32, &mut self.offset_hist);
            literals_start = end;
        }

        if literals_start < current_len {
            handle_sequence(Sequence::Literals {
                literals: &current[literals_start..],
            });
        }
    }
}

#[cfg(test)]
mod storage_tests {
    //! Stage D coverage for `MatchTable` entry points that the
    //! end-to-end compression path doesn't naturally hit on CI:
    //!  * `set_dictionary_limit_from_primed_bytes(0)` — the "clear"
    //!    branch is only entered when a dictionary frame is reset.
    //!  * BT-mode incompressible-block skip path on `skip_matching`.
    //!  * `replay_history_for_rebase_bt` — exercised only when the
    //!    BT cursor crosses the rolling-rebase threshold (~`u32::MAX`),
    //!    so we drive it directly here.
    use alloc::vec;

    use super::*;

    fn new_table(window: usize) -> MatchTable {
        let mut t = MatchTable::new(window);
        // window_size is driven by `push_test_chunk` (= sum of live chunk
        // lengths), so it is not preset here.
        t.hash_log = 8;
        t.chain_log = 8;
        t.hash3_log = 0;
        t
    }

    #[test]
    fn set_dictionary_limit_from_primed_bytes_zero_clears_limit() {
        let mut t = new_table(64);
        t.history_abs_start = 100;
        t.dictionary_limit_abs = Some(123);
        t.set_dictionary_limit_from_primed_bytes(0);
        assert_eq!(t.dictionary_limit_abs, None);
    }

    #[test]
    fn set_dictionary_limit_from_primed_bytes_offsets_from_history_start() {
        let mut t = new_table(64);
        t.history_abs_start = 100;
        t.set_dictionary_limit_from_primed_bytes(40);
        assert_eq!(t.dictionary_limit_abs, Some(140));
    }

    #[test]
    fn skip_matching_bt_incompressible_routes_through_sparse_block() {
        let mut t = new_table(32);
        t.push_test_chunk(vec![0u8; 32]);
        t.ensure_tables();
        t.uses_bt = true;
        t.is_btultra2 = false;
        t.search_depth = 4;
        let before_skip_until = t.skip_insert_until_abs;
        t.skip_matching(Some(true));
        // BT + incompressible path must take the
        // `bt_insert_sparse_incompressible_block` branch and advance
        // `skip_insert_until_abs` to current_abs_end.
        assert!(t.skip_insert_until_abs >= t.window_size);
        assert!(t.skip_insert_until_abs > before_skip_until);
    }

    #[test]
    fn skip_matching_bt_dense_routes_through_bt_update_tree() {
        let mut t = new_table(32);
        t.push_test_chunk(vec![1u8; 32]);
        t.ensure_tables();
        t.uses_bt = true;
        t.is_btultra2 = false;
        t.search_depth = 4;
        // `incompressible_hint = None` → dense bt_update_tree_until path
        t.skip_matching(None);
        assert_eq!(t.skip_insert_until_abs, t.history_abs_start + t.window_size);
    }

    #[test]
    fn replay_history_for_rebase_bt_walks_inserted_prefix() {
        let mut t = new_table(64);
        // Construct a contiguous mirror long enough for the BT walker
        // (`bt_insert_step_no_rebase` reads 8-byte prefixes).
        t.history = vec![0u8; 64];
        for (i, slot) in t.history.iter_mut().enumerate() {
            *slot = (i % 17) as u8;
        }
        t.history_start = 0;
        t.history_abs_start = 0;
        t.window_size = 64;
        t.position_base = 0;
        t.search_depth = 4;
        t.uses_bt = true;
        t.ensure_tables();
        // Replay the first 32 positions; the BT walker writes entries
        // into the hash table (via `hash_table[hash] = stored`) so the
        // ground-truth observation is "some hash slots are no longer
        // HC_EMPTY".
        assert!(t.hash_table.iter().all(|&v| v == HC_EMPTY));
        t.replay_history_for_rebase_bt(0, 32);
        assert!(
            t.hash_table.iter().any(|&v| v != HC_EMPTY),
            "BT replay must populate hash table"
        );
    }

    #[test]
    fn begin_rebase_clears_index_tables_and_resets_base() {
        let mut t = new_table(32);
        t.hash_table = vec![7; 16];
        t.chain_table = vec![9; 16];
        t.hash3_table = vec![5; 16];
        t.history_abs_start = 50;
        t.position_base = 0;
        t.index_shift = 4;
        t.allow_zero_relative_position = false;

        t.begin_rebase();

        assert_eq!(t.position_base, 50);
        assert_eq!(t.index_shift, 0);
        assert!(t.allow_zero_relative_position);
        assert!(t.hash_table.iter().all(|&v| v == HC_EMPTY));
        assert!(t.chain_table.iter().all(|&v| v == HC_EMPTY));
        assert!(t.hash3_table.iter().all(|&v| v == HC_EMPTY));
    }

    /// Regression: `rebase_positions_cold` must replay the HC3 side
    /// table along with the main hash / chain replay. `begin_rebase`
    /// zeroes `hash3_table`, so without an explicit refill every HC3
    /// probe before `abs_pos` returns "empty" until the next encode
    /// position falls due. On long-running btultra2 streams that
    /// silently changes match selection (the btultra2 cascade leans
    /// heavily on HC3 short matches).
    #[test]
    fn rebase_positions_cold_rebuilds_hash3_for_btultra2() {
        let mut t = new_table(64);
        t.history = b"abcdef_abcdef_abcdef_abcdef_abcdef_abcdef".to_vec();
        t.history_start = 0;
        t.history_abs_start = 0;
        t.window_size = t.history.len();
        // `history` is set directly above; just record it as one live chunk.
        t.chunk_lens.push_back(t.history.len());
        t.hash_log = 8;
        t.chain_log = 8;
        // btultra2-style: HC3 side table allocated.
        t.hash3_log = 6;
        t.is_btultra2 = true;
        t.search_depth = 4;
        t.ensure_tables();

        // Pre-fill the HC3 table the way the encoder would by walking
        // positions up to the would-be rebase point.
        t.update_hash3_until(20);
        assert!(
            t.hash3_table.iter().any(|&v| v != HC_EMPTY),
            "fixture precondition: hash3 must be non-empty before rebase"
        );

        t.rebase_positions_cold(20);

        assert!(
            t.hash3_table.iter().any(|&v| v != HC_EMPTY),
            "rebase must repopulate the HC3 side table — \
             btultra2 short-match selection depends on it"
        );
    }

    #[test]
    fn insert_positions_with_step_zero_step_is_noop() {
        let mut t = new_table(32);
        t.history = vec![0u8; 32];
        t.push_test_chunk(vec![0u8; 32]);
        t.ensure_tables();
        let next_to_update3_before = t.next_to_update3;
        // step=0 must early-return without touching anything.
        t.insert_positions_with_step(0, 16, 0);
        assert!(t.hash_table.iter().all(|&v| v == HC_EMPTY));
        assert_eq!(t.next_to_update3, next_to_update3_before);
    }

    #[test]
    fn insert_positions_with_step_saturating_step_breaks_loop() {
        // step = usize::MAX so first iteration overflows
        // `pos.saturating_add(step)` to usize::MAX, then the `next <= pos`
        // guard breaks out of the loop after one insert.
        let mut t = new_table(32);
        t.history = vec![1u8; 32];
        t.push_test_chunk(vec![1u8; 32]);
        t.ensure_tables();
        t.insert_positions_with_step(0, 16, usize::MAX);
        // Exactly one position should have been inserted before the
        // loop terminated — observe that only one slot is non-empty.
        let non_empty = t.hash_table.iter().filter(|&&v| v != HC_EMPTY).count();
        assert!(
            non_empty <= 1,
            "step=usize::MAX must break after the first insert"
        );
    }

    #[test]
    fn apply_limited_update_after_long_match_hc_mode_is_noop() {
        // HC mode (`uses_bt = false`) — function must early-return
        // without mutating `skip_insert_until_abs`.
        let mut t = new_table(32);
        t.uses_bt = false;
        t.skip_insert_until_abs = 100;
        t.apply_limited_update_after_long_match(1000);
        assert_eq!(
            t.skip_insert_until_abs, 100,
            "HC mode must not adjust skip cursor"
        );
    }

    #[test]
    fn apply_limited_update_after_long_match_bt_mode_caps_gap_at_384() {
        // BT mode with gap > 384 → cap the skip cursor so future
        // `bt_update_tree_until` doesn't walk an unbounded prefix.
        let mut t = new_table(32);
        t.uses_bt = true;
        t.skip_insert_until_abs = 0;
        // current_abs_start = 1000 → gap = 1000 → cap subtracts
        // (gap - 384).min(192) = 192, so result is 1000 - 192 = 808.
        t.apply_limited_update_after_long_match(1000);
        assert_eq!(t.skip_insert_until_abs, 808);
    }

    #[test]
    fn apply_limited_update_after_long_match_small_gap_is_noop() {
        let mut t = new_table(32);
        t.uses_bt = true;
        t.skip_insert_until_abs = 800;
        // gap = 200 < 384 → no change.
        t.apply_limited_update_after_long_match(1000);
        assert_eq!(t.skip_insert_until_abs, 800);
    }

    #[test]
    fn emit_optimal_plan_empty_plan_emits_full_literals() {
        let mut t = new_table(8);
        t.push_test_chunk(b"abcdefgh".to_vec());
        let mut emitted: Vec<u8> = Vec::new();
        t.emit_optimal_plan(8, &[], &mut |seq| {
            if let Sequence::Literals { literals } = seq {
                emitted.extend_from_slice(literals);
            }
        });
        assert_eq!(emitted, b"abcdefgh");
    }

    #[test]
    fn emit_optimal_plan_skips_oversized_plan_item_and_emits_trailing_literals() {
        let mut t = new_table(8);
        t.push_test_chunk(b"abcdefgh".to_vec());
        // Plan item asks for `start + match_len > current_len` → skip.
        // The function must still emit the trailing literals at the end.
        let plan = [HcOptimalSequence {
            offset: 1,
            lit_len: 4,
            match_len: 99, // overflows the 8-byte window → continue
        }];
        let mut triples = 0usize;
        let mut trailing: Vec<u8> = Vec::new();
        t.emit_optimal_plan(8, &plan, &mut |seq| match seq {
            Sequence::Triple { .. } => triples += 1,
            Sequence::Literals { literals } => trailing.extend_from_slice(literals),
        });
        assert_eq!(triples, 0, "oversized plan item must be skipped");
        assert_eq!(
            trailing, b"abcdefgh",
            "trailing-literals path must emit the full window when plan skipped everything"
        );
    }
}
