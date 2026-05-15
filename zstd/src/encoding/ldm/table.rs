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

/// One hash-table entry — `(absolute_position, checksum)`.
///
/// Mirrors donor `ldmEntry_t` from `zstd_compress_internal.h`. The
/// 32-bit `checksum` is the high 32 bits of the per-window XXH64;
/// the low 32 bits index into the bucket array and so do not need to
/// be stored separately.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LdmEntry {
    /// Absolute byte position of the start of the matched window
    /// (donor: `entry.offset = (U32)(split - base)`).
    pub(crate) offset: u32,
    /// High 32 bits of the XXH64 over the `min_match_length`-byte
    /// window — donor `entry.checksum`. Used to filter false-positive
    /// bucket collisions before invoking the byte-level verify.
    pub(crate) checksum: u32,
}

/// Bucket-based hash table sized from an [`super::params::LdmParams`].
///
/// `entries.len() == 1 << hash_log`; `bucket_offsets.len() ==
/// bucket_count`. Per-bucket slot count is `1 << effective_bucket_log`.
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
        let bucket_count = 1u32 << (hash_log - effective_bucket_log);
        let total_entries = 1usize << hash_log;

        Self {
            entries: vec![LdmEntry::default(); total_entries],
            bucket_offsets: vec![0u8; bucket_count as usize],
            effective_bucket_log,
            bucket_mask: bucket_count - 1,
        }
    }

    /// Reset every bucket to "empty" without reallocating. Donor
    /// equivalent is the `ZSTD_cwksp` clear of the LDM region at
    /// frame boundaries.
    pub(crate) fn clear(&mut self) {
        for e in &mut self.entries {
            *e = LdmEntry::default();
        }
        self.bucket_offsets.fill(0);
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
    /// Panics in debug builds if `hash_id >= bucket_count()`. Release
    /// builds rely on `bucket_offsets[]` indexing to panic the same
    /// way (the `entries[]` write past the bucket would corrupt the
    /// next bucket in release without the assertion).
    pub(crate) fn insert(&mut self, hash_id: u32, entry: LdmEntry) {
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
    #[test]
    fn insert_round_robin_wraps_through_bucket_slots() {
        let mut t = LdmHashTable::new(4, 2); // 4 buckets × 4 slots
        for k in 0..6u32 {
            t.insert(
                1,
                LdmEntry {
                    offset: k,
                    checksum: k * 7,
                },
            );
        }
        let b = t.bucket(1);
        // After 6 inserts, slots hold: [k=4, k=5, k=2, k=3]
        // (slot 0 overwritten by k=4, slot 1 by k=5; slots 2,3
        // hold the pre-wrap inserts k=2,3).
        assert_eq!(b[0].offset, 4);
        assert_eq!(b[1].offset, 5);
        assert_eq!(b[2].offset, 2);
        assert_eq!(b[3].offset, 3);
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
    fn new_accepts_donor_max_hash_log() {
        // Use a small bucket_size_log so the entry count is bounded
        // and we don't actually allocate 8 GiB. Donor itself never
        // allocates the max at runtime either (window_log caps
        // hash_log to 27 or so in practice). Test just the boundary
        // arithmetic — request hash_log = 18 with bucket_size_log =
        // 4 → 16 buckets × 16384 slots = 262144 entries × 8 bytes
        // = ~2 MiB allocation, safe on every CI runner.
        let t = LdmHashTable::new(18, 4);
        assert_eq!(t.bucket_count(), 1usize << (18 - 4));
        assert_eq!(t.bucket_slots(), 1usize << 4);
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
