use super::*;

/// Upstream zstd parity: `ZSTD_hash4` on `[0x01, 0x02, 0x03, 0x04]` with
/// hash_log=12 produces a specific bit pattern. Captured here as a
/// regression tripwire so any future refactor of the multiply
/// constants surfaces immediately.
#[test]
fn hash4_matches_expected_value_on_known_input() {
    let table = FastHashTable::new(12, 4);
    let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    // SAFETY: data has 8 ≥ 4 readable bytes.
    let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
    // Manual upstream zstd calc: u32::from_le_bytes(0x04030201) * 0x9E3779B1 >> 20.
    let expected = 0x04030201u32.wrapping_mul(0x9E3779B1) >> 20;
    assert_eq!(
        h, expected,
        "hash4 must match upstream zstd multiply-shift formula"
    );
}

#[test]
fn hash5_matches_expected_value_on_known_input() {
    let table = FastHashTable::new(13, 5);
    let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    // SAFETY: data has 8 ≥ 5 readable bytes.
    let h = unsafe { table.hash_ptr::<5>(data.as_ptr()) };
    let u = u64::from_le_bytes(data);
    let expected = (((u << (64 - 40)).wrapping_mul(889_523_592_379u64)) >> (64 - 13)) as u32;
    assert_eq!(h, expected);
}

#[test]
fn get_put_round_trip_under_known_hash() {
    let mut table = FastHashTable::new(8, 4);
    let data = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];
    // SAFETY: data has 8 ≥ 4 readable bytes.
    let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
    // SAFETY: h came from hash_ptr on this table.
    unsafe {
        assert_eq!(table.get(h), 0, "fresh table reads sentinel");
        table.put(h, 0xCAFE_BABE);
        assert_eq!(table.get(h), 0xCAFE_BABE);
    }
}

#[test]
fn clear_resets_all_entries_to_sentinel() {
    let mut table = FastHashTable::new(6, 4);
    let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
    // SAFETY: 4 readable bytes.
    let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
    // SAFETY: hash came from hash_ptr.
    unsafe {
        table.put(h, 42);
    }
    table.clear();
    // SAFETY: hash came from hash_ptr.
    let read_back = unsafe { table.get(h) };
    assert_eq!(read_back, 0, "clear must zero every entry");
}

/// Upstream zstd parity for mls=6: `ZSTD_hash6` uses
/// `(u << (64-48)).wrapping_mul(PRIME_6_BYTES) >> (64 - hash_log)`.
#[test]
fn hash6_matches_expected_value_on_known_input() {
    let table = FastHashTable::new(14, 6);
    let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    // SAFETY: data has 8 ≥ 6 readable bytes (the implementation
    // performs a u64 load and shifts off the unused top bits).
    let h = unsafe { table.hash_ptr::<6>(data.as_ptr()) };
    let u = u64::from_le_bytes(data);
    let expected = (((u << (64 - 48)).wrapping_mul(227_718_039_650_203u64)) >> (64 - 14)) as u32;
    assert_eq!(
        h, expected,
        "hash6 must match upstream zstd multiply-shift formula"
    );
}

/// Upstream zstd parity for mls=7: `ZSTD_hash7` shifts by `(64-56)` and
/// multiplies by `PRIME_7_BYTES`.
#[test]
fn hash7_matches_expected_value_on_known_input() {
    let table = FastHashTable::new(15, 7);
    let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    // SAFETY: data has 8 ≥ 7 readable bytes.
    let h = unsafe { table.hash_ptr::<7>(data.as_ptr()) };
    let u = u64::from_le_bytes(data);
    let expected = (((u << (64 - 56)).wrapping_mul(58_295_818_150_454_627u64)) >> (64 - 15)) as u32;
    assert_eq!(
        h, expected,
        "hash7 must match upstream zstd multiply-shift formula"
    );
}

/// Upstream zstd parity for mls=8: `ZSTD_hash8` does NOT shift the input
/// (full u64), then multiplies by `PRIME_8_BYTES` (upstream zstd's
/// `prime8bytes = 0xCF1BBCDCB7A56463ULL`).
#[test]
fn hash8_matches_expected_value_on_known_input() {
    let table = FastHashTable::new(16, 8);
    let data = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    // SAFETY: data has 8 readable bytes.
    let h = unsafe { table.hash_ptr::<8>(data.as_ptr()) };
    let u = u64::from_le_bytes(data);
    let expected = (u.wrapping_mul(0xCF1BBCDCB7A56463u64) >> (64 - 16)) as u32;
    assert_eq!(
        h, expected,
        "hash8 must match upstream zstd multiply-shift formula"
    );
}

/// Boundary: `hash_log = 1` is the smallest accepted value
/// (table of 2 entries). Verifies the constructor doesn't reject
/// the minimum and that the table actually allocates.
#[test]
fn hash_log_minimum_one_constructs_two_entry_table() {
    let table = FastHashTable::new(1, 4);
    let data = [0u8, 0, 0, 0];
    // SAFETY: 4 readable bytes; hash output occupies 1 bit so
    // hash ∈ {0, 1}.
    let h = unsafe { table.hash_ptr::<4>(data.as_ptr()) };
    assert!(h < 2, "hash_log=1 must produce values ∈ {{0, 1}} (got {h})");
}

/// Boundary: `hash_log = ZSTD_HASHLOG_MAX` (30) is the largest
/// accepted value. Calling [`FastHashTable::new`] with this value
/// would allocate ≈4 GiB (`1 << 30` × `sizeof(u32)`), well over
/// per-test memory budgets on CI runners — instead exercise the
/// same accept path via the non-allocating [`validate_params`]
/// helper. Pairs with the `should_panic` tests below that prove
/// rejection of the out-of-band cases.
#[test]
fn hash_log_maximum_thirty_is_accepted_by_validation() {
    validate_params(ZSTD_HASHLOG_MAX, 4);
    validate_params(ZSTD_HASHLOG_MAX, 8);
}

#[test]
#[should_panic(expected = "hash_log must be in 1..=")]
fn panics_on_zero_hash_log() {
    let _ = FastHashTable::new(0, 4);
}

#[test]
#[should_panic(expected = "hash_log must be in 1..=")]
fn panics_on_hash_log_above_zstd_hashlog_max() {
    // 31 > ZSTD_HASHLOG_MAX (30) → constructor rejects.
    let _ = FastHashTable::new(31, 4);
}

#[test]
#[should_panic(expected = "ZSTD Fast strategy only supports mls 4..=8")]
fn panics_on_mls_below_four() {
    let _ = FastHashTable::new(12, 3);
}

#[test]
#[should_panic(expected = "ZSTD Fast strategy only supports mls 4..=8")]
fn panics_on_mls_above_eight() {
    let _ = FastHashTable::new(12, 9);
}
