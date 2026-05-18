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

#[derive(Default)]
struct EncodedBlockParts {
    literals: Vec<u8>,
    sequences: Vec<RawSequence>,
}

#[derive(Default)]
pub(crate) struct CompressedBlockScratch {
    parts: EncodedBlockParts,
    partitions: Vec<usize>,
    prefix_sums: SequencePrefixSums,
    compressed: Vec<u8>,
    estimator_sequences: Vec<crate::blocks::sequence_section::Sequence>,
    estimator_workspace: EstimatorWorkspace,
}

impl CompressedBlockScratch {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

#[derive(Default)]
struct SequencePrefixSums {
    lit: Vec<usize>,
    ml: Vec<usize>,
}

impl SequencePrefixSums {
    fn rebuild(&mut self, sequences: &[RawSequence]) {
        self.lit.clear();
        self.ml.clear();
        // `Vec::reserve_exact(additional)` adds `additional` elements ABOVE
        // current length, not capacity. Subtracting `capacity` here would
        // request `N - cap` more, leaving the Vec with `cap = max(cap, N-cap)`
        // — still below the `N` we need whenever `cap < N/2`, forcing a
        // reallocation on the very next `push`. After `clear()` length is 0,
        // so subtracting `len()` (here always 0) is the correct delta.
        let target = sequences.len() + 1;
        if self.lit.capacity() < target {
            self.lit.reserve_exact(target - self.lit.len());
        }
        if self.ml.capacity() < target {
            self.ml.reserve_exact(target - self.ml.len());
        }
        self.lit.push(0);
        self.ml.push(0);
        for seq in sequences {
            self.lit
                .push(*self.lit.last().unwrap_or(&0) + seq.ll as usize);
            self.ml
                .push(*self.ml.last().unwrap_or(&0) + seq.ml as usize);
        }
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
    let mut scratch = core::mem::take(&mut state.block_scratch);
    collect_block_parts(state, &mut scratch.parts);
    encode_block_parts_with_sequence_scratch(
        state,
        &scratch.parts.literals,
        &scratch.parts.sequences,
        output,
        &mut scratch.estimator_sequences,
    );
    state.block_scratch = scratch;
}

pub(crate) fn compress_block_with_post_split<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    output: &mut Vec<u8>,
) {
    let mut scratch = core::mem::take(&mut state.block_scratch);
    collect_block_parts(state, &mut scratch.parts);
    if scratch.parts.sequences.len() <= 4 {
        let source_len = state.matcher.get_last_space().len();
        scratch.compressed.clear();
        let mut emit_buffers = SingleSequenceEmitBuffers {
            output,
            compressed: &mut scratch.compressed,
            sequence_scratch: &mut scratch.estimator_sequences,
        };
        let emitted_raw = emit_single_sequence_block(
            state,
            last_block,
            source_len,
            &scratch.parts.literals,
            &scratch.parts.sequences,
            &mut emit_buffers,
        );
        if emitted_raw {
            output.extend_from_slice(state.matcher.get_last_space());
        }
        state.block_scratch = scratch;
        return;
    }

    scratch.partitions.clear();
    scratch.prefix_sums.rebuild(&scratch.parts.sequences);
    let mut workspace = core::mem::take(&mut scratch.estimator_workspace);
    let mut estimator = SplitEstimator {
        parts: &scratch.parts,
        prefix_sums: &scratch.prefix_sums,
        block_entry: ProbeEntryState {
            last_huff_table: state.last_huff_table.clone(),
            ll_previous: state.fse_tables.ll_previous.clone(),
            ml_previous: state.fse_tables.ml_previous.clone(),
            of_previous: state.fse_tables.of_previous.clone(),
            offset_hist: state.offset_hist,
        },
        scratch_state: CompressState {
            matcher: EntropyOnlyMatcher,
            last_huff_table: state.last_huff_table.clone(),
            fse_tables: clone_fse_tables(&state.fse_tables),
            block_scratch: super::CompressedBlockScratch::new(),
            offset_hist: state.offset_hist,
        },
        workspace,
    };
    estimator.derive_block_splits(0, scratch.parts.sequences.len(), &mut scratch.partitions);
    scratch.partitions.push(scratch.parts.sequences.len());
    workspace = estimator.workspace;
    scratch.estimator_workspace = workspace;

    scratch.compressed.clear();
    let mut seq_start = 0usize;
    let mut lit_start = 0usize;
    let mut src_start = 0usize;
    for (partition_idx, &seq_end) in scratch.partitions.iter().enumerate() {
        let last_partition = partition_idx + 1 == scratch.partitions.len();
        let chunk_lit_len = scratch.prefix_sums.lit_range(seq_start, seq_end);
        let chunk_match_len = scratch.prefix_sums.ml_range(seq_start, seq_end);
        let lit_end = if last_partition {
            scratch.parts.literals.len()
        } else {
            lit_start + chunk_lit_len
        };
        let src_size = if last_partition {
            state.matcher.get_last_space().len() - src_start
        } else {
            chunk_lit_len + chunk_match_len
        };
        let mut emit_buffers = SingleSequenceEmitBuffers {
            output,
            compressed: &mut scratch.compressed,
            sequence_scratch: &mut scratch.estimator_sequences,
        };
        let emitted_raw = emit_single_sequence_block(
            state,
            last_block && last_partition,
            src_size,
            &scratch.parts.literals[lit_start..lit_end],
            &scratch.parts.sequences[seq_start..seq_end],
            &mut emit_buffers,
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
    state.block_scratch = scratch;
}

fn collect_block_parts<M: Matcher>(state: &mut CompressState<M>, parts: &mut EncodedBlockParts) {
    let src_len = state.matcher.get_last_space().len();
    parts.literals.clear();
    parts.sequences.clear();
    // `reserve_exact(N)` adds capacity above LENGTH, not above existing
    // capacity. Both `literals` and `sequences` were just `clear()`-ed (len
    // = 0), so subtracting `len()` ensures `cap >= N` after the call — the
    // older `cap - cap` form left the Vec under-provisioned whenever the
    // existing capacity was less than half of the target.
    if parts.literals.capacity() < src_len {
        parts.literals.reserve_exact(src_len - parts.literals.len());
    }
    let sequence_capacity = src_len / 8;
    if parts.sequences.capacity() < sequence_capacity {
        parts
            .sequences
            .reserve_exact(sequence_capacity - parts.sequences.len());
    }
    state.matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => parts.literals.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            let ll = literals.len() as u32;
            parts.literals.extend_from_slice(literals);
            parts.sequences.push(RawSequence {
                ll,
                ml: match_len as u32,
                offset: offset as u32,
            });
        }
    });
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
            state.fse_tables.ll_default_ref(),
            sequences.iter().map(|seq| encode_literal_length(seq.ll).0),
            9,
        );
        let ml_mode = choose_table(
            state.fse_tables.ml_previous.as_ref(),
            state.fse_tables.ml_default_ref(),
            sequences.iter().map(|seq| encode_match_len(seq.ml).0),
            9,
        );
        let of_mode = choose_table(
            state.fse_tables.of_previous.as_ref(),
            state.fse_tables.of_default_ref(),
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

/// Workspace shared across estimator probes so per-probe cost computation never
/// allocates. Counts are zeroed at the top of every probe.
struct EstimatorWorkspace {
    lit_counts: Box<[usize; 256]>,
    ll_counts: Box<[usize; 256]>,
    ml_counts: Box<[usize; 256]>,
    of_counts: Box<[usize; 256]>,
    sequences: Vec<crate::blocks::sequence_section::Sequence>,
}

impl Default for EstimatorWorkspace {
    fn default() -> Self {
        Self {
            lit_counts: Box::new([0; 256]),
            ll_counts: Box::new([0; 256]),
            ml_counts: Box::new([0; 256]),
            of_counts: Box::new([0; 256]),
            sequences: Vec::new(),
        }
    }
}

/// Dry-run analog of [`encode_block_parts_with_sequence_scratch`]: mirrors the
/// real encoder's `compress_literals` and `choose_table` decisions byte-for-byte
/// (same `last_huff_table` lookup, same FSE mode selection, same
/// `remember_last_used_tables` mutation), and computes the would-be output size
/// in bytes via existing cost primitives instead of running the per-sequence
/// FSE bit-level write. Splitter probes use this path to get the same byte
/// count `encode_block_parts` would produce while saving the dominant
/// `encode_sequences` write cost on every probe.
fn estimate_block_parts_size<M: Matcher>(
    state: &mut CompressState<M>,
    literals_vec: &[u8],
    raw_sequences: &[RawSequence],
    workspace: &mut EstimatorWorkspace,
) -> usize {
    encode_raw_sequences_into(
        raw_sequences,
        &mut state.offset_hist,
        &mut workspace.sequences,
    );

    let lit_bytes = estimate_literals_section_bytes(
        literals_vec,
        &mut state.last_huff_table,
        &mut workspace.lit_counts,
    );

    let seq_bytes = if workspace.sequences.is_empty() {
        1
    } else {
        estimate_sequences_section_bytes(
            &workspace.sequences,
            &mut state.fse_tables,
            &mut workspace.ll_counts,
            &mut workspace.ml_counts,
            &mut workspace.of_counts,
        )
    };

    lit_bytes + seq_bytes
}

fn estimate_literals_section_bytes(
    literals: &[u8],
    last_huff: &mut Option<huff0_encoder::HuffmanTable>,
    counts: &mut [usize; 256],
) -> usize {
    // Mirror `encode_block_parts_with_sequence_scratch` literal-mode branches.
    if literals.len() < 8 {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    }
    if all_bytes_identical(literals) {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + 1;
    }

    counts.fill(0);
    for &b in literals {
        counts[b as usize] += 1;
    }
    let max_sym = counts.iter().rposition(|&c| c > 0).unwrap_or_default();
    let new_table = huff0_encoder::HuffmanTable::build_from_counts(&counts[..=max_sym]);

    let Some(new_desc) = new_table.writeable_table_description_size() else {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    };
    // For lit_size ≥ 256, donor `compress_literals` calls `encoder.encode4x`
    // which splits the data in 4 streams with a 6-byte jumptable and per-stream
    // byte-aligned padding. Bare `estimate_compressed_size_from_counts` would
    // model a single stream and undercount by ~6–10 bytes per section, biasing
    // splitter probes. We reuse `estimate_compressed_size` on each quarter so
    // the cost matches the actual wire format.
    let new_payload = estimate_huff_payload_bytes(&new_table, literals, counts);

    // Mirror `compress_literals` reuse-vs-new decision **byte-for-byte**.
    // The real encoder compares single-stream `estimate_compressed_size` for
    // both new and old tables (see `compress_literals` below); the actual
    // wire output is the 4-stream `encode4x` layout once the table is chosen.
    // Using the 4-stream `estimate_huff_payload_bytes_checked` here would
    // disagree with the encoder and bias the splitter to pick a different
    // table than the encoder ultimately emits.
    let use_new =
        decide_huff_reuse_like_encoder(&new_table, last_huff.as_ref(), new_desc, literals);
    let reuse_payload = if !use_new {
        // Safe to recompute with 4-stream model now that the table is chosen:
        // the chosen-table path always returns the actual wire cost.
        last_huff
            .as_ref()
            .and_then(|t| estimate_huff_payload_bytes_checked(t, literals))
    } else {
        None
    };

    let payload: usize = if use_new {
        new_payload
    } else {
        reuse_payload.unwrap_or(literals.len())
    };
    let tree_desc = if use_new { new_desc } else { 0 };
    let compressed_header = compressed_literals_header_bytes(literals.len());
    let total = compressed_header + tree_desc + payload;

    // Mirror `compress_literals` raw fallback: compare on-wire sections
    // *byte-for-byte* — `total` already includes the compressed literals
    // header (and the Huffman tree description when emitting a new table),
    // so the raw side must also include its own 1–3 byte header. Comparing
    // against bare `literals.len()` would bias the splitter toward raw on
    // small payloads where the raw header (1 byte for size < 32) is smaller
    // than the compressed header (3 bytes for size < 1 KB).
    let raw_section_bytes = uncompressed_literals_header_bytes(literals.len()) + literals.len();
    if total >= raw_section_bytes {
        *last_huff = None;
        return raw_section_bytes;
    }

    if use_new {
        *last_huff = Some(new_table);
    }
    total
}

fn estimate_sequences_section_bytes(
    sequences: &[crate::blocks::sequence_section::Sequence],
    fse_tables: &mut FseTables,
    ll_counts: &mut [usize; 256],
    ml_counts: &mut [usize; 256],
    of_counts: &mut [usize; 256],
) -> usize {
    ll_counts.fill(0);
    ml_counts.fill(0);
    of_counts.fill(0);
    let mut extra_bits: usize = 0;
    for seq in sequences {
        let (ll, _, ll_bits) = encode_literal_length(seq.ll);
        let (ml, _, ml_bits) = encode_match_len(seq.ml);
        let (of, _, _) = encode_offset(seq.of);
        ll_counts[ll as usize] += 1;
        ml_counts[ml as usize] += 1;
        of_counts[of as usize] += 1;
        // Donor: OF code's value equals its additional-bits width.
        extra_bits += ll_bits + ml_bits + of as usize;
    }

    // Same `choose_table` calls as the real encoder — counts the iterator
    // internally, identical decision path.
    let ll_mode = choose_table(
        fse_tables.ll_previous.as_ref(),
        fse_tables.ll_default_ref(),
        sequences.iter().map(|seq| encode_literal_length(seq.ll).0),
        9,
    );
    let ml_mode = choose_table(
        fse_tables.ml_previous.as_ref(),
        fse_tables.ml_default_ref(),
        sequences.iter().map(|seq| encode_match_len(seq.ml).0),
        9,
    );
    let of_mode = choose_table(
        fse_tables.of_previous.as_ref(),
        fse_tables.of_default_ref(),
        sequences.iter().map(|seq| encode_offset(seq.of).0),
        8,
    );

    let ll_bits_chosen =
        fse_section_bits_for_mode(&ll_mode, ll_counts, fse_tables.ll_default_ref());
    let ml_bits_chosen =
        fse_section_bits_for_mode(&ml_mode, ml_counts, fse_tables.ml_default_ref());
    let of_bits_chosen =
        fse_section_bits_for_mode(&of_mode, of_counts, fse_tables.of_default_ref());

    let ll_table_desc_bytes = mode_table_description_bytes(&ll_mode);
    let ml_table_desc_bytes = mode_table_description_bytes(&ml_mode);
    let of_table_desc_bytes = mode_table_description_bytes(&of_mode);

    // nbSeq varint header (donor RFC 8878 §3.1.1.3.2.1): 1–3 bytes.
    let nb_seq_header = match sequences.len() {
        0..=127 => 1,
        128..=0x7FFF => 2,
        _ => 3,
    };
    let mode_byte = 1;

    let bit_content = ll_bits_chosen + ml_bits_chosen + of_bits_chosen + extra_bits;
    // `encode_sequences` tail: if already byte-aligned, writes one extra byte
    // (`write_bits(1u32, 8)`); else writes `8 - bit_content % 8` padding bits.
    let padding_bits = if bit_content.is_multiple_of(8) {
        8
    } else {
        8 - bit_content % 8
    };
    let stream_bytes = (bit_content + padding_bits) / 8;

    // Mirror state mutation done by `encode_block_parts_with_sequence_scratch`.
    let ll_last = into_last_used_table(ll_mode);
    let ml_last = into_last_used_table(ml_mode);
    let of_last = into_last_used_table(of_mode);
    remember_last_used_tables(fse_tables, ll_last, ml_last, of_last);

    nb_seq_header
        + mode_byte
        + ll_table_desc_bytes
        + of_table_desc_bytes
        + ml_table_desc_bytes
        + stream_bytes
}

/// Bit cost of a sequence section under `mode`, matching what
/// `encode_sequences` would emit: FSE state transitions + final state flush.
fn fse_section_bits_for_mode(
    mode: &FseTableMode<'_>,
    counts: &[usize; 256],
    default: &FSETable,
) -> usize {
    let max_symbol = counts.iter().rposition(|&c| c > 0).unwrap_or_default();
    match mode {
        FseTableMode::Predefined(t) => {
            cross_entropy_cost(counts, max_symbol, t).unwrap_or(0) + t.acc_log() as usize
        }
        FseTableMode::Encoded(t) => {
            // New table built from these very counts — `fse_bit_cost` is
            // strictly more accurate than the `entropy_cost` proxy here.
            fse_bit_cost(counts, max_symbol, t).unwrap_or_else(|| {
                let total: usize = counts[..=max_symbol].iter().sum();
                entropy_cost(counts, max_symbol, total)
            }) + t.acc_log() as usize
        }
        FseTableMode::RepeatLast(prev) => {
            // `PreviousFseTable::Rle(_).as_table()` returns `None`. The real
            // encoder in that case writes no FSE state transitions and no
            // final-state flush — `encode_sequences` short-circuits on a
            // `None` table mapping — so the section costs 0 bits, matching
            // the bare `Rle(_)` arm below. Falling back to `default` here
            // would over-count by the default table's acc_log plus its
            // per-code cross-entropy and bias splitter probes.
            match prev.as_table(default) {
                Some(table) => {
                    fse_bit_cost(counts, max_symbol, table).unwrap_or(0) + table.acc_log() as usize
                }
                None => 0,
            }
        }
        FseTableMode::Rle(_) => 0,
    }
}

/// Byte size of the table description `encode_table` writes for each FSE mode.
fn mode_table_description_bytes(mode: &FseTableMode<'_>) -> usize {
    match mode {
        FseTableMode::Predefined(_) | FseTableMode::RepeatLast(_) => 0,
        FseTableMode::Encoded(table) => table.table_header_bits() / 8,
        FseTableMode::Rle(_) => 1,
    }
}

/// Shared reuse-vs-new Huffman table decision used by both the real encoder
/// (`compress_literals`) and the splitter cost estimator
/// (`estimate_literals_section_bytes`). Returns `true` when a fresh table
/// should be emitted, `false` when the prior table can be reused.
///
/// Decision logic is byte-for-byte the donor's: the old-table cost is the
/// single-stream `estimate_compressed_size` (returns `None` when the prior
/// table lacks codes for a symbol present in the current literals — in which
/// case we must emit a new table). The new-table cost is its description
/// size plus the single-stream payload estimate. A small-input guard
/// (`new_desc + 12 >= literals.len()`) keeps the reuse path for tiny blocks
/// where the description alone would exceed the literals.
fn decide_huff_reuse_like_encoder(
    new_table: &huff0_encoder::HuffmanTable,
    last_table: Option<&huff0_encoder::HuffmanTable>,
    new_desc: usize,
    literals: &[u8],
) -> bool {
    let Some(prev) = last_table else {
        return true;
    };
    let Some(old_estimate) = prev.estimate_compressed_size(literals) else {
        return true;
    };
    let new_estimate = new_table
        .estimate_compressed_size(literals)
        .unwrap_or(literals.len());
    !(old_estimate <= new_desc + new_estimate || new_desc + 12 >= literals.len())
}

/// Mirrors `compress_literals` choice: lit_size < 256 → single huff0 stream
/// (`encode`), else → 4-stream layout (`encode4x`) with a 6-byte jumptable and
/// per-stream byte-aligned padding. Returns the exact wire-format byte cost of
/// the Huffman-encoded payload, excluding the literals section header and the
/// Huffman tree description.
fn estimate_huff_payload_bytes(
    table: &huff0_encoder::HuffmanTable,
    literals: &[u8],
    counts: &[usize; 256],
) -> usize {
    if literals.len() < 256 {
        table.estimate_compressed_size_from_counts(counts)
    } else {
        let split_size = literals.len().div_ceil(4);
        let s1 = &literals[..split_size];
        let s2 = &literals[split_size..split_size * 2];
        let s3 = &literals[split_size * 2..split_size * 3];
        let s4 = &literals[split_size * 3..];
        let mut total = 6; // 3 × u16 jumptable entries
        for stream in [s1, s2, s3, s4] {
            total += table
                .estimate_compressed_size(stream)
                .unwrap_or(stream.len());
        }
        total
    }
}

/// `estimate_huff_payload_bytes` variant that returns `None` when the table
/// can't encode some symbol in `literals` (Huffman codes with `num_bits == 0`).
/// Required to mirror `compress_literals`'s reuse-failure branch where the
/// real encoder bails to the new-table path.
fn estimate_huff_payload_bytes_checked(
    table: &huff0_encoder::HuffmanTable,
    literals: &[u8],
) -> Option<usize> {
    if literals.len() < 256 {
        table.estimate_compressed_size(literals)
    } else {
        let split_size = literals.len().div_ceil(4);
        let s1 = &literals[..split_size];
        let s2 = &literals[split_size..split_size * 2];
        let s3 = &literals[split_size * 2..split_size * 3];
        let s4 = &literals[split_size * 3..];
        let mut total = 6;
        for stream in [s1, s2, s3, s4] {
            total += table.estimate_compressed_size(stream)?;
        }
        Some(total)
    }
}

/// Donor RFC 8878 §3.1.1.3.1.2 raw/RLE literals header size (bytes).
fn uncompressed_literals_header_bytes(lit_size: usize) -> usize {
    match lit_size {
        0..=31 => 1,
        32..=4095 => 2,
        _ => 3,
    }
}

/// Donor RFC 8878 §3.1.1.3.1.1 compressed literals section header size (bytes,
/// excluding the Huffman tree description itself).
fn compressed_literals_header_bytes(lit_size: usize) -> usize {
    match lit_size {
        0..1024 => 3,
        1024..16384 => 4,
        _ => 5,
    }
}

struct SingleSequenceEmitBuffers<'a> {
    output: &'a mut Vec<u8>,
    compressed: &'a mut Vec<u8>,
    sequence_scratch: &'a mut Vec<crate::blocks::sequence_section::Sequence>,
}

fn emit_single_sequence_block<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    source_len: usize,
    literals: &[u8],
    sequences: &[RawSequence],
    buffers: &mut SingleSequenceEmitBuffers<'_>,
) -> bool {
    let saved_offset_hist = state.offset_hist;
    let saved_huff_table = state.last_huff_table.clone();
    let saved_ll_previous = state.fse_tables.ll_previous.clone();
    let saved_ml_previous = state.fse_tables.ml_previous.clone();
    let saved_of_previous = state.fse_tables.of_previous.clone();
    buffers.compressed.clear();
    encode_block_parts_with_sequence_scratch(
        state,
        literals,
        sequences,
        buffers.compressed,
        buffers.sequence_scratch,
    );
    let min_gain = (source_len >> 8) + 2;
    if buffers.compressed.len() >= source_len.saturating_sub(min_gain) {
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
        header.serialize(buffers.output);
        true
    } else {
        let header = BlockHeader {
            last_block,
            block_type: BlockType::Compressed,
            block_size: buffers.compressed.len() as u32,
        };
        header.serialize(buffers.output);
        buffers.output.extend_from_slice(buffers.compressed);
        false
    }
}

fn encode_raw_sequences_into(
    raw_sequences: &[RawSequence],
    offset_hist: &mut [u32; 3],
    out: &mut Vec<crate::blocks::sequence_section::Sequence>,
) {
    out.clear();
    // `reserve_exact` argument is the increment over LENGTH, not capacity —
    // see `SequencePrefixSums::rebuild` for the full rationale.
    if out.capacity() < raw_sequences.len() {
        out.reserve_exact(raw_sequences.len() - out.len());
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
    // The `*_default` fields are cfg-typed via the
    // [`crate::fse::fse_encoder::FseDefaultTable`] alias —
    // `&'static FSETable` on atomic / `critical-section` targets
    // (Copy, zero-cost clone via field-access) and
    // `Box<FSETable>` on the cache-less no-atomic path (needs
    // `Clone::clone` for a deep copy). Method resolution of
    // `.clone()` on `&'static FSETable` resolves via auto-deref to
    // `FSETable::clone` (returns owned `FSETable`) which is the
    // WRONG return type for the atomic arm — the cfg-split below
    // picks the correct expression explicitly per target/feature.
    //
    // The block-split estimator path that calls this helper does
    // not run on the per-frame hot path (it fires only when block
    // pre-splitting decides to estimate sub-block costs, levels
    // 11+), so the no-atomic deep-clone cost is amortised in the
    // broader estimator overhead.
    FseTables {
        #[cfg(any(target_has_atomic = "ptr", feature = "critical-section"))]
        ll_default: fse_tables.ll_default,
        #[cfg(not(any(target_has_atomic = "ptr", feature = "critical-section")))]
        ll_default: fse_tables.ll_default.clone(),
        ll_previous: fse_tables.ll_previous.clone(),
        #[cfg(any(target_has_atomic = "ptr", feature = "critical-section"))]
        ml_default: fse_tables.ml_default,
        #[cfg(not(any(target_has_atomic = "ptr", feature = "critical-section")))]
        ml_default: fse_tables.ml_default.clone(),
        ml_previous: fse_tables.ml_previous.clone(),
        #[cfg(any(target_has_atomic = "ptr", feature = "critical-section"))]
        of_default: fse_tables.of_default,
        #[cfg(not(any(target_has_atomic = "ptr", feature = "critical-section")))]
        of_default: fse_tables.of_default.clone(),
        of_previous: fse_tables.of_previous.clone(),
    }
}

/// Snapshot of the Huffman/FSE/repeat-offset state the real encoder would
/// have at a given partition boundary. Cloning is the only way to thread
/// state through recursive bisect probes (each branch needs its own copy),
/// but the snapshot is small relative to the full encode cost the dry-run
/// estimator replaces.
#[derive(Clone)]
struct ProbeEntryState {
    last_huff_table: Option<huff0_encoder::HuffmanTable>,
    ll_previous: Option<PreviousFseTable>,
    ml_previous: Option<PreviousFseTable>,
    of_previous: Option<PreviousFseTable>,
    offset_hist: [u32; 3],
}

struct SplitEstimator<'a> {
    parts: &'a EncodedBlockParts,
    prefix_sums: &'a SequencePrefixSums,
    block_entry: ProbeEntryState,
    scratch_state: CompressState<EntropyOnlyMatcher>,
    workspace: EstimatorWorkspace,
}

impl SplitEstimator<'_> {
    /// Run a single estimator probe seeded from `entry`. Returns the would-be
    /// emitted byte count for this partition, a `raw_fallback` flag (true
    /// when the estimate said this range will be emitted as a raw block in
    /// the real encoder — the cost is then capped at `source_len + 3`), and
    /// the post-probe state to feed into the sibling partition. When the
    /// partition would raw-fallback, the real encoder restores the entry
    /// state, so we return `entry` unchanged.
    fn estimate_subblock_size(
        &mut self,
        start_idx: usize,
        end_idx: usize,
        entry: &ProbeEntryState,
    ) -> (usize, bool, ProbeEntryState) {
        let lit_start = self.prefix_sums.lit[start_idx];
        let lit_len = self.prefix_sums.lit_range(start_idx, end_idx);
        let match_len = self.prefix_sums.ml_range(start_idx, end_idx);
        let lit_end = if end_idx == self.parts.sequences.len() {
            self.parts.literals.len()
        } else {
            lit_start + lit_len
        };
        self.scratch_state.last_huff_table = entry.last_huff_table.clone();
        self.scratch_state.fse_tables.ll_previous = entry.ll_previous.clone();
        self.scratch_state.fse_tables.ml_previous = entry.ml_previous.clone();
        self.scratch_state.fse_tables.of_previous = entry.of_previous.clone();
        self.scratch_state.offset_hist = entry.offset_hist;
        let emitted_payload = estimate_block_parts_size(
            &mut self.scratch_state,
            &self.parts.literals[lit_start..lit_end],
            &self.parts.sequences[start_idx..end_idx],
            &mut self.workspace,
        );
        let source_len = (lit_end - lit_start) + match_len;
        let min_gain = (source_len >> 8) + 2;
        let raw_fallback = emitted_payload >= source_len.saturating_sub(min_gain);
        let cost = if raw_fallback {
            source_len
        } else {
            emitted_payload
        } + 3;
        // Real emit on raw fallback restores the entry state — see
        // `emit_single_sequence_block`'s saved-state restore branch.
        let post = if raw_fallback {
            entry.clone()
        } else {
            ProbeEntryState {
                last_huff_table: self.scratch_state.last_huff_table.clone(),
                ll_previous: self.scratch_state.fse_tables.ll_previous.clone(),
                ml_previous: self.scratch_state.fse_tables.ml_previous.clone(),
                of_previous: self.scratch_state.fse_tables.of_previous.clone(),
                offset_hist: self.scratch_state.offset_hist,
            }
        };
        (cost, raw_fallback, post)
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
        let entry = self.block_entry.clone();
        let (full, full_raw_fallback, _) = self.estimate_subblock_size(start_idx, end_idx, &entry);
        // G3 — whole-block bail-out before partition split. Donor
        // `ZSTD_compressSubBlock_multi` (`zstd_compress_superblock.c:530-532`)
        // bails when `estBlockSize > srcSize` (strict). Our trigger is
        // the `raw_fallback` flag from `estimate_subblock_size`, which
        // fires on the **stricter** `emitted_payload >= source_len -
        // min_gain` condition (where `min_gain = (source_len >> 8) + 2`,
        // ≈0.4% margin — see line 917 of this file). So we bail in a
        // narrow band `[source_len - min_gain, source_len + 3]` where
        // donor would still recurse and *might* find a compressible
        // split.
        //
        // Why this is safe ratio-wise:
        // - The bail-out routes to `compress_block_with_post_split`'s
        //   single-partition path → `emit_single_sequence_block` →
        //   which has the SAME `min_gain` expansion fallback at line
        //   779-780. So whatever block we bail on would have been
        //   raw-fallbacked by the real emit anyway, by the same
        //   threshold.
        // - For a missed split-win to matter, both sub-blocks would
        //   need to compress strictly (no raw-fallback in either
        //   half), AND `cost(first) + cost(second) < source_len + 3`.
        //   The wider donor band gives at most `min_gain` bytes of
        //   theoretical recoverable ratio per block.
        // - Empirically validated: `compare_ffi --list` REPORT lines
        //   show **zero rust_bytes delta** vs main on every
        //   (scenario, level) cell across the full bench matrix.
        //
        // Returning with `partitions` left empty lets the outer loop
        // emit the block as a single partition, avoiding the bisect's
        // recursive `estimate_subblock_size` walks. Cheap: the `full`
        // probe ran whether or not bisect proceeds, so zero estimator
        // work added on the bail-out path; significant work saved on
        // long-input incompressible-ish blocks at high levels (where
        // optimal parser produces > MIN_SEQUENCES_BLOCK_SPLITTING
        // sequences).
        if full_raw_fallback {
            return;
        }
        self.derive_block_splits_with_full(start_idx, end_idx, full, entry, partitions);
    }

    /// Returns the post-emit state at `end_idx` produced by whichever
    /// partitioning the recursion settles on (single emit OR multiple
    /// nested splits). Callers thread this into the sibling probe so the
    /// right-hand recursion sees the actual donor-parity state the real
    /// emit would land in, not just the "left as one big partition" state.
    fn derive_block_splits_with_full(
        &mut self,
        start_idx: usize,
        end_idx: usize,
        full: usize,
        entry: ProbeEntryState,
        partitions: &mut Vec<usize>,
    ) -> ProbeEntryState {
        if end_idx - start_idx < MIN_SEQUENCES_BLOCK_SPLITTING
            || partitions.len() >= MAX_NB_BLOCK_SPLITS
        {
            // Leaf: this range will be emitted as a single partition, so the
            // exit state is the post-state of that single-partition probe.
            let (_cost, _raw_fallback, post) =
                self.estimate_subblock_size(start_idx, end_idx, &entry);
            return post;
        }
        let mid_idx = (start_idx + end_idx) / 2;
        let (first, _, first_post) = self.estimate_subblock_size(start_idx, mid_idx, &entry);
        // Donor parity: score the right half from the left's post-state,
        // not from the parent's block-entry state. Without this propagation
        // `second` is evaluated as a fresh-block start, biasing the
        // `first + second < full` decision toward overly optimistic splits.
        let (second, _, _) = self.estimate_subblock_size(mid_idx, end_idx, &first_post);
        if first + second < full {
            // If the left side gets further split, the true state at
            // `mid_idx` is the left subtree's exit state, not `first_post`.
            // Thread the returned state into the right recursion so the
            // right subtree probes against actual donor-parity state.
            let left_post =
                self.derive_block_splits_with_full(start_idx, mid_idx, first, entry, partitions);
            if partitions.len() >= MAX_NB_BLOCK_SPLITS {
                return left_post;
            }
            partitions.push(mid_idx);
            return self
                .derive_block_splits_with_full(mid_idx, end_idx, second, left_post, partitions);
        }
        // No split here — this range will be emitted as one partition.
        let (_cost, _raw_fallback, post) = self.estimate_subblock_size(start_idx, end_idx, &entry);
        post
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
    let ll_table = mode_table(ll_mode, defaults.ll_default_ref());
    let ml_table = mode_table(ml_mode, defaults.ml_default_ref());
    let of_table = mode_table(of_mode, defaults.of_default_ref());
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
    // Shared with the splitter cost estimator
    // (`estimate_literals_section_bytes`) so both code paths agree on which
    // table they would pick for a given `(new_table, last_table, literals)`
    // input.
    let new_table = decide_huff_reuse_like_encoder(
        &new_encoder_table,
        last_table,
        new_table_description_size,
        literals,
    );
    let encoder_table = if new_table {
        &new_encoder_table
    } else {
        last_table.expect("reuse path implies prior table exists")
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

    // Compare on-wire sections byte-for-byte: `total_len` already includes
    // the compressed literals header + tree description + payload, so the
    // raw side must also include its own 1–3 byte header
    // (`uncompressed_literals_header_bytes`). Comparing against bare
    // `literals.len()` would bias toward raw on small payloads where the
    // raw header is smaller than the compressed header.
    let raw_section_bytes = uncompressed_literals_header_bytes(literals.len()) + literals.len();
    if total_len >= raw_section_bytes {
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
            block_scratch: super::CompressedBlockScratch::new(),
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
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
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
        let ll_repeat = choose_table(
            fse_tables.ll_previous.as_ref(),
            fse_tables.ll_default_ref(),
            sample_codes.iter().copied(),
            9,
        );
        let ml_repeat = choose_table(
            fse_tables.ml_previous.as_ref(),
            fse_tables.ml_default_ref(),
            sample_codes.iter().copied(),
            9,
        );
        let of_repeat = choose_table(
            fse_tables.of_previous.as_ref(),
            fse_tables.of_default_ref(),
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
            previous_table(fse_tables.ll_previous.as_ref(), fse_tables.ll_default_ref()).unwrap(),
        );

        remember_last_used_tables(
            &mut fse_tables,
            None,
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
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
        let mode = choose_table(
            None,
            fse_tables.ll_default_ref(),
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
