//! Bucket-based hash table for the LDM producer.
//!
//! Direct port of the table portion of `ldmState_t` and the
//! `ZSTD_ldm_getBucket` / `ZSTD_ldm_insertEntry` helpers from
//! `lib/compress/zstd_ldm.c` v1.5.7.
//!
//! Layout (donor `zstd_ldm.c:188-207`):
//!
//! ```text
//! entries: [LdmEntry; 1 << hash_log]
//!   = bucket_count buckets of `1 << bucket_size_log` slots each
//!
//! bucket_offsets: [u8; bucket_count]
//!   = round-robin write cursor per bucket (donor uses one BYTE)
//!
//! bucket_count = 1 << (hash_log - bucket_size_log)
//! ```
//!
//! Lookup is a bare slice into `entries`; insertion is a single
//! 64-bit write plus a one-byte modular cursor bump. There is no
//! eviction policy beyond the round-robin overwrite, which mirrors
//! donor's behaviour and is correct because LDM tolerates dropped
//! candidates (the verify step rejects stale entries via the
//! checksum + window-distance check).
//!
//! Donor `bucket_size_log` is silently clamped to `hash_log`
//! (`zstd_ldm.c:176`); the same clamp lives in [`LdmHashTable::new`].
//!
//! See [`super::params::LdmParams`] for how the logs are derived.

use alloc::vec;
use alloc::vec::Vec;

/// One hash-table entry — `(offset_relative_to_position_base,
/// checksum)`.
///
/// Mirrors donor `ldmEntry_t` from `zstd_compress_internal.h`.
///
/// **Offset semantics:** `offset` is **NOT an absolute stream
/// position**. It is stored relative to the owning
/// [`LdmHashTable::position_base`] so that streams larger than
/// `u32::MAX` (4 GiB) can be encoded without truncation: the
/// table calls [`LdmHashTable::resolve`] to translate
/// `entry.offset` to an absolute position when the producer or
/// search path needs it, and periodically rebases via
/// [`LdmHashTable::reduce`] (donor `ZSTD_ldm_reduceTable`,
/// `zstd_ldm.c:520`) to keep `position - position_base ≤
/// u32::MAX`. Same scheme `MatchTable` uses for the BT/HC chain
/// table.
///
/// The 32-bit `checksum` is the high 32 bits of the per-window
/// XXH64; the low 32 bits index into the bucket array and so do
/// not need to be stored separately.
///
/// `offset == 0` is the reserved **empty-slot sentinel** —
/// callers must never produce a real relative offset of 0
/// (every insert path adds 1 to the relative position before
/// storing).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LdmEntry {
    /// Position relative to [`LdmHashTable::position_base`], with
    /// a +1 bias so a fresh default-zeroed entry remains
    /// distinguishable as "empty". A real absolute position
    /// `P >= position_base` is stored as `(P - position_base + 1)
    /// as u32`; the [`LdmHashTable::resolve`] helper handles the
    /// inverse translation.
    pub(crate) offset: u32,
    /// High 32 bits of the XXH64 over the `min_match_length`-byte
    /// window — donor `entry.checksum`. Used to filter
    /// false-positive bucket collisions before invoking the
    /// byte-level verify.
    pub(crate) checksum: u32,
}

/// Margin reserved above the high-water mark before triggering a
/// rebase. Donor `ZSTD_ldm_reduceTable` is called at a coarser
/// granularity (per-frame `ZSTD_window_update`); ours kicks in
/// per-insert via [`LdmHashTable::ensure_room_for`] but uses a
/// generous safety band so the rebase is amortised across many
/// inserts.
const REBASE_GUARD_BAND: u32 = 1u32 << 30;

/// Bucket-based hash table sized from an [`super::params::LdmParams`].
///
/// `entries.len() == 1 << hash_log`; `bucket_offsets.len() ==
/// bucket_count`. Per-bucket slot count is `1 << effective_bucket_log`.
///
/// **Coordinate model:** entries store positions relative to
/// `position_base` (see [`LdmEntry`] for the `+1` empty-slot
/// convention). The producer interacts with the table via the
/// [`LdmHashTable::insert_absolute`] / [`LdmHashTable::resolve`]
/// helpers so absolute-position support extends past `u32::MAX`
/// transparently — same pattern as `MatchTable`'s chain rebase.
#[derive(Clone)]
pub(crate) struct LdmHashTable {
    entries: Vec<LdmEntry>,
    bucket_offsets: Vec<u8>,
    /// Effective bucket-size log after the donor's
    /// `MIN(bucketSizeLog, hashLog)` clamp (`zstd_ldm.c:176`).
    effective_bucket_log: u32,
    /// Hash-id mask: `bucket_count - 1`. The caller must hand in
    /// hash ids in `[0, bucket_count)`; this mask is exposed so the
    /// producer can clamp without re-deriving it.
    bucket_mask: u32,
    /// Absolute stream position that corresponds to a stored
    /// `entry.offset == 1` (offset 0 is the empty-slot sentinel).
    /// Advanced by [`Self::reduce`] when the producer is about to
    /// store a position beyond the `u32` window — donor
    /// `ZSTD_ldm_reduceTable` (`zstd_ldm.c:520`).
    position_base: usize,
}

impl LdmHashTable {
    /// Allocate a fresh table for the given parameters.
    ///
    /// Donor parity: matches `ZSTD_ldm_getTableSize` (`zstd_ldm.c:175-180`)
    /// in shape — `hashTable + bucketOffsets`. `bucket_size_log` is
    /// clamped to `hash_log` to mirror the donor's silent floor on
    /// the per-bucket slot count.
    ///
    /// # Panics
    ///
    /// Panics if `hash_log == 0` (no buckets) or `hash_log > 30`
    /// (would allocate > 8 GiB of entries — far beyond donor's
    /// `ZSTD_LDM_HASHLOG_MAX = 30`). Both bounds match donor.
    pub(crate) fn new(hash_log: u32, bucket_size_log: u32) -> Self {
        assert!(hash_log > 0, "hash_log must be > 0");
        assert!(
            hash_log <= 30,
            "hash_log {hash_log} exceeds donor ZSTD_LDM_HASHLOG_MAX (30)"
        );
        // Donor `zstd_ldm.c:176`: effective bucket_size_log is the
        // min of caller's request and hash_log. Without the clamp a
        // bucket would span the whole table and bucket_count would
        // be zero — undefined behaviour upstream and a div-by-zero
        // here.
        let effective_bucket_log = bucket_size_log.min(hash_log);
        // Donor cap `ZSTD_LDM_BUCKETSIZELOG_MAX = 8` (`zstd.h:1300`).
        // We store the per-bucket round-robin cursor in a `u8`
        // (mirrors donor `BYTE bucketOffsets[]` in `zstd_ldm.c:202`),
        // which silently truncates at 256 slots — i.e. above this
        // cap the cursor would only cycle through the first 256
        // entries of a larger bucket, dropping new inserts on the
        // floor. The assertion enforces the donor pre-condition so
        // callers that bypass `LdmParams::adjust_for` (which already
        // clamps to `LDM_BUCKETSIZELOG_MAX`) still get a clear
        // failure instead of silent data loss.
        assert!(
            effective_bucket_log <= 8,
            "effective bucket_size_log {effective_bucket_log} exceeds donor \
             ZSTD_LDM_BUCKETSIZELOG_MAX (8); a u8 cursor would silently \
             truncate at 256 slots — widen the cursor or clamp the request"
        );
        let bucket_count = 1u32 << (hash_log - effective_bucket_log);
        let total_entries = 1usize << hash_log;

        Self {
            entries: vec![LdmEntry::default(); total_entries],
            bucket_offsets: vec![0u8; bucket_count as usize],
            effective_bucket_log,
            bucket_mask: bucket_count - 1,
            position_base: 0,
        }
    }

    /// Heap bytes held by the entry array and the per-bucket offset array.
    pub(crate) fn heap_size(&self) -> usize {
        self.entries.capacity() * core::mem::size_of::<LdmEntry>() + self.bucket_offsets.capacity()
    }

    /// Reset every bucket to "empty" without reallocating, and
    /// rewind the rebase base so a fresh frame can start its
    /// absolute positions from any value (including 0) without
    /// tripping the `abs_pos >= position_base` assertion in
    /// [`Self::insert_absolute`]. Donor equivalent is the
    /// `ZSTD_cwksp` clear of the LDM region at frame boundaries.
    /// `LdmEntry` is `Copy` so the slice `fill` compiles down to
    /// a bulk memset — meaningful when `hash_log` is large and
    /// this runs at every frame boundary.
    pub(crate) fn clear(&mut self) {
        self.entries.fill(LdmEntry::default());
        self.bucket_offsets.fill(0);
        self.position_base = 0;
    }

    /// Number of buckets (`1 << (hash_log - bucket_size_log)`).
    pub(crate) const fn bucket_count(&self) -> usize {
        // bucket_mask + 1 == bucket_count; computed at construction
        // so this is a single load.
        self.bucket_mask as usize + 1
    }

    /// Per-bucket slot count (`1 << effective_bucket_log`).
    pub(crate) const fn bucket_slots(&self) -> usize {
        1usize << self.effective_bucket_log
    }

    /// Slice of `bucket_slots()` entries for `hash_id`.
    ///
    /// `hash_id` MUST be in `[0, bucket_count())`. The caller is
    /// responsible for masking via [`Self::bucket_mask`] before
    /// calling — donor `ZSTD_ldm_getBucket` performs no clamping
    /// either, leaving the responsibility to the producer.
    pub(crate) fn bucket(&self, hash_id: u32) -> &[LdmEntry] {
        let start = (hash_id as usize) << self.effective_bucket_log;
        let len = self.bucket_slots();
        &self.entries[start..start + len]
    }

    /// Insert `entry` into the bucket for `hash_id` at the bucket's
    /// next round-robin slot.
    ///
    /// Donor `ZSTD_ldm_insertEntry` (`zstd_ldm.c:198-207`): the
    /// per-bucket `bucket_offsets[hash_id]` byte is the next write
    /// position, post-bump it modulo `1 << bucket_size_log`. The
    /// modulo is implemented by masking with `slots - 1`.
    ///
    /// # Panics
    ///
    /// Panics in all build modes if `entry.offset == 0` — that
    /// value is reserved for the empty-slot sentinel and an
    /// accepted insert would be silently dropped by
    /// [`Self::resolve`]; callers must produce a +1-biased
    /// relative offset (use [`Self::insert_absolute`] for the
    /// common path).
    ///
    /// Out-of-range `hash_id` panics in both debug and release
    /// (Vec bounds-check on `bucket_offsets[hash_id]`); the
    /// `debug_assert!` exists purely to produce an earlier,
    /// clearer error than the bare index-out-of-bounds.
    pub(crate) fn insert(&mut self, hash_id: u32, entry: LdmEntry) {
        // Runtime check (not `debug_assert!`): storing offset 0
        // would alias the empty-slot sentinel, so a buggy caller
        // would silently lose candidates rather than fail fast.
        assert!(
            entry.offset != 0,
            "offset 0 is reserved for the empty-slot sentinel; \
             use `insert_absolute` (which applies the +1 bias) or \
             store a +1-biased relative offset before calling insert"
        );
        debug_assert!(
            hash_id <= self.bucket_mask,
            "hash_id {hash_id} out of range (bucket_count = {})",
            self.bucket_count()
        );
        // Read the cursor first (a copy out of the byte array) so the
        // subsequent mutable accesses to `entries` and the write-back
        // to `bucket_offsets` do not need to live simultaneously.
        let slot_mask = self.bucket_slots() - 1;
        let bucket_start = (hash_id as usize) << self.effective_bucket_log;
        let offset = self.bucket_offsets[hash_id as usize] as usize;
        self.entries[bucket_start + offset] = entry;
        // Post-increment modulo bucket size. Donor stores the result
        // in a BYTE (so the mask is implicit at bucket_size_log <= 8);
        // we mirror the explicit mask so values above 8 (clamped by
        // params, but defensive here) still wrap correctly.
        let next = (offset + 1) & slot_mask;
        self.bucket_offsets[hash_id as usize] = next as u8;
    }

    /// Mask to use on a raw hash to derive `hash_id`. Saves the
    /// caller from re-deriving `bucket_count - 1`.
    pub(crate) const fn bucket_mask(&self) -> u32 {
        self.bucket_mask
    }

    /// Absolute stream position that corresponds to the +1
    /// relative-offset bias — i.e. an entry with `offset == 1`
    /// refers to absolute byte `position_base`.
    pub(crate) const fn position_base(&self) -> usize {
        self.position_base
    }

    /// Translate an entry's stored `offset` (relative + 1-biased)
    /// back to an absolute stream position. Returns `None` for
    /// the empty-slot sentinel (`offset == 0`).
    pub(crate) fn resolve(&self, entry: &LdmEntry) -> Option<usize> {
        match entry.offset {
            0 => None,
            rel => Some(self.position_base + (rel as usize) - 1),
        }
    }

    /// Insert a candidate at absolute position `abs_pos` into the
    /// bucket for `hash_id`. The caller is responsible for
    /// invoking [`Self::ensure_room_for`] beforehand so the
    /// relative-offset arithmetic stays inside `u32`.
    ///
    /// # Panics
    ///
    /// Panics in **all build modes** if `abs_pos < position_base`
    /// (enforced by `checked_sub(...).unwrap_or_else(panic!)`) or
    /// if `abs_pos - position_base + 1 > u32::MAX` (enforced by an
    /// `assert!`). Both checks are runtime — never `debug_assert!`
    /// — so a contract violation cannot silently wrap a relative
    /// offset and corrupt the table in release. The producer must
    /// call `ensure_room_for` before the first insert that could
    /// exceed the window.
    pub(crate) fn insert_absolute(&mut self, hash_id: u32, abs_pos: usize, checksum: u32) {
        // Use runtime panics (rather than `debug_assert!`) for both
        // preconditions so a contract violation never silently
        // wraps in release: an unchecked `abs_pos - position_base`
        // would underflow and the subsequent `as u32` cast would
        // corrupt the table, hiding the bug far from its source.
        let rel = abs_pos.checked_sub(self.position_base).unwrap_or_else(|| {
            panic!(
                "insert position {abs_pos} is below position_base {}; \
                 callers must rebase via `ensure_room_for` or clear before \
                 reaching this state",
                self.position_base
            )
        });
        assert!(
            rel < u32::MAX as usize,
            "insert position {abs_pos} (rel {rel}) exceeds u32 window; \
             producer must call `ensure_room_for` before insert"
        );
        // +1 bias so 0 stays reserved for the empty-slot sentinel.
        // The cast happens BEFORE the `+ 1` so the addition runs in
        // `u32` arithmetic — guarded by the `rel < u32::MAX` assert
        // above, the `+ 1` cannot overflow, and intermediate `usize`
        // values stay bounded on 32-bit targets where
        // `usize == u32`.
        let stored = (rel as u32) + 1;
        self.insert(
            hash_id,
            LdmEntry {
                offset: stored,
                checksum,
            },
        );
    }

    /// Ensure that absolute position `abs_pos` can be stored as a
    /// `u32` offset relative to [`Self::position_base`] without
    /// overflow. When the relative position would exceed
    /// `u32::MAX − REBASE_GUARD_BAND`, advance `position_base` by
    /// [`REBASE_GUARD_BAND`] and run [`Self::reduce`] across all
    /// entries. In the common case (`rel <= max_rel`) the call is
    /// a single compare and returns immediately.
    ///
    /// The producer calls this before every insert so positions
    /// past 4 GiB rebase transparently. Caller positions remain
    /// absolute on both sides of the rebase — `Self::resolve` /
    /// `Self::insert_absolute` handle the relative-coordinate
    /// translation internally. Donor: `ZSTD_ldm_reduceTable`
    /// (`zstd_ldm.c:520`), invoked from `ZSTD_window_update`.
    pub(crate) fn ensure_room_for(&mut self, abs_pos: usize) {
        if abs_pos < self.position_base {
            // The caller asked about a position BEFORE the
            // current base — that's the producer error case,
            // not a rebase trigger.
            return;
        }
        // Leave at least `REBASE_GUARD_BAND` of u32 headroom
        // above the current insert position so the next batch of
        // inserts doesn't immediately re-rebase. The producer
        // advances `abs_pos` in small strides today, so the loop
        // typically runs at most once; the `while` is required by
        // the doc guarantee — a caller that jumps the position by
        // more than one guard band (e.g. a future hand-off path
        // that skips frame regions) would otherwise leave `rel`
        // above `u32::MAX` after a single shift and panic on the
        // next insert.
        let max_rel = u32::MAX as usize - REBASE_GUARD_BAND as usize;
        while abs_pos - self.position_base > max_rel {
            // Shift the base forward by `REBASE_GUARD_BAND` —
            // matches donor's "subtract the reducer value, clamp
            // anything below to 0" semantics.
            self.reduce(REBASE_GUARD_BAND);
        }
    }

    /// Donor `ZSTD_ldm_reduceTable` (`zstd_ldm.c:520`): subtract
    /// `reducer` from every entry's relative offset, saturating
    /// anything below at 0 (which becomes the empty-slot
    /// sentinel); advance `position_base` by `reducer` so future
    /// inserts continue from the shifted origin.
    pub(crate) fn reduce(&mut self, reducer: u32) {
        for entry in &mut self.entries {
            if entry.offset <= reducer {
                entry.offset = 0;
            } else {
                entry.offset -= reducer;
            }
        }
        self.position_base = self.position_base.saturating_add(reducer as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hash_log = 8`, `bucket_size_log = 4` → 16 buckets ×
    /// 16 slots = 256 entries, matches donor sizing math.
    #[test]
    fn new_table_sizes_match_donor_formulae() {
        let t = LdmHashTable::new(8, 4);
        assert_eq!(t.bucket_count(), 16);
        assert_eq!(t.bucket_slots(), 16);
        assert_eq!(t.entries.len(), 256);
        assert_eq!(t.bucket_offsets.len(), 16);
        assert_eq!(t.bucket_mask(), 15);
    }

    /// Donor `MIN(bucketSizeLog, hashLog)` clamp must apply: when
    /// caller requests `bucket_size_log > hash_log` the bucket
    /// collapses to a single bucket covering all entries.
    #[test]
    fn new_clamps_bucket_size_log_to_hash_log() {
        let t = LdmHashTable::new(6, 12); // bucket > hash → clamp
        assert_eq!(t.bucket_count(), 1, "clamp must yield a single bucket");
        assert_eq!(t.bucket_slots(), 1usize << 6);
        assert_eq!(t.entries.len(), 1usize << 6);
    }

    /// Round-robin insertion fills the bucket then wraps.
    /// Uses offsets `1..=6` (not `0..6`) so the test does not
    /// rely on the sentinel value `0`, which
    /// [`LdmHashTable::insert`] now rejects with a runtime assert.
    #[test]
    fn insert_round_robin_wraps_through_bucket_slots() {
        let mut t = LdmHashTable::new(4, 2); // 4 buckets × 4 slots
        for k in 1..=6u32 {
            t.insert(
                1,
                LdmEntry {
                    offset: k,
                    checksum: k * 7,
                },
            );
        }
        let b = t.bucket(1);
        // After 6 inserts in a 4-slot bucket the round-robin
        // cursor cycles 0→1→2→3→0→1, so the last write to each
        // slot is k=5 (slot 0), k=6 (slot 1), k=3 (slot 2),
        // k=4 (slot 3).
        assert_eq!(b[0].offset, 5);
        assert_eq!(b[1].offset, 6);
        assert_eq!(b[2].offset, 3);
        assert_eq!(b[3].offset, 4);
    }

    /// `insert` must reject the empty-slot sentinel `offset == 0`
    /// — otherwise the entry would survive in the bucket but be
    /// invisible to [`LdmHashTable::resolve`] (which treats `0`
    /// as the empty marker), silently dropping candidates.
    /// Regression for PR #139 round-16 review (CodeRabbit Major).
    #[test]
    #[should_panic(expected = "offset 0 is reserved")]
    fn insert_panics_on_sentinel_offset_zero() {
        let mut t = LdmHashTable::new(4, 2);
        t.insert(
            0,
            LdmEntry {
                offset: 0,
                checksum: 0xDEAD,
            },
        );
    }

    /// Inserts to one bucket must not bleed into adjacent buckets.
    /// Guards against off-by-one in the `bucket_start` arithmetic.
    #[test]
    fn insert_does_not_contaminate_adjacent_bucket() {
        let mut t = LdmHashTable::new(4, 2);
        t.insert(
            2,
            LdmEntry {
                offset: 42,
                checksum: 0xCAFE,
            },
        );
        let b0 = t.bucket(0);
        let b1 = t.bucket(1);
        let b3 = t.bucket(3);
        for e in b0.iter().chain(b1.iter()).chain(b3.iter()) {
            assert_eq!(
                *e,
                LdmEntry::default(),
                "neighbouring buckets must stay empty"
            );
        }
        assert_eq!(t.bucket(2)[0].offset, 42);
    }

    /// `clear` rewinds bucket cursors and zeros entries.
    #[test]
    fn clear_zeros_entries_and_rewinds_cursors() {
        let mut t = LdmHashTable::new(4, 2);
        for k in 0..4u32 {
            t.insert(
                k % 4,
                LdmEntry {
                    offset: k + 1,
                    checksum: k * 11,
                },
            );
        }
        t.clear();
        for e in t.bucket(0).iter().chain(t.bucket(3).iter()) {
            assert_eq!(*e, LdmEntry::default());
        }
        for c in &t.bucket_offsets {
            assert_eq!(*c, 0);
        }
        // First insert after clear must land at slot 0.
        t.insert(
            2,
            LdmEntry {
                offset: 99,
                checksum: 0,
            },
        );
        assert_eq!(t.bucket(2)[0].offset, 99);
    }

    /// Boundary-arithmetic smoke test: a moderately large `hash_log`
    /// must allocate without panic and produce a sane bucket count.
    /// Doubles as a guard that the assertions don't accidentally
    /// reject the donor-supported range.
    ///
    /// We deliberately do NOT use `hash_log = 30` (donor's max)
    /// because that would allocate 8 GiB of entries; the bucket
    /// arithmetic is the same at every log so 18 is sufficient.
    /// Gated to 64-bit pointer widths to avoid the 32-bit CI shards
    /// where the 2 MiB allocation would still succeed but the
    /// `usize` × `u32` cast would over-restrict the integer types
    /// we exercise elsewhere.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn new_accepts_large_hash_log_smoke() {
        // Use a small bucket_size_log so the entry count is bounded
        // and we don't actually allocate 8 GiB. Donor itself never
        // allocates the max at runtime either (window_log caps
        // hash_log to 27 or so in practice). Test just the boundary
        // arithmetic — request hash_log = 18 with bucket_size_log =
        // 4 → 16384 buckets × 16 slots = 262144 entries × 8 bytes
        // = ~2 MiB allocation, safe on every CI runner.
        let t = LdmHashTable::new(18, 4);
        assert_eq!(t.bucket_count(), 1usize << (18 - 4));
        assert_eq!(t.bucket_slots(), 1usize << 4);
    }

    /// `effective_bucket_log > 8` (donor `LDM_BUCKETSIZELOG_MAX`)
    /// must panic, not silently truncate the `u8` round-robin
    /// cursor at 256 slots. Donor pre-condition mirrored from
    /// `zstd_ldm.c:202` where `bucketOffsets` is a `BYTE`.
    #[test]
    #[should_panic(expected = "ZSTD_LDM_BUCKETSIZELOG_MAX")]
    fn new_rejects_bucket_size_log_above_donor_cap() {
        // hash_log = 12, bucket_size_log = 9 → effective = 9 > 8
        // → assertion fires.
        let _ = LdmHashTable::new(12, 9);
    }

    /// `insert_absolute` + `resolve` round-trip preserves the
    /// absolute position across the +1 empty-slot bias.
    #[test]
    fn insert_absolute_round_trips_through_resolve() {
        let mut t = LdmHashTable::new(4, 2);
        t.insert_absolute(1, 42, 0xCAFE);
        let entry = t.bucket(1)[0];
        assert_eq!(entry.checksum, 0xCAFE);
        assert_eq!(
            t.resolve(&entry),
            Some(42),
            "stored relative offset must resolve back to the inserted absolute"
        );
    }

    /// `resolve` returns `None` for the empty-slot sentinel
    /// (`offset == 0`), distinguishing it from any real
    /// inserted position.
    #[test]
    fn resolve_returns_none_for_empty_slot() {
        let t = LdmHashTable::new(4, 2);
        let empty = LdmEntry::default();
        assert_eq!(empty.offset, 0);
        assert_eq!(t.resolve(&empty), None);
    }

    /// `reduce` subtracts the reducer from every entry's relative
    /// offset (saturating at 0 = empty sentinel) and advances
    /// `position_base` so future `resolve` calls translate back
    /// to the same absolute positions. Donor
    /// `ZSTD_ldm_reduceTable` (`zstd_ldm.c:520`).
    #[test]
    fn reduce_preserves_resolved_absolute_positions() {
        let mut t = LdmHashTable::new(4, 2);
        t.insert_absolute(0, 100, 0xAAAA);
        t.insert_absolute(1, 200, 0xBBBB);
        t.insert_absolute(2, 300, 0xCCCC);
        assert_eq!(t.position_base(), 0);

        // Shift the base forward by 150; positions 100 and 200
        // should still resolve to 100 and 200 (the relative
        // offsets shift but absolute stays).
        t.reduce(150);
        assert_eq!(t.position_base(), 150);
        // pos 100 had relative offset 101 → after reduce: max(101−150, 0) = 0 (sentinel)
        let entry0 = t.bucket(0)[0];
        assert_eq!(
            t.resolve(&entry0),
            None,
            "pos 100 < new_base 150 must be evicted"
        );
        // pos 200 had relative 201 → 201−150 = 51 → resolved 150 + 51 − 1 = 200
        let entry1 = t.bucket(1)[0];
        assert_eq!(t.resolve(&entry1), Some(200));
        // pos 300 had relative 301 → 301−150 = 151 → resolved 150 + 151 − 1 = 300
        let entry2 = t.bucket(2)[0];
        assert_eq!(t.resolve(&entry2), Some(300));
    }

    /// `ensure_room_for` triggers a rebase when the relative
    /// offset would exceed the guard band, keeping the `u32`
    /// storage valid for streams past 4 GiB.
    #[test]
    fn ensure_room_for_rebases_above_guard_band() {
        let mut t = LdmHashTable::new(4, 2);
        // First insert at moderate offset — no rebase needed.
        t.insert_absolute(0, 1024, 0xAAAA);
        assert_eq!(t.position_base(), 0);

        // Probe a position that would overflow the guard band:
        // u32::MAX - REBASE_GUARD_BAND + 1 = the smallest abs
        // that triggers a rebase.
        let trigger_pos = (u32::MAX as usize) - (REBASE_GUARD_BAND as usize) + 1;
        t.ensure_room_for(trigger_pos);
        assert_eq!(
            t.position_base(),
            REBASE_GUARD_BAND as usize,
            "rebase must advance position_base by REBASE_GUARD_BAND"
        );
        // The earlier insert at 1024 had relative offset 1025;
        // after rebase by 2^30 it's clamped to 0 (empty) since
        // 1025 < REBASE_GUARD_BAND.
        assert_eq!(t.resolve(&t.bucket(0)[0]), None);

        // A fresh insert past the rebase boundary must still
        // round-trip.
        t.insert_absolute(2, trigger_pos, 0xCAFE);
        assert_eq!(t.resolve(&t.bucket(2)[0]), Some(trigger_pos));
    }

    /// `ensure_room_for` must loop until `rel <= max_rel` even if
    /// the caller jumps the position past several guard bands in
    /// a single call. With the old single-shot `if`, a jump
    /// larger than `2 * REBASE_GUARD_BAND` left `rel` above
    /// `u32::MAX - REBASE_GUARD_BAND`, so the next
    /// `insert_absolute` would panic on the `(rel + 1) as u32`
    /// cast. Regression for PR #139 round-14 review (CodeRabbit
    /// Major).
    ///
    /// Gated to 64-bit pointer widths: `5 * REBASE_GUARD_BAND`
    /// (= 5 GiB) overflows `usize` on 32-bit targets, where the
    /// scenario is unreachable anyway because `usize::MAX` < 4
    /// GiB caps the addressable stream below the rebase
    /// threshold.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn ensure_room_for_loops_across_multiple_guard_bands() {
        let mut t = LdmHashTable::new(4, 2);
        // Jump past two guard bands at once. With u32::MAX ≈
        // 4 * REBASE_GUARD_BAND and max_rel = u32::MAX -
        // REBASE_GUARD_BAND ≈ 3 * REBASE_GUARD_BAND, an abs_pos
        // of 5 * REBASE_GUARD_BAND yields rel = 5 *
        // REBASE_GUARD_BAND > max_rel even after one reduce
        // (rel = 4 * REBASE_GUARD_BAND > 3 * REBASE_GUARD_BAND).
        // A second reduce brings rel = 3 * REBASE_GUARD_BAND ≤
        // max_rel and the loop exits.
        let abs_pos = 5usize * (REBASE_GUARD_BAND as usize);
        t.ensure_room_for(abs_pos);
        let max_rel = u32::MAX as usize - REBASE_GUARD_BAND as usize;
        assert!(
            abs_pos - t.position_base() <= max_rel,
            "ensure_room_for must rebase until rel ≤ max_rel \
             (got rel = {}, max_rel = {})",
            abs_pos - t.position_base(),
            max_rel
        );
        // The subsequent insert must succeed (no u32 overflow).
        t.insert_absolute(0, abs_pos, 0xFEED);
        assert_eq!(t.resolve(&t.bucket(0)[0]), Some(abs_pos));
    }

    /// `insert_absolute` must panic in BOTH debug and release
    /// builds when called with an `abs_pos` below the current
    /// `position_base` — silently underflowing the subtraction
    /// would store a wraparound relative offset and corrupt the
    /// table far from the bug's source. Regression for PR #139
    /// round-6 review (Copilot + CodeRabbit, Major).
    #[test]
    #[should_panic(expected = "below position_base")]
    fn insert_absolute_panics_below_position_base() {
        let mut t = LdmHashTable::new(4, 2);
        t.reduce(100); // shift position_base to 100
        t.insert_absolute(0, 50, 0xCAFE); // 50 < 100 → panic
    }

    /// `clear()` must reset `position_base` to 0 so a subsequent
    /// frame can insert at any absolute position (including
    /// values below the previous frame's `position_base`)
    /// without tripping the `abs_pos >= position_base`
    /// assertion. Regression for PR #139 round-4 CodeRabbit
    /// nitpick.
    #[test]
    fn clear_resets_position_base() {
        let mut t = LdmHashTable::new(4, 2);
        t.insert_absolute(0, 1024, 0xAAAA);
        t.reduce(1 << 20); // shift base forward
        assert!(t.position_base() > 0);
        t.clear();
        assert_eq!(t.position_base(), 0, "clear must rewind position_base to 0");
        // Inserting at absolute 0 after clear must succeed —
        // would panic on the assertion if position_base wasn't
        // reset.
        t.insert_absolute(0, 0, 0xCAFE);
        let entry = t.bucket(0)[0];
        assert_eq!(t.resolve(&entry), Some(0));
    }

    /// `bucket_mask` returned by the table must agree with the
    /// derived `bucket_count - 1`. Guard against drift if the
    /// internal field is renamed.
    #[test]
    fn bucket_mask_matches_count_minus_one() {
        let t = LdmHashTable::new(8, 3);
        assert_eq!(t.bucket_mask() as usize + 1, t.bucket_count());
    }
}
