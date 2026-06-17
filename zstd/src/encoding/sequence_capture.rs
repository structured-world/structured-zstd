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
        // Plain `+`: diagnostic per-block index; the comparator runs on bench
        // fixtures whose block count is nowhere near u32::MAX.
        self.current_block += 1;
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
        // Plain `+`: diagnostic per-block index; the comparator runs on bench
        // fixtures whose block count is nowhere near u32::MAX.
        self.current_block += 1;
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
            // Match by reference so `seq` stays owned for the
            // forward to `handle_sequence`. Today every field of
            // `Sequence` is `Copy` (`&[u8]`, `usize`), so a by-value
            // match would also leave `seq` usable through implicit
            // copy semantics, but binding by-ref is robust if any
            // future field on `Sequence` turns non-Copy
            // (PR #149 review #29).
            match &seq {
                Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                } => {
                    recorded.borrow_mut().push(CapturedRawSequence {
                        block_idx,
                        seq_in_block,
                        ll: literals.len() as u32,
                        of: *offset as u32,
                        ml: *match_len as u32,
                    });
                    // Plain `+`: sequences per block <= MAX_BLOCK_SIZE, far under u32::MAX.
                    seq_in_block += 1;
                }
                Sequence::Literals { literals } => {
                    block_tail_ll = literals.len() as u32;
                }
            }
            handle_sequence(seq);
        });
        self.block_tail_lengths.borrow_mut().push(block_tail_ll);
        // Plain `+`: diagnostic per-block index; the comparator runs on bench
        // fixtures whose block count is nowhere near u32::MAX.
        self.current_block += 1;
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
/// Rust-emitted sequence stream against libzstd's
/// `ZSTD_generateSequences` for the same `(input, level)` pair.
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
/// # Raw-fallback detection
///
/// The matcher hook records triples eagerly; the encoder may later
/// discard a compressed attempt and emit a Raw_Block when
/// `compressed.len() >= MAX_BLOCK_SIZE`. The capture would then
/// contain phantom triples whose on-wire form has no sequences. To
/// prevent silently misaligned output, this function parses the
/// emitted frame's block headers (RFC 8878 §3.1.1.2.2) via
/// [`detect_raw_or_rle_blocks_in_frame`] and panics with a clear
/// diagnostic if any Raw_Block or RLE_Block is present. Callers
/// see a hard failure instead of a misleading capture
/// (PR #149 review #25).
pub fn compress_and_collect_sequences(input: &[u8], level: CompressionLevel) -> SequenceCapture {
    compress_and_collect_sequences_impl(input, level, None, None)
}

/// Raw-content dictionary variant: attaches `raw_content` via
/// [`crate::decoding::Dictionary::from_raw_content`] + `set_dictionary`, the
/// exact path the `dict_builder` raw-dict tests use. Lets the dict-ratio audit
/// reproduce a raw-content (non-serialized) dictionary scenario.
pub fn compress_and_collect_sequences_with_raw_content(
    input: &[u8],
    level: CompressionLevel,
    raw_content: &[u8],
) -> SequenceCapture {
    compress_and_collect_sequences_impl(input, level, None, Some(raw_content))
}

/// Dictionary-primed variant of [`compress_and_collect_sequences`].
///
/// Attaches the serialized `dict` blob via
/// [`FrameCompressor::set_dictionary_from_bytes`] before compressing, so the
/// captured stream reflects dictionary priming (matcher hash-table prime +
/// offset-history seed + entropy-table seed) exactly as the production
/// dict-compress path would. Used by the dict-ratio audit to diff the Fast
/// backend's dict-primed sequence stream against the dfast backend's (which
/// matches libzstd's stream) on the same `(input, dict)` pair.
pub fn compress_and_collect_sequences_with_dict(
    input: &[u8],
    level: CompressionLevel,
    dict: &[u8],
) -> SequenceCapture {
    compress_and_collect_sequences_impl(input, level, Some(dict), None)
}

fn compress_and_collect_sequences_impl(
    input: &[u8],
    level: CompressionLevel,
    dict: Option<&[u8]>,
    raw_content: Option<&[u8]>,
) -> SequenceCapture {
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
    // Only the POST-split path breaks the per-matcher-call block
    // counter. Two distinct splitter mechanisms exist in
    // `frame_compressor.rs`:
    //
    // * Pre-split (`Level(11..=15)` borders + `optimal_block_size`,
    //   borders-only): the splitter chooses a shrunken `block_len`
    //   BEFORE the matcher runs; the suffix is parked in
    //   `pending_input` and the next compress-loop iteration calls
    //   the matcher again on the suffix. Each matcher call still
    //   maps to exactly ONE physical on-wire block, so
    //   `CapturingMatcher::current_block` tracks correctly.
    //
    // * Post-split (`Level(16..=22)` + window >= 1<<17, dispatched
    //   from `levels/fastest.rs::compress_block_encoded` via
    //   `compress_block_with_post_split`): a SINGLE matcher call's
    //   output is split into multiple physical blocks by
    //   `blocks::compress_block_with_post_split`. One matcher call
    //   → N blocks → `current_block` only increments once,
    //   `block_tail_lengths.len()` is short by `N - 1`.
    //
    // Reject `Level(n >= 16)` only. Covers `Level(16..=22)` and
    // clamped `Level(>22)` (match_generator.rs:412-415 lands on
    // Level 22 params for n > 22). `Level(11..=15)` is allowed
    // because pre-split produces a separate matcher call per
    // physical block (PR #149 review #24 + #27 + #30).
    let post_split = matches!(level, CompressionLevel::Level(n) if n >= 16);
    assert!(
        !post_split,
        "compress_and_collect_sequences does not support post-split \
         levels (Level(n) where n >= 16): `compress_block_with_post_split` \
         emits multiple physical blocks per matcher call, which the \
         current per-matcher-call block counter cannot track. The \
         tool is validated for Fastest / Default / Better / Best / \
         Level(1..=15); higher numeric levels (including levels above \
         22 which clamp to Level 22 params) need per-physical-block \
         hooks that don't exist yet.",
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
    if let Some(dict) = dict {
        compressor
            .set_dictionary_from_bytes(dict)
            .expect("dictionary should attach for sequence capture");
    }
    if let Some(raw) = raw_content {
        let d = crate::decoding::Dictionary::from_raw_content(0xD1C7_0008, raw.to_vec())
            .expect("raw-content dictionary should build for sequence capture");
        compressor
            .set_dictionary(d)
            .expect("raw-content dictionary should attach for sequence capture");
    }
    compressor.set_source(input);
    compressor.set_drain(&mut output);
    // Hint the exact input size so the matcher picks the same
    // hash-table / window class the production one-shot path uses
    // (`compress_to_vec` does the same). Without the hint, the matcher
    // assumes streaming sizing, which would diverge from libzstd's
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
    // Detect raw-fallback / RLE on-wire blocks. The matcher records
    // triples eagerly, but `compress_block_encoded` may discard the
    // compressed block and emit a Raw_Block when
    // `compressed.len() >= MAX_BLOCK_SIZE` (compression made things
    // bigger). The matcher-side capture would then contain phantom
    // triples for a block whose on-wire form has no sequences,
    // turning the comparator's signal into spurious `RustOnly` rows.
    // Parse the emitted frame's block headers and panic if any Raw
    // or RLE block is present so the broken precondition surfaces
    // immediately instead of being misread as a real divergence
    // (PR #149 review #25).
    let raw_or_rle = detect_raw_or_rle_blocks_in_frame(&output).expect(
        "sequence_capture: failed to parse emitted frame header — refusing to \
         return a possibly-misaligned capture without raw-block detection",
    );
    assert!(
        raw_or_rle.is_empty(),
        "compress_and_collect_sequences: emitted frame contains {} raw/RLE block(s) at \
         on-wire indices {:?}. The matcher recorded triples for those blocks but the \
         on-wire form has no sequences for them — alignment against FFI delimiters \
         would silently shift. Use a more compressible fixture (or a smaller block \
         size) that keeps every block on the compressed path.",
        raw_or_rle.len(),
        raw_or_rle,
    );
    SequenceCapture {
        sequences,
        block_tail_lengths,
    }
}

/// Walk the emitted Zstandard frame and return the on-wire indices
/// of any Raw_Block or RLE_Block entries (RFC 8878 §3.1.1.2.2). The
/// capture's matcher hook cannot observe the encoder's late
/// raw-fallback decision; this parser gives us a way to fail-fast
/// when that decision happens. Returns `Err` on malformed frames so
/// the caller can panic with a clearer diagnostic than a silent
/// short read.
fn detect_raw_or_rle_blocks_in_frame(frame: &[u8]) -> Result<Vec<usize>, &'static str> {
    const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    if frame.len() < 6 || frame[..4] != ZSTD_MAGIC {
        return Err("frame missing zstd magic");
    }
    let mut cursor = 4_usize;
    let fhd = frame[cursor];
    cursor += 1;
    let dict_id_flag = fhd & 0b11;
    let content_checksum_flag = (fhd >> 2) & 1;
    let single_segment_flag = (fhd >> 5) & 1;
    let fcs_flag = (fhd >> 6) & 0b11;
    // Window_Descriptor byte present only when single_segment_flag = 0.
    if single_segment_flag == 0 {
        cursor = cursor.checked_add(1).ok_or("cursor overflow")?;
    }
    let dict_id_size = match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        _ => unreachable!(),
    };
    cursor = cursor.checked_add(dict_id_size).ok_or("cursor overflow")?;
    // Frame_Content_Size: 0/1/2/4/8 bytes per
    // (single_segment_flag, fcs_flag) combination per RFC 8878.
    let fcs_size = match (single_segment_flag, fcs_flag) {
        (1, 0) => 1,
        (_, 0) => 0,
        (_, 1) => 2,
        (_, 2) => 4,
        (_, 3) => 8,
        _ => unreachable!(),
    };
    cursor = cursor.checked_add(fcs_size).ok_or("cursor overflow")?;
    if cursor > frame.len() {
        return Err("truncated frame header");
    }

    // Iterate blocks until last_block bit is set. Each block has a
    // 3-byte little-endian header: bit 0 = last, bits 1-2 =
    // block_type (0=Raw, 1=RLE, 2=Compressed, 3=Reserved), bits 3-23
    // = block_size (Block_Content size for Raw/Compressed,
    // Regenerated_Size for RLE).
    let mut raw_or_rle = Vec::new();
    let mut block_idx: usize = 0;
    loop {
        if cursor.checked_add(3).ok_or("cursor overflow")? > frame.len() {
            return Err("truncated block header");
        }
        let header = u32::from(frame[cursor])
            | (u32::from(frame[cursor + 1]) << 8)
            | (u32::from(frame[cursor + 2]) << 16);
        cursor += 3;
        let last_block = (header & 1) != 0;
        let block_type = (header >> 1) & 0b11;
        let block_size = (header >> 3) as usize;
        match block_type {
            0 => {
                // Raw_Block: Block_Content is `block_size` literal bytes.
                raw_or_rle.push(block_idx);
                cursor = cursor.checked_add(block_size).ok_or("cursor overflow")?;
            }
            1 => {
                // RLE_Block: 1-byte content, regenerated to `block_size` bytes.
                raw_or_rle.push(block_idx);
                cursor = cursor.checked_add(1).ok_or("cursor overflow")?;
            }
            2 => {
                // Compressed_Block: Block_Content is `block_size` bytes.
                cursor = cursor.checked_add(block_size).ok_or("cursor overflow")?;
            }
            3 => return Err("reserved block_type in frame"),
            _ => unreachable!(),
        }
        block_idx += 1;
        if cursor > frame.len() {
            return Err("block content extends past frame end");
        }
        if last_block {
            break;
        }
    }
    // Optional 4-byte content checksum (validated separately by the
    // decoder; not consumed here beyond bounds-checking).
    if content_checksum_flag == 1 && cursor.checked_add(4).is_none_or(|end| end > frame.len()) {
        return Err("truncated content checksum");
    }
    Ok(raw_or_rle)
}

#[cfg(test)]
mod tests {
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
        let _ = compress_and_collect_sequences(
            b"hello there general kenobi",
            CompressionLevel::Level(16),
        );
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
}
