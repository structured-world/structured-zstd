//! Bench-only sequence-stream capture for FFI-parity audits.
//!
//! Exposed under the `bench_internals` feature so the regular crate API
//! surface stays unaffected. The single public entry point —
//! [`compress_and_collect_sequences`] — drives the production
//! [`FrameCompressor`] pipeline at the requested `CompressionLevel` and
//! records every `Sequence::Triple` the matcher emits (tagged with its
//! block index) plus the trailing-literal length of every block so
//! callers can walk a cumulative position counter that matches
//! on-wire byte consumption. This is the Rust-side input to the
//! `compare_ffi_sequences` bench, which emits raw
//! `Equal` / `Differ` / `RustOnly` / `FfiOnly` verdicts over which a
//! human triages residual ratio deltas into interpretation classes
//! ("algorithmic win" / "cost source" / "missed match" —
//! `Phase 7 / 7-tooling-seq-cmp`). The interpretation labels are
//! human-applied reasoning on top of the raw verdicts; this module
//! and its consumer bench only produce the data, not the labels.
//!
//! Implementation goes through [`FrameCompressor::new_with_matcher`] +
//! a [`CapturingMatcher`] wrapper rather than driving the matcher in
//! isolation, so the captured stream reflects block-splitter decisions,
//! strategy-tag selection and per-level resets exactly as the
//! production encoder would emit them. Capturing the matcher in
//! isolation would skip the frame-level chunking and produce a stream
//! that does NOT match what the on-wire frame encodes.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::encoding::{CompressionLevel, FrameCompressor, MatchGeneratorDriver, Matcher, Sequence};

/// One sequence captured from the encoder's matcher output, in
/// "raw" form (offset is the actual byte distance, NOT the wire-format
/// offset code with rep-history shift).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedRawSequence {
    /// Zero-based index of the block this sequence belongs to.
    pub block_idx: u32,
    /// Zero-based position within the block (resets at block boundary).
    pub seq_in_block: u32,
    /// Literal length in bytes that precede the match copy.
    pub ll: u32,
    /// Byte distance to copy from (1-based, matches the matcher's
    /// `Sequence::Triple.offset` semantics — NOT the encoded `of` code).
    pub of: u32,
    /// Match length in bytes.
    pub ml: u32,
}

/// Combined result of one capture run.
///
/// `sequences` holds every `Sequence::Triple` the matcher emitted, in
/// input order. `block_tail_lengths` holds one entry per emitted block
/// (matcher call to `start_matching` / `skip_matching` /
/// `skip_matching_with_hint`) with the count of trailing literal bytes
/// for that block — i.e. the bytes between the last triple's
/// (literals + match) span and the block end. Callers that walk a
/// cumulative position counter across the whole frame
/// (`Σ (ll + ml)` per triple, plus `block_tail_lengths[block]` at each
/// block boundary) get a position that matches the on-wire bytes
/// consumed; without the tail counts a block with trailing literals
/// would silently undercount and shift every subsequent comparison.
#[derive(Clone, Debug, Default)]
pub struct SequenceCapture {
    /// Triple sequences, one per `Sequence::Triple` event in input order.
    pub sequences: Vec<CapturedRawSequence>,
    /// Trailing-literal length per emitted block, indexed by block_idx.
    /// Contains one entry per block the matcher saw, INCLUDING blocks
    /// that emitted zero `Sequence::Triple` events (e.g. fully-literal
    /// blocks routed through `start_matching` with only a terminal
    /// `Sequence::Literals` event, or raw blocks routed through
    /// `skip_matching` / `skip_matching_with_hint`). The vec length is
    /// therefore the total number of blocks processed, which may
    /// exceed `sequences.last().map(|s| s.block_idx + 1).unwrap_or(0)`
    /// whenever any trailing block emitted no triples.
    pub block_tail_lengths: Vec<u32>,
}

/// `Matcher` wrapper that forwards every method to an inner
/// [`MatchGeneratorDriver`] while appending each emitted
/// `Sequence::Triple` to a shared recorder and the per-block
/// trailing-literal length to a parallel vec. Shared `Rc<RefCell<…>>`
/// lets the caller pull captured state out without consuming the
/// `FrameCompressor` mid-frame.
struct CapturingMatcher {
    inner: MatchGeneratorDriver,
    recorded: Rc<RefCell<Vec<CapturedRawSequence>>>,
    block_tail_lengths: Rc<RefCell<Vec<u32>>>,
    current_block: u32,
}

impl Matcher for CapturingMatcher {
    fn get_next_space(&mut self) -> Vec<u8> {
        self.inner.get_next_space()
    }

    fn get_last_space(&mut self) -> &[u8] {
        self.inner.get_last_space()
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        self.inner.commit_space(space);
    }

    fn skip_matching(&mut self) {
        // No-triple block path (raw / RLE / hint-driven fast paths
        // routed through the matcher trait): every byte of the
        // committed space is "trailing literals" from the alignment
        // perspective — no triples, just bytes flowing through.
        // Read `get_last_space().len()` BEFORE forwarding so we don't
        // race the inner state machine, which may consume the buffer.
        let tail_ll = self.inner.get_last_space().len() as u32;
        self.inner.skip_matching();
        self.block_tail_lengths.borrow_mut().push(tail_ll);
        self.current_block = self.current_block.saturating_add(1);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        // Same accounting as `skip_matching`. The hint variant is
        // taken on both the incompressible/raw-block path AND the
        // RLE fast-path for constant runs that the block-emit layer
        // catches; in either case no triples are produced and the
        // entire committed space is trailing literals from the
        // alignment perspective.
        let tail_ll = self.inner.get_last_space().len() as u32;
        self.inner.skip_matching_with_hint(incompressible_hint);
        self.block_tail_lengths.borrow_mut().push(tail_ll);
        self.current_block = self.current_block.saturating_add(1);
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        let recorded = self.recorded.clone();
        let block_idx = self.current_block;
        let mut seq_in_block: u32 = 0;
        // `Sequence::Literals` is emitted as the last event of a block
        // (per the `Matcher` trait doc) and carries the bytes between
        // the final triple and the block end. If no triple is emitted
        // for this block (rare but possible — e.g. fully-literal block
        // routed through `start_matching` instead of `skip_matching`)
        // the closure may see only a `Literals` event with the whole
        // block's bytes. If the matcher emits no `Literals` event at
        // all (block whose last triple consumes exactly to the block
        // boundary) the default `0` is correct.
        let mut block_tail_ll: u32 = 0;
        self.inner.start_matching(|seq| {
            match seq {
                Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                } => {
                    recorded.borrow_mut().push(CapturedRawSequence {
                        block_idx,
                        seq_in_block,
                        ll: literals.len() as u32,
                        of: offset as u32,
                        ml: match_len as u32,
                    });
                    seq_in_block = seq_in_block.saturating_add(1);
                }
                Sequence::Literals { literals } => {
                    block_tail_ll = literals.len() as u32;
                }
            }
            handle_sequence(seq);
        });
        self.block_tail_lengths.borrow_mut().push(block_tail_ll);
        self.current_block = self.current_block.saturating_add(1);
    }

    fn reset(&mut self, level: CompressionLevel) {
        self.inner.reset(level);
        self.recorded.borrow_mut().clear();
        self.block_tail_lengths.borrow_mut().clear();
        self.current_block = 0;
    }

    fn set_source_size_hint(&mut self, size: u64) {
        self.inner.set_source_size_hint(size);
    }

    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        self.inner.prime_with_dictionary(dict_content, offset_hist);
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        self.inner.seed_dictionary_entropy(huff, ll, ml, of);
    }

    fn supports_dictionary_priming(&self) -> bool {
        self.inner.supports_dictionary_priming()
    }

    fn window_size(&self) -> u64 {
        self.inner.window_size()
    }
}

/// Compress `input` at `level` through the production
/// [`FrameCompressor`] pipeline and return every emitted
/// `Sequence::Triple` plus per-block trailing-literal counts as a
/// [`SequenceCapture`].
///
/// The compressed output is discarded — only matcher metadata is
/// returned. Use this from a benchmark or audit tool to diff the
/// Rust-emitted sequence stream against a donor FFI side
/// (`ZSTD_generateSequences`) for the same `(input, level)` pair.
///
/// Trailing-literal lengths are captured per block via the matcher's
/// terminal `Sequence::Literals` event (or the entire committed space
/// for `skip_matching` blocks) and surfaced separately so callers
/// walking a cumulative `Σ (ll + ml)` position counter across the
/// whole frame can apply the tail length at each block boundary.
/// Without this, a block with trailing literals would silently
/// undercount and shift every subsequent comparison — `Literals`
/// events were initially dropped from the recorder and the resulting
/// alignment loss showed up as spurious `RustOnly` / `FfiOnly` noise
/// on multi-block fixtures.
///
/// # Known limitation: raw-fallback blocks
///
/// The capture records what the matcher PRODUCED, not what the
/// encoder eventually WROTE on-wire. `compress_block_encoded` may
/// fall back to a raw block when `compressed.len() >= MAX_BLOCK_SIZE`
/// (compression made the block bigger than the source). In that
/// case the matcher's triples are recorded in the capture but the
/// on-wire frame contains no sequences for that block, so the
/// comparator will report spurious `RustOnly` rows. The wrapper
/// has no way to observe the raw-fallback decision from inside the
/// `Matcher` trait — fixing this would require hooking the encoder
/// downstream of the matcher pass, which is out of scope for this
/// bench-only tool. Triage the recorded stream with this caveat in
/// mind: a Rust block where every triple appears as `RustOnly` and
/// FFI has the matching offsets nowhere likely means raw fallback,
/// not a real algorithmic divergence (PR #149 review #21).
pub fn compress_and_collect_sequences(input: &[u8], level: CompressionLevel) -> SequenceCapture {
    // Empty input bypasses the matcher entirely: `FrameCompressor`
    // emits a zero-length raw block without calling any `Matcher`
    // method. The reconstruction invariant `Σ(ll+ml)+Σ(tails) ==
    // input.len()` would trivially pass (`0 == 0`) but
    // `block_tail_lengths.len()` would be 0 — violating the
    // public "one entry per emitted block" contract. Reject
    // explicitly so callers using `tail_lengths.len()` as a block
    // count get a clear diagnostic (PR #149 review #20).
    assert!(
        !input.is_empty(),
        "compress_and_collect_sequences requires non-empty input: \
         the frame compressor emits a zero-length raw block for \
         empty input without invoking the matcher, so no block \
         metadata is recorded.",
    );
    // `CompressionLevel::Uncompressed` short-circuits the encoder
    // before any `Matcher` method runs — the frame compressor emits
    // raw blocks straight from input without consulting
    // `CapturingMatcher`. The recorder would stay empty and the
    // post-compress invariant assert would panic with a misleading
    // "matcher-bypassing block path" message even though the input
    // is perfectly valid. Reject the variant explicitly with a
    // diagnostic that points at the actual constraint
    // (PR #149 review round 4 #12).
    assert!(
        !matches!(level, CompressionLevel::Uncompressed),
        "compress_and_collect_sequences does not support \
         CompressionLevel::Uncompressed: raw-block emission bypasses \
         the matcher entirely, so no sequences or block tails are \
         recorded. Use a compressible level (Fastest / Level(N) / \
         Default / Better) for sequence-stream audits.",
    );
    // Levels that route through the donor block-splitter
    // (`donor_split_block_by_chunks` / `_fromBorders`) can emit
    // multiple physical on-wire blocks per single `start_matching`
    // call. `CapturingMatcher::current_block` increments once per
    // matcher invocation, so on those levels the recorded
    // `block_tail_lengths` would NOT be "one entry per emitted
    // on-wire block" and alignment against FFI delimiters would
    // shift by the split-block count. Reject pre-split levels with
    // a diagnostic until per-physical-block bookkeeping is wired
    // through — current scope is the Lane A `7-compress-default`
    // audit at `Level(3)` (PR #149 review #22).
    let post_split = matches!(level, CompressionLevel::Best)
        || matches!(level, CompressionLevel::Level(11..=22));
    assert!(
        !post_split,
        "compress_and_collect_sequences does not support pre-split \
         levels (Best / Level(11..=22)): the donor block-splitter \
         emits multiple physical blocks per matcher call, which the \
         current per-matcher-call block counter cannot track. The \
         tool is validated for Fastest / Default / Better / \
         Level(1..=10); higher levels need per-physical-block hooks \
         that don't exist yet.",
    );
    // Mirror `FrameCompressor::new()` matcher construction. The
    // `reset()` call inside `compress()` re-derives the real per-level
    // window/strategy from `level`, so the seed values here only need
    // to keep the matcher usable up to that reset.
    let driver = MatchGeneratorDriver::new(1024 * 128, 1);
    let recorded: Rc<RefCell<Vec<CapturedRawSequence>>> = Rc::new(RefCell::new(Vec::new()));
    let block_tail_lengths: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
    let matcher = CapturingMatcher {
        inner: driver,
        recorded: recorded.clone(),
        block_tail_lengths: block_tail_lengths.clone(),
        current_block: 0,
    };
    let mut output: Vec<u8> = Vec::new();
    let mut compressor: FrameCompressor<&[u8], &mut Vec<u8>, CapturingMatcher> =
        FrameCompressor::new_with_matcher(matcher, level);
    compressor.set_source(input);
    compressor.set_drain(&mut output);
    // Hint the exact input size so the matcher picks the same
    // hash-table / window class the production one-shot path uses
    // (`compress_to_vec` does the same). Without the hint, the matcher
    // assumes streaming sizing, which would diverge from the donor's
    // `ZSTD_generateSequences` (which receives `srcSize` directly).
    compressor.set_source_size_hint(input.len() as u64);
    compressor.compress();
    // `Rc::try_unwrap` succeeds because the inner `CapturingMatcher`
    // is dropped when `compressor` goes out of scope at the end of the
    // function, leaving us as the sole `Rc` owner.
    drop(compressor);
    // `Rc::try_unwrap` succeeds because the inner `CapturingMatcher`
    // is dropped when `compressor` goes out of scope above, leaving
    // us as the sole `Rc` owner for both vecs.
    let sequences = Rc::try_unwrap(recorded)
        .expect("CapturingMatcher dropped with compressor; recorder is single-owner")
        .into_inner();
    let block_tail_lengths = Rc::try_unwrap(block_tail_lengths)
        .expect("CapturingMatcher dropped with compressor; tail-length vec is single-owner")
        .into_inner();
    // Fail-fast invariant check: the encoder has a few paths that
    // could emit blocks WITHOUT routing through any `Matcher` method
    // on `CapturingMatcher` (e.g. an `Uncompressed`-level shortcut
    // that emits raw blocks directly from `compress()`, or a future
    // bypass introduced by an internal refactor). Today RLE-shaped
    // constant runs in practice still reach the matcher via
    // `skip_matching_with_hint`, but the assert guards against any
    // future divergence. On such inputs the captured stream would
    // miss entire blocks, so callers walking the cumulative
    // position counter (e.g. `compare_ffi_sequences::align_and_diff`)
    // would silently shift every subsequent row. Panic with a
    // diagnostic instead of returning a quietly-wrong
    // `SequenceCapture` (PR #149 review round 2 #7).
    let reconstructed: u64 = sequences
        .iter()
        .map(|s| s.ll as u64 + s.ml as u64)
        .sum::<u64>()
        + block_tail_lengths.iter().map(|t| *t as u64).sum::<u64>();
    assert_eq!(
        reconstructed,
        input.len() as u64,
        "sequence_capture: matcher-bypassing block path (RLE block? raw-frame fast-path?) \
         left the captured stream short: Σ(ll+ml)+Σ(tails)={reconstructed}, input.len()={}. \
         The current wrapper only sees blocks routed through `Matcher` methods on \
         `CapturingMatcher`. Use a non-RLE-friendly fixture or extend capture to \
         cover the bypassing path before relying on cumulative-position alignment.",
        input.len(),
    );
    SequenceCapture {
        sequences,
        block_tail_lengths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::CompressionLevel;
    use alloc::vec::Vec;

    /// On a 16 KiB repeating 16-byte pattern, the encoder must emit
    /// at least one `Triple` sequence — every position past the first
    /// 16 bytes finds a long match 16 bytes back. Constant runs
    /// (`AAAA…`) intentionally avoided: they route to RLE block
    /// emission which bypasses the matcher entirely and would
    /// silently fail this test for the wrong reason.
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

    /// Random / incompressible input should emit at most a sparse
    /// trickle of triples (the dfast hash can luck into a 5-byte
    /// collision on any 1 KiB stream), well below "every position
    /// is a match". This bounds-the-rate test guards against a
    /// wrapper bug that fabricates phantom sequences or carries
    /// state across calls — a clean wrapper produces few or zero
    /// triples here, but ZERO is not the strict contract.
    #[test]
    fn captures_bounded_triples_on_incompressible_input() {
        // Deterministic non-repeating bytes via a simple LCG.
        let mut state: u32 = 0x1234_5678;
        let data: Vec<u8> = (0..1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as u8
            })
            .collect();
        let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(3));
        let seqs = &captured.sequences;
        // Some matches may still surface on a 1 KiB LCG stream (the
        // dfast hash can luck into a 5-byte collision), but the count
        // must stay well below "every position is a match" — otherwise
        // the wrapper is recording phantom sequences.
        assert!(
            seqs.len() < data.len() / 16,
            "incompressible input emitted suspiciously many sequences: {} (limit: {})",
            seqs.len(),
            data.len() / 16,
        );
        // Each block (matcher call) must contribute exactly one
        // tail-length entry — even when no triples were emitted.
        // Block count is `last.block_idx + 1` if any triples, or
        // we expect at least one block was processed since the
        // input is 1 KiB > 0.
        assert!(
            !captured.block_tail_lengths.is_empty(),
            "no block tail lengths recorded for non-empty input",
        );
        // Cumulative position must still equal `data.len()`. For an
        // incompressible block where the encoder routes through
        // `start_matching` (rare with `set_source_size_hint`, but
        // possible), the trailing-literal tail will cover most of the
        // block; with `skip_matching` routing, the tail equals the
        // entire committed space. Either way, `Σ (ll + ml) + tails`
        // must reconstruct the input length exactly.
        let cumulative: u64 = seqs.iter().map(|s| s.ll as u64 + s.ml as u64).sum::<u64>()
            + captured
                .block_tail_lengths
                .iter()
                .map(|t| *t as u64)
                .sum::<u64>();
        assert_eq!(
            cumulative,
            data.len() as u64,
            "Σ(ll+ml) over triples + Σ(block_tail_lengths) must reconstruct input length",
        );
    }

    /// Constant runs (`[b'A'; N]`) currently route through the
    /// matcher's `skip_matching` path (or emit a single long match),
    /// NOT through an RLE-bypass that skips `CapturingMatcher`
    /// entirely. Verify the invariant still holds on this shape so
    /// `compare_ffi_sequences` can include constant-run fixtures
    /// without tripping the fail-fast assert in
    /// `compress_and_collect_sequences`. If a future encoder change
    /// introduces a true RLE-bypass path on this input the assert
    /// will fire — at which point the wrapper needs extending to
    /// plumb synthetic block metadata out of the bypassing path
    /// (PR #149 review round 2 #7).
    #[test]
    fn constant_run_routes_through_matcher_path() {
        let data: Vec<u8> = alloc::vec![b'A'; 16 * 1024];
        // Calling this is the assertion: the fail-fast invariant
        // check inside `compress_and_collect_sequences` panics with
        // "matcher-bypassing block path" if either `sequences` or
        // `block_tail_lengths` undercount the input bytes. Reaching
        // here without panic proves the matcher path covers
        // constant runs.
        let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(3));
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
    /// 256 KiB chosen so the encoder produces exactly 2 blocks under
    /// the default 128 KiB chunk size, keeping the assertion crisp
    /// (PR #149 review #19).
    #[test]
    fn captures_multi_block_tails_and_indices() {
        // Repeating 64-byte pattern: compressible enough to emit
        // sequences (not RLE) and long enough that every position
        // past the first 64 bytes finds a match, exercising the
        // matcher rather than a fast skip path.
        let pattern: [u8; 64] = {
            let mut p = [0u8; 64];
            for (i, b) in p.iter_mut().enumerate() {
                *b = (i as u8).wrapping_mul(31).wrapping_add(7);
            }
            p
        };
        let data: Vec<u8> = pattern.iter().copied().cycle().take(256 * 1024).collect();
        let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(3));

        // Multi-block invariant: at least 2 blocks recorded.
        assert!(
            captured.block_tail_lengths.len() >= 2,
            "expected ≥2 block tail entries for 256 KiB input, got {}: {:?}",
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
}
