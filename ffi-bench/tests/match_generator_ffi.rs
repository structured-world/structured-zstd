//! Sequence-stream conformance: the level-22 (literal-length, offset,
//! match-length) triples our match generator emits must match the C
//! reference's `ZSTD_generateSequences` output on the corpus proxies.
//! Our side is captured through the `structured_zstd::testing` facade
//! (pure Rust); the C reference is produced here via the `zstd` bindings,
//! so the library crate never links them.
#![cfg(feature = "bench_internals")]

use structured_zstd::testing::collect_level22_sequences;

/// Block-delimiter pseudo-sequences (`offset == 0 && match_len == 0`) are
/// merged into the following triple's literal run, then dropped, so the
/// stream matches the facade's already-merged output.
fn merge_block_delimiters(sequences: Vec<(usize, usize, usize)>) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::with_capacity(sequences.len());
    let mut pending_lits = 0usize;
    for (lit_len, offset, match_len) in sequences {
        if offset == 0 && match_len == 0 {
            pending_lits += lit_len;
            continue;
        }
        out.push((lit_len + pending_lits, offset, match_len));
        pending_lits = 0;
    }
    out
}

fn reference_level22_sequences(data: &[u8]) -> Vec<(usize, usize, usize)> {
    use zstd::zstd_safe;
    use zstd::zstd_safe::zstd_sys;

    fn assert_zstd_ok(code: usize, context: &str) {
        assert_eq!(
            unsafe { zstd_sys::ZSTD_isError(code) },
            0,
            "{context} failed: {}",
            zstd_safe::get_error_name(code)
        );
    }

    let raw: Vec<(usize, usize, usize)> = unsafe {
        let cctx = zstd_sys::ZSTD_createCCtx();
        assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");

        assert_zstd_ok(
            zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
                22,
            ),
            "ZSTD_c_compressionLevel",
        );

        let seq_capacity = zstd_safe::sequence_bound(data.len());
        let mut seqs = vec![
            zstd_sys::ZSTD_Sequence {
                offset: 0,
                litLength: 0,
                matchLength: 0,
                rep: 0,
            };
            seq_capacity
        ];

        let seq_count = zstd_sys::ZSTD_generateSequences(
            cctx,
            seqs.as_mut_ptr(),
            seqs.len(),
            data.as_ptr().cast(),
            data.len(),
        );
        assert_zstd_ok(seq_count, "ZSTD_generateSequences");
        let rc = zstd_sys::ZSTD_freeCCtx(cctx);
        assert_eq!(rc, 0, "ZSTD_freeCCtx failed");

        seqs.truncate(seq_count);
        seqs.into_iter()
            .map(|seq| {
                (
                    seq.litLength as usize,
                    seq.offset as usize,
                    seq.matchLength as usize,
                )
            })
            .collect()
    };

    merge_block_delimiters(raw)
        .into_iter()
        .filter(|(_, offset, match_len)| *offset != 0 || *match_len != 0)
        .collect()
}

fn assert_level22_sequences_match_reference(data: &[u8]) {
    let rust = collect_level22_sequences(data);
    let reference = reference_level22_sequences(data);

    if rust != reference {
        let first_diff = rust
            .iter()
            .zip(reference.iter())
            .position(|(lhs, rhs)| lhs != rhs)
            .unwrap_or_else(|| rust.len().min(reference.len()));
        let rust_pos = rust
            .iter()
            .take(first_diff)
            .fold(0usize, |acc, seq| acc + seq.0 + seq.2);
        let ref_pos = reference
            .iter()
            .take(first_diff)
            .fold(0usize, |acc, seq| acc + seq.0 + seq.2);
        let start = first_diff.saturating_sub(4);
        let rust_window = &rust[start..rust.len().min(first_diff + 4)];
        let ref_window = &reference[start..reference.len().min(first_diff + 4)];
        panic!(
            "level22 sequence path diverged at idx {first_diff}: rust={:?} reference={:?} (rust_len={} ref_len={} rust_pos={rust_pos} ref_pos={ref_pos} rust_window={rust_window:?} ref_window={ref_window:?})",
            rust.get(first_diff),
            reference.get(first_diff),
            rust.len(),
            reference.len(),
        );
    }
}

// NOTE: the large-corpus (`z000033`, ~1 MiB) exact-sequence-parity test was
// removed. Inputs at/above the 128 KiB block size go through the block
// pre-splitter, which matches upstream's SAMPLING tier but whose match parser
// makes its own block-boundary choices, so the sequence partition legitimately
// diverges from `ZSTD_generateSequences`. Binary sequence parity is NOT a
// drop-in requirement (the crate is a drop-in replacement for libzstd, not a
// byte-for-byte encoder port); the properties that DO matter for `z000033` at
// level 22 — our output is no larger than upstream's and round-trips through
// the reference decoder —
// are pinned by `cross_validation::level22_stays_within_ffi_level22_on_corpus_proxy`
// (`level22.len() <= ffi_level22.len()`) and the cross-validation round-trip
// suite. The small-corpus case below (`z000030`, ~15 KiB, below the pre-split
// threshold) keeps the exact-parity assertion, where the partition is
// deterministic.
#[test]
fn level22_sequences_match_reference_on_small_corpus_proxy() {
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../zstd/decodecorpus_files/z000030"
    ));
    assert_level22_sequences_match_reference(data);
}
