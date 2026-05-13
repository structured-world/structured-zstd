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
    pub(crate) window: VecDeque<Vec<u8>>,
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
}

impl MatchTable {
    pub(crate) fn new(max_window_size: usize) -> Self {
        Self {
            max_window_size,
            window: VecDeque::new(),
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
        }
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
    /// `hash_log` / `chain_log` / `hash3_log` configuration. No-op if
    /// the main hash_table is already sized; the backend-switch path
    /// clears it to `Vec::new()` to force a fresh allocation here on
    /// the next frame.
    pub(crate) fn ensure_tables(&mut self) {
        if self.hash_table.is_empty() {
            self.hash_table = alloc::vec![HC_EMPTY; 1 << self.hash_log];
            let hash3_size = if self.hash3_log == 0 {
                0
            } else {
                1 << self.hash3_log
            };
            self.hash3_table = alloc::vec![HC_EMPTY; hash3_size];
            self.chain_table = alloc::vec![HC_EMPTY; 1 << self.chain_log];
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

    /// Append a freshly committed buffer to the rolling window. Evicts
    /// the oldest slices until the new total fits inside
    /// `max_window_size`, hands them back through `reuse_space` for
    /// pool reuse, then extends the contiguous `history` mirror.
    ///
    /// History duplicates window data for O(1) contiguous access during
    /// match finding (`common_prefix_len`, `extend_backwards`). Peak:
    /// ~2x window size for data buffers + 6 MB tables.
    pub(crate) fn add_data(&mut self, data: Vec<u8>, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(data.len() <= self.max_window_size);
        while self.window_size + data.len() > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
        }
        self.compact_history();
        self.history.extend_from_slice(&data);
        self.next_to_update3 = self.next_to_update3.max(self.history_abs_start);
        self.window_size += data.len();
        self.window.push_back(data);
    }

    /// Drop window slices that have rolled past `max_window_size`.
    /// Used after `max_window_size` shrinks (dictionary release path).
    pub(crate) fn trim_to_window(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        while self.window_size > self.max_window_size {
            let removed = self.window.pop_front().unwrap();
            self.window_size -= removed.len();
            self.history_start += removed.len();
            self.history_abs_start += removed.len();
            reuse_space(removed);
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
        self.window.back().unwrap().as_slice()
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
        2 * (abs_pos.saturating_add(self.index_shift) & self.bt_mask())
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
    /// them. Window buffers are drained through `reuse_space` so the
    /// driver can recycle them across frames.
    pub(crate) fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
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
        for mut data in self.window.drain(..) {
            data.resize(data.capacity(), 0);
            reuse_space(data);
        }
    }
}
