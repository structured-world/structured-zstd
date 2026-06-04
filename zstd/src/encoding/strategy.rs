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

/// Parse strategy — what the outer match loop does with the candidates
/// a search method produces. Orthogonal to [`SearchMethod`]: the same
/// parse can run on top of any search backend (donor decouples these as
/// `cParams.strategy`'s parse half vs the `useRowMatchFinder` switch).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParseMode {
    /// Commit the first acceptable match (donor `ZSTD_greedy`, depth 0).
    Greedy,
    /// One-position lazy lookahead (donor `ZSTD_lazy`, depth 1).
    Lazy,
    /// Two-position lazy lookahead (donor `ZSTD_lazy2`, depth 2).
    Lazy2,
    /// Optimal cost-model parse (donor `ZSTD_btopt`/`btultra`/`btultra2`).
    Optimal,
}

impl ParseMode {
    /// Lazy lookahead depth for the greedy/lazy band (0/1/2). `Optimal`
    /// has no lazy depth and returns 0.
    pub(crate) const fn lazy_depth(self) -> u8 {
        match self {
            Self::Greedy => 0,
            Self::Lazy => 1,
            Self::Lazy2 => 2,
            Self::Optimal => 0,
        }
    }

    /// Derive the greedy/lazy parse from a lazy-lookahead depth. Depth >= 2
    /// saturates to `Lazy2`. Used to read the existing `LevelParams.lazy_depth`
    /// into the decoupled parse axis.
    pub(crate) const fn from_lazy_depth(depth: u8) -> Self {
        match depth {
            0 => Self::Greedy,
            1 => Self::Lazy,
            _ => Self::Lazy2,
        }
    }
}

/// Search method — how match candidates are produced for a position.
/// Orthogonal to [`ParseMode`] (donor `cParams.strategy` search half plus
/// the `useRowMatchFinder` cParam that swaps `HashChain` for `RowHash` in
/// the greedy/lazy band).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SearchMethod {
    /// Single-table fast finder (donor `ZSTD_fast`).
    Fast,
    /// Two parallel hash tables (donor `ZSTD_dfast`).
    DoubleFast,
    /// Row-hash SIMD-tag finder (donor row matchfinder).
    RowHash,
    /// Hash-chain finder (donor `ZSTD_HcFindBestMatch`).
    HashChain,
    /// Binary-tree finder for the optimal parser (donor `ZSTD_BtGetAllMatches`).
    BinaryTree,
}

impl SearchMethod {
    /// Storage backend (matcher variant) this search method runs in.
    /// `BinaryTree` shares the `HashChain` storage (the BT scratch lives
    /// inside the hash-chain matcher, gated by `uses_bt`).
    pub(crate) const fn backend(self) -> BackendTag {
        match self {
            Self::Fast => BackendTag::Simple,
            Self::DoubleFast => BackendTag::Dfast,
            Self::RowHash => BackendTag::Row,
            Self::HashChain | Self::BinaryTree => BackendTag::HashChain,
        }
    }
}

/// Compile-time encoder strategy. Each concrete implementor is a ZST
/// whose associated `const`s tell the optimal parser / match finder
/// which donor-equivalent path to execute. Hot entry points are
/// generic over `S: Strategy`, so monomorphisation strips every
/// dead `if S::FOO` arm at codegen time.
pub(crate) trait Strategy: Copy + 'static {
    /// Match-finder backend this strategy runs on.
    const BACKEND: BackendTag;

    /// Nominal minimum match length for this strategy, mirroring the
    /// upstream `clevels.h` `searchLength` column (3 for btultra/btultra2,
    /// where the mls=3 hash3 probe is active). Descriptive: the optimal
    /// parser's actual acceptance floor is the crate-wide
    /// `HC_OPT_MIN_MATCH_LEN` (= 3) and the row matcher uses
    /// `ROW_MIN_MATCH_LEN`; this const documents the strategy's intent and
    /// is not read on the hot path.
    const MIN_MATCH: usize;

    /// `accurate` flag for [`crate::encoding::cost_model::HcOptimalCostProfile`]
    /// — enables refined statistics weighting (donor `ZSTD_btultra` and
    /// above).
    const ACCURATE_PRICE: bool;

    /// Donor "small offset bonus" toggle. Enabled for Lazy2 / BtOpt to
    /// favour decompression speed; disabled for BtUltra / BtUltra2.
    const FAVOR_SMALL_OFFSETS: bool;

    /// Compile-time gate for the `static (mls==3)` short-match probe.
    /// Both btultra and btultra2 search the hash3 table (their
    /// `clevels.h` minMatch is 3); btopt does not (minMatch >= 4).
    const USE_HASH3: bool;

    /// Whether the optimal parser runs the in-block two-pass dynamic
    /// statistics seed (upstream `ZSTD_initStats_ultra`). Only btultra2
    /// does this; btultra is single-pass even though it now shares the
    /// hash3 short-match probe. Defaults to `false` so every other
    /// strategy drops the seed-pass body at codegen time.
    const TWO_PASS_SEED: bool = false;

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
    // `MAX_CHAIN_DEPTH` / `SUFFICIENT_MATCH_LEN` are placeholder
    // values for non-BT strategies — the trait associated consts
    // must be total, but only the optimal parser reads them and
    // it runs exclusively under `S::USE_BT == true`. The
    // `debug_assert!(<S>::USE_BT, …)` guard in
    // `HcOptimalCostProfile::const_for_strategy` plus the
    // `debug_assert!(<S>::USE_BT, …)` at the top of
    // `build_optimal_plan_impl_body!` make this unreachable in
    // debug builds. Future refactors that introduce a non-BT
    // reader must add a fresh guard or replace these placeholders
    // with real Fast values.
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
    // Placeholder optimal-parser consts; see `Fast` for the
    // unreachable-by-design contract.
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
    // Placeholder optimal-parser consts; see `Fast` for the
    // unreachable-by-design contract.
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
    // Lazy is HashChain-backed but `USE_BT == false`, so the optimal
    // parser entry point is unreachable for this strategy. These
    // values mirror the donor `lazy2` cost profile (would be the
    // right defaults if a future caller did build a profile for the
    // lazy/hc path), but with no current reader the same
    // unreachable-by-design contract from `Fast` applies.
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

/// Level 18 — upstream `ZSTD_btultra`. BT + opt with refined price
/// tables and no small-offset bias; shares the mls=3 hash3 short-match
/// probe but stays single-pass (no two-pass dynamic-stats seed).
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct BtUltra;

impl Strategy for BtUltra {
    const BACKEND: BackendTag = BackendTag::HashChain;
    const MIN_MATCH: usize = 3;
    const ACCURATE_PRICE: bool = true;
    const FAVOR_SMALL_OFFSETS: bool = false;
    // clevels.h level 18 minMatch = 3 → the hash3 short-match probe is
    // active, same as btultra2. The two-pass seed stays btultra2-only
    // (TWO_PASS_SEED defaults to false here).
    const USE_HASH3: bool = true;
    const USE_BT: bool = true;
    const OPT_LEVEL: u8 = 2;
    // 1 << searchLog for level 18 (clevels.h searchLog = 6). The BT walk
    // caps compares at min(MAX_CHAIN_DEPTH, hc.search_depth); both must be
    // >= 64 for level 18 to reach upstream search depth.
    const MAX_CHAIN_DEPTH: usize = 64;
    const SUFFICIENT_MATCH_LEN: usize = usize::MAX;
}

/// Levels 19-22 — upstream `ZSTD_btultra2`. BT + opt with the two-pass
/// dynamic-statistics seed and the hash3 short-match table.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct BtUltra2;

impl Strategy for BtUltra2 {
    const BACKEND: BackendTag = BackendTag::HashChain;
    const MIN_MATCH: usize = 3;
    const ACCURATE_PRICE: bool = true;
    const FAVOR_SMALL_OFFSETS: bool = false;
    const USE_HASH3: bool = true;
    const TWO_PASS_SEED: bool = true;
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
    /// Mirrors donor `ZSTD_defaultCParameters[0]` (srcSize > 256 KiB
    /// tier) strategy column at `zstd/lib/compress/clevels.h:25-50`:
    ///
    /// * 1-2 → `Fast`
    /// * 3-4 → `Dfast`
    /// * 5 → `Greedy`
    /// * 6-15 → `Lazy` (donor splits 6/7=lazy, 8-12=lazy2,
    ///   13-15=btlazy2; we collapse all three onto our `Lazy` tag and
    ///   carry the lazy_depth variance via `LevelParams.lazy_depth`)
    /// * 16-17 → `BtOpt`
    /// * 18 → `BtUltra`
    /// * 19-22 → `BtUltra2`
    pub(crate) const fn for_level(level: u8) -> Self {
        match level {
            1 | 2 => Self::Fast,
            3 | 4 => Self::Dfast,
            5 => Self::Greedy,
            6..=15 => Self::Lazy,
            16 | 17 => Self::BtOpt,
            18 => Self::BtUltra,
            19 => Self::BtUltra2,
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
                    // Clamp in `i32` BEFORE casting to `u8`: a bare
                    // `n as u8` truncates values ≥ 256 (e.g.
                    // `Level(256)` wraps to `0`, `Level(257)` to
                    // `1`) and silently routes them to the wrong
                    // strategy. `MAX_LEVEL` (22) fits a u8 by
                    // definition, so the cast after the i32 clamp
                    // is lossless.
                    let clamped_i32 = n.clamp(1, CompressionLevel::MAX_LEVEL);
                    Self::for_level(clamped_i32 as u8)
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

    /// Default search method this tag historically ran on. Used to seed
    /// the decoupled [`SearchMethod`] axis from the legacy tag during
    /// migration; once `LEVEL_TABLE` carries explicit configs this is the
    /// fallback for tag-only call sites.
    pub(crate) const fn search(self) -> SearchMethod {
        match self {
            Self::Fast => SearchMethod::Fast,
            Self::Dfast => SearchMethod::DoubleFast,
            Self::Greedy => SearchMethod::RowHash,
            Self::Lazy => SearchMethod::HashChain,
            Self::BtOpt | Self::BtUltra | Self::BtUltra2 => SearchMethod::BinaryTree,
        }
    }

    /// Default parse mode for this tag, ignoring per-level lazy depth (the
    /// lazy band's real depth comes from `LevelParams.lazy_depth` via
    /// [`ParseMode::from_lazy_depth`]). The opt tags map to `Optimal`.
    pub(crate) const fn parse_mode(self) -> ParseMode {
        match self {
            Self::Fast | Self::Dfast | Self::Greedy => ParseMode::Greedy,
            Self::Lazy => ParseMode::Lazy,
            Self::BtOpt | Self::BtUltra | Self::BtUltra2 => ParseMode::Optimal,
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
    fn for_compression_level_clamps_oversized_numeric_levels_to_btultra2() {
        // Regression: pre-fix `Level(256)` cast `n as u8` first,
        // wrapping to `0` and routing to `Dfast`. After clamp-then-
        // cast every level above MAX_LEVEL (22) must land on
        // BtUltra2 (the saturating top of the band).
        use crate::encoding::CompressionLevel;
        assert_eq!(
            StrategyTag::for_compression_level(CompressionLevel::Level(23)),
            StrategyTag::BtUltra2,
        );
        assert_eq!(
            StrategyTag::for_compression_level(CompressionLevel::Level(255)),
            StrategyTag::BtUltra2,
        );
        assert_eq!(
            StrategyTag::for_compression_level(CompressionLevel::Level(256)),
            StrategyTag::BtUltra2,
        );
        assert_eq!(
            StrategyTag::for_compression_level(CompressionLevel::Level(257)),
            StrategyTag::BtUltra2,
        );
        assert_eq!(
            StrategyTag::for_compression_level(CompressionLevel::Level(i32::MAX)),
            StrategyTag::BtUltra2,
        );
    }

    #[test]
    fn level_to_tag_matches_donor_table() {
        // Spot-check every band boundary and one mid-band level.
        assert_eq!(StrategyTag::for_level(1), StrategyTag::Fast);
        assert_eq!(StrategyTag::for_level(2), StrategyTag::Fast);
        assert_eq!(StrategyTag::for_level(3), StrategyTag::Dfast);
        assert_eq!(StrategyTag::for_level(4), StrategyTag::Dfast);
        assert_eq!(StrategyTag::for_level(5), StrategyTag::Greedy);
        assert_eq!(StrategyTag::for_level(9), StrategyTag::Lazy);
        assert_eq!(StrategyTag::for_level(15), StrategyTag::Lazy);
        assert_eq!(StrategyTag::for_level(16), StrategyTag::BtOpt);
        assert_eq!(StrategyTag::for_level(17), StrategyTag::BtOpt);
        assert_eq!(StrategyTag::for_level(18), StrategyTag::BtUltra);
        // Donor `clevels.h` level 19 uses `ZSTD_btultra2` (searchLog 7,
        // two-pass dynamic stats + hash3), not plain btultra.
        assert_eq!(StrategyTag::for_level(19), StrategyTag::BtUltra2);
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

    // hash3 short-match probe: active for btultra + btultra2 (clevels.h
    // minMatch 3); btopt and below do not search it. The in-block
    // two-pass dynamic-stats seed is btultra2-only.
    const _USE_HASH3_LAYOUT: () = {
        assert!(!Fast::USE_HASH3);
        assert!(!Dfast::USE_HASH3);
        assert!(!Greedy::USE_HASH3);
        assert!(!Lazy::USE_HASH3);
        assert!(!BtOpt::USE_HASH3);
        assert!(BtUltra::USE_HASH3);
        assert!(BtUltra2::USE_HASH3);
        assert!(!BtOpt::TWO_PASS_SEED);
        assert!(!BtUltra::TWO_PASS_SEED);
        assert!(BtUltra2::TWO_PASS_SEED);
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
        // 1 << searchLog for clevels.h level 18 (searchLog = 6).
        assert!(BtUltra::MAX_CHAIN_DEPTH == 64);
        assert!(BtUltra2::MAX_CHAIN_DEPTH == 512);
        assert!(BtOpt::SUFFICIENT_MATCH_LEN == usize::MAX);
        assert!(BtUltra::SUFFICIENT_MATCH_LEN == usize::MAX);
        assert!(BtUltra2::SUFFICIENT_MATCH_LEN == usize::MAX);
    };
}
