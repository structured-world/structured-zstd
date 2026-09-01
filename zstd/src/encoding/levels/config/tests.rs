use alloc::format;
use alloc::vec::Vec;

use super::resolve_level_params;
use crate::encoding::CompressionLevel;
use crate::encoding::cparams::get_cparams;
use crate::encoding::strategy::{SearchMethod, StrategyTag};

/// The parameters the encoder actually runs for a (level, source size) pair
/// are upstream's `ZSTD_getCParams(level, size, 0)` for that pair: strategy,
/// window / hash / chain widths, search depth, minMatch and targetLength, on
/// every backend family, across the four upstream size tiers.
#[test]
fn resolved_level_params_follow_upstream_cparams_over_a_size_grid() {
    const SIZES: [u64; 8] = [
        1024,
        4096,
        10 * 1024,
        16 * 1024,
        224_787,
        1 << 20,
        1_022_035,
        100 << 20,
    ];
    let mut mismatches = Vec::new();
    for level in (-7..=22).filter(|&l| l != 0) {
        for &size in &SIZES {
            let p = resolve_level_params(CompressionLevel::Level(level), Some(size));
            let cp = get_cparams(level, size, 0);
            let mut bad = Vec::new();
            let (tag, depth, search) = match cp.strategy {
                1 => (StrategyTag::Fast, 0, SearchMethod::Fast),
                2 => (StrategyTag::Dfast, 0, SearchMethod::DoubleFast),
                3 => (StrategyTag::Greedy, 0, SearchMethod::RowHash),
                4 => (StrategyTag::Lazy, 1, SearchMethod::RowHash),
                5 => (StrategyTag::Lazy, 2, SearchMethod::RowHash),
                6 => (StrategyTag::Btlazy2, 2, SearchMethod::BinaryTreeLazy),
                7 => (StrategyTag::BtOpt, 2, SearchMethod::BinaryTree),
                8 => (StrategyTag::BtUltra, 2, SearchMethod::BinaryTree),
                9 => (StrategyTag::BtUltra2, 2, SearchMethod::BinaryTree),
                other => panic!("unknown upstream strategy {other}"),
            };
            if p.strategy_tag != tag || p.search != search {
                bad.push(format!(
                    "strategy {:?}/{:?} != upstream {} ({:?}/{:?})",
                    p.strategy_tag, p.search, cp.strategy, tag, search
                ));
            }
            if matches!(tag, StrategyTag::Lazy) && p.lazy_depth != depth {
                bad.push(format!("lazy_depth {} != {depth}", p.lazy_depth));
            }
            if u32::from(p.window_log) != cp.window_log {
                bad.push(format!("window_log {} != {}", p.window_log, cp.window_log));
            }
            let search_depth = 1usize << cp.search_log;
            match tag {
                StrategyTag::Fast => {
                    let f = p.fast.expect("fast config");
                    if f.hash_log != cp.hash_log || f.mls != cp.min_match {
                        bad.push(format!(
                            "fast hash/mls {}/{} != {}/{}",
                            f.hash_log, f.mls, cp.hash_log, cp.min_match
                        ));
                    }
                    let step = (cp.target_length as usize).max(1) + 1;
                    if f.step_size != step {
                        bad.push(format!("fast step {} != {step}", f.step_size));
                    }
                }
                StrategyTag::Dfast => {
                    let d = p.dfast.expect("dfast config");
                    if u32::from(d.long_hash_log) != cp.hash_log
                        || u32::from(d.short_hash_log) != cp.chain_log
                    {
                        bad.push(format!(
                            "dfast long/short {}/{} != {}/{}",
                            d.long_hash_log, d.short_hash_log, cp.hash_log, cp.chain_log
                        ));
                    }
                }
                StrategyTag::Greedy | StrategyTag::Lazy | StrategyTag::Btlazy2 => {
                    let r = p.row.expect("row config");
                    let expect = (
                        cp.hash_log as usize,
                        cp.chain_log as usize,
                        cp.search_log.clamp(4, 6) as usize,
                        search_depth,
                        cp.target_length as usize,
                        cp.min_match as usize,
                        tag == StrategyTag::Btlazy2,
                    );
                    let got = (
                        r.hash_bits,
                        r.chain_log,
                        r.row_log,
                        r.search_depth,
                        r.target_len,
                        r.mls,
                        r.bt,
                    );
                    if got != expect {
                        bad.push(format!(
                            "row (hash,chain,row_log,depth,target,mls,bt) {got:?} != {expect:?}"
                        ));
                    }
                }
                StrategyTag::BtOpt | StrategyTag::BtUltra | StrategyTag::BtUltra2 => {
                    let h = p.hc.expect("hc config");
                    let expect = (
                        cp.hash_log as usize,
                        cp.chain_log as usize,
                        search_depth,
                        cp.target_length as usize,
                        cp.min_match.clamp(4, 6) as usize,
                    );
                    let got = (
                        h.hash_log,
                        h.chain_log,
                        h.search_depth,
                        h.target_len,
                        h.search_mls,
                    );
                    if got != expect {
                        bad.push(format!(
                            "hc (hash,chain,depth,target,mls) {got:?} != {expect:?}"
                        ));
                    }
                }
            }
            if !bad.is_empty() {
                mismatches.push(format!("L{level} size {size}: {}", bad.join("; ")));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} (level, size) cells diverge from ZSTD_getCParams:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
