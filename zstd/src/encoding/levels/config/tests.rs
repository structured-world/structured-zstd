use alloc::format;
use alloc::vec::Vec;

use super::resolve_level_params;
use crate::encoding::CompressionLevel;
use crate::encoding::cparams::get_cparams;
use crate::encoding::strategy::{SearchMethod, StrategyTag};

/// A dictionary prepared for itself and a dictionary loaded into the frame
/// resolve different shapes, and the loaded one is the frame's own.
///
/// A dictionary's preparation assumes a source of a few hundred bytes, since it
/// cannot know which frames will use it. That is the right guess while the
/// frame is about the dictionary and badly wrong once the frame is megabytes of
/// its own content: the tables come out sized for the dictionary and the window
/// for almost nothing. Loaded into the frame, the shape is the one the source
/// asks for, the dictionary counting only as content the window must cover.
#[test]
fn a_loaded_dictionary_resolves_the_frames_own_shape() {
    let level = CompressionLevel::Level(1);
    let sizes = crate::encoding::DictionarySizes::raw_content(64 * 1024);
    let source = 4 * 1024 * 1024u64;

    let (prepared, prepared_plan) = super::resolve_level_params_with_dict(
        level,
        Some(source),
        sizes,
        &crate::encoding::parameters::ParamOverrides::default(),
    );
    let (loaded, loaded_plan) =
        super::resolve_level_params_for_loaded_dict(level, Some(source), sizes);

    // The frame's own shape is what this level resolves for a source of this
    // size with that much dictionary content in front of it.
    let own = get_cparams(super::numeric_level(level), source, sizes.content);
    assert_eq!(loaded.window_log, own.window_log as u8);
    assert_eq!(
        loaded.fast.expect("level 1 is a Fast row").hash_log,
        own.hash_log
    );

    // And it is not the dictionary's, whose tables were sized for a source the
    // frame dwarfs. The window is the frame's under either shape; the table
    // widths are what a dictionary lends.
    let prepared_hash = prepared.fast.expect("level 1 is a Fast row").hash_log;
    assert!(
        loaded.fast.expect("level 1 is a Fast row").hash_log > prepared_hash,
        "a {source} byte source resolved hash log {} where its dictionary's shape gives {prepared_hash}",
        loaded.fast.expect("level 1 is a Fast row").hash_log,
    );

    // Nothing is searched in place: the dictionary is in the frame's tables.
    assert!(prepared_plan.is_none_or(|plan| !plan.attach) || loaded_plan.is_none());
    assert!(loaded_plan.is_none_or(|plan| !plan.attach));
}

/// The estimate is a budget figure, so it answers for whatever it is asked —
/// including a window log no encoder would accept. Shifting by one is undefined
/// past the width of the type, so an unbounded value turns a question about
/// memory into a panic; the answer is the largest window there is.
#[test]
fn a_window_log_beyond_the_encoders_own_maximum_is_bounded() {
    let huge = super::estimated_compression_workspace_bytes_for_run(
        CompressionLevel::Level(3),
        None,
        Some(64),
        false,
        None,
    );
    let largest = super::estimated_compression_workspace_bytes_for_run(
        CompressionLevel::Level(3),
        None,
        Some(30),
        false,
        None,
    );
    assert_eq!(
        huge, largest,
        "a window past the maximum is the maximum, not a shift off the end of the type"
    );
}

/// The same figure has to survive being asked on a 32-bit target, where the
/// largest long-distance table alone exceeds what a `usize` can count. An
/// estimate that wraps understates the memory it exists to bound, which is
/// worse than one that saturates: the caller is told a run fits when it cannot.
#[cfg(feature = "ldm")]
#[test]
fn the_estimate_saturates_rather_than_wrapping() {
    // The widest table the logs allow: 2^30 entries of 8 bytes plus the bucket
    // cursors. On a 64-bit host that is the exact figure; on a 32-bit one it
    // saturates. Either way it is large, where a wrap would make it small.
    let widest = crate::encoding::ldm::table::LdmHashTable::estimated_workspace_bytes(30, 8);
    assert!(
        widest >= (1usize << 30).saturating_mul(2),
        "the widest table is counted, not wrapped away: {widest}"
    );

    // And the estimate carries the table the parameters actually produce: the
    // same run costs more with long-distance matching than without it, and the
    // difference is that table rather than a wrapped total.
    let with_ldm = super::estimated_compression_workspace_bytes_for_run(
        CompressionLevel::Level(22),
        None,
        Some(30),
        true,
        None,
    );
    let without_ldm = super::estimated_compression_workspace_bytes_for_run(
        CompressionLevel::Level(22),
        None,
        Some(30),
        false,
        None,
    );
    assert!(
        with_ldm > without_ldm,
        "the matcher's table is part of the figure: {with_ldm} vs {without_ldm}"
    );
    assert!(
        without_ldm >= 1usize << 30,
        "and the window alone is already a gibibyte at this log: {without_ldm}"
    );
}

/// Parameters that override nothing are the level itself, so the estimate for
/// them is the level's own over every level and source size, with a dictionary
/// or without; and each knob that resizes what the frame builds moves it.
/// Both estimates resolve through the path the matcher's reset takes, so a
/// disagreement here is one of them building something the encoder does not.
#[test]
fn the_parameters_estimate_is_the_levels_until_a_knob_resizes_the_frame() {
    use crate::encoding::{CompressionParameters, DictionarySizes, Strategy};
    for level in -7..=22 {
        for source in [None, Some(4 << 10), Some(300 << 10), Some(64 << 20)] {
            for dictionary in [None, Some(DictionarySizes::raw_content(32 << 10))] {
                let plain = CompressionParameters::builder(CompressionLevel::Level(level))
                    .build()
                    .unwrap();
                assert_eq!(
                    super::estimated_compression_workspace_bytes_for_parameters(
                        &plain, source, dictionary
                    ),
                    super::estimated_compression_workspace_bytes_for_run(
                        CompressionLevel::Level(level),
                        source,
                        None,
                        false,
                        dictionary,
                    ),
                    "level {level}, source {source:?}, dictionary {dictionary:?}"
                );
            }
        }
    }

    let source = Some(32 << 10);
    let base = CompressionParameters::builder(CompressionLevel::Level(1))
        .build()
        .unwrap();
    let wider = CompressionParameters::builder(CompressionLevel::Level(1))
        .hash_log(16)
        .build()
        .unwrap();
    let optimal = CompressionParameters::builder(CompressionLevel::Level(1))
        .strategy(Strategy::Btultra2)
        .build()
        .unwrap();
    let estimate = |p: &CompressionParameters| {
        super::estimated_compression_workspace_bytes_for_parameters(p, source, None)
    };
    assert!(estimate(&wider) > estimate(&base), "a wider hash table");
    assert!(
        estimate(&optimal) > estimate(&base),
        "the optimal parser's tables and scratch"
    );
}

/// An override the builder accepts can ask for a table no 32-bit machine can
/// hold: `hashLog` 30 is four GiB of Fast table. The estimate has to say so
/// rather than let the shift lose its high bits and report the table as free,
/// or a memory ceiling weighed against it admits a run that cannot allocate.
/// On a 64-bit host the figure is the table's; on a 32-bit one it is pinned.
#[test]
fn a_table_wider_than_the_address_space_is_not_estimated_as_nothing() {
    use crate::encoding::CompressionParameters;
    let wide = CompressionParameters::builder(CompressionLevel::Level(1))
        .hash_log(30)
        .build()
        .unwrap();
    let estimate = super::estimated_compression_workspace_bytes_for_parameters(&wide, None, None);
    let table = (4u64 << 30).min(usize::MAX as u64) as usize;
    assert!(
        estimate >= table,
        "a 2^30-entry table is counted, not shifted away: {estimate}"
    );
}

/// Regression: a dictionary whose content cannot be indexed by the tagged
/// attach tables (Fast / Dfast position fields hold at most 2^24 bytes) is
/// primed in COPY mode, so the frame must run the CDict's verbatim table
/// geometry (`ZSTD_resetCCtx_byCopyingCDict`), not the source-capped
/// attach-mode widths a 4 KiB source would resolve: copying a 17 MiB
/// dictionary into source-sized tables collides away its matches.
#[test]
fn oversized_attach_dictionary_resolves_the_copy_geometry() {
    use crate::encoding::DictionarySizes;
    use crate::encoding::cparams::{copy_cparams, get_cdict_cparams};
    let sizes = DictionarySizes::raw_content(17 * 1024 * 1024);
    for level in [1, 3] {
        let (params, _plan) = super::resolve_level_params_with_dict(
            CompressionLevel::Level(level),
            Some(4096),
            sizes,
            &Default::default(),
        );
        let cdict = get_cdict_cparams(level, sizes.serialized, &Default::default());
        let copy = copy_cparams(
            cdict,
            u32::from(
                super::resolve_level_params(CompressionLevel::Level(level), Some(4096)).window_log,
            ),
        );
        match level {
            1 => {
                let f = params.fast.expect("fast config");
                assert_eq!(
                    f.hash_log, copy.hash_log,
                    "L1: copy-mode dictionary frame keeps the CDict hashLog"
                );
            }
            _ => {
                let d = params.dfast.expect("dfast config");
                assert_eq!(
                    (u32::from(d.long_hash_log), u32::from(d.short_hash_log)),
                    (copy.hash_log, copy.chain_log),
                    "L3: copy-mode dictionary frame keeps the CDict table geometry"
                );
            }
        }
    }
}

/// Regression: an explicit `search_log` override keeps the FULL
/// `1 << search_log` compare budget for the chain / tree finders (upstream
/// `nbAttempts`); only the row finder's per-row budget is bounded by
/// `row_log`, and it applies that bound at the search site. Capping the
/// stored depth at `1 << row_log` silently truncated e.g. `search_log(7)`
/// on btlazy2 to 64 compares.
#[test]
fn search_log_override_keeps_the_full_depth_for_chain_and_tree() {
    use crate::encoding::parameters::ParamOverrides;
    let ov = ParamOverrides {
        search_log: Some(7),
        ..Default::default()
    };
    let mut params = resolve_level_params(CompressionLevel::Level(15), Some(1 << 20));
    super::apply_param_overrides(&mut params, &ov);
    let row = params.row.expect("btlazy2 carries a row config");
    assert_eq!(row.row_log, 6, "rowLog clamps to 4..=6");
    assert_eq!(
        row.search_depth,
        1 << 7,
        "the tree walk budget is 1 << searchLog, not capped by rowLog"
    );
}

/// `targetLength` is the Fast strategy's step (upstream zstd_fast.c:
/// `stepSize = targetLength + !targetLength + 1`), so an explicit one moves
/// the step exactly as the level's own value does, and zero keeps the
/// default step.
#[test]
fn a_target_length_override_sets_the_fast_step() {
    use crate::encoding::parameters::ParamOverrides;
    for (target_length, step) in [(8, 9), (1, 2), (0, 2)] {
        let ov = ParamOverrides {
            target_length: Some(target_length),
            ..Default::default()
        };
        let mut params = resolve_level_params(CompressionLevel::Level(1), Some(1 << 20));
        super::apply_param_overrides(&mut params, &ov);
        assert_eq!(
            params.fast.expect("level 1 is a Fast row").step_size,
            step,
            "targetLength {target_length}"
        );
    }
}

/// A strategy override moves a frame onto another matcher backend, and only
/// that backend's tables are built: level 22 run as Fast holds the Fast table,
/// not the hash-chain tables its own row sized. Its estimate is therefore
/// level 1's at the same window, not the two added together.
#[test]
fn a_strategy_override_counts_only_the_backend_it_selects() {
    use crate::encoding::{CompressionParameters, Strategy};
    let as_fast = CompressionParameters::builder(CompressionLevel::Level(22))
        .strategy(Strategy::Fast)
        .build()
        .unwrap();
    let level_one = CompressionParameters::builder(CompressionLevel::Level(1))
        .window_log(27)
        .build()
        .unwrap();
    let estimate = |p: &CompressionParameters| {
        super::estimated_compression_workspace_bytes_for_parameters(p, None, None)
    };
    assert_eq!(estimate(&as_fast), estimate(&level_one));
}

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
