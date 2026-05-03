use alloc::{boxed::Box, vec::Vec};

use crate::{
    bit_io::BitWriter,
    blocks::block::BlockType,
    encoding::block_header::BlockHeader,
    encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable},
    encoding::{Matcher, Sequence},
    fse::fse_encoder::{FSETable, build_table_from_symbol_counts},
    huff0::huff0_encoder,
};

const MIN_SEQUENCES_BLOCK_SPLITTING: usize = 300;
const MAX_NB_BLOCK_SPLITS: usize = 196;

/// Donor `kInverseProbabilityLog256`: floor(-log2(x / 256) * 256).
const INVERSE_PROBABILITY_LOG_256: [usize; 256] = [
    0, 2048, 1792, 1642, 1536, 1453, 1386, 1329, 1280, 1236, 1197, 1162, 1130, 1100, 1073, 1047,
    1024, 1001, 980, 960, 941, 923, 906, 889, 874, 859, 844, 830, 817, 804, 791, 779, 768, 756,
    745, 734, 724, 714, 704, 694, 685, 676, 667, 658, 650, 642, 633, 626, 618, 610, 603, 595, 588,
    581, 574, 567, 561, 554, 548, 542, 535, 529, 523, 517, 512, 506, 500, 495, 489, 484, 478, 473,
    468, 463, 458, 453, 448, 443, 438, 434, 429, 424, 420, 415, 411, 407, 402, 398, 394, 390, 386,
    382, 377, 373, 370, 366, 362, 358, 354, 350, 347, 343, 339, 336, 332, 329, 325, 322, 318, 315,
    311, 308, 305, 302, 298, 295, 292, 289, 286, 282, 279, 276, 273, 270, 267, 264, 261, 258, 256,
    253, 250, 247, 244, 241, 239, 236, 233, 230, 228, 225, 222, 220, 217, 215, 212, 209, 207, 204,
    202, 199, 197, 194, 192, 190, 187, 185, 182, 180, 178, 175, 173, 171, 168, 166, 164, 162, 159,
    157, 155, 153, 151, 149, 146, 144, 142, 140, 138, 136, 134, 132, 130, 128, 126, 123, 121, 119,
    117, 115, 114, 112, 110, 108, 106, 104, 102, 100, 98, 96, 94, 93, 91, 89, 87, 85, 83, 82, 80,
    78, 76, 74, 73, 71, 69, 67, 66, 64, 62, 61, 59, 57, 55, 54, 52, 50, 49, 47, 46, 44, 42, 41, 39,
    37, 36, 34, 33, 31, 30, 28, 26, 25, 23, 22, 20, 19, 17, 16, 14, 13, 11, 10, 8, 7, 5, 4, 2, 1,
];

/// Compile-time guarantee that MAX_BLOCK_SIZE fits in the 18-bit size format.
const _: () = assert!(crate::common::MAX_BLOCK_SIZE <= 262_143);

struct EncodedBlockParts {
    literals: Vec<u8>,
    sequences: Vec<RawSequence>,
}

struct SequencePrefixSums {
    lit: Vec<usize>,
    ml: Vec<usize>,
}

impl SequencePrefixSums {
    fn build(sequences: &[RawSequence]) -> Self {
        let mut lit = Vec::with_capacity(sequences.len() + 1);
        let mut ml = Vec::with_capacity(sequences.len() + 1);
        lit.push(0);
        ml.push(0);
        for seq in sequences {
            lit.push(*lit.last().unwrap_or(&0) + seq.ll as usize);
            ml.push(*ml.last().unwrap_or(&0) + seq.ml as usize);
        }
        Self { lit, ml }
    }

    fn lit_range(&self, start: usize, end: usize) -> usize {
        self.lit[end] - self.lit[start]
    }

    fn ml_range(&self, start: usize, end: usize) -> usize {
        self.ml[end] - self.ml[start]
    }
}

#[derive(Clone, Copy)]
struct RawSequence {
    ll: u32,
    ml: u32,
    offset: u32,
}

struct EntropyOnlyMatcher;

enum HuffmanTableUpdate {
    New(huff0_encoder::HuffmanTable),
    Reused,
    Cleared,
}

impl Matcher for EntropyOnlyMatcher {
    fn get_next_space(&mut self) -> Vec<u8> {
        unreachable!("entropy estimator never requests input space")
    }

    fn get_last_space(&mut self) -> &[u8] {
        unreachable!("entropy estimator never reads source bytes")
    }

    fn commit_space(&mut self, _space: Vec<u8>) {
        unreachable!("entropy estimator never commits input")
    }

    fn skip_matching(&mut self) {
        unreachable!("entropy estimator never updates match state")
    }

    fn start_matching(&mut self, _handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        unreachable!("entropy estimator never generates sequences")
    }

    fn reset(&mut self, _level: crate::encoding::CompressionLevel) {}

    fn window_size(&self) -> u64 {
        0
    }
}

/// A block of [`crate::common::BlockType::Compressed`]
pub fn compress_block<M: Matcher>(state: &mut CompressState<M>, output: &mut Vec<u8>) {
    let parts = collect_block_parts(state);
    encode_block_parts(state, &parts.literals, &parts.sequences, output);
}

pub(crate) fn compress_block_with_post_split<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    output: &mut Vec<u8>,
) {
    let parts = collect_block_parts(state);
    if parts.sequences.len() <= 4 {
        let source_len = state.matcher.get_last_space().len();
        let mut compressed_scratch = Vec::new();
        let emitted_raw = emit_single_sequence_block(
            state,
            last_block,
            source_len,
            &parts.literals,
            &parts.sequences,
            output,
            &mut compressed_scratch,
        );
        if emitted_raw {
            output.extend_from_slice(state.matcher.get_last_space());
        }
        return;
    }

    let mut partitions = Vec::new();
    let prefix_sums = SequencePrefixSums::build(&parts.sequences);
    let mut estimator = SplitEstimator {
        parts: &parts,
        prefix_sums: &prefix_sums,
        huff_table: state.last_huff_table.as_ref(),
        offset_hist: state.offset_hist,
        ll_previous: state.fse_tables.ll_previous.clone(),
        ml_previous: state.fse_tables.ml_previous.clone(),
        of_previous: state.fse_tables.of_previous.clone(),
        scratch_state: CompressState {
            matcher: EntropyOnlyMatcher,
            last_huff_table: state.last_huff_table.clone(),
            fse_tables: clone_fse_tables(&state.fse_tables),
            offset_hist: state.offset_hist,
        },
        scratch_output: Vec::new(),
        scratch_sequences: Vec::new(),
    };
    estimator.derive_block_splits(0, parts.sequences.len(), &mut partitions);
    partitions.push(parts.sequences.len());

    let mut compressed_scratch = Vec::new();
    let mut seq_start = 0usize;
    let mut lit_start = 0usize;
    let mut src_start = 0usize;
    for (partition_idx, &seq_end) in partitions.iter().enumerate() {
        let last_partition = partition_idx + 1 == partitions.len();
        let chunk_lit_len = prefix_sums.lit_range(seq_start, seq_end);
        let chunk_match_len = prefix_sums.ml_range(seq_start, seq_end);
        let lit_end = if last_partition {
            parts.literals.len()
        } else {
            lit_start + chunk_lit_len
        };
        let src_size = if last_partition {
            state.matcher.get_last_space().len() - src_start
        } else {
            chunk_lit_len + chunk_match_len
        };
        let emitted_raw = emit_single_sequence_block(
            state,
            last_block && last_partition,
            src_size,
            &parts.literals[lit_start..lit_end],
            &parts.sequences[seq_start..seq_end],
            output,
            &mut compressed_scratch,
        );
        if emitted_raw {
            output.extend_from_slice(
                &state.matcher.get_last_space()[src_start..src_start + src_size],
            );
        }
        seq_start = seq_end;
        lit_start = lit_end;
        src_start += src_size;
    }
}

fn collect_block_parts<M: Matcher>(state: &mut CompressState<M>) -> EncodedBlockParts {
    let src_len = state.matcher.get_last_space().len();
    let mut literals_vec = Vec::with_capacity(src_len);
    let mut sequences = Vec::with_capacity(src_len / 8);
    state.matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => literals_vec.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            let ll = literals.len() as u32;
            literals_vec.extend_from_slice(literals);
            sequences.push(RawSequence {
                ll,
                ml: match_len as u32,
                offset: offset as u32,
            });
        }
    });
    EncodedBlockParts {
        literals: literals_vec,
        sequences,
    }
}

fn encode_block_parts<M: Matcher>(
    state: &mut CompressState<M>,
    literals_vec: &[u8],
    raw_sequences: &[RawSequence],
    output: &mut Vec<u8>,
) {
    let mut sequences = Vec::new();
    encode_block_parts_with_sequence_scratch(
        state,
        literals_vec,
        raw_sequences,
        output,
        &mut sequences,
    );
}

fn encode_block_parts_with_sequence_scratch<M: Matcher>(
    state: &mut CompressState<M>,
    literals_vec: &[u8],
    raw_sequences: &[RawSequence],
    output: &mut Vec<u8>,
    sequences: &mut Vec<crate::blocks::sequence_section::Sequence>,
) {
    encode_raw_sequences_into(raw_sequences, &mut state.offset_hist, sequences);

    // literals section

    let mut writer = BitWriter::from(output);
    if literals_vec.len() >= 8 && all_bytes_identical(literals_vec) {
        rle_literals(literals_vec, &mut writer);
        state.last_huff_table = None;
    } else if literals_vec.len() >= 8 {
        match compress_literals(literals_vec, state.last_huff_table.as_ref(), &mut writer) {
            HuffmanTableUpdate::New(table) => {
                state.last_huff_table.replace(table);
            }
            HuffmanTableUpdate::Reused => {}
            HuffmanTableUpdate::Cleared => {
                state.last_huff_table = None;
            }
        }
    } else {
        raw_literals(literals_vec, &mut writer);
        state.last_huff_table = None;
    }

    // sequences section

    if sequences.is_empty() {
        writer.write_bits(0u8, 8);
    } else {
        encode_seqnum(sequences.len(), &mut writer);

        // Choose the tables
        let ll_mode = choose_table(
            state.fse_tables.ll_previous.as_ref(),
            &state.fse_tables.ll_default,
            sequences.iter().map(|seq| encode_literal_length(seq.ll).0),
            9,
        );
        let ml_mode = choose_table(
            state.fse_tables.ml_previous.as_ref(),
            &state.fse_tables.ml_default,
            sequences.iter().map(|seq| encode_match_len(seq.ml).0),
            9,
        );
        let of_mode = choose_table(
            state.fse_tables.of_previous.as_ref(),
            &state.fse_tables.of_default,
            sequences.iter().map(|seq| encode_offset(seq.of).0),
            8,
        );

        writer.write_bits(encode_fse_table_modes(&ll_mode, &ml_mode, &of_mode), 8);

        encode_table(&ll_mode, &mut writer);
        encode_table(&of_mode, &mut writer);
        encode_table(&ml_mode, &mut writer);

        encode_sequences(
            sequences,
            &mut writer,
            &ll_mode,
            &ml_mode,
            &of_mode,
            &state.fse_tables,
        );

        let ll_last = into_last_used_table(ll_mode);
        let ml_last = into_last_used_table(ml_mode);
        let of_last = into_last_used_table(of_mode);
        remember_last_used_tables(&mut state.fse_tables, ll_last, ml_last, of_last);
    }
    writer.flush();
}

fn emit_single_sequence_block<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    source_len: usize,
    literals: &[u8],
    sequences: &[RawSequence],
    output: &mut Vec<u8>,
    compressed: &mut Vec<u8>,
) -> bool {
    let saved_offset_hist = state.offset_hist;
    let saved_huff_table = state.last_huff_table.clone();
    let saved_ll_previous = state.fse_tables.ll_previous.clone();
    let saved_ml_previous = state.fse_tables.ml_previous.clone();
    let saved_of_previous = state.fse_tables.of_previous.clone();
    compressed.clear();
    encode_block_parts(state, literals, sequences, compressed);
    let min_gain = (source_len >> 8) + 2;
    if compressed.len() >= source_len.saturating_sub(min_gain) {
        state.offset_hist = saved_offset_hist;
        state.last_huff_table = saved_huff_table;
        state.fse_tables.ll_previous = saved_ll_previous;
        state.fse_tables.ml_previous = saved_ml_previous;
        state.fse_tables.of_previous = saved_of_previous;
        let header = BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: source_len as u32,
        };
        header.serialize(output);
        true
    } else {
        let header = BlockHeader {
            last_block,
            block_type: BlockType::Compressed,
            block_size: compressed.len() as u32,
        };
        header.serialize(output);
        output.extend_from_slice(compressed);
        false
    }
}

fn encode_raw_sequences_into(
    raw_sequences: &[RawSequence],
    offset_hist: &mut [u32; 3],
    out: &mut Vec<crate::blocks::sequence_section::Sequence>,
) {
    out.clear();
    if out.capacity() < raw_sequences.len() {
        out.reserve_exact(raw_sequences.len() - out.capacity());
    }
    out.extend(
        raw_sequences
            .iter()
            .map(|seq| crate::blocks::sequence_section::Sequence {
                ll: seq.ll,
                ml: seq.ml,
                of: encode_offset_with_history(seq.offset, seq.ll, offset_hist),
            }),
    );
}

fn clone_fse_tables(fse_tables: &FseTables) -> FseTables {
    FseTables {
        ll_default: fse_tables.ll_default.clone(),
        ll_previous: fse_tables.ll_previous.clone(),
        ml_default: fse_tables.ml_default.clone(),
        ml_previous: fse_tables.ml_previous.clone(),
        of_default: fse_tables.of_default.clone(),
        of_previous: fse_tables.of_previous.clone(),
    }
}

struct SplitEstimator<'a> {
    parts: &'a EncodedBlockParts,
    prefix_sums: &'a SequencePrefixSums,
    huff_table: Option<&'a huff0_encoder::HuffmanTable>,
    offset_hist: [u32; 3],
    ll_previous: Option<PreviousFseTable>,
    ml_previous: Option<PreviousFseTable>,
    of_previous: Option<PreviousFseTable>,
    scratch_state: CompressState<EntropyOnlyMatcher>,
    scratch_output: Vec<u8>,
    scratch_sequences: Vec<crate::blocks::sequence_section::Sequence>,
}

impl SplitEstimator<'_> {
    fn estimate_subblock_size(&mut self, start_idx: usize, end_idx: usize) -> usize {
        let lit_start = self.prefix_sums.lit[start_idx];
        let lit_len = self.prefix_sums.lit_range(start_idx, end_idx);
        let match_len = self.prefix_sums.ml_range(start_idx, end_idx);
        let lit_end = if end_idx == self.parts.sequences.len() {
            self.parts.literals.len()
        } else {
            lit_start + lit_len
        };
        self.scratch_state.last_huff_table = self.huff_table.cloned();
        self.scratch_state.fse_tables.ll_previous = self.ll_previous.clone();
        self.scratch_state.fse_tables.ml_previous = self.ml_previous.clone();
        self.scratch_state.fse_tables.of_previous = self.of_previous.clone();
        self.scratch_state.offset_hist = self.offset_hist;
        self.scratch_output.clear();
        encode_block_parts_with_sequence_scratch(
            &mut self.scratch_state,
            &self.parts.literals[lit_start..lit_end],
            &self.parts.sequences[start_idx..end_idx],
            &mut self.scratch_output,
            &mut self.scratch_sequences,
        );
        let source_len = (lit_end - lit_start) + match_len;
        let min_gain = (source_len >> 8) + 2;
        let emitted_payload = if self.scratch_output.len() >= source_len.saturating_sub(min_gain) {
            source_len
        } else {
            self.scratch_output.len()
        };
        emitted_payload + 3
    }

    fn derive_block_splits(
        &mut self,
        start_idx: usize,
        end_idx: usize,
        partitions: &mut Vec<usize>,
    ) {
        if end_idx - start_idx < MIN_SEQUENCES_BLOCK_SPLITTING
            || partitions.len() >= MAX_NB_BLOCK_SPLITS
        {
            return;
        }
        let mid_idx = (start_idx + end_idx) / 2;
        let full = self.estimate_subblock_size(start_idx, end_idx);
        let first = self.estimate_subblock_size(start_idx, mid_idx);
        let second = self.estimate_subblock_size(mid_idx, end_idx);
        let estimator_tolerance = full / 512;
        if first + second < full + estimator_tolerance {
            self.derive_block_splits(start_idx, mid_idx, partitions);
            if partitions.len() >= MAX_NB_BLOCK_SPLITS {
                return;
            }
            partitions.push(mid_idx);
            self.derive_block_splits(mid_idx, end_idx, partitions);
        }
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum FseTableMode<'a> {
    Predefined(&'a FSETable),
    Encoded(FSETable),
    Rle(u8),
    RepeatLast(&'a PreviousFseTable),
}

impl FseTableMode<'_> {
    pub fn as_table<'a>(&'a self, default: &'a FSETable) -> Option<&'a FSETable> {
        match self {
            Self::Predefined(t) => Some(t),
            Self::RepeatLast(previous) => previous.as_table(default),
            Self::Encoded(t) => Some(t),
            Self::Rle(_) => None,
        }
    }
}

fn entropy_cost(counts: &[usize; 256], max_symbol: usize, total: usize) -> usize {
    let mut cost = 0usize;
    for &count in counts.iter().take(max_symbol + 1) {
        if count == 0 {
            continue;
        }
        let mut norm = 256 * count / total;
        if norm == 0 {
            norm = 1;
        }
        cost += count * INVERSE_PROBABILITY_LOG_256[norm];
    }
    cost >> 8
}

fn cross_entropy_cost(counts: &[usize; 256], max_symbol: usize, table: &FSETable) -> Option<usize> {
    let acc_log = table.acc_log();
    if acc_log > 8 {
        return None;
    }
    let shift = 8 - acc_log;
    let mut cost = 0usize;
    for (symbol, &count) in counts.iter().enumerate().take(max_symbol + 1) {
        if count == 0 {
            continue;
        }
        let prob = table.symbol_probability(symbol as u8);
        if prob == 0 {
            return None;
        }
        let norm = if prob == -1 { 1 } else { prob as usize };
        let norm_256 = norm << shift;
        if norm_256 == 0 || norm_256 >= 256 {
            return None;
        }
        cost += count * INVERSE_PROBABILITY_LOG_256[norm_256];
    }
    Some(cost >> 8)
}

fn fse_bit_cost(counts: &[usize; 256], max_symbol: usize, table: &FSETable) -> Option<usize> {
    let table_log = table.acc_log() as usize;
    let table_size = 1usize << table_log;
    let mut cost = 0usize;
    for (symbol, &count) in counts.iter().enumerate().take(max_symbol + 1) {
        if count == 0 {
            continue;
        }
        let prob = table.symbol_probability(symbol as u8);
        if prob == 0 {
            return None;
        }
        let delta_nb_bits = match prob {
            -1 | 1 => (table_log << 16).saturating_sub(table_size),
            prob if prob > 1 => {
                let prob = prob as usize;
                let max_bits_out = table_log - (prob - 1).ilog2() as usize;
                let min_state_plus = prob << max_bits_out;
                (max_bits_out << 16).saturating_sub(min_state_plus)
            }
            _ => return None,
        };
        let min_nb_bits = delta_nb_bits >> 16;
        let threshold = (min_nb_bits + 1) << 16;
        if delta_nb_bits + table_size > threshold {
            return None;
        }
        let delta_from_threshold = threshold - (delta_nb_bits + table_size);
        let normalized_delta = (delta_from_threshold << 8) >> table_log;
        let bit_cost = (min_nb_bits + 1) * 256 - normalized_delta;
        let bad_cost = (table_log + 1) << 8;
        if bit_cost >= bad_cost {
            return None;
        }
        cost += count * bit_cost;
    }
    Some(cost >> 8)
}

fn choose_table<'a>(
    previous: Option<&'a PreviousFseTable>,
    default_table: &'a FSETable,
    data: impl Iterator<Item = u8>,
    max_log: u8,
) -> FseTableMode<'a> {
    // Collect symbol distribution
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for symbol in data {
        counts[symbol as usize] += 1;
        total += 1;
    }

    if total == 0 {
        return FseTableMode::Predefined(default_table);
    }

    // Build a new table from the actual data distribution
    let max_symbol = counts
        .iter()
        .rposition(|&count| count > 0)
        .unwrap_or_default();
    let distinct_symbols = counts.iter().filter(|&&count| count > 0).take(2).count();
    if distinct_symbols == 1 {
        let symbol = max_symbol as u8;
        if let Some(PreviousFseTable::Rle(prev_symbol)) = previous
            && *prev_symbol == symbol
        {
            return FseTableMode::RepeatLast(previous.unwrap());
        }
        if total <= 2 && default_table.symbol_probability(symbol) != 0 {
            return FseTableMode::Predefined(default_table);
        }
        return FseTableMode::Rle(symbol);
    }

    let use_low_prob_count = total >= 2048;
    let new_table = (distinct_symbols > 1).then(|| {
        build_table_from_symbol_counts(&counts[..=max_symbol], max_log, use_low_prob_count)
    });

    // Mirror donor `ZSTD_selectEncodingType()` for optimal strategies:
    // compare default cross-entropy, repeat-table FSE bit cost, and
    // compressed table header plus entropy-bound payload cost.
    let new_total_cost = new_table.as_ref().map(|table| {
        table
            .table_header_bits()
            .saturating_add(entropy_cost(&counts, max_symbol, total))
    });

    let predefined_cost = cross_entropy_cost(&counts, max_symbol, default_table);

    let previous_cost = previous.and_then(|previous| {
        previous
            .as_table(default_table)
            .and_then(|table| fse_bit_cost(&counts, max_symbol, table))
    });

    enum Choice {
        Previous,
        Predefined,
        New,
    }

    let mut best: Option<(usize, Choice)> = None;

    if let Some(cost) = previous_cost {
        best = Some((cost, Choice::Previous));
    }

    if let Some(cost) = predefined_cost {
        match best {
            Some((best_cost, _)) if best_cost <= cost => {}
            _ => best = Some((cost, Choice::Predefined)),
        }
    }

    if let Some(cost) = new_total_cost {
        match best {
            Some((best_cost, _)) if best_cost <= cost => {}
            _ => best = Some((cost, Choice::New)),
        }
    }

    match best.map(|(_, choice)| choice) {
        Some(Choice::Previous) => previous
            .map(FseTableMode::RepeatLast)
            .unwrap_or(FseTableMode::Predefined(default_table)),
        Some(Choice::Predefined) => FseTableMode::Predefined(default_table),
        Some(Choice::New) => new_table
            .map(FseTableMode::Encoded)
            .unwrap_or(FseTableMode::Predefined(default_table)),
        None => {
            let fallback_counts = [counts[0], 0];
            let fallback = if max_symbol == 0 {
                // `build_table_from_symbol_counts` needs at least two entries, so
                // single-symbol streams use a phantom zero-count second slot here.
                build_table_from_symbol_counts(&fallback_counts, max_log, use_low_prob_count)
            } else {
                build_table_from_symbol_counts(&counts[..=max_symbol], max_log, use_low_prob_count)
            };
            FseTableMode::Encoded(fallback)
        }
    }
}

fn encode_table(mode: &FseTableMode<'_>, writer: &mut BitWriter<&mut Vec<u8>>) {
    match mode {
        FseTableMode::Predefined(_) => {}
        FseTableMode::RepeatLast(_) => {}
        FseTableMode::Encoded(table) => table.write_table(writer),
        FseTableMode::Rle(symbol) => writer.write_bits(*symbol, 8),
    }
}

fn encode_fse_table_modes(
    ll_mode: &FseTableMode<'_>,
    ml_mode: &FseTableMode<'_>,
    of_mode: &FseTableMode<'_>,
) -> u8 {
    fn mode_to_bits(mode: &FseTableMode<'_>) -> u8 {
        match mode {
            FseTableMode::Predefined(_) => 0,
            FseTableMode::Rle(_) => 1,
            FseTableMode::Encoded(_) => 2,
            FseTableMode::RepeatLast(_) => 3,
        }
    }
    mode_to_bits(ll_mode) << 6 | mode_to_bits(of_mode) << 4 | mode_to_bits(ml_mode) << 2
}

fn remember_last_used_tables(
    fse_tables: &mut FseTables,
    ll_last: Option<PreviousFseTable>,
    ml_last: Option<PreviousFseTable>,
    of_last: Option<PreviousFseTable>,
) {
    remember_last_used_table(&mut fse_tables.ll_previous, ll_last);
    remember_last_used_table(&mut fse_tables.ml_previous, ml_last);
    remember_last_used_table(&mut fse_tables.of_previous, of_last);
}

#[cfg(test)]
fn previous_table<'a>(
    previous: Option<&'a PreviousFseTable>,
    default: &'a FSETable,
) -> Option<&'a FSETable> {
    previous.and_then(|previous| previous.as_table(default))
}

fn remember_last_used_table(slot: &mut Option<PreviousFseTable>, next: Option<PreviousFseTable>) {
    if let Some(next) = next {
        *slot = Some(next);
    }
}

fn into_last_used_table(mode: FseTableMode<'_>) -> Option<PreviousFseTable> {
    match mode {
        FseTableMode::Encoded(table) => Some(PreviousFseTable::Custom(Box::new(table))),
        FseTableMode::Predefined(_) => Some(PreviousFseTable::Default),
        FseTableMode::Rle(symbol) => Some(PreviousFseTable::Rle(symbol)),
        FseTableMode::RepeatLast(_) => None,
    }
}

fn encode_sequences(
    sequences: &[crate::blocks::sequence_section::Sequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    ll_mode: &FseTableMode<'_>,
    ml_mode: &FseTableMode<'_>,
    of_mode: &FseTableMode<'_>,
    defaults: &FseTables,
) {
    fn mode_table<'a>(mode: &'a FseTableMode<'_>, default: &'a FSETable) -> Option<&'a FSETable> {
        mode.as_table(default)
    }

    let sequence = sequences[sequences.len() - 1];
    let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
    let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
    let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);
    let ll_table = mode_table(ll_mode, &defaults.ll_default);
    let ml_table = mode_table(ml_mode, &defaults.ml_default);
    let of_table = mode_table(of_mode, &defaults.of_default);
    let mut ll_state = ll_table.map(|table| table.start_state(ll_code));
    let mut ml_state = ml_table.map(|table| table.start_state(ml_code));
    let mut of_state = of_table.map(|table| table.start_state(of_code));

    writer.write_bits(ll_add_bits, ll_num_bits);
    writer.write_bits(ml_add_bits, ml_num_bits);
    writer.write_bits(of_add_bits, of_num_bits);

    // encode backwards so the decoder reads the first sequence first
    if sequences.len() > 1 {
        for sequence in (0..=sequences.len() - 2).rev() {
            let sequence = sequences[sequence];
            let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
            let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
            let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);

            if let (Some(table), Some(state)) = (of_table, of_state) {
                let next = table.next_state(of_code, state.index);
                let diff = state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                of_state = Some(next);
            }
            if let (Some(table), Some(state)) = (ml_table, ml_state) {
                let next = table.next_state(ml_code, state.index);
                let diff = state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                ml_state = Some(next);
            }
            if let (Some(table), Some(state)) = (ll_table, ll_state) {
                let next = table.next_state(ll_code, state.index);
                let diff = state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                ll_state = Some(next);
            }

            writer.write_bits(ll_add_bits, ll_num_bits);
            writer.write_bits(ml_add_bits, ml_num_bits);
            writer.write_bits(of_add_bits, of_num_bits);
        }
    }
    if let (Some(state), Some(table)) = (ml_state, ml_table) {
        writer.write_bits(state.index as u64, table.table_size.ilog2() as usize);
    }
    if let (Some(state), Some(table)) = (of_state, of_table) {
        writer.write_bits(state.index as u64, table.table_size.ilog2() as usize);
    }
    if let (Some(state), Some(table)) = (ll_state, ll_table) {
        writer.write_bits(state.index as u64, table.table_size.ilog2() as usize);
    }

    let bits_to_fill = writer.misaligned();
    if bits_to_fill == 0 {
        writer.write_bits(1u32, 8);
    } else {
        writer.write_bits(1u32, bits_to_fill);
    }
}

fn encode_seqnum(seqnum: usize, writer: &mut BitWriter<impl AsMut<Vec<u8>>>) {
    const UPPER_LIMIT: usize = 0xFFFF + 0x7F00;
    match seqnum {
        1..=127 => writer.write_bits(seqnum as u32, 8),
        128..=0x7FFF => {
            let upper = ((seqnum >> 8) | 0x80) as u8;
            let lower = seqnum as u8;
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        0x8000..=UPPER_LIMIT => {
            let encode = seqnum - 0x7F00;
            let upper = (encode >> 8) as u8;
            let lower = encode as u8;
            writer.write_bits(255u8, 8);
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        _ => unreachable!(),
    }
}

fn encode_literal_length(len: u32) -> (u8, u32, usize) {
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

fn encode_match_len(len: u32) -> (u8, u32, usize) {
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

/// Convert an actual byte offset into the encoded offset code, using repeat offset
/// history per RFC 8878 §3.1.2.5. Updates `offset_hist` in place.
///
/// Encoded offset codes: 1/2/3 = repeat offsets, N+3 = new absolute offset N.
pub(in crate::encoding) fn encode_offset_with_history(
    actual_offset: u32,
    lit_len: u32,
    offset_hist: &mut [u32; 3],
) -> u32 {
    let encoded = if lit_len > 0 {
        if actual_offset == offset_hist[0] {
            1
        } else if actual_offset == offset_hist[1] {
            2
        } else if actual_offset == offset_hist[2] {
            3
        } else {
            actual_offset + 3
        }
    } else {
        // When lit_len == 0, repeat offset mapping shifts per RFC 8878:
        // code 1 → rep[1], code 2 → rep[2], code 3 → rep[0]-1
        if actual_offset == offset_hist[1] {
            1
        } else if actual_offset == offset_hist[2] {
            2
        } else if actual_offset == offset_hist[0].wrapping_sub(1) && offset_hist[0] > 1 {
            3
        } else {
            actual_offset + 3
        }
    };

    // Update history (same rules as decoder)
    if lit_len > 0 {
        match encoded {
            1 => { /* rep[0] stays the same */ }
            2 => {
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
            _ => {
                offset_hist[2] = offset_hist[1];
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
        }
    } else {
        match encoded {
            1 => {
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
            2 => {
                offset_hist[2] = offset_hist[1];
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
            _ => {
                offset_hist[2] = offset_hist[1];
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
        }
    }

    encoded
}

fn encode_offset(len: u32) -> (u8, u32, usize) {
    let log = len.ilog2();
    let lower = len & ((1 << log) - 1);
    (log as u8, lower, log as usize)
}

fn all_bytes_identical(literals: &[u8]) -> bool {
    literals
        .first()
        .is_some_and(|&first| literals.iter().all(|&byte| byte == first))
}

fn write_uncompressed_literals_header(
    section_type: u8,
    literals_len: usize,
    writer: &mut BitWriter<&mut Vec<u8>>,
) {
    writer.write_bits(section_type, 2);
    match literals_len {
        0..=31 => {
            writer.write_bits(0u8, 1);
            writer.write_bits(literals_len as u8, 5);
        }
        32..=4095 => {
            writer.write_bits(1u8, 2);
            writer.write_bits(literals_len as u16, 12);
        }
        _ => {
            writer.write_bits(3u8, 2);
            writer.write_bits(literals_len as u32, 20);
        }
    }
}

fn raw_literals(literals: &[u8], writer: &mut BitWriter<&mut Vec<u8>>) {
    write_uncompressed_literals_header(0, literals.len(), writer);
    writer.append_bytes(literals);
}

fn rle_literals(literals: &[u8], writer: &mut BitWriter<&mut Vec<u8>>) {
    debug_assert!(!literals.is_empty());
    debug_assert!(all_bytes_identical(literals));
    write_uncompressed_literals_header(1, literals.len(), writer);
    writer.append_bytes(&literals[..1]);
}

fn compress_literals(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    writer: &mut BitWriter<&mut Vec<u8>>,
) -> HuffmanTableUpdate {
    let reset_idx = writer.index();

    let new_encoder_table = huff0_encoder::HuffmanTable::build_from_data(literals);

    let Some(new_table_description_size) = new_encoder_table.writeable_table_description_size()
    else {
        raw_literals(literals, writer);
        return HuffmanTableUpdate::Cleared;
    };
    let new_payload_estimate = new_encoder_table
        .estimate_compressed_size(literals)
        .unwrap_or(literals.len());
    let (encoder_table, new_table) = if let Some(table) = last_table {
        if let Some(old_payload_estimate) = table.estimate_compressed_size(literals) {
            if old_payload_estimate <= new_table_description_size + new_payload_estimate
                || new_table_description_size + 12 >= literals.len()
            {
                (table, false)
            } else {
                (&new_encoder_table, true)
            }
        } else {
            (&new_encoder_table, true)
        }
    } else {
        (&new_encoder_table, true)
    };

    if new_table {
        writer.write_bits(2u8, 2); // compressed literals type
    } else {
        writer.write_bits(3u8, 2); // treeless compressed literals type
    }

    // RFC 8878 §3.1.1.3.1.1 Size_Format (spec limits):
    //   0b00: single stream, 10-bit (≤ 1023)  |  0b01: 4 streams, 10-bit (≤ 1023)
    //   0b10: 4 streams, 14-bit (≤ 16383)     |  0b11: 4 streams, 18-bit (≤ 262143)
    //
    // Runtime: hard guard — truncated 18-bit writes produce corrupt streams.
    // Note: format args omitted intentionally to avoid uncoverable dead code in coverage.
    assert!(
        literals.len() <= 262_143,
        "literals exceed RFC 8878 18-bit size limit (262143)"
    );
    let (size_format, size_bits) = match literals.len() {
        0..256 => (0b00u8, 10),
        256..1024 => (0b01, 10),
        1024..16384 => (0b10, 14),
        _ => (0b11, 18),
    };

    writer.write_bits(size_format, 2);
    writer.write_bits(literals.len() as u32, size_bits);
    let size_index = writer.index();
    writer.write_bits(0u32, size_bits);
    let index_before = writer.index();
    let mut encoder = huff0_encoder::HuffmanEncoder::new(encoder_table, writer);
    if size_format == 0 {
        encoder.encode(literals, new_table)
    } else {
        encoder.encode4x(literals, new_table)
    };
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    // If encoded len is bigger than the raw literals we are better off just writing the raw literals here
    if total_len >= literals.len() {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
        HuffmanTableUpdate::Cleared
    } else if new_table {
        HuffmanTableUpdate::New(new_encoder_table)
    } else {
        HuffmanTableUpdate::Reused
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::{
        FseTableMode, RawSequence, choose_table, emit_single_sequence_block, encode_match_len,
        encode_offset_with_history, previous_table, remember_last_used_tables,
    };
    use crate::encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable};
    use crate::fse::fse_encoder::build_table_from_symbol_counts;
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
            fse_tables: FseTables::new(),
            offset_hist: [10, 20, 30],
        };
        let source = [0xA5; 8];
        let sequences = [RawSequence {
            ll: 0,
            ml: 5,
            offset: 20,
        }];
        let mut output = Vec::new();
        let mut compressed_scratch = Vec::new();

        let emitted_raw = emit_single_sequence_block(
            &mut state,
            true,
            source.len(),
            &[],
            &sequences,
            &mut output,
            &mut compressed_scratch,
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
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default).unwrap(),
            &fse_tables.ll_default
        ));
        assert!(tables_match(
            previous_table(fse_tables.ml_previous.as_ref(), &fse_tables.ml_default).unwrap(),
            &fse_tables.ml_default
        ));
        assert!(tables_match(
            previous_table(fse_tables.of_previous.as_ref(), &fse_tables.of_default).unwrap(),
            &fse_tables.of_default
        ));

        let sample_codes = [0u8, 1u8];
        let ll_repeat = choose_table(
            fse_tables.ll_previous.as_ref(),
            &fse_tables.ll_default,
            sample_codes.iter().copied(),
            9,
        );
        let ml_repeat = choose_table(
            fse_tables.ml_previous.as_ref(),
            &fse_tables.ml_default,
            sample_codes.iter().copied(),
            9,
        );
        let of_repeat = choose_table(
            fse_tables.of_previous.as_ref(),
            &fse_tables.of_default,
            sample_codes.iter().copied(),
            8,
        );

        assert!(matches!(ll_repeat, FseTableMode::RepeatLast(_)));
        assert!(matches!(ml_repeat, FseTableMode::RepeatLast(_)));
        assert!(matches!(of_repeat, FseTableMode::RepeatLast(_)));
    }

    #[test]
    fn remember_last_used_tables_reuses_existing_custom_slot_for_repeat() {
        let mut fse_tables = FseTables::new();
        let custom = build_table_from_symbol_counts(&[1, 1], 5, false);
        fse_tables.ll_previous = Some(PreviousFseTable::Custom(Box::new(custom)));

        let before = core::ptr::from_ref(
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default).unwrap(),
        );

        remember_last_used_tables(
            &mut fse_tables,
            None,
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
        );

        let after = core::ptr::from_ref(
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default).unwrap(),
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
            &fse_tables.ll_default,
            core::iter::repeat_n(0u8, 32),
            9,
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
        );
        assert!(matches!(mode, FseTableMode::Encoded(_)));
    }
}
