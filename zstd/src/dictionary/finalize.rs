//! Finalizing raw dictionary content into a dictionary (upstream zstd
//! `zdict.c`, `ZDICT_finalizeDictionary` and `ZDICT_analyzeEntropy`).
//!
//! The entropy tables a dictionary carries seed the first block of every frame
//! compressed with it, so they are drawn from what such blocks actually hold:
//! each sample's first block is compressed with the content as a raw
//! dictionary, and the literals and sequence codes it produces are counted.

use super::FinalizeOptions;
use super::samples::{SampleSet, TrainingError, refuse};
use crate::bit_io::BitWriter;
use crate::decoding::dictionary::MAGIC_NUM as DICT_MAGIC_NUM;
use crate::encoding::blocks::{
    encode_literal_length, encode_match_len, encode_offset, encode_offset_with_history,
    encode_offset_with_history_fast,
};
use crate::encoding::workspace::Workspace;
use crate::encoding::{
    CompressionLevel, DictionarySizes, EncoderDictionary, FrameCompressor, HistoryBuf,
    MatchGeneratorDriver, Matcher, Sequence,
};
use crate::fse::fse_encoder::{FSETable, write_ncount_at_log};
use crate::huff0::huff0_encoder::{HuffmanEncoder, HuffmanTable, WeightScratch};
use std::{io, vec::Vec};

/// Smallest dictionary finalized (upstream zstd `ZDICT_DICTSIZE_MIN`).
pub(super) const DICT_SIZE_MIN: usize = 256;
/// Repeat offsets a dictionary starts frames with (upstream `repStartValue`).
/// The content has to reach the largest of them.
const START_REPS: [u32; 3] = [1, 4, 8];
const MIN_CONTENT_SIZE: usize = 8;
/// Longest literals code (upstream `ZDICT_analyzeEntropy`, `huffLog`).
const HUF_MAX_BITS: usize = 11;
/// Table logs of the three sequence streams (upstream `OffFSELog`,
/// `MLFSELog`, `LLFSELog`).
const OF_LOG: u8 = 8;
const ML_LOG: u8 = 9;
const LL_LOG: u8 = 9;
/// Highest offset code a first block can need (upstream `OFFCODE_MAX`).
const OFFCODE_MAX: u32 = 30;
/// Most of a sample the statistics read: its first block.
const BLOCK_SIZE_MAX: usize = 128 << 10;

/// Finalize `content` into a dictionary of at most `dict_size` bytes, drawing
/// the entropy tables from the first `count` samples. Content that does not
/// fit after the header is cut from the front, keeping the best segments.
pub(super) fn finalize<'s>(
    content: &[u8],
    samples: &SampleSet<'s>,
    count: usize,
    dict_size: usize,
    options: FinalizeOptions,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut analysis = Analysis::default();
    finalize_into(
        &mut out,
        &mut analysis,
        content,
        samples,
        count,
        dict_size,
        options,
    )?;
    Ok(out)
}

/// A compressor over one sample at a time, recording what it matches.
type AnalysisCompressor<'s> = FrameCompressor<&'s [u8], Vec<u8>, Recorder>;

/// What the entropy analysis keeps from one candidate to the next: the
/// compressor it runs the samples through, whose matcher and scratch then
/// survive the change of dictionary, its frame buffer, its block kinds and
/// the buffers the literals code is built in.
#[derive(Default)]
pub(super) struct Analysis<'s> {
    /// The compressor and the level it was built for.
    compressor: Option<(i32, AnalysisCompressor<'s>)>,
    frame: Vec<u8>,
    compressed: Vec<bool>,
    huffman: WeightScratch,
}

/// [`finalize`] over `out` and `analysis`, whose allocations a search keeps
/// from one candidate to the next.
pub(super) fn finalize_into<'s>(
    out: &mut Vec<u8>,
    analysis: &mut Analysis<'s>,
    content: &[u8],
    samples: &SampleSet<'s>,
    count: usize,
    dict_size: usize,
    options: FinalizeOptions,
) -> io::Result<()> {
    check_size(dict_size)?;
    if content.is_empty() {
        return Err(invalid("raw dictionary content must not be empty"));
    }
    let dict_id = options.dict_id.unwrap_or_else(|| derive_dict_id(content));
    if dict_id == 0 {
        return Err(invalid("dictionary id must be non-zero"));
    }
    out.clear();
    out.reserve(dict_size);
    out.extend_from_slice(&DICT_MAGIC_NUM);
    out.extend_from_slice(&dict_id.to_le_bytes());
    analyze_entropy(out, analysis, content, samples, count, options.level)?;
    for rep in START_REPS {
        out.extend_from_slice(&rep.to_le_bytes());
    }
    let room = match dict_size.checked_sub(out.len()) {
        Some(room) if room >= MIN_CONTENT_SIZE => room,
        _ => return Err(too_small()),
    };
    let content = &content[content.len() - content.len().min(room)..];
    // Zeros ahead of short content: the last byte is the best position.
    if content.len() < MIN_CONTENT_SIZE {
        out.resize(out.len() + MIN_CONTENT_SIZE - content.len(), 0);
    }
    out.extend_from_slice(content);
    Ok(())
}

/// Refuse a dictionary below the smallest one finalized. Upstream zstd's
/// order (`ZDICT_finalizeDictionary`): checked before anything else, the
/// samples included.
pub(super) fn check_size(dict_size: usize) -> io::Result<()> {
    if dict_size < DICT_SIZE_MIN {
        return Err(too_small());
    }
    Ok(())
}

fn too_small() -> io::Error {
    refuse(
        TrainingError::DictionaryTooSmall,
        "dictionary size too small to fit header and offset history",
    )
}

/// A refusal upstream reports as a failed creation, with no finer cause.
fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

fn derive_dict_id(raw_content: &[u8]) -> u32 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in raw_content {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // Upstream's compliant range: ids below 32768 are reserved.
    ((h % ((1u64 << 31) - 32768)) + 32768) as u32
}

/// Literal and sequence-code counts, every symbol starting at one so each can
/// be described (upstream zstd `ZDICT_analyzeEntropy`).
struct EntropyCounts {
    literals: [usize; 256],
    /// Every code a 32-bit offset base has, as upstream's `offcodeCount`
    /// holds: only the codes up to the alphabet's bound are described, and a
    /// sequence is counted without asking which side of it its code is on.
    offset_codes: [usize; 32],
    match_lengths: [usize; 53],
    literal_lengths: [usize; 36],
}

/// Append the literals table, the three sequence tables and nothing else to
/// `out`, from the statistics of the first `count` samples compressed with
/// `content` as a raw dictionary at `level`.
fn analyze_entropy<'s>(
    out: &mut Vec<u8>,
    analysis: &mut Analysis<'s>,
    content: &[u8],
    samples: &SampleSet<'s>,
    count: usize,
    level: i32,
) -> io::Result<()> {
    // The largest offset a first block can reach sets the offset alphabet.
    let reach = content.len() as u64 + BLOCK_SIZE_MAX as u64;
    let offcode_max = reach.ilog2();
    if offcode_max > OFFCODE_MAX {
        return Err(invalid(
            "dictionary content is too large to describe its offsets",
        ));
    }
    let mut counts = EntropyCounts {
        literals: [1; 256],
        offset_codes: [1; 32],
        match_lengths: [1; 53],
        literal_lengths: [1; 36],
    };
    count_samples(&mut counts, analysis, content, samples, count, level)?;

    let huffman = &mut analysis.huffman;
    let mut literals = literals_table(&mut counts.literals, huffman);
    // Encoded in the table's own buffer and the scratch's FSE table, both kept
    // from one candidate to the next, so the writer below reads it back.
    literals.fill_weight_description_from_codes(huffman.weight_fse_table());
    // Each description goes straight into the dictionary; the sequence tables
    // are normalized and described without being built.
    let mut writer = BitWriter::from(&mut *out);
    HuffmanEncoder::new(&literals, &mut writer).write_table();
    huffman.recycle(literals);
    write_ncount_at_log(
        &counts.offset_codes[..=offcode_max as usize],
        OF_LOG,
        &mut writer,
    );
    write_ncount_at_log(&counts.match_lengths, ML_LOG, &mut writer);
    write_ncount_at_log(&counts.literal_lengths, LL_LOG, &mut writer);
    writer.flush();
    Ok(())
}

/// The literals code for `counts`, built in `scratch`.
fn literals_table(counts: &mut [usize; 256], scratch: &mut WeightScratch) -> HuffmanTable {
    // Summed over every sample, the counts can pass what a tree node holds
    // (`u32`); halving keeps their proportions, and rounding up keeps every
    // symbol that occurred in the code.
    // The literals counted are bytes held in memory, so the sum fits `u64`.
    while counts.iter().map(|&count| count as u64).sum::<u64>() >= u64::from(u32::MAX) {
        for count in counts.iter_mut() {
            *count = count.div_ceil(2);
        }
    }
    let literals = HuffmanTable::build_limited_in(counts, HUF_MAX_BITS, scratch);
    if literals.table_log() != 8 {
        return literals;
    }
    // Every symbol at eight bits describes nothing and cannot be written; a
    // mostly flat distribution that still compresses stands in (upstream zstd
    // `ZDICT_flatLit`).
    *counts = [2; 256];
    counts[0] = 4;
    counts[253] = 1;
    counts[254] = 1;
    scratch.recycle(literals);
    let flat = HuffmanTable::build_limited_in(counts, HUF_MAX_BITS, scratch);
    debug_assert_eq!(flat.table_log(), 9);
    flat
}

/// Compress the first 128 KiB of each of the first `count` samples with
/// `content` as a raw dictionary and count what its compressed blocks hold.
fn count_samples<'s>(
    counts: &mut EntropyCounts,
    analysis: &mut Analysis<'s>,
    content: &[u8],
    samples: &SampleSet<'s>,
    count: usize,
    level: i32,
) -> io::Result<()> {
    // The candidate is copied into the encoder dictionary, where upstream zstd
    // references it (`ZSTD_dlm_byRef`): under callgrind the copy and the
    // dictionary's preparation are 0.02% of a default training run, against
    // 52.7% for compressing the samples with it. Borrowing would put a lifetime
    // on the encoder dictionary for none of it.
    let dictionary = crate::decoding::Dictionary::from_raw_content(0, content.to_vec())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let Analysis {
        compressor,
        frame,
        compressed,
        ..
    } = analysis;
    // One compressor per level serves every candidate: the dictionary is all
    // that changes, so its matcher and scratch are kept.
    if compressor.as_ref().is_none_or(|(built, _)| *built != level) {
        let recorder = Recorder {
            inner: MatchGeneratorDriver::new(BLOCK_SIZE_MAX, 1),
            blocks: Vec::new(),
            sequences: Vec::new(),
        };
        let mut built =
            FrameCompressor::new_with_matcher(recorder, CompressionLevel::from_level(level));
        // One written block per matched one, so each is counted by its own
        // kind; upstream's analysis compresses a single block, never split.
        built.forbid_post_split();
        *compressor = Some((level, built));
    }
    let (_, compressor) = compressor.as_mut().expect("built above");
    compressor
        .set_encoder_dictionary(EncoderDictionary::from_dictionary(dictionary))
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    // Every sample runs the parameters of the average one, as upstream's
    // analysis does (`ZSTD_getParams(level, averageSampleSize, dictSize)`):
    // one set of tables for the whole pass, which also keeps the dictionary
    // resident from one sample to the next instead of indexing it again.
    let average = samples.leading(count).len() / count.max(1);
    // Every block of a sample is counted, not only the first as upstream's
    // `ZDICT_countEStats` does: counting the first block alone, at our window
    // or at upstream's, measured worse on the repository files (the samples
    // compressed 0.013% and 0.05% larger in total across COVER, FastCOVER
    // and the default search), so the rest of a sample is signal, not noise.
    for index in 0..count {
        let sample = samples.sample(index);
        let block = &sample[..sample.len().min(BLOCK_SIZE_MAX)];
        if block.is_empty() {
            continue;
        }
        compressor.set_source(block);
        compressor.set_source_size_hint(average as u64);
        compressor.compress_known_into(block.len() as u64, frame);
        let fast_offset_codes = compressor.uses_fast_offset_codes();
        compressed_blocks(frame, compressed);
        let recorder = compressor.matcher_mut();
        // A block written raw or as one repeated byte holds no sequences to
        // learn from, and its bytes are no literals of any compressed block.
        // With no cut after matching, the frame holds one block per recorded
        // one.
        debug_assert_eq!(compressed.len(), recorder.blocks.len());
        let mut reps = START_REPS;
        let mut start = 0;
        for (block, &counted) in recorder.blocks.iter().zip(compressed.iter()) {
            let block_sequences = &recorder.sequences[start..block.sequences_end];
            start = block.sequences_end;
            // A block left out does not advance the repeat offsets either: the
            // encoder restores them when it writes a block raw.
            if !counted {
                continue;
            }
            for (total, &seen) in counts.literals.iter_mut().zip(&block.literals) {
                *total += seen as usize;
            }
            for &(ll, offset, ml) in block_sequences {
                // The code the block wrote for this offset, under its policy.
                let off_base = if fast_offset_codes {
                    encode_offset_with_history_fast(offset, ll, &mut reps)
                } else {
                    encode_offset_with_history(offset, ll, &mut reps)
                };
                counts.literal_lengths[encode_literal_length(ll).0 as usize] += 1;
                counts.match_lengths[encode_match_len(ml).0 as usize] += 1;
                // The code is the offset base's highest bit, below 32.
                let code = encode_offset(off_base).0 as usize;
                debug_assert!(code < counts.offset_codes.len());
                counts.offset_codes[code] += 1;
            }
        }
        recorder.blocks.clear();
        recorder.sequences.clear();
    }
    Ok(())
}

/// Whether each block of `frame` is a compressed one, into `out`; nothing for
/// bytes that are not a frame.
fn compressed_blocks(frame: &[u8], out: &mut Vec<bool>) {
    out.clear();
    let Ok((_, header_len)) = crate::decoding::frame::read_frame_header_with_format(frame, false)
    else {
        return;
    };
    let mut at = usize::from(header_len);
    while let Some(header) = frame.get(at..at + 3) {
        // Block header (RFC 8878 3.1.1.2): Last_Block is bit 0, Block_Type
        // bits 1-2, Block_Size the rest; an RLE block carries one byte.
        let word = u32::from(header[0]) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
        let kind = (word >> 1) & 3;
        out.push(kind == 2);
        if word & 1 == 1 {
            return;
        }
        let body = if kind == 1 { 1 } else { (word >> 3) as usize };
        at += 3 + body;
    }
}

/// The production matcher, recording the literals and sequences of every block
/// it matches, block by block. Everything else is forwarded, so frames
/// compress as any compressor's would, dictionary reuse included.
struct Recorder {
    inner: MatchGeneratorDriver,
    /// One entry per block of the frame, matched or skipped, in order.
    blocks: Vec<RecordedBlock>,
    /// Literal length, offset and match length of each sequence.
    sequences: Vec<(u32, u32, u32)>,
}

/// What one block handed to the matcher produced.
struct RecordedBlock {
    literals: [u32; 256],
    /// Where the block's sequences end in [`Recorder::sequences`].
    sequences_end: usize,
}

impl Recorder {
    /// A block the matcher did not search: it yields nothing.
    fn record_skipped(&mut self) {
        self.blocks.push(RecordedBlock {
            literals: [0; 256],
            sequences_end: self.sequences.len(),
        });
    }
}

impl Matcher for Recorder {
    fn get_last_space(&mut self) -> &[u8] {
        self.inner.get_last_space()
    }

    fn fill_in_place(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        self.inner.fill_in_place(capacity, fill)
    }

    fn uncommitted_input(&self) -> &[u8] {
        self.inner.uncommitted_input()
    }

    fn commit_filled(&mut self, len: usize) {
        self.inner.commit_filled(len);
    }

    fn skip_matching(&mut self) {
        self.inner.skip_matching();
        self.record_skipped();
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.inner.skip_matching_with_hint(incompressible_hint);
        self.record_skipped();
    }

    fn start_matching(&mut self, mut handle_sequence: impl FnMut(Sequence)) {
        let block_start = self.sequences.len();
        let mut tail = 0usize;
        let Self {
            inner, sequences, ..
        } = self;
        inner.start_matching(|sequence| {
            match sequence {
                Sequence::Literals { len } => tail += len,
                Sequence::Triple {
                    literal_len,
                    offset,
                    match_len,
                } => {
                    // Lengths and offsets of one block, far below `u32::MAX`.
                    sequences.push((literal_len as u32, offset as u32, match_len as u32));
                }
            }
            handle_sequence(sequence);
        });
        // The literals are the block's bytes between the matches, read back by
        // position once matching is done, as the encoder gathers them.
        let block = self.inner.get_last_space();
        let mut literals = [0u32; 256];
        let mut pos = 0usize;
        let mut count = |bytes: &[u8]| {
            for &byte in bytes {
                literals[usize::from(byte)] += 1;
            }
        };
        for &(ll, _, ml) in &self.sequences[block_start..] {
            count(&block[pos..pos + ll as usize]);
            pos += ll as usize + ml as usize;
        }
        count(&block[pos..pos + tail]);
        let sequences_end = self.sequences.len();
        self.blocks.push(RecordedBlock {
            literals,
            sequences_end,
        });
    }

    fn reset(&mut self, level: CompressionLevel) {
        self.inner.reset(level);
    }

    fn reset_in_workspace(&mut self, level: CompressionLevel, workspace: &mut Workspace) {
        self.inner.reset_in_workspace(level, workspace);
    }

    fn leave_workspace(&mut self) {
        self.inner.leave_workspace();
    }

    fn set_source_size_hint(&mut self, size: u64) {
        self.inner.set_source_size_hint(size);
    }

    fn set_dictionary_size_hint(&mut self, sizes: DictionarySizes) {
        self.inner.set_dictionary_size_hint(sizes);
    }

    fn clear_param_overrides(&mut self) {
        self.inner.clear_param_overrides();
    }

    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        self.inner.prime_with_dictionary(dict_content, offset_hist);
    }

    fn dictionary_is_resident(&self) -> bool {
        self.inner.dictionary_is_resident()
    }

    fn reapply_resident_dictionary(&mut self, offset_hist: [u32; 3]) {
        self.inner.reapply_resident_dictionary(offset_hist);
    }

    fn restore_primed_dictionary(&mut self, level: CompressionLevel) -> bool {
        self.inner.restore_primed_dictionary(level)
    }

    fn capture_primed_dictionary(&mut self, level: CompressionLevel) {
        self.inner.capture_primed_dictionary(level);
    }

    fn invalidate_primed_dictionary(&mut self) {
        self.inner.invalidate_primed_dictionary();
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&HuffmanTable>,
        ll: Option<&FSETable>,
        ml: Option<&FSETable>,
        of: Option<&FSETable>,
    ) {
        self.inner.seed_dictionary_entropy(huff, ll, ml, of);
    }

    fn supports_dictionary_priming(&self) -> bool {
        self.inner.supports_dictionary_priming()
    }

    fn block_samples_match_dict(&self, block: &[u8]) -> bool {
        self.inner.block_samples_match_dict(block)
    }

    fn heap_size(&self) -> usize {
        self.inner.heap_size()
    }

    fn window_size(&self) -> u64 {
        self.inner.window_size()
    }
}

#[cfg(test)]
mod tests;
