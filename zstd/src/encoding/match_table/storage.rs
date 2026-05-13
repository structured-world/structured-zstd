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
