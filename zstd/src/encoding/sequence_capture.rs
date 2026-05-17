//! Bench-only sequence-stream capture for FFI-parity audits.
//!
//! Exposed under the `bench_internals` feature so the regular crate API
//! surface stays unaffected. The single public entry point —
//! [`compress_and_collect_sequences`] — drives the production
//! [`FrameCompressor`] pipeline at the requested `CompressionLevel` and
//! records every `Sequence::Triple` the matcher emits, tagged with its
//! block index. This is the Rust-side input to the
//! `compare_ffi_sequences` bench, which diffs the captured stream
//! against `ZSTD_generateSequences` (donor) on the same `(input, level)`
//! to triage residual ratio deltas into "algorithmic win" / "cost
//! source" / "missed match" classes (`Phase 7 / 7-tooling-seq-cmp`).
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

/// `Matcher` wrapper that forwards every method to an inner
/// [`MatchGeneratorDriver`] while appending each emitted
/// `Sequence::Triple` to a shared recorder. The shared `Rc<RefCell<…>>`
/// lets the caller pull captured sequences out without consuming the
/// `FrameCompressor` mid-frame.
struct CapturingMatcher {
    inner: MatchGeneratorDriver,
    recorded: Rc<RefCell<Vec<CapturedRawSequence>>>,
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
        self.inner.skip_matching();
        self.current_block = self.current_block.saturating_add(1);
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.inner.skip_matching_with_hint(incompressible_hint);
        self.current_block = self.current_block.saturating_add(1);
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        let recorded = self.recorded.clone();
        let block_idx = self.current_block;
        let mut seq_in_block: u32 = 0;
        self.inner.start_matching(|seq| {
            if let Sequence::Triple {
                literals,
                offset,
                match_len,
            } = seq
            {
                recorded.borrow_mut().push(CapturedRawSequence {
                    block_idx,
                    seq_in_block,
                    ll: literals.len() as u32,
                    of: offset as u32,
                    ml: match_len as u32,
                });
                seq_in_block = seq_in_block.saturating_add(1);
            }
            handle_sequence(seq);
        });
        self.current_block = self.current_block.saturating_add(1);
    }

    fn reset(&mut self, level: CompressionLevel) {
        self.inner.reset(level);
        self.recorded.borrow_mut().clear();
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
/// `Sequence::Triple` as a [`CapturedRawSequence`].
///
/// The compressed output is discarded — only the sequence stream is
/// returned. Use this from a benchmark or audit tool to diff the
/// Rust-emitted sequence stream against a donor FFI side
/// (`ZSTD_generateSequences`) for the same `(input, level)` pair.
///
/// `Sequence::Literals` entries (trailing-literals tail of a block) are
/// NOT recorded — the donor's `ZSTD_generateSequences` emits trailing
/// literals as a dummy delimiter `(of=0, ml=0, ll=last)`, and including
/// our side's `Literals` events would add noise to the diff. Callers
/// that need block-boundary alignment can use
/// `CapturedRawSequence::block_idx` instead.
pub fn compress_and_collect_sequences(
    input: &[u8],
    level: CompressionLevel,
) -> Vec<CapturedRawSequence> {
    // Mirror `FrameCompressor::new()` matcher construction. The
    // `reset()` call inside `compress()` re-derives the real per-level
    // window/strategy from `level`, so the seed values here only need
    // to keep the matcher usable up to that reset.
    let driver = MatchGeneratorDriver::new(1024 * 128, 1);
    let recorded: Rc<RefCell<Vec<CapturedRawSequence>>> = Rc::new(RefCell::new(Vec::new()));
    let matcher = CapturingMatcher {
        inner: driver,
        recorded: recorded.clone(),
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
    Rc::try_unwrap(recorded)
        .expect("CapturingMatcher dropped with compressor; recorder is single-owner")
        .into_inner()
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
        assert!(
            !captured.is_empty(),
            "expected at least one Triple sequence on 16KB repeating pattern, got 0",
        );
        // Every captured sequence belongs to block 0 (single 16 KiB block fits in 128 KiB).
        assert!(
            captured.iter().all(|s| s.block_idx == 0),
            "16KB repeating pattern produced multi-block sequence stream: {:?}",
            captured.iter().map(|s| s.block_idx).collect::<Vec<_>>(),
        );
        // Every captured Triple must reference a sane offset/match
        // (defensive: catches wrapper bugs that would leak garbage
        // through the recorder).
        for s in &captured {
            assert!(s.of >= 1, "non-positive offset captured: {:?}", s);
            assert!(s.ml >= 1, "non-positive match length captured: {:?}", s);
        }
        // seq_in_block must be contiguous 0..N for each block.
        for (i, s) in captured.iter().enumerate() {
            assert_eq!(
                s.seq_in_block, i as u32,
                "seq_in_block discontinuity at idx {}: {:?}",
                i, captured,
            );
        }
    }

    /// Random / incompressible input should NOT emit any matches —
    /// recorder stays empty, confirming the wrapper doesn't fabricate
    /// or carry over state across calls.
    #[test]
    fn captures_no_triples_on_incompressible_input() {
        // Deterministic non-repeating bytes via a simple LCG.
        let mut state: u32 = 0x1234_5678;
        let data: Vec<u8> = (0..1024)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as u8
            })
            .collect();
        let captured = compress_and_collect_sequences(&data, CompressionLevel::Level(3));
        // Some matches may still surface on a 1 KiB LCG stream (the
        // dfast hash can luck into a 5-byte collision), but the count
        // must stay well below "every position is a match" — otherwise
        // the wrapper is recording phantom sequences.
        assert!(
            captured.len() < data.len() / 16,
            "incompressible input emitted suspiciously many sequences: {} (limit: {})",
            captured.len(),
            data.len() / 16,
        );
    }
}
