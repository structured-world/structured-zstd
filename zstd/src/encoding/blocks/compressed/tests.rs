use super::{
    FseTableMode, LastUsedTable, RawSequence, choose_table, emit_single_sequence_block,
    encode_literal_length, encode_match_len, encode_offset_with_history, min_gain,
    min_literals_to_compress, previous_table, remember_last_used_tables,
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
    let mut new_tbl = huff0_encoder::HuffmanTable::build_from_data(&skewed_literals);
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
                copy_tier: crate::decoding::simd_copy::ExactCopyTier::resolve(),
                last_huff_table: seed_table.clone(),
                huff_table_spare: None,
                huff_rollback: None,
                huff_weights: Default::default(),
                fse_tables: FseTables::new(),
                block_scratch: CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: *strat,
                pre_split: None,
                huf_optimal_search: true,
                literal_compression_disabled: false,
            };
            let mut emit_state = CompressState::<EntropyOnlyMatcher> {
                matcher: EntropyOnlyMatcher,
                copy_tier: crate::decoding::simd_copy::ExactCopyTier::resolve(),
                last_huff_table: seed_table,
                huff_table_spare: None,
                huff_rollback: None,
                huff_weights: Default::default(),
                fse_tables: FseTables::new(),
                block_scratch: CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: *strat,
                pre_split: None,
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
fn a_section_with_flat_ends_costs_what_the_emitter_writes_for_it() {
    use super::{
        CompressedBlockScratch, EntropyOnlyMatcher, EstimatorWorkspace,
        encode_block_parts_with_sequence_scratch, estimate_block_parts_size,
    };
    // The shape the end-sample shortcut exists for, and the one where the
    // estimator and the emitter can disagree: both ends look random, the
    // interior is heavily biased. A histogram of the whole section says
    // "compresses well"; the two 4 KiB end samples say "flat". The emitter
    // trusts the samples and writes a raw section, so the estimator has to
    // reach the same decision at the same point in the branch order. If it
    // prices this as Huffman-compressed instead, the splitter picks a
    // partition on a price nothing can produce.
    //
    // Sized past the shortcut's own floor (it declines to sample sections
    // smaller than ratio x sample), with each end cycling all 256 values so
    // the largest per-end count is far under the flatness bound.
    let mut literals: Vec<u8> = (0..4096u32).map(|i| (i % 256) as u8).collect();
    literals.extend(core::iter::repeat_n(0u8, 40_000));
    literals.extend((0..4096u32).map(|i| (i % 256) as u8));

    let make_state = || CompressState::<EntropyOnlyMatcher> {
        matcher: EntropyOnlyMatcher,
        copy_tier: crate::decoding::simd_copy::ExactCopyTier::resolve(),
        last_huff_table: None,
        huff_table_spare: None,
        huff_rollback: None,
        huff_weights: Default::default(),
        fse_tables: FseTables::new(),
        block_scratch: CompressedBlockScratch::new(),
        offset_hist: [1, 4, 8],
        strategy_tag: StrategyTag::Lazy,
        pre_split: None,
        huf_optimal_search: true,
        literal_compression_disabled: false,
    };
    let mut est_state = make_state();
    let mut emit_state = make_state();

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
        "estimator priced a flat-ended section at {est} bytes, emitter wrote {}",
        emitted.len(),
    );
    // Pin that this really is the raw outcome, not parity reached by both
    // sides compressing: a shortcut that stopped firing on both would keep
    // the equality above while losing what the test is about.
    assert!(
        emitted.len() >= literals.len(),
        "expected a raw literals section, got {} bytes for {} literals",
        emitted.len(),
        literals.len(),
    );
    // The shortcut clears any carried table on both sides.
    assert!(est_state.last_huff_table.is_none());
    assert!(emit_state.last_huff_table.is_none());
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
        copy_tier: crate::decoding::simd_copy::ExactCopyTier::resolve(),
        last_huff_table: None,
        huff_table_spare: None,
        huff_rollback: None,
        huff_weights: Default::default(),
        fse_tables: FseTables::new(),
        block_scratch: super::CompressedBlockScratch::new(),
        offset_hist: [10, 20, 30],
        strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
        pre_split: None,
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
        [
            LastUsedTable::Default,
            LastUsedTable::Default,
            LastUsedTable::Default,
        ],
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
    // Slots for a table these calls are expected NOT to build; the assertions
    // below are that every axis repeats instead.
    let mut ll_slot = crate::fse::fse_encoder::FSETable::blank();
    let mut ml_slot = crate::fse::fse_encoder::FSETable::blank();
    let mut of_slot = crate::fse::fse_encoder::FSETable::blank();
    let ll_repeat = choose_table(
        fse_tables.ll_previous.as_ref(),
        fse_tables.ll_default_ref(),
        sample_codes.iter().copied(),
        9,
        strat,
        &mut ll_slot,
    );
    let ml_repeat = choose_table(
        fse_tables.ml_previous.as_ref(),
        fse_tables.ml_default_ref(),
        sample_codes.iter().copied(),
        9,
        strat,
        &mut ml_slot,
    );
    let of_repeat = choose_table(
        fse_tables.of_previous.as_ref(),
        fse_tables.of_default_ref(),
        sample_codes.iter().copied(),
        8,
        strat,
        &mut of_slot,
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
        // A slot the reuse path must leave alone.
        let mut slot = crate::fse::fse_encoder::FSETable::blank();
        let mode = super::choose_table_from_counts(
            Some(&previous),
            fse_tables.ll_default_ref(),
            &mut counts,
            total,
            1, // highest non-zero code in {0,1}
            9,
            strategy,
            None,
            &mut slot,
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
        [
            LastUsedTable::Keep,
            LastUsedTable::Default,
            LastUsedTable::Default,
        ],
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
    let mut slot = crate::fse::fse_encoder::FSETable::blank();
    let mode = choose_table(
        None,
        fse_tables.ll_default_ref(),
        core::iter::repeat_n(0u8, 32),
        9,
        crate::encoding::strategy::StrategyTag::Lazy,
        &mut slot,
    );
    assert!(matches!(mode, FseTableMode::Rle(0)));
}

/// The range-match form both length coders had before they became table
/// lookups, kept as an oracle. It spells out every code's baseline and width
/// explicitly, so it is the readable statement of the format and an
/// independent check that the tables and the extra-bit masking agree with it.
fn literal_length_ranges(len: u32) -> (u8, u32, usize) {
    match len {
        0..=15 => (len as u8, 0, 0),
        16..=17 => (16, len - 16, 1),
        18..=19 => (17, len - 18, 1),
        20..=21 => (18, len - 20, 1),
        22..=23 => (19, len - 22, 1),
        24..=27 => (20, len - 24, 2),
        28..=31 => (21, len - 28, 2),
        32..=39 => (22, len - 32, 3),
        40..=47 => (23, len - 40, 3),
        48..=63 => (24, len - 48, 4),
        64..=127 => (25, len - 64, 6),
        128..=255 => (26, len - 128, 7),
        256..=511 => (27, len - 256, 8),
        512..=1023 => (28, len - 512, 9),
        1024..=2047 => (29, len - 1024, 10),
        2048..=4095 => (30, len - 2048, 11),
        4096..=8191 => (31, len - 4096, 12),
        8192..=16383 => (32, len - 8192, 13),
        16384..=32767 => (33, len - 16384, 14),
        32768..=65535 => (34, len - 32768, 15),
        65536..=131071 => (35, len - 65536, 16),
        131072.. => unreachable!(),
    }
}

fn match_len_ranges(len: u32) -> (u8, u32, usize) {
    match len {
        0..=2 => unreachable!(),
        3..=34 => (len as u8 - 3, 0, 0),
        35..=36 => (32, len - 35, 1),
        37..=38 => (33, len - 37, 1),
        39..=40 => (34, len - 39, 1),
        41..=42 => (35, len - 41, 1),
        43..=46 => (36, len - 43, 2),
        47..=50 => (37, len - 47, 2),
        51..=58 => (38, len - 51, 3),
        59..=66 => (39, len - 59, 3),
        67..=82 => (40, len - 67, 4),
        83..=98 => (41, len - 83, 4),
        99..=130 => (42, len - 99, 5),
        131..=258 => (43, len - 131, 7),
        259..=514 => (44, len - 259, 8),
        515..=1026 => (45, len - 515, 9),
        1027..=2050 => (46, len - 1027, 10),
        2051..=4098 => (47, len - 2051, 11),
        4099..=8194 => (48, len - 4099, 12),
        8195..=16386 => (49, len - 8195, 13),
        16387..=32770 => (50, len - 16387, 14),
        32771..=65538 => (51, len - 32771, 15),
        65539..=131074 => (52, len - 65539, 16),
        131075.. => unreachable!(),
    }
}

#[test]
fn literal_length_coding_agrees_with_the_ranges_over_every_length() {
    for len in 0..131_072u32 {
        assert_eq!(
            encode_literal_length(len),
            literal_length_ranges(len),
            "literal length {len}",
        );
    }
}

#[test]
fn match_length_coding_agrees_with_the_ranges_over_every_length() {
    for len in 3..131_075u32 {
        assert_eq!(
            encode_match_len(len),
            match_len_ranges(len),
            "match length {len}"
        );
    }
}

#[test]
fn choose_table_without_previous_does_not_unwrap_none() {
    let only_zero_one_table = build_table_from_symbol_counts(&[1, 1], 5, false);
    let mut slot = crate::fse::fse_encoder::FSETable::blank();
    let mode = choose_table(
        None,
        &only_zero_one_table,
        [1u8, 2].into_iter().cycle().take(32),
        5,
        crate::encoding::strategy::StrategyTag::Lazy,
        &mut slot,
    );
    assert!(matches!(mode, FseTableMode::Encoded(_)));
}
