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

/// Donor `ZSTD_minLiteralsToCompress` (`zstd_compress_literals.c:114-127`):
/// strategy-aware floor below which `compress_literals` does not even
/// attempt huf compression and falls back to raw.
///
/// Formula: `shift = MIN(9 - donor_strategy, 3); mintc = (huf_repeat ==
/// valid) ? 6 : (8 << shift)`. With huf reuse available, the per-block huf
/// header overhead is gone, so the cheap floor is 6 bytes. Without it, the
/// huf tree-description must be serialized per block — alphabet size and
/// max symbol determine its exact byte cost, but on payloads near the
/// per-strategy floor that overhead dominates and the compressed section
/// loses to raw. Donor's shift table picks the floor per strategy:
/// strategy 1..6 → 64 bytes, strategy 7 (btopt) → 32, strategy 8 (btultra)
/// → 16, strategy 9 (btultra2) → 8.
///
/// Our `StrategyTag` enum has seven variants (no separate lazy2/btlazy2 —
/// the `Lazy` variant covers donor strategies 4..6). Within the
/// fast..lazy2 band donor's shift table is flat: strategies 1..6 all
/// pin `shift = MIN(9 - strat, 3) = 3`, so `Lazy → 64-byte floor`
/// regardless of which donor index (4, 5, or 6) we'd nominally use.
/// No aggressiveness gradient within this band to preserve.
#[inline]
fn min_literals_to_compress(
    strategy: crate::encoding::strategy::StrategyTag,
    has_huf_table: bool,
) -> usize {
    use crate::encoding::strategy::StrategyTag;
    if has_huf_table {
        return 6;
    }
    let shift: u32 = match strategy {
        StrategyTag::Fast | StrategyTag::Dfast | StrategyTag::Greedy | StrategyTag::Lazy => 3,
        StrategyTag::BtOpt => 2,
        StrategyTag::BtUltra => 1,
        StrategyTag::BtUltra2 => 0,
    };
    8usize << shift
}

/// Donor `ZSTD_minGain` (`zstd_compress_internal.h:677-684`):
/// strategy-aware minimum-compression margin. In donor it gates both
/// the block-level "compressed block must beat raw + minGain" decision
/// and the literal-section `cLitSize >= srcSize - minGain` fallback.
///
/// Formula: `minlog = (strat >= btultra) ? strat - 1 : 6; (src_size >>
/// minlog) + 2`. So:
/// - fast..btopt (strat 1..7): minlog=6 → ~1.5% margin + 2 bytes
/// - btultra (strat 8): minlog=7 → ~0.78% margin + 2 bytes
/// - btultra2 (strat 9): minlog=8 → ~0.39% margin + 2 bytes
///
/// **Current usage in this crate:** wired into the literal-section
/// raw-fallback gate (`compress_literals` +
/// `estimate_literals_section_bytes`) only — those sites previously
/// had no margin at all (bare `>= raw_section_bytes`).
/// **Not yet wired into** the block-level emit/probe paths
/// (`emit_single_sequence_block`, `SplitEstimator::estimate_subblock_size`),
/// which still use a uniform `(source_len >> 8) + 2` calculation
/// (the btultra2 value applied across all strategies). Migrating
/// those sites is a separate cleanup.
#[inline]
fn min_gain(src_size: usize, strategy: crate::encoding::strategy::StrategyTag) -> usize {
    use crate::encoding::strategy::StrategyTag;
    let minlog: u32 = match strategy {
        StrategyTag::BtUltra => 7,
        StrategyTag::BtUltra2 => 8,
        _ => 6,
    };
    (src_size >> minlog) + 2
}

/// Donor `compress_literals` raw-fallback gate
/// (`zstd_compress_literals.c:187-188`): emit raw when
/// `cLitSize >= srcSize - minGain`, where `cLitSize` is the HUF payload
/// plus tree description (the bytes `HUF_compress*` writes — excluding
/// the surrounding literals lhSize) and `srcSize` is the literal-payload
/// length. Compares payload-vs-srcSize, NOT on-wire-vs-on-wire, so the
/// gate is symmetric in header overhead.
///
/// Centralized helper so `compress_literals` and
/// `estimate_literals_section_bytes` share the exact same decision and
/// neither side can drift back to the pre-2026-05 on-wire comparison
/// (which inflated the threshold by `compressed_lhsize - raw_lhsize`
/// bytes and rejected marginally-winning compressed sections).
#[inline]
fn use_raw_literal_fallback(
    huf_section_size: usize,
    literals_len: usize,
    strategy: crate::encoding::strategy::StrategyTag,
) -> bool {
    huf_section_size >= literals_len.saturating_sub(min_gain(literals_len, strategy))
}

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

/// [`SeqSink`] over a block's flat literal + sequence buffers. Lets the Fast
/// matcher push matches straight from its hot loop (no `Sequence` enum, no
/// closure dispatch) — the production consumer of
/// [`Matcher::start_matching_into`].
struct BlockPartsSink<'a> {
    literals: &'a mut Vec<u8>,
    sequences: &'a mut Vec<RawSequence>,
}

impl crate::encoding::SeqSink for BlockPartsSink<'_> {
    #[inline(always)]
    fn push_seq(&mut self, literals: &[u8], offset: u32, match_len: u32) {
        let ll = literals.len() as u32;
        append_literals(self.literals, literals);
        self.sequences.push(RawSequence {
            ll,
            ml: match_len,
            offset,
        });
    }
    #[inline(always)]
    fn push_tail(&mut self, literals: &[u8]) {
        append_literals(self.literals, literals);
    }
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
    #[cfg(all(feature = "lsm", feature = "hash"))] mut block_checksums: Option<&mut Vec<u32>>,
) {
    let mut scratch = core::mem::take(&mut state.block_scratch);
    collect_block_parts(state, &mut scratch.parts);
    if scratch.parts.sequences.len() <= 4 {
        let source_len = state.matcher.get_last_space().len();
        // `block_checksums: Option<&mut Vec<u32>>`; `as_deref_mut` unwraps
        // exactly one level of `&mut`, yielding `Option<&mut Vec<u32>>` here
        // (the blanket `impl<T: ?Sized> Deref for &mut T` has
        // `Target = T`, so the deref chain does NOT cascade into
        // `Vec<u32>::Target = [u32]`). Hence `sink: &mut Vec<u32>` and
        // `Vec::push` is in scope.
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if let Some(sink) = block_checksums.as_deref_mut() {
            sink.push(crate::encoding::frame_compressor::xxh64_block_low32(
                state.matcher.get_last_space(),
            ));
        }
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
            strategy_tag: state.strategy_tag,
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
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if let Some(sink) = block_checksums.as_deref_mut() {
            sink.push(crate::encoding::frame_compressor::xxh64_block_low32(
                &state.matcher.get_last_space()[src_start..src_start + src_size],
            ));
        }
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

/// Append `lits` to `dst` using inline byte / u64 ops for short
/// slices, avoiding the libc memmove call overhead that
/// `Vec::extend_from_slice` lowers to for runtime-sized
/// `ptr::copy_nonoverlapping`. Fast L1 emits literal runs of 1-10
/// bytes typically — at thousands of sequences per block, the per-
/// emit libc call dominated the hot path (flamegraph: 60 % of CPU
/// in `__memmove_avx_unaligned_erms` chain).
///
/// Route through `simd_copy::copy_bytes_overshooting` with src.1 ==
/// dst.1 == lit_len (no overshoot READ; we don't know how much
/// readable slack the caller's slice has). For lit_len ≤ 32 that
/// drops into the byte-by-byte / overlapping-u64 path, fully
/// inlineable. Larger runs fall through `extend_from_slice` —
/// they're rare and libc memmove amortises across the longer copy.
#[inline]
fn append_literals(dst: &mut Vec<u8>, lits: &[u8]) {
    let lit_len = lits.len();
    if lit_len == 0 {
        return;
    }
    if lit_len <= 32 {
        // Production callers (`collect_block_parts`) pre-reserve
        // `src_len` of spare capacity, so the sum of all literal
        // runs across a block fits without grow. But this is a SAFE
        // fn (module-private; callers in this same file are the
        // only ones today, but the safety net still must hold), so
        // we enforce the precondition in release too — otherwise a
        // future caller skipping the pre-reserve would get an
        // immediate 32-byte OOB write into whatever follows the
        // `Vec`'s allocation. The branch is cold on the production
        // hot path (debug_assert in tests confirms it stays
        // untaken).
        let cur_len = dst.len();
        if dst.capacity() - cur_len < lit_len {
            dst.reserve(lit_len);
        }
        let dst_ptr = unsafe { dst.as_mut_ptr().add(cur_len) };
        // SAFETY: `lits` is a valid slice (so reading `lit_len`
        // bytes from `lits.as_ptr()` is in-bounds); the
        // `dst.reserve(lit_len)` above guarantees `dst_ptr` has
        // `lit_len` bytes of spare capacity. copy_bytes_overshooting
        // writes EXACTLY `lit_len` bytes when
        // `min(src.1, dst.1) == lit_len`.
        unsafe {
            crate::decoding::simd_copy::copy_bytes_overshooting(
                (lits.as_ptr(), lit_len),
                (dst_ptr, lit_len),
                lit_len,
            );
            dst.set_len(cur_len + lit_len);
        }
    } else {
        dst.extend_from_slice(lits);
    }
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
    let mut sink = BlockPartsSink {
        literals: &mut parts.literals,
        sequences: &mut parts.sequences,
    };
    state.matcher.start_matching_into(&mut sink);
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
    // Donor `compress_literals` (`zstd_compress_literals.c:153-160`):
    // `srcSize < ZSTD_minLiteralsToCompress(strategy, prevHuf->repeatMode)`
    // → `ZSTD_noCompressLiterals` (raw). The threshold is strategy-aware
    // (see `min_literals_to_compress`). With huf reuse available the
    // floor drops to 6 since there is no per-block huf-header overhead.
    let strategy = state.strategy_tag;
    let has_huf_table = state.last_huff_table.is_some();
    let min_lits = min_literals_to_compress(strategy, has_huf_table);
    // RLE pre-check: donor `compress_literals` reaches RLE only through
    // the `cLitSize == 1` branch (`zstd_compress_literals.c:192-201`)
    // after passing the `min_lits` gate and running a full HUF compress —
    // so donor emits raw for any all-identical section under `min_lits`
    // (e.g. 8..63 bytes at fast/dfast/greedy/lazy without HUF reuse).
    // RLE and raw share the same lhSize for a given `len`
    // (both use `uncompressed_literals_header_bytes`), so RLE = lhSize + 1
    // and raw = lhSize + len. That makes RLE equal to raw on `len == 1`
    // and smaller by exactly `len - 1` bytes for `len >= 2`, regardless of
    // the lhSize tier (1 / 2 / 3 / 5 bytes). Our pre-check fires for ANY
    // all-identical literal slice regardless of strategy/min_lits.
    // This produces strictly smaller output than donor on the small
    // all-identical edges while still matching donor on `>= min_lits`
    // inputs (where donor's compress+`cLitSize==1` path reaches the same
    // RLE block).
    // Note the order — RLE pre-check runs BEFORE `min_lits`;
    // `estimate_literals_section_bytes` mirrors this exactly so probe
    // costs match emit byte-for-byte.
    if !literals_vec.is_empty() && all_bytes_identical(literals_vec) {
        rle_literals(literals_vec, &mut writer);
        state.last_huff_table = None;
    } else if literals_vec.len() >= min_lits {
        match compress_literals(
            literals_vec,
            state.last_huff_table.as_ref(),
            &mut writer,
            strategy,
        ) {
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

        // Single-pass histogram of ll/ml/of codes across all sequences.
        // Previously did three separate `sequences.iter().map(...)`
        // passes; folded into one loop here saves the per-element
        // closure overhead (profile #220 round 3: `Map::fold` +
        // `call_mut` accounted for ~5% of total bench CPU).
        let mut ll_counts = [0usize; 256];
        let mut ml_counts = [0usize; 256];
        let mut of_counts = [0usize; 256];
        for seq in sequences.iter() {
            ll_counts[encode_literal_length(seq.ll).0 as usize] += 1;
            ml_counts[encode_match_len(seq.ml).0 as usize] += 1;
            of_counts[encode_offset(seq.of).0 as usize] += 1;
        }
        let total = sequences.len();

        let ll_mode = choose_table_from_counts(
            state.fse_tables.ll_previous.as_ref(),
            state.fse_tables.ll_default_ref(),
            &ll_counts,
            total,
            9,
        );
        let ml_mode = choose_table_from_counts(
            state.fse_tables.ml_previous.as_ref(),
            state.fse_tables.ml_default_ref(),
            &ml_counts,
            total,
            9,
        );
        let of_mode = choose_table_from_counts(
            state.fse_tables.of_previous.as_ref(),
            state.fse_tables.of_default_ref(),
            &of_counts,
            total,
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
        state.strategy_tag,
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
    strategy: crate::encoding::strategy::StrategyTag,
) -> usize {
    // Mirror `encode_block_parts_with_sequence_scratch` literal-mode branches
    // **in the same order**. The emitter pre-checks `all_identical`
    // (any non-empty section) BEFORE the `min_lits` gate — RLE and raw
    // share `uncompressed_literals_header_bytes(len)` (1/2/3/5 bytes by
    // length tier), so on all-identical inputs RLE = lhSize + 1 equals
    // raw = lhSize + len at `len == 1` and is smaller by `len - 1` for
    // `len >= 2`. RLE is never worse than raw, so it is selected
    // regardless of strategy. Estimator must use the same ordering and
    // predicate so probe costs match emit byte-for-byte.
    if !literals.is_empty() && all_bytes_identical(literals) {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + 1;
    }
    let min_lits = min_literals_to_compress(strategy, last_huff.is_some());
    if literals.len() < min_lits {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    }

    // Donor preferRepeat fast-path: skip the histogram +
    // `build_from_counts` cost. Mirrors donor's
    // `huf_compress.c:1360-1364` policy — when the prior table
    // is valid for the input, REUSE unconditionally regardless
    // of whether a freshly-built table would compress better.
    // This is a deliberate CPU-avoidance bias on fast-band tiny
    // sections; see `decide_huff_reuse_prefer_repeat_forces_reuse_for_fast_band`
    // test which seeds a fixture where size-comparison would
    // pick new and asserts the override still picks reuse.
    // Mirrors `compress_literals` so both code paths agree
    // byte-for-byte. The prev-table validation
    // (`estimate_compressed_size` returns Some) gates the
    // short-circuit so we still fall through to rebuild when the
    // prior table can't encode the current literals.
    if prefer_repeat_eligible(strategy, literals.len())
        && let Some(prev) = last_huff.as_ref()
        && let Some(reuse_payload) = estimate_huff_payload_bytes_checked(prev, literals)
    {
        let compressed_header = compressed_literals_header_bytes(literals.len());
        let total = compressed_header + reuse_payload; // no tree_desc on reuse
        let raw_section_bytes = uncompressed_literals_header_bytes(literals.len()) + literals.len();
        let huf_section_size = total - compressed_header;
        if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
            *last_huff = None;
            return raw_section_bytes;
        }
        return total;
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
    let use_new = decide_huff_reuse_like_encoder(
        &new_table,
        last_huff.as_ref(),
        new_desc,
        literals,
        strategy,
    );
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

    // Donor `compress_literals` raw-fallback gate
    // (`zstd_compress_literals.c:187-188`):
    //   `cLitSize >= srcSize - minGain`
    // where `cLitSize` is the encoded literals payload + tree description
    // (output of `HUF_compress*`, excluding the surrounding lhSize bytes)
    // and `srcSize` is the literal-payload length. In our terms:
    //   - donor `cLitSize` ≡ `total - compressed_header` (tree_desc + payload)
    //   - donor `srcSize`  ≡ `literals.len()`
    // Using the on-wire `total >= raw_section_bytes - mg` form (which
    // includes the compressed header on the LHS and the raw header on
    // the RHS) skews the threshold by `compressed_header - raw_header`
    // bytes and rejects compressed sections that donor would keep,
    // losing ratio. Mirror donor's payload-vs-srcSize form here.
    let raw_section_bytes = uncompressed_literals_header_bytes(literals.len()) + literals.len();
    let huf_section_size = total - compressed_header; // tree_desc + payload, no lhSize
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
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
/// Donor `HUF_flags_preferRepeat` gate (`zstd_compress_literals.c:165`):
/// fast-band strategies (`strategy < ZSTD_lazy` → Fast / Dfast /
/// Greedy in our enum) with short literal sections (≤ 1024 bytes)
/// prefer reusing the previous tree over rebuilding it. Inside
/// donor's HUF_compress (`huf_compress.c:1360-1364, 1396-1400`),
/// the flag short-circuits the rebuild path when the prior table
/// is valid; we mirror it at our caller layer so the wasted
/// `HuffmanTable::build_from_data` work is also skipped on the
/// fast-band reuse path. Note this is an UNCONDITIONAL reuse
/// override — donor intentionally picks reuse even when a fresh
/// table would compress better, trading a small ratio loss on
/// tiny sections for the CPU saved on the tree build. The
/// `decide_huff_reuse_like_encoder` helper then implements a
/// MIXED policy: the preferRepeat override fires first for the
/// fast band; outside that band, the existing size-comparison
/// heuristic decides reuse vs rebuild based on estimated bytes.
#[inline]
fn prefer_repeat_eligible(
    strategy: crate::encoding::strategy::StrategyTag,
    literals_len: usize,
) -> bool {
    use crate::encoding::strategy::StrategyTag;
    matches!(
        strategy,
        StrategyTag::Fast | StrategyTag::Dfast | StrategyTag::Greedy
    ) && literals_len <= 1024
}

fn decide_huff_reuse_like_encoder(
    new_table: &huff0_encoder::HuffmanTable,
    last_table: Option<&huff0_encoder::HuffmanTable>,
    new_desc: usize,
    literals: &[u8],
    strategy: crate::encoding::strategy::StrategyTag,
) -> bool {
    let Some(prev) = last_table else {
        return true;
    };
    let Some(old_estimate) = prev.estimate_compressed_size(literals) else {
        return true;
    };
    // Late-stage `HUF_flags_preferRepeat` mirror — kept here for
    // any caller that bypasses the early fast-path in
    // `compress_literals` / `estimate_literals_section_bytes`.
    // The early fast-paths short-circuit BEFORE `build_from_data`
    // / `build_from_counts` to skip wasted tree-build work; this
    // late gate covers the (currently unreachable) shape where the
    // new table is built first and the decision still wants to
    // reuse.
    if prefer_repeat_eligible(strategy, literals.len()) {
        return false;
    }
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
        // ≈0.4% margin — see the `min_gain` computation inside
        // `estimate_subblock_size` above). So we bail in a narrow band
        // `[source_len - min_gain, source_len + 3]` where donor would
        // still recurse and *might* find a compressible split.
        //
        // Why this is safe ratio-wise:
        // - The bail-out routes to `compress_block_with_post_split`'s
        //   single-partition path → `emit_single_sequence_block`,
        //   which applies the SAME `min_gain` expansion fallback (its
        //   `buffers.compressed.len() >= source_len - min_gain` check
        //   right before deciding raw-fallback). So for the
        //   single-partition path specifically, any block we bail on
        //   here would also raw-fallback there by the same threshold —
        //   no wire-output drift from this bail-out vs the "let the
        //   real emit decide" alternative.
        // - Returning here does skip the split case, so this is NOT a
        //   proof that a recursive split could never do better: in
        //   principle, both sub-blocks could compress strictly (no
        //   raw-fallback in either half) and beat the whole-block
        //   outcome. For such a missed split-win to matter, both
        //   sub-blocks would need to compress strictly AND
        //   `cost(first) + cost(second) < source_len + 3`. The wider
        //   donor band gives at most `min_gain` bytes of theoretical
        //   recoverable ratio per block.
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
    choose_table_from_counts(previous, default_table, &counts, total, max_log)
}

/// Same decision logic as [`choose_table`] but takes pre-computed
/// symbol counts and total directly. Hot-path callers in
/// `compress_literals_and_sequences` use this overload to avoid
/// re-iterating the sequence vec three times (one pass per
/// ll/ml/of stream); the iterator form is kept for the cost
/// estimator's call sites where the data is already in iterator
/// form.
fn choose_table_from_counts<'a>(
    previous: Option<&'a PreviousFseTable>,
    default_table: &'a FSETable,
    counts: &[usize; 256],
    total: usize,
    max_log: u8,
) -> FseTableMode<'a> {
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
            .saturating_add(entropy_cost(counts, max_symbol, total))
    });

    let predefined_cost = cross_entropy_cost(counts, max_symbol, default_table);

    let previous_cost = previous.and_then(|previous| {
        previous
            .as_table(default_table)
            .and_then(|table| fse_bit_cost(counts, max_symbol, table))
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

    // Donor-faithful sequence loop: write state diffs + extras via
    // unchecked fast-path adds with explicit `flush_bulk` calls at
    // safe burst boundaries. Per-sequence bit budget:
    //   state diffs: of (<=8) + ml (<=9) + ll (<=9) = 26 bits → one
    //                burst between flushes.
    //   extras:      ll (<=16) + ml (<=16) + of (<=24) = 56 bits →
    //                one burst between flushes.
    //
    // Total per sequence: 82 bits ⇒ at least 2 flushes (one per burst).
    // Mirrors donor `ZSTD_encodeSequences_body`
    // (`zstd_compress_sequences.c:303-360`) which uses BIT_addBitsFast
    // + BIT_flushBitsFast at the same burst boundaries.
    //
    // Pre-reserve output capacity for the worst-case sequence section
    // size (~10 bytes/sequence + 32 byte slack) so the per-flush
    // `extend_from_slice` never triggers a Vec realloc.
    if sequences.len() > 1 {
        writer.reserve_output(sequences.len() * 12 + 64);
        // Pre-loop flush: the safe `write_bits` calls above for the
        // final sequence's add_bits leave `bits_in_partial` in
        // 0..=63. Before the first unchecked-add burst we drain to
        // < 8 leftover so the per-burst budget math (state diffs ≤
        // 30 + leftover ≤ 8 = 38 < 64) holds invariantly.
        // SAFETY: `reserve_output` above guarantees capacity ≥
        // current_len + sequences.len() * 12 + 64 ≥ current_len + 8.
        unsafe {
            writer.flush_bulk();
        }
        for sequence in (0..=sequences.len() - 2).rev() {
            let sequence = sequences[sequence];
            let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
            let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
            let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);

            // State diffs burst: max 30 bits (10+10+9 worst case for
            // acc_log ≤ 9 ll/ml + acc_log ≤ 8 of) + ≤ 7 leftover from
            // prior flush = ≤ 37 bits total — well under 64.
            //
            // SAFETY (for every `write_bits_64_no_check` below):
            // - the prior `flush_bulk` left `bits_in_partial ≤ 7`;
            // - each FSE state diff has `next.num_bits ≤ acc_log ≤ 10`;
            //   three diffs back-to-back add ≤ 30 bits → total ≤ 37,
            //   well below the 64-bit accumulator cap.
            // - `diff = state.index - next.baseline` cannot exceed
            //   `(1 << num_bits) - 1`, so `diff >> num_bits == 0`.
            // `reserve_output(sequences.len() * 12 + 64)` above
            // pre-allocated enough spare capacity to cover every
            // per-sequence flush in this loop (≤ 16 bytes per
            // sequence, plus the 32-byte slack on top of the 64-byte
            // header reserve).
            if let (Some(table), Some(state)) = (of_table, of_state) {
                let next = table.next_state(of_code, state.index);
                let diff = state.index - next.baseline;
                unsafe {
                    writer.write_bits_64_no_check(diff as u64, next.num_bits as usize);
                }
                of_state = Some(next);
            }
            if let (Some(table), Some(state)) = (ml_table, ml_state) {
                let next = table.next_state(ml_code, state.index);
                let diff = state.index - next.baseline;
                unsafe {
                    writer.write_bits_64_no_check(diff as u64, next.num_bits as usize);
                }
                ml_state = Some(next);
            }
            if let (Some(table), Some(state)) = (ll_table, ll_state) {
                let next = table.next_state(ll_code, state.index);
                let diff = state.index - next.baseline;
                unsafe {
                    writer.write_bits_64_no_check(diff as u64, next.num_bits as usize);
                }
                ll_state = Some(next);
            }
            unsafe {
                writer.flush_bulk();
            }

            // Extras burst: ll (≤16) + ml (≤16) + of (≤ window_log,
            // up to 30 for our max window_log). With ≤ 7 leftover from
            // the prior flush_bulk, total ll+ml+of+partial can exceed
            // 64 once of_num_bits > 25. Donor handles this via
            // `longOffsets` mode that splits high offsets across two
            // BIT_addBits calls; we instead drain the partial after ml
            // and write of into a fresh container. The branch matches
            // donor's `MEM_32bits()` flush-between-each-component
            // shape on the 32-bit build (which has the same 64-bit
            // container constraint).
            //
            // SAFETY: `encode_literal_length` / `encode_match_len`
            // bound `*_num_bits ≤ 16` and return a clean `*_add_bits`
            // (low `num_bits` bits only). `encode_offset` bounds
            // `of_num_bits ≤ ilog2(of)`, capped at the encoder's
            // `window_log` ≤ 30; the conditional flush_bulk above
            // drains the partial when of_num_bits crosses the 24-bit
            // threshold where the sum could exceed 64.
            unsafe {
                writer.write_bits_64_no_check(ll_add_bits as u64, ll_num_bits);
                writer.write_bits_64_no_check(ml_add_bits as u64, ml_num_bits);
            }
            if of_num_bits > 24 {
                unsafe {
                    writer.flush_bulk();
                }
            }
            unsafe {
                writer.write_bits_64_no_check(of_add_bits as u64, of_num_bits);
                writer.flush_bulk();
            }
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

/// Reuse-only literals emit. Writes the full RFC 8878 §3.1.1.3.1.1
/// treeless literals section: type bits (`0b11`), 2-bit
/// size_format, the regenerated (uncompressed) literals length
/// field, the compressed length field placeholder (patched after
/// the huf payload is emitted), and the huf-encoded payload using
/// `last_table` (no tree description, since the decoder reuses the
/// previously-emitted one). Used by `compress_literals` when the
/// donor preferRepeat gate short-circuits the rebuild path.
/// Mirrors the post-decide reuse branch at the bottom of
/// `compress_literals` byte-for-byte (same size_format ladder, same
/// min_gain raw-fallback gate) so the wire output is identical to
/// the size-comparison reuse path when both would pick reuse.
fn emit_reuse_literals(
    literals: &[u8],
    last_table: &huff0_encoder::HuffmanTable,
    writer: &mut BitWriter<&mut Vec<u8>>,
    reset_idx: usize,
    strategy: crate::encoding::strategy::StrategyTag,
) -> HuffmanTableUpdate {
    writer.write_bits(3u8, 2); // treeless compressed literals type
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
    let mut encoder = huff0_encoder::HuffmanEncoder::new(last_table, writer);
    if size_format == 0 {
        encoder.encode(literals, false);
    } else {
        encoder.encode4x(literals, false);
    }
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    let compressed_header_len = compressed_literals_header_bytes(literals.len());
    let huf_section_size = total_len - compressed_header_len;
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
        HuffmanTableUpdate::Cleared
    } else {
        HuffmanTableUpdate::Reused
    }
}

fn compress_literals(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    writer: &mut BitWriter<&mut Vec<u8>>,
    strategy: crate::encoding::strategy::StrategyTag,
) -> HuffmanTableUpdate {
    let reset_idx = writer.index();

    // Donor preferRepeat fast-path: when Fast/Dfast/Greedy on
    // <=1024-byte literals AND the prior table can encode this
    // input (`estimate_compressed_size` returns Some), skip the
    // expensive `HuffmanTable::build_from_data` and route the
    // emit straight through the reuse path. Mirrors donor's
    // HUF_compress shape: `huf_compress.c:1360-1364` checks the
    // flag BEFORE the histogram + tree-build, so the rebuild cost
    // is avoided on fast-band tiny sections. Without this gate,
    // we paid `build_from_data` then short-circuited at the
    // decide-helper — wasted CPU on the hot fast-level path.
    if prefer_repeat_eligible(strategy, literals.len())
        && let Some(prev) = last_table
        && prev.estimate_compressed_size(literals).is_some()
    {
        return emit_reuse_literals(literals, prev, writer, reset_idx, strategy);
    }

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
        strategy,
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

    // Donor `compress_literals` raw-fallback gate
    // (`zstd_compress_literals.c:187-188`):
    //   `cLitSize >= srcSize - minGain`
    // where donor's `cLitSize` is the encoded literals payload plus the
    // tree description (output of `HUF_compress*`, excluding the
    // surrounding `lhSize` literals header), and `srcSize` is the
    // literal-payload length. In our terms:
    //   - donor `cLitSize` ≡ `total_len - compressed_literals_header_bytes`
    //     (i.e. tree_desc + huf_payload, no lhSize)
    //   - donor `srcSize`  ≡ `literals.len()`
    // Comparing `total_len >= raw_section_bytes - minGain` (with the
    // compressed-section lhSize on the LHS and raw-section header on
    // the RHS) skews the threshold by `compressed_header - raw_header`
    // bytes and rejects compressed sections that donor would keep —
    // direct ratio loss. Mirror donor's payload-vs-srcSize form here.
    // `minGain` is strategy-aware (`min_gain` helper above; ~1.56% for
    // fast..btopt, ~0.78% for btultra, ~0.39% for btultra2). Saturating
    // subtraction covers tiny inputs where `literals.len() < minGain`.
    let compressed_header_len = compressed_literals_header_bytes(literals.len());
    let huf_section_size = total_len - compressed_header_len; // tree_desc + payload, no lhSize
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
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
        encode_offset_with_history, min_gain, min_literals_to_compress, previous_table,
        remember_last_used_tables,
    };
    use crate::encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable};
    use crate::encoding::strategy::StrategyTag;
    use crate::fse::fse_encoder::build_table_from_symbol_counts;
    use crate::huff0::huff0_encoder;
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
    fn min_literals_to_compress_returns_per_strategy_floor() {
        for strat in [
            StrategyTag::Fast,
            StrategyTag::Dfast,
            StrategyTag::Greedy,
            StrategyTag::Lazy,
        ] {
            assert_eq!(min_literals_to_compress(strat, false), 64);
            assert_eq!(min_literals_to_compress(strat, true), 6);
        }
        assert_eq!(min_literals_to_compress(StrategyTag::BtOpt, false), 32);
        assert_eq!(min_literals_to_compress(StrategyTag::BtOpt, true), 6);
        assert_eq!(min_literals_to_compress(StrategyTag::BtUltra, false), 16);
        assert_eq!(min_literals_to_compress(StrategyTag::BtUltra, true), 6);
        assert_eq!(min_literals_to_compress(StrategyTag::BtUltra2, false), 8);
        assert_eq!(min_literals_to_compress(StrategyTag::BtUltra2, true), 6);
    }

    #[test]
    fn min_gain_returns_per_strategy_margin() {
        let src = 4096usize;
        for strat in [
            StrategyTag::Fast,
            StrategyTag::Dfast,
            StrategyTag::Greedy,
            StrategyTag::Lazy,
            StrategyTag::BtOpt,
        ] {
            assert_eq!(min_gain(src, strat), (src >> 6) + 2);
        }
        assert_eq!(min_gain(src, StrategyTag::BtUltra), (src >> 7) + 2);
        assert_eq!(min_gain(src, StrategyTag::BtUltra2), (src >> 8) + 2);
        assert_eq!(min_gain(0, StrategyTag::Fast), 2);
        assert_eq!(min_gain(63, StrategyTag::Fast), 2);
        assert_eq!(min_gain(64, StrategyTag::Fast), 3);
    }

    #[test]
    fn use_raw_literal_fallback_uses_payload_vs_srcsize_threshold() {
        use super::{compressed_literals_header_bytes, use_raw_literal_fallback};
        // Donor formula: `huf_section_size >= literals_len - min_gain`,
        // payload-vs-srcSize (no headers on either side). Verify the
        // gate is symmetric in header overhead by hitting the boundary
        // where the old on-wire `total >= raw_section - mg` form would
        // have disagreed.
        let strategy = StrategyTag::Fast; // min_gain(20, Fast) = (20>>6)+2 = 2

        // literals_len = 20: raw_header = 1, compressed_header = 3.
        // New threshold (payload-vs-srcSize):
        //   keep huf iff huf_section_size <  20 - 2 = 18
        //   fallback   iff huf_section_size >= 18
        //
        // Old (regressed) threshold on-wire-vs-on-wire:
        //   total = huf_section_size + 3
        //   fallback iff total >= (20 + 1) - 2 = 19
        //                iff huf_section_size >= 16
        //
        // Gap where formulas disagree: huf_section_size in [16, 18).
        // The new formula MUST keep huf in this gap.
        let literals_len = 20usize;
        // Sanity-check the literals-header constants the math relies on.
        assert_eq!(compressed_literals_header_bytes(literals_len), 3);
        assert_eq!(super::uncompressed_literals_header_bytes(literals_len), 1);

        // Inside the gap — new keeps, old would have rejected:
        assert!(!use_raw_literal_fallback(16, literals_len, strategy));
        assert!(!use_raw_literal_fallback(17, literals_len, strategy));

        // At/above new threshold — both new and old fall back:
        assert!(use_raw_literal_fallback(18, literals_len, strategy));
        assert!(use_raw_literal_fallback(19, literals_len, strategy));

        // Below the old threshold — both keep:
        assert!(!use_raw_literal_fallback(15, literals_len, strategy));
        assert!(!use_raw_literal_fallback(0, literals_len, strategy));
    }

    #[test]
    fn prefer_repeat_eligible_matches_donor_gate() {
        use super::prefer_repeat_eligible;
        // Donor `zstd_compress_literals.c:165`:
        //   strategy < ZSTD_lazy && srcSize <= 1024 -> HUF_flags_preferRepeat
        // ZSTD_lazy == 4 in donor enum; our `< Lazy` set is
        // {Fast, Dfast, Greedy}. Verify the gate fires for each
        // and stays off for the rest.
        for lit_len in [0usize, 1, 64, 256, 1024] {
            assert!(
                prefer_repeat_eligible(StrategyTag::Fast, lit_len),
                "Fast/{lit_len}"
            );
            assert!(
                prefer_repeat_eligible(StrategyTag::Dfast, lit_len),
                "Dfast/{lit_len}"
            );
            assert!(
                prefer_repeat_eligible(StrategyTag::Greedy, lit_len),
                "Greedy/{lit_len}"
            );
            assert!(
                !prefer_repeat_eligible(StrategyTag::Lazy, lit_len),
                "Lazy/{lit_len}"
            );
            assert!(
                !prefer_repeat_eligible(StrategyTag::BtOpt, lit_len),
                "BtOpt/{lit_len}"
            );
            assert!(
                !prefer_repeat_eligible(StrategyTag::BtUltra, lit_len),
                "BtUltra/{lit_len}"
            );
            assert!(
                !prefer_repeat_eligible(StrategyTag::BtUltra2, lit_len),
                "BtUltra2/{lit_len}"
            );
        }
        // Above the 1024-byte size threshold the gate stays off
        // for ALL strategies (donor `srcSize <= 1024` is the
        // closed upper bound).
        for lit_len in [1025usize, 2048, 16384] {
            assert!(!prefer_repeat_eligible(StrategyTag::Fast, lit_len));
            assert!(!prefer_repeat_eligible(StrategyTag::Dfast, lit_len));
            assert!(!prefer_repeat_eligible(StrategyTag::Greedy, lit_len));
        }
    }

    #[test]
    fn decide_huff_reuse_prefer_repeat_forces_reuse_for_fast_band() {
        use super::{decide_huff_reuse_like_encoder, huff0_encoder};
        // Fixture chosen so size-comparison heuristic and the
        // preferRepeat short-circuit DISAGREE: `prev` is built
        // from a broad uniform sweep so it can encode any byte;
        // the literals payload is heavily skewed (240 zeros + 16
        // outliers) so a freshly-built `new_tbl` would compress
        // strictly better than `prev`. Without preferRepeat, the
        // heuristic picks `new` (returns true); WITH preferRepeat
        // the fast-band gate forces reuse (returns false).
        // Removing the short-circuit flips Fast/Dfast/Greedy to
        // true and breaks this test — that's the regression gate.
        let prev_training: Vec<u8> = (0..1024u32).map(|i| (i % 256) as u8).collect();
        let prev = huff0_encoder::HuffmanTable::build_from_data(&prev_training);
        let mut skewed_literals: Vec<u8> = Vec::with_capacity(256);
        skewed_literals.extend(core::iter::repeat_n(0u8, 240));
        skewed_literals.extend((0..16u8).map(|i| 200 + i));
        let new_tbl = huff0_encoder::HuffmanTable::build_from_data(&skewed_literals);
        let new_desc = new_tbl
            .writeable_table_description_size()
            .expect("non-empty table emits a description");

        // Distinguishing precondition: WITHOUT preferRepeat the
        // size comparison must prefer new (else the test isn't
        // exercising the override). Verify by running with a
        // strategy outside the eligible band: Lazy returns
        // true (=new) on this fixture.
        assert!(
            decide_huff_reuse_like_encoder(
                &new_tbl,
                Some(&prev),
                new_desc,
                &skewed_literals,
                StrategyTag::Lazy,
            ),
            "fixture precondition: size-comparison must prefer new for Lazy on skewed literals"
        );

        // Eligible fast band: preferRepeat forces reuse (=false)
        // despite size-comparison preferring new.
        for strategy in [StrategyTag::Fast, StrategyTag::Dfast, StrategyTag::Greedy] {
            assert!(
                !decide_huff_reuse_like_encoder(
                    &new_tbl,
                    Some(&prev),
                    new_desc,
                    &skewed_literals,
                    strategy,
                ),
                "{strategy:?} <= 1024 must short-circuit to reuse despite size-comparison favouring new"
            );
        }

        // Above the 1024-byte threshold the gate stays off even
        // for Fast/Dfast/Greedy — falls through to the size
        // heuristic, which on this fixture prefers new.
        let mut big_skewed = skewed_literals.clone();
        big_skewed.extend(core::iter::repeat_n(0u8, 1024));
        assert!(
            big_skewed.len() > 1024,
            "fixture must exceed 1024 to disable preferRepeat"
        );
        assert!(
            decide_huff_reuse_like_encoder(
                &new_tbl,
                Some(&prev),
                new_desc,
                &big_skewed,
                StrategyTag::Fast,
            ),
            "Fast at len > 1024 must NOT short-circuit (gate disabled), falls through to size heuristic"
        );
    }

    #[test]
    fn estimator_literals_section_mirrors_emit_for_short_inputs() {
        use super::{
            CompressedBlockScratch, EntropyOnlyMatcher, EstimatorWorkspace,
            encode_block_parts_with_sequence_scratch, estimate_block_parts_size,
        };
        // For each strategy at boundary literal lengths around `min_lits`
        // and across the all-identical RLE pre-check (fires for any
        // non-empty all-identical input under every strategy/HUF state),
        // estimator's predicted size MUST equal the bytes the emitter
        // actually writes. Cases include: (a) fresh state with
        // `last_huff_table: None` covering the strategy-specific
        // `min_lits` band (8/16/32/64), (b) seeded HUF-reuse state at
        // the lowered floor of 6 — under the prior hardcoded `len >= 8`
        // gate 6/7-byte all-identical sections went raw, now they pass
        // `min_lits == 6` and the donor-parity HUF+`cLitSize==1` path
        // would route them to RLE; the pre-check shortcuts that path,
        // (c) sub-`min_lits` all-identical sections that take the
        // RLE pre-check regardless of strategy.
        type Inputs = &'static [(usize, bool)];
        let cases: &[(StrategyTag, bool, Inputs)] = &[
            // (strategy, seed_huff_reuse, [(len, all_identical)])
            (
                StrategyTag::Fast,
                false,
                &[
                    (1, true), // sub-min_lits all-identical → RLE
                    (5, true), // sub-min_lits all-identical → RLE
                    (8, true),
                    (8, false),
                    (63, true),
                    (63, false),
                    (64, false),
                ],
            ),
            (
                StrategyTag::BtUltra2,
                false,
                &[(7, true), (7, false), (8, true), (8, false), (16, false)],
            ),
            (
                StrategyTag::BtOpt,
                false,
                &[(8, true), (31, true), (32, false)],
            ),
            // HUF reuse path: floor drops to 6. Under the prior
            // hardcoded `len >= 8` gate 6/7-byte sections went raw;
            // post-fix, all-identical 6/7-byte sections take the RLE
            // pre-check and stay byte-equivalent estimator-vs-emit
            // (donor parity would route them HUF→cLitSize==1→RLE).
            // Also exercise non-identical 6-byte raw fallback and
            // 16-byte HUF reuse path.
            (
                StrategyTag::Lazy,
                true,
                &[(6, true), (7, true), (6, false), (16, false)],
            ),
        ];

        for (strat, seed_huff, inputs) in cases {
            for (len, identical) in *inputs {
                let literals: Vec<u8> = if *identical {
                    alloc::vec![0x5Au8; *len]
                } else {
                    (0..*len as u8).collect()
                };
                // Seed both estimator and emit state with the same
                // synthetic HUF table when `seed_huff` is true so the
                // reuse path's `min_lits == 6` floor is exercised.
                // Counts from a varied byte sequence give a valid
                // (writeable) table that survives the `decide_huff_reuse`
                // decision when literals are large enough to consider it.
                let seed_table = if *seed_huff {
                    let mut counts = [0usize; 256];
                    for b in (0..=63u8).chain(64..=127u8) {
                        counts[b as usize] = 1;
                    }
                    Some(huff0_encoder::HuffmanTable::build_from_counts(&counts))
                } else {
                    None
                };
                let mut est_state = CompressState::<EntropyOnlyMatcher> {
                    matcher: EntropyOnlyMatcher,
                    last_huff_table: seed_table.clone(),
                    fse_tables: FseTables::new(),
                    block_scratch: CompressedBlockScratch::new(),
                    offset_hist: [1, 4, 8],
                    strategy_tag: *strat,
                };
                let mut emit_state = CompressState::<EntropyOnlyMatcher> {
                    matcher: EntropyOnlyMatcher,
                    last_huff_table: seed_table,
                    fse_tables: FseTables::new(),
                    block_scratch: CompressedBlockScratch::new(),
                    offset_hist: [1, 4, 8],
                    strategy_tag: *strat,
                };
                let mut workspace = EstimatorWorkspace::default();
                let est = estimate_block_parts_size(&mut est_state, &literals, &[], &mut workspace);
                let mut emitted: Vec<u8> = Vec::new();
                let mut scratch: Vec<crate::blocks::sequence_section::Sequence> = Vec::new();
                encode_block_parts_with_sequence_scratch(
                    &mut emit_state,
                    &literals,
                    &[],
                    &mut emitted,
                    &mut scratch,
                );
                assert_eq!(
                    est,
                    emitted.len(),
                    "estimator/emit parity broken: strategy={:?} seed_huff={} len={} identical={} est={} emit={}",
                    strat,
                    seed_huff,
                    len,
                    identical,
                    est,
                    emitted.len(),
                );
            }
        }
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
            strategy_tag: crate::encoding::strategy::StrategyTag::Fast,
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
