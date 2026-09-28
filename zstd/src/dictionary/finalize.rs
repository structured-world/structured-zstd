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
    CompressionLevel, CompressionParameters, DictionarySizes, EncoderDictionary, FrameCompressor,
    HistoryBuf, LevelParameters, MatchGeneratorDriver, Matcher, Sequence,
};
use crate::fse::fse_encoder::{FSETable, write_ncount_at_log};
use crate::huff0::huff0_encoder::{HuffmanEncoder, HuffmanTable};
use std::{io, vec, vec::Vec};

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
pub(super) fn finalize(
    content: &[u8],
    samples: &SampleSet<'_>,
    count: usize,
    dict_size: usize,
    options: FinalizeOptions,
) -> io::Result<Vec<u8>> {
    // Upstream zstd's order (`ZDICT_finalizeDictionary`): the size first.
    if dict_size < DICT_SIZE_MIN {
        return Err(too_small());
    }
    if content.is_empty() {
        return Err(invalid("raw dictionary content must not be empty"));
    }
    let dict_id = options.dict_id.unwrap_or_else(|| derive_dict_id(content));
    if dict_id == 0 {
        return Err(invalid("dictionary id must be non-zero"));
    }
    let mut out = Vec::with_capacity(dict_size);
    out.extend_from_slice(&DICT_MAGIC_NUM);
    out.extend_from_slice(&dict_id.to_le_bytes());
    analyze_entropy(&mut out, content, samples, count, options.level)?;
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
    Ok(out)
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
    offset_codes: Vec<usize>,
    match_lengths: [usize; 53],
    literal_lengths: [usize; 36],
}

/// Append the literals table, the three sequence tables and nothing else to
/// `out`, from the statistics of the first `count` samples compressed with
/// `content` as a raw dictionary at `level`.
fn analyze_entropy(
    out: &mut Vec<u8>,
    content: &[u8],
    samples: &SampleSet<'_>,
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
        offset_codes: vec![1; offcode_max as usize + 1],
        match_lengths: [1; 53],
        literal_lengths: [1; 36],
    };
    count_samples(&mut counts, content, samples, count, level)?;

    let mut literals = literals_table(&counts.literals);
    if literals.table_log() == 8 {
        // Every symbol at eight bits describes nothing and cannot be written;
        // a mostly flat distribution that still compresses stands in
        // (upstream zstd `ZDICT_flatLit`).
        counts.literals = [2; 256];
        counts.literals[0] = 4;
        counts.literals[253] = 1;
        counts.literals[254] = 1;
        literals = literals_table(&counts.literals);
        debug_assert_eq!(literals.table_log(), 9);
    }
    // Each description goes straight into the dictionary; the sequence tables
    // are normalized and described without being built.
    let mut writer = BitWriter::from(&mut *out);
    HuffmanEncoder::new(&literals, &mut writer).write_table();
    write_ncount_at_log(&counts.offset_codes, OF_LOG, &mut writer);
    write_ncount_at_log(&counts.match_lengths, ML_LOG, &mut writer);
    write_ncount_at_log(&counts.literal_lengths, LL_LOG, &mut writer);
    writer.flush();
    Ok(())
}

/// Compress the first block of each of the first `count` samples with
/// `content` as a raw dictionary and count what the compressed blocks hold.
fn count_samples(
    counts: &mut EntropyCounts,
    content: &[u8],
    samples: &SampleSet<'_>,
    count: usize,
    level: i32,
) -> io::Result<()> {
    let dictionary = crate::decoding::Dictionary::from_raw_content(0, content.to_vec())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let recorder = Recorder {
        inner: MatchGeneratorDriver::new(BLOCK_SIZE_MAX, 1),
        armed: false,
        literals: [0; 256],
        sequences: Vec::new(),
    };
    let mut compressor: FrameCompressor<&[u8], Vec<u8>, Recorder> =
        FrameCompressor::new_with_matcher(recorder, CompressionLevel::from_level(level));
    compressor
        .set_encoder_dictionary(EncoderDictionary::from_dictionary(dictionary))
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    // Every sample runs the parameters of the average one with this content,
    // as upstream's analysis does (`ZSTD_getParams(level, averageSampleSize,
    // dictSize)`): one set of tables for the whole pass, which also keeps the
    // dictionary resident from one sample to the next instead of indexing it
    // again.
    let average = samples.leading(count).len() / count.max(1);
    let params = LevelParameters::for_level(
        level,
        Some(average as u64).filter(|&size| size != 0),
        content.len(),
    );
    compressor.set_parameters(
        &CompressionParameters::builder(CompressionLevel::from_level(level))
            .window_log(params.window_log)
            .hash_log(params.hash_log)
            .chain_log(params.chain_log)
            .search_log(params.search_log)
            .min_match(params.min_match)
            .target_length(params.target_length)
            .strategy(params.strategy)
            .build()
            .expect("the level table's parameters are in range"),
    );
    // Only a sample's first block is counted, `MIN(128 KiB, 1 << windowLog)`
    // bytes compressed as one block (upstream zstd `ZDICT_countEStats`). The
    // rest of a sample is not fed at all.
    let block_size = BLOCK_SIZE_MAX.min(1 << params.window_log);
    // A frame's window is fitted to its source, and upstream's analysis sizes
    // it for the sample and the content together (`ZSTD_adjustCParams`,
    // `tSize = srcSize + dictSize`), so that is the size every frame hints.
    let sized_as = (average + content.len()) as u64;
    let mut frame = Vec::new();
    for index in 0..count {
        let sample = samples.sample(index);
        let block = &sample[..sample.len().min(block_size)];
        if block.is_empty() {
            continue;
        }
        frame.clear();
        compressor.matcher_mut().armed = true;
        compressor.set_source(block);
        compressor.set_drain(frame);
        compressor.set_source_size_hint(sized_as);
        compressor.compress();
        frame = compressor.take_drain().expect("the drain was set above");
        let fast_offset_codes = compressor.uses_fast_offset_codes();
        let recorder = compressor.matcher_mut();
        // A block written raw holds no sequences to learn from, and its bytes
        // are no literals of any compressed block (upstream skips it too).
        if first_block_is_compressed(&frame) {
            for (total, &seen) in counts.literals.iter_mut().zip(&recorder.literals) {
                *total += seen as usize;
            }
            let mut reps = START_REPS;
            for &(ll, offset, ml) in &recorder.sequences {
                // The code the block wrote for this offset, under its policy.
                let off_base = if fast_offset_codes {
                    encode_offset_with_history_fast(offset, ll, &mut reps)
                } else {
                    encode_offset_with_history(offset, ll, &mut reps)
                };
                counts.literal_lengths[encode_literal_length(ll).0 as usize] += 1;
                counts.match_lengths[encode_match_len(ml).0 as usize] += 1;
                if let Some(slot) = counts
                    .offset_codes
                    .get_mut(encode_offset(off_base).0 as usize)
                {
                    *slot += 1;
                }
            }
        }
        recorder.literals = [0; 256];
        recorder.sequences.clear();
    }
    Ok(())
}

/// Whether `frame`'s first block is a compressed one.
fn first_block_is_compressed(frame: &[u8]) -> bool {
    let Ok((_, header_len)) = crate::decoding::frame::read_frame_header_with_format(frame, false)
    else {
        return false;
    };
    // Block_Type is bits 1-2 of the block header (RFC 8878 3.1.1.2).
    frame
        .get(usize::from(header_len))
        .is_some_and(|&byte| (byte >> 1) & 3 == 2)
}

/// The literals code for `counts`, its weights scaled to its longest code
/// rather than to the length limit, as upstream writes it (`HUF_writeCTable`
/// takes the `maxNbBits` its build returned): the same codes, and the table
/// log a decoder derives from the weights is the one the code needs.
fn literals_table(counts: &[usize; 256]) -> HuffmanTable {
    let table = HuffmanTable::build_limited(counts, HUF_MAX_BITS);
    let longest = (0..=255u8)
        .filter_map(|symbol| table.num_bits_for_symbol(symbol))
        .max()
        .map_or(HUF_MAX_BITS, usize::from);
    if longest < HUF_MAX_BITS {
        HuffmanTable::build_limited(counts, longest)
    } else {
        table
    }
}

/// The production matcher, recording the literals and sequences of the first
/// block it matches after being armed. Everything else is forwarded, so frames
/// compress as any compressor's would, dictionary reuse included.
struct Recorder {
    inner: MatchGeneratorDriver,
    /// Whether the next matched block is recorded; cleared once it is.
    armed: bool,
    literals: [u32; 256],
    /// Literal length, offset and match length of each sequence.
    sequences: Vec<(u32, u32, u32)>,
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
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.inner.skip_matching_with_hint(incompressible_hint);
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        if !core::mem::take(&mut self.armed) {
            self.inner.start_matching(handle_sequence);
            return;
        }
        let Self {
            inner,
            literals,
            sequences,
            ..
        } = self;
        inner.start_matching(|sequence| {
            match &sequence {
                Sequence::Literals { literals: bytes } => {
                    for &byte in *bytes {
                        literals[usize::from(byte)] += 1;
                    }
                }
                Sequence::Triple {
                    literals: bytes,
                    offset,
                    match_len,
                } => {
                    for &byte in *bytes {
                        literals[usize::from(byte)] += 1;
                    }
                    // Lengths and offsets of one block, far below `u32::MAX`.
                    sequences.push((bytes.len() as u32, *offset as u32, *match_len as u32));
                }
            }
            handle_sequence(sequence);
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

    fn apply_parameters(&mut self, params: &CompressionParameters) {
        self.inner.apply_parameters(params);
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
