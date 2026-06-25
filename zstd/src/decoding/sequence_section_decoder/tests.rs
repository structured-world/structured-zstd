use super::*;
use alloc::vec::Vec;

/// Regression gate for the predefined FSE table cache: every cached
/// table must be byte-identical to the table the rebuild path would
/// produce on the next call. If the cache ever drifts from the
/// rebuild output (different `decode` entries, different
/// `accuracy_log`, different `offsets_long_share` for OF) the
/// dispatch in `maybe_update_fse_tables` would silently decode
/// against a stale table — the bench delta would still look fine
/// but cross-validation against the upstream zstd would diverge on the
/// next ratio gate.
#[cfg(feature = "std")]
#[test]
fn predefined_fse_caches_match_rebuild_output() {
    use crate::fse::SeqFSETable;

    let mut ll_rebuild = SeqFSETable::new(MAX_LITERAL_LENGTH_CODE);
    ll_rebuild
        .build_from_probabilities(
            LL_DEFAULT_ACC_LOG,
            &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    ll_rebuild.enrich_with_packed_seq_meta(&LL_META);
    let ll_cached = predefined_ll_table();
    assert_eq!(ll_rebuild.accuracy_log, ll_cached.accuracy_log);
    assert_eq!(ll_rebuild.decode().len(), ll_cached.decode().len());
    for (i, (a, b)) in ll_rebuild
        .decode()
        .iter()
        .zip(ll_cached.decode().iter())
        .enumerate()
    {
        assert_eq!(a.num_bits, b.num_bits, "LL entry {i} num_bits mismatch");
        assert_eq!(a.new_state, b.new_state, "LL entry {i} new_state mismatch");
        assert_eq!(
            a.base_value, b.base_value,
            "LL entry {i} base_value mismatch"
        );
        assert_eq!(
            a.num_additional_bits, b.num_additional_bits,
            "LL entry {i} num_additional_bits mismatch"
        );
    }

    let mut ml_rebuild = SeqFSETable::new(MAX_MATCH_LENGTH_CODE);
    ml_rebuild
        .build_from_probabilities(
            ML_DEFAULT_ACC_LOG,
            &Vec::from(&MATCH_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    ml_rebuild.enrich_with_packed_seq_meta(&ML_META);
    let ml_cached = predefined_ml_table();
    assert_eq!(ml_rebuild.accuracy_log, ml_cached.accuracy_log);
    assert_eq!(ml_rebuild.decode().len(), ml_cached.decode().len());
    for (i, (a, b)) in ml_rebuild
        .decode()
        .iter()
        .zip(ml_cached.decode().iter())
        .enumerate()
    {
        assert_eq!(a.num_bits, b.num_bits, "ML entry {i} num_bits mismatch");
        assert_eq!(a.new_state, b.new_state, "ML entry {i} new_state mismatch");
        assert_eq!(
            a.base_value, b.base_value,
            "ML entry {i} base_value mismatch"
        );
        assert_eq!(
            a.num_additional_bits, b.num_additional_bits,
            "ML entry {i} num_additional_bits mismatch"
        );
    }

    let mut of_rebuild = SeqFSETable::new(MAX_OFFSET_CODE);
    of_rebuild
        .build_from_probabilities(
            OF_DEFAULT_ACC_LOG,
            &Vec::from(&OFFSET_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    of_rebuild.enrich_for_offsets();
    let of_rebuild_share = compute_offsets_long_share(&of_rebuild);
    let (of_cached, of_cached_share) = predefined_of_table();
    assert_eq!(of_rebuild.accuracy_log, of_cached.accuracy_log);
    assert_eq!(of_rebuild.decode().len(), of_cached.decode().len());
    assert_eq!(
        of_rebuild_share, of_cached_share,
        "OF offsets_long_share mismatch"
    );
    for (i, (a, b)) in of_rebuild
        .decode()
        .iter()
        .zip(of_cached.decode().iter())
        .enumerate()
    {
        assert_eq!(a.num_bits, b.num_bits, "OF entry {i} num_bits mismatch");
        assert_eq!(a.new_state, b.new_state, "OF entry {i} new_state mismatch");
        assert_eq!(
            a.base_value, b.base_value,
            "OF entry {i} base_value mismatch"
        );
        assert_eq!(
            a.num_additional_bits, b.num_additional_bits,
            "OF entry {i} num_additional_bits mismatch"
        );
    }
}

#[test]
fn test_ll_default() {
    let mut table = crate::fse::SeqFSETable::new(MAX_LITERAL_LENGTH_CODE);
    table
        .build_from_probabilities(
            LL_DEFAULT_ACC_LOG,
            &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();

    assert!(table.decode().len() == 64);

    //just test a few values. TODO test all values
    assert!(table.decode()[0].num_bits == 4);
    assert!(table.decode()[0].new_state == 0);

    assert!(table.decode()[19].num_bits == 6);
    assert!(table.decode()[19].new_state == 0);

    assert!(table.decode()[39].num_bits == 4);
    assert!(table.decode()[39].new_state == 16);

    assert!(table.decode()[60].num_bits == 6);
    assert!(table.decode()[60].new_state == 0);

    assert!(table.decode()[59].num_bits == 5);
    assert!(table.decode()[59].new_state == 32);
}

#[cfg(test)]
mod offsets_long_share_tests {
    use super::super::compute_offsets_long_share;

    /// Construct a synthetic offsets [`SeqFSETable`] with the given
    /// offset code per entry. Mirrors the post-`enrich_for_offsets`
    /// shape used by `compute_offsets_long_share`: each entry's
    /// `num_additional_bits` holds the code (== source byte for
    /// `code < 32`, the only range this helper reads).
    fn synthetic_offsets_table(accuracy_log: u8, symbols: &[u8]) -> crate::fse::SeqFSETable {
        use crate::fse::SeqSymbol;
        let size = 1usize << accuracy_log;
        assert_eq!(
            symbols.len(),
            size,
            "symbols.len() must equal 1 << accuracy_log"
        );
        let mut t = crate::fse::SeqFSETable::new(31);
        t.accuracy_log = accuracy_log;
        let entries: alloc::vec::Vec<SeqSymbol> = symbols
            .iter()
            .map(|&s| SeqSymbol {
                new_state: 0,
                num_bits: 0,
                num_additional_bits: s,
                base_value: 0,
            })
            .collect();
        t.set_decode_for_test(&entries);
        t
    }

    #[test]
    fn zero_long_codes_returns_zero_share() {
        // A table with only short offset codes (all symbols <= 22).
        // Upstream zstd parity: share is the count of symbols > 22, scaled to
        // OffFSELog = 8 — with zero such symbols, share is 0
        // regardless of accuracy_log.
        for log in [3u8, 5, 6, 8] {
            let size = 1usize << log;
            let symbols: alloc::vec::Vec<u8> = (0..size).map(|i| (i as u8) % 22).collect();
            let table = synthetic_offsets_table(log, &symbols);
            assert_eq!(
                compute_offsets_long_share(&table),
                0,
                "log={log}: pure short-offset table must score 0"
            );
        }
    }

    #[test]
    fn long_codes_scale_to_offset_fse_log_reference() {
        // accuracy_log = 5 → 32-entry table. One symbol at code 23
        // (just above the threshold of 22), the rest at 0. Upstream zstd
        // scales the raw count by `OffFSELog - accuracy_log` =
        // `8 - 5 = 3`, so 1 << 3 = 8 should land at the 64-bit
        // `MIN_LONG_OFFSET_SHARE = 7` threshold (just over).
        let mut symbols = [0u8; 32];
        symbols[7] = 23;
        let table = synthetic_offsets_table(5, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 8);
    }

    #[test]
    fn raw_count_at_offset_fse_log_passes_through_unscaled() {
        // accuracy_log = OffFSELog = 8 → 256-entry table. No scaling
        // applied (shift by zero), so the share equals the raw count
        // of symbols > 22.
        let mut symbols = [0u8; 256];
        for sym in symbols.iter_mut().take(15) {
            *sym = 25;
        }
        let table = synthetic_offsets_table(8, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 15);
    }

    #[test]
    fn threshold_is_strict_greater_than() {
        // Symbol == LONG_OFFSET_CODE_THRESHOLD (22) does NOT count —
        // matches upstream zstd `> 22` strict-greater predicate. Only
        // symbols 23..MAX raise the share.
        let mut symbols = [0u8; 256];
        for sym in symbols.iter_mut().take(50) {
            *sym = 22;
        }
        let table = synthetic_offsets_table(8, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 0);
        symbols[0] = 23;
        let table = synthetic_offsets_table(8, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 1);
    }
}

#[cfg(test)]
mod compute_use_long_pipeline_tests {
    use super::super::{ADVANCE, compute_use_long_pipeline};

    /// Per-target `MIN_LONG_OFFSET_SHARE` (mirrors the cfg in
    /// `compute_use_long_pipeline`). Keep in sync with the constant
    /// in production code so the tests follow target_pointer_width.
    #[cfg(target_pointer_width = "64")]
    const MIN_SHARE: u32 = 7;
    #[cfg(not(target_pointer_width = "64"))]
    const MIN_SHARE: u32 = 20;
    const HISTORY_THRESHOLD: usize = 1 << 24;

    #[test]
    fn rejects_when_num_sequences_below_2x_advance() {
        // Below `ADVANCE * 2` (= 16): never engage the long pipeline,
        // regardless of cold-dict / history / share signals.
        assert!(!compute_use_long_pipeline(
            ADVANCE * 2 - 1,
            true,
            HISTORY_THRESHOLD + 1,
            u32::MAX
        ));
    }

    #[test]
    fn cold_dict_forces_long_pipeline_at_min_seq_count() {
        // At the `ADVANCE * 2` boundary, `ddict_is_cold == true` is a
        // sufficient override — history / share are not read.
        assert!(compute_use_long_pipeline(ADVANCE * 2, true, 0, 0));
    }

    #[test]
    fn history_at_threshold_does_not_engage() {
        // Gate uses `>` not `>=`: history exactly at threshold fails.
        assert!(!compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            HISTORY_THRESHOLD,
            MIN_SHARE,
        ));
    }

    #[test]
    fn history_just_above_threshold_engages_when_share_meets_min() {
        // Strictly above threshold + share at the per-target min: engage.
        assert!(compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            HISTORY_THRESHOLD + 1,
            MIN_SHARE,
        ));
    }

    #[test]
    fn share_below_min_blocks_engagement_even_with_large_history() {
        // Even with the share one below the per-target minimum, no engage.
        const _: () = assert!(MIN_SHARE > 0, "test invariant: MIN_SHARE > 0 required");
        assert!(!compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            HISTORY_THRESHOLD + 1,
            MIN_SHARE - 1,
        ));
    }

    #[test]
    fn saturating_history_engages_when_share_meets_min() {
        // `total_history = usize::MAX` (the saturating-add fallback path
        // from the per-tier wrappers) is well above the threshold.
        assert!(compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            usize::MAX,
            MIN_SHARE,
        ));
    }
}

#[cfg(test)]
mod init_sequence_stream_tests {
    use super::super::super::scratch::FSEScratch;
    use super::super::init_sequence_stream;
    use crate::blocks::sequence_section::SequencesHeader;
    use crate::cpu_kernel::ScalarKernel;
    use crate::decoding::decode_buffer::DecodeBuffer;
    use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError};
    use crate::decoding::ringbuffer::RingBuffer;

    /// The sequence bitstream must end with a single `1` sentinel bit in
    /// the final byte; the decoder skips trailing `0` padding looking for
    /// it. A final byte (read first, MSB-down) that is all zeros has no
    /// sentinel, so more than 8 padding bits are skipped — that must be
    /// rejected as `ExtraPadding`, not decoded.
    #[test]
    fn rejects_bitstream_with_excess_padding() {
        let mut header = SequencesHeader::new();
        // `0x01` = one sequence; `0x00` modes byte = all axes Predefined,
        // so `maybe_update_fse_tables` reads zero table bytes and the
        // whole source is the (malformed) bitstream.
        header.parse_from_header(&[0x01, 0x00]).unwrap();

        let mut fse = FSEScratch::new();
        let mut buffer = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        // All-zero bitstream: no sentinel `1` bit anywhere.
        let source = [0u8; 2];

        // `SeqStreamSetup` is not `Debug`, so assert on the `Result`
        // directly via `matches!` rather than unwrapping the Ok side.
        let result = init_sequence_stream::<RingBuffer, ScalarKernel>(
            &header,
            &source,
            &mut fse,
            &mut buffer,
            None,
        );
        assert!(
            matches!(
                result,
                Err(DecompressBlockError::DecodeSequenceError(
                    DecodeSequenceError::ExtraPadding { .. }
                ))
            ),
            "all-zero padding must be rejected as ExtraPadding"
        );
    }

    /// One-sequence, all-Predefined header. Pairs with an ample non-zero
    /// bitstream so `init_sequence_stream` returns `Ok` (the sequence loop
    /// may still error afterwards). Used to drive the per-tier monolith
    /// preambles directly: on x86_64 CI only the avx2 tier is selected at
    /// runtime, so scalar / bmi2 / vbmi2 and the K-generic impl would
    /// otherwise never execute their (shared) preamble.
    fn predefined_one_sequence_header() -> SequencesHeader {
        let mut header = SequencesHeader::new();
        header.parse_from_header(&[0x01, 0x00]).unwrap();
        header
    }

    /// Drives the always-available decoders (the portable scalar tier and
    /// the K-generic impl that backs the aarch64 NEON/SVE path) through a
    /// well-formed preamble. The result is intentionally ignored: a
    /// crafted bitstream need not yield a valid sequence, only reach and
    /// pass the preamble.
    #[test]
    fn scalar_tier_and_generic_impl_run_preamble() {
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];

        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        let _ = crate::decoding::seq_decoder_scalar::decode_and_execute_sequences_scalar(
            &header,
            &source,
            &mut fse,
            &mut buf,
            &mut offset_hist,
            &lits,
            None,
        );

        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        let _ = super::super::decode_and_execute_sequences_impl::<RingBuffer, ScalarKernel>(
            &header,
            &source,
            &mut fse,
            &mut buf,
            &mut offset_hist,
            &lits,
            None,
        );
    }

    /// Drive the BMI2 monolith preamble directly. The runtime kernel
    /// selector prefers the avx2 tier on any CPU that has BMI2, so this
    /// tier never runs through the normal dispatch on CI hardware; call it
    /// directly (guarded on the same feature it requires).
    #[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
    #[test]
    fn bmi2_tier_runs_preamble() {
        if !std::is_x86_feature_detected!("bmi2") {
            return;
        }
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];
        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        // SAFETY: BMI2 confirmed available by the runtime check above.
        let _ = unsafe {
            crate::decoding::seq_decoder_bmi2::decode_and_execute_sequences_bmi2::<RingBuffer>(
                &header,
                &source,
                &mut fse,
                &mut buf,
                &mut offset_hist,
                &lits,
                None,
            )
        };
    }

    /// Drive the AVX2 monolith preamble directly. The avx2 tier is the
    /// production path on AVX2 hardware, so this is usually also covered by
    /// the normal decode tests; the explicit call keeps the preamble
    /// covered even on a runner that lacks AVX2.
    #[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_avx2"))]
    #[test]
    fn avx2_tier_runs_preamble() {
        if !(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("bmi2")) {
            return;
        }
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];
        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        // SAFETY: AVX2 + BMI2 confirmed available by the runtime check.
        let _ = unsafe {
            crate::decoding::seq_decoder_avx2::decode_and_execute_sequences_avx2::<RingBuffer>(
                &header,
                &source,
                &mut fse,
                &mut buf,
                &mut offset_hist,
                &lits,
                None,
            )
        };
    }

    /// Drive the VBMI2 monolith preamble directly. Requires AVX-512 VBMI2,
    /// which most CI runners lack; the test self-skips there, so this tier
    /// is only covered on AVX-512 hardware.
    #[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_vbmi2"))]
    #[test]
    fn vbmi2_tier_runs_preamble() {
        // Mirror the production dispatch gate: the unsafe monolith is annotated
        // `target_feature(bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw)`, so a
        // bare `avx512vbmi2` probe could SIGILL on a CPU that reports VBMI2 but
        // lacks a companion feature. `detect_cpu_kernel() == Vbmi2` verifies the
        // full set before we call it.
        if !matches!(
            crate::cpu_kernel::detect_cpu_kernel(),
            crate::cpu_kernel::CpuKernelTag::Vbmi2
        ) {
            return;
        }
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];
        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        // SAFETY: AVX-512 VBMI2 confirmed available by the runtime check.
        let _ = unsafe {
            crate::decoding::seq_decoder_vbmi2::decode_and_execute_sequences_vbmi2::<RingBuffer>(
                &header,
                &source,
                &mut fse,
                &mut buf,
                &mut offset_hist,
                &lits,
                None,
            )
        };
    }
}
