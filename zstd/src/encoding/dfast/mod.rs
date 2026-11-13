//! Double-fast match finder (default-level backend, upstream zstd parity for
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
use core::convert::TryInto;

use super::Sequence;
use super::dict_attach::{DictAttach, DictKeep};
use super::fastpath::EncodeTier;
use super::incompressible::block_looks_incompressible;
use super::match_generator::{
    DFAST_EMPTY_SLOT, DFAST_HASH_BITS, DFAST_INCOMPRESSIBLE_SKIP_STEP, DFAST_MIN_MATCH_LEN,
    DFAST_REBASE_GUARD_BAND, DFAST_SHORT_HASH_BITS_DELTA, DFAST_SKIP_STEP_GROWTH_INTERVAL,
};
use super::match_table::helpers::extend_backwards_shared;
use super::match_table::storage::{REBASE_RESET_FLOOR_CEILING, check_stream_abs_headroom};
use super::opt::types::MatchCandidate;
use super::simple::fast_kernel::count::count_forward_dict_2segment;
use super::util::hash_top_bits;
use super::workspace::{HistoryBuf, RetiredHistory, Table, Workspace, region_bytes};

/// Upstream zstd `HASH_READ_SIZE` (`zstd_compress_internal.h`): the largest probe
/// width any hash / equality check in the dfast hot path reads at once.
/// Loop guards must stop scanning when fewer than `HASH_READ_SIZE` bytes
/// remain ahead of the probe cursor, matching upstream zstd `ilimit = iend -
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

/// Upstream `kSearchStrength` (`zstd_compress_internal.h`): the dictionary
/// scan loop advances by `((ip - anchor) >> kSearchStrength) + 1`, so the
/// stride grows by one for every `1 << kSearchStrength` bytes travelled since
/// the last match. Expressed as the shift because that loop applies it per
/// position; the two-cursor loop counts the same distance out in
/// `DFAST_SKIP_STEP_GROWTH_INTERVAL`-sized intervals instead, and the two must
/// agree.
const DFAST_SKIP_STEP_SHIFT: usize = DFAST_SKIP_STEP_GROWTH_INTERVAL.trailing_zeros() as usize;
const _: () = assert!(
    DFAST_SKIP_STEP_GROWTH_INTERVAL == 1 << DFAST_SKIP_STEP_SHIFT,
    "the step-growth interval must be a power of two to express it as a shift",
);

/// Move the matcher's repeat pair past a match it emitted. Only `reps[0]` and
/// `reps[1]` are ever probed, so only they move, the way upstream carries
/// `offset_1` and `offset_2` (zstd_double_fast.c:285-286); the encoder codes
/// the offset against its own full history, whose first two entries
/// (`encode_offset_with_history_fast`) move exactly so.
#[inline(always)]
fn note_match_offset(reps: &mut [u32; 3], offset: u32, literal_len: usize) {
    // Only a repeat of `reps[0]` after literals leaves the pair in place. With
    // no literals a match at `reps[1]` swaps the two, which is the same move
    // as making any new offset the newest.
    if literal_len == 0 || offset != reps[0] {
        reps[1] = reps[0];
        reps[0] = offset;
    }
}

/// Cached `DFTRACE` env flag for the dfast commit-path diagnostic (read once;
/// see the `DFTRACE` gate in the fast-loop commit handler). Compiled only with
/// `kernel-trace`, so a production build carries no diagnostic in its match
/// loop.
#[cfg(feature = "kernel-trace")]
static DFTRACE_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// A scan-path condition, counted by `branch_profile` in `kernel-trace`
/// builds and passed through untouched otherwise.
macro_rules! dfast_branch {
    ($id:expr, $cond:expr) => {{
        let taken: bool = $cond;
        #[cfg(feature = "kernel-trace")]
        crate::encoding::branch_profile::hit($id, taken);
        taken
    }};
}

#[derive(Clone)]
pub(crate) struct DfastMatchGenerator {
    pub(crate) max_window_size: usize,
    /// Per-block length queue. Previously held the raw input
    /// `VecDeque<Vec<u8>>` for each appended block — that duplicated
    /// every byte in `history`, doubling the input footprint relative
    /// to upstream zstd on the hot path. Now stores only the lengths so the
    /// matcher can still pop blocks block-by-block on eviction
    /// (advancing `history_start` by each pop) while the actual byte
    /// storage lives once, in `history`.
    pub(crate) window_blocks: VecDeque<usize>,
    pub(crate) window_size: usize,
    /// Bytes at the tail of `history` that have been read but not yet claimed
    /// by a block (see `fill_uncommitted`). Tracked explicitly rather than
    /// derived from `window_size`, because a primed dictionary also lives in
    /// `history` and the two do not sum to `history.len()`.
    pub(crate) uncommitted_len: usize,
    // We keep a contiguous searchable history to avoid rebuilding and reseeding
    // the matcher state from disjoint block buffers on every block. In the
    // context's workspace when a context drives the matcher.
    pub(crate) history: HistoryBuf,
    pub(crate) history_start: usize,
    pub(crate) history_abs_start: usize,
    pub(crate) offset_hist: [u32; 3],
    /// While a dictionary is primed: how far into it the tables start
    /// indexing, `0` for all of it (see [`Self::set_dictionary_index_floor`]).
    dict_index_floor: usize,
    // Storage: single `u32` per bucket — upstream zstd-parity overwrite-on-
    // collision. Each slot holds a +1-biased relative position
    // (`(abs_pos - position_base + 1) as u32`); `DFAST_EMPTY_SLOT = 0`
    // is therefore never a real value. The two tables are sized
    // independently: `long_hash` (8-byte hash) uses `long_hash_bits`
    // (upstream zstd `hashTable`); `short_hash` (4-byte hash) uses
    // `short_hash_bits` = `long - 1` (upstream zstd `chainTable` for dfast).
    // Upstream zstd parity at Level 3: `2^17 × 4 + 2^16 × 4 = 768 KiB`. The
    // ratio loss from single-slot is compensated by upstream zstd's
    // `_search_next_long` retry — after a short-hash hit, the search
    // probes long_hash at `ip + 1` and picks the longer of the two
    // (see `hash_candidate`, invoked via `best_match`, for the retry).
    /// Single backing allocation for both hash tables (cwksp analog): the long
    /// table occupies `[0, long_len)` and the short table `[long_len,
    /// long_len + short_len)`. One allocation instead of two halves the
    /// per-fresh-frame large-table allocator churn (the page-fault / calloc
    /// storm that dominated dfast on medium inputs). Access through the
    /// `long_*` / `short_*` helpers, which apply the short-region offset. In
    /// the context's workspace when a context drives the matcher.
    pub(crate) tables: Table<u32>,
    /// Whether the live slots are tagged (`(rel + 1) << DFAST_TAG_BITS |
    /// tag`), chosen per frame by [`Self::bind_tables`].
    pub(crate) tagged: bool,
    /// Absolute position whose `(abs_pos - position_base + 1)` slot
    /// encoding evaluates to `1`. Advances only via [`Self::reduce`]
    /// when an insert is about to overflow the u32 window — the
    /// frame-level `STREAM_ABS_HEADROOM` gate already bounds
    /// `history_abs_start` against `usize::MAX`, so a rebase trigger
    /// here only fires on encoder sessions that span more than
    /// `u32::MAX - DFAST_REBASE_GUARD_BAND ≈ 3 GiB` of input through
    /// a single matcher instance. Upstream zstd parity: `ZSTD_window_reduce`
    /// (`zstd_compress_internal.h`).
    pub(crate) position_base: usize,
    /// Long-hash table bit-width — `long_hash.len() == 1 <<
    /// long_hash_bits`. Upstream zstd parity with `cParams.hashLog` (17 for
    /// Level 3 large input, 16 for Level 2; see `clevels.h`).
    pub(crate) long_hash_bits: usize,
    /// Short-hash table bit-width — `short_hash.len() == 1 <<
    /// short_hash_bits`. Default is `long_hash_bits -
    /// DFAST_SHORT_HASH_BITS_DELTA`, upstream zstd parity with
    /// `cParams.chainLog` for dfast levels (one bit smaller than the
    /// long hash). Halves the short-table footprint without losing
    /// measurable ratio — the 4-byte short hash overwrites less
    /// frequently than the 8-byte long hash on average, so the
    /// smaller bucket count is the upstream zstd-correct sizing.
    pub(crate) short_hash_bits: usize,
    /// The short table's key width in bytes, 4..=7 (upstream `cParams.minMatch`,
    /// the double-fast loops' `mls` template), set by [`Self::set_min_match`].
    pub(crate) mls: usize,
    /// Immutable dictionary long+short hash tables (upstream zstd `dictMatchState`)
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
    /// Dict tables detached from [`Self::dict`] and kept: built for the
    /// dictionary in use while frames copy it instead of attaching it, or
    /// kept for their allocation when the dictionary changed. Neither
    /// travels with a snapshot.
    dict_keep: DictKeep<DfastDictTables>,
    /// Dict table builds started, and how many of them allocated. Test-only.
    #[cfg(test)]
    pub(crate) dict_table_builds: (usize, usize),
    /// The attached dictionary tables' widths for the next dictionary frame
    /// (the CDict's `hashLog` / `chainLog`), set by the driver; `None` uses
    /// the live widths.
    dict_table_bits: Option<(usize, usize)>,
    /// Whether this frame's tables were filled from the dictionary's index
    /// ([`Self::finish_dict_copy`]): the next frame fills its own the same
    /// way, so the driver keeps no snapshot of them. Cleared by
    /// [`Self::reset`].
    dict_copied_from_index: bool,
    /// Borrowed one-shot input window, registered once per frame:
    /// `(input_base_ptr, total_len)`. When `Some`, the fast loop sources
    /// bytes from the caller's in-place input slice with absolute input
    /// positions (`abs_start = position_base = start_offset = 0`) instead of
    /// copying each block into the owned `history` concat — the dfast analog
    /// of the Fast backend's borrowed window. `None` = owned (history-copying)
    /// path. Cleared on `reset()`.
    pub(crate) borrowed_input: Option<(*const u8, usize)>,
    /// Active borrowed block range `[block_start, block_end)` (absolute input
    /// offsets), re-staged before each block scan so `scan_source` bounds the
    /// readable length to `block_end` (byte-identical match window to the
    /// owned evicting path) and `get_last_space` reports the borrowed block
    /// to the emit pipeline. Only meaningful while `borrowed_input` is `Some`.
    pub(crate) borrowed_block: Option<(usize, usize)>,
    /// Whether the borrowed window belongs to a frame with an attached
    /// dictionary. Such a frame numbers its input as the owned path does, right
    /// after the dictionary at the front of the history, so its slots pack
    /// against `position_base` and the next frame retires them by moving the
    /// floor instead of emptying the tables.
    borrowed_dict: bool,
    /// Absolute position of the borrowed input's first byte: `0` without a
    /// dictionary, the end of the dictionary with one.
    borrowed_abs_base: usize,
    /// Absolute end of the input a dictionary frame scanned in place, which
    /// the history does not hold, for the next frame's floor.
    borrowed_abs_end: usize,
    /// Set by [`Self::reset`] when it re-borrowed a resident attach-mode
    /// dictionary (kept the dict bytes at the front of history + the cached dict
    /// tables in place instead of clearing + re-committing them). Signals the
    /// frame compressor to SKIP `prime_with_dictionary` this frame.
    pub(crate) dict_resident: bool,
    /// Set by [`Self::retire_history`] ahead of a layout and taken by the next
    /// [`Self::reset`], which would otherwise read the floor off a history
    /// already cut down to what it keeps.
    pub(crate) retired: Option<RetiredHistory>,
}

/// The dfast backend's immutable dictionary tables — a long+short pair mirroring
/// the live long / short regions of [`DfastMatchGenerator::tables`] in
/// shape, sized to the same `(long_hash_bits, short_hash_bits)`. Held by the
/// shared [`DictAttach`] level-1 lifecycle; the per-tier dual-probe LOOKUP is
/// level-2 in this backend's kernel. Slots hold a +1-biased concat index
/// shifted by [`DFAST_TAG_BITS`] with the hash tag in the low bits
/// (upstream `ZSTD_SHORT_CACHE`); `DFAST_EMPTY_SLOT = 0` is "no entry".
#[derive(Debug, Default, Clone)]
pub(crate) struct DfastDictTables {
    pub(crate) long: alloc::vec::Vec<u32>,
    pub(crate) short: alloc::vec::Vec<u32>,
    /// The tables' own widths (the CDict's `hashLog` / `chainLog`, upstream
    /// `dictCParams`), independent of the source-capped live tables: the
    /// probes hash the position at these widths.
    pub(crate) long_bits: usize,
    pub(crate) short_bits: usize,
    /// The short table's key width they were built with (the matcher's `mls`).
    pub(crate) mls: usize,
    /// How far into the dictionary the tables start indexing (the matcher's
    /// `dict_index_floor` when they were built).
    pub(crate) index_floor: usize,
}

/// Upstream `ZSTD_SHORT_CACHE_TAG_BITS`: the low bits of a tagged slot hold a
/// hash tag so a probe rejects most collisions without touching the candidate's
/// bytes (every untagged collision on incompressible input is a cache miss).
/// The dictionary tables are always tagged; the live tables are when their
/// positions fit what is left of the slot ([`DFAST_TAGGED_WINDOW_LIMIT`]).
pub(crate) const DFAST_TAG_BITS: u32 = 8;
const DFAST_TAG_MASK: u32 = (1 << DFAST_TAG_BITS) - 1;
/// Largest dictionary the tagged slots can index (`(index + 1) << 8` must fit
/// `u32`); larger dictionaries take the copy path.
pub(crate) const DFAST_ATTACH_DICT_MAX_LEN: usize = (1usize << (32 - DFAST_TAG_BITS)) - 2;

/// Largest relative position a tagged live slot holds (`rel + 1` beside the
/// tag in a `u32`); a slice scanned in place never rebases, so it is also the
/// longest one whose tables can be tagged.
pub(crate) const DFAST_TAGGED_MAX_REL: usize = (1usize << (32 - DFAST_TAG_BITS)) - 2;
/// How far a tagged table's rebase moves its base. What survives a rebase is
/// everything newer than `DFAST_TAGGED_MAX_REL - DFAST_TAGGED_REBASE`, so
/// that span has to cover the window and a pending block.
const DFAST_TAGGED_REBASE: u32 = 1 << 23;
/// Largest window (dictionary included) whose frames tag the live tables:
/// the rebase above keeps `DFAST_TAGGED_MAX_REL - DFAST_TAGGED_REBASE` bytes,
/// a little under 2^23, which covers this window and a block.
pub(crate) const DFAST_TAGGED_WINDOW_LIMIT: usize = 1 << 22;
/// Smallest window whose frames tag the live tables. A tagged scan carries
/// three tags across its loop; x86_64 holds them in registers, i686 with seven
/// general registers spills them, and there the tag only pays once the window
/// is large enough for the candidate load it skips to miss the cache. Measured
/// on i686: bare slots 3-5% faster at a 16 KiB window and 1.5% at 64 KiB, tags
/// 1-2.5% faster from 256 KiB; x86_64 tags win at every size.
#[cfg(target_arch = "x86")]
pub(crate) const DFAST_TAGGED_WINDOW_FLOOR: usize = 1 << 18;
/// See the i686 definition: elsewhere every eligible window is tagged, the
/// policy measured on x86_64 and the one targets without a measurement of their
/// own take.
#[cfg(not(target_arch = "x86"))]
pub(crate) const DFAST_TAGGED_WINDOW_FLOOR: usize = 0;
const _: () = assert!(
    DFAST_TAGGED_MAX_REL - DFAST_TAGGED_REBASE as usize
        >= DFAST_TAGGED_WINDOW_LIMIT + crate::common::MAX_BLOCK_SIZE as usize,
    "a tagged rebase must keep the whole window",
);

/// The short table's key width for a `cParams.minMatch`: upstream templates
/// the double-fast loops on 4..=7 and runs every other value as 4
/// (`ZSTD_compressBlock_doubleFast`, zstd_double_fast.c:572-584).
pub(crate) fn dfast_key_len(min_match: u8) -> usize {
    match min_match {
        5 => 5,
        6 => 6,
        7 => 7,
        _ => 4,
    }
}

/// The short table's key for the `MLS` bytes an 8-byte load `v8` starts with,
/// in the top bits of the word (upstream `ZSTD_hashPtr(p, hBits, mls)`).
#[inline(always)]
const fn short_key<const MLS: usize>(v8: u64) -> u64 {
    v8 << (64 - 8 * MLS)
}

/// The shortest match a bare short-hash hit commits with an `MLS`-byte key.
/// Upstream takes any hit whose first four bytes agree
/// (`zstd_double_fast.c:220`); with a 4-byte key that is a full-key match, so
/// the floor is the key, while wider keys keep [`DFAST_MIN_MATCH_LEN`].
#[inline(always)]
const fn short_hit_floor<const MLS: usize>() -> usize {
    if MLS < DFAST_MIN_MATCH_LEN {
        MLS
    } else {
        DFAST_MIN_MATCH_LEN
    }
}

/// [`short_key`] read from exactly the `MLS` bytes at `p`, never past them.
///
/// # Safety
/// `p` must point to `MLS` readable bytes.
#[inline(always)]
unsafe fn short_key_exact<const MLS: usize>(p: *const u8) -> u64 {
    // SAFETY: the first four of the `MLS` readable bytes as a word.
    let mut v = u64::from(unsafe { (p as *const u32).read_unaligned() });
    let mut i = 4;
    while i < MLS {
        // SAFETY: `i < MLS`.
        v |= u64::from(unsafe { *p.add(i) }) << (8 * i);
        i += 1;
    }
    v << (64 - 8 * MLS)
}

/// [`short_key`] for a width known only at run time, from the `mls` bytes at
/// `p` with `avail >= mls` of them readable.
///
/// # Safety
/// `p` must point to `avail` readable bytes.
#[inline(always)]
unsafe fn short_key_at(p: *const u8, avail: usize, mls: usize) -> u64 {
    debug_assert!((4..=7).contains(&mls) && avail >= mls);
    let raw = if avail >= 8 {
        // SAFETY: 8 bytes are readable.
        unsafe { (p as *const u64).read_unaligned() }
    } else {
        // SAFETY: `mls <= avail` bytes are readable, the first four as a word.
        let mut v = u64::from(unsafe { (p as *const u32).read_unaligned() });
        for i in 4..mls {
            v |= u64::from(unsafe { *p.add(i) }) << (8 * i);
        }
        v
    };
    raw << (64 - 8 * mls)
}

/// The slot index a hash product's top `64 - shift` bits form (a table holds
/// at most 2^30 slots, so the width stays within the helper's 32 bits).
#[inline(always)]
fn dfast_slot(mixed: u64, shift: usize) -> usize {
    hash_top_bits(mixed, (64 - shift) as u32) as usize
}

/// The tag of a hash product whose top bits (above `shift`) form the slot
/// index: the `DFAST_TAG_BITS` bits right below the index.
#[inline(always)]
fn dfast_tag(mixed: u64, shift: usize) -> u32 {
    ((mixed >> (shift - DFAST_TAG_BITS as usize)) as u32) & DFAST_TAG_MASK
}

/// How far a hash product shifts down to leave the slot index and, with
/// `TAGGED`, the tag below it: one count per table where the slot and the tag
/// would each need their own.
#[inline(always)]
const fn dfast_key_shift<const TAGGED: bool>(shift: usize) -> usize {
    if TAGGED {
        shift - DFAST_TAG_BITS as usize
    } else {
        shift
    }
}

/// The slot index and tag ([`dfast_slot`], [`dfast_tag`]) of a hash product,
/// from the one shift [`dfast_key_shift`] gives: the slot is the high part of
/// the shifted product and the tag its low byte (0 untagged).
#[inline(always)]
fn dfast_slot_and_tag<const TAGGED: bool>(mixed: u64, key_shift: usize) -> (usize, u32) {
    let key = mixed >> key_shift;
    if TAGGED {
        (
            (key >> DFAST_TAG_BITS) as usize,
            key as u32 & DFAST_TAG_MASK,
        )
    } else {
        (key as usize, 0)
    }
}

/// [`live_slot`] for a tag [`dfast_slot_and_tag`] already split off.
#[inline(always)]
const fn live_slot_with_tag<const TAGGED: bool>(packed: u32, tag: u32) -> u32 {
    if TAGGED {
        (packed << DFAST_TAG_BITS) | tag
    } else {
        packed
    }
}

/// The word a live slot stores for the packed position `packed` under the
/// hash product `mixed`: the bare position, or with `TAGGED` the position
/// above the hash's tag. The format is a constant of the caller, chosen once
/// above its loop, so neither form carries the other's shift and mask.
#[inline(always)]
fn live_slot<const TAGGED: bool>(packed: u32, mixed: u64, shift: usize) -> u32 {
    if TAGGED {
        (packed << DFAST_TAG_BITS) | dfast_tag(mixed, shift)
    } else {
        packed
    }
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
            uncommitted_len: 0,
            history: HistoryBuf::new(),
            history_start: 0,
            history_abs_start: 0,
            offset_hist: [1, 4, 8],
            dict_index_floor: 0,
            tables: Table::empty(),
            tagged: false,
            position_base: 0,
            long_hash_bits: DFAST_HASH_BITS,
            short_hash_bits: DFAST_HASH_BITS - DFAST_SHORT_HASH_BITS_DELTA,
            mls: 5,
            dict: DictAttach::new(),
            dict_keep: DictKeep::new(),
            #[cfg(test)]
            dict_table_builds: (0, 0),
            dict_table_bits: None,
            dict_copied_from_index: false,
            borrowed_input: None,
            borrowed_block: None,
            borrowed_dict: false,
            borrowed_abs_base: 0,
            borrowed_abs_end: 0,
            dict_resident: false,
            retired: None,
        }
    }

    /// Whether the last [`Self::reset`] re-borrowed a resident dictionary (kept
    /// the dict bytes + cached dict tables in place). The driver reports this up
    /// so the frame compressor skips `prime_with_dictionary` for the frame.
    pub(crate) fn dict_resident(&self) -> bool {
        self.dict_resident
    }

    /// Number of slots in the long hash table (`[0, long_len)` of `tables`).
    #[inline(always)]
    pub(crate) fn long_len(&self) -> usize {
        1usize << self.long_hash_bits
    }

    /// Number of slots in the short hash table (`[long_len, +short_len)`).
    #[inline(always)]
    pub(crate) fn short_len(&self) -> usize {
        1usize << self.short_hash_bits
    }

    /// Slots of both tables that hold a position.
    #[cfg(test)]
    pub(crate) fn occupied_slots(&self) -> usize {
        let len = self.long_len() + self.short_len();
        // SAFETY: the two tables are `long_len + short_len` contiguous slots.
        let slots = unsafe { core::slice::from_raw_parts(self.long_ptr(), len) };
        slots
            .iter()
            .filter(|&&slot| slot != DFAST_EMPTY_SLOT)
            .count()
    }

    /// Base const-pointer to the long table region.
    #[inline(always)]
    fn long_ptr(&self) -> *const u32 {
        self.tables.as_ptr()
    }

    /// Base const-pointer to the short table region (offset past the long).
    #[inline(always)]
    fn short_ptr(&self) -> *const u32 {
        // SAFETY: `tables.len() == long_len + short_len`, so `long_len` is in
        // bounds (one past the long region = start of the short region).
        unsafe { self.tables.as_ptr().add(self.long_len()) }
    }

    /// Base mut-pointer to the long table region.
    #[inline(always)]
    fn long_mut_ptr(&mut self) -> *mut u32 {
        self.tables.as_mut_ptr()
    }

    /// Base mut-pointer to the short table region (offset past the long).
    #[inline(always)]
    fn short_mut_ptr(&mut self) -> *mut u32 {
        let off = self.long_len();
        // SAFETY: as `short_ptr`; `off == long_len` is in bounds.
        unsafe { self.tables.as_mut_ptr().add(off) }
    }

    /// Set both hash table sizes from the per-level
    /// [`DfastConfig`](crate::encoding::levels::config::DfastConfig):
    /// `long_bits` = upstream zstd `cParams.hashLog`, `short_bits` = upstream zstd
    /// `cParams.chainLog`, taken as resolved: the cParams bounds keep both at
    /// or above `ZSTD_HASHLOG_MIN` / `ZSTD_CHAINLOG_MIN` (6), and the resolver
    /// has already down-sized them for the source.
    pub(crate) fn set_hash_bits(&mut self, long_bits: usize, short_bits: usize) {
        debug_assert!(
            long_bits >= 6 && short_bits >= 6,
            "dfast table widths {long_bits}/{short_bits} below the cParams floor"
        );
        let long_clamped = long_bits;
        let short_clamped = short_bits;
        let resized = self.long_hash_bits != long_clamped || self.short_hash_bits != short_clamped;
        if resized {
            self.long_hash_bits = long_clamped;
            self.short_hash_bits = short_clamped;
            // Drop the combined backing so the tables are laid out (or
            // allocated) again at the new (long_len + short_len).
            self.tables = Table::empty();
        }
        if resized {
            // A table-size change detaches the cached dict tables; parked, they
            // come back at the next prime only if their own widths are still
            // the ones it resolves.
            self.dict_keep.park(&mut self.dict);
        }
    }

    /// Set the short table's key width from upstream `cParams.minMatch`
    /// ([`dfast_key_len`]). The short slots are keyed by it, so a change lays
    /// the tables out anew and detaches the dict tables, as a width change in
    /// [`Self::set_hash_bits`] does.
    pub(crate) fn set_min_match(&mut self, min_match: u8) {
        let mls = dfast_key_len(min_match);
        if self.mls != mls {
            self.mls = mls;
            self.tables = Table::empty();
            self.dict_keep.park(&mut self.dict);
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
            rel <= self.max_rel(),
            "DfastMatchGenerator::pack_slot: rel {rel} past the slot's range — \
             caller must invoke ensure_room_for before insert"
        );
        (rel as u32) + 1
    }

    /// The packed position a live slot word names for a probe hashed to
    /// `mixed`, or the empty sentinel when a tagged word carries another tag.
    #[inline(always)]
    fn packed_in(&self, word: u32, mixed: u64, shift: usize) -> u32 {
        if !self.tagged {
            word
        } else if word & DFAST_TAG_MASK == dfast_tag(mixed, shift) {
            word >> DFAST_TAG_BITS
        } else {
            DFAST_EMPTY_SLOT
        }
    }

    /// Largest relative position a slot holds in the current format.
    #[inline(always)]
    fn max_rel(&self) -> usize {
        if self.tagged {
            DFAST_TAGGED_MAX_REL
        } else {
            u32::MAX as usize - 1
        }
    }

    /// Ensure that an absolute position `abs_pos` fits in the `u32`
    /// slot encoding when packed. If the relative offset would
    /// exceed `u32::MAX - DFAST_REBASE_GUARD_BAND`, advance the base
    /// by `DFAST_REBASE_GUARD_BAND` (in a loop, in case the caller
    /// jumped past multiple guard bands at once) and shift every
    /// stored slot down by the same amount. Mirrors
    /// `LdmHashTable::ensure_room_for` and the upstream zstd's
    /// `ZSTD_window_reduce` semantics.
    pub(crate) fn ensure_room_for(&mut self, abs_pos: usize) {
        if abs_pos < self.position_base {
            // Pre-base positions can't push us past the u32 ceiling.
            return;
        }
        // A tagged slot holds far fewer positions, so it rebases by less and
        // keeps the window across the rebase (see `DFAST_TAGGED_REBASE`).
        let (max_rel, reducer) = if self.tagged {
            (DFAST_TAGGED_MAX_REL, DFAST_TAGGED_REBASE)
        } else {
            (
                u32::MAX as usize - DFAST_REBASE_GUARD_BAND as usize,
                DFAST_REBASE_GUARD_BAND,
            )
        };
        while abs_pos - self.position_base > max_rel {
            self.reduce(reducer);
        }
    }

    /// Subtract `reducer` from every stored position. Slots whose position
    /// was `<= reducer` become the empty sentinel; a tagged slot keeps its
    /// tag. Advance `position_base` by the same amount so future inserts
    /// continue from the rebased origin.
    fn reduce(&mut self, reducer: u32) {
        let shift = if self.tagged { DFAST_TAG_BITS } else { 0 };
        // A tagged table's reducer is below 2^24, so the shift stays inside
        // `u32`.
        let floor = ((reducer + 1) << shift) - 1;
        let slot_reducer = reducer << shift;
        let shift_slots = |slots: &mut [u32]| {
            for slot in slots.iter_mut() {
                *slot = if *slot <= floor {
                    DFAST_EMPTY_SLOT
                } else {
                    *slot - slot_reducer
                };
            }
        };
        let long_len = self.long_len();
        let (long, short) = self.tables.split_at_mut(long_len);
        shift_slots(long);
        shift_slots(short);
        self.position_base += reducer as usize;
    }

    /// Workspace bytes the long and short tables take at the current widths.
    pub(crate) fn tables_workspace_bytes(&self) -> usize {
        region_bytes::<u32>(self.long_len() + self.short_len())
    }

    /// Lays the long and short tables out in the open `workspace`, their
    /// slots tagged when `tagged`. When they do not continue the previous
    /// frame's tables in the same slot format they start empty, and the next
    /// [`Self::reset`] knows they hold nothing an earlier frame wrote.
    pub(crate) fn bind_tables(&mut self, workspace: &mut Workspace, tagged: bool) {
        let total = self.long_len() + self.short_len();
        let kept = self.tables.bind(workspace, total, DFAST_EMPTY_SLOT);
        if kept && self.tagged != tagged {
            // The same bytes in the other slot format name nothing.
            self.tables.fill(DFAST_EMPTY_SLOT);
        }
        let continued = kept && self.tagged == tagged;
        if !continued && tagged {
            // Empty tables can take any base; starting it at the next frame's
            // floor keeps a tagged slot's short position range from being
            // spent on bytes earlier frames already retired. The layout runs
            // before the reset applies that floor, so it is read from the
            // retired history when there is one.
            self.position_base = self
                .retired
                .as_ref()
                .map_or(self.history_abs_start, |retired| retired.next_floor);
        }
        self.tagged = tagged;
    }

    /// Becomes `snapshot`, copying its tables, history and block-length queue
    /// into the buffers this matcher already holds (the first two in a
    /// context's workspace) instead of cloning them into new allocations. The
    /// buffers are lent out of `snapshot` for the clone of the rest and put
    /// back. Copies the other way as well, so a snapshot is captured into the
    /// buffers of the one it replaces.
    ///
    /// The attached-dictionary state is neither side's to give: a snapshot
    /// is taken and restored only in frames that copy the dictionary into
    /// the live tables, where it is detached, and each side keeps the tables
    /// it parked for the frames that attach.
    pub(crate) fn restore_snapshot(&mut self, snapshot: &mut Self) {
        debug_assert!(!self.dict.is_attached() && !snapshot.dict.is_attached());
        let snapshot_tables = core::mem::take(&mut snapshot.tables);
        let snapshot_history = core::mem::take(&mut snapshot.history);
        let snapshot_blocks = core::mem::take(&mut snapshot.window_blocks);
        let snapshot_dict = core::mem::take(&mut snapshot.dict);
        let mut tables = core::mem::take(&mut self.tables);
        let mut history = core::mem::take(&mut self.history);
        let mut blocks = core::mem::take(&mut self.window_blocks);
        let dict = core::mem::take(&mut self.dict);
        let dict_keep = core::mem::take(&mut self.dict_keep);
        #[cfg(test)]
        let dict_table_builds = self.dict_table_builds;
        tables.clone_from(&snapshot_tables);
        history.clone_from(&snapshot_history);
        blocks.clone_from(&snapshot_blocks);
        *self = snapshot.clone();
        self.tables = tables;
        self.history = history;
        self.window_blocks = blocks;
        self.dict = dict;
        self.dict_keep = dict_keep;
        #[cfg(test)]
        {
            self.dict_table_builds = dict_table_builds;
        }
        snapshot.tables = snapshot_tables;
        snapshot.history = snapshot_history;
        snapshot.window_blocks = snapshot_blocks;
        snapshot.dict = snapshot_dict;
    }

    /// Heap bytes this matcher owns: the history and the long/short hash
    /// tables when they are not in a context's workspace, the window-block
    /// deque, and any attached dictionary tables.
    pub(crate) fn heap_size(&self) -> usize {
        let u32_sz = core::mem::size_of::<u32>();
        self.window_blocks.capacity() * core::mem::size_of::<usize>()
            + self.history.owned_bytes()
            + self.tables.owned_bytes()
            + self
                .dict
                .table()
                .into_iter()
                .chain(self.dict_keep.table())
                .map(|t| (t.long.capacity() + t.short.capacity()) * u32_sz)
                .sum::<usize>()
            + self.dict_keep.slot_bytes()
    }

    /// Settles, ahead of the next frame's layout, what [`Self::reset`] keeps of
    /// the history, and drops the rest, so the layout sizes the room for and
    /// carries over only that. Once per frame; the reset takes the result.
    pub(crate) fn retire_history(&mut self) {
        if self.retired.is_none() {
            self.retired = Some(self.retire());
        }
    }

    fn retire(&mut self) -> RetiredHistory {
        // Bytes an abandoned frame ingested but never claimed are not part of
        // the next frame, and they must not count towards the floor advance.
        // Dropped first because every tail-relative bound subtracts this count
        // from the buffer length and would underflow once history is cleared.
        self.history
            .truncate(self.history.len() - self.uncommitted_len);
        self.uncommitted_len = 0;
        // A dictionary frame scanned in place wrote positions past the history
        // it holds, up to the end of its input.
        let next_floor = (self.history_abs_start + (self.history.len() - self.history_start))
            .max(core::mem::take(&mut self.borrowed_abs_end));
        // Re-borrow: an attach-mode reused dict frame keeps its bytes resident at
        // the front of history (`[0, region)`) + the cached concat-keyed dict
        // tables, so the per-frame dict re-commit (the dominant ~37% memmove on
        // a profiled small dfast frame) is skipped — the frame compressor then
        // skips `prime_with_dictionary`. The floor-advance still rejects the
        // previous frame's INPUT (its abs falls below `next_floor`); the dict
        // matches come from the separate dict tables, which bypass the floor.
        // Gated on the dict being fully resident at `history_start == 0` and the
        // floor-advance staying bounded.
        let kept = if self.dict.is_primed()
            && self.history_start == 0
            && next_floor <= REBASE_RESET_FLOOR_CEILING
        {
            let r = self.dict.region_len();
            (r > 0 && self.history.len() >= r).then_some(r)
        } else {
            None
        };
        // Keep `[0, region)` (the dict); drop the previous frame's input.
        self.history.truncate(kept.unwrap_or(0));
        RetiredHistory { next_floor, kept }
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
        let RetiredHistory {
            next_floor,
            kept: reborrow_region,
        } = self.retired.take().unwrap_or_else(|| self.retire());
        self.offset_hist = [1, 4, 8];
        self.dict_copied_from_index = false;
        if let Some(region) = reborrow_region {
            self.window_size = region;
            self.window_blocks.clear();
            self.window_blocks.push_back(region);
            // Bump the eviction window by the dict size (clamped to
            // MAX_PRIMED_WINDOW_SIZE) so the resident dict + the next input both
            // stay — base is `1 << window_log` (<= 2^30 < the ceiling), so the
            // headroom subtraction can't underflow and the sum can't overflow.
            let headroom = crate::encoding::match_table::storage::MAX_PRIMED_WINDOW_SIZE
                - self.max_window_size;
            self.max_window_size += region.min(headroom);
            self.history_abs_start = next_floor;
            self.dict_resident = true;
        } else {
            self.window_size = 0;
            self.history.clear();
            self.history_start = 0;
            // Non-reborrow reset starts with an empty window; clear the block
            // ledger to match. (The reborrow branch above intentionally keeps a
            // single `[region]` entry for the resident dict — see below.)
            self.window_blocks.clear();
            self.dict_resident = false;
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
                if !self.tables.is_empty() {
                    self.tables.fill(DFAST_EMPTY_SLOT);
                }
            }
        }
        // NOTE: `window_blocks` is cleared per-branch above (the reborrow branch
        // keeps its `[region]` dict entry; the non-reborrow branch clears). It
        // must NOT be cleared unconditionally here — doing so dropped the
        // resident dict block while `window_size`/`history` still counted it,
        // desyncing the ledger so the next eviction popped the wrong block.
        // Drop any borrowed window: the input slice it pointed at does not
        // outlive the frame, and the next frame re-stages its own (or runs
        // the owned path). A stale pointer must never survive a reset.
        self.borrowed_input = None;
        self.borrowed_block = None;
        self.borrowed_dict = false;
        self.borrowed_abs_base = 0;
    }

    /// The most recently committed block, or an empty slice if none has been
    /// committed yet (the same gate `skip_matching` / `start_matching` use).
    pub(crate) fn get_last_space(&self) -> &[u8] {
        if let (Some((ptr, _total)), Some((block_start, block_end))) =
            (self.borrowed_input, self.borrowed_block)
        {
            // Borrowed window: the active block is the in-place input range
            // `[block_start, block_end)`, staged before the scan so the emit
            // pipeline's pre-scan `get_last_space().len()` reserve is correct.
            // SAFETY: borrowed liveness contract; `block_start <= block_end <=
            // buffer len` (validated when staged).
            return unsafe {
                core::slice::from_raw_parts(ptr.add(block_start), block_end - block_start)
            };
        }
        let last_len = self.window_blocks.back().copied().unwrap_or(0);
        &self.history[self.history.len() - self.uncommitted_len - last_len
            ..self.history.len() - self.uncommitted_len]
    }

    /// Read `input` in and commit it as one block, the way the driver does
    /// in two calls.
    #[cfg(test)]
    pub(crate) fn commit_input(&mut self, input: impl AsRef<[u8]>) {
        let input = input.as_ref();
        self.fill_uncommitted(input.len(), |history| {
            history.extend_from_slice(input);
            (input.len(), false)
        });
        self.commit_block(input.len());
    }

    /// Phase 1 of ingest: let `fill` write STRAIGHT into the tail of
    /// `history`, with room reserved for `capacity` more bytes. Returns
    /// `(appended, eof)` from `fill`.
    ///
    /// The bytes land in the buffer but are NOT yet part of the window: the
    /// block boundary is only chosen afterwards, by the pre-split pass looking
    /// at [`Self::uncommitted`]. Whatever the splitter leaves over simply stays
    /// in `history` and becomes the head of the next block, so a carried
    /// suffix costs no copy.
    ///
    /// Call [`Self::commit_block`] once the length is known.
    pub(crate) fn fill_uncommitted(
        &mut self,
        capacity: usize,
        fill: impl FnOnce(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        // Count the bytes already carried, not just this top-up: `capacity` is
        // `block_capacity - carried` (and zero on the EOF re-inspection), yet
        // the carried suffix still becomes window on the next commit.
        check_stream_abs_headroom(
            self.history_abs_start,
            self.window_size,
            capacity + self.uncommitted_len,
        );
        // The eviction ceiling and the dict retire both run in `commit_block`,
        // keyed on the length a block actually claims. Sizing the ceiling here
        // off `capacity` (a whole read buffer, not a block) would trip it on the
        // first fill and fault in the full window+window/4 mirror for frames
        // that never evict.
        self.history.reserve(capacity);

        let before = self.history.len();
        let (appended, eof) = fill(&mut self.history);
        debug_assert_eq!(
            self.history.len(),
            before + appended,
            "fill_uncommitted: fill reported {appended} bytes but grew history by {}",
            self.history.len() - before,
        );
        self.uncommitted_len += appended;
        (appended, eof)
    }

    /// Bytes read but not yet claimed by a block: the pre-split pass picks the
    /// block boundary inside this slice.
    pub(crate) fn uncommitted(&self) -> &[u8] {
        &self.history[self.history.len() - self.uncommitted_len..]
    }

    /// Phase 2 of ingest: claim `len` bytes from the head of
    /// [`Self::uncommitted`] as the next block. Any remainder stays put for
    /// the next block.
    pub(crate) fn commit_block(&mut self, len: usize) {
        // An empty block records nothing: a `0` in `window_blocks` would let
        // a caller that commits empty blocks grow the deque without bound,
        // since `window_size` never grows to evict them.
        if len == 0 {
            return;
        }
        assert!(len <= self.max_window_size);
        // Hard assert, like the window check above: this runs once per block,
        // and an over-long claim would wrap `uncommitted_len` in release and
        // surface as a panic far from the cause.
        assert!(
            len <= self.uncommitted().len(),
            "commit_block: {len} exceeds the {} uncommitted bytes",
            self.uncommitted().len(),
        );
        if self.window_size + len > self.max_window_size {
            // Eviction advances `history_start`, so the dict tables' concat
            // indices (primed at `history_start == 0`) stop addressing the dict
            // bytes: detach them (the dict's ratio benefit ends once it slides
            // out of the window, as on the Fast backend). Parked, they serve
            // the next frame, which commits the dictionary at the front again.
            self.dict_keep.park(&mut self.dict);
            // Cap the history near the live window: once eviction starts, grow
            // linearly to window + window/4 + one block rather than doubling;
            // `compact_history`'s quarter-window drain keeps the length under
            // it. Small frames that never fill the window stay data-sized.
            let target = self.max_window_size
                + (self.max_window_size >> 2)
                + crate::common::MAX_BLOCK_SIZE as usize;
            if target > self.history.len() && self.history.capacity() < target {
                self.history.reserve_exact(target - self.history.len());
            }
        }
        while self.window_size + len > self.max_window_size {
            let removed_len = self.window_blocks.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        // Compact AFTER the eviction that raised `history_start`, so the drain
        // trigger sees the dead prefix this block's eviction created.
        self.compact_history();
        self.window_size += len;
        self.window_blocks.push_back(len);
        self.uncommitted_len -= len;
    }

    /// Evict retained blocks until the window fits `max_window_size`, and
    /// drop the dead prefix at once rather than at the next commit. Callers
    /// needing the evicted byte count derive it from the `window_size` delta.
    pub(crate) fn trim_to_window(&mut self) {
        if self.window_size > self.max_window_size || self.history_start != 0 {
            // Any history shift slides the dictionary out of (or within) the
            // concat, staling the dict tables' concat indices: detach them
            // (parked, as on eviction).
            self.dict_keep.park(&mut self.dict);
        }
        while self.window_size > self.max_window_size {
            let removed_len = self.window_blocks.pop_front().unwrap();
            self.window_size -= removed_len;
            self.history_start += removed_len;
            self.history_abs_start += removed_len;
        }
        if self.history_start != 0 {
            // Drop the dead prefix. Its room is the context's (or the
            // buffer's own, released with it), not the allocator's to take
            // back now.
            self.history.drain_front(self.history_start);
            self.history_start = 0;
        }
    }

    pub(crate) fn skip_matching(&mut self, incompressible_hint: Option<bool>) {
        self.ensure_hash_tables();
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            // No block committed yet, so none to seed. Mirrors the gate
            // `start_matching` uses.
            return;
        }
        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        let current_abs_end = current_abs_start + current_len;
        let tail_start = current_abs_start
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
            .max(self.live_floor());
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

    /// First absolute position the live tables index. An attached dictionary
    /// is indexed in its own tables only, as upstream's `dictMatchState` is, so
    /// the live tables start at the input after it: the seam re-seed of the
    /// first block never writes the dictionary's tail, whose keys run into the
    /// input and which a frame scanning that input in place could not read as
    /// one run.
    fn live_floor(&self) -> usize {
        if self.dict.table().is_some() {
            self.history_abs_start + self.dict.region_len()
        } else {
            self.history_abs_start
        }
    }

    /// Index a dictionary being primed only from `floor` bytes into it, `0`
    /// for all of it; the driver sets it for the prime and clears it after.
    pub(crate) fn set_dictionary_index_floor(&mut self, floor: usize) {
        self.dict_index_floor = floor;
    }

    pub(crate) fn skip_matching_dense(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            // Same gate as `skip_matching` and `start_matching`: no block
            // committed yet, so none to seed.
            return;
        }
        let current_abs_end = self.history_abs_start + self.window_size;
        // A dictionary being primed sits at the front of the history, so its
        // index floor counts from there.
        let floor = self.history_abs_start + self.dict_index_floor;
        let current_abs_start = (current_abs_end - current_len).max(floor.min(current_abs_end));
        let backfill_start = current_abs_start
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
            .max(self.history_abs_start)
            .max(floor);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        self.insert_positions(current_abs_start, current_abs_end);
    }

    /// The committed window followed by the input read but not yet committed.
    #[cfg(feature = "ldm")]
    pub(crate) fn window_with_pending(&self) -> &[u8] {
        &self.history[self.history_start..]
    }

    /// Hand the last committed block of `len` bytes back to the uncommitted
    /// input, undoing only the bookkeeping of [`Self::commit_block`]: what it
    /// evicted stays evicted, so committing the same bytes again in pieces
    /// evicts nothing more.
    #[cfg(feature = "ldm")]
    pub(crate) fn uncommit_block(&mut self, len: usize) {
        assert_eq!(self.window_blocks.pop_back(), Some(len));
        self.window_size -= len;
        self.uncommitted_len += len;
    }

    /// Index the last committed block as the body of a long-distance match:
    /// only its tail, as upstream fills the fast tables after one
    /// (`ZSTD_ldm_limitTableUpdate` + `ZSTD_ldm_fillFastTables`), since the
    /// match itself is never searched.
    #[cfg(feature = "ldm")]
    pub(crate) fn skip_long_match(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        let end = self.history_abs_start + self.window_size;
        let from = end - current_len.min(crate::encoding::ldm::LONG_MATCH_INDEXED_TAIL);
        self.insert_positions(from, end);
    }

    /// Carry a long-distance match the driver emitted into the repeat
    /// offsets, as the encoder will code it.
    #[cfg(feature = "ldm")]
    pub(crate) fn note_long_match(&mut self, offset: usize, literal_len: usize) {
        note_match_offset(&mut self.offset_hist, offset as u32, literal_len);
    }

    /// Upstream zstd `ZSTD_dictMatchState` attach (dfast): hash the dictionary block at
    /// the front of history into the SEPARATE immutable [`Self::dict`] tables
    /// (long+short), once, instead of re-priming the live tables every frame
    /// (`skip_matching_dense`). The dual-probe kernel then searches live + dict
    /// without per-frame dict cost. Cached via the `DictAttach` primed flag.
    pub(crate) fn skip_matching_for_dict_attach(&mut self) {
        self.ensure_hash_tables();
        if self.tagged {
            // The dictionary loop reads the live tables bare. A frame laid out
            // tagged without knowing a dictionary was coming converts here,
            // once, before its first block: its tables hold nothing of this
            // frame yet.
            self.tables.fill(DFAST_EMPTY_SLOT);
            self.tagged = false;
        }
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

    /// Whether a frame that copies a dictionary of `dict_len` bytes can take
    /// its live tables from the dictionary's index instead of hashing the
    /// dictionary into them: the index has the live tables' widths (a copied
    /// dictionary gives the frame its own, upstream
    /// `ZSTD_resetCCtx_byCopyingCDict`), and every position of the dictionary
    /// fits beside a tag in an index slot.
    pub(crate) fn dict_index_serves_copy(&self, dict_len: usize) -> bool {
        self.dict_table_bits == Some((self.long_hash_bits, self.short_hash_bits))
            && dict_len < (1usize << (u32::BITS - DFAST_TAG_BITS)) - 1
    }

    /// [`Self::skip_matching_for_dict_attach`] for a frame that copies the
    /// dictionary: the block goes into the dictionary's index, built once per
    /// dictionary, and the live tables are left for
    /// [`Self::finish_dict_copy`] to fill from it.
    pub(crate) fn skip_matching_for_dict_copy(&mut self) {
        self.ensure_hash_tables();
        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            return;
        }
        let end_concat = self.window_size;
        self.prime_dict_tables_for_range(end_concat - current_len, end_concat);
    }

    /// Ends the prime of a copied dictionary: the live tables take every slot
    /// of the dictionary's index (upstream `ZSTD_copyCDictTableIntoCCtx`), and
    /// the index is set aside for the next frame of this dictionary, attached
    /// or copied.
    pub(crate) fn finish_dict_copy(&mut self) {
        self.dict.mark_primed();
        self.copy_dict_index_into_live();
        self.dict_keep.park(&mut self.dict);
        self.dict_copied_from_index = true;
    }

    /// Whether [`Self::finish_dict_copy`] filled this frame's tables.
    pub(crate) fn dict_copied_from_index(&self) -> bool {
        self.dict_copied_from_index
    }

    /// Writes every live slot from the dictionary index's slot of the same
    /// hash: an index slot holds a position counted from the dictionary's
    /// start above its tag, and the live slot holds that position counted
    /// from `position_base`, with the tag when the live format is tagged. The
    /// result is what hashing the dictionary into the live tables leaves: the
    /// same keys in the same order, the last position of each slot winning.
    fn copy_dict_index_into_live(&mut self) {
        let region = self.dict.region_len();
        if self.dict.table().is_none() || region == 0 {
            // A dictionary too short to index: nothing of it is searchable,
            // and what earlier frames left in the tables lies below the floor.
            return;
        }
        debug_assert_eq!(self.history_start, 0, "a dictionary is primed at the front");
        // Every slot is rewritten, so nothing holds the old base: it restarts
        // at the dictionary's first byte, where a slot's packed position is
        // the index's own. A tagged table is then the index word for word,
        // and a bare one the index without its tags.
        self.position_base = self.history_abs_start;
        debug_assert!(region <= self.max_rel());
        let tagged = self.tagged;
        let long_len = self.long_len();
        let Some(index) = self.dict.table() else {
            return;
        };
        let (long, short) = self.tables.split_at_mut(long_len);
        let fill = |live: &mut [u32], index: &[u32]| {
            if tagged {
                live.copy_from_slice(index);
            } else {
                for (slot, &word) in live.iter_mut().zip(index) {
                    *slot = word >> DFAST_TAG_BITS;
                }
            }
        };
        fill(long, &index.long);
        fill(short, &index.short);
    }

    /// Let go of the cached dict tables: the dictionary changed. Their
    /// allocation stays for the next dictionary's tables.
    pub(crate) fn invalidate_dict_cache(&mut self) {
        self.dict_keep.forget(&mut self.dict);
    }

    /// Detach the dict tables for a frame that copies the dictionary into the
    /// live tables instead; the next attaching frame of the same dictionary
    /// takes them back ([`Self::revive_dict_cache`]).
    pub(crate) fn park_dict_cache(&mut self) {
        self.dict_keep.park(&mut self.dict);
    }

    /// Attach the parked dict tables again for a dictionary of `dict_len`
    /// bytes about to be primed, when they were built for one of that length
    /// at the widths and index floor this prime resolves: the dictionary is
    /// the same one (a change goes through [`Self::invalidate_dict_cache`]),
    /// so the tables are what a rebuild would produce. Returns whether they
    /// were.
    pub(crate) fn revive_dict_cache(&mut self, dict_len: usize) -> bool {
        let (long_bits, short_bits) = self
            .dict_table_bits
            .unwrap_or((self.long_hash_bits, self.short_hash_bits));
        let floor = self.dict_index_floor;
        let mls = self.mls;
        self.dict_keep.revive(&mut self.dict, |tables, region_len| {
            tables.long_bits == long_bits
                && tables.short_bits == short_bits
                && tables.mls == mls
                && tables.index_floor == floor
                && region_len == dict_len
        })
    }

    /// The attached dictionary tables' `(long, short)` widths for the next
    /// dictionary frame: the CDict's `hashLog` / `chainLog`; `None` uses the
    /// live widths.
    pub(crate) fn set_dict_table_bits(&mut self, bits: Option<(usize, usize)>) {
        // A resident table must not be re-borrowed by `reset` when the next
        // frame carries no dictionary (`None`: the frame header declares
        // none, so no output may reference the dict bytes) or resolves
        // another CDict geometry (the dual-probe kernel would hash at the
        // new widths into the old table). Dropping it makes the reset
        // re-prime (or run plain) instead.
        match (self.dict.table(), bits) {
            // A frame without a dictionary lets go of every dict table.
            (_, None) => {
                self.dict.invalidate();
                self.dict_keep.release();
            }
            (Some(table), Some((long_bits, short_bits)))
                if table.long_bits != long_bits || table.short_bits != short_bits =>
            {
                self.dict_keep.park(&mut self.dict);
            }
            _ => {}
        }
        self.dict_table_bits = bits;
    }

    /// `(long, short)` bit widths of the attached dictionary tables. Test-only.
    #[cfg(test)]
    pub(crate) fn dict_table_bits(&self) -> Option<(usize, usize)> {
        self.dict.table().map(|d| {
            (
                d.long.len().trailing_zeros() as usize,
                d.short.len().trailing_zeros() as usize,
            )
        })
    }

    /// `(long, short)` bit widths of the live tables. Test-only.
    #[cfg(test)]
    pub(crate) fn live_table_bits(&self) -> (usize, usize) {
        (self.long_hash_bits, self.short_hash_bits)
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
        // The CDict's geometry when the driver resolved one (upstream sizes
        // and hashes the dictMatchState tables with `dictCParams`), else the
        // live widths. A retained table of another width is rebuilt.
        let (long_bits, short_bits) = self
            .dict_table_bits
            .unwrap_or((self.long_hash_bits, self.short_hash_bits));
        // The short key width is the frame's (upstream hashes the dms short
        // table with the context's `mls`).
        let mls = self.mls;
        if self
            .dict
            .table()
            .is_some_and(|d| d.long_bits != long_bits || d.short_bits != short_bits || d.mls != mls)
        {
            // Not fully built (the primed return above), so this keeps only
            // its allocation.
            self.dict_keep.park(&mut self.dict);
            self.dict.set_region_len(end_concat);
        }
        // Lookahead-safe cutoffs within the concat: long needs 8 readable
        // bytes, short needs `mls`.
        let long_safe_end = concat_len.saturating_sub(7).min(end_concat);
        let short_safe_end = concat_len.saturating_sub(mls - 1).min(end_concat);
        // The seam window `[backfill_floor, start_concat)` holds positions that
        // only became hashable when THIS chunk extended history (e.g. a `4+1`
        // or `7+1` dict chunking), so gate the early return on `backfill_floor`,
        // not `start_concat` — otherwise those seam inserts are dropped and the
        // attached dict tables stay incomplete.
        // Nothing below the index floor is indexed, seam included: the chunk
        // before it was not.
        let floor = self.dict_index_floor.min(end_concat);
        let backfill_floor = start_concat
            .max(floor)
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
            .max(floor);
        if backfill_floor >= short_safe_end {
            return;
        }
        let index_floor = self.dict_index_floor;
        // The previous dictionary's tables at these widths are emptied in
        // place rather than allocated anew.
        let spare = if self.dict.is_attached() {
            None
        } else {
            self.dict_keep
                .take_spare()
                .filter(|tables| tables.long_bits == long_bits && tables.short_bits == short_bits)
                .map(|mut tables| {
                    // The allocation fits; the key width is rewritten with it.
                    tables.mls = mls;
                    tables
                })
        };
        #[cfg(test)]
        if !self.dict.is_attached() {
            self.dict_table_builds.0 += 1;
            self.dict_table_builds.1 += usize::from(spare.is_none());
        }
        let dict = self.dict.table_mut_or_init(|| match spare {
            Some(mut tables) => {
                tables.long.fill(DFAST_EMPTY_SLOT);
                tables.short.fill(DFAST_EMPTY_SLOT);
                tables.index_floor = index_floor;
                tables
            }
            None => DfastDictTables {
                long: alloc::vec![DFAST_EMPTY_SLOT; 1usize << long_bits],
                short: alloc::vec![DFAST_EMPTY_SLOT; 1usize << short_bits],
                long_bits,
                short_bits,
                mls,
                index_floor,
            },
        });
        let short_shift = 64 - short_bits;
        let long_shift = 64 - long_bits;
        let base = self.history.as_ptr();
        let long_ptr = dict.long.as_mut_ptr();
        let short_ptr = dict.short.as_mut_ptr();
        // SAFETY: `base.add(history_start + pos)` is in-bounds for
        // `pos + 8 <= concat_len` (long) / `pos + mls <= concat_len` (short),
        // enforced by the `*_safe_end` cutoffs (the short loop reads `mls`
        // bytes, never past `concat_len`).
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
                // Upstream `ZSTD_hashPtr(ip, hBitsS, mls)`: the low `mls`
                // bytes in the top bits, as every short-key site builds it.
                let long_mixed = v8.wrapping_mul(PRIME);
                let short_mixed = (v8 << (64 - 8 * mls)).wrapping_mul(PRIME);
                let short_idx = dfast_slot(short_mixed, short_shift);
                let long_idx = dfast_slot(long_mixed, long_shift);
                // Upstream `ZSTD_writeTaggedIndex`: the slot packs the index
                // with the next `DFAST_TAG_BITS` bits of the hash, so a
                // probe rejects a colliding slot on the tag alone, without
                // loading the dictionary bytes.
                let index = ((pos as u32) + 1) << DFAST_TAG_BITS;
                *short_ptr.add(short_idx) = index | dfast_tag(short_mixed, short_shift);
                *long_ptr.add(long_idx) = index | dfast_tag(long_mixed, long_shift);
            }
            pos += 1;
        }
        while pos < short_safe_end {
            unsafe {
                let load_ptr = base.add(history_start + pos);
                // Assembled from the `mls` bytes left, never over-reading the
                // <8-byte tail.
                let short_mixed = short_key_at(load_ptr, concat_len - pos, mls).wrapping_mul(PRIME);
                let short_idx = dfast_slot(short_mixed, short_shift);
                *short_ptr.add(short_idx) =
                    (((pos as u32) + 1) << DFAST_TAG_BITS) | dfast_tag(short_mixed, short_shift);
            }
            pos += 1;
        }
    }

    pub(crate) fn start_matching<K: EncodeTier>(
        &mut self,
        mut handle_sequence: impl FnMut(Sequence),
    ) {
        self.ensure_hash_tables();

        let current_len = self.window_blocks.back().copied().unwrap_or(0);
        if current_len == 0 {
            return;
        }

        let current_abs_start = self.history_abs_start + self.window_size - current_len;
        // Re-seed the previous block's seam. With an `mls`-byte short hash,
        // a position within `mls - 1` bytes of the prior block end could not
        // form its full key when that block was processed (the trailing bytes
        // arrived with THIS block); re-hash that tail now that history spans
        // it, so cross-block matches anchored in the seam are found. Mirrors
        // `skip_matching_dense`'s backfill.
        let backfill_start = current_abs_start
            .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
            .max(self.live_floor());
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        // dfast is the upstream zstd's greedy double-fast at every level (no lazy
        // variant exists — lazy parsing is the separate `ZSTD_lazy`/`lazy2`
        // strategy), so there is a single match path.
        self.start_matching_fast_loop::<K>(current_abs_start, current_len, &mut handle_sequence);
    }

    /// Register the caller's in-place input buffer for the borrowed one-shot
    /// path (upstream zstd `ZSTD_CCtx` in-place input). The dfast analog of
    /// [`super::simple::fast_matcher::FastKernelMatcher::set_borrowed_window`]:
    /// subsequent blocks scan ranges of `buffer` directly instead of copying
    /// each into the owned `history` concat.
    ///
    /// # Safety
    /// `buffer` must stay live and unmodified until [`Self::clear_borrowed_window`]
    /// (or [`Self::reset`]) — the matcher stores a raw pointer into it and
    /// dereferences it during every staged block scan.
    pub(crate) unsafe fn set_borrowed_window(&mut self, buffer: &[u8]) {
        // A borrowed scan packs absolute input positions, so the layout left a
        // slice longer than a tagged slot holds untagged.
        assert!(
            !self.tagged || buffer.len() <= DFAST_TAGGED_MAX_REL,
            "a borrowed input of {} bytes needs untagged tables",
            buffer.len()
        );
        self.borrowed_input = Some((buffer.as_ptr(), buffer.len()));
        self.borrowed_block = None;
        // Positions continue from earlier frames, so their slots fall below
        // this frame's floor and the tables are never emptied, as upstream
        // continues its indices across frames of a reused context
        // (`ZSTDirp_continue`, zstd_compress.c:2168). An attached dictionary
        // stays at the front of the history and the input follows it. Without
        // one the input starts a whole window past the earlier frames, so a
        // position's window floor alone excludes them and the scan takes it
        // with one subtraction, where clamping it to the input's start would
        // cost a compare and two more constants on every position.
        self.borrowed_dict = self.dict.table().is_some();
        self.borrowed_abs_base = if self.borrowed_dict {
            assert!(
                self.history_start == 0 && self.window_size == self.dict.region_len(),
                "an in-place dictionary frame needs the dictionary alone at the front of the history",
            );
            self.history_abs_start + self.window_size
        } else {
            self.history_abs_start + self.window_size + self.max_window_size
        };
    }

    /// Drop the borrowed input window (the caller's slice no longer lives).
    pub(crate) fn clear_borrowed_window(&mut self) {
        self.borrowed_input = None;
        self.borrowed_block = None;
        self.borrowed_dict = false;
        self.borrowed_abs_base = 0;
    }

    /// Make `[block_start, block_end)` the active borrowed block BEFORE the
    /// scan, so the emit pipeline's pre-scan `get_last_space().len()` reserve
    /// reports this block (not a stale or whole-input range).
    pub(crate) fn stage_borrowed_block(&mut self, block_start: usize, block_end: usize) {
        let (_ptr, total) = self
            .borrowed_input
            .expect("stage_borrowed_block requires a registered borrowed window");
        // Always-on (not debug_assert): the range feeds the unsafe slice
        // builders in `scan_source` / `get_last_space`, so an out-of-range or
        // inverted range must fault here, not deep in the kernel.
        assert!(
            block_start <= block_end && block_end <= total,
            "borrowed block bounds out of range: start={block_start} end={block_end} total={total}",
        );
        self.borrowed_block = Some((block_start, block_end));
        self.borrowed_abs_end = self
            .borrowed_abs_end
            .max(self.borrowed_abs_base + block_end);
    }

    /// Borrowed one-shot equivalent of [`Self::start_matching`]: scan
    /// `[block_start, block_end)` of the registered borrowed window in place
    /// (no copy into the history). Produces a byte-identical sequence stream to
    /// the owned path for in-window inputs — positions are absolute input
    /// offsets, candidate reads land in the same buffer, and the seam re-seed
    /// re-hashes the prior block's short-key tail exactly as the owned loop
    /// does once `history` spans it.
    pub(crate) fn start_matching_borrowed<K: EncodeTier>(
        &mut self,
        block_start: usize,
        block_end: usize,
        mut handle_sequence: impl FnMut(Sequence),
    ) {
        self.stage_borrowed_block(block_start, block_end);
        self.ensure_hash_tables();
        let current_len = block_end - block_start;
        if current_len == 0 {
            return;
        }
        let input_abs = self.borrowed_abs_base;
        let current_abs_start = input_abs + block_start;
        // Seam re-seed (mirror `start_matching`): re-hash the prior block's
        // trailing `BOUNDARY_DENSE_TAIL_LEN` bytes now that the readable
        // length spans them so a position whose short key could not form at
        // the prior block end becomes matchable. The borrowed floor is the
        // input start, so the first block (block_start == 0) backfills
        // nothing.
        let backfill_start = input_abs + block_start.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        if backfill_start < current_abs_start {
            self.insert_positions(backfill_start, current_abs_start);
        }
        self.start_matching_fast_loop::<K>(current_abs_start, current_len, &mut handle_sequence);
    }

    /// Borrowed one-shot equivalent of [`Self::skip_matching`]: stage the
    /// block (so `get_last_space` reports it for the RLE/Raw emit) without
    /// scanning. The block's bytes already sit in the borrowed buffer at
    /// their absolute offsets, so a future block reaches them by offset just
    /// as the owned skip's `history` append makes them reachable; the
    /// `Some(false)` dictionary-priming case hashes every position so future
    /// blocks can MATCH them, mirroring the owned skip's prime path.
    pub(crate) fn skip_matching_borrowed(
        &mut self,
        block_start: usize,
        block_end: usize,
        incompressible_hint: Option<bool>,
    ) {
        self.stage_borrowed_block(block_start, block_end);
        if incompressible_hint.is_none() {
            return;
        }
        self.ensure_hash_tables();
        // Input offsets to absolute positions; the seam stays inside the input.
        let input_abs = self.borrowed_abs_base;
        let seam_start = input_abs + block_start.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        let block_start = input_abs + block_start;
        let block_end = input_abs + block_end;
        // Seam before the block, as on the owned skip path: the preceding
        // block's last few positions were hashed under a source that ended
        // there, so without re-seeding them a match starting just before this
        // block has no entry to find.
        if seam_start < block_start {
            self.insert_positions(seam_start, block_start);
        }
        // Written off as noise without being searched — index it sparsely so a
        // later block duplicating it has something to match against, which is
        // what the owned path already does.
        if incompressible_hint == Some(true) {
            // The same step the owned skip path uses, and through the same
            // entry point, so the stream-headroom bound its assert pins
            // still holds: the insert loop advances with an unchecked
            // `pos += step` in ABSOLUTE stream coordinates, which the
            // headroom reserve is sized for at this step and no wider.
            self.insert_positions_with_step(block_start, block_end, DFAST_INCOMPRESSIBLE_SKIP_STEP);
            // And densely at the end, so the next block can match across the
            // boundary without the sparse step having thinned out exactly the
            // positions it will look for first.
            let tail_start = block_end
                .saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN)
                .max(block_start);
            if tail_start < block_end {
                self.insert_positions(tail_start, block_end);
            }
        } else {
            self.insert_positions(block_start, block_end);
        }
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
    fn probe_tail_ip0_only<K: EncodeTier, const MLS: usize>(
        &self,
        current_abs_start: usize,
        current_len: usize,
        ip0: usize,
        literals_start: usize,
        borrowed: bool,
    ) -> Option<MatchCandidate> {
        debug_assert!(ip0 + HASH_READ_SIZE <= current_len);
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        let short_shift = 64 - self.short_hash_bits;
        let long_shift = 64 - self.long_hash_bits;

        let abs_ip0 = current_abs_start + ip0;
        let lit_len_ip0 = ip0 - literals_start;
        // Byte source through `scan_source()` so the tail probe reads the
        // borrowed input in place when a borrowed window is active.
        let (history_base_ptr, history_start_offset, history_abs_start, position_base, concat_len) =
            self.scan_source();

        // Per-position window-low bound, as the fast loop's probe floor. A
        // borrowed input keeps every byte in place, so the input's start alone
        // would admit candidates older than the advertised window and emit an
        // unresolvable offset for over-window inputs; bound by `abs_ip0 -
        // advertised_window` as well. Owned mode keeps the eviction floor
        // `history_abs_start`.
        let wlow = if borrowed {
            // No lower than the input's start, below which lie earlier frames.
            abs_ip0
                .saturating_sub(self.max_window_size)
                .max(history_abs_start)
        } else {
            history_abs_start
        };

        let concat_idx0 = abs_ip0 - history_abs_start;

        // SAFETY: `concat_idx0 + 8 <= concat_len` follows from the
        // caller's `ip0 + HASH_READ_SIZE <= current_len` precondition
        // (the live history contains the full current block).
        let v8_0 = unsafe {
            (history_base_ptr.add(history_start_offset + concat_idx0) as *const u64)
                .read_unaligned()
        };
        // `v4_0` (low 4 bytes) is the 4-byte equality-gate key below; the short
        // HASH keys on the `MLS`-byte window (upstream `ZSTD_hashPtr(ip, hBitsS,
        // mls)`).
        let v4_0 = v8_0 & 0xFFFF_FFFF;
        let mixed_l0 = v8_0.wrapping_mul(PRIME);
        let mixed_s0 = short_key::<MLS>(v8_0).wrapping_mul(PRIME);
        let hl0_idx = dfast_slot(mixed_l0, long_shift);
        let hs0_idx = dfast_slot(mixed_s0, short_shift);

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
        let idxl0 = self.packed_in(
            unsafe { *self.long_ptr().add(hl0_idx) },
            mixed_l0,
            long_shift,
        );
        let idxs0 = self.packed_in(
            unsafe { *self.short_ptr().add(hs0_idx) },
            mixed_s0,
            short_shift,
        );

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
            if cand_pos >= wlow && cand_pos < abs_ip0 {
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
                        let ext = K::common_prefix_len_ptr(lhs, rhs, max_fwd);
                        match_len += ext;
                    }
                    let concat = unsafe {
                        core::slice::from_raw_parts(
                            history_base_ptr.add(history_start_offset),
                            concat_len,
                        )
                    };
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
            if cand_pos_s >= wlow && cand_pos_s < abs_ip0 {
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
                        let ext = K::common_prefix_len_ptr(lhs, rhs, max_fwd);
                        s_match_len += ext;
                    }
                    let concat = unsafe {
                        core::slice::from_raw_parts(
                            history_base_ptr.add(history_start_offset),
                            concat_len,
                        )
                    };
                    let short_cand = extend_backwards_shared(
                        concat,
                        history_abs_start,
                        cand_pos_s,
                        abs_ip0,
                        s_match_len,
                        lit_len_ip0,
                    );
                    if short_cand.match_len >= short_hit_floor::<MLS>() {
                        return Some(short_cand);
                    }
                }
            }
        }

        None
    }

    /// Upstream zstd `zstd_double_fast.c` post-match rep-0 extension. After the
    /// primary match has been emitted and `pos` advanced past it, upstream zstd
    /// opportunistically chains additional `rep_2`-coded matches at the
    /// new cursor as long as 4 bytes at `ip` keep matching the bytes at
    /// `ip - offset_2` (in upstream zstd naming; in Rust offset terms this is
    /// `offset_hist[1]` once `lit_len == 0` after the just-emitted
    /// primary). Each iteration:
    ///
    ///   * emits one zero-literal sequence with the old `offset_hist[1]`,
    ///   * swaps `offset_hist[0]` ↔ `offset_hist[1]` (the upstream zstd
    ///     `offset_2 = offset_1; offset_1 = old_offset_2;` swap),
    ///   * skips the hash-table probe entirely on every extra match.
    ///
    /// Critically uses upstream zstd's `MINMATCH = 4` here rather than the
    /// `DFAST_MIN_MATCH_LEN = 5` enforced on the main search
    /// loop. The upstream zstd accepts any 4-byte rep extension; we mirror that
    /// because the rep emission carries no offset cost — even a 4-byte
    /// rep is a net win over re-running the full hash search. Returns
    /// the new value of `pos` and updates `literals_start` in place to
    /// the post-rep-chain anchor.
    fn extend_with_repcode_after_match<K: EncodeTier>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        mut pos: usize,
        literals_start: &mut usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) -> usize {
        // Source bytes + rebase coordinate through `scan_source()` so a
        // borrowed window's rep-extension reads the in-place input.
        //
        // Resolved once for the whole chain, not per link. Nothing the loop
        // does moves them: the only mutation reaching the buffer is
        // `insert_position`'s rebase, which shifts slot values and
        // `position_base` — the one field of the five this loop discards —
        // and never reallocates the history.
        let (base_ptr, start_offset, abs_start, _position_base, concat_len) = self.scan_source();
        // SAFETY: `base_ptr + start_offset` is the live source start and
        // `concat_len` its readable length (owned history or borrowed
        // input); every read below is gated `< concat.len()`. Raw-ptr
        // backed, so no borrow is held on `self`.
        let concat = unsafe { core::slice::from_raw_parts(base_ptr.add(start_offset), concat_len) };
        // Scanned in place, the dictionary is not in `concat` but at the
        // front of the history, right before it in the window.
        let dict: &[u8] = if self.borrowed_dict {
            // SAFETY: the attached dictionary is the committed history, which
            // nothing writes while the frame is matched.
            unsafe {
                core::slice::from_raw_parts(
                    self.history.as_ptr().add(self.history_start),
                    self.dict.region_len(),
                )
            }
        } else {
            &[]
        };
        loop {
            // Need at least DFAST_REP_MIN_MATCH_LEN bytes of room past `pos`.
            if pos + DFAST_REP_MIN_MATCH_LEN > current_len {
                break;
            }
            // After a primary emit `literals_start == pos`, so `lit_len`
            // on the next sequence is zero — upstream zstd's rep probe uses
            // `offset_2` (== `offset_hist[1]` under our encoding).
            let rep = self.offset_hist[1] as usize;
            if rep == 0 {
                break;
            }
            let abs_pos = current_abs_start + pos;
            let cur_idx = abs_pos - abs_start;
            // `checked_sub` is the authoritative bound here: a valid rep
            // can reach beyond the current block into retained history
            // (the contiguous `live_history()` buffer covers
            // `history_abs_start..history_abs_end`), so the only hard
            // constraint is `cur_idx >= rep` (i.e. the candidate is in
            // the addressable history range). A previous draft also
            // gated on `rep > pos`, which over-rejected valid offsets
            // that point into retained history near block boundaries —
            // exactly the upstream zstd-style chain win this helper is meant to
            // recover.
            if cur_idx + DFAST_REP_MIN_MATCH_LEN > concat.len() {
                break;
            }
            let match_len = match cur_idx.checked_sub(rep) {
                Some(cand_idx) => {
                    // Cheap 4-byte gate before the SIMD `common_prefix_len`. Read
                    // it as one unaligned u32 rather than a slice `!=` (which
                    // lowers to a libc `memcmp` CALL). Bounds: `cur_idx + 4 <=
                    // len` from the `DFAST_REP_MIN_MATCH_LEN` (= 4) check above,
                    // and `cand_idx = cur_idx - rep < cur_idx` so `cand_idx + 4
                    // <= len` too.
                    let gate_eq = unsafe {
                        concat.as_ptr().add(cur_idx).cast::<u32>().read_unaligned()
                            == concat.as_ptr().add(cand_idx).cast::<u32>().read_unaligned()
                    };
                    if !gate_eq {
                        break;
                    }
                    // SAFETY: `cand_idx < cur_idx < concat.len()` (above), and
                    // the compare stops at the shorter of the two tails.
                    unsafe {
                        K::common_prefix_len_ptr(
                            concat.as_ptr().add(cand_idx),
                            concat.as_ptr().add(cur_idx),
                            concat.len() - cur_idx,
                        )
                    }
                }
                // Before the input: in the dictionary, when it is scanned in
                // place, and counted on into the input after it.
                None if rep - cur_idx <= dict.len() => count_forward_dict_2segment(
                    dict,
                    dict.len() - (rep - cur_idx),
                    concat,
                    cur_idx,
                    0,
                ),
                None => break,
            };
            if match_len < DFAST_REP_MIN_MATCH_LEN {
                break;
            }
            // Upstream zstd immediate-repcode insertion (zstd_double_fast.c:314-315):
            // INSIDE the rep chain, upstream writes BOTH hash tables at the rep
            // position itself (`hashSmall[hash(ip)] = ip; hashLong[hash(ip)] = ip`)
            // before advancing — NOT the `curr+2 / ip-2 / ip-1` primary-match
            // complementary set (that pattern belongs to `_match_found`, lines
            // 300-304, reached only by the non-rep store path). Inserting the
            // wrong set here leaves the rep-position keys stale, so a later
            // position re-resolves the long hash to an older far candidate
            // instead of the stable rep offset — the dfast ratio gap vs C.
            self.insert_position(abs_pos);
            // Emit zero-literal rep sequence.
            handle_sequence(Sequence::Triple {
                literal_len: 0,
                offset: rep,
                match_len,
            });
            // A repeat at `offset_hist[1]` with no literals swaps the two
            // (zstd_double_fast.c:313).
            self.offset_hist.swap(0, 1);
            pos += match_len;
            *literals_start = pos;
        }
        pos
    }

    /// Index the starts of the block's dense tail that the scan did not pass,
    /// `pos` being where it stopped, so the next block can match into them.
    pub(crate) fn seed_remaining_hashable_starts(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        pos: usize,
    ) {
        let boundary_tail_start = current_len.saturating_sub(Self::BOUNDARY_DENSE_TAIL_LEN);
        // Never the gap the scan's step skipped before it stopped, nor what a
        // match ending in the tail covered: upstream's loops index neither,
        // and on incompressible input that gap is the step, hundreds of bytes
        // by a block's end. Seeding it cost 11% of the wall-clock time of a
        // 100 KiB random frame at level 3 (1.18x -> 1.05x of libzstd in the
        // same run) for 43 bytes over 3.1 MB of corpus at levels 3-4.
        let mut seed_pos = pos.min(current_len).max(boundary_tail_start);
        // Every start with a whole short key; past it nothing is hashable.
        while seed_pos + self.mls <= current_len {
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
        if let (Some((ptr, _total)), Some((_block_start, block_end))) =
            (self.borrowed_input, self.borrowed_block)
        {
            // Borrowed one-shot window: the input's first byte is at
            // `borrowed_abs_base` and the readable length is the active
            // block's end. No history concat, no copy into it. Candidate reads
            // from earlier blocks land below `block_end`, in range. Slots pack
            // against `position_base`, as the owned path's do.
            return (
                ptr,
                0,
                self.borrowed_abs_base,
                self.position_base,
                block_end,
            );
        }
        let start_offset = self.history_start;
        (
            self.history.as_ptr(),
            start_offset,
            self.history_abs_start,
            self.position_base,
            // Committed bytes only, as in `owned_scan_descriptor`: with in-place
            // ingest the next block's bytes already sit past the end, and the
            // hash-insert guard fed from here must not admit positions in them.
            self.history.len() - self.uncommitted_len - start_offset,
        )
    }

    /// Byte-source descriptor for the BORROWED kernel: `(input_ptr,
    /// block_end)`, the input's first byte and the active block's end in
    /// input offsets. The input's first position is `borrowed_abs_base`.
    #[inline(always)]
    fn borrowed_scan_descriptor(&self) -> (*const u8, usize) {
        let (ptr, _total) = self
            .borrowed_input
            .expect("BORROWED kernel dispatched without a borrowed window");
        let (_block_start, block_end) = self
            .borrowed_block
            .expect("BORROWED kernel dispatched without a staged block");
        (ptr, block_end)
    }

    /// Byte-source descriptor for the owned (history-concat) kernel. Mirrors
    /// the owned arm of [`Self::scan_source`] without the borrowed branch, so
    /// the `!BORROWED` const kernel reads the rebased fields directly.
    #[inline(always)]
    fn owned_scan_descriptor(&self) -> (*const u8, usize, usize, usize, usize) {
        let start_offset = self.history_start;
        (
            self.history.as_ptr(),
            start_offset,
            self.history_abs_start,
            self.position_base,
            // Committed bytes only: the next block's bytes can already sit
            // past the end, and a forward match count must not reach into
            // them.
            self.history.len() - self.uncommitted_len - start_offset,
        )
    }

    /// `(ptr, len)` of the block currently being emitted, for the literal
    /// slices in `emit_candidate` / `emit_trailing_literals`. Owned: the
    /// `history` concat tail (`last_len` bytes). Borrowed: the in-place
    /// input sub-slice `[current_abs_start, block_end)`. Returns a raw
    /// pointer (not a `&[u8]` tied to `&self`) so the caller can rebuild
    /// the slice locally and still take `&mut self` for `offset_hist`.
    #[inline]
    fn current_block_ptr_len(&self, current_abs_start: usize) -> (*const u8, usize) {
        if let (Some((ptr, _total)), Some((_block_start, block_end))) =
            (self.borrowed_input, self.borrowed_block)
        {
            // SAFETY: borrowed liveness contract; the block's input offset
            // `current_abs_start - borrowed_abs_base` = block_start <=
            // block_end <= buffer len (entry-validated).
            let block_start = current_abs_start - self.borrowed_abs_base;
            (unsafe { ptr.add(block_start) }, block_end - block_start)
        } else {
            let last_len = self.window_blocks.back().copied().unwrap_or(0);
            // Measure back from the COMMITTED end: unclaimed input can sit
            // past it.
            let off = self.history.len() - self.uncommitted_len - last_len;
            // SAFETY: `off + last_len` is the committed end, in bounds.
            (unsafe { self.history.as_ptr().add(off) }, last_len)
        }
    }

    // Force-inline on native (the dfast monolithization speedup) but NOT on
    // wasm32, where inlining these per-match helpers into every call site
    // bloats the module past the .wasm size budget; wasm is size-, not
    // speed-sensitive, so let LLVM keep them out-of-line there.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn emit_candidate<const MLS: usize>(
        &mut self,
        current_abs_start: usize,
        literals_start: &mut usize,
        candidate: MatchCandidate,
        scan_pos: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) -> usize {
        // Upstream zstd `zstd_double_fast.c` parity: the inner search loop already
        // inserts every position it VISITS (step-accelerated), so the literal
        // run is hashed exactly as densely as the upstream zstd's cursor swept it —
        // stepped-over and block-anchor (position 0) positions are NOT
        // re-inserted here (the upstream zstd skips them too via `ip += (ip ==
        // prefixStart)` + the growing `step`). Match interior: upstream zstd fills only
        // the sparse 3-target set (`curr+2`, `ip-2`, `ip-1`), each clamped to
        // the open match interval.
        // Upstream zstd complementary insertion (zstd_double_fast.c:300-304) is
        // ASYMMETRIC across the two tables:
        //   hashLong:  curr+2, ip-2
        //   hashSmall: curr+2, ip-1
        // The previous form inserted curr+2/ip-2/ip-1 into BOTH tables, putting
        // ip-1 into long and ip-2 into short that upstream never writes —
        // polluting the long table so later positions resolve to the wrong
        // (far, non-rep) candidate. Mirror the exact per-table target set.
        //
        // `curr` is the iteration's SCAN position (`scan_pos`, upstream `curr`
        // = `(U32)(ip-base)` fixed at zstd_double_fast.c:184), NOT the match
        // start: upstream advances `ip` for a rep1 (`ip++`, the match begins at
        // `scan+1`) and rewinds it during backward catch-up, but `curr` stays
        // pinned to the scan cursor. Anchoring `curr+2` on `match_start` instead
        // shifts the insert by +1 on every rep1 (and by the catch-up length on
        // extended matches), seeding the short hash with the wrong positions —
        // a divergence that compounds across a block.
        let post_match_end = candidate.start + candidate.match_len;
        // `match_len >= DFAST_REP_MIN_MATCH_LEN` (4) for every committed match,
        // so `post_match_end >= 4`: the `- 2` / `- 1` cannot underflow (plain
        // arithmetic, no `saturating_*` masking).
        let curr_plus_2 = scan_pos + 2;
        let ip_minus_2 = post_match_end - 2;
        let ip_minus_1 = post_match_end - 1;
        self.insert_complementary::<MLS>(curr_plus_2, ip_minus_2, ip_minus_1);
        // A committed match starts at or after the literal run it closes.
        let start = candidate.start - current_abs_start;
        debug_assert!(start >= *literals_start);
        let literal_len = start - *literals_start;
        handle_sequence(Sequence::Triple {
            literal_len,
            offset: candidate.offset,
            match_len: candidate.match_len,
        });
        note_match_offset(&mut self.offset_hist, candidate.offset as u32, literal_len);
        *literals_start = start + candidate.match_len;
        start
    }

    fn emit_trailing_literals(
        &self,
        current_abs_start: usize,
        literals_start: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        let (_, cur_len) = self.current_block_ptr_len(current_abs_start);
        if literals_start < cur_len {
            handle_sequence(Sequence::Literals {
                len: cur_len - literals_start,
            });
        }
    }

    pub(crate) fn ensure_hash_tables(&mut self) {
        // Independent sizing per upstream zstd `clevels.h`: long-hash =
        // `hashLog`, short-hash = `chainLog`. Lazy allocation so
        // Fastest/Uncompressed never pay the dfast-level memory cost.
        let total = self.long_len() + self.short_len();
        if self.tables.len() != total {
            // A matcher no context laid out (driven on its own) allocates the
            // tables itself: one zeroed allocation for both regions.
            self.tables = Table::owned(alloc::vec![DFAST_EMPTY_SLOT; total]);
        }
    }

    fn compact_history(&mut self) {
        if self.history_start == 0 {
            return;
        }
        // Drain the dead prefix at a quarter window (paired with the one-time
        // `reserve_exact` in `commit_block`) so the buffer stays near
        // `window + window/4` instead of doubling to ~2x window on long streams.
        // Compare against the COMMITTED length: `history.len()` also counts
        // bytes no block has claimed yet, which would delay the trigger by
        // however much input happens to be read ahead.
        if self.history_start >= (self.max_window_size >> 2)
            || self.history_start * 2 >= self.history.len() - self.uncommitted_len
        {
            self.history.drain_front(self.history_start);
            self.history_start = 0;
        }
    }

    pub(crate) fn live_history(&self) -> &[u8] {
        // Stop at the committed end, not at `history.len()`: the next block's
        // bytes may already be read into the tail, and a scan must not see
        // past the block it is compressing (a forward match count would
        // otherwise run into input the block does not own).
        &self.history[self.history_start..self.history.len() - self.uncommitted_len]
    }

    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn insert_positions(&mut self, start: usize, end: usize) {
        self.insert_range(start, end, 1);
    }

    /// Hash every `step`-th position in `[start, end)` into both tables,
    /// resolving the scan source, the rebase check, the slot bias and the slot
    /// format once for the whole range rather than per position.
    fn insert_range(&mut self, start: usize, end: usize, step: usize) {
        if self.tagged {
            self.insert_range_as::<true>(start, end, step);
        } else {
            self.insert_range_as::<false>(start, end, step);
        }
    }

    fn insert_range_as<const TAGGED: bool>(&mut self, start: usize, end: usize, step: usize) {
        debug_assert!(step >= 1, "insert_range needs a positive step");
        // Source the byte buffer + rebase coordinates through `scan_source()`
        // so a borrowed window's batch re-seed hashes the in-place input
        // (owned returns its `history` fields, byte-identical).
        let (history_base_ptr, history_start, history_abs_start, _pb0, concat_len) =
            self.scan_source();
        let start = start.max(history_abs_start);
        let end = end.min(history_abs_start + concat_len);
        if start >= end {
            return;
        }
        // Hoist the rebase trigger out of the inner loop (a single
        // `ensure_room_for(end - 1)` covers every `pack_slot` in the range);
        // it may advance `position_base`, so read that AFTER the call. An
        // input scanned in place packs as the owned path does.
        self.ensure_room_for(end - 1);
        let position_base = self.position_base;

        // Snapshot the remaining per-call invariants. `&mut self` blocks the
        // optimiser from hoisting these loads across the inner loop — the
        // bodies of `short_hash` / `long_hash` writes mutate through `self`,
        // so each iteration would otherwise reload `*_hash_bits`. With ~1
        // input byte per call on the dfast hot path that re-load shape was
        // the dominant cost in the per-position cluster. `history_base_ptr`
        // stays valid across `ensure_room_for` (it never reallocs `history`).
        let short_hash_bits = self.short_hash_bits;
        let long_hash_bits = self.long_hash_bits;
        let short_hash_ptr = self.short_mut_ptr();
        let long_hash_ptr = self.long_mut_ptr();
        let short_shift = 64 - short_hash_bits;
        let long_shift = 64 - long_hash_bits;
        let mls = self.mls;
        // The short key keeps the low `mls` bytes of an 8-byte load.
        let short_key_shift = 64 - 8 * mls;

        // Two contiguous regions in the input range:
        // * `[start .. long_safe_end)` — every position has at least 8
        //   bytes of lookahead, so both short and long hashes get
        //   inserted from a single 8-byte unaligned load.
        // * `[long_safe_end .. short_safe_end)` — only `mls..8` bytes
        //   remain, so only the short hash gets inserted.
        // Past `short_safe_end` neither hash has enough lookahead and
        // upstream zstd parity is "no insert" — skip entirely.
        let abs_concat_end = history_abs_start + concat_len;
        let long_safe_end = abs_concat_end.saturating_sub(7).min(end);
        let short_safe_end = abs_concat_end.saturating_sub(mls - 1).min(end);

        // SAFETY: `history_base_ptr.add(history_start + idx)` is
        // in-bounds for `idx + 8 <= concat_len`, which the two
        // `*_safe_end` cutoffs enforce. `short_hash_ptr.add(k)` /
        // `long_hash_ptr.add(k)` are in-bounds because
        // `ensure_hash_tables` sizes the two tables to `1 <<
        // *_hash_bits` and `k = mixed >> (64 - bits)` has at most
        // `bits` bits set. `position_base` and `history_abs_start`
        // are constant across the loop after the single `ensure_room_for`
        // call above. `packed` fits in `u32` by that same gate.
        // A block written off without being searched is indexed only so a LATER
        // duplicate can find it, and the short table alone does that: the search
        // probes it first and the long table is an accelerator on top. Writing
        // both scattered a store across twice the pages of a table this frame
        // otherwise never touches, and those pages are faulted in one by one —
        // on a mebibyte of noise at level 3 the sparse insert alone took ninety
        // page faults per frame.
        //
        // Which of the two the range wants is decided by `step`, and it is
        // decided ONCE here rather than at every position: the sparse walk then
        // carries neither the gate nor the long key it would throw away, and the
        // dense one advances by a constant.
        macro_rules! insert_long_range {
            ($pos:ident, $dense:literal, $step:expr) => {
                while $pos < long_safe_end {
                    unsafe {
                        let idx = $pos - history_abs_start;
                        let packed = (($pos - position_base) as u32) + 1;
                        let load_ptr = history_base_ptr.add(history_start + idx);
                        let v8 = (load_ptr as *const u64).read_unaligned();
                        // Upstream zstd parity (`zstd_compress_internal.h:923-924`):
                        // scalar `* prime8bytes` then shift to high bits. Drops
                        // the CRC32d-based kernel dispatch (3-4 instructions) for
                        // a single mul on the per-byte insert path. Short hash keys
                        // on the `mls`-byte window (upstream `ZSTD_hashPtr`).
                        let mixed_short =
                            (v8 << short_key_shift).wrapping_mul(0xCF1BBCDCB7A56463_u64);
                        let short_idx = dfast_slot(mixed_short, short_shift);
                        *short_hash_ptr.add(short_idx) =
                            live_slot::<TAGGED>(packed, mixed_short, short_shift);
                        if $dense {
                            let mixed_long = v8.wrapping_mul(0xCF1BBCDCB7A56463_u64);
                            let long_idx = dfast_slot(mixed_long, long_shift);
                            *long_hash_ptr.add(long_idx) =
                                live_slot::<TAGGED>(packed, mixed_long, long_shift);
                        }
                    }
                    $pos += $step;
                }
            };
        }
        let mut pos = start;
        if step == 1 {
            insert_long_range!(pos, true, 1);
        } else {
            insert_long_range!(pos, false, step);
        }
        while pos < short_safe_end {
            unsafe {
                let idx = pos - history_abs_start;
                let packed = ((pos - position_base) as u32) + 1;
                let load_ptr = history_base_ptr.add(history_start + idx);
                // Assembled from the `mls` bytes left, so the <8-byte tail is
                // never over-read.
                let mixed_short = short_key_at(load_ptr, concat_len - idx, mls)
                    .wrapping_mul(0xCF1BBCDCB7A56463_u64);
                let short_idx = dfast_slot(mixed_short, short_shift);
                *short_hash_ptr.add(short_idx) =
                    live_slot::<TAGGED>(packed, mixed_short, short_shift);
            }
            pos += step;
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
        // Clamping happens inside `insert_range`, against the source it
        // resolves — which is the borrowed window when one is staged. Clamping
        // here against the OWNED bounds as well is redundant for an owned
        // window (the two agree) and empties the range for a borrowed one,
        // which is a window a borrowed frame never populates.
        self.insert_range(start, end, step);
    }

    /// The four complementary insertions upstream makes after a match
    /// (`zstd_double_fast.c:300-304`), with the scan source, the rebase
    /// check and the slot bias resolved once for all four rather than per
    /// insertion.
    ///
    /// Upstream writes four inline hash-and-store pairs here. Routing each
    /// through a single-insertion helper re-derives the scan source, re-runs
    /// the rebase guard and rebuilds a bounds-checked slice every time, which
    /// measured at 7.6% of a level-3 encode against roughly nothing
    /// identifiable on the reference profile.
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn insert_complementary<const MLS: usize>(
        &mut self,
        curr_plus_2: usize,
        ip_minus_2: usize,
        ip_minus_1: usize,
    ) {
        if self.tagged {
            self.insert_complementary_as::<true, MLS>(curr_plus_2, ip_minus_2, ip_minus_1);
        } else {
            self.insert_complementary_as::<false, MLS>(curr_plus_2, ip_minus_2, ip_minus_1);
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn insert_complementary_as<const TAGGED: bool, const MLS: usize>(
        &mut self,
        curr_plus_2: usize,
        ip_minus_2: usize,
        ip_minus_1: usize,
    ) {
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        // `ensure_room_for` is monotone in its argument, so a base with room
        // for the furthest of the three has room for the nearer two, and all
        // four slots then pack against that single base. Runs before
        // `scan_source` / `position_base` are read because a rebase moves the
        // base out from under both.
        self.ensure_room_for(curr_plus_2.max(ip_minus_2).max(ip_minus_1));
        let (base_ptr, start_offset, abs_start, _position_base, concat_len) = self.scan_source();
        let position_base = self.position_base;
        let long_shift = 64 - self.long_hash_bits;
        let short_shift = 64 - self.short_hash_bits;
        let long_ptr = self.long_mut_ptr();
        let short_ptr = self.short_mut_ptr();
        // SAFETY: `base_ptr + start_offset` is the live source start (owned
        // `history[history_start..]` or the borrowed input slice) and
        // `concat_len` its readable byte count, taken exactly as
        // `insert_masked` takes them; every load below is gated on `idx + 8`
        // (long key) or `idx + MLS` (short key) against that length, and a
        // `pos` below `abs_start` wraps `idx` into a value no gate admits.
        let src = unsafe { base_ptr.add(start_offset) };
        let pack = |pos: usize| -> u32 {
            debug_assert!(
                pos >= position_base,
                "complementary insert {pos} below position_base {position_base}",
            );
            ((pos - position_base) as u32) + 1
        };

        // Order matters when two targets collide in one table: it is the
        // order `insert_long` / `insert_short` were called in, so a collision
        // leaves the same occupant as before.
        for pos in [curr_plus_2, ip_minus_2] {
            let idx = pos.wrapping_sub(abs_start);
            if idx + HASH_READ_SIZE <= concat_len {
                // SAFETY: the gate above puts `idx + 8` inside `concat_len`.
                let value = unsafe { (src.add(idx) as *const u64).read_unaligned() };
                let mixed = value.wrapping_mul(PRIME);
                let slot = dfast_slot(mixed, long_shift);
                debug_assert!(slot < self.long_len());
                // SAFETY: `long_shift = 64 - long_hash_bits`, so `slot` is
                // below `1 << long_hash_bits`, the long table's length.
                unsafe { *long_ptr.add(slot) = live_slot::<TAGGED>(pack(pos), mixed, long_shift) };
            }
        }
        // Short key is the low `MLS` bytes in the same form the fast-loop
        // probe builds from its 8-byte load; a position with fewer than `MLS`
        // readable bytes is left to the seam re-seed rather than hashed
        // against a zero-padded key.
        for pos in [curr_plus_2, ip_minus_1] {
            let idx = pos.wrapping_sub(abs_start);
            if idx + MLS <= concat_len {
                // SAFETY: the gate above puts `idx + MLS` inside `concat_len`.
                let mixed = unsafe { short_key_exact::<MLS>(src.add(idx)) }.wrapping_mul(PRIME);
                let slot = dfast_slot(mixed, short_shift);
                debug_assert!(slot < self.short_len());
                // SAFETY: `short_shift = 64 - short_hash_bits`, so `slot` is
                // below the short table's length, and `short_mut_ptr` already
                // points at the short region.
                unsafe {
                    *short_ptr.add(slot) = live_slot::<TAGGED>(pack(pos), mixed, short_shift)
                };
            }
        }
    }

    /// Write `pos` into both hash tables. The asymmetric per-table targets
    /// upstream writes after a match live in [`Self::insert_complementary`],
    /// which resolves their shared coordinates once instead of per position.
    #[inline]
    pub(crate) fn insert_position(&mut self, pos: usize) {
        if self.tagged {
            self.insert_position_as::<true>(pos);
        } else {
            self.insert_position_as::<false>(pos);
        }
    }

    #[inline]
    fn insert_position_as<const TAGGED: bool>(&mut self, pos: usize) {
        // Source the bytes + rebase coordinates through `scan_source()` so a
        // borrowed window's seam / tail re-seeds hash the in-place input
        // exactly as the owned path hashes its `history` concat.
        let (base_ptr, start_offset, abs_start, _position_base, concat_len) = self.scan_source();
        let idx = pos.wrapping_sub(abs_start);
        // Pre-rebase guard, so the slot value below fits its range; owned
        // and in-place inputs pack alike.
        self.ensure_room_for(pos);
        let packed = self.pack_slot(pos);
        // SAFETY: `base_ptr + start_offset` is the live source start (owned
        // `history[history_start..]` or the borrowed input slice) and
        // `concat_len` its readable byte count; the `idx + mls` / `idx + 8`
        // gates keep both keyed reads in range. The slice is raw-pointer
        // backed (holds no borrow on `self`), so the `&mut self.short_hash`
        // / `&mut self.long_hash` writes below stay sound. The `*_hash_index`
        // helpers mask to `long_hash_bits` / `short_hash_bits` and
        // `ensure_hash_tables` sizes both tables to `1 << bits`, so every
        // index is below the table length — eliding the bounds check on this
        // per-byte hot path saves ~4 instructions per call.
        //
        // Single-slot overwrite (upstream parity): upstream
        // `ZSTD_compressBlock_doubleFast_*` writes a single `U32` per hash
        // position and relies on the dense `_search_next_long` retry in
        // `hash_candidate` (via `best_match`) to preserve compression ratio.
        // Short key needs `mls` readable bytes. A position closer to the
        // source end is not inserted here; the `start_matching` seam re-seed
        // picks it up once the next block extends the source far enough to
        // form its full key.
        let concat = unsafe { core::slice::from_raw_parts(base_ptr.add(start_offset), concat_len) };
        if idx + self.mls <= concat_len {
            let short_shift = 64 - self.short_hash_bits;
            let mixed = short_hash_key(&concat[idx..], self.mls).wrapping_mul(DFAST_HASH_PRIME);
            let short = dfast_slot(mixed, short_shift);
            debug_assert!(short < self.short_len());
            // Short region starts at `long_len`.
            let slot = self.long_len() + short;
            unsafe {
                *self.tables.get_unchecked_mut(slot) =
                    live_slot::<TAGGED>(packed, mixed, short_shift)
            };
        }

        if idx + 8 <= concat_len {
            let long_shift = 64 - self.long_hash_bits;
            let key = u64::from_le_bytes(concat[idx..idx + 8].try_into().unwrap());
            let mixed = key.wrapping_mul(DFAST_HASH_PRIME);
            let long = dfast_slot(mixed, long_shift);
            debug_assert!(long < self.long_len());
            unsafe {
                *self.tables.get_unchecked_mut(long) =
                    live_slot::<TAGGED>(packed, mixed, long_shift)
            };
        }
    }

    /// Short-hash index of the first `mls` bytes of `data` (upstream
    /// `ZSTD_hashPtr(ip, hBitsS, mls)`); `data` MUST hold at least `mls` bytes.
    #[cfg(test)]
    pub(crate) fn short_hash_index(&self, data: &[u8]) -> usize {
        self.hash_index_with_bits(short_hash_key(data, self.mls), self.short_hash_bits)
    }

    #[cfg(test)]
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

    #[cfg(test)]
    fn hash_index_with_bits(&self, value: u64, bits: usize) -> usize {
        // Upstream zstd parity (`zstd_compress_internal.h:923-924`, `ZSTD_hash8`):
        // a single 64-bit multiply by `prime8bytes` followed by a high-bits
        // shift. Drops the CRC32d + rotate + mul kernel dispatch the rest
        // of the crate uses — for dfast the upstream zstd's scalar hash is
        // distribution-equivalent and one instruction shorter on the hot
        // path.
        let mixed = value.wrapping_mul(DFAST_HASH_PRIME);
        (mixed >> (64 - bits)) as usize
    }
}

/// Upstream zstd `prime8bytes` (`zstd_compress_internal.h`), the multiplier
/// both dfast hashes take.
const DFAST_HASH_PRIME: u64 = 0xCF1BBCDCB7A56463;

/// The short hash's key over the first `mls` bytes of `data`, the form every
/// short-key site builds ([`short_key`]). `data` MUST hold at least `mls`
/// bytes — every call site gates on that lookahead, so no zero-padded key is
/// ever formed (a padded short key would populate buckets for starts upstream
/// skips).
#[inline(always)]
fn short_hash_key(data: &[u8], mls: usize) -> u64 {
    debug_assert!(data.len() >= mls, "short hash needs a full {mls}-byte key");
    // SAFETY: `data` holds `data.len() >= mls` readable bytes.
    unsafe { short_key_at(data.as_ptr(), data.len(), mls) }
}

/// Per-kernel body of the dfast fast match loop. Mirrors the BT/opt
/// `*_body!` macros: the wrapper carries the `#[target_feature]` umbrella and
/// passes its tier `common_prefix_len_ptr` as `$cpl`, so the 7 match-extension
/// cpl calls inline under one umbrella and `select_kernel()` is resolved ONCE
/// per block in the bare dispatcher, never per cpl call.
/// Walk a match back over the literal run (upstream zstd
/// `zstd_double_fast.c:236-239`, `ip > anchor && match > prefixLowest`), in the
/// block's own coordinates: the match path starts from pointers the probe
/// already holds, so no position of any other system is rebuilt for it.
///
/// # Safety
///
/// `candidate` and `floor` point into the buffer holding the block at
/// `block_ptr`, with `floor <= candidate < block_ptr + cursor` and the bytes
/// from `floor` up to the cursor readable; `literals_start <= cursor`.
#[inline(always)]
unsafe fn catch_up_in_block(
    mut candidate: *const u8,
    floor: *const u8,
    block_ptr: *const u8,
    mut cursor: usize,
    literals_start: usize,
    mut match_len: usize,
) -> MatchCandidate {
    // SAFETY: the guards keep both reads above `floor` and the run's start,
    // inside the buffer by the caller's contract.
    unsafe {
        while cursor > literals_start
            && candidate > floor
            && *candidate.sub(1) == *block_ptr.add(cursor - 1)
        {
            candidate = candidate.sub(1);
            cursor -= 1;
            match_len += 1;
        }
        MatchCandidate {
            start: cursor,
            offset: block_ptr.add(cursor).offset_from(candidate) as usize,
            match_len,
        }
    }
}

macro_rules! start_matching_fast_loop_body {
    ($self:ident, $current_abs_start:ident, $current_len:ident, $handle_sequence:ident, $cpl:path, $tier:ty, $borrowed:expr, $tagged:expr, $mls:ident) => {{
        // Behaviour change vs the pre-refactor `start_matching_general`:
        // this fast loop deliberately drops the strict-incompressible
        // early-skip path (the `block_looks_incompressible_strict` short
        // circuit + `miss_run` / `DFAST_LOCAL_SKIP_TRIGGER` thresholding).
        // The step ramp is now driven purely by distance traveled
        // (`DFAST_SKIP_STEP_GROWTH_INTERVAL = 256`, matching upstream zstd's
        // `kStepIncr = 1 << kSearchStrength`), so blocks the strict gate used to bail out of early now scan
        // through the standard ramp. `block_looks_incompressible_strict`
        // is still used by `levels/fastest.rs` for the Fastest preset
        // and by `incompressible.rs` unit tests, so the helper itself
        // stays.
        //
        // Upstream zstd outer/inner structure (`zstd_double_fast.c:167-322`):
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
        // Advance the slot packing's rebase past this block, owned or in
        // place: both pack against `position_base`.
        $self.ensure_room_for($current_abs_start + $current_len - 1);
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        // One shift per table for both the slot and its tag: two counts per
        // table are four invariants the scan cannot keep in registers.
        let short_key_shift = dfast_key_shift::<{ $tagged }>(64 - $self.short_hash_bits);
        let long_key_shift = dfast_key_shift::<{ $tagged }>(64 - $self.long_hash_bits);
        // Tagged slots are `packed << DFAST_TAG_BITS | tag`. `$tagged` is a
        // const, so every tag term below folds away on the untagged kernel.
        debug_assert_eq!($tagged, $self.tagged, "kernel and table disagree on the slot format");
        let tag_shift: u32 = if $tagged { DFAST_TAG_BITS } else { 0 };
        let mut pos = 1usize;
        let mut literals_start = 0usize;

        // Advertised window cap = `1 << window_log`. Owned mode evicts, so
        // `history_abs_start` already bounds candidates to the live window;
        // borrowed mode keeps the whole input in place (no eviction), so an
        // OVER-window borrowed scan must explicitly reject candidates whose
        // offset would exceed the advertised window — otherwise it would emit
        // an offset the decoder cannot resolve. Hoisted (loop-invariant).
        let advertised_window = $self.max_window_size;

            // Read once per block, above the outer loop, as upstream resolves
            // its bases once per block: `ensure_room_for` above covered the
            // whole block, so a match moves neither the tables nor the bytes.
            // `emit_candidate` on the tail path may, and nothing past it reads
            // these: `pos` only grows, so that path is taken again or ends.
            // The byte-source quintet comes through `scan_source()` so a
            // borrowed one-shot window can substitute the owned `history`
            // concat without touching this kernel body: the owned path
            // returns its rebased fields, a borrowed window its constant
            // descriptor. The hash-table pointers are mode-invariant (the
            // tables persist across owned/borrowed) so they stay direct.
            // `$borrowed` is a compile-time const (the kernel is monomorphised
            // per borrowed/owned), so only one arm survives. A borrowed input
            // starts at `borrowed_abs_base` with no offset into its buffer, and
            // packs against `position_base` as the owned history does.
            let (history_base_ptr, history_start_offset, history_abs_start, position_base, concat_len) =
                if $borrowed {
                    let (ptr, block_end) = $self.borrowed_scan_descriptor();
                    (ptr, 0usize, $self.borrowed_abs_base, $self.position_base, block_end)
                } else {
                    $self.owned_scan_descriptor()
                };
            let short_hash_ptr = $self.short_mut_ptr();
            let long_hash_ptr = $self.long_mut_ptr();
            // Block-relative cursor base, the shape upstream zstd scans with:
            // it walks `ip` against a single `base`, while our positions carry
            // three coordinate systems (block-relative `ip`, absolute, and the
            // history-concat index). Reads keyed off `ip` alone let the whole
            // concat axis stay out of the loop; the bias is a per-block
            // constant because both ends are re-snapshotted by the outer loop.
            //
            // SAFETY: `$current_abs_start >= history_abs_start` (the block is
            // part of live history), and `history_start_offset + bias` is the
            // block's first byte, in bounds of the buffer.
            let block_bias = $current_abs_start - history_abs_start;
            let block_ptr = unsafe { history_base_ptr.add(history_start_offset + block_bias) };
            // First live byte, where a walk back over the literals stops.
            let live_ptr = block_ptr.wrapping_sub(block_bias);
            // Readable bytes from the block's start. Equals `$current_len`
            // whenever the block ends the live history, which is the only
            // shape the fast loop runs in; asserted rather than assumed
            // because a mismatch would silently change match lengths.
            //
            // The forward-scan budgets below subtract from this with plain
            // arithmetic, not `saturating_sub`: entry needs `pos + 8 <=
            // block_len` and the advance breaks out the moment `ip1 + 8`
            // passes it, so `ip1 + 8 <= block_len` holds on every iteration
            // and `ip0 < ip1`. A clamp there would only hide a broken guard.
            let block_len = concat_len - block_bias;
            // Last cursor position with a full hash key still readable, which is
            // upstream's `ilimit`. The scan compares against this rather than
            // adding the lookahead to the cursor every position.
            //
            // Wrapping: a block shorter than a key wraps here, and leaves the
            // outer loop at its guard, before anything reads this.
            let scan_limit = $current_len.wrapping_sub(HASH_READ_SIZE);
            // Slot payload for a block-relative cursor: packing a position is
            // `(abs - position_base) + 1`, and `abs = $current_abs_start + ip`,
            // so the whole `position_base` term collapses into a per-block
            // constant and the per-position work is one add. `ensure_room_for`
            // at entry guarantees the sum stays inside `u32` for the whole
            // block, which is what makes the narrowing cast safe here.
            let packed_bias = (($current_abs_start - position_base) as u32) + 1;
            debug_assert!(
                !$borrowed || packed_bias as usize > advertised_window,
                "a borrowed input must start a window past the earlier frames",
            );
            // A slot value indexes its candidate's bytes directly off this
            // pointer, the way upstream indexes off `base`. Decoding a slot the
            // long way — `position_base + slot - 1` into position space, minus
            // `history_abs_start` into concat space, plus the source pointer and
            // its start offset — spends four constants that then have to stay
            // LIVE for the whole search loop. They do not fit: the loop already
            // reloads a dozen invariants from the stack every iteration, and
            // each one it stops needing frees a register for the ones left.
            //
            // Folded here, all four collapse into one pointer, and the loop's
            // per-candidate work becomes a single add.
            // Wrapping arithmetic, not `offset`/`add`: the fold is `-1` whenever
            // the rebase base and the history origin coincide, which is every
            // borrowed scan and the first owned one, so the intermediate lands
            // before the allocation. `offset` requires each intermediate to stay
            // in bounds and is undefined there even though nothing dereferences
            // it; `wrapping_offset` defers that requirement to the dereference,
            // which the gates below place inside live history.
            let slot_base_ptr = history_base_ptr
                .wrapping_add(history_start_offset)
                .wrapping_offset(position_base as isize - history_abs_start as isize - 1);
            // Lowest candidate position for the whole block. Owned history
            // evicts past the window, so its start is the floor. A borrowed
            // input keeps everything in place, and its floor is the block's end
            // less the window, once per block, as upstream takes
            // `prefixLowestIndex` from the block end
            // (`ZSTD_getLowestPrefixIndex(ms, endIndex, windowLog)`,
            // zstd_double_fast.c:115). A block never exceeds the window, so the
            // floor is at or before the block's start, and every candidate at or
            // above it is within the window of every position in the block. An
            // input shorter than the window has its floor at its start, which is
            // what the clamp says; earlier frames lie below that start.
            let floor_abs = if $borrowed {
                history_abs_start + concat_len.saturating_sub(advertised_window)
            } else {
                history_abs_start
            };
            debug_assert!(floor_abs <= $current_abs_start);
            // The same fold in slot space, so the window floor is one unsigned
            // compare against a slot value rather than a decode plus a compare
            // in position space. The empty sentinel is 0 and this is at least 1,
            // so it subsumes the emptiness test.
            let min_slot = ((floor_abs - position_base) as u32) + 1;
            // Upstream fuses "slot is populated" and "candidate is in window"
            // into one unsigned compare against `prefixLowestIndex`
            // (`zstd_double_fast.c:213`), which needs the window floor carried in
            // slot space. Measured here it costs more than it saves: the floor
            // then has to stay live in a register across a loop that already
            // spills 31 distinct slots, where the emptiness test against the zero
            // sentinel needed no register at all. It removed 0.75% of the
            // instructions and added 1.0-1.5% to the cycles, on a flat control
            // arm. The compare against the sentinel stays.
            debug_assert_eq!(
                block_len, $current_len,
                "fast loop expects the scanned block to end live history",
            );

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
            // they're skipped — upstream zstd accepts that exact tradeoff at
            // the iend boundary).
            if ip1 + HASH_READ_SIZE > $current_len {
                if let Some(committed) = $self.probe_tail_ip0_only::<$tier, $mls>(
                    $current_abs_start,
                    $current_len,
                    ip0,
                    literals_start,
                    $borrowed,
                ) {
                    let start = $self.emit_candidate::<$mls>(
                        $current_abs_start,
                        &mut literals_start,
                        committed,
                        $current_abs_start + ip0,
                        $handle_sequence,
                    );
                    pos = start + committed.match_len;
                    pos = $self.extend_with_repcode_after_match::<$tier>(
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

            // Pre-compute long hash at ip0 ONCE per outer iter.
            // `concat_idx = ($current_abs_start + ip0) - history_abs_start`
            // is the byte offset within `live_history`.
            // The long slot itself, not its index: the index would need the
            // table base back at every store, and both would stay live.
            let mut hl0_slot: *mut u32;
            // Tag of `hl0_idx`'s hash, carried alongside it (0 when untagged).
            let mut tl0: u32;
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
                let hl0_idx;
                (hl0_idx, tl0) =
                    dfast_slot_and_tag::<{ $tagged }>(v8.wrapping_mul(PRIME), long_key_shift);
                hl0_slot = long_hash_ptr.add(hl0_idx);
            }

            // Repcode 1 for this outer iteration, resolved once: upstream keeps
            // `offset_1` in a local and takes it out of reach at the block start
            // (zstd_double_fast.c:157-164), so its scan has no gate and tests
            // the rep only on an equal read. Here the offset is clamped to the
            // reach of the first cursor, which later cursors only exceed, so the
            // read needs no gate either; the hit below re-reads the real rep.
            let rep_floor = if $borrowed { floor_abs - history_abs_start } else { 0 };
            let rep_reach = ip1 + block_bias - rep_floor;
            // The candidate behind the cursor, addressed off the cursor the way
            // the block itself is. Wrapping: for a rep reaching before the block
            // this points before `block_ptr`.
            let rep_base =
                block_ptr.wrapping_sub(($self.offset_hist[0] as usize).clamp(1, rep_reach));

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
                // `u8` is a debug path tag (0=rep1, 1=long@ip0, 2=short/_search_next_long,
                // 3=dict-long, 4=dict-snl), surfaced by the `DFTRACE` env gate in the
                // commit handler to diagnose match-path / offset divergence vs C ffi.
                // The complementary insertion anchors on `ip0` at the break (upstream
                // `curr`, zstd_double_fast.c:184), NOT on the (rep1 `+1` / catch-up
                // adjusted) match start.
                Committed(MatchCandidate, u8),
                Tail(usize),
            }

            let inner_exit: InnerExit = 'inner: loop {
                // The probe at the cursor keeps a floor of its own position
                // when the input is borrowed, though the block's floor would do:
                // with the block's, the compiler lays the probe's gate out so
                // that the scan's branch misses double on the same branch stream
                // (4.4M -> 8.4M on 128 KiB of logs at level 3, cycles +3..5%),
                // and taking the floor per position here is 4% faster than the
                // build before the block floor. Any candidate it admits is within
                // the window. The retry and the repcode gate take the block's.
                // The input starts a window past every earlier frame (see
                // `set_borrowed_window`), so this floor excludes them unclamped:
                // `packed_curr` is at least `advertised_window + 1`, asserted
                // at the block's start.
                let packed_curr = (ip0 as u32) + packed_bias;
                let min_slot0 = if $borrowed {
                    packed_curr - advertised_window as u32
                } else {
                    min_slot
                };
                // Literal lengths are derived at the emit sites rather than
                // carried: both inputs are live there anyway, and the loop is
                // register-saturated, so two values held across every scanned
                // position for the benefit of the rare match path cost more
                // than the subtraction does.

                // Load 8 bytes at ip0 for both short (low 4) and long
                // probe equality checks. We already used `v8_at_ip0` to
                // compute hl0/idxl0 in the outer init / previous iter's
                // carry; reload now (cheap unaligned read) so the
                // `read_unaligned` is from the same offset the
                // probe-eq below will use.
                let v8_0 = unsafe {
                    (block_ptr.add(ip0) as *const u64)
                        .read_unaligned()
                };
                // `v4_0` (low 4 bytes) is the cheap 4-byte equality-gate key
                // below; the short HASH keys on the `$mls`-byte window (upstream
                // `ZSTD_hashPtr(ip, hBitsS, mls)`) to match `short_hash_index`.
                //
                // Carried from the load the hash already did, not re-read at the
                // comparison the way the reference reads `MEM_read32(ip)`. Doing
                // it the reference's way frees a register and measured 2.8%
                // worse in cycles: one AND off a value already in hand beats a
                // second load competing with the probe loads for the same ports.
                let v4_0 = v8_0 & 0xFFFF_FFFF;
                let (hs0_idx, ts0) = dfast_slot_and_tag::<{ $tagged }>(
                    short_key::<$mls>(v8_0).wrapping_mul(PRIME),
                    short_key_shift,
                );
                let idxs0 = unsafe { *short_hash_ptr.add(hs0_idx) };
                // Folded with its tag here, while the tag is in a register: the
                // probe sits past the long check, and carrying the tag there
                // takes a store and a reload at every position.
                let slot_s0 = (idxs0 ^ ts0).rotate_right(tag_shift);
                // The long slot is read here, a whole position after the prefetch
                // below issued it, rather than where the previous iteration
                // computed its address: carried across the position, the value
                // takes a store and a reload, and the prefetch has no time to
                // land. No long store comes between the two points.
                // SAFETY: `hl0_slot` is inside the long table.
                let idxl0 = unsafe { *hl0_slot };

                // Upstream zstd parity (`zstd_double_fast.c:187`): update BOTH
                // tables at curr BEFORE checking matches. The benefit is
                // for hash-table consumers, NOT the rep peek (rep at ip+1
                // reads `offset_hist[0]`, never the hash tables). The
                // upstream zstd rationale is the long-hash retry path: the next
                // inner iter's `idxl1` lookup (`hashLong[hl1_idx]`) can
                // collide with this iter's `hl0_idx`, and writing curr
                // first means a $self-collision still resolves to a real
                // match instead of the previous occupant. The short
                // probe of the same iter and the `_search_next_long`
                // retry at ip+1 are the other consumers that see the
                // fresh write.
                unsafe {
                    *hl0_slot = (packed_curr << tag_shift) | tl0;
                    *short_hash_ptr.add(hs0_idx) = (packed_curr << tag_shift) | ts0;
                }

                // Upstream zstd parity (`zstd_double_fast.c:190`): inline rep1
                // peek at ip+1, 4-byte gate. Upstream zstd's hot path checks ONLY
                // `offset_1` here (full 3-rep walk lives in lazy/btopt).
                // Since the peek is at `ip+1` with `pos >= literals_start`,
                // the literal length at `ip1` is >= 1, so `offset_hist[0]` is the upstream zstd's
                // `offset_1`. The `repcode_candidate_shared` helper we used
                // before walked all three offsets + did a full SIMD
                // `common_prefix_len` per probe, paying ~3× the work for
                // rep2/rep3 hits that the dfast fast path never benefits
                // from (those wins live in the lazy/btopt strategies).
                //
                // The candidate base is resolved per outer iteration above
                // (`rep_base`), so the scan carries one load for it.
                    // Window-low bound ONLY (upstream zstd `zstd_double_fast.c:190`
                    // gates the rep on nothing but `offset_1 > 0` and the 4-byte
                    // equality at `ip+1-offset_1`, i.e. the candidate is in the
                    // prefix). A prior extra `cand_pos_r >= literals_start` clause
                    // rejected essentially EVERY rep — after a match
                    // `literals_start ≈ ip`, so any back-reference (cand before
                    // the literal cursor) failed it — collapsing the rep path and
                    // forcing the long-hash to mint creeping offsets.
                    {
                        // Off the cursor through `rep_base`, as upstream reads
                        // `MEM_read32(ip+1-offset_1)`, but with the offset folded
                        // into the base: the address is the same unsigned
                        // `base + ip1` the block is read with, no signed offset.
                        // 4-byte gate; full forward count only if it passes.
                        // SAFETY: the clamp to the first cursor's reach keeps
                        // the candidate at or after the floor of live history
                        // for every later cursor, so the 4 bytes are inside it.
                        let cand4 = unsafe {
                            (rep_base.wrapping_add(ip1) as *const u32).read_unaligned()
                        };
                        let cur4 = unsafe {
                            (block_ptr.add(ip1) as *const u32)
                                .read_unaligned()
                        };
                        // The rep is read again here, on the equal read, rather
                        // than carried: a clamped offset read other bytes than
                        // the rep names, so the real candidate is compared too.
                        // Two nested tests: as one condition the compiler folds
                        // the rep's load into the scan beside the compare.
                        if cand4 == cur4 {
                        let rep = $self.offset_hist[0] as usize;
                        if rep != 0
                            && rep + rep_floor <= ip1 + block_bias
                            && unsafe {
                                (block_ptr.wrapping_add(ip1).wrapping_sub(rep) as *const u32)
                                    .read_unaligned()
                            } == cur4
                        {
                            let rep_cand_ptr = block_ptr.wrapping_add(ip1).wrapping_sub(rep);
                            let mut match_len = 4usize;
                            let max_fwd = block_len - (ip1 + 4);
                            unsafe {
                                let rhs =
                                    block_ptr.add(ip1 + 4);
                                let ext = $cpl(
                                    rep_cand_ptr.add(4), rhs, max_fwd,
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
                                // SAFETY: the rep gate puts the candidate in live
                                // history, before the cursor.
                                let rep_cand = unsafe {
                                    catch_up_in_block(
                                        rep_cand_ptr,
                                        live_ptr,
                                        block_ptr,
                                        ip1,
                                        literals_start,
                                        match_len,
                                    )
                                };
                                break 'inner InnerExit::Committed(rep_cand, 0);
                            }
                        }
                    }
                }

                // Precompute hl1 (upstream zstd `_search_next_long` carry, line 197).
                let v8_1 = unsafe {
                    (block_ptr.add(ip1) as *const u64)
                        .read_unaligned()
                };
                let (hl1_idx, tl1) =
                    dfast_slot_and_tag::<{ $tagged }>(v8_1.wrapping_mul(PRIME), long_key_shift);

                // Prefetch the RANDOM-ACCESS long slot the loop is about to
                // read, while there is work to hide the latency behind. The
                // hardware prefetcher cannot predict a hash-indexed address, and
                // this slot is loaded unconditionally at the next position.
                // Upstream instead prefetches the sequential input
                // (`PREFETCH_L1(ip1 + 64)`), which the hardware already covers,
                // and only inside its step-growth branch.
                //
                // The short slot is NOT prefetched. Its address needs a hash of
                // the same `v8_1` bytes that the next iteration hashes again as
                // its own `hs0_idx`, so warming it meant computing that hash
                // twice per position and throwing the first away.
                // SAFETY: `hl1_idx < 1 << long_hash_bits`, the long table's
                // length, so the prefetched address is inside it.
                let hl1_slot = unsafe { long_hash_ptr.add(hl1_idx) };
                crate::decoding::prefetch::prefetch_l1_at(hl1_slot as *const u8);

                // Long match check at ip0 with idxl0. 8-byte equality
                // gate (`MEM_read64`) — if it passes, candidate is real.
                //
                // Branchy validity rather than a branchless `in_long` mask: the
                // slot-populated / in-window / before-cursor checks are highly
                // predictable once the table is warm, so the branch predictor
                // speculates the hot 8-byte candidate load past them. A
                // branchless mask instead makes the load ADDRESS depend on the
                // mask (`cand_idx &= -(in_long)`), serialising the loop's single
                // hottest instruction (~13% self-time on z000033) behind the
                // mask — a stall the predictor cannot hide.
                //
                // Upstream disagrees and selects the ADDRESS instead
                // (`ZSTD_selectAddr`, `zstd_double_fast.c:203`): a rejected
                // candidate reads a stand-in, the compare runs unconditionally,
                // and its comment calls the gate "(somewhat) unpredictable".
                // Ported here — cmov between the candidate and `block_ptr`, so
                // not even a register spent on the stand-in — that shape measured
                // 2.0% worse in cycles across three interleaved alternations
                // (962-972 vs 979-994, no overlap). The gate is predictable in
                // OUR table, so branching skips the load rather than waiting on a
                // cmov to address it.
                // Both bounds, in slot space. The upper one is NOT redundant: a
                // slot can name a position PAST the cursor, because a reused
                // borrowed frame inherits the previous frame's table while its
                // scan descriptor reports the origin as zero again, so the
                // floor-advance that retires those slots on the owned path does
                // nothing here. A shorter following frame then finds slots
                // beyond its own input. `packed_curr` is the cursor in the same
                // space and is already in hand for the stores below.
                // Tag, floor and cursor are ONE unsigned compare: xoring the
                // expected tag clears the word's low bits only on a match, and
                // rotating them to the top makes any mismatch a value above
                // every position, so `slot - floor < cursor - floor` admits
                // exactly a populated, matching, in-window slot, and leaves the
                // slot itself in hand. Three short-circuit tests let the
                // compiler order them, and it tested the tag first: then a slot
                // an earlier frame wrote over the same bytes passed the tag and
                // fell to the floor, a branch the predictor missed on every
                // such slot (twice the misses of empty tables on a reused
                // context recompressing similar input).
                let slot_l0 = (idxl0 ^ tl0).rotate_right(tag_shift);
                if slot_l0.wrapping_sub(min_slot0) < packed_curr - min_slot0 {
                    let slot_l0 = slot_l0 as usize;
                    // SAFETY: the gates admit only slots naming a position at or
                    // after the window floor and before the cursor, so this
                    // lands inside live history — the same buffer and length
                    // bounds `v8_0` is read under.
                    let cand_ptr = slot_base_ptr.wrapping_add(slot_l0);
                    let cand_v8 = unsafe { (cand_ptr as *const u64).read_unaligned() };
                    if cand_v8 == v8_0 {
                        {
                            debug_assert!(
                                cand_ptr < block_ptr.wrapping_add(ip0),
                                "long candidate at or past the cursor {ip0}",
                            );
                            {
                            // 8 bytes match; count forward + extend back.
                            let mut match_len = 8usize;
                            let max_fwd = block_len - (ip0 + 8);
                            // SAFETY: both ptrs at the same buffer; offsets
                            // verified above. `max_fwd` caps the scan to
                            // the live region.
                            unsafe {
                                let rhs =
                                    block_ptr.add(ip0 +8);
                                let ext = $cpl(
                                    cand_ptr.add(8), rhs, max_fwd,
                                );
                                match_len += ext;
                            }
                            // SAFETY: the gates put the candidate in live
                            // history, before the cursor.
                            let cand = unsafe {
                                catch_up_in_block(
                                    cand_ptr,
                                    live_ptr,
                                    block_ptr,
                                    ip0,
                                    literals_start,
                                    match_len,
                                )
                            };
                            // Upstream zstd `_match_found` (zstd_double_fast.c:287):
                            // `if (step < 4) hashLong[hl1] = ip1`. Insert the
                            // lookahead position's long hash before storing the
                            // match — safe only while `step < 4` (then `ip1` is
                            // below the post-match cursor `ip + mLength`).
                            if step < 4 {
                                let packed_ip1 = (ip1 as u32) + packed_bias;
                                unsafe {
                                    *hl1_slot = (packed_ip1 << tag_shift) | tl1;
                                }
                            }
                            break 'inner InnerExit::Committed(cand, 1);
                        }
                    }
                    }
                }

                // Short match check at ip0 with idxs0 — 4-byte gate
                // ONLY (upstream zstd `zstd_double_fast.c:220`). Forward count
                // and `_search_next_long` retry happen ONLY on hit.
                // Branchy validity, same shape as the ip1 long retry below:
                // the slot-populated / in-window / before-cursor checks are
                // predictable after warmup, so the predictor speculates the
                // 4-byte candidate load past them. A branchless mask would tie
                // the load address to the mask, serialising it.
                // One compare for tag, floor and cursor, as the long probe.
                if slot_s0.wrapping_sub(min_slot0) < packed_curr - min_slot0 {
                    let slot_s0 = slot_s0 as usize;
                    // SAFETY: as in the long probe, the gates admit only slots
                    // naming a position at or after the floor and before the
                    // cursor.
                    let cand_ptr = slot_base_ptr.wrapping_add(slot_s0);
                    let cand4 = unsafe { (cand_ptr as *const u32).read_unaligned() };
                    if cand4 == v4_0 as u32 {
                        {
                            debug_assert!(
                                cand_ptr < block_ptr.wrapping_add(ip0),
                                "short candidate at or past the cursor {ip0}",
                            );
                            {
                            // Short hit: count forward from byte 4 onwards.
                            let mut s_match_len = 4usize;
                            let max_fwd = block_len - (ip0 + 4);
                            unsafe {
                                let rhs =
                                    block_ptr.add(ip0 +4);
                                let ext = $cpl(
                                    cand_ptr.add(4), rhs, max_fwd,
                                );
                                s_match_len += ext;
                            }
                            // SAFETY: the gates put the candidate in live
                            // history, before the cursor.
                            let short_cand = unsafe {
                                catch_up_in_block(
                                    cand_ptr,
                                    live_ptr,
                                    block_ptr,
                                    ip0,
                                    literals_start,
                                    s_match_len,
                                )
                            };

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
                            let short_hit_valid =
                                short_cand.match_len >= short_hit_floor::<$mls>();

                            // Upstream zstd `_search_next_long` retry (line 260):
                            // try long match at ip1 with precomputed idxl1.
                            // If it produces a strictly longer match, use it.
                            let mut chosen = short_cand;
                            let mut retry_upgraded = false;
                            // One compare for tag, floor and cursor, as the
                            // probes at ip0, against the block's floor.
                            let packed_ip1 = (ip1 as u32) + packed_bias;
                            // SAFETY: `hl1_slot` is inside the long table.
                            let idxl1 = unsafe { *hl1_slot };
                            let slot_l1 = (idxl1 ^ tl1).rotate_right(tag_shift);
                            if slot_l1.wrapping_sub(min_slot) < packed_ip1 - min_slot {
                                let cand_ptr_l1 = slot_base_ptr.wrapping_add(slot_l1 as usize);
                                debug_assert!(
                                    cand_ptr_l1 >= live_ptr
                                        && cand_ptr_l1 < block_ptr.wrapping_add(ip1)
                                );
                                {
                                    let cand_v8_l1 =
                                        unsafe { (cand_ptr_l1 as *const u64).read_unaligned() };
                                    if cand_v8_l1 == v8_1 {
                                        let mut l1_match_len = 8usize;
                                        let max_fwd_l1 = block_len - (ip1 + 8);
                                        unsafe {
                                            let rhs = block_ptr.add(ip1 + 8);
                                            let ext = $cpl(
                                                cand_ptr_l1.add(8), rhs, max_fwd_l1,
                                            );
                                            l1_match_len += ext;
                                        }
                                        if l1_match_len > short_cand.match_len {
                                            // SAFETY: the gate puts the candidate
                                            // in live history, before the cursor.
                                            chosen = unsafe {
                                                catch_up_in_block(
                                                    cand_ptr_l1,
                                                    live_ptr,
                                                    block_ptr,
                                                    ip1,
                                                    literals_start,
                                                    l1_match_len,
                                                )
                                            };
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
                                // Upstream zstd `_match_found` (zstd_double_fast.c:287):
                                // `if (step < 4) hashLong[hl1] = ip1`.
                                if step < 4 {
                                    let packed_ip1 = (ip1 as u32) + packed_bias;
                                    unsafe {
                                        *hl1_slot = (packed_ip1 << tag_shift) | tl1;
                                    }
                                }
                                break 'inner InnerExit::Committed(chosen, 2);
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
                }

                // Step bump on distance (upstream zstd `zstd_double_fast.c:224-228`).
                // Upstream grows the step unbounded (one per `kStepIncr` travelled);
                // no cap, so the scan stride matches byte-for-byte.
                //
                // A branch taken once per `kStepIncr` bytes, with upstream's
                // prefetch of the input ahead inside it. Without a body worth
                // branching over, the compiler selects the bump with a `cmov`
                // and a borrow on every scanned position instead.
                if ip1 >= next_step_pos {
                    // Prefetching never faults, so the address may run past the
                    // block's end.
                    crate::decoding::prefetch::prefetch_l1_at(block_ptr.wrapping_add(ip1 + 64));
                    crate::decoding::prefetch::prefetch_l1_at(block_ptr.wrapping_add(ip1 + 128));
                    step += 1;
                    next_step_pos += DFAST_SKIP_STEP_GROWTH_INTERVAL;
                }

                // Advance: ip0 = ip1; ip1 += step; carry hl1 → hl0 / idxl1 → idxl0.
                ip0 = ip1;
                ip1 += step;
                hl0_slot = hl1_slot;
                tl0 = tl1;
                // Against a precomputed limit, the way upstream compares
                // `ip1 <= ilimit` with `ilimit = iend - HASH_READ_SIZE`. Adding
                // the lookahead to the cursor instead spends the add on every
                // scanned position to reach the same answer.
                //
                // Worth 1.11% of the program's instructions. Cycles read 1.36%
                // higher, of which 0.68% is code layout — the same two binaries
                // differ by that much at a level where this line cannot run —
                // leaving a remainder inside the build-to-build drift measured
                // on this host. Kept for the op-reduction, not on a speed claim.
                if ip1 > scan_limit {
                    // First position the fast loop did NOT pack into the
                    // hash tables. `seed_remaining_hashable_starts` will
                    // pick up from `ip0` instead of restarting at the
                    // outer-entry `pos`.
                    break 'inner InnerExit::Tail(ip0);
                }
            };

            match inner_exit {
                InnerExit::Committed(candidate, _path_tag) => {
                    // `DFTRACE` env gate (`kernel-trace` builds): dump each
                    // committed match's path tag + (offset, match_len,
                    // literal_len) so the match-path / offset stream can be
                    // diffed against C ffi when chasing a dfast ratio divergence.
                    #[cfg(feature = "kernel-trace")]
                    if *DFTRACE_ENABLED.get_or_init(|| std::env::var_os("DFTRACE").is_some()) {
                        std::eprintln!(
                            "DFT path={} off={} ml={} ll={}",
                            _path_tag,
                            candidate.offset,
                            candidate.match_len,
                            candidate.start - literals_start,
                        );
                    }
                    // Stored, indexed and followed by the immediate repcodes
                    // right here, in the loop's own coordinates, as upstream
                    // does from `_match_found` on (zstd_double_fast.c:285-320):
                    // `ensure_room_for` covered the whole block on entry, so
                    // nothing on this path moves the tables or the bytes.
                    let start = candidate.start;
                    debug_assert!(start >= literals_start);
                    let literal_len = start - literals_start;
                    $handle_sequence(Sequence::Triple {
                        literal_len,
                        offset: candidate.offset,
                        match_len: candidate.match_len,
                    });
                    note_match_offset(&mut $self.offset_hist, candidate.offset as u32, literal_len);
                    pos = start + candidate.match_len;
                    literals_start = pos;
                    // Complementary insertion (zstd_double_fast.c:300-304):
                    // long at `curr + 2` and `ip - 2`, short at `curr + 2` and
                    // `ip - 1`, each where its key is readable, in that order.
                    let curr = ip0;
                    // `match_len >= 4`, so `pos >= 4` and the subtractions hold.
                    // All four positions sit at or before `pos`: with 8 bytes
                    // readable there every key is, and one test covers the four
                    // inserts, as upstream's single `ip <= ilimit` does
                    // (zstd_double_fast.c:297). The key at `curr + 2` is read
                    // once for both tables.
                    debug_assert!(curr + 2 <= pos - 2);
                    if pos + HASH_READ_SIZE <= block_len {
                        // SAFETY: `curr + 2 < pos - 1 < pos` and
                        // `pos + 8 <= block_len`, so each 8-byte read is inside
                        // the block; the indices are below their table's length.
                        unsafe {
                            let v2 = (block_ptr.add(curr + 2) as *const u64).read_unaligned();
                            let vl = (block_ptr.add(pos - 2) as *const u64).read_unaligned();
                            let vs = (block_ptr.add(pos - 1) as *const u64).read_unaligned();
                            let at2 = ((curr + 2) as u32) + packed_bias;
                            let (slot, tag) = dfast_slot_and_tag::<{ $tagged }>(
                                v2.wrapping_mul(PRIME),
                                long_key_shift,
                            );
                            *long_hash_ptr.add(slot) = live_slot_with_tag::<{ $tagged }>(at2, tag);
                            let (slot, tag) = dfast_slot_and_tag::<{ $tagged }>(
                                vl.wrapping_mul(PRIME),
                                long_key_shift,
                            );
                            *long_hash_ptr.add(slot) = live_slot_with_tag::<{ $tagged }>(
                                ((pos - 2) as u32) + packed_bias,
                                tag,
                            );
                            let (slot, tag) = dfast_slot_and_tag::<{ $tagged }>(
                                short_key::<$mls>(v2).wrapping_mul(PRIME),
                                short_key_shift,
                            );
                            *short_hash_ptr.add(slot) = live_slot_with_tag::<{ $tagged }>(at2, tag);
                            let (slot, tag) = dfast_slot_and_tag::<{ $tagged }>(
                                short_key::<$mls>(vs).wrapping_mul(PRIME),
                                short_key_shift,
                            );
                            *short_hash_ptr.add(slot) = live_slot_with_tag::<{ $tagged }>(
                                ((pos - 1) as u32) + packed_bias,
                                tag,
                            );
                        }
                    // Immediate repcodes at `offset_2` (zstd_double_fast.c:306-320),
                    // under the same `ip <= ilimit` as the inserts and each other.
                    while pos + HASH_READ_SIZE <= block_len {
                        let rep = $self.offset_hist[1] as usize;
                        // Concat index of the cursor: a candidate before the
                        // live history is out of reach.
                        let idx = pos + block_bias;
                        if rep == 0 || rep > idx {
                            break;
                        }
                        // SAFETY: `idx - rep < idx` and `pos + 4 <= block_len`,
                        // both inside the live bytes.
                        let rep_ptr = block_ptr.wrapping_add(pos).wrapping_sub(rep);
                        let (c4, p4) = unsafe {
                            (
                                (rep_ptr as *const u32).read_unaligned(),
                                (block_ptr.add(pos) as *const u32).read_unaligned(),
                            )
                        };
                        if c4 != p4 {
                            break;
                        }
                        // SAFETY: as above; the length caps the count to the
                        // block.
                        let r_len = 4 + unsafe {
                            $cpl(
                                rep_ptr.add(4),
                                block_ptr.add(pos + 4),
                                block_len - (pos + 4),
                            )
                        };
                        // Both tables at the rep position (zstd_double_fast.c:314-315),
                        // short first as the single-position insert writes them.
                        let packed = (pos as u32) + packed_bias;
                        // SAFETY: `pos + 8 <= block_len` holds both keys; the
                        // indices are below their table's length.
                        unsafe {
                            let v = (block_ptr.add(pos) as *const u64).read_unaligned();
                            let (slot, tag) = dfast_slot_and_tag::<{ $tagged }>(
                                short_key::<$mls>(v).wrapping_mul(PRIME),
                                short_key_shift,
                            );
                            *short_hash_ptr.add(slot) =
                                live_slot_with_tag::<{ $tagged }>(packed, tag);
                            let (slot, tag) = dfast_slot_and_tag::<{ $tagged }>(
                                v.wrapping_mul(PRIME),
                                long_key_shift,
                            );
                            *long_hash_ptr.add(slot) =
                                live_slot_with_tag::<{ $tagged }>(packed, tag);
                        }
                        $handle_sequence(Sequence::Triple {
                            literal_len: 0,
                            offset: rep,
                            match_len: r_len,
                        });
                        // A repeat at `offset_2` with no literals swaps the two
                        // (zstd_double_fast.c:313).
                        $self.offset_hist.swap(0, 1);
                        pos += r_len;
                        literals_start = pos;
                    }
                    }
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
        $self.emit_trailing_literals($current_abs_start, literals_start, $handle_sequence);
    }};
}

/// The dictionary scan loop, in the shape upstream gives its dictionary
/// variant (`ZSTD_compressBlock_doubleFast_dictMatchState_generic`,
/// zstd_double_fast.c:328-545) rather than the shape of its no-dictionary one.
///
/// Upstream keeps the two apart, and the difference is not decoration. The
/// no-dictionary loop carries TWO cursors: it scans at `ip0`, precomputes the
/// long hash of `ip1` so the `_search_next_long` retry and the next iteration
/// both find it ready, and carries `hl1`/`idxl1` across iterations. That trade
/// buys a hash per two positions at the price of keeping the second cursor's
/// index, slot, position and window bound live the whole way round. The
/// dictionary loop cannot afford it: it must also keep two table pointers, the
/// dictionary's own two hash shifts and its region bound live, and the machine
/// has no registers left. Bolting the dictionary probes onto the two-cursor
/// loop is what this replaces; the profile of that arrangement had the loop
/// reloading its own invariants from the stack at every probe.
///
/// So this scans one cursor, hashes each position once, and steps the way the
/// reference's dictionary loop steps — `ip += ((ip - anchor) >> 8) + 1`,
/// accelerating with the distance from the last match rather than from the
/// block start. The probe ORDER is the reference's too, and one part of it
/// matters for more than register pressure: the dictionary's short table is
/// consulted only when the live short slot is EMPTY or out of window
/// (zstd_double_fast.c:437-449, an `else if` on the slot, not on the compare).
/// With the tables a small dictionary sizes, every slot is occupied within a
/// few hundred positions, so that arm all but stops firing — which is the work
/// the two-cursor arrangement was doing on every single position.
///
/// `$borrowed` picks where the input lies. Owned, the dictionary and the input
/// are one run of the history. Borrowed, the input is the caller's slice, read
/// where it lies as upstream's one-shot frame reads it, and the dictionary
/// stays at the front of the history: a candidate in the input reads the slice
/// alone, and one in the dictionary is counted on into the input across the
/// two buffers (upstream `ZSTD_count_2segments`). Both number the input the
/// same way, so the two emit the same sequences.
///
/// What the shape is worth, on 20 000 dictionary frames of a 10 KiB random
/// fixture (i9, three interleaved rounds of prebuilt binaries, with the same
/// harness minus the dictionary as the control arm): 8161 / 8125 / 8412 ->
/// 6594 / 6574 / 6626 M cycles and 11,980 -> 9,292 M instructions, against
/// libzstd's 5035 / 5044 / 5033 M and 9,750 M measured in the same runs. The
/// gap to it goes 1.62x -> 1.31x in cycles, and in instructions we now run
/// slightly FEWER than it does — what is left there is execution density, not
/// work.
macro_rules! start_matching_dict_loop_body {
    ($self:ident, $current_abs_start:ident, $current_len:ident, $handle_sequence:ident, $cpl:path, $tier:ty, $borrowed:ident, $mls:ident) => {{
        debug_assert!($current_len > 0, "dict_loop precondition: $current_len > 0");
        $self.ensure_room_for($current_abs_start + $current_len - 1);
        const PRIME: u64 = 0xCF1BBCDCB7A56463_u64;
        let short_shift = 64 - $self.short_hash_bits;
        let long_shift = 64 - $self.long_hash_bits;
        let mut pos = 1usize;
        let mut literals_start = 0usize;

        // The immutable dictionary tables, snapshotted once: nothing mutates
        // them while matching, so unlike the live tables they need no
        // re-snapshot per outer iteration.
        let (dict_long_ptr, dict_short_ptr, dict_end, dict_long_shift, dict_short_shift) = {
            let d = $self
                .dict
                .table()
                .expect("dict kernel dispatched without a dict table");
            (
                d.long.as_ptr(),
                d.short.as_ptr(),
                $self.dict.region_len(),
                64 - d.long_bits,
                64 - d.short_bits,
            )
        };

        // The block's cursors, read once: a match is stored, indexed and
        // followed by its repcodes inside this loop, so nothing between two
        // scans moves the tables or the bytes (upstream resolves the same
        // bases once above its loop, zstd_double_fast.c:338-361). A block too
        // short for one key has nothing to scan, and its limits do not exist.
        if pos + HASH_READ_SIZE <= $current_len {
            let (
                history_base_ptr,
                history_start_offset,
                history_abs_start,
                position_base,
                concat_len,
            ) = $self.owned_scan_descriptor();
            let short_hash_ptr = $self.short_mut_ptr();
            let long_hash_ptr = $self.long_mut_ptr();
            // The window's first byte: the dictionary's, at `history_abs_start`.
            // SAFETY: the history start is in bounds of the buffer.
            let window_ptr = unsafe { history_base_ptr.add(history_start_offset) };
            // Where the bytes of a live position lie: `live_ptr` holds the
            // position `live_abs`. Owned, that is the whole window; borrowed,
            // the input slice, which follows the dictionary.
            let (live_ptr, live_abs, live_len) = if $borrowed {
                let (input_ptr, block_end) = $self.borrowed_scan_descriptor();
                (input_ptr, $self.borrowed_abs_base, block_end)
            } else {
                (window_ptr, history_abs_start, concat_len)
            };
            let block_bias = $current_abs_start - live_abs;
            // SAFETY: the block is part of the live bytes, so its first byte is
            // in bounds of their buffer.
            let block_ptr = unsafe { live_ptr.add(block_bias) };
            let block_len = live_len - block_bias;
            debug_assert_eq!(block_len, $current_len);
            // The two sides of a candidate in the dictionary, scanned in place.
            // SAFETY: the dictionary is the committed history and the input
            // runs to the block's end; nothing writes either while matching.
            let (dict_bytes, input_bytes): (&[u8], &[u8]) = unsafe {
                (
                    core::slice::from_raw_parts(window_ptr, if $borrowed { dict_end } else { 0 }),
                    core::slice::from_raw_parts(live_ptr, if $borrowed { live_len } else { 0 }),
                )
            };
            let scan_limit = $current_len - HASH_READ_SIZE;
            let packed_bias = (($current_abs_start - position_base) as u32) + 1;
            // Slot payload to candidate bytes in one add, as in the two-cursor
            // kernel: see the note there for why the four coordinate constants
            // are folded into a pointer rather than kept live.
            let slot_base_ptr =
                live_ptr.wrapping_offset(position_base as isize - live_abs as isize - 1);
            // Every live slot of this frame names a position of its input, so
            // the floor is where the live bytes begin; owned, no slot names the
            // dictionary in front of the input either (`live_floor`).
            let min_slot = ((live_abs - position_base) as u32) + 1;
            // Backward extension of a candidate at `cand` (absolute) matching
            // the cursor at `cur` for `len` bytes, back to at most `lit` bytes
            // before the cursor and never before the dictionary.
            let extend_back = move |cand: usize, cur: usize, len: usize, lit: usize| {
                if $borrowed {
                    extend_backwards_split(
                        window_ptr,
                        history_abs_start,
                        live_ptr,
                        live_abs,
                        cand,
                        cur,
                        len,
                        lit,
                    )
                } else {
                    // SAFETY: the window and its readable length come from
                    // `owned_scan_descriptor()` above.
                    let concat = unsafe { core::slice::from_raw_parts(window_ptr, concat_len) };
                    extend_backwards_shared(concat, history_abs_start, cand, cur, len, lit)
                }
            };
            // Length of a match against the dictionary byte `dp` at block
            // offset `at`, `known` bytes of it already compared equal.
            let dict_match_len = move |dp: usize, at: usize, known: usize| -> usize {
                if $borrowed {
                    count_forward_dict_2segment(dict_bytes, dp, input_bytes, block_bias + at, known)
                } else {
                    // SAFETY: the dictionary sits right before the input in the
                    // window, so the count crosses into it like any in-window
                    // match; `block_len - (at + known)` caps it.
                    known
                        + unsafe {
                            $cpl(
                                window_ptr.add(dp + known),
                                block_ptr.add(at + known),
                                block_len - (at + known),
                            )
                        }
                }
            };

            // Absolute position to byte, one base per segment, for the rep
            // gate's select.
            let live_rebase = live_ptr.wrapping_sub(live_abs);
            let dict_rebase = window_ptr.wrapping_sub(history_abs_start);
            // These repeat offsets only steer the search: the emitter codes
            // each match from its own history. Every one is brought within the
            // window in front of the block, so a candidate `ip + 1 - rep`
            // never lands before the dictionary's start and the scan needs no
            // test for it, as upstream's needs none (it asserts the same,
            // zstd_double_fast.c:380, its loader refusing a dictionary with a
            // farther offset). Only a raw dictionary shorter than the default
            // offsets, or one built by hand, carries such an offset; a match
            // committed from here on is never farther back than the window.
            let reach = ($current_abs_start - history_abs_start + 1) as u32;
            // The offsets live in a local for the block, as upstream keeps
            // `offset_1` and `offset_2` (zstd_double_fast.c:348): read and
            // rotated through the matcher, every match paid a load and a store
            // of each. They go back to the matcher when the block ends.
            let mut reps = $self.offset_hist.map(|rep| rep.clamp(1, reach));
            'outer: loop {
                if pos + HASH_READ_SIZE > $current_len {
                    break 'outer;
                }
                let rep1 = reps[0] as usize;

                let mut ip = pos;
                let inner_exit: DfastInnerExit = 'inner: loop {
                    let abs_ip = $current_abs_start + ip;
                    let packed_curr = (ip as u32) + packed_bias;
                    // SAFETY: the loop guard keeps `ip + 8 <= block_len`.
                    let v8 = unsafe { (block_ptr.add(ip) as *const u64).read_unaligned() };
                    let v4 = v8 as u32;
                    let hl_idx = dfast_slot(v8.wrapping_mul(PRIME), long_shift);
                    // The short key over the frame's `$mls` bytes (upstream
                    // `ZSTD_hashPtr(ip, hBitsS, mls)`).
                    let hs_idx = dfast_slot(short_key::<$mls>(v8).wrapping_mul(PRIME), short_shift);
                    // SAFETY: both indices are below their table's length.
                    let (idxl, idxs) =
                        unsafe { (*long_hash_ptr.add(hl_idx), *short_hash_ptr.add(hs_idx)) };
                    // Both tables updated at the cursor BEFORE the checks, as the
                    // reference does (zstd_double_fast.c:404): a self-collision on
                    // the `+1` long retry then resolves to a real match rather than
                    // to the slot's previous occupant.
                    // SAFETY: as above.
                    unsafe {
                        *long_hash_ptr.add(hl_idx) = packed_curr;
                        *short_hash_ptr.add(hs_idx) = packed_curr;
                    }

                    // Repcode at ip+1, 4-byte gate (zstd_double_fast.c:407-415).
                    // `ip + 8 <= block_len` from the loop guard covers the read.
                    // The candidate may lie anywhere from the dictionary's start.
                    // Borrowed, the candidate lies in the dictionary or in the input:
                    // its bytes are picked by a select, a candidate whose 4 bytes
                    // straddle the seam is rejected, and only a 4-byte hit pays for
                    // a count (upstream zstd (zstd_double_fast.c:400-410)).
                    {
                        debug_assert!(rep1 != 0 && rep1 <= abs_ip + 1 - history_abs_start);
                        let cand_r = abs_ip + 1 - rep1;
                        let in_live = !$borrowed || cand_r >= live_abs;
                        if dfast_branch!(1, !$borrowed || (live_abs - 1).wrapping_sub(cand_r) >= 3)
                        {
                            let rep_base = if in_live { live_rebase } else { dict_rebase };
                            // SAFETY: `cand_r` is at or after the dictionary's start
                            // and before the cursor; off the seam, its 4 bytes lie
                            // whole in the segment `rep_base` numbers. The cursor's
                            // read is inside the block.
                            let (cand4, cur4) = unsafe {
                                (
                                    (rep_base.wrapping_add(cand_r) as *const u32).read_unaligned(),
                                    (block_ptr.add(ip + 1) as *const u32).read_unaligned(),
                                )
                            };
                            if dfast_branch!(2, cand4 == cur4) {
                                let match_len = if in_live {
                                    // SAFETY: same buffer; the length caps the scan
                                    // to the block.
                                    4 + unsafe {
                                        $cpl(
                                            live_ptr.add(cand_r - live_abs + 4),
                                            block_ptr.add(ip + 1 + 4),
                                            block_len - (ip + 1 + 4),
                                        )
                                    }
                                } else {
                                    dict_match_len(cand_r - history_abs_start, ip + 1, 4)
                                };
                                // Rep coding mints no offset, so the reference
                                // accepts a 4-byte hit here where a hash match
                                // needs 5.
                                if dfast_branch!(3, match_len >= DFAST_REP_MIN_MATCH_LEN) {
                                    let cand = extend_back(
                                        cand_r,
                                        abs_ip + 1,
                                        match_len,
                                        ip + 1 - literals_start,
                                    );
                                    break 'inner DfastInnerExit::Committed(cand, 0, abs_ip);
                                }
                            }
                        }
                    }

                    // Long match at the cursor: live table first, dictionary only
                    // if that missed (zstd_double_fast.c:417-433, `else if
                    // dictTagsMatchL`).
                    // Floor and cursor in one unsigned compare, as in the
                    // no-dictionary loop: with two, the compiler tests the floor
                    // first, and slots a previous frame left below it make that
                    // branch mispredict.
                    if dfast_branch!(4, idxl.wrapping_sub(min_slot) < packed_curr - min_slot) {
                        // SAFETY: the gates admit only slots naming a position at
                        // or after the window floor and before the cursor, so this
                        // lands inside live history.
                        let cand_v8 = unsafe {
                            (slot_base_ptr.wrapping_add(idxl as usize) as *const u64)
                                .read_unaligned()
                        };
                        if dfast_branch!(5, cand_v8 == v8) {
                            let cand_abs = position_base + (idxl as usize) - 1;
                            // SAFETY: the slot names a live position before the
                            // cursor; the length caps the scan to the block.
                            let match_len = 8 + unsafe {
                                $cpl(
                                    live_ptr.add(cand_abs - live_abs + 8),
                                    block_ptr.add(ip + 8),
                                    block_len - (ip + 8),
                                )
                            };
                            let cand =
                                extend_back(cand_abs, abs_ip, match_len, ip - literals_start);
                            break 'inner DfastInnerExit::Committed(cand, 1, abs_ip);
                        }
                    }
                    {
                        let dmix = v8.wrapping_mul(PRIME);
                        // SAFETY: the index is below the dict long table's length.
                        let dl = unsafe { *dict_long_ptr.add((dmix >> dict_long_shift) as usize) };
                        // The tag rejects a colliding slot without touching the
                        // dictionary bytes (upstream `ZSTD_comparePackedTags`). An
                        // empty slot whose tag happens to agree decodes to a
                        // position past every byte and fails the bound below, so
                        // no emptiness test is spent on it, as upstream spends none.
                        if dfast_branch!(
                            6,
                            (dl & DFAST_TAG_MASK) == dfast_tag(dmix, dict_long_shift)
                        ) {
                            let dp = ((dl >> DFAST_TAG_BITS) as usize).wrapping_sub(1);
                            if dfast_branch!(7, dp < dict_end) {
                                debug_assert!(dp + HASH_READ_SIZE <= dict_end);
                                // SAFETY: dict long slots were written only for
                                // positions with 8 bytes of lookahead inside the
                                // dictionary, so `dp + 8 <= dict_end`.
                                let dcand_v8 =
                                    unsafe { (window_ptr.add(dp) as *const u64).read_unaligned() };
                                if dfast_branch!(8, dcand_v8 == v8) {
                                    let match_len = dict_match_len(dp, ip, 8);
                                    let cand = extend_back(
                                        history_abs_start + dp,
                                        abs_ip,
                                        match_len,
                                        ip - literals_start,
                                    );
                                    break 'inner DfastInnerExit::Committed(cand, 3, abs_ip);
                                }
                            }
                        }
                    }

                    // Short match at the cursor. The dictionary's short table is
                    // consulted only when the live slot is EMPTY or out of window
                    // — the reference branches on the slot, not on the compare
                    // (zstd_double_fast.c:437-449) — so with a warm live table this
                    // arm all but stops firing.
                    // The candidate's absolute position, and whether it lies in
                    // the dictionary.
                    let short_cand_abs: usize;
                    let short_in_dict: bool;
                    if dfast_branch!(9, idxs.wrapping_sub(min_slot) < packed_curr - min_slot) {
                        // SAFETY: as for the long slot above.
                        let cand4 = unsafe {
                            (slot_base_ptr.wrapping_add(idxs as usize) as *const u32)
                                .read_unaligned()
                        };
                        if dfast_branch!(10, cand4 != v4) {
                            ip += ((ip - literals_start) >> DFAST_SKIP_STEP_SHIFT) + 1;
                            if dfast_branch!(24, ip > scan_limit) {
                                break 'inner DfastInnerExit::Tail(ip);
                            }
                            continue 'inner;
                        }
                        short_cand_abs = position_base + (idxs as usize) - 1;
                        short_in_dict = false;
                    } else {
                        let dsmix = short_key::<$mls>(v8).wrapping_mul(PRIME);
                        // SAFETY: the index is below the dict short table's length.
                        let ds =
                            unsafe { *dict_short_ptr.add((dsmix >> dict_short_shift) as usize) };
                        let mut found = usize::MAX;
                        // An empty slot fails the bound, as for the long probe.
                        if dfast_branch!(
                            11,
                            (ds & DFAST_TAG_MASK) == dfast_tag(dsmix, dict_short_shift)
                        ) {
                            let dp = ((ds >> DFAST_TAG_BITS) as usize).wrapping_sub(1);
                            if dfast_branch!(12, dp < dict_end) {
                                debug_assert!(dp + 4 <= dict_end);
                                // SAFETY: dict short slots were written only for
                                // positions with 4 bytes of lookahead inside the
                                // dictionary.
                                let dcand4 =
                                    unsafe { (window_ptr.add(dp) as *const u32).read_unaligned() };
                                if dfast_branch!(13, dcand4 == v4) {
                                    found = dp;
                                }
                            }
                        }
                        if dfast_branch!(14, found == usize::MAX) {
                            ip += ((ip - literals_start) >> DFAST_SKIP_STEP_SHIFT) + 1;
                            if dfast_branch!(24, ip > scan_limit) {
                                break 'inner DfastInnerExit::Tail(ip);
                            }
                            continue 'inner;
                        }
                        short_cand_abs = history_abs_start + found;
                        short_in_dict = true;
                    }

                    // `_search_next_long` (zstd_double_fast.c:453-483): a short hit
                    // is held while the long tables are asked about `ip+1`, and a
                    // strictly longer answer there wins. Guarded on the lookahead
                    // the `+1` probe needs; the reference gets the same guard from
                    // its strict `ip < ilimit`.
                    let s_match_len = if $borrowed && short_in_dict {
                        dict_match_len(short_cand_abs - history_abs_start, ip, 4)
                    } else {
                        // SAFETY: owned, the dictionary and the input are one run
                        // of live bytes; the length caps the scan to the block.
                        4 + unsafe {
                            $cpl(
                                live_ptr.add(short_cand_abs - live_abs + 4),
                                block_ptr.add(ip + 4),
                                block_len - (ip + 4),
                            )
                        }
                    };
                    let short_cand =
                        extend_back(short_cand_abs, abs_ip, s_match_len, ip - literals_start);
                    // A bare 4-byte hash hit mints a wire offset that costs more
                    // than the four bytes buy, so it is only taken from 5 up; the
                    // `+1` upgrade below starts at 8 and is always above the floor.
                    // Upstream takes it from 4. On the tiers whose `minMatch` is 4
                    // that floor gave the same cycles and lost ratio: a 4 KiB log
                    // under a trained dictionary at level 3 came out 1693 bytes
                    // against 1677, a 128 KiB log at level 4 39075 against 37340.
                    let mut chosen = short_cand;
                    let mut upgraded = false;
                    if dfast_branch!(15, ip + 1 + HASH_READ_SIZE <= block_len) {
                        // SAFETY: guarded directly above.
                        let v8_1 =
                            unsafe { (block_ptr.add(ip + 1) as *const u64).read_unaligned() };
                        let hl1_idx = dfast_slot(v8_1.wrapping_mul(PRIME), long_shift);
                        // SAFETY: the index is below the long table's length.
                        let idxl1 = unsafe { *long_hash_ptr.add(hl1_idx) };
                        let packed_next = packed_curr + 1;
                        // The probed position is indexed as upstream indexes it
                        // (`hashLong[hl3] = curr + 1`, zstd_double_fast.c:459):
                        // nothing else writes it, since the complementary
                        // insertion after a match covers `curr + 2` and the two
                        // positions before the match end.
                        // SAFETY: as for the read above.
                        unsafe { *long_hash_ptr.add(hl1_idx) = packed_next };
                        let mut live_hit = false;
                        if dfast_branch!(16, idxl1.wrapping_sub(min_slot) < packed_next - min_slot)
                        {
                            // SAFETY: as for the long slot above.
                            let cand_v8 = unsafe {
                                (slot_base_ptr.wrapping_add(idxl1 as usize) as *const u64)
                                    .read_unaligned()
                            };
                            if dfast_branch!(17, cand_v8 == v8_1) {
                                live_hit = true;
                                let cand_abs = position_base + (idxl1 as usize) - 1;
                                // SAFETY: as for the long slot above.
                                let l1_len = 8 + unsafe {
                                    $cpl(
                                        live_ptr.add(cand_abs - live_abs + 8),
                                        block_ptr.add(ip + 1 + 8),
                                        block_len - (ip + 1 + 8),
                                    )
                                };
                                if dfast_branch!(18, l1_len > short_cand.match_len) {
                                    chosen = extend_back(
                                        cand_abs,
                                        abs_ip + 1,
                                        l1_len,
                                        ip + 1 - literals_start,
                                    );
                                    upgraded = true;
                                }
                            }
                        }
                        if dfast_branch!(19, !live_hit) {
                            let dmix1 = v8_1.wrapping_mul(PRIME);
                            // SAFETY: the index is below the dict long table's length.
                            let dl1 =
                                unsafe { *dict_long_ptr.add((dmix1 >> dict_long_shift) as usize) };
                            if dfast_branch!(
                                20,
                                (dl1 & DFAST_TAG_MASK) == dfast_tag(dmix1, dict_long_shift)
                            ) {
                                let dp1 = ((dl1 >> DFAST_TAG_BITS) as usize).wrapping_sub(1);
                                if dp1 < dict_end {
                                    debug_assert!(dp1 + HASH_READ_SIZE <= dict_end);
                                    // SAFETY: as for the dict long probe above.
                                    let dcand_v8 = unsafe {
                                        (window_ptr.add(dp1) as *const u64).read_unaligned()
                                    };
                                    if dfast_branch!(21, dcand_v8 == v8_1) {
                                        let dl1_len = dict_match_len(dp1, ip + 1, 8);
                                        if dfast_branch!(22, dl1_len > short_cand.match_len) {
                                            chosen = extend_back(
                                                history_abs_start + dp1,
                                                abs_ip + 1,
                                                dl1_len,
                                                ip + 1 - literals_start,
                                            );
                                            upgraded = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if dfast_branch!(
                        23,
                        upgraded || short_cand.match_len >= short_hit_floor::<$mls>()
                    ) {
                        break 'inner DfastInnerExit::Committed(chosen, 2, abs_ip);
                    }
                    // A below-floor short hit with no upgrade: keep scanning.
                    ip += ((ip - literals_start) >> DFAST_SKIP_STEP_SHIFT) + 1;
                    if dfast_branch!(24, ip > scan_limit) {
                        break 'inner DfastInnerExit::Tail(ip);
                    }
                };

                match inner_exit {
                    DfastInnerExit::Committed(candidate, _path_tag, scan_pos) => {
                        #[cfg(feature = "kernel-trace")]
                        if *DFTRACE_ENABLED.get_or_init(|| std::env::var_os("DFTRACE").is_some()) {
                            std::eprintln!(
                                "DFT path={} off={} ml={} ll={}",
                                _path_tag,
                                candidate.offset,
                                candidate.match_len,
                                candidate.start - $current_abs_start - literals_start,
                            );
                        }
                        // The match is stored, indexed and followed by the
                        // immediate repcodes right here, in the loop's own
                        // coordinates, as upstream does from `_match_found` on
                        // (zstd_double_fast.c:496-538): the block's rebase was
                        // settled on entry, so nothing on this path can move the
                        // tables or the bytes.
                        let start = candidate.start - $current_abs_start;
                        debug_assert!(start >= literals_start);
                        let literal_len = start - literals_start;
                        $handle_sequence(Sequence::Triple {
                            literal_len,
                            offset: candidate.offset,
                            match_len: candidate.match_len,
                        });
                        note_match_offset(&mut reps, candidate.offset as u32, literal_len);
                        pos = start + candidate.match_len;
                        literals_start = pos;
                        // Complementary insertion (zstd_double_fast.c:510-515):
                        // long at `curr + 2` and `ip - 2`, short at `curr + 2` and
                        // `ip - 1`, each where its key is readable, in that order.
                        let curr = scan_pos - $current_abs_start;
                        // A match ends at least 4 bytes past the position that
                        // found it, so all four positions sit at or before
                        // `pos`: with 8 bytes readable there, every key is, and
                        // one test covers the four inserts, as upstream's single
                        // `ip <= ilimit` does (zstd_double_fast.c:507). The key
                        // at `curr + 2` is read once for both tables.
                        debug_assert!(curr + 2 <= pos - 2);
                        if pos + HASH_READ_SIZE <= block_len {
                            // SAFETY: `curr + 2 < pos - 1 < pos` and
                            // `pos + 8 <= block_len`, so each 8-byte read is
                            // inside the block; the indices are below their
                            // table's length.
                            unsafe {
                                let v2 = (block_ptr.add(curr + 2) as *const u64).read_unaligned();
                                let vl = (block_ptr.add(pos - 2) as *const u64).read_unaligned();
                                let vs = (block_ptr.add(pos - 1) as *const u64).read_unaligned();
                                let at2 = ((curr + 2) as u32) + packed_bias;
                                *long_hash_ptr
                                    .add(dfast_slot(v2.wrapping_mul(PRIME), long_shift)) = at2;
                                *long_hash_ptr
                                    .add(dfast_slot(vl.wrapping_mul(PRIME), long_shift)) =
                                    ((pos - 2) as u32) + packed_bias;
                                *short_hash_ptr.add(dfast_slot(
                                    short_key::<$mls>(v2).wrapping_mul(PRIME),
                                    short_shift,
                                )) = at2;
                                *short_hash_ptr.add(dfast_slot(
                                    short_key::<$mls>(vs).wrapping_mul(PRIME),
                                    short_shift,
                                )) = ((pos - 1) as u32) + packed_bias;
                            }
                            // Immediate repcodes at `offset_2` (zstd_double_fast.c:518-537),
                            // reaching into the dictionary as upstream's does, under
                            // the same `ip <= ilimit` as the inserts and each other.
                            while pos + HASH_READ_SIZE <= block_len {
                                let rep = reps[1] as usize;
                                let abs_pos = $current_abs_start + pos;
                                if rep == 0 || rep > abs_pos - history_abs_start {
                                    break;
                                }
                                let cand = abs_pos - rep;
                                let r_len = if !$borrowed || cand >= live_abs {
                                    // SAFETY: `live_abs <= cand < abs_pos`, both inside
                                    // the live bytes with 4 readable at the cursor.
                                    let (c4, p4) = unsafe {
                                        (
                                            (live_ptr.add(cand - live_abs) as *const u32)
                                                .read_unaligned(),
                                            (block_ptr.add(pos) as *const u32).read_unaligned(),
                                        )
                                    };
                                    if c4 != p4 {
                                        break;
                                    }
                                    // SAFETY: as above; the length caps the count to
                                    // the block.
                                    4 + unsafe {
                                        $cpl(
                                            live_ptr.add(cand - live_abs + 4),
                                            block_ptr.add(pos + 4),
                                            block_len - (pos + 4),
                                        )
                                    }
                                } else {
                                    dict_match_len(cand - history_abs_start, pos, 0)
                                };
                                if r_len < DFAST_REP_MIN_MATCH_LEN {
                                    break;
                                }
                                // Both tables at the rep position (zstd_double_fast.c:530-531),
                                // short first as the single-position insert writes them.
                                // SAFETY: `pos + 8 <= block_len` holds both keys; the
                                // indices are below their table's length.
                                unsafe {
                                    let v = (block_ptr.add(pos) as *const u64).read_unaligned();
                                    *short_hash_ptr.add(dfast_slot(
                                        short_key::<$mls>(v).wrapping_mul(PRIME),
                                        short_shift,
                                    )) = (pos as u32) + packed_bias;
                                    *long_hash_ptr
                                        .add(dfast_slot(v.wrapping_mul(PRIME), long_shift)) =
                                        (pos as u32) + packed_bias;
                                }
                                $handle_sequence(Sequence::Triple {
                                    literal_len: 0,
                                    offset: rep,
                                    match_len: r_len,
                                });
                                // A repeat at `offset_2` with no literals swaps the
                                // two (zstd_double_fast.c:528).
                                reps.swap(0, 1);
                                pos += r_len;
                                literals_start = pos;
                            }
                        }
                    }
                    DfastInnerExit::Tail(seed) => {
                        pos = seed;
                        break 'outer;
                    }
                }
            }
            $self.offset_hist = reps;
        }

        $self.seed_remaining_hashable_starts($current_abs_start, $current_len, pos);
        $self.emit_trailing_literals($current_abs_start, literals_start, $handle_sequence);
    }};
}

/// How the dictionary scan loop left its inner loop: with a match to emit
/// (candidate, a path tag the `DFTRACE` gate prints, and the SCAN position the
/// complementary insertion anchors on), or out of scan room at the first
/// position it did not pack into the tables.
enum DfastInnerExit {
    Committed(MatchCandidate, u8, usize),
    Tail(usize),
}

/// [`extend_backwards_shared`] over a window held in two buffers: the
/// dictionary from `dict_abs` and the input scanned in place from `input_abs`,
/// which follows it. The cursor side is always in the input.
#[inline]
#[allow(clippy::too_many_arguments)]
fn extend_backwards_split(
    dict: *const u8,
    dict_abs: usize,
    input: *const u8,
    input_abs: usize,
    mut candidate_pos: usize,
    mut abs_pos: usize,
    mut match_len: usize,
    lit_len: usize,
) -> MatchCandidate {
    let min_abs_pos = abs_pos - lit_len;
    while abs_pos > min_abs_pos && candidate_pos > dict_abs {
        let cand = candidate_pos - 1;
        // SAFETY: the cursor stays above the literal run's start, which is in
        // the input, and the candidate at or above the dictionary's start;
        // each side reads the buffer that holds its position.
        let (cand_byte, cur_byte) = unsafe {
            (
                if cand >= input_abs {
                    *input.add(cand - input_abs)
                } else {
                    *dict.add(cand - dict_abs)
                },
                *input.add(abs_pos - 1 - input_abs),
            )
        };
        if cand_byte != cur_byte {
            break;
        }
        candidate_pos -= 1;
        abs_pos -= 1;
        match_len += 1;
    }
    MatchCandidate {
        start: abs_pos,
        offset: abs_pos - candidate_pos,
        match_len,
    }
}

impl DfastMatchGenerator {
    /// The dfast loop of the encoder tier `K`: its
    /// `start_matching_fast_loop_<tier>` wrapper, so every per-position
    /// `common_prefix_len_ptr` inlines under one `#[target_feature]` umbrella
    /// (mirrors `build_optimal_plan_impl`). The tier match keeps only that one
    /// call.
    fn start_matching_fast_loop<K: EncodeTier>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        // Which loop this block scans with is settled ONCE here, off the hot
        // path, so nothing inside the scan branches on a block-invariant.
        //
        // A dictionary sends the block to its OWN loop, the way upstream keeps
        // `noDict` and `dictMatchState` as separate functions: the shapes
        // differ (one cursor against two) because the dictionary's tables,
        // shifts and region bound leave no registers for a second cursor's
        // state. See `start_matching_dict_loop_body!`.
        //
        // Without one, `BORROWED` picks between a borrowed-window scan and the
        // owned history concat: the borrowed kernel folds the rebase
        // coordinates to a literal `0`, erasing the per-position arithmetic the
        // owned path needs (upstream `base + index` shape). With one, it picks
        // whether the input follows the dictionary in the history or is read
        // where the caller holds it.
        let use_dict = self.dict.table().is_some();
        let borrowed = self.borrowed_block.is_some();
        // The slot format is the third axis, settled here the same way. A
        // dictionary frame reads the live tables bare (see `bind_tables`'s
        // callers), so only the plain loops come in both formats.
        let tagged = self.tagged;
        debug_assert!(
            !(use_dict && tagged),
            "the dictionary loop reads bare slots"
        );
        // The short key width is the last axis: both loops are templated on
        // it, as upstream templates `noDict` and `dictMatchState` on `mls`
        // and picks one with a single `switch` (zstd_double_fast.c:568-605).
        let mls = self.mls;
        debug_assert!((4..=7).contains(&mls), "dfast key width {mls} out of 4..=7");
        macro_rules! dispatch_dict {
            ($kernel:ident, $dict_kernel:ident) => {
                match mls {
                    4 => dispatch_dict!(@at 4, $kernel, $dict_kernel),
                    5 => dispatch_dict!(@at 5, $kernel, $dict_kernel),
                    6 => dispatch_dict!(@at 6, $kernel, $dict_kernel),
                    _ => dispatch_dict!(@at 7, $kernel, $dict_kernel),
                }
            };
            (@at $m:literal, $kernel:ident, $dict_kernel:ident) => {
                if use_dict {
                    if borrowed {
                        self.$dict_kernel::<true, $m>(current_abs_start, current_len, handle_sequence)
                    } else {
                        self.$dict_kernel::<false, $m>(current_abs_start, current_len, handle_sequence)
                    }
                } else {
                    match (borrowed, tagged) {
                        (true, true) => self.$kernel::<true, true, $m>(
                            current_abs_start,
                            current_len,
                            handle_sequence,
                        ),
                        (true, false) => self.$kernel::<true, false, $m>(
                            current_abs_start,
                            current_len,
                            handle_sequence,
                        ),
                        (false, true) => self.$kernel::<false, true, $m>(
                            current_abs_start,
                            current_len,
                            handle_sequence,
                        ),
                        (false, false) => self.$kernel::<false, false, $m>(
                            current_abs_start,
                            current_len,
                            handle_sequence,
                        ),
                    }
                }
            };
        }
        #[cfg(all(
            target_arch = "aarch64",
            target_endian = "little",
            feature = "kernel-neon"
        ))]
        {
            // NEON is resolved at compile time here, so the cached tier is not
            // read and a release build carries no choice at all. Tests ask for
            // the scalar dictionary loop through it, which is the only way this
            // target can check the two against each other — the branch is
            // `cfg(test)` and does not exist in a shipped build.
            #[cfg(test)]
            if use_dict
                && const { matches!(K::TIER, crate::encoding::fastpath::FastpathKernel::Scalar) }
            {
                macro_rules! scalar_dict {
                    ($m:literal) => {
                        if borrowed {
                            self.start_matching_dict_loop_scalar::<true, $m>(
                                current_abs_start,
                                current_len,
                                handle_sequence,
                            )
                        } else {
                            self.start_matching_dict_loop_scalar::<false, $m>(
                                current_abs_start,
                                current_len,
                                handle_sequence,
                            )
                        }
                    };
                }
                return match mls {
                    4 => scalar_dict!(4),
                    5 => scalar_dict!(5),
                    6 => scalar_dict!(6),
                    _ => scalar_dict!(7),
                };
            }
            unsafe { dispatch_dict!(start_matching_fast_loop_neon, start_matching_dict_loop_neon) }
        }
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            use crate::encoding::fastpath::FastpathKernel;
            crate::encoding::fastpath::tier_match!(K {
                #[cfg(feature = "kernel-avx2")]
                FastpathKernel::Avx2Bmi2 => unsafe {
                    dispatch_dict!(
                        start_matching_fast_loop_avx2_bmi2,
                        start_matching_dict_loop_avx2_bmi2
                    )
                },
                #[cfg(feature = "kernel-sse")]
                FastpathKernel::Sse2 | FastpathKernel::Sse42 => unsafe {
                    dispatch_dict!(start_matching_fast_loop_sse2, start_matching_dict_loop_sse2)
                },
                FastpathKernel::Scalar => dispatch_dict!(
                    start_matching_fast_loop_scalar,
                    start_matching_dict_loop_scalar
                ),
            })
        }
        #[cfg(all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        ))]
        {
            // Same `cfg(test)` door as the NEON arm: simd128 is resolved at
            // compile time here, so without it this target could never run the
            // scalar dictionary loop to check the SIMD one against.
            #[cfg(test)]
            if use_dict
                && const { matches!(K::TIER, crate::encoding::fastpath::FastpathKernel::Scalar) }
            {
                macro_rules! scalar_dict {
                    ($m:literal) => {
                        if borrowed {
                            self.start_matching_dict_loop_scalar::<true, $m>(
                                current_abs_start,
                                current_len,
                                handle_sequence,
                            )
                        } else {
                            self.start_matching_dict_loop_scalar::<false, $m>(
                                current_abs_start,
                                current_len,
                                handle_sequence,
                            )
                        }
                    };
                }
                return match mls {
                    4 => scalar_dict!(4),
                    5 => scalar_dict!(5),
                    6 => scalar_dict!(6),
                    _ => scalar_dict!(7),
                };
            }
            unsafe {
                dispatch_dict!(
                    start_matching_fast_loop_simd128,
                    start_matching_dict_loop_simd128
                )
            }
        }
        #[cfg(not(any(
            all(
                target_arch = "aarch64",
                target_endian = "little",
                feature = "kernel-neon"
            ),
            target_arch = "x86",
            target_arch = "x86_64",
            all(
                target_arch = "wasm32",
                target_feature = "simd128",
                feature = "kernel-simd128"
            )
        )))]
        {
            dispatch_dict!(
                start_matching_fast_loop_scalar,
                start_matching_dict_loop_scalar
            )
        }
    }

    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        feature = "kernel-neon"
    ))]
    #[target_feature(enable = "neon")]
    unsafe fn start_matching_fast_loop_neon<
        const BORROWED: bool,
        const TAGGED: bool,
        const MLS: usize,
    >(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
            crate::encoding::fastpath::NeonTier,
            BORROWED,
            TAGGED,
            MLS
        )
    }

    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    #[target_feature(enable = "sse2")]
    unsafe fn start_matching_fast_loop_sse2<
        const BORROWED: bool,
        const TAGGED: bool,
        const MLS: usize,
    >(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::sse2::common_prefix_len_ptr,
            crate::encoding::fastpath::Sse2Tier,
            BORROWED,
            TAGGED,
            MLS
        )
    }

    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    #[target_feature(enable = "avx2,bmi2")]
    unsafe fn start_matching_fast_loop_avx2_bmi2<
        const BORROWED: bool,
        const TAGGED: bool,
        const MLS: usize,
    >(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
            crate::encoding::fastpath::Avx2Bmi2Tier,
            BORROWED,
            TAGGED,
            MLS
        )
    }

    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    #[target_feature(enable = "simd128")]
    unsafe fn start_matching_fast_loop_simd128<
        const BORROWED: bool,
        const TAGGED: bool,
        const MLS: usize,
    >(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::simd128::common_prefix_len_ptr,
            crate::encoding::fastpath::Simd128Tier,
            BORROWED,
            TAGGED,
            MLS
        )
    }

    #[cfg(not(any(
        all(
            target_arch = "aarch64",
            target_endian = "little",
            feature = "kernel-neon"
        ),
        all(
            target_arch = "wasm32",
            target_feature = "simd128",
            feature = "kernel-simd128"
        )
    )))]
    #[allow(unused_unsafe)]
    fn start_matching_fast_loop_scalar<
        const BORROWED: bool,
        const TAGGED: bool,
        const MLS: usize,
    >(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_fast_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
            crate::encoding::fastpath::ScalarTier,
            BORROWED,
            TAGGED,
            MLS
        )
    }

    #[cfg(all(
        target_arch = "aarch64",
        target_endian = "little",
        feature = "kernel-neon"
    ))]
    #[target_feature(enable = "neon")]
    // A function of its own, as upstream's `dictMatchState` loop per `mls` is
    // (zstd_double_fast.c:562-565): inlined into the dispatcher, every key
    // width and both loop kinds shared one register allocation. Called once
    // per block, so the call costs nothing that counts.
    #[inline(never)]
    unsafe fn start_matching_dict_loop_neon<const BORROWED: bool, const MLS: usize>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_dict_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::neon::common_prefix_len_ptr,
            crate::encoding::fastpath::NeonTier,
            BORROWED,
            MLS
        )
    }

    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-sse"
    ))]
    #[target_feature(enable = "sse2")]
    // Out of line, as for the NEON loop above.
    #[inline(never)]
    unsafe fn start_matching_dict_loop_sse2<const BORROWED: bool, const MLS: usize>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_dict_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::sse2::common_prefix_len_ptr,
            crate::encoding::fastpath::Sse2Tier,
            BORROWED,
            MLS
        )
    }

    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-avx2"
    ))]
    #[target_feature(enable = "avx2,bmi2")]
    // Out of line, as for the NEON loop above.
    #[inline(never)]
    unsafe fn start_matching_dict_loop_avx2_bmi2<const BORROWED: bool, const MLS: usize>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_dict_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::avx2_bmi2::common_prefix_len_ptr,
            crate::encoding::fastpath::Avx2Bmi2Tier,
            BORROWED,
            MLS
        )
    }

    #[cfg(all(
        target_arch = "wasm32",
        target_feature = "simd128",
        feature = "kernel-simd128"
    ))]
    #[target_feature(enable = "simd128")]
    // Out of line, as for the NEON loop above.
    #[inline(never)]
    unsafe fn start_matching_dict_loop_simd128<const BORROWED: bool, const MLS: usize>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_dict_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::simd128::common_prefix_len_ptr,
            crate::encoding::fastpath::Simd128Tier,
            BORROWED,
            MLS
        )
    }

    /// Also built in test builds on the targets whose dispatch is
    /// unconditional (aarch64+NEON, wasm+simd128), where nothing would
    /// otherwise compile it: without it those targets have no second kernel to
    /// check the first against, and the scalar/SIMD agreement the dispatch
    /// assumes would go untested exactly where the SIMD one always wins.
    #[cfg(any(
        not(any(
            all(
                target_arch = "aarch64",
                target_endian = "little",
                feature = "kernel-neon"
            ),
            all(
                target_arch = "wasm32",
                target_feature = "simd128",
                feature = "kernel-simd128"
            )
        )),
        test
    ))]
    #[allow(unused_unsafe)]
    // Out of line, as for the NEON loop above.
    #[inline(never)]
    fn start_matching_dict_loop_scalar<const BORROWED: bool, const MLS: usize>(
        &mut self,
        current_abs_start: usize,
        current_len: usize,
        handle_sequence: &mut impl FnMut(Sequence),
    ) {
        start_matching_dict_loop_body!(
            self,
            current_abs_start,
            current_len,
            handle_sequence,
            crate::encoding::fastpath::scalar::common_prefix_len_ptr,
            crate::encoding::fastpath::ScalarTier,
            BORROWED,
            MLS
        )
    }
}

#[cfg(test)]
mod extend_with_repcode_tests;
#[cfg(test)]
mod tests;
