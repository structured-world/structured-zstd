use super::*;

/// `hash_log = 8`, `bucket_size_log = 4` → 16 buckets ×
/// 16 slots = 256 entries, matches upstream zstd sizing math.
#[test]
fn new_table_sizes_match_size_formulae() {
    let t = LdmHashTable::new(8, 4);
    assert_eq!(t.bucket_count(), 16);
    assert_eq!(t.bucket_slots(), 16);
    assert_eq!(t.entries.len(), 256);
    assert_eq!(t.bucket_offsets.len(), 16);
    assert_eq!(t.bucket_mask(), 15);
}

/// Upstream zstd `MIN(bucketSizeLog, hashLog)` clamp must apply: when
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
/// reject the upstream zstd-supported range.
///
/// We deliberately do NOT use `hash_log = 30` (upstream zstd's max)
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
    // and we don't actually allocate 8 GiB. Upstream zstd itself never
    // allocates the max at runtime either (window_log caps
    // hash_log to 27 or so in practice). Test just the boundary
    // arithmetic — request hash_log = 18 with bucket_size_log =
    // 4 → 16384 buckets × 16 slots = 262144 entries × 8 bytes
    // = ~2 MiB allocation, safe on every CI runner.
    let t = LdmHashTable::new(18, 4);
    assert_eq!(t.bucket_count(), 1usize << (18 - 4));
    assert_eq!(t.bucket_slots(), 1usize << 4);
}

/// `effective_bucket_log > 8` (upstream zstd `LDM_BUCKETSIZELOG_MAX`)
/// must panic, not silently truncate the `u8` round-robin
/// cursor at 256 slots. Upstream zstd pre-condition mirrored from
/// `zstd_ldm.c:202` where `bucketOffsets` is a `BYTE`.
#[test]
#[should_panic(expected = "ZSTD_LDM_BUCKETSIZELOG_MAX")]
fn new_rejects_bucket_size_log_above_cap() {
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
/// to the same absolute positions. Upstream zstd
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
