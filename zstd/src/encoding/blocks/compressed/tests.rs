use super::{
    FseTableMode, RawSequence, choose_table, emit_single_sequence_block, encode_match_len,
    encode_offset_with_history, min_gain, min_literals_to_compress, previous_table,
    remember_last_used_tables,
};
use crate::encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable};
use crate::encoding::strategy::StrategyTag;
use crate::fse::fse_encoder::build_table_from_symbol_counts;
use crate::huff0::huff0_encoder;
use alloc::vec::Vec;

fn tables_match(
    lhs: &crate::fse::fse_encoder::FSETable,
    rhs: &crate::fse::fse_encoder::FSETable,
) -> bool {
    lhs.table_size == rhs.table_size
        && (0..=255u8)
            .all(|symbol| lhs.symbol_probability(symbol) == rhs.symbol_probability(symbol))
}

#[test]
fn repeat_offset_codes_follow_rfc_mapping() {
    let mut hist = [10, 20, 30];
    assert_eq!(encode_offset_with_history(10, 5, &mut hist), 1);
    assert_eq!(hist, [10, 20, 30]);

    let mut hist = [10, 20, 30];
    assert_eq!(encode_offset_with_history(20, 5, &mut hist), 2);
    assert_eq!(hist, [20, 10, 30]);

    let mut hist = [10, 20, 30];
    assert_eq!(encode_offset_with_history(30, 5, &mut hist), 3);
    assert_eq!(hist, [30, 10, 20]);

    let mut hist = [10, 20, 30];
    assert_eq!(encode_offset_with_history(20, 0, &mut hist), 1);
    assert_eq!(hist, [20, 10, 30]);

    let mut hist = [10, 20, 30];
    assert_eq!(encode_offset_with_history(30, 0, &mut hist), 2);
    assert_eq!(hist, [30, 10, 20]);

    let mut hist = [10, 20, 30];
    assert_eq!(encode_offset_with_history(9, 0, &mut hist), 3);
    assert_eq!(hist, [9, 10, 20]);
}

#[test]
fn min_literals_to_compress_returns_per_strategy_floor() {
    for strat in [
        StrategyTag::Fast,
        StrategyTag::Dfast,
        StrategyTag::Greedy,
        StrategyTag::Lazy,
        StrategyTag::Btlazy2,
    ] {
        assert_eq!(min_literals_to_compress(strat, false), 64);
        assert_eq!(min_literals_to_compress(strat, true), 6);
    }
    assert_eq!(min_literals_to_compress(StrategyTag::BtOpt, false), 32);
    assert_eq!(min_literals_to_compress(StrategyTag::BtOpt, true), 6);
    assert_eq!(min_literals_to_compress(StrategyTag::BtUltra, false), 16);
    assert_eq!(min_literals_to_compress(StrategyTag::BtUltra, true), 6);
    assert_eq!(min_literals_to_compress(StrategyTag::BtUltra2, false), 8);
    assert_eq!(min_literals_to_compress(StrategyTag::BtUltra2, true), 6);
}

#[test]
fn min_gain_returns_per_strategy_margin() {
    let src = 4096usize;
    for strat in [
        StrategyTag::Fast,
        StrategyTag::Dfast,
        StrategyTag::Greedy,
        StrategyTag::Lazy,
        StrategyTag::Btlazy2,
        StrategyTag::BtOpt,
    ] {
        assert_eq!(min_gain(src, strat), (src >> 6) + 2);
    }
    assert_eq!(min_gain(src, StrategyTag::BtUltra), (src >> 7) + 2);
    assert_eq!(min_gain(src, StrategyTag::BtUltra2), (src >> 8) + 2);
    assert_eq!(min_gain(0, StrategyTag::Fast), 2);
    assert_eq!(min_gain(63, StrategyTag::Fast), 2);
    assert_eq!(min_gain(64, StrategyTag::Fast), 3);
}

#[test]
fn use_raw_literal_fallback_uses_payload_vs_srcsize_threshold() {
    use super::{compressed_literals_header_bytes, use_raw_literal_fallback};
    // Upstream zstd formula: `huf_section_size >= literals_len - min_gain`,
    // payload-vs-srcSize (no headers on either side). Verify the
    // gate is symmetric in header overhead by hitting the boundary
    // where the old on-wire `total >= raw_section - mg` form would
    // have disagreed.
    let strategy = StrategyTag::Fast; // min_gain(20, Fast) = (20>>6)+2 = 2

    // literals_len = 20: raw_header = 1, compressed_header = 3.
    // New threshold (payload-vs-srcSize):
    //   keep huf iff huf_section_size <  20 - 2 = 18
    //   fallback   iff huf_section_size >= 18
    //
    // Old (regressed) threshold on-wire-vs-on-wire:
    //   total = huf_section_size + 3
    //   fallback iff total >= (20 + 1) - 2 = 19
    //                iff huf_section_size >= 16
    //
    // Gap where formulas disagree: huf_section_size in [16, 18).
    // The new formula MUST keep huf in this gap.
    let literals_len = 20usize;
    // Sanity-check the literals-header constants the math relies on.
    assert_eq!(compressed_literals_header_bytes(literals_len), 3);
    assert_eq!(super::uncompressed_literals_header_bytes(literals_len), 1);

    // Inside the gap — new keeps, old would have rejected:
    assert!(!use_raw_literal_fallback(16, literals_len, strategy));
    assert!(!use_raw_literal_fallback(17, literals_len, strategy));

    // At/above new threshold — both new and old fall back:
    assert!(use_raw_literal_fallback(18, literals_len, strategy));
    assert!(use_raw_literal_fallback(19, literals_len, strategy));

    // Below the old threshold — both keep:
    assert!(!use_raw_literal_fallback(15, literals_len, strategy));
    assert!(!use_raw_literal_fallback(0, literals_len, strategy));
}

#[test]
fn prefer_repeat_eligible_applies_gate() {
    use super::prefer_repeat_eligible;
    // Upstream zstd `zstd_compress_literals.c:165`:
    //   strategy < ZSTD_lazy && srcSize <= 1024 -> HUF_flags_preferRepeat
    // ZSTD_lazy == 4 in upstream zstd enum; our `< Lazy` set is
    // {Fast, Dfast, Greedy}. Verify the gate fires for each
    // and stays off for the rest.
    for lit_len in [0usize, 1, 64, 256, 1024] {
        assert!(
            prefer_repeat_eligible(StrategyTag::Fast, lit_len),
            "Fast/{lit_len}"
        );
        assert!(
            prefer_repeat_eligible(StrategyTag::Dfast, lit_len),
            "Dfast/{lit_len}"
        );
        assert!(
            prefer_repeat_eligible(StrategyTag::Greedy, lit_len),
            "Greedy/{lit_len}"
        );
        assert!(
            !prefer_repeat_eligible(StrategyTag::Lazy, lit_len),
            "Lazy/{lit_len}"
        );
        assert!(
            !prefer_repeat_eligible(StrategyTag::Btlazy2, lit_len),
            "Btlazy2/{lit_len}"
        );
        assert!(
            !prefer_repeat_eligible(StrategyTag::BtOpt, lit_len),
            "BtOpt/{lit_len}"
        );
        assert!(
            !prefer_repeat_eligible(StrategyTag::BtUltra, lit_len),
            "BtUltra/{lit_len}"
        );
        assert!(
            !prefer_repeat_eligible(StrategyTag::BtUltra2, lit_len),
            "BtUltra2/{lit_len}"
        );
    }
    // Above the 1024-byte size threshold the gate stays off
    // for ALL strategies (upstream zstd `srcSize <= 1024` is the
    // closed upper bound).
    for lit_len in [1025usize, 2048, 16384] {
        assert!(!prefer_repeat_eligible(StrategyTag::Fast, lit_len));
        assert!(!prefer_repeat_eligible(StrategyTag::Dfast, lit_len));
        assert!(!prefer_repeat_eligible(StrategyTag::Greedy, lit_len));
        assert!(!prefer_repeat_eligible(StrategyTag::Btlazy2, lit_len));
    }
}

#[test]
fn decide_huff_reuse_prefer_repeat_forces_reuse_for_fast_band() {
    use super::{decide_huff_reuse_like_encoder, huff0_encoder};
    // Fixture chosen so size-comparison heuristic and the
    // preferRepeat short-circuit DISAGREE: `prev` is built
    // from a broad uniform sweep so it can encode any byte;
    // the literals payload is heavily skewed (240 zeros + 16
    // outliers) so a freshly-built `new_tbl` would compress
    // strictly better than `prev`. Without preferRepeat, the
    // heuristic picks `new` (returns true); WITH preferRepeat
    // the fast-band gate forces reuse (returns false).
    // Removing the short-circuit flips Fast/Dfast/Greedy to
    // true and breaks this test — that's the regression gate.
    let prev_training: Vec<u8> = (0..1024u32).map(|i| (i % 256) as u8).collect();
    let prev = huff0_encoder::HuffmanTable::build_from_data(&prev_training);
    let mut skewed_literals: Vec<u8> = Vec::with_capacity(256);
    skewed_literals.extend(core::iter::repeat_n(0u8, 240));
    skewed_literals.extend((0..16u8).map(|i| 200 + i));
    let new_tbl = huff0_encoder::HuffmanTable::build_from_data(&skewed_literals);
    let new_desc = new_tbl
        .writeable_table_description_size()
        .expect("non-empty table emits a description");

    // Distinguishing precondition: WITHOUT preferRepeat the
    // size comparison must prefer new (else the test isn't
    // exercising the override). Verify by running with a
    // strategy outside the eligible band: Lazy returns
    // true (=new) on this fixture.
    assert!(
        decide_huff_reuse_like_encoder(
            &new_tbl,
            Some(&prev),
            new_desc,
            &skewed_literals,
            StrategyTag::Lazy,
        ),
        "fixture precondition: size-comparison must prefer new for Lazy on skewed literals"
    );

    // Eligible fast band: preferRepeat forces reuse (=false)
    // despite size-comparison preferring new.
    for strategy in [StrategyTag::Fast, StrategyTag::Dfast, StrategyTag::Greedy] {
        assert!(
            !decide_huff_reuse_like_encoder(
                &new_tbl,
                Some(&prev),
                new_desc,
                &skewed_literals,
                strategy,
            ),
            "{strategy:?} <= 1024 must short-circuit to reuse despite size-comparison favouring new"
        );
    }

    // Above the 1024-byte threshold the gate stays off even
    // for Fast/Dfast/Greedy — falls through to the size
    // heuristic, which on this fixture prefers new.
    let mut big_skewed = skewed_literals.clone();
    big_skewed.extend(core::iter::repeat_n(0u8, 1024));
    assert!(
        big_skewed.len() > 1024,
        "fixture must exceed 1024 to disable preferRepeat"
    );
    assert!(
        decide_huff_reuse_like_encoder(
            &new_tbl,
            Some(&prev),
            new_desc,
            &big_skewed,
            StrategyTag::Fast,
        ),
        "Fast at len > 1024 must NOT short-circuit (gate disabled), falls through to size heuristic"
    );
}

#[test]
fn estimator_literals_section_mirrors_emit_for_short_inputs() {
    use super::{
        CompressedBlockScratch, EntropyOnlyMatcher, EstimatorWorkspace,
        encode_block_parts_with_sequence_scratch, estimate_block_parts_size,
    };
    // For each strategy at boundary literal lengths around `min_lits`
    // and across the all-identical RLE pre-check (fires for any
    // non-empty all-identical input under every strategy/HUF state),
    // estimator's predicted size MUST equal the bytes the emitter
    // actually writes. Cases include: (a) fresh state with
    // `last_huff_table: None` covering the strategy-specific
    // `min_lits` band (8/16/32/64), (b) seeded HUF-reuse state at
    // the lowered floor of 6 — under the prior hardcoded `len >= 8`
    // gate 6/7-byte all-identical sections went raw, now they pass
    // `min_lits == 6` and the upstream zstd-parity HUF+`cLitSize==1` path
    // would route them to RLE; the pre-check shortcuts that path,
    // (c) sub-`min_lits` all-identical sections that take the
    // RLE pre-check regardless of strategy.
    type Inputs = &'static [(usize, bool)];
    let cases: &[(StrategyTag, bool, Inputs)] = &[
        // (strategy, seed_huff_reuse, [(len, all_identical)])
        (
            StrategyTag::Fast,
            false,
            &[
                (1, true), // sub-min_lits all-identical → RLE
                (5, true), // sub-min_lits all-identical → RLE
                (8, true),
                (8, false),
                (63, true),
                (63, false),
                (64, false),
            ],
        ),
        (
            StrategyTag::BtUltra2,
            false,
            &[(7, true), (7, false), (8, true), (8, false), (16, false)],
        ),
        (
            StrategyTag::BtOpt,
            false,
            &[(8, true), (31, true), (32, false)],
        ),
        // HUF reuse path: floor drops to 6. Under the prior
        // hardcoded `len >= 8` gate 6/7-byte sections went raw;
        // post-fix, all-identical 6/7-byte sections take the RLE
        // pre-check and stay byte-equivalent estimator-vs-emit
        // (upstream zstd parity would route them HUF→cLitSize==1→RLE).
        // Also exercise non-identical 6-byte raw fallback and
        // 16-byte HUF reuse path.
        (
            StrategyTag::Lazy,
            true,
            &[(6, true), (7, true), (6, false), (16, false)],
        ),
    ];

    for (strat, seed_huff, inputs) in cases {
        for (len, identical) in *inputs {
            let literals: Vec<u8> = if *identical {
                alloc::vec![0x5Au8; *len]
            } else {
                (0..*len as u8).collect()
            };
            // Seed both estimator and emit state with the same
            // synthetic HUF table when `seed_huff` is true so the
            // reuse path's `min_lits == 6` floor is exercised.
            // Counts from a varied byte sequence give a valid
            // (writeable) table that survives the `decide_huff_reuse`
            // decision when literals are large enough to consider it.
            let seed_table = if *seed_huff {
                let mut counts = [0usize; 256];
                for b in (0..=63u8).chain(64..=127u8) {
                    counts[b as usize] = 1;
                }
                Some(huff0_encoder::HuffmanTable::build_from_counts(&counts))
            } else {
                None
            };
            let mut est_state = CompressState::<EntropyOnlyMatcher> {
                matcher: EntropyOnlyMatcher,
                last_huff_table: seed_table.clone(),
                huff_table_spare: None,
                fse_tables: FseTables::new(),
                block_scratch: CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: *strat,
                huf_optimal_search: true,
                literal_compression_disabled: false,
            };
            let mut emit_state = CompressState::<EntropyOnlyMatcher> {
                matcher: EntropyOnlyMatcher,
                last_huff_table: seed_table,
                huff_table_spare: None,
                fse_tables: FseTables::new(),
                block_scratch: CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: *strat,
                huf_optimal_search: true,
                literal_compression_disabled: false,
            };
            let mut workspace = EstimatorWorkspace::default();
            let est = estimate_block_parts_size(&mut est_state, &literals, &[], &mut workspace);
            let mut emitted: Vec<u8> = Vec::new();
            let mut scratch: Vec<crate::blocks::sequence_section::Sequence> = Vec::new();
            encode_block_parts_with_sequence_scratch(
                &mut emit_state,
                &literals,
                &[],
                &mut emitted,
                &mut scratch,
            );
            assert_eq!(
                est,
                emitted.len(),
                "estimator/emit parity broken: strategy={:?} seed_huff={} len={} identical={} est={} emit={}",
                strat,
                seed_huff,
                len,
                identical,
                est,
                emitted.len(),
            );
        }
    }
}

#[test]
fn encode_match_len_uses_correct_upper_range_base() {
    assert_eq!(encode_match_len(65539), (52, 0, 16));
    assert_eq!(encode_match_len(65540), (52, 1, 16));
    assert_eq!(encode_match_len(131074), (52, 65535, 16));
}

#[test]
fn raw_partition_fallback_restores_repeat_offset_history() {
    let mut state = CompressState {
        matcher: super::EntropyOnlyMatcher,
        last_huff_table: None,
        huff_table_spare: None,
        fse_tables: FseTables::new(),
        block_scratch: super::CompressedBlockScratch::new(),
        offset_hist: [10, 20, 30],
        strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        huf_optimal_search: true,
        literal_compression_disabled: false,
    };
    let source = [0xA5; 8];
    let sequences = [RawSequence {
        ll: 0,
        ml: 5,
        offset: 20,
    }];
    let mut output = Vec::new();
    let mut compressed_scratch = Vec::new();
    let mut sequence_scratch = Vec::new();

    let mut emit_buffers = super::SingleSequenceEmitBuffers {
        output: &mut output,
        compressed: &mut compressed_scratch,
        sequence_scratch: &mut sequence_scratch,
    };
    let emitted_raw = emit_single_sequence_block(
        &mut state,
        true,
        source.len(),
        &[],
        &sequences,
        &mut emit_buffers,
    );
    if emitted_raw {
        output.extend_from_slice(&source);
    }

    assert_eq!(
        state.offset_hist,
        [10, 20, 30],
        "raw post-split fallback must not advance decoder repeat-offset history"
    );
    assert_eq!(
        (output[0] >> 1) & 0b11,
        0,
        "fixture should force the partition to fall back to a Raw block"
    );
}

#[test]
fn remember_last_used_tables_keeps_predefined_and_repeat_modes() {
    let mut fse_tables = FseTables::new();

    remember_last_used_tables(
        &mut fse_tables,
        Some(PreviousFseTable::Default),
        Some(PreviousFseTable::Default),
        Some(PreviousFseTable::Default),
    );

    assert!(tables_match(
        previous_table(fse_tables.ll_previous.as_ref(), fse_tables.ll_default_ref()).unwrap(),
        fse_tables.ll_default_ref()
    ));
    assert!(tables_match(
        previous_table(fse_tables.ml_previous.as_ref(), fse_tables.ml_default_ref()).unwrap(),
        fse_tables.ml_default_ref()
    ));
    assert!(tables_match(
        previous_table(fse_tables.of_previous.as_ref(), fse_tables.of_default_ref()).unwrap(),
        fse_tables.of_default_ref()
    ));

    let sample_codes = [0u8, 1u8];
    // Lazy is a non-fast-band strategy, so this exercises the cost-based
    // repeat decision (not the fast-band shortcut).
    let strat = crate::encoding::strategy::StrategyTag::Lazy;
    let ll_repeat = choose_table(
        fse_tables.ll_previous.as_ref(),
        fse_tables.ll_default_ref(),
        sample_codes.iter().copied(),
        9,
        strat,
    );
    let ml_repeat = choose_table(
        fse_tables.ml_previous.as_ref(),
        fse_tables.ml_default_ref(),
        sample_codes.iter().copied(),
        9,
        strat,
    );
    let of_repeat = choose_table(
        fse_tables.of_previous.as_ref(),
        fse_tables.of_default_ref(),
        sample_codes.iter().copied(),
        8,
        strat,
    );

    assert!(matches!(ll_repeat, FseTableMode::RepeatLast(_)));
    assert!(matches!(ml_repeat, FseTableMode::RepeatLast(_)));
    assert!(matches!(of_repeat, FseTableMode::RepeatLast(_)));
}

/// Fast-band strategies (fast/dfast/greedy) reuse a covering previous FSE
/// table without building a new one (upstream zstd `preferRepeat`), even on a
/// fresh distribution where the cost-based path could pick a new table.
/// A non-fast-band strategy on an identical distribution takes the
/// cost-based path instead.
#[test]
fn fast_band_strategies_prefer_repeat_fse_table() {
    use crate::encoding::strategy::StrategyTag;
    let prev = build_table_from_symbol_counts(&[8, 1], 9, false);
    let previous =
        PreviousFseTable::Custom(crate::encoding::frame_compressor::SharedFseTable::new(prev));
    let fse_tables = FseTables::new();
    // Distribution over symbols {0,1}, both covered by `previous`.
    let mut counts = [0usize; 256];
    counts[0] = 4;
    counts[1] = 6;
    let total = 10;

    // All fast-band strategies (Fast, Dfast, Greedy) unconditionally
    // reuse the covering previous table; cover every eligible arm so an
    // enum-arm regression in the implementation branch is caught.
    for strategy in [StrategyTag::Fast, StrategyTag::Dfast, StrategyTag::Greedy] {
        let mode = super::choose_table_from_counts(
            Some(&previous),
            fse_tables.ll_default_ref(),
            &mut counts,
            total,
            1, // highest non-zero code in {0,1}
            9,
            strategy,
            None,
        );
        assert!(
            matches!(mode, FseTableMode::RepeatLast(_)),
            "fast-band {strategy:?} must reuse the covering previous table",
        );
    }
}

#[test]
fn remember_last_used_tables_reuses_existing_custom_slot_for_repeat() {
    let mut fse_tables = FseTables::new();
    let custom = build_table_from_symbol_counts(&[1, 1], 5, false);
    fse_tables.ll_previous = Some(PreviousFseTable::Custom(
        crate::encoding::frame_compressor::SharedFseTable::new(custom),
    ));

    let before = core::ptr::from_ref(
        previous_table(fse_tables.ll_previous.as_ref(), fse_tables.ll_default_ref()).unwrap(),
    );

    remember_last_used_tables(
        &mut fse_tables,
        None,
        Some(PreviousFseTable::Default),
        Some(PreviousFseTable::Default),
    );

    let after = core::ptr::from_ref(
        previous_table(fse_tables.ll_previous.as_ref(), fse_tables.ll_default_ref()).unwrap(),
    );

    assert_eq!(before, after);
    assert!(matches!(
        fse_tables.ll_previous.as_ref(),
        Some(PreviousFseTable::Custom(_))
    ));
}

#[test]
fn choose_table_handles_single_symbol_distribution() {
    let fse_tables = FseTables::new();
    let mode = choose_table(
        None,
        fse_tables.ll_default_ref(),
        core::iter::repeat_n(0u8, 32),
        9,
        crate::encoding::strategy::StrategyTag::Lazy,
    );
    assert!(matches!(mode, FseTableMode::Rle(0)));
}

#[test]
fn choose_table_without_previous_does_not_unwrap_none() {
    let only_zero_one_table = build_table_from_symbol_counts(&[1, 1], 5, false);
    let mode = choose_table(
        None,
        &only_zero_one_table,
        [1u8, 2].into_iter().cycle().take(32),
        5,
        crate::encoding::strategy::StrategyTag::Lazy,
    );
    assert!(matches!(mode, FseTableMode::Encoded(_)));
}
