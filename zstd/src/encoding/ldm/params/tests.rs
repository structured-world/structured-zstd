use super::*;

/// Spot-check upstream zstd strategy → hash_rate_log mapping
/// (`zstd_ldm.c:151`): `7 - strategy/3`.
///
///   strategy 1 (fast)     → 7
///   strategy 3 (greedy)   → 6
///   strategy 6 (btlazy2)  → 5
///   strategy 9 (btultra2) → 4
/// `adjust_for` must panic in BOTH debug and release builds
/// when handed an out-of-range strategy (upstream zstd 1..=9). The
/// inner `LDM_HASH_RLOG - (strategy / 3)` would otherwise
/// underflow `u32` for `strategy >= 24` and produce
/// nonsensical params. Regression for PR #139 round-10
/// review (Copilot).
#[test]
#[should_panic(expected = "strategy must be a upstream zstd 1..=9 ordinal")]
fn adjust_for_panics_on_out_of_range_strategy() {
    let _ = LdmParams::adjust_for(27, 24);
}

#[test]
fn adjust_strategy_to_hash_rate_log_matches_table() {
    assert_eq!(LdmParams::adjust_for(27, 1).hash_rate_log, 7);
    assert_eq!(LdmParams::adjust_for(27, 3).hash_rate_log, 6);
    assert_eq!(LdmParams::adjust_for(27, 6).hash_rate_log, 5);
    assert_eq!(LdmParams::adjust_for(27, 9).hash_rate_log, 4);
}

/// `hash_log` clamping: `BOUNDED(6, window_log - hash_rate_log, 30)`.
///
///   window_log = 27, strategy = 1 → hash_rate_log = 7
///     → window_log - hash_rate_log = 20, in range → hash_log = 20
///   window_log = 10, strategy = 1 → hash_rate_log = 7
///     → window_log - hash_rate_log = 3 < 6 → clamps to 6
///   window_log = 7,  strategy = 1 → hash_rate_log = 7
///     → window_log <= hash_rate_log → degenerate → hash_log = 6
#[test]
fn adjust_hash_log_clamps_within_bounds() {
    assert_eq!(LdmParams::adjust_for(27, 1).hash_log, 20);
    assert_eq!(LdmParams::adjust_for(10, 1).hash_log, LDM_HASHLOG_MIN);
    assert_eq!(LdmParams::adjust_for(7, 1).hash_log, LDM_HASHLOG_MIN);
}

/// `min_match_length` halving at btultra (strategy ≥ 8).
/// Upstream zstd `zstd_ldm.c:163-164`.
#[test]
fn adjust_min_match_halved_for_btultra_and_above() {
    assert_eq!(
        LdmParams::adjust_for(27, 7).min_match_length,
        LDM_MIN_MATCH_LENGTH as u32
    );
    assert_eq!(
        LdmParams::adjust_for(27, 8).min_match_length,
        (LDM_MIN_MATCH_LENGTH / 2) as u32
    );
    assert_eq!(
        LdmParams::adjust_for(27, 9).min_match_length,
        (LDM_MIN_MATCH_LENGTH / 2) as u32
    );
}

/// `bucket_size_log = BOUNDED(LDM_BUCKET_SIZE_LOG, strategy,
/// LDM_BUCKETSIZELOG_MAX)` — upstream zstd `zstd_ldm.c:168`.
/// `LDM_BUCKET_SIZE_LOG = 4`, `LDM_BUCKETSIZELOG_MAX = 8`.
#[test]
fn adjust_bucket_size_log_clamps_strategy_to_bounds() {
    // strategy 1 < lower bound 4 → clamps up
    assert_eq!(LdmParams::adjust_for(27, 1).bucket_size_log, 4);
    // strategy 4 == lower bound → identity
    assert_eq!(LdmParams::adjust_for(27, 4).bucket_size_log, 4);
    // strategy 7 in range → identity
    assert_eq!(LdmParams::adjust_for(27, 7).bucket_size_log, 7);
    // strategy 9 > upper bound 8 → clamps down
    assert_eq!(LdmParams::adjust_for(27, 9).bucket_size_log, 8);
}

/// Derived hash-table size + bucket slots agree with the
/// `1 << log` definitions and with each other.
#[test]
fn derived_helpers_match_log_definitions() {
    let p = LdmParams::adjust_for(27, 5);
    assert_eq!(p.hash_table_entries(), 1usize << p.hash_log);
    assert_eq!(p.bucket_slots(), 1usize << p.bucket_size_log);
}
