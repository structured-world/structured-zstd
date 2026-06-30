use super::dms_hash_log;

/// Regression: a tiny source makes `ZSTD_adjustCParams` shrink the BT matcher's
/// `hash_log` below the dms table's nominal 10-bit floor. The sizing must bound
/// that floor by `hash_log` rather than `clamp(10, hash_log)` with `min > max`,
/// which panicked (`min = 10, max = 7`) when priming the dict match binary-tree
/// on a BT strategy (level >= 13) with a dictionary attached.
#[test]
fn dms_hash_log_does_not_panic_when_matcher_hash_log_below_ten() {
    // region 128 -> ceil_log2 = 7; matcher hash_log = 7 (the reported case).
    let h = dms_hash_log(128, 7);
    assert!(
        h <= 7,
        "dms hash_log must never exceed the matcher hash_log, got {h}"
    );
    assert!(
        h >= 1,
        "dms hash_log must size at least one bucket, got {h}"
    );

    // Smallest sane matcher hash_log: still no panic, table sized to it.
    assert_eq!(dms_hash_log(64, 6), 6);
    assert_eq!(dms_hash_log(8, 8), 8);
}

/// With `hash_log >= 10` the established behaviour is unchanged: the dms table
/// gets at least 10 bits, grows with `ceil_log2(region)`, and is capped at the
/// matcher's `hash_log`.
#[test]
fn dms_hash_log_keeps_ten_floor_and_hash_log_cap_when_room_allows() {
    // Tiny region -> floored at 10.
    assert_eq!(dms_hash_log(64, 22), 10);
    // ceil_log2(region) wins between the 10 floor and the hash_log cap.
    assert_eq!(dms_hash_log(1 << 14, 22), 14);
    // Capped at hash_log.
    assert_eq!(dms_hash_log(1 << 20, 16), 16);
}
