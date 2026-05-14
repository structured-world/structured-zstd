//! Encoder strategy enum + const-generic strategy dispatch (Phase 3 of
//! #111).
//!
//! Phase 1 introduced the [`HcParseMode`] runtime enum that gates the
//! Lazy2 / BtOpt / BtUltra / BtUltra2 code paths inside `HcMatchGenerator`
//! and `cost_model`. Phase 3 lifts that decision (and the parallel
//! `MatcherBackend` runtime tag in `match_generator`) into the type
//! system: each `level → Strategy` mapping is a concrete ZST that
//! implements the [`Strategy`] trait. Hot-path entry points become
//! generic over `S: Strategy` and the compiler monomorphises the inner
//! loops per variant, dropping every dead `if S::FOO` branch at
//! `codegen` time.
//!
//! The runtime enums survive until the per-call-site migration is
//! complete; this module documents the bridge.

#![allow(dead_code)]

/// Runtime dispatch tag selecting which optimal-parser / match-finder
/// pipeline `HcMatchGenerator` should execute. The cost-model profile
/// and the matcher's `start_matching` body both branch on this enum
/// until the Phase 3 `Strategy` trait replaces the runtime match.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum HcParseMode {
    Lazy2,
    BtOpt,
    BtUltra,
    BtUltra2,
}

/// Donor `ZSTD_compressionParameters.strategy` equivalent — names the
/// concrete match-finder backend a [`Strategy`] is paired with. Used in
/// the transitional bridge between the [`Strategy`] trait and the
/// existing `MatcherBackend` runtime enum.
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
/// which donor-equivalent path to execute. The runtime [`HcParseMode`]
/// enum will be removed once every read site is migrated to `if
/// S::USE_BT` / `if S::USE_HASH3` / etc.
///
/// Donor parity reference: `ZSTD_compressionParameters` in
/// `lib/compress/zstd_compress_internal.h` and the per-level table in
/// `lib/compress/clevels.h`.
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

    /// Donor `parse_mode` mirror — kept as an associated const so the
    /// transitional bridge between `Strategy` and the runtime
    /// [`HcParseMode`] enum stays cheap to write.
    const PARSE_MODE: HcParseMode;
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
    // Simple backend does not actually consult parse_mode; pick the
    // narrowest variant for the bridge.
    const PARSE_MODE: HcParseMode = HcParseMode::Lazy2;
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
    const PARSE_MODE: HcParseMode = HcParseMode::Lazy2;
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
    const PARSE_MODE: HcParseMode = HcParseMode::Lazy2;
}

/// Levels 5-15 — donor `ZSTD_lazy2` on a hash chain. Differ from each
/// other by runtime `search_depth` / `hash_log` / `chain_log` /
/// `target_len` / `lazy_depth` only — those are runtime
/// `HcConfig` fields, not compile-time `Strategy` consts.
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
    const PARSE_MODE: HcParseMode = HcParseMode::Lazy2;
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
    const PARSE_MODE: HcParseMode = HcParseMode::BtOpt;
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
    const PARSE_MODE: HcParseMode = HcParseMode::BtUltra;
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
    const PARSE_MODE: HcParseMode = HcParseMode::BtUltra2;
}

/// Compile-time strategy tag for the per-level dispatcher. Each
/// variant maps to exactly one [`Strategy`] implementor; the dispatcher
/// stays runtime-tagged because it only fires once per frame on
/// `reset()`, so the cost of a 7-arm match is invisible compared to
/// the per-block hot-loop work it dispatches into.
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
            // Out-of-range levels collapse onto BtUltra2 — the
            // existing `resolve_level_params` already clamps the
            // numeric level into the table, this is just the matching
            // tail.
            _ => Self::BtUltra2,
        }
    }

    /// Bridge to the runtime [`HcParseMode`] enum that the existing
    /// `HcMatchGenerator` paths still consume. Will go away in the
    /// final cleanup commit when the runtime enum is deleted.
    pub(crate) const fn parse_mode(self) -> HcParseMode {
        match self {
            Self::Fast | Self::Dfast | Self::Greedy | Self::Lazy => HcParseMode::Lazy2,
            Self::BtOpt => HcParseMode::BtOpt,
            Self::BtUltra => HcParseMode::BtUltra,
            Self::BtUltra2 => HcParseMode::BtUltra2,
        }
    }

    /// Bridge to the runtime [`BackendTag`] for the dispatcher entry
    /// point.
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
        assert_eq!(S::PARSE_MODE, tag.parse_mode(), "parse_mode mismatch");
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

    #[test]
    fn use_bt_aligns_with_parse_mode() {
        // Lazy2 strategies must not walk the BT; BtOpt / BtUltra /
        // BtUltra2 must. This is the invariant that lets the inner
        // optimal parser drop the `if self.parse_mode == Lazy2 …`
        // branch in favour of `if !S::USE_BT`.
        assert!(!Fast::USE_BT);
        assert!(!Dfast::USE_BT);
        assert!(!Greedy::USE_BT);
        assert!(!Lazy::USE_BT);
        assert!(BtOpt::USE_BT);
        assert!(BtUltra::USE_BT);
        assert!(BtUltra2::USE_BT);
    }

    #[test]
    fn use_hash3_only_set_for_btultra2() {
        // Donor parity: hash3 is exclusively a BtUltra2 feature.
        assert!(!Fast::USE_HASH3);
        assert!(!Dfast::USE_HASH3);
        assert!(!Greedy::USE_HASH3);
        assert!(!Lazy::USE_HASH3);
        assert!(!BtOpt::USE_HASH3);
        assert!(!BtUltra::USE_HASH3);
        assert!(BtUltra2::USE_HASH3);
    }

    #[test]
    fn accurate_and_favor_small_offsets_track_cost_model_for_mode() {
        // Mirror the `HcOptimalCostProfile::for_mode` runtime table so
        // the eventual `const_for::<S>` rewrite is a mechanical swap.
        assert!(!Lazy::ACCURATE_PRICE && Lazy::FAVOR_SMALL_OFFSETS);
        assert!(!BtOpt::ACCURATE_PRICE && BtOpt::FAVOR_SMALL_OFFSETS);
        assert!(BtUltra::ACCURATE_PRICE && !BtUltra::FAVOR_SMALL_OFFSETS);
        assert!(BtUltra2::ACCURATE_PRICE && !BtUltra2::FAVOR_SMALL_OFFSETS);
    }
}
