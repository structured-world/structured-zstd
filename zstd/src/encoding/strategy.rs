//! Encoder strategy types — Phase 3 of #111.
//!
//! Every per-position branch the encoder used to dispatch at runtime
//! (lazy / optimal split, BT walker on/off, hash3 short-match probe,
//! refined / coarse cost model) now reads from a compile-time
//! `S: Strategy` parameter. The compiler monomorphises the inner
//! loops per concrete `S` and drops the dead arms during codegen.
//!
//! ## Dispatch flow
//!
//! ```text
//! Matcher::start_matching                       // 7-arm match on StrategyTag (per block)
//!  └─ compress_block::<S>                       // S::BACKEND const match
//!      ├─ Simple/Dfast/Row                      // backends without parse_mode
//!      └─ HcMatchGenerator::start_matching_strategy::<S>
//!          ├─ S::USE_BT == false → start_matching_lazy
//!          └─ S::USE_BT == true  → start_matching_optimal::<S>
//!              ├─ HcOptimalCostProfile::const_for_strategy::<S>()
//!              ├─ should_run_btultra2_seed_pass::<S>          // const false unless S = BtUltra2
//!              └─ build_optimal_plan::<S>
//!                  └─ build_optimal_plan_impl::<S, ACC, FAV>
//!                      └─ SIMD wrapper::<S, ACC, FAV>
//!                          └─ build_optimal_plan_impl_body!(S)
//!                              ├─ S::OPT_LEVEL == 0  → abort_on_worse_match
//!                              ├─ S::OPT_LEVEL >= 2  → opt_level (refined)
//!                              └─ $collect::<S, true>
//!                                  └─ collect_optimal_candidates_initialized_body!(S)
//!                                      └─ S::USE_HASH3 → hash3 lookup (const-gated)
//! ```
//!
//! Donor parity reference: `ZSTD_compressionParameters` in
//! `lib/compress/zstd_compress_internal.h` and the per-level table in
//! `lib/compress/clevels.h`.

#![allow(dead_code)]

/// Donor `ZSTD_compressionParameters.strategy` equivalent — names the
/// concrete match-finder backend a [`Strategy`] runs on top of. The
/// runtime [`StrategyTag`] dispatcher and the [`Strategy::BACKEND`]
/// associated const both produce values of this type, so the
/// per-block driver dispatch and the per-strategy backend selection
/// stay in lock-step.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum BackendTag {
    /// `SimpleMatchGenerator` — level 1.
    Simple,
    /// `DfastMatchGenerator` — levels 2-3.
    Dfast,
    /// `RowMatchGenerator` — level 4.
    Row,
    /// `HcMatchGenerator` — levels 5-22.
    HashChain,
}

/// Compile-time encoder strategy. Each concrete implementor is a ZST
/// whose associated `const`s tell the optimal parser / match finder
/// which donor-equivalent path to execute. Hot entry points are
/// generic over `S: Strategy`, so monomorphisation strips every
/// dead `if S::FOO` arm at codegen time.
pub(crate) trait Strategy: Copy + 'static {
    /// Match-finder backend this strategy runs on.
    const BACKEND: BackendTag;

    /// Minimum match length the parser will produce.
    const MIN_MATCH: usize;

    /// `accurate` flag for [`crate::encoding::cost_model::HcOptimalCostProfile`]
    /// — enables refined statistics weighting (donor `ZSTD_btultra` and
    /// above).
    const ACCURATE_PRICE: bool;

    /// Donor "small offset bonus" toggle. Enabled for Lazy2 / BtOpt to
    /// favour decompression speed; disabled for BtUltra / BtUltra2.
    const FAVOR_SMALL_OFFSETS: bool;

    /// Compile-time gate for the donor `static (mls==3)` short-match
    /// probe inside `ZSTD_insertBtAndGetAllMatches`. Only BtUltra2
    /// drives the hash3 table today.
    const USE_HASH3: bool;

    /// Whether the optimal parser walks the BT — `false` for Lazy2,
    /// `true` for BtOpt / BtUltra / BtUltra2.
    const USE_BT: bool;

    /// Donor `optLevel` (0 = btopt, 2 = btultra / btultra2). Drives the
    /// `opt_level >= 2` price-table refinement in
    /// `build_optimal_plan_impl_body!`.
    const OPT_LEVEL: u8;

    /// Donor `max_chain_depth` for the optimal-parser cost profile.
    /// Used by `HcOptimalCostProfile::const_for_strategy::<S>()`.
    const MAX_CHAIN_DEPTH: usize;

    /// Donor `sufficient_match_len` — the BT walker bails out as soon
    /// as a candidate at or above this length is seen. `usize::MAX`
    /// means "never bail early".
    const SUFFICIENT_MATCH_LEN: usize;
}

/// Level 1 — donor `ZSTD_fast`. Single-table Simple matcher.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Fast;

impl Strategy for Fast {
    const BACKEND: BackendTag = BackendTag::Simple;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = false;
    const FAVOR_SMALL_OFFSETS: bool = true;
    const USE_HASH3: bool = false;
    const USE_BT: bool = false;
    const OPT_LEVEL: u8 = 0;
    // Optimal-parser consts are unreachable for non-BT strategies —
    // pin them to the Lazy2 row so the trait stays total.
    const MAX_CHAIN_DEPTH: usize = 8;
    const SUFFICIENT_MATCH_LEN: usize = 32;
}

/// Levels 2-3 — donor `ZSTD_dfast`. Two parallel hash chains.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Dfast;

impl Strategy for Dfast {
    const BACKEND: BackendTag = BackendTag::Dfast;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = false;
    const FAVOR_SMALL_OFFSETS: bool = true;
    const USE_HASH3: bool = false;
    const USE_BT: bool = false;
    const OPT_LEVEL: u8 = 0;
    const MAX_CHAIN_DEPTH: usize = 8;
    const SUFFICIENT_MATCH_LEN: usize = 32;
}

/// Level 4 — donor `ZSTD_greedy` with row hashing.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Greedy;

impl Strategy for Greedy {
    const BACKEND: BackendTag = BackendTag::Row;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = false;
    const FAVOR_SMALL_OFFSETS: bool = true;
    const USE_HASH3: bool = false;
    const USE_BT: bool = false;
    const OPT_LEVEL: u8 = 0;
    const MAX_CHAIN_DEPTH: usize = 8;
    const SUFFICIENT_MATCH_LEN: usize = 32;
}

/// Levels 5-15 — donor `ZSTD_lazy2` on a hash chain. Levels inside
/// the band differ only by runtime `HcConfig` fields (`search_depth`,
/// `hash_log`, `chain_log`, `target_len`, `lazy_depth`), not by
/// compile-time `Strategy` consts, so they share a single type.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Lazy;

impl Strategy for Lazy {
    const BACKEND: BackendTag = BackendTag::HashChain;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = false;
    const FAVOR_SMALL_OFFSETS: bool = true;
    const USE_HASH3: bool = false;
    const USE_BT: bool = false;
    const OPT_LEVEL: u8 = 0;
    const MAX_CHAIN_DEPTH: usize = 8;
    const SUFFICIENT_MATCH_LEN: usize = 32;
}

/// Levels 16-17 — donor `ZSTD_btopt`. BT + opt without the ultra
/// price-table refinements.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct BtOpt;

impl Strategy for BtOpt {
    const BACKEND: BackendTag = BackendTag::HashChain;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = false;
    const FAVOR_SMALL_OFFSETS: bool = true;
    const USE_HASH3: bool = false;
    const USE_BT: bool = true;
    const OPT_LEVEL: u8 = 0;
    const MAX_CHAIN_DEPTH: usize = 32;
    const SUFFICIENT_MATCH_LEN: usize = usize::MAX;
}

/// Levels 18-19 — donor `ZSTD_btultra`. BT + opt with refined price
/// tables and no small-offset bias.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct BtUltra;

impl Strategy for BtUltra {
    const BACKEND: BackendTag = BackendTag::HashChain;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = true;
    const FAVOR_SMALL_OFFSETS: bool = false;
    const USE_HASH3: bool = false;
    const USE_BT: bool = true;
    const OPT_LEVEL: u8 = 2;
    const MAX_CHAIN_DEPTH: usize = 32;
    const SUFFICIENT_MATCH_LEN: usize = usize::MAX;
}

/// Levels 20-22 — donor `ZSTD_btultra2`. BT + opt with the two-pass
/// dynamic-statistics seed and the hash3 short-match table.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct BtUltra2;

impl Strategy for BtUltra2 {
    const BACKEND: BackendTag = BackendTag::HashChain;
    const MIN_MATCH: usize = 4;
    const ACCURATE_PRICE: bool = true;
    const FAVOR_SMALL_OFFSETS: bool = false;
    const USE_HASH3: bool = true;
    const USE_BT: bool = true;
    const OPT_LEVEL: u8 = 2;
    const MAX_CHAIN_DEPTH: usize = 512;
    const SUFFICIENT_MATCH_LEN: usize = usize::MAX;
}

/// Runtime strategy tag for the per-level dispatcher. Each variant
/// maps to exactly one [`Strategy`] implementor; the dispatcher
/// itself stays runtime-tagged because it only fires once per frame
/// on `reset()`, so the cost of a 7-arm match is invisible compared
/// to the per-block hot-loop work it dispatches into.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum StrategyTag {
    Fast,
    Dfast,
    Greedy,
    Lazy,
    BtOpt,
    BtUltra,
    BtUltra2,
}

impl StrategyTag {
    /// Map a compression level (1..=22) to its [`StrategyTag`].
    ///
    /// Matches `LEVEL_TABLE` in `match_generator.rs` and the donor
    /// `clevels.h` table:
    /// * 1 → `Fast`
    /// * 2-3 → `Dfast`
    /// * 4 → `Greedy`
    /// * 5-15 → `Lazy`
    /// * 16-17 → `BtOpt`
    /// * 18-19 → `BtUltra`
    /// * 20-22 → `BtUltra2`
    pub(crate) const fn for_level(level: u8) -> Self {
        match level {
            1 => Self::Fast,
            2 | 3 => Self::Dfast,
            4 => Self::Greedy,
            5..=15 => Self::Lazy,
            16 | 17 => Self::BtOpt,
            18 | 19 => Self::BtUltra,
            _ => Self::BtUltra2,
        }
    }

    /// Map a [`CompressionLevel`] to its [`StrategyTag`]. Mirrors the
    /// per-level dispatch in `match_generator::resolve_level_params`.
    pub(crate) fn for_compression_level(level: crate::encoding::CompressionLevel) -> Self {
        use crate::encoding::CompressionLevel;
        match level {
            CompressionLevel::Uncompressed => Self::Fast,
            CompressionLevel::Fastest => Self::Fast,
            CompressionLevel::Default => Self::Dfast,
            CompressionLevel::Better => Self::Lazy,
            CompressionLevel::Best => Self::Lazy,
            CompressionLevel::Level(n) => {
                if n <= 0 {
                    if n == 0 { Self::Dfast } else { Self::Fast }
                } else {
                    let clamped = (n as u8).min(CompressionLevel::MAX_LEVEL as u8);
                    Self::for_level(clamped)
                }
            }
        }
    }

    /// Bridge to [`BackendTag`] for the dispatcher entry point.
    pub(crate) const fn backend(self) -> BackendTag {
        match self {
            Self::Fast => BackendTag::Simple,
            Self::Dfast => BackendTag::Dfast,
            Self::Greedy => BackendTag::Row,
            Self::Lazy | Self::BtOpt | Self::BtUltra | Self::BtUltra2 => BackendTag::HashChain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_strategy_matches_tag<S: Strategy>(tag: StrategyTag) {
        assert_eq!(S::BACKEND, tag.backend(), "backend mismatch");
    }

    #[test]
    fn strategy_consts_match_tag_bridge() {
        assert_strategy_matches_tag::<Fast>(StrategyTag::Fast);
        assert_strategy_matches_tag::<Dfast>(StrategyTag::Dfast);
        assert_strategy_matches_tag::<Greedy>(StrategyTag::Greedy);
        assert_strategy_matches_tag::<Lazy>(StrategyTag::Lazy);
        assert_strategy_matches_tag::<BtOpt>(StrategyTag::BtOpt);
        assert_strategy_matches_tag::<BtUltra>(StrategyTag::BtUltra);
        assert_strategy_matches_tag::<BtUltra2>(StrategyTag::BtUltra2);
    }

    #[test]
    fn level_to_tag_matches_donor_table() {
        // Spot-check every band boundary and one mid-band level.
        assert_eq!(StrategyTag::for_level(1), StrategyTag::Fast);
        assert_eq!(StrategyTag::for_level(2), StrategyTag::Dfast);
        assert_eq!(StrategyTag::for_level(3), StrategyTag::Dfast);
        assert_eq!(StrategyTag::for_level(4), StrategyTag::Greedy);
        assert_eq!(StrategyTag::for_level(5), StrategyTag::Lazy);
        assert_eq!(StrategyTag::for_level(9), StrategyTag::Lazy);
        assert_eq!(StrategyTag::for_level(15), StrategyTag::Lazy);
        assert_eq!(StrategyTag::for_level(16), StrategyTag::BtOpt);
        assert_eq!(StrategyTag::for_level(17), StrategyTag::BtOpt);
        assert_eq!(StrategyTag::for_level(18), StrategyTag::BtUltra);
        assert_eq!(StrategyTag::for_level(19), StrategyTag::BtUltra);
        assert_eq!(StrategyTag::for_level(20), StrategyTag::BtUltra2);
        assert_eq!(StrategyTag::for_level(22), StrategyTag::BtUltra2);
    }

    // The next three blocks live at module scope so the assertions
    // run at compile time and never reach the `cargo nextest` runner.
    // `clippy::assertions_on_constants` requires this form for
    // const-only inputs.

    // `use_bt_aligns_with_parse_mode`: Lazy2 strategies must not walk
    // the BT; BtOpt / BtUltra / BtUltra2 must. Invariant that lets
    // the inner optimal parser drop the `if self.parse_mode == Lazy2
    // …` branch in favour of `if !S::USE_BT`.
    const _USE_BT_LAYOUT: () = {
        assert!(!Fast::USE_BT);
        assert!(!Dfast::USE_BT);
        assert!(!Greedy::USE_BT);
        assert!(!Lazy::USE_BT);
        assert!(BtOpt::USE_BT);
        assert!(BtUltra::USE_BT);
        assert!(BtUltra2::USE_BT);
    };

    // `use_hash3_only_set_for_btultra2`: hash3 is exclusively a
    // BtUltra2 feature (donor parity).
    const _USE_HASH3_LAYOUT: () = {
        assert!(!Fast::USE_HASH3);
        assert!(!Dfast::USE_HASH3);
        assert!(!Greedy::USE_HASH3);
        assert!(!Lazy::USE_HASH3);
        assert!(!BtOpt::USE_HASH3);
        assert!(!BtUltra::USE_HASH3);
        assert!(BtUltra2::USE_HASH3);
    };

    // Mirror the per-strategy fields the optimal-parser cost profile
    // is built from, so the layout (accurate / favor_small_offsets /
    // max_chain_depth / sufficient_match_len) cannot regress
    // silently.
    const _COST_MODEL_LAYOUT: () = {
        assert!(!Lazy::ACCURATE_PRICE && Lazy::FAVOR_SMALL_OFFSETS);
        assert!(!BtOpt::ACCURATE_PRICE && BtOpt::FAVOR_SMALL_OFFSETS);
        assert!(BtUltra::ACCURATE_PRICE && !BtUltra::FAVOR_SMALL_OFFSETS);
        assert!(BtUltra2::ACCURATE_PRICE && !BtUltra2::FAVOR_SMALL_OFFSETS);
        assert!(BtOpt::MAX_CHAIN_DEPTH == 32);
        assert!(BtUltra::MAX_CHAIN_DEPTH == 32);
        assert!(BtUltra2::MAX_CHAIN_DEPTH == 512);
        assert!(BtOpt::SUFFICIENT_MATCH_LEN == usize::MAX);
        assert!(BtUltra::SUFFICIENT_MATCH_LEN == usize::MAX);
        assert!(BtUltra2::SUFFICIENT_MATCH_LEN == usize::MAX);
    };
}
