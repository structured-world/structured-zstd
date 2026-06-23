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
    assert_strategy_matches_tag::<Btlazy2>(StrategyTag::Btlazy2);
    assert_strategy_matches_tag::<BtOpt>(StrategyTag::BtOpt);
    assert_strategy_matches_tag::<BtUltra>(StrategyTag::BtUltra);
    assert_strategy_matches_tag::<BtUltra2>(StrategyTag::BtUltra2);
}

/// Pin the `Btlazy2` tag's full bridge: it runs the BinaryTree finder on
/// the HashChain backend with a Lazy parse (upstream zstd `ZSTD_btlazy2`).
#[test]
fn btlazy2_tag_bridge_contract() {
    assert_eq!(StrategyTag::Btlazy2.backend(), BackendTag::HashChain);
    assert_eq!(StrategyTag::Btlazy2.search(), SearchMethod::BinaryTree);
    assert_eq!(StrategyTag::Btlazy2.parse_mode(), ParseMode::Lazy2);
    // The BT walk cap must let L15's search_depth = 64 govern (BtOpt's
    // 32 would silently halve it); full find, no early bail.
    assert_eq!(Btlazy2::MAX_CHAIN_DEPTH, 64);
    assert_eq!(Btlazy2::SUFFICIENT_MATCH_LEN, usize::MAX);
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
fn level_to_tag_matches_default_table() {
    // Spot-check every band boundary and one mid-band level.
    assert_eq!(StrategyTag::for_level(1), StrategyTag::Fast);
    assert_eq!(StrategyTag::for_level(2), StrategyTag::Fast);
    assert_eq!(StrategyTag::for_level(3), StrategyTag::Dfast);
    assert_eq!(StrategyTag::for_level(4), StrategyTag::Dfast);
    assert_eq!(StrategyTag::for_level(5), StrategyTag::Greedy);
    assert_eq!(StrategyTag::for_level(9), StrategyTag::Lazy);
    assert_eq!(StrategyTag::for_level(12), StrategyTag::Lazy);
    // Upstream zstd `clevels.h` 13-15 are `ZSTD_btlazy2` — distinct from the
    // Row-backed `Lazy` band.
    assert_eq!(StrategyTag::for_level(13), StrategyTag::Btlazy2);
    assert_eq!(StrategyTag::for_level(15), StrategyTag::Btlazy2);
    assert_eq!(StrategyTag::for_level(16), StrategyTag::BtOpt);
    assert_eq!(StrategyTag::for_level(17), StrategyTag::BtOpt);
    assert_eq!(StrategyTag::for_level(18), StrategyTag::BtUltra);
    // Upstream zstd `clevels.h` level 19 uses `ZSTD_btultra2` (searchLog 7,
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
    assert!(Btlazy2::USE_BT);
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
    assert!(!Btlazy2::USE_HASH3);
    assert!(!Btlazy2::TWO_PASS_SEED);
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
    assert!(!Btlazy2::ACCURATE_PRICE && Btlazy2::FAVOR_SMALL_OFFSETS);
    assert!(!BtOpt::ACCURATE_PRICE && BtOpt::FAVOR_SMALL_OFFSETS);
    assert!(BtUltra::ACCURATE_PRICE && !BtUltra::FAVOR_SMALL_OFFSETS);
    assert!(BtUltra2::ACCURATE_PRICE && !BtUltra2::FAVOR_SMALL_OFFSETS);
    // btlazy2 runs the full BT find (no early bail) and a search depth
    // that lets L15's configured search_depth=64 govern; see `Btlazy2`.
    assert!(Btlazy2::MAX_CHAIN_DEPTH == 64);
    assert!(Btlazy2::SUFFICIENT_MATCH_LEN == usize::MAX);
    assert!(BtOpt::MAX_CHAIN_DEPTH == 32);
    // 1 << searchLog for clevels.h level 18 (searchLog = 6).
    assert!(BtUltra::MAX_CHAIN_DEPTH == 64);
    assert!(BtUltra2::MAX_CHAIN_DEPTH == 512);
    assert!(BtOpt::SUFFICIENT_MATCH_LEN == usize::MAX);
    assert!(BtUltra::SUFFICIENT_MATCH_LEN == usize::MAX);
    assert!(BtUltra2::SUFFICIENT_MATCH_LEN == usize::MAX);
};
