use super::*;
use crate::encoding::CompressionLevel;
use alloc::vec::Vec;

/// On a 16 KiB repeating 16-byte pattern, the encoder must emit
/// at least one `Triple` sequence — every position past the first
/// 16 bytes finds a long match 16 bytes back. Constant-run input
/// (`AAAA…`) is covered separately by
/// `rejects_constant_run_with_rle_on_wire_block` because the
/// raw/RLE detector rejects that shape at the public-API
/// boundary (different contract from the per-position match
/// capture this test pins). On the rotating pattern here we
/// want at least one captured triple, which a clean
/// matcher always produces.
#[test]
fn captures_at_least_one_triple_on_repeating_pattern() {
    let pattern: [u8; 16] = *b"PATTERN_1234_END";
    let data: Vec<u8> = pattern.iter().copied().cycle().take(16 * 1024).collect();
    let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(3));
    let seqs = &captured.sequences;
    assert!(
        !seqs.is_empty(),
        "expected at least one Triple sequence on 16KB repeating pattern, got 0",
    );
    // Every captured sequence belongs to block 0 (single 16 KiB block fits in 128 KiB).
    assert!(
        seqs.iter().all(|s| s.block_idx == 0),
        "16KB repeating pattern produced multi-block sequence stream: {:?}",
        seqs.iter().map(|s| s.block_idx).collect::<Vec<_>>(),
    );
    // Every captured Triple must reference a sane offset/match
    // (defensive: catches wrapper bugs that would leak garbage
    // through the recorder).
    for s in seqs {
        assert!(s.of >= 1, "non-positive offset captured: {:?}", s);
        assert!(s.ml >= 1, "non-positive match length captured: {:?}", s);
    }
    // seq_in_block must be contiguous 0..N for each block.
    for (i, s) in seqs.iter().enumerate() {
        assert_eq!(
            s.seq_in_block, i as u32,
            "seq_in_block discontinuity at idx {}: {:?}",
            i, seqs,
        );
    }
    // Exactly one block was emitted, so exactly one tail-length entry.
    assert_eq!(captured.block_tail_lengths.len(), 1);
    // Reconstructed cumulative position must equal the input size:
    // `Σ (ll + ml)` over triples PLUS the block's trailing-literal
    // length must reach `data.len()`. This is the alignment
    // invariant that motivated capturing tail lengths in the
    // first place (PR #149 review).
    let cumulative: u64 = seqs.iter().map(|s| s.ll as u64 + s.ml as u64).sum::<u64>()
        + captured.block_tail_lengths[0] as u64;
    assert_eq!(
        cumulative,
        data.len() as u64,
        "Σ(ll+ml) + tail must reconstruct the input length exactly: \
             seqs sum + tail {} should == input {}",
        cumulative,
        data.len(),
    );
}

/// Random / incompressible input is steered to a Raw_Block by the
/// encoder's raw fast-path classifier (`should_use_raw_for_block`
/// in `frame_compressor`), not by the late
/// `compressed.len() >= MAX_BLOCK_SIZE` fallback — 1 KiB of LCG
/// noise is well under one frame block, so the wire format
/// commits to `Raw_Block` before any sequence emission, and the
/// post-compress raw/RLE detector must panic. Before the
/// detector existed, this test verified the wrapper bounded
/// phantom-triple production; now the API-level guarantee is
/// stronger — the function refuses to return a misaligned
/// capture for such inputs (PR #149 review #25).
#[test]
#[should_panic(expected = "raw/RLE block")]
fn rejects_incompressible_input_with_raw_on_wire_block() {
    // Deterministic non-repeating bytes via a simple LCG.
    let mut state: u32 = 0x1234_5678;
    let data: Vec<u8> = (0..1024)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as u8
        })
        .collect();
    // Calling the function is the test: the raw-block detector
    // inside `compress_and_collect_sequences` must panic before
    // returning, so any code after this point is unreachable.
    let _ = compress_and_collect_sequences(&data, CompressionLevel::Level(3));
}

/// Constant runs (`[b'A'; N]`) cause the encoder to emit an
/// RLE_Block on-wire (matcher may still produce a long
/// rep-coded triple, but the block payload is encoded as
/// `(byte, regenerated_size)`, not as sequences). The raw-block
/// detector treats RLE the same as Raw — both have no on-wire
/// sequences for the comparator to diff — and must panic to
/// prevent silently misaligned output (PR #149 review #25).
#[test]
#[should_panic(expected = "raw/RLE block")]
fn rejects_constant_run_with_rle_on_wire_block() {
    let data: Vec<u8> = alloc::vec![b'A'; 16 * 1024];
    let _ = compress_and_collect_sequences(&data, CompressionLevel::Level(3));
}

/// Multi-block input (≥ 256 KiB, two 128 KiB frame blocks) is
/// the only shape that exercises per-block tail accounting
/// across a block boundary. Single-block tests pass even if a
/// regression records only the first/last block tail or
/// mis-increments `current_block` across the 128 KiB boundary.
///
/// This test pins:
///   - `Σ(ll+ml) + Σ tails == input.len()` across multiple blocks,
///   - `block_tail_lengths.len() >= 2` (at least two blocks),
///   - every recorded triple's `block_idx` falls inside the
///     emitted-block range (no off-by-one on the counter),
///   - block indices observed in triples are contiguous from 0
///     (no gaps, no missed `current_block` increments).
///
/// 200 KiB (≈ 1.5 × 128 KiB) chosen to guarantee ≥ 2 frame blocks
/// while staying small enough that the final partial block still
/// compresses (no trailing Raw_Block from a sub-threshold tail),
/// keeping the multi-block assertion crisp (PR #149 review #19).
#[test]
fn captures_multi_block_tails_and_indices() {
    // Repeating 16-byte pattern at 200 KiB (≈ 1.5 × 128 KiB
    // frame block) — guaranteed to span ≥2 blocks while
    // compressing densely enough to stay on the Compressed_Block
    // path, avoiding the raw/RLE detector at the end of
    // `compress_and_collect_sequences`. Larger / less repetitive
    // fixtures risk a trailing Raw_Block when the final chunk is
    // small enough that compression doesn't pay for itself.
    let pattern: [u8; 16] = *b"PATTERN_1234_END";
    let data: Vec<u8> = pattern.iter().copied().cycle().take(200 * 1024).collect();
    let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(3));

    // Multi-block invariant: at least 2 blocks recorded.
    assert!(
        captured.block_tail_lengths.len() >= 2,
        "expected ≥2 block tail entries for 200 KiB input, got {}: {:?}",
        captured.block_tail_lengths.len(),
        captured.block_tail_lengths,
    );

    // Cumulative reconstruction across all blocks.
    let cumulative: u64 = captured
        .sequences
        .iter()
        .map(|s| s.ll as u64 + s.ml as u64)
        .sum::<u64>()
        + captured
            .block_tail_lengths
            .iter()
            .map(|t| *t as u64)
            .sum::<u64>();
    assert_eq!(
        cumulative,
        data.len() as u64,
        "multi-block Σ(ll+ml)+Σ(tails) mismatch: got {}, want {} \
             (blocks={}, triples={})",
        cumulative,
        data.len(),
        captured.block_tail_lengths.len(),
        captured.sequences.len(),
    );

    // Every triple's block_idx must be in [0, num_blocks).
    let num_blocks = captured.block_tail_lengths.len() as u32;
    for s in &captured.sequences {
        assert!(
            s.block_idx < num_blocks,
            "triple block_idx={} out of range (num_blocks={})",
            s.block_idx,
            num_blocks,
        );
    }

    // Block indices observed in triples must be contiguous
    // starting at 0 (no skipped `current_block` increments).
    let mut max_seen: i64 = -1;
    for s in &captured.sequences {
        let idx = s.block_idx as i64;
        assert!(
            idx <= max_seen + 1,
            "non-monotonic / gapped block_idx in triple stream: \
                 max_seen={max_seen}, observed={idx}",
        );
        if idx > max_seen {
            max_seen = idx;
        }
    }
}

/// Pin the empty-input guard. Without this assert the
/// reconstruction invariant trivially passes (`0 == 0`) but
/// `block_tail_lengths` stays empty, breaking the public
/// per-emitted-block contract.
#[test]
#[should_panic(expected = "requires non-empty input")]
fn rejects_empty_input() {
    let _ = compress_and_collect_sequences(&[], CompressionLevel::Level(3));
}

/// Pin the `CompressionLevel::Uncompressed` reject path. That
/// variant routes through the encoder's raw-block fast-path
/// without invoking the matcher, leaving the recorder empty and
/// the post-compress invariant misleading.
#[test]
#[should_panic(expected = "CompressionLevel::Uncompressed")]
fn rejects_uncompressed_level() {
    let _ = compress_and_collect_sequences(
        b"hello there general kenobi",
        CompressionLevel::Uncompressed,
    );
}

/// Pin the post-split-level reject path. `Level(16..=22)` (and
/// any `Level(n > 22)` that clamps to Level 22 params) goes
/// through `compress_block_with_post_split` which emits multiple
/// physical on-wire blocks per single matcher call — the
/// per-matcher-call counter cannot track that shape.
/// `Level(11..=15)` is intentionally NOT rejected: those levels
/// pre-split via `optimal_block_size` (borders), so each
/// shrunken block is processed by its own matcher call and the
/// counter stays correct.
#[test]
#[should_panic(expected = "does not support post-split levels")]
fn rejects_post_split_numeric_level() {
    let _ =
        compress_and_collect_sequences(b"hello there general kenobi", CompressionLevel::Level(16));
}

/// `Best` is documented as "roughly equivalent to Level 11" but
/// `pre_split_level` matches the EXACT enum variants
/// (`Level(11..=15)` / `Level(16..=22)`) — the `Best` arm
/// falls through to `None`, so the named preset does NOT
/// trigger the block-splitter. The guard above
/// intentionally allows `Best` through; this test pins the
/// matcher path stays alignment-correct for it. Without it, a
/// future tightening of the guard to also reject `Best` would
/// silently break a valid capture path.
#[test]
fn captures_through_best_preset() {
    // 32 KiB of a 32-byte rotating pattern: compressible enough
    // for Best (lazy2 / btlazy2 strategy) to produce a non-empty
    // sequence stream, small enough to keep the test fast.
    let pattern: [u8; 32] = {
        let mut p = [0u8; 32];
        for (i, b) in p.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(11).wrapping_add(3);
        }
        p
    };
    let data: Vec<u8> = pattern.iter().copied().cycle().take(32 * 1024).collect();
    let captured = compress_and_collect_sequences(&data, CompressionLevel::Best);
    let cumulative: u64 = captured
        .sequences
        .iter()
        .map(|s| s.ll as u64 + s.ml as u64)
        .sum::<u64>()
        + captured
            .block_tail_lengths
            .iter()
            .map(|t| *t as u64)
            .sum::<u64>();
    assert_eq!(cumulative, data.len() as u64);
}

/// Positive coverage for the pre-split numeric range
/// (`Level(11..=15)`). The borders splitter parks the tail in
/// `pending_input` BEFORE the matcher runs, so each shrunken
/// portion gets its own matcher call and the counter stays
/// accurate. Without this test, a future tightening of the
/// guard to reject `Level(11..=15)` would regress a valid
/// capture path.
#[test]
fn captures_through_pre_split_level_15() {
    // 32 KiB of a 16-byte rotating pattern: compressible enough
    // for lazy2 (Level 11..=15 strategy) to produce a non-empty
    // sequence stream without tripping the raw/RLE detector.
    let pattern: [u8; 16] = *b"PATTERN_1234_END";
    let data: Vec<u8> = pattern.iter().copied().cycle().take(32 * 1024).collect();
    let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(15));
    let cumulative: u64 = captured
        .sequences
        .iter()
        .map(|s| s.ll as u64 + s.ml as u64)
        .sum::<u64>()
        + captured
            .block_tail_lengths
            .iter()
            .map(|t| *t as u64)
            .sum::<u64>();
    assert_eq!(cumulative, data.len() as u64);
}
