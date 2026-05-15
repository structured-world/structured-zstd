//! LDM tuning parameters and donor-parity defaults.
//!
//! Direct port of `ldmParams_t` (`zstd_compress_internal.h`) and the
//! `ZSTD_ldm_adjustParameters` derivation logic (`zstd_ldm.c:135`),
//! v1.5.7.
//!
//! Donor flow:
//!
//! 1. Caller starts with a zeroed `ldmParams_t` (all auto-derive).
//! 2. [`LdmParams::adjust_for`] fills the zero-valued knobs from
//!    `windowLog` / `strategy` using the same `BOUNDED` clamps as
//!    donor.
//! 3. The returned struct then feeds [`super::gear_hash::GearHashState::new`]
//!    and the bucket hash table.

use super::gear_hash::{LDM_BUCKET_SIZE_LOG, LDM_HASH_RLOG, LDM_MIN_MATCH_LENGTH};

/// Donor `ZSTD_HASHLOG_MIN` (`zstd.h:1268`).
pub(crate) const LDM_HASHLOG_MIN: u32 = 6;
/// Donor `ZSTD_HASHLOG_MAX` (`zstd.h:1267`). Donor caps at
/// `min(WINDOWLOG_MAX, 30)`; both Rust and donor share the `30` cap
/// because no platform we target exceeds it.
pub(crate) const LDM_HASHLOG_MAX: u32 = 30;
/// Donor `ZSTD_LDM_BUCKETSIZELOG_MAX` (`zstd.h:1300`).
pub(crate) const LDM_BUCKETSIZELOG_MAX: u32 = 8;

/// Donor strategy ordinal for `ZSTD_btultra`. Triggers the
/// `minMatchLength /= 2` step in `adjust_for`. Donor `zstd.h`
/// enumerates strategies as 1..=9 in order:
/// fast, dfast, greedy, lazy, lazy2, btlazy2, btopt, btultra, btultra2.
pub(crate) const STRATEGY_BTULTRA: u32 = 8;

/// LDM parameter set fed to both the gear-hash producer and the
/// bucket hash table. Field semantics mirror donor `ldmParams_t`.
///
/// All-zero `LdmParams::default()` is the donor sentinel for
/// "auto-derive every knob"; [`Self::adjust_for`] performs that
/// derivation.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LdmParams {
    /// Compression `windowLog` (mirrors `cParams->windowLog`). The
    /// LDM window size in bytes is `1 << window_log`.
    pub(crate) window_log: u32,
    /// `log2` of the LDM hash-table size (in entries). Donor name:
    /// `hashLog`. Clamped to `[LDM_HASHLOG_MIN, LDM_HASHLOG_MAX]`.
    pub(crate) hash_log: u32,
    /// `log2` of bytes between successive split-point checks (on
    /// average). Donor name: `hashRateLog`. Defaults derived from
    /// strategy.
    pub(crate) hash_rate_log: u32,
    /// Minimum accepted LDM match length in bytes. Donor name:
    /// `minMatchLength`. Defaults to [`LDM_MIN_MATCH_LENGTH`], halved
    /// for `>= btultra`.
    pub(crate) min_match_length: u32,
    /// `log2` of the per-bucket entry count. Donor name:
    /// `bucketSizeLog`. Clamped to
    /// `[LDM_BUCKET_SIZE_LOG, LDM_BUCKETSIZELOG_MAX]`.
    pub(crate) bucket_size_log: u32,
}

impl LdmParams {
    /// Derive a complete parameter set from caller-supplied
    /// `window_log` + `strategy` using donor `ZSTD_ldm_adjustParameters`
    /// (`zstd_ldm.c:135`). Strategy is the donor 1..=9 enum ordinal
    /// (fast=1 .. btultra2=9).
    ///
    /// Returns a fully-initialised `LdmParams` with no zero fields.
    pub(crate) fn adjust_for(window_log: u32, strategy: u32) -> Self {
        debug_assert!(
            (1..=9).contains(&strategy),
            "strategy must be a donor 1..=9 ordinal (donor asserts at \
             zstd_ldm.c:149 + zstd_ldm.c:167)"
        );

        let mut params = Self {
            window_log,
            ..Self::default()
        };

        // hash_rate_log: donor `zstd_ldm.c:141-153`. With `hash_log`
        // still zero (the only path we expose), donor falls into the
        // `7 - strategy/3` branch.
        if params.hash_rate_log == 0 {
            if params.hash_log > 0 && params.window_log > params.hash_log {
                params.hash_rate_log = params.window_log - params.hash_log;
            } else {
                // Map [fast=1, rate 7] .. [btultra2=9, rate 4].
                // Donor `zstd_ldm.c:151`.
                params.hash_rate_log = LDM_HASH_RLOG - (strategy / 3);
            }
        }

        // hash_log: donor `zstd_ldm.c:154-160`.
        if params.hash_log == 0 {
            params.hash_log = if params.window_log <= params.hash_rate_log {
                LDM_HASHLOG_MIN
            } else {
                bounded(
                    LDM_HASHLOG_MIN,
                    params.window_log - params.hash_rate_log,
                    LDM_HASHLOG_MAX,
                )
            };
        }

        // min_match_length: donor `zstd_ldm.c:161-165`.
        if params.min_match_length == 0 {
            let mut mml = LDM_MIN_MATCH_LENGTH as u32;
            if strategy >= STRATEGY_BTULTRA {
                mml /= 2;
            }
            params.min_match_length = mml;
        }

        // bucket_size_log: donor `zstd_ldm.c:166-169`.
        if params.bucket_size_log == 0 {
            params.bucket_size_log = bounded(LDM_BUCKET_SIZE_LOG, strategy, LDM_BUCKETSIZELOG_MAX);
        }

        params
    }

    /// Hash-table size in entries: `1 << hash_log`.
    pub(crate) const fn hash_table_entries(&self) -> usize {
        1usize << self.hash_log
    }

    /// Per-bucket slot count: `1 << bucket_size_log`.
    pub(crate) const fn bucket_slots(&self) -> usize {
        1usize << self.bucket_size_log
    }
}

/// Donor `BOUNDED(lo, val, hi)` macro: clamp `val` into `[lo, hi]`.
/// Equivalent to `val.clamp(lo, hi)` but kept as a free function so
/// the donor-line citations read naturally.
#[inline]
const fn bounded(lo: u32, val: u32, hi: u32) -> u32 {
    if val < lo {
        lo
    } else if val > hi {
        hi
    } else {
        val
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-check donor strategy → hash_rate_log mapping
    /// (`zstd_ldm.c:151`): `7 - strategy/3`.
    ///
    ///   strategy 1 (fast)     → 7
    ///   strategy 3 (greedy)   → 6
    ///   strategy 6 (btlazy2)  → 5
    ///   strategy 9 (btultra2) → 4
    #[test]
    fn adjust_strategy_to_hash_rate_log_matches_donor_table() {
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
    fn adjust_hash_log_clamps_within_donor_bounds() {
        assert_eq!(LdmParams::adjust_for(27, 1).hash_log, 20);
        assert_eq!(LdmParams::adjust_for(10, 1).hash_log, LDM_HASHLOG_MIN);
        assert_eq!(LdmParams::adjust_for(7, 1).hash_log, LDM_HASHLOG_MIN);
    }

    /// `min_match_length` halving at btultra (strategy ≥ 8).
    /// Donor `zstd_ldm.c:163-164`.
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
    /// LDM_BUCKETSIZELOG_MAX)` — donor `zstd_ldm.c:168`.
    /// `LDM_BUCKET_SIZE_LOG = 4`, `LDM_BUCKETSIZELOG_MAX = 8`.
    #[test]
    fn adjust_bucket_size_log_clamps_strategy_to_donor_bounds() {
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
}
