use alloc::{boxed::Box, vec::Vec};

mod literal_runs;

use crate::{
    bit_io::BitWriter,
    blocks::block::BlockType,
    encoding::block_header::BlockHeader,
    encoding::fastpath::EncodeTier,
    encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable, SharedFseTable},
    encoding::workspace::{FORMAT_MIN_MATCH, RegionVec, Workspace, region_bytes},
    encoding::{Matcher, Sequence},
    fse::fse_encoder::{
        FSETable, build_seq_ctable_into, build_table_from_symbol_counts_into,
        fse_header_bits_for_counts,
    },
    huff0::huff0_encoder,
};

const MIN_SEQUENCES_BLOCK_SPLITTING: usize = 300;
const MAX_NB_BLOCK_SPLITS: usize = 196;

/// Upstream zstd `ZSTD_minLiteralsToCompress` (`zstd_compress_literals.c:114-127`):
/// strategy-aware floor below which `compress_literals` does not even
/// attempt huf compression and falls back to raw.
///
/// Formula: `shift = MIN(9 - strategy, 3); mintc = (huf_repeat ==
/// valid) ? 6 : (8 << shift)`. With huf reuse available, the per-block huf
/// header overhead is gone, so the cheap floor is 6 bytes. Without it, the
/// huf tree-description must be serialized per block — alphabet size and
/// max symbol determine its exact byte cost, but on payloads near the
/// per-strategy floor that overhead dominates and the compressed section
/// loses to raw. Upstream zstd's shift table picks the floor per strategy:
/// strategy 1..6 → 64 bytes, strategy 7 (btopt) → 32, strategy 8 (btultra)
/// → 16, strategy 9 (btultra2) → 8.
///
/// Our `StrategyTag` enum has eight variants: `Lazy` covers upstream zstd strategies
/// 4..5 (greedy/lazy/lazy2) and `Btlazy2` is the separate upstream zstd strategy 6.
/// Within the fast..btlazy2 band upstream zstd's shift table is flat: strategies 1..6
/// all pin `shift = MIN(9 - strat, 3) = 3`, so both `Lazy` and `Btlazy2` land
/// on the 64-byte floor. No aggressiveness gradient within this band to
/// preserve (the gradient only starts at btopt).
///
/// The floor drops to 6 only for a valid previous table (upstream
/// `huf_repeat == HUF_repeat_valid`): a dictionary's table coding every byte,
/// not one a block built.
#[inline]
fn min_literals_to_compress(
    strategy: crate::encoding::strategy::StrategyTag,
    repeat_valid: bool,
) -> usize {
    use crate::encoding::strategy::StrategyTag;
    if repeat_valid {
        return 6;
    }
    let shift: u32 = match strategy {
        StrategyTag::Fast
        | StrategyTag::Dfast
        | StrategyTag::Greedy
        | StrategyTag::Lazy
        | StrategyTag::Btlazy2 => 3,
        StrategyTag::BtOpt => 2,
        StrategyTag::BtUltra => 1,
        StrategyTag::BtUltra2 => 0,
    };
    8usize << shift
}

/// Upstream zstd `ZSTD_minGain` (`zstd_compress_internal.h:677-684`):
/// strategy-aware minimum-compression margin. In upstream zstd it gates both
/// the block-level "compressed block must beat raw + minGain" decision
/// and the literal-section `cLitSize >= srcSize - minGain` fallback.
///
/// Formula: `minlog = (strat >= btultra) ? strat - 1 : 6; (src_size >>
/// minlog) + 2`. So:
/// - fast..btopt (strat 1..7): minlog=6 → ~1.5% margin + 2 bytes
/// - btultra (strat 8): minlog=7 → ~0.78% margin + 2 bytes
/// - btultra2 (strat 9): minlog=8 → ~0.39% margin + 2 bytes
///
/// Wired into the literal-section raw fallback ([`use_raw_literal_fallback`])
/// and the block-level one ([`keeps_compressed_block`]).
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

/// Whether a block whose `src_size` bytes compressed to a `payload`-byte
/// section is written compressed rather than raw: upstream keeps it only when
/// it saves `minGain` (`ZSTD_entropyCompressSeqStore`, zstd_compress.c:2967:
/// `cSize >= srcSize - minGain` returns 0, a raw block). Shared by every path
/// that writes or prices a block, so they agree on which blocks go out raw.
#[inline]
pub(crate) fn keeps_compressed_block(
    payload: usize,
    src_size: usize,
    strategy: crate::encoding::strategy::StrategyTag,
) -> bool {
    // Upstream never compresses a block under seven bytes
    // (`ZSTD_buildSeqStore` returns `ZSTDbss_noCompress` below
    // `MIN_CBLOCK_SIZE + ZSTD_blockHeaderSize + 1 + 1`), which also keeps the
    // subtraction below from underflowing.
    const MIN_COMPRESSED_SOURCE: usize = 2 + 3 + 1 + 1;
    src_size >= MIN_COMPRESSED_SOURCE && payload < src_size - min_gain(src_size, strategy)
}

/// Whether a literals section is coded as one Huffman stream rather than four:
/// below 256 bytes, or below 1 KiB when the previous table is valid (upstream
/// `ZSTD_compressLiterals`, zstd_compress_literals.c:142 and :171).
#[inline]
fn single_literal_stream(len: usize, repeat_valid: bool) -> bool {
    len < 256 || (repeat_valid && len < 1024)
}

/// Upstream zstd `compress_literals` raw-fallback gate
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

/// Upstream zstd `kInverseProbabilityLog256`: floor(-log2(x / 256) * 256).
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

/// A block's literals and sequences, in the context's workspace.
#[derive(Default)]
struct EncodedBlockParts {
    literals: RegionVec<u8>,
    sequences: RegionVec<RawSequence>,
}

#[derive(Default)]
pub(crate) struct CompressedBlockScratch {
    parts: EncodedBlockParts,
    partitions: Vec<usize>,
    prefix_sums: SequencePrefixSums,
    compressed: Vec<u8>,
    /// Lazily allocated: only the block-split estimator path uses it, and
    /// `compress_block`'s `mem::take` constructs a throwaway `Default`
    /// scratch every block — an eager workspace made that default pay four
    /// 2 KiB `Box<[usize; 256]>` allocations per block for paths that never
    /// probe a split.
    estimator_workspace: Option<EstimatorWorkspace>,
    /// Reusable scratch for the block-split estimator's inner
    /// `CompressState` — kept across frames so the estimator does not
    /// re-allocate a whole `CompressedBlockScratch` (4×`Box<[u32;256]>`
    /// count tables + Vecs) every frame in a reused compressor. `Box`
    /// breaks the type recursion; `None` by default (lazily filled on
    /// first block-split). The estimator uses `EntropyOnlyMatcher` and
    /// never re-splits, so this nesting is one level deep.
    estimator_inner: Option<Box<CompressedBlockScratch>>,
}

impl CompressedBlockScratch {
    /// Heap bytes this scratch keeps between blocks, for a caller sizing a
    /// context. Every buffer here is kept on purpose — the scratch is taken and
    /// put back around a block rather than rebuilt — so all of them count, not
    /// only the two that are behind an `Option`. The nested estimator scratch is
    /// a `Box`, so its own allocation counts as well as whatever it reports: the
    /// owner's `size_of` covers the pointer, not what it points at, and leaving
    /// the box out understated a context that has taken the post-split path by
    /// the whole struct.
    ///
    /// The literal and sequence buffers live in the context's workspace and are
    /// counted with it, not here.
    pub(crate) fn retained_heap_size(&self) -> usize {
        self.partitions.capacity() * core::mem::size_of::<usize>()
            + self.prefix_sums.heap_size()
            + self.compressed.capacity()
            + self
                .estimator_workspace
                .as_ref()
                .map_or(0, EstimatorWorkspace::heap_size)
            + self.estimator_inner.as_ref().map_or(0, |inner| {
                core::mem::size_of::<CompressedBlockScratch>() + inner.retained_heap_size()
            })
    }

    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The most [`Self::retained_heap_size`] reports for blocks of up to
    /// `block_capacity` bytes: the entropy-coded block, and where `splits` is
    /// set, everything the post-split splitter keeps — its partitions, the
    /// prefix sums, the probe tables and the nested scratch its probes use.
    ///
    /// The splitter recurses by halves down to `MIN_SEQUENCES_BLOCK_SPLITTING`
    /// sequences and splits at most `MAX_NB_BLOCK_SPLITS` times, so a block
    /// visits at most `2 * MAX_NB_BLOCK_SPLITS + 1` ranges at three probes
    /// each, and holds at most three tables per live probe state, two states
    /// per level of recursion.
    pub(crate) fn max_retained_heap_size(block_capacity: usize, splits: bool) -> usize {
        let compressed = crate::encoding::compress_bound(block_capacity);
        if !splits {
            return compressed;
        }
        let usize_sz = core::mem::size_of::<usize>();
        // A bound for any frame, so the format's minimum match.
        let sequences = max_sequences(block_capacity, FORMAT_MIN_MATCH);
        let partitions = (MAX_NB_BLOCK_SPLITS + 1).next_power_of_two() * usize_sz;
        // `lit` and `ml` are reserved exactly; the codes and their bit sums
        // grow by doubling, so they keep up to twice what a block needed.
        let prefix_sums =
            2 * (sequences + 1) * usize_sz + 2 * (sequences + 1) * usize_sz + 2 * 2 * sequences;
        let depth = (sequences / MIN_SEQUENCES_BLOCK_SPLITTING + 1)
            .next_power_of_two()
            .trailing_zeros() as usize
            + 1;
        let pooled_tables = 3 * (2 * depth + 4);
        let probes = 3 * (2 * MAX_NB_BLOCK_SPLITS + 1) + 1;
        let estimator = core::mem::size_of::<[usize; 256]>()
            + 3 * core::mem::size_of::<SequenceHistogram>()
            + pooled_tables.next_power_of_two() * core::mem::size_of::<SharedFseTable>()
            + pooled_tables
                * (core::mem::size_of::<FSETable>()
                    + crate::encoding::frame_compressor::shared_table_overhead())
            + probes.next_power_of_two() * core::mem::size_of::<huff0_encoder::HuffmanTable>();
        let inner = core::mem::size_of::<CompressedBlockScratch>()
            + Self::max_retained_heap_size(block_capacity, false);
        compressed + partitions + prefix_sums + estimator + inner
    }

    /// Workspace bytes the buffers need for blocks of up to `block_capacity`
    /// source bytes whose sequences cover at least `sequence_floor` bytes each.
    pub(crate) fn workspace_bytes(block_capacity: usize, sequence_floor: usize) -> usize {
        let sequences = max_sequences(block_capacity, sequence_floor);
        region_bytes::<u8>(block_capacity) + region_bytes::<RawSequence>(sequences)
    }

    /// Takes the buffers from the open `workspace` layout, sized for blocks of
    /// up to `block_capacity` source bytes and the layout's sequence floor.
    pub(crate) fn bind(&mut self, workspace: &mut Workspace, block_capacity: usize) {
        let sequences = max_sequences(block_capacity, workspace.sequence_floor());
        self.parts.literals = workspace.buffer(block_capacity);
        self.parts.sequences = workspace.buffer(sequences);
    }

    /// Drops the buffers, for a frame that builds no compressed block and so
    /// lays none out; a region of an earlier layout must not outlive it.
    pub(crate) fn unbind(&mut self) {
        self.parts.literals = RegionVec::empty();
        self.parts.sequences = RegionVec::empty();
    }
}

/// The most sequences a block of `block_len` source bytes can hold when each
/// covers at least `sequence_floor` of them (upstream `ZSTD_maxNbSeq`,
/// zstd_compress.c, dividing by 3 or 4 as the frame's minimum match allows).
fn max_sequences(block_len: usize, sequence_floor: usize) -> usize {
    block_len / sequence_floor
}

#[derive(Default)]
struct SequencePrefixSums {
    lit: Vec<usize>,
    ml: Vec<usize>,
    /// Each sequence's literal-length and match-length codes, and a prefix sum
    /// of the extra bits the two carry. Neither depends on the offset history,
    /// so they are derived once per block (upstream zstd `ZSTD_seqToCodes`) and
    /// every split probe reads its range from here.
    ll_code: Vec<u8>,
    ml_code: Vec<u8>,
    length_bits: Vec<usize>,
}

/// The precomputed length codes of a run of sequences.
#[derive(Clone, Copy)]
struct LengthCodes<'a> {
    ll: &'a [u8],
    ml: &'a [u8],
    /// Extra bits of every literal and match length in the run.
    bits: usize,
}

impl SequencePrefixSums {
    fn heap_size(&self) -> usize {
        (self.lit.capacity() + self.ml.capacity() + self.length_bits.capacity())
            * core::mem::size_of::<usize>()
            + self.ll_code.capacity()
            + self.ml_code.capacity()
    }

    /// The length codes of sequences `start..end`.
    fn codes(&self, start: usize, end: usize) -> LengthCodes<'_> {
        LengthCodes {
            ll: &self.ll_code[start..end],
            ml: &self.ml_code[start..end],
            bits: self.length_bits[end] - self.length_bits[start],
        }
    }

    fn rebuild(&mut self, sequences: &[RawSequence]) {
        self.ll_code.clear();
        self.ml_code.clear();
        self.length_bits.clear();
        self.ll_code.reserve(sequences.len());
        self.ml_code.reserve(sequences.len());
        self.length_bits.reserve(sequences.len() + 1);
        let mut bits = 0usize;
        self.length_bits.push(0);
        for seq in sequences {
            let (ll, _, ll_bits) = encode_literal_length(seq.ll);
            let (ml, _, ml_bits) = encode_match_len(seq.ml);
            self.ll_code.push(ll);
            self.ml_code.push(ml);
            bits += ll_bits + ml_bits;
            self.length_bits.push(bits);
        }
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

/// One collected sequence.
///
/// `off_base` holds the offset the matcher found until [`fill_and_count`]
/// runs over the sequence, and the wire code from then on: 1/2/3 for the repeat
/// offsets, N+3 for an explicit N. It is one field rather than two because the
/// found offset has no reader once its code exists, and a fourth word would
/// widen every sequence in the block for a value with a lifetime of one pass.
/// Upstream keeps the same single slot, filled at match time
/// (`SeqDef::offBase`, written by `ZSTD_storeSeq`); ours cannot be filled that
/// early because the code depends on a repeat-offset history that must not
/// advance across a partition the emitter ends up writing raw.
///
/// Each length carries its own code and extra-bit width in the bits above it,
/// written by [`fill_and_count`] and read by the bitstream writer, so a block
/// needs no buffer of codes beside its sequences. A block's lengths stay under
/// 2^17 (the literal length) and at most 2^17 (the match length), which leaves
/// 15 and 14 free bits for the 11 each needs. The offset code is not stored:
/// it is the high bit of `off_base`, one instruction away. The length codes
/// are kept because recomputing them in the writer costs two table lookups, a
/// bit scan and the branches between the small-value tables and the
/// logarithmic form; upstream likewise keeps its codes between the same two
/// passes (`ZSTD_seqToCodes`).
#[derive(Clone, Copy)]
struct RawSequence {
    ll: u32,
    ml: u32,
    off_base: u32,
}

/// Where the literal-length code starts in `ll`; its extra-bit width follows
/// six bits higher.
const LL_CODE_SHIFT: u32 = 17;
/// Where the match-length code starts in `ml`; its width follows likewise.
const ML_CODE_SHIFT: u32 = 18;

impl RawSequence {
    /// Stores each length's code and extra-bit width above the length.
    #[inline(always)]
    fn set_codes(&mut self, ll_code: u8, ll_bits: usize, ml_code: u8, ml_bits: usize) {
        debug_assert!(ll_code < 64 && ml_code < 64 && ll_bits < 32 && ml_bits < 32);
        debug_assert!(self.ll >> LL_CODE_SHIFT == 0 && self.ml >> ML_CODE_SHIFT == 0);
        self.ll |= (ll_code as u32 | (ll_bits as u32) << 6) << LL_CODE_SHIFT;
        self.ml |= (ml_code as u32 | (ml_bits as u32) << 6) << ML_CODE_SHIFT;
    }

    #[inline(always)]
    fn ll_code(&self) -> u8 {
        (self.ll >> LL_CODE_SHIFT & 63) as u8
    }

    #[inline(always)]
    fn ll_bits(&self) -> usize {
        (self.ll >> (LL_CODE_SHIFT + 6)) as usize
    }

    #[inline(always)]
    fn ml_code(&self) -> u8 {
        (self.ml >> ML_CODE_SHIFT & 63) as u8
    }

    #[inline(always)]
    fn ml_bits(&self) -> usize {
        (self.ml >> (ML_CODE_SHIFT + 6)) as usize
    }

    /// The offset code, which is also its extra-bit width (upstream's
    /// `ofBits = ofCode`).
    #[inline(always)]
    fn of_code(&self) -> u8 {
        // An offset value is at least 1 (a repeat code), so the high bit exists.
        debug_assert!(self.off_base != 0);
        (31 - self.off_base.leading_zeros()) as u8
    }
}

struct EntropyOnlyMatcher;

impl Matcher for EntropyOnlyMatcher {
    fn get_last_space(&mut self) -> &[u8] {
        unreachable!("entropy estimator never reads source bytes")
    }

    fn fill_in_place(
        &mut self,
        _capacity: usize,
        _fill: &mut dyn FnMut(&mut crate::encoding::HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        unreachable!("entropy estimator never takes input")
    }

    fn uncommitted_input(&self) -> &[u8] {
        unreachable!("entropy estimator never takes input")
    }

    fn commit_filled(&mut self, _len: usize) {
        unreachable!("entropy estimator never commits input")
    }

    fn skip_matching(&mut self) {
        unreachable!("entropy estimator never updates match state")
    }

    fn start_matching(&mut self, _handle_sequence: impl FnMut(Sequence)) {
        unreachable!("entropy estimator never generates sequences")
    }

    fn reset(&mut self, _level: crate::encoding::CompressionLevel) {}

    fn window_size(&self) -> u64 {
        0
    }
}

/// A block of [`crate::blocks::block::BlockType::Compressed`]. Its tables are
/// returned for the caller to [`commit_block_tables`] if it keeps the block,
/// or [`discard_block_tables`] if it writes the block raw instead.
#[must_use = "the block's tables have to be committed or discarded"]
pub(crate) fn compress_block<M: Matcher, K: EncodeTier>(
    state: &mut CompressState<M>,
    output: &mut Vec<u8>,
) -> BlockTables {
    let mut scratch = core::mem::take(&mut state.block_scratch);
    collect_block_parts::<M, K>(state, &mut scratch.parts);
    let tables = encode_block_parts::<M, K>(
        state,
        &scratch.parts.literals,
        &mut scratch.parts.sequences,
        output,
    );
    state.block_scratch = scratch;
    tables
}

pub(crate) fn compress_block_with_post_split<M: Matcher, K: EncodeTier>(
    state: &mut CompressState<M>,
    last_block: bool,
    output: &mut Vec<u8>,
    #[cfg(feature = "lsm")] mut block_decompressed_sizes: Option<&mut Vec<u32>>,
    #[cfg(all(feature = "lsm", feature = "hash"))] mut block_checksums: Option<&mut Vec<u32>>,
) {
    let mut scratch = core::mem::take(&mut state.block_scratch);
    collect_block_parts::<M, K>(state, &mut scratch.parts);
    if scratch.parts.sequences.len() <= 4 {
        let source_len = state.matcher.get_last_space().len();
        #[cfg(feature = "lsm")]
        if let Some(sink) = block_decompressed_sizes.as_deref_mut() {
            sink.push(source_len as u32);
        }
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
        };
        let emitted_raw = emit_single_sequence_block::<M, K>(
            state,
            last_block,
            source_len,
            &scratch.parts.literals,
            &mut scratch.parts.sequences,
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
    let mut workspace = scratch.estimator_workspace.take().unwrap_or_default();
    let built_huff = core::mem::take(&mut workspace.built_huff);
    // Reuse the estimator's inner scratch across frames instead of
    // allocating a fresh `CompressedBlockScratch` (count tables + Vecs)
    // every block-split. Lazily created on the first split.
    let inner_scratch = scratch
        .estimator_inner
        .take()
        .map(|b| *b)
        .unwrap_or_default();
    let mut estimator = SplitEstimator {
        parts: &scratch.parts,
        prefix_sums: &scratch.prefix_sums,
        block_entry: ProbeEntryState {
            huff: if state.last_huff_table.is_some() {
                HuffRef::BlockEntry
            } else {
                HuffRef::None
            },
            ll_previous: state.fse_tables.ll_previous.clone(),
            ml_previous: state.fse_tables.ml_previous.clone(),
            of_previous: state.fse_tables.of_previous.clone(),
            offset_hist: state.offset_hist,
        },
        entry_huff: state.last_huff_table.as_ref(),
        entry_huff_valid: state.huf_repeat_valid,
        built_huff,
        scratch_state: CompressState {
            matcher: EntropyOnlyMatcher,
            sequence_capture: None,
            // The splitter's scratch state never reaches the raw-skip, which
            // is decided one level up on the whole block.
            seen_content: Default::default(),
            // Inherited rather than re-resolved: this scratch state stands in
            // for the same compressor on the same CPU.
            copy_kernel: state.copy_kernel,
            // Probes read the table they repeat from `entry_huff` /
            // `built_huff`; this slot stays empty.
            last_huff_table: None,
            huf_repeat_valid: false,
            huff_table_spare: None,
            // Lent, not created: the estimator builds a table per split
            // candidate, and a fresh scratch here would take its buffers again
            // on every post-split block and drop them at the end of it. Handed
            // back below, so the emitter that follows keeps using the same one.
            huff_weights: core::mem::take(&mut state.huff_weights),
            fse_tables: probe_fse_tables(&state.fse_tables),
            block_scratch: inner_scratch,
            // The probes estimate from the outer block's buffers and never
            // collect or encode a block of their own, so nothing is carved.
            workspace: crate::encoding::workspace::Workspace::new(),
            offset_hist: state.offset_hist,
            strategy_tag: state.strategy_tag,
            split_strategy: state.split_strategy,
            pre_split: state.pre_split,
            huf_optimal_search: state.huf_optimal_search,
            literal_compression_disabled: state.literal_compression_disabled,
            post_split_allowed: state.post_split_allowed,
        },
        workspace,
    };
    estimator.derive_block_splits(0, scratch.parts.sequences.len(), &mut scratch.partitions);
    scratch.partitions.push(scratch.parts.sequences.len());
    workspace = estimator.workspace;
    // Stash the inner scratch back for the next frame (its buffers stay
    // allocated; the estimator clears them per use), and take the weight
    // builder's buffers back so the emitter and the next block reuse them.
    let CompressState {
        block_scratch: inner_block_scratch,
        mut huff_weights,
        fse_tables: probe_tables,
        ..
    } = estimator.scratch_state;
    // The last probe's tables join the pool for the next block's probes.
    workspace.recycle_previous(probe_tables.ll_previous);
    workspace.recycle_previous(probe_tables.ml_previous);
    workspace.recycle_previous(probe_tables.of_previous);
    for handle in [
        probe_tables.ll_next,
        probe_tables.ml_next,
        probe_tables.of_next,
    ]
    .into_iter()
    .flatten()
    {
        workspace.recycle_fse(handle);
    }
    // The tables the probes built go back to the builder for its next tables,
    // and their emptied arena to the workspace for the next block.
    let mut built_huff = estimator.built_huff;
    for table in built_huff.drain(..) {
        huff_weights.recycle(table);
    }
    workspace.built_huff = built_huff;
    scratch.estimator_workspace = Some(workspace);
    state.huff_weights = huff_weights;
    scratch.estimator_inner = Some(Box::new(inner_block_scratch));

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
        #[cfg(feature = "lsm")]
        if let Some(sink) = block_decompressed_sizes.as_deref_mut() {
            sink.push(src_size as u32);
        }
        #[cfg(all(feature = "lsm", feature = "hash"))]
        if let Some(sink) = block_checksums.as_deref_mut() {
            sink.push(crate::encoding::frame_compressor::xxh64_block_low32(
                &state.matcher.get_last_space()[src_start..src_start + src_size],
            ));
        }
        let mut emit_buffers = SingleSequenceEmitBuffers {
            output,
            compressed: &mut scratch.compressed,
        };
        let emitted_raw = emit_single_sequence_block::<M, K>(
            state,
            last_block && last_partition,
            src_size,
            &scratch.parts.literals[lit_start..lit_end],
            &mut scratch.parts.sequences[seq_start..seq_end],
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

/// Run the matcher over the block and collect its sequences, then its literals.
///
/// The emit records a sequence and nothing else; the literal runs are copied
/// afterwards in one pass (see [`literal_runs`]), under the tier resolved when
/// the compressor was built. Upstream copies each run as it stores the
/// sequence (`ZSTD_storeSeq`); here the match loop calls the emit from a dozen
/// sites, and a copy inlined at each of them costs the loop more in decode
/// pressure than the calls it saves (+2.16% cycles at level 3, measured).
fn collect_block_parts<M: Matcher, K: EncodeTier>(
    state: &mut CompressState<M>,
    parts: &mut EncodedBlockParts,
) {
    parts.literals.clear();
    parts.sequences.clear();
    // Trailing literals, reported after the last sequence; the runs are read
    // back by position, so no sequence may follow them.
    let mut tail = 0usize;
    let sequences = &mut parts.sequences;
    // The Fast band codes each offset as it is emitted, against the history the
    // block starts from. Upstream decides the offBase at the store
    // (`ZSTD_storeSeq` in `zstd_fast.c`); here the derivation is inlined at
    // each of the kernel's emit sites, where the rep-or-explicit outcome is
    // nearly fixed per site and predicts, instead of in a later pass where every
    // sequence waits on the history the previous one rotated. A block written
    // raw restores `offset_hist`, so the next block starts from the decoder's
    // history either way.
    //
    // Greedy and above keep the raw offset and are coded once the partition a
    // sequence lands in is about to be encoded (`fill_and_count`), since the
    // post-split can rewind the history at a partition boundary.
    //
    // A flag rather than a const parameter: one closure type keeps one copy of
    // every matcher's scan, where two would compile each backend twice, and the
    // flag is fixed for the block, so its test at each emit site predicts.
    let code_offsets = uses_fast_offset_codes(state.strategy_tag);
    let block_start_hist = state.offset_hist;
    let mut hist = state.offset_hist;
    // The built-in matcher searches compiled for the frame's tier; a matcher
    // from outside the crate knows nothing of tiers. The branch folds per
    // matcher type.
    match state.matcher.builtin_driver() {
        Some(driver) => {
            builtin_block_sequences::<K>(driver, &mut tail, sequences, code_offsets, &mut hist);
            // The unchecked push rests on every built-in sequence covering the
            // frame's sequence floor; checked here, after the search, so the
            // record inlined in the match loop carries nothing for it.
            #[cfg(debug_assertions)]
            {
                let floor = state.workspace.sequence_floor();
                for seq in sequences.iter() {
                    let ml = seq.ml as usize;
                    debug_assert!(
                        ml >= floor,
                        "a {ml}-byte match under a sequence floor of {floor}"
                    );
                }
            }
        }
        None => state.matcher.start_matching(|seq| {
            record_sequence(seq, &mut tail, sequences, code_offsets, &mut hist)
        }),
    }
    if code_offsets {
        state.offset_hist = hist;
    }
    // Once per block, after the match loop: the loop itself is the same
    // whether or not anyone records it. One delimiter per matched block is
    // one per written block, as `compress_block_encoded` keeps the post-split
    // off while a parse is recorded.
    if let Some(capture) = state.sequence_capture.as_mut() {
        record_block_parse(
            capture,
            &parts.sequences,
            tail,
            code_offsets,
            block_start_hist,
        );
    }
    literal_runs::gather_literals::<K>(
        state.matcher.get_last_space(),
        &parts.sequences,
        tail,
        &mut parts.literals,
    );
}

/// The built-in matcher's search of the block under tier `K`, recorded as in
/// [`collect_block_parts`]. Generic over the tier alone: the record closure's
/// type carries every parameter of the function it is written in, so written
/// in `collect_block_parts` it would compile the whole search again for each
/// matcher type that wraps the driver.
#[inline]
fn builtin_block_sequences<K: EncodeTier>(
    driver: &mut crate::encoding::match_generator::MatchGeneratorDriver,
    tail: &mut usize,
    sequences: &mut RegionVec<RawSequence>,
    code_offsets: bool,
    hist: &mut [u32; 3],
) {
    // `move`: the flag rides in the closure by value. Borrowed, every
    // sequence read it through the closure and the reference it holds, one
    // more load behind the sink's own.
    driver.start_matching_with::<K>(move |seq| {
        record_builtin_sequence(seq, tail, sequences, code_offsets, hist)
    });
}

/// [`record_sequence`] for the built-in matcher, whose reports need no
/// checking: it reports the block's trailing literals last, and a block of at
/// most 128 KiB bounds every length far below 32 bits. Both hold by
/// construction of the driver and are asserted in debug builds; a custom
/// matcher's reports go through the checked path.
#[inline(always)]
fn record_builtin_sequence(
    seq: Sequence,
    tail: &mut usize,
    sequences: &mut RegionVec<RawSequence>,
    code_offsets: bool,
    hist: &mut [u32; 3],
) {
    match seq {
        Sequence::Literals { len } => *tail += len,
        Sequence::Triple {
            literal_len,
            offset,
            match_len,
        } => {
            debug_assert_eq!(*tail, 0, "literals reported before a sequence");
            debug_assert!((literal_len | match_len | offset) <= u32::MAX as usize);
            let off_base = if code_offsets {
                encode_offset_with_history_fast(offset as u32, literal_len as u32, hist)
            } else {
                offset as u32
            };
            // SAFETY: the buffer holds `max_sequences` of the largest block at
            // the frame's sequence floor (`LevelParams::sequence_floor`), and
            // every sequence of the built-in matcher covers at least that many
            // bytes of the block (checked after the search in debug builds), so
            // it cannot fill up.
            unsafe {
                sequences.push_within_capacity(RawSequence {
                    ll: literal_len as u32,
                    ml: match_len as u32,
                    off_base,
                })
            };
        }
    }
}

/// Append a block's parse to `capture` as external sequences with plain
/// offsets and their repeat-offset numbers, against the `history` the block
/// started from, closed by a delimiter carrying the `tail` literals. With
/// `code_offsets` the block's offsets are wire codes, otherwise plain.
#[cold]
fn record_block_parse(
    capture: &mut Vec<crate::encoding::sequences::ExternalSequence>,
    sequences: &[RawSequence],
    tail: usize,
    code_offsets: bool,
    mut history: [u32; 3],
) {
    use crate::encoding::sequences::ExternalSequence;
    for sequence in sequences {
        let (offset, code) = if code_offsets {
            let code = sequence.off_base;
            let offset = crate::encoding::sequences::plain_offset(code, sequence.ll, &mut history);
            (offset, code)
        } else {
            let offset = sequence.off_base;
            let code = encode_offset_with_history(offset, sequence.ll, &mut history);
            (offset, code)
        };
        capture.push(ExternalSequence {
            offset,
            lit_len: sequence.ll,
            match_len: sequence.ml,
            // Codes 1..=3 are the repeat offsets (RFC 8878 3.1.1.5).
            rep: if code <= 3 { code } else { 0 },
        });
    }
    // A block is at most 128 KiB.
    capture.push(ExternalSequence::delimiter(tail as u32));
}

/// Record one reported sequence. With `code_offsets` the offset is replaced by
/// its fast-band wire code against `hist`, which it advances; without it the
/// raw offset is kept for [`fill_and_count`] to code and `hist` is untouched.
#[inline(always)]
fn record_sequence(
    seq: Sequence,
    tail: &mut usize,
    sequences: &mut RegionVec<RawSequence>,
    code_offsets: bool,
    hist: &mut [u32; 3],
) {
    match seq {
        Sequence::Literals { len } => *tail += len,
        Sequence::Triple {
            literal_len,
            offset,
            match_len,
        } => {
            // A custom matcher's order is checked in every build: literals out
            // of order would be copied from the wrong place without an error.
            assert_eq!(*tail, 0, "literals reported before a sequence");
            // A custom matcher's report is checked before it is narrowed: a
            // length that lost its high bits could still fit the block and
            // pass the gather's bounds check. One compare for all three; it
            // folds away where `usize` is 32 bits.
            assert!(
                (literal_len | match_len | offset) <= u32::MAX as usize,
                "a sequence length exceeds 32 bits"
            );
            let off_base = if code_offsets {
                encode_offset_with_history_fast(offset as u32, literal_len as u32, hist)
            } else {
                offset as u32
            };
            sequences.push(RawSequence {
                ll: literal_len as u32,
                ml: match_len as u32,
                off_base,
            });
        }
    }
}

fn encode_block_parts<M: Matcher, K: EncodeTier>(
    state: &mut CompressState<M>,
    literals_vec: &[u8],
    raw_sequences: &mut [RawSequence],
    output: &mut Vec<u8>,
    // What the block decided for its tables, for the caller to commit once it
    // knows the block is kept.
) -> BlockTables {
    // A block with no sequences writes no tables, so every axis keeps what it
    // had whatever the caller decides.
    let mut decisions = [LastUsedTable::Keep; 3];

    // literals section

    let mut writer = BitWriter::from(output);
    // Upstream zstd `compress_literals` (`zstd_compress_literals.c:153-160`):
    // `srcSize < ZSTD_minLiteralsToCompress(strategy, prevHuf->repeatMode)`
    // → `ZSTD_noCompressLiterals` (raw). The threshold is strategy-aware
    // (see `min_literals_to_compress`). With huf reuse available the
    // floor drops to 6 since there is no per-block huf-header overhead.
    let strategy = state.strategy_tag;
    let min_lits = min_literals_to_compress(strategy, state.huf_repeat_valid);
    // RLE pre-check: upstream zstd `compress_literals` reaches RLE only through
    // the `cLitSize == 1` branch (`zstd_compress_literals.c:192-201`)
    // after passing the `min_lits` gate and running a full HUF compress —
    // so upstream zstd emits raw for any all-identical section under `min_lits`
    // (e.g. 8..63 bytes at fast/dfast/greedy/lazy without HUF reuse).
    // RLE and raw share the same lhSize for a given `len`
    // (both use `uncompressed_literals_header_bytes`), so RLE = lhSize + 1
    // and raw = lhSize + len. That makes RLE equal to raw on `len == 1`
    // and smaller by exactly `len - 1` bytes for `len >= 2`, regardless of
    // the lhSize tier (1 / 2 / 3 / 5 bytes). Our pre-check fires for ANY
    // all-identical literal slice regardless of strategy/min_lits.
    // This produces strictly smaller output than upstream zstd on the small
    // all-identical edges while still matching upstream zstd on `>= min_lits`
    // inputs (where upstream zstd's compress+`cLitSize==1` path reaches the same
    // RLE block).
    // Note the order — RLE pre-check runs BEFORE `min_lits`;
    // `estimate_literals_section_bytes` mirrors this exactly so probe
    // costs match emit byte-for-byte.
    //
    // This is the LITERALS-section RLE inside a compressed block, reached only
    // when the block already carries sequences. A block whose ENTIRE content
    // is one repeated byte never gets here: `compress_block_encoded` emits a
    // block-level RLE block (Block_Type 1) for it first. The remaining
    // small-input framing economy (single-segment header, store-raw fallback
    // for non-shrinking blocks) lives in `append_frame_header` /
    // `compress_block_encoded`, not in this literals path.
    // What the section does to the Huffman table is returned, not applied: the
    // table the block started from stays in place until the caller keeps the
    // block, so a block that loses to a raw one has nothing to put back. A raw
    // or RLE section leaves it in place for the next block to repeat, as
    // upstream copies `prevHuf` into `nextHuf` on those paths
    // (zstd_compress_literals.c:152, :189, :199).
    let huff = if state.literal_compression_disabled {
        // Upstream zstd `ZSTD_literalsCompressionIsDisabled` (auto mode:
        // `strategy == ZSTD_fast && targetLength > 0`, i.e. the negative levels):
        // emit RAW literals, skipping the Huffman pass entirely
        // (`ZSTD_noCompressLiterals`). Trades ratio for encode speed and matches
        // C's negative-band frames byte-for-byte (the literals section there is
        // Raw, not Compressed/RLE).
        raw_literals(literals_vec, &mut writer);
        HuffOutcome::Keep
    } else if !literals_vec.is_empty() && all_bytes_identical(literals_vec) {
        rle_literals(literals_vec, &mut writer);
        HuffOutcome::Keep
    } else if literals_vec.len() >= min_lits {
        match compress_literals::<K>(
            literals_vec,
            state.last_huff_table.as_ref(),
            state.huf_repeat_valid,
            &mut writer,
            strategy,
            state.huf_optimal_search,
            &mut state.huff_weights,
            literals_suspected_incompressible(literals_vec.len(), raw_sequences.len()),
        ) {
            Some(table) => HuffOutcome::New(table),
            None => HuffOutcome::Keep,
        }
    } else {
        raw_literals(literals_vec, &mut writer);
        HuffOutcome::Keep
    };

    // sequences section

    if raw_sequences.is_empty() {
        writer.write_bits(0u8, 8);
    } else {
        encode_seqnum(raw_sequences.len(), &mut writer);

        // Single-pass histogram of ll/ml/of codes across all sequences.
        // Previously did three separate `sequences.iter().map(...)`
        // passes; folded into one loop here saves the per-element
        // closure overhead (profile #220 round 3: `Map::fold` +
        // `call_mut` accounted for ~5% of total bench CPU).
        let mut ll_counts: SequenceHistogram = [0; SEQUENCE_CODE_SLOTS];
        let mut ml_counts: SequenceHistogram = [0; SEQUENCE_CODE_SLOTS];
        let mut of_counts: SequenceHistogram = [0; SEQUENCE_CODE_SLOTS];
        // The offset codes are derived in this same pass rather than by a
        // walk of their own ahead of the literals section: both passes read
        // every sequence, and the second one was reading back what the first
        // had just written.
        let counts = SequenceCodeCounts {
            ll: &mut ll_counts,
            ml: &mut ml_counts,
            of: &mut of_counts,
        };
        let (ll_max, ml_max, of_max) = if uses_fast_offset_codes(state.strategy_tag) {
            fill_and_count::<true>(raw_sequences, &mut state.offset_hist, counts)
        } else {
            fill_and_count::<false>(raw_sequences, &mut state.offset_hist, counts)
        };
        let raw_sequences: &[RawSequence] = raw_sequences;
        let total = raw_sequences.len();

        // Stream codes of the LAST sequence: upstream zstd codes the final symbol
        // of each stream via the FSE init-state and drops one occurrence of it
        // from the emitted table's histogram (see `build_seq_ctable`). `Some`
        // here because these modes are written to the frame.
        let (last_ll, last_ml, last_of) = raw_sequences.last().map_or((0, 0, 0), |seq| {
            (
                seq.ll_code() as usize,
                seq.ml_code() as usize,
                seq.of_code() as usize,
            )
        });

        // Destructured because the default-table accessors borrow the whole of
        // `FseTables`, which would collide with taking a `*_next` slot mutably.
        let FseTables {
            ll_previous,
            ml_previous,
            of_previous,
            ll_next,
            ml_next,
            of_next,
            ll_default,
            ml_default,
            of_default,
        } = &mut state.fse_tables;
        let ll_default: &FSETable = ll_default;
        let ml_default: &FSETable = ml_default;
        let of_default: &FSETable = of_default;

        let ll_mode = choose_table_from_counts(
            ll_previous.as_ref(),
            ll_default,
            &mut ll_counts,
            total,
            ll_max,
            9,
            state.strategy_tag,
            Some(last_ll),
            ll_next,
        );
        let ml_mode = choose_table_from_counts(
            ml_previous.as_ref(),
            ml_default,
            &mut ml_counts,
            total,
            ml_max,
            9,
            state.strategy_tag,
            Some(last_ml),
            ml_next,
        );
        let of_mode = choose_table_from_counts(
            of_previous.as_ref(),
            of_default,
            &mut of_counts,
            total,
            of_max,
            8,
            state.strategy_tag,
            Some(last_of),
            of_next,
        );

        writer.write_bits(encode_fse_table_modes(&ll_mode, &ml_mode, &of_mode), 8);

        encode_table(&ll_mode, &mut writer);
        encode_table(&of_mode, &mut writer);
        encode_table(&ml_mode, &mut writer);

        encode_sequences::<K>(
            raw_sequences,
            &mut writer,
            [&ll_mode, &ml_mode, &of_mode],
            [ll_default, ml_default, of_default],
        );

        // Consuming the modes ends their borrow of the slots. The decisions go
        // back to the caller rather than being applied here: a block that
        // loses to a raw one must leave the previous tables alone, and
        // upstream expresses that by not swapping (`out:` in
        // `ZSTD_compressBlock_internal`) rather than by undoing anything.
        decisions = [
            into_last_used_table(ll_mode),
            into_last_used_table(ml_mode),
            into_last_used_table(of_mode),
        ];
    }
    writer.flush();
    BlockTables {
        fse: decisions,
        huff,
    }
}

/// What a block decided for the tables the next block may repeat, held until
/// the caller knows whether the block is kept. Upstream builds into
/// `nextCBlock` and makes it the previous state only on confirmation
/// (`ZSTD_blockState_confirmRepcodesAndEntropyTables`); a block that goes out
/// raw is simply not confirmed, so nothing the block started from is ever
/// copied to be put back.
pub(crate) struct BlockTables {
    fse: [LastUsedTable; 3],
    huff: HuffOutcome,
}

/// Make a kept block's tables the ones the next block reads.
pub(crate) fn commit_block_tables<M: Matcher>(state: &mut CompressState<M>, tables: BlockTables) {
    remember_last_used_tables(&mut state.fse_tables, tables.fse);
    match tables.huff {
        HuffOutcome::Keep => {}
        HuffOutcome::New(table) => state.replace_huff_table(table),
    }
    state.fse_tables.end_block();
}

/// Drop a discarded block's tables, keeping a Huffman table it built as a
/// buffer for the next build; the FSE tables it built stay in their `*_next`
/// slots for the same reason.
pub(crate) fn discard_block_tables<M: Matcher>(state: &mut CompressState<M>, tables: BlockTables) {
    if let HuffOutcome::New(table) = tables.huff {
        state.park_huff_table(table);
    }
    state.fse_tables.end_block();
}

/// Workspace shared across estimator probes so per-probe cost computation never
/// allocates. Counts are zeroed at the top of every probe.
struct EstimatorWorkspace {
    lit_counts: Box<[usize; 256]>,
    ll_counts: Box<SequenceHistogram>,
    ml_counts: Box<SequenceHistogram>,
    of_counts: Box<SequenceHistogram>,
    /// FSE table handles no probe state holds any more, for the next probe to
    /// build into. Every one is unique, so a build writes it in place.
    spare_fse: Vec<SharedFseTable>,
    /// The arena a block's probes keep the Huffman tables they build in
    /// ([`SplitEstimator::built_huff`]), empty between blocks: the tables go back
    /// to the weight builder, the room stays for the next block's probes.
    built_huff: Vec<huff0_encoder::HuffmanTable>,
}

impl EstimatorWorkspace {
    /// The four boxed count tables, always present once the workspace exists,
    /// the pooled tables with their reference counts, and the room kept for
    /// the probes' Huffman tables.
    fn heap_size(&self) -> usize {
        core::mem::size_of::<[usize; 256]>()
            + 3 * core::mem::size_of::<SequenceHistogram>()
            + self.built_huff.capacity() * core::mem::size_of::<huff0_encoder::HuffmanTable>()
            + self.spare_fse.capacity() * core::mem::size_of::<SharedFseTable>()
            + self.spare_fse.len()
                * (core::mem::size_of::<FSETable>()
                    + crate::encoding::frame_compressor::shared_table_overhead())
    }

    /// Pool a handle, unless something else still holds it: a shared table
    /// cannot be built into, so it is only let go.
    fn recycle_fse(&mut self, mut handle: SharedFseTable) {
        if SharedFseTable::get_mut(&mut handle).is_some() {
            self.spare_fse.push(handle);
        }
    }

    /// Pool the custom tables of an axis state that is going away.
    fn recycle_previous(&mut self, previous: Option<PreviousFseTable>) {
        if let Some(PreviousFseTable::Custom(handle)) = previous {
            self.recycle_fse(handle);
        }
    }
}

impl Default for EstimatorWorkspace {
    fn default() -> Self {
        Self {
            lit_counts: Box::new([0; 256]),
            ll_counts: Box::new([0; SEQUENCE_CODE_SLOTS]),
            ml_counts: Box::new([0; SEQUENCE_CODE_SLOTS]),
            of_counts: Box::new([0; SEQUENCE_CODE_SLOTS]),
            spare_fse: Vec::new(),
            built_huff: Vec::new(),
        }
    }
}

/// Dry-run analog of [`encode_block_parts`]: mirrors the
/// real encoder's `compress_literals` and `choose_table` decisions byte-for-byte
/// (same `last_huff_table` lookup, same FSE mode selection, same
/// `remember_last_used_tables` mutation), and computes the would-be output size
/// in bytes via existing cost primitives instead of running the per-sequence
/// FSE bit-level write. Splitter probes use this path to get the same byte
/// count `encode_block_parts` would produce while saving the dominant
/// `encode_sequences` write cost on every probe.
#[cfg(test)]
fn estimate_block_parts_size<M: Matcher>(
    state: &mut CompressState<M>,
    literals_vec: &[u8],
    raw_sequences: &[RawSequence],
    workspace: &mut EstimatorWorkspace,
) -> usize {
    let mut sums = SequencePrefixSums::default();
    sums.rebuild(raw_sequences);
    let previous = state.last_huff_table.take();
    let tables = &mut state.fse_tables;
    let previous_fse = [
        tables.ll_previous.take(),
        tables.ml_previous.take(),
        tables.of_previous.take(),
    ];
    let previous_valid = state.huf_repeat_valid;
    let (bytes, outcome, decisions) = estimate_block_parts_size_with(
        state,
        previous.as_ref(),
        previous_valid,
        [
            previous_fse[0].as_ref(),
            previous_fse[1].as_ref(),
            previous_fse[2].as_ref(),
        ],
        literals_vec,
        raw_sequences,
        sums.codes(0, raw_sequences.len()),
        workspace,
    );
    state.last_huff_table = match outcome {
        HuffOutcome::Keep => previous,
        HuffOutcome::New(table) => {
            state.huf_repeat_valid = false;
            Some(table)
        }
    };
    let [ll, ml, of] = previous_fse;
    let tables = &mut state.fse_tables;
    tables.ll_previous = ll;
    tables.ml_previous = ml;
    tables.of_previous = of;
    remember_last_used_tables(tables, decisions);
    for next in [
        &mut tables.ll_next,
        &mut tables.ml_next,
        &mut tables.of_next,
    ] {
        if let Some(handle) = next.take() {
            workspace.recycle_fse(handle);
        }
    }
    bytes
}

/// What pricing a literals section did to the Huffman table the next section
/// may repeat: keep the one it was given (a repeat, a raw or an RLE section all
/// leave it in place, upstream's `nextHuf = prevHuf`), or take a new one. A
/// probe reads the previous table and reports this, rather than rewriting a
/// copy of it, so the table it starts from is borrowed.
enum HuffOutcome {
    Keep,
    New(huff0_encoder::HuffmanTable),
}

/// `estimate_block_parts_size` for sequences whose length codes are already
/// derived: the splitter's probes, which price many ranges of one block. The
/// tables the section may repeat are borrowed, the Huffman table as `previous`
/// and the FSE tables as `previous_fse`, and what the section does with each is
/// returned beside the size, a built FSE table in its `*_next` slot of
/// `state.fse_tables`; the offset history is advanced in `state`.
// One argument over the lint's threshold: each is a part of the state the
// section is priced from, mirroring what the emitter reads.
#[allow(clippy::too_many_arguments)]
fn estimate_block_parts_size_with<M: Matcher>(
    state: &mut CompressState<M>,
    previous: Option<&huff0_encoder::HuffmanTable>,
    previous_valid: bool,
    previous_fse: [Option<&PreviousFseTable>; 3],
    literals_vec: &[u8],
    raw_sequences: &[RawSequence],
    codes: LengthCodes<'_>,
    workspace: &mut EstimatorWorkspace,
) -> (usize, HuffOutcome, [LastUsedTable; 3]) {
    let (lit_bytes, outcome) = estimate_literals_section_bytes(
        literals_vec,
        previous,
        previous_valid,
        &mut workspace.lit_counts,
        state.strategy_tag,
        state.huf_optimal_search,
        state.literal_compression_disabled,
        &mut state.huff_weights,
        literals_suspected_incompressible(literals_vec.len(), raw_sequences.len()),
    );

    // A section without sequences writes no tables, so every axis keeps its own.
    let (seq_bytes, decisions) = if raw_sequences.is_empty() {
        (1, [LastUsedTable::Keep; 3])
    } else {
        estimate_sequences_section_bytes(
            raw_sequences,
            codes,
            &mut state.offset_hist,
            previous_fse,
            &mut state.fse_tables,
            workspace,
            state.strategy_tag,
        )
    };

    (lit_bytes + seq_bytes, outcome, decisions)
}

// One argument over the lint's threshold. Every one of them is a distinct
// decision the emitter makes about this section, and this function exists to
// reproduce those decisions in the same order; bundling them into a struct
// would hide exactly the correspondence that has to stay visible.
#[allow(clippy::too_many_arguments)]
fn estimate_literals_section_bytes(
    literals: &[u8],
    last_huff: Option<&huff0_encoder::HuffmanTable>,
    repeat_valid: bool,
    counts: &mut [usize; 256],
    strategy: crate::encoding::strategy::StrategyTag,
    huf_search: bool,
    lit_disabled: bool,
    weight_scratch: &mut huff0_encoder::WeightScratch,
    suspected_incompressible: bool,
) -> (usize, HuffOutcome) {
    let raw = || {
        (
            uncompressed_literals_header_bytes(literals.len()) + literals.len(),
            HuffOutcome::Keep,
        )
    };
    // Mirror `encode_block_parts` literal-mode branches
    // **in the same order**. The disabled gate (negative levels: raw literals,
    // no Huffman) is checked FIRST exactly as the emitter does.
    if lit_disabled {
        return raw();
    }
    // The emitter pre-checks `all_identical`
    // (any non-empty section) BEFORE the `min_lits` gate — RLE and raw
    // share `uncompressed_literals_header_bytes(len)` (1/2/3/5 bytes by
    // length tier), so on all-identical inputs RLE = lhSize + 1 equals
    // raw = lhSize + len at `len == 1` and is smaller by `len - 1` for
    // `len >= 2`. RLE is never worse than raw, so it is selected
    // regardless of strategy. Estimator must use the same ordering and
    // predicate so probe costs match emit byte-for-byte.
    if !literals.is_empty() && all_bytes_identical(literals) {
        return (
            uncompressed_literals_header_bytes(literals.len()) + 1,
            HuffOutcome::Keep,
        );
    }
    let min_lits = min_literals_to_compress(strategy, repeat_valid);
    if literals.len() < min_lits {
        return raw();
    }
    let single_stream = single_literal_stream(literals.len(), repeat_valid);
    // The section repeating the previous table, priced as the emitter writes
    // it: no description, raw when it does not save `minGain`.
    let repeat = |prev: &huff0_encoder::HuffmanTable| {
        let reuse_payload = estimate_huff_payload_bytes_checked(prev, literals, single_stream)
            .expect("a repeated table codes the section");
        if use_raw_literal_fallback(reuse_payload, literals.len(), strategy) {
            raw()
        } else {
            (
                compressed_literals_header_bytes(literals.len()) + reuse_payload,
                HuffOutcome::Keep,
            )
        }
    };

    // preferRepeat with a valid table, before any histogram, as the emitter.
    if prefer_repeat_eligible(strategy, literals.len())
        && repeat_valid
        && let Some(prev) = last_huff
    {
        return repeat(prev);
    }

    // Mirror the emitter's end-sample shortcut, in the same position. Without
    // it a section with flat ends and a biased interior costs as
    // Huffman-compressed here and is emitted raw there, and the splitter picks
    // a partition on a price the emitter cannot produce.
    if suspected_incompressible && end_samples_look_flat(literals) {
        return raw();
    }

    let (max_sym, largest_count) = crate::histogram::count_bytes(literals, counts);
    // Mirror `compress_literals`' upstream zstd pre-build incompressibility gate
    // byte-for-byte (flat histogram → raw section, no tree build) so
    // splitter probe costs match what the emitter writes.
    if largest_count <= (literals.len() >> 7) + 4 {
        return raw();
    }

    // preferRepeat with a table that has to be checked, as the emitter.
    if prefer_repeat_eligible(strategy, literals.len())
        && let Some(prev) = last_huff
        && prev
            .estimate_compressed_size_from_counts_checked(counts)
            .is_some()
    {
        return repeat(prev);
    }
    // Mutable because the size query is what encodes the weight description
    // into the table's own buffer, so the emitter that follows reads it.
    let mut new_table = huff0_encoder::HuffmanTable::build_from_counts_gated_with(
        &counts[..=max_sym],
        literals.len(),
        huf_search,
        weight_scratch,
    );

    let Some(new_desc) =
        new_table.writeable_table_description_size(weight_scratch.weight_fse_table())
    else {
        // Nothing downstream reads this table; hand its buffers to the next
        // build rather than dropping them.
        weight_scratch.recycle(new_table);
        return raw();
    };
    // Mirror `compress_literals` reuse-vs-new decision **byte-for-byte**.
    // The real encoder compares single-stream `estimate_compressed_size` for
    // both new and old tables (see `compress_literals` below); the actual
    // wire output is the stream layout `single_stream` picks once the table is
    // chosen.
    let use_new =
        match decide_huff_reuse_like_encoder(&new_table, last_huff, new_desc, literals, counts) {
            HuffChoice::Raw => {
                weight_scratch.recycle(new_table);
                return raw();
            }
            HuffChoice::New => true,
            HuffChoice::Repeat => false,
        };
    // The four-stream layout carries a 6-byte jumptable and per-stream padding,
    // which a single-stream estimate would undercount by ~6–10 bytes and bias
    // the splitter's probes; each quarter is estimated on its own instead.
    let payload: usize = if use_new {
        estimate_huff_payload_bytes(&new_table, literals, counts, single_stream)
    } else {
        last_huff
            .and_then(|t| estimate_huff_payload_bytes_checked(t, literals, single_stream))
            .unwrap_or(literals.len())
    };
    let tree_desc = if use_new { new_desc } else { 0 };
    let compressed_header = compressed_literals_header_bytes(literals.len());
    let total = compressed_header + tree_desc + payload;

    // Upstream zstd `compress_literals` raw-fallback gate
    // (`zstd_compress_literals.c:187-188`):
    //   `cLitSize >= srcSize - minGain`
    // where `cLitSize` is the encoded literals payload + tree description
    // (output of `HUF_compress*`, excluding the surrounding lhSize bytes)
    // and `srcSize` is the literal-payload length. In our terms:
    //   - upstream zstd `cLitSize` ≡ `total - compressed_header` (tree_desc + payload)
    //   - upstream zstd `srcSize`  ≡ `literals.len()`
    // Using the on-wire `total >= raw_section_bytes - mg` form (which
    // includes the compressed header on the LHS and the raw header on
    // the RHS) skews the threshold by `compressed_header - raw_header`
    // bytes and rejects compressed sections that upstream zstd would keep,
    // losing ratio. Mirror upstream zstd's payload-vs-srcSize form here.
    let huf_section_size = total - compressed_header; // tree_desc + payload, no lhSize
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
        weight_scratch.recycle(new_table);
        return raw();
    }

    if use_new {
        (total, HuffOutcome::New(new_table))
    } else {
        weight_scratch.recycle(new_table);
        (total, HuffOutcome::Keep)
    }
}

/// Price a sequence section. The offset codes are the one part that depends on
/// the history the section starts from, so they are resolved here, in one pass
/// that advances `offset_hist` as the emitter would; the length codes and
/// their extra bits come precomputed in `codes`. Each histogram is built once
/// and handed to the table selection, which the emitter's path counts again.
///
/// The tables the section may repeat are `previous` (LL, ML, OF), borrowed: a
/// probe reads them and does not take a handle. What the section decided for
/// each axis is returned beside the size, a built table left in that axis's
/// `*_next` slot of `fse_tables`, for the caller to commit.
fn estimate_sequences_section_bytes(
    sequences: &[RawSequence],
    codes: LengthCodes<'_>,
    offset_hist: &mut [u32; 3],
    previous: [Option<&PreviousFseTable>; 3],
    fse_tables: &mut FseTables,
    workspace: &mut EstimatorWorkspace,
    strategy: crate::encoding::strategy::StrategyTag,
) -> (usize, [LastUsedTable; 3]) {
    let EstimatorWorkspace {
        ll_counts,
        ml_counts,
        of_counts,
        ..
    } = workspace;
    let (ll_counts, ml_counts, of_counts) = (&mut **ll_counts, &mut **ml_counts, &mut **of_counts);
    debug_assert_eq!(codes.ll.len(), sequences.len());
    let histogram = |codes: &[u8], counts: &mut SequenceHistogram| {
        counts.fill(0);
        let mut max = 0u8;
        for &code in codes {
            debug_assert!(usize::from(code) < SEQUENCE_CODE_SLOTS);
            // SAFETY: a length code is at most 52, under the histogram's slots.
            unsafe { *counts.get_unchecked_mut(usize::from(code)) += 1 };
            max = max.max(code);
        }
        max as usize
    };
    let ll_max = histogram(codes.ll, ll_counts);
    let ml_max = histogram(codes.ml, ml_counts);
    of_counts.fill(0);
    let mut of_max = 0usize;
    let mut of_bits = 0usize;
    let mut hist = *offset_hist;
    let mut count_offset = |off_base: u32| {
        let (of, _, _) = encode_offset(off_base);
        debug_assert!(usize::from(of) < SEQUENCE_CODE_SLOTS);
        // SAFETY: an offset code is at most 31, under the histogram's slots.
        unsafe { *of_counts.get_unchecked_mut(usize::from(of)) += 1 };
        of_max = of_max.max(of as usize);
        // Upstream zstd: OF code's value equals its additional-bits width.
        of_bits += of as usize;
    };
    // Only post-split strategies are estimated, and the Fast band, whose
    // sequences arrive already coded, never post-splits.
    debug_assert!(!uses_fast_offset_codes(strategy));
    for seq in sequences {
        count_offset(encode_offset_with_history(seq.off_base, seq.ll, &mut hist));
    }
    *offset_hist = hist;
    let extra_bits = codes.bits + of_bits;
    let total = sequences.len();

    // Destructured for the same reason as the emitter: the default accessors
    // borrow the whole struct, which would collide with the `*_next` slots.
    let FseTables {
        ll_next,
        ml_next,
        of_next,
        ll_default,
        ml_default,
        of_default,
        ..
    } = fse_tables;
    let ll_default: &FSETable = ll_default;
    let ml_default: &FSETable = ml_default;
    let of_default: &FSETable = of_default;
    let [ll_previous, ml_previous, of_previous] = previous;

    // The table selection the real encoder makes, from the same histograms.
    let ll_mode = choose_table_from_counts(
        ll_previous,
        ll_default,
        ll_counts,
        total,
        ll_max,
        9,
        strategy,
        None,
        ll_next,
    );
    let ml_mode = choose_table_from_counts(
        ml_previous,
        ml_default,
        ml_counts,
        total,
        ml_max,
        9,
        strategy,
        None,
        ml_next,
    );
    let of_mode = choose_table_from_counts(
        of_previous,
        of_default,
        of_counts,
        total,
        of_max,
        8,
        strategy,
        None,
        of_next,
    );

    let ll_bits_chosen = fse_section_bits_for_mode(&ll_mode, ll_counts, ll_default);
    let ml_bits_chosen = fse_section_bits_for_mode(&ml_mode, ml_counts, ml_default);
    let of_bits_chosen = fse_section_bits_for_mode(&of_mode, of_counts, of_default);

    let ll_table_desc_bytes = mode_table_description_bytes(&ll_mode);
    let ml_table_desc_bytes = mode_table_description_bytes(&ml_mode);
    let of_table_desc_bytes = mode_table_description_bytes(&of_mode);

    // nbSeq varint header (upstream zstd RFC 8878 §3.1.1.3.2.1): 1–3 bytes.
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

    // What `encode_block_parts` would commit, left to the caller to commit.
    let decisions = [
        into_last_used_table(ll_mode),
        into_last_used_table(ml_mode),
        into_last_used_table(of_mode),
    ];

    (
        nb_seq_header
            + mode_byte
            + ll_table_desc_bytes
            + of_table_desc_bytes
            + ml_table_desc_bytes
            + stream_bytes,
        decisions,
    )
}

/// Bit cost of a sequence section under `mode`, matching what
/// `encode_sequences` would emit: FSE state transitions + final state flush.
fn fse_section_bits_for_mode(
    mode: &FseTableMode<'_>,
    counts: &SequenceHistogram,
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
/// Decision logic is byte-for-byte the upstream zstd's: the old-table cost is the
/// single-stream `estimate_compressed_size` (returns `None` when the prior
/// table lacks codes for a symbol present in the current literals — in which
/// case we must emit a new table). The new-table cost is its description
/// size plus the single-stream payload estimate. A small-input guard
/// (`new_desc + 12 >= literals.len()`) keeps the reuse path for tiny blocks
/// where the description alone would exceed the literals.
/// Upstream zstd `HUF_flags_preferRepeat` gate (`zstd_compress_literals.c:165`):
/// fast-band strategies (`strategy < ZSTD_lazy` → Fast / Dfast /
/// Greedy in our enum) with short literal sections (≤ 1024 bytes)
/// prefer reusing the previous tree over rebuilding it. Inside
/// upstream zstd's HUF_compress (`huf_compress.c:1360-1364, 1396-1400`),
/// the flag short-circuits the rebuild path when the prior table
/// is valid; we mirror it at our caller layer so the wasted
/// `HuffmanTable::build_from_data` work is also skipped on the
/// fast-band reuse path. Note this is an UNCONDITIONAL reuse
/// override — upstream zstd intentionally picks reuse even when a fresh
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

/// Which table a literals section is coded with once the new one is built
/// (upstream `HUF_compress_internal`, huf_compress.c:1415-1425).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HuffChoice {
    /// The previous table: it costs no more than the new one with its
    /// description, or the description alone eats the section.
    Repeat,
    /// The table just built.
    New,
    /// Neither: with no usable previous table, the description plus the
    /// 12-byte margin reaches the section size.
    Raw,
}

fn decide_huff_reuse_like_encoder(
    new_table: &huff0_encoder::HuffmanTable,
    last_table: Option<&huff0_encoder::HuffmanTable>,
    new_desc: usize,
    literals: &[u8],
    counts: &[usize; 256],
) -> HuffChoice {
    // `hSize + 12 >= srcSize`: the new table is not worth its description.
    let new_or_raw = if new_desc + 12 >= literals.len() {
        HuffChoice::Raw
    } else {
        HuffChoice::New
    };
    let Some(prev) = last_table else {
        return new_or_raw;
    };
    // Off the histogram, not the literals: the same sum over at most 256
    // symbols instead of over every byte of the section, which is where
    // upstream reads it from too (huf_compress.c:1416-1417). On a 4 KiB
    // dictionary frame the two per-literal walks this replaces were the
    // largest single item outside the matcher.
    //
    // Three arms in one session on the i9 (before, after, and the C reference
    // through `ffi_loop_dict`), `perf stat -r 3`, three rounds, 20 000 frames
    // of the corpus fixture with its dictionary — cycles / instructions / wall:
    //
    //             4 KiB frame                    10 KiB frame
    //   before    2.92-2.94 G / 8.035 G / 0.70 s  4.81-4.83 G / 12.837 G / 1.15 s
    //   after     2.27-2.31 G / 6.664 G / 0.54 s  3.41-3.46 G /  9.556 G / 0.82 s
    //   reference 1.35-1.37 G / 4.307 G / 0.32 s  2.53-2.54 G /  7.128 G / 0.61 s
    //
    // So 22% of the cycles and 17% of the instructions on the 4 KiB frame, 29%
    // and 26% on the 10 KiB one, taking this path from 2.16x of the reference
    // to 1.68x and from 1.90x to 1.36x. On input the literal stage writes off
    // as incompressible the decision never runs, and the change measures as
    // nothing there (instructions 4.8813 G against 4.8843 G) — as expected.
    // A previous table that cannot code this section is upstream's
    // `HUF_repeat_none` after `HUF_validateCTable`.
    let Some(old_estimate) = prev.estimate_compressed_size_from_counts_checked(counts) else {
        return new_or_raw;
    };
    let new_estimate = new_table
        .estimate_compressed_size_from_counts_checked(counts)
        .unwrap_or(literals.len());
    if old_estimate <= new_desc + new_estimate || new_desc + 12 >= literals.len() {
        HuffChoice::Repeat
    } else {
        HuffChoice::New
    }
}

/// Mirrors `compress_literals` choice: one stream ([`single_literal_stream`],
/// `encode`), else the 4-stream layout (`encode4x`) with a 6-byte jumptable and
/// per-stream byte-aligned padding. Returns the exact wire-format byte cost of
/// the Huffman-encoded payload, excluding the literals section header and the
/// Huffman tree description.
fn estimate_huff_payload_bytes(
    table: &huff0_encoder::HuffmanTable,
    literals: &[u8],
    counts: &[usize; 256],
    single_stream: bool,
) -> usize {
    if single_stream {
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
    single_stream: bool,
) -> Option<usize> {
    if single_stream {
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

/// Upstream zstd RFC 8878 §3.1.1.3.1.2 raw/RLE literals header size (bytes).
fn uncompressed_literals_header_bytes(lit_size: usize) -> usize {
    match lit_size {
        0..=31 => 1,
        32..=4095 => 2,
        _ => 3,
    }
}

/// Upstream zstd RFC 8878 §3.1.1.3.1.1 compressed literals section header size (bytes,
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
}

fn emit_single_sequence_block<M: Matcher, K: EncodeTier>(
    state: &mut CompressState<M>,
    last_block: bool,
    source_len: usize,
    literals: &[u8],
    sequences: &mut [RawSequence],
    buffers: &mut SingleSequenceEmitBuffers<'_>,
) -> bool {
    let saved_offset_hist = state.offset_hist;
    // The tables are only read here and the block's decisions come back to be
    // committed, so writing the partition raw leaves nothing to restore.
    buffers.compressed.clear();
    let tables = encode_block_parts::<M, K>(state, literals, sequences, buffers.compressed);
    if !keeps_compressed_block(buffers.compressed.len(), source_len, state.strategy_tag) {
        state.offset_hist = saved_offset_hist;
        discard_block_tables(state, tables);
        let header = BlockHeader {
            last_block,
            block_type: BlockType::Raw,
            block_size: source_len as u32,
        };
        header.serialize(buffers.output);
        true
    } else {
        // The block is kept, so its tables become what the next one reads.
        // Upstream's confirm step, at the same point in the decision.
        commit_block_tables(state, tables);
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

/// Slots in a sequence-code histogram: enough for the widest of the three
/// alphabets, the 53 match-length codes (RFC 8878 3.1.1.3.2.1.1). Upstream's
/// `HIST_countFast_wksp` likewise clears and counts only up to the last code;
/// a byte-wide histogram here zeroed 6 KiB a block for three streams that use
/// under 128 slots between them.
const SEQUENCE_CODE_SLOTS: usize = 64;

/// Occurrences of each code of one sequence stream.
type SequenceHistogram = [usize; SEQUENCE_CODE_SLOTS];

/// The three sequence-code histograms, passed as one argument so the pass that
/// fills them stays under the register-pressure of six.
struct SequenceCodeCounts<'a> {
    ll: &'a mut SequenceHistogram,
    ml: &'a mut SequenceHistogram,
    of: &'a mut SequenceHistogram,
}

/// Fill each sequence's wire offset code in place, advancing the repeat-offset
/// history across the run, and histogram the three code streams while the
/// sequence is in hand. Returns the highest code seen per stream, which the
/// table selector needs and would otherwise find by scanning ~200 always-zero
/// slots.
///
/// One pass, not two: deriving the offset codes and counting them both read
/// every sequence, and the counting pass was reading back what the filling pass
/// had just written. Upstream splits them (`ZSTD_seqToCodes` then
/// `HIST_countFast_wksp`) because its codes go to three separate byte arrays;
/// ours are already where they belong.
///
/// `OFFSETS_CODED` says the sequences already carry their wire codes: the Fast
/// band codes each offset as it is collected ([`collect_block_parts`]), and
/// never post-splits, so its block is one partition coded against the history
/// it started from. Greedy and above keep the raw offset until here and search
/// all three repeat offsets, which is what the full
/// `encode_offset_with_history` mirrors.
///
/// Per PARTITION, not per block: the emitter can write a partition raw, and
/// when it does it restores the history, so the partition after it must be
/// filled from the restored one. Filling here, just before each partition is
/// encoded, is what keeps that true.
fn fill_and_count<const OFFSETS_CODED: bool>(
    raw_sequences: &mut [RawSequence],
    offset_hist: &mut [u32; 3],
    counts: SequenceCodeCounts<'_>,
) -> (usize, usize, usize) {
    let SequenceCodeCounts {
        ll: ll_counts,
        ml: ml_counts,
        of: of_counts,
    } = counts;
    // The history is rotated by every sequence and read by the next one. Held
    // behind the caller's reference it was three stores into the compressor per
    // sequence, because the loop also writes through the sequence slice and the
    // optimiser would not keep the array in registers across that. A local copy
    // written back once is the same three words, moved once.
    let mut hist = *offset_hist;
    for seq in raw_sequences.iter_mut() {
        let off_base = if OFFSETS_CODED {
            seq.off_base
        } else {
            let off_base = encode_offset_with_history(seq.off_base, seq.ll, &mut hist);
            seq.off_base = off_base;
            off_base
        };
        let (ll_code, _, ll_bits) = encode_literal_length(seq.ll);
        let (ml_code, _, ml_bits) = encode_match_len(seq.ml);
        let (of_code, _, _) = encode_offset(off_base);
        seq.set_codes(ll_code, ll_bits, ml_code, ml_bits);
        debug_assert!(
            usize::from(ll_code.max(ml_code).max(of_code)) < SEQUENCE_CODE_SLOTS,
            "the code functions stay inside their alphabets",
        );
        // SAFETY: the codes are at most 35 (LL), 52 (ML) and 31 (OF), under the
        // histogram's `SEQUENCE_CODE_SLOTS`. The LL and ML bounds hold in release
        // because `encode_literal_length` and `encode_match_len` assert the
        // length below 2^17 before deriving a code from its high bit, so a larger
        // code panics there before reaching this write; the OF code is `ilog2`
        // of a `u32`. Those asserts must stay.
        unsafe {
            *ll_counts.get_unchecked_mut(usize::from(ll_code)) += 1;
            *ml_counts.get_unchecked_mut(usize::from(ml_code)) += 1;
            *of_counts.get_unchecked_mut(usize::from(of_code)) += 1;
        }
    }
    if !OFFSETS_CODED {
        *offset_hist = hist;
    }
    // A short slice finds its maxima in the codes just written, three compares
    // a sequence, rather than in three scans of up to 118 slots between them:
    // below 16 sequences the scans cost more than the codes they summarise.
    // That is the size of a block the post-split sends here with one to four
    // sequences, and of a small frame's only block; on the i9 those encodes
    // retire 0.3 to 0.5% fewer instructions for it, with the clock inside the
    // layout noise of the builds compared.
    const SHORT_SLICE_SEQUENCES: usize = 16;
    if raw_sequences.len() <= SHORT_SLICE_SEQUENCES {
        let (mut ll, mut ml, mut of) = (0u8, 0u8, 0u8);
        for seq in raw_sequences.iter() {
            ll = ll.max(seq.ll_code());
            ml = ml.max(seq.ml_code());
            of = of.max(seq.of_code());
        }
        return (ll as usize, ml as usize, of as usize);
    }
    use crate::blocks::sequence_section::{
        MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
    };
    (
        highest_used_code::<{ MAX_LITERAL_LENGTH_CODE as usize }>(ll_counts),
        highest_used_code::<{ MAX_MATCH_LENGTH_CODE as usize }>(ml_counts),
        highest_used_code::<{ MAX_OFFSET_CODE as usize }>(of_counts),
    )
}

/// The highest code with a non-zero count, which the table selector needs and
/// would otherwise find by scanning all 256 slots.
///
/// Carried as a running maximum through the counting loop until it was three
/// compares a sequence there against one bounded scan a stream a block. Each
/// scan starts at its own alphabet's last code (`MAX_CODE`: 35, 52 or 31),
/// where upstream's `HIST_count_simple` starts its own, so nothing past the
/// alphabet is read.
fn highest_used_code<const MAX_CODE: usize>(counts: &SequenceHistogram) -> usize {
    debug_assert!(
        counts[MAX_CODE + 1..].iter().all(|&count| count == 0),
        "a code past the alphabet's last ({MAX_CODE}) was counted",
    );
    counts[..=MAX_CODE]
        .iter()
        .rposition(|&count| count != 0)
        .unwrap_or(0)
}

/// The FSE tables a split probe's scratch state works with: the defaults, and
/// no previous table, since every probe reads the one it may repeat from the
/// state it starts from.
fn probe_fse_tables(fse_tables: &FseTables) -> FseTables {
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
        ll_previous: None,
        #[cfg(any(target_has_atomic = "ptr", feature = "critical-section"))]
        ml_default: fse_tables.ml_default,
        #[cfg(not(any(target_has_atomic = "ptr", feature = "critical-section")))]
        ml_default: fse_tables.ml_default.clone(),
        ml_previous: None,
        #[cfg(any(target_has_atomic = "ptr", feature = "critical-section"))]
        of_default: fse_tables.of_default,
        #[cfg(not(any(target_has_atomic = "ptr", feature = "critical-section")))]
        of_default: fse_tables.of_default.clone(),
        of_previous: None,
        // Empty, not blank tables: a probe gets its own slots so it cannot
        // overwrite what the emitter is describing, but most probes never
        // build a custom table and must not pay for one.
        ll_next: None,
        ml_next: None,
        of_next: None,
    }
}

/// Snapshot of the Huffman/FSE/repeat-offset state the real encoder would
/// have at a given partition boundary. Cloning is the only way to thread
/// state through recursive bisect probes (each branch needs its own copy),
/// but the snapshot is small relative to the full encode cost the dry-run
/// estimator replaces.
#[derive(Clone)]
struct ProbeEntryState {
    huff: HuffRef,
    ll_previous: Option<PreviousFseTable>,
    ml_previous: Option<PreviousFseTable>,
    of_previous: Option<PreviousFseTable>,
    offset_hist: [u32; 3],
}

/// Which Huffman table a probe state may repeat. Tables are never copied into
/// a state: the block's entry table is borrowed from the compressor, and a
/// table a probe builds is kept once in the estimator's arena.
#[derive(Clone, Copy)]
enum HuffRef {
    None,
    BlockEntry,
    Built(usize),
}

struct SplitEstimator<'a> {
    parts: &'a EncodedBlockParts,
    prefix_sums: &'a SequencePrefixSums,
    block_entry: ProbeEntryState,
    /// The table the block starts from, borrowed.
    entry_huff: Option<&'a huff0_encoder::HuffmanTable>,
    /// Whether `entry_huff` is valid (a dictionary's, untouched): only that
    /// table is, a probe's own builds never are.
    entry_huff_valid: bool,
    /// Every table a probe built, addressed by [`HuffRef::Built`].
    built_huff: Vec<huff0_encoder::HuffmanTable>,
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
        // Every table the probe may repeat is read where the entry state keeps
        // it; a handle is taken only for what the post state keeps. A build slot
        // comes from the pool, so a probe that builds a table writes into one it
        // already has.
        let tables = &mut self.scratch_state.fse_tables;
        for next in [
            &mut tables.ll_next,
            &mut tables.ml_next,
            &mut tables.of_next,
        ] {
            if next.is_none() {
                *next = self.workspace.spare_fse.pop();
            }
        }
        self.scratch_state.offset_hist = entry.offset_hist;
        let (previous, previous_valid) = match entry.huff {
            HuffRef::None => (None, false),
            HuffRef::BlockEntry => (self.entry_huff, self.entry_huff_valid),
            HuffRef::Built(at) => (Some(&self.built_huff[at]), false),
        };
        let (emitted_payload, outcome, decisions) = estimate_block_parts_size_with(
            &mut self.scratch_state,
            previous,
            previous_valid,
            [
                entry.ll_previous.as_ref(),
                entry.ml_previous.as_ref(),
                entry.of_previous.as_ref(),
            ],
            &self.parts.literals[lit_start..lit_end],
            &self.parts.sequences[start_idx..end_idx],
            self.prefix_sums.codes(start_idx, end_idx),
            &mut self.workspace,
        );
        let source_len = (lit_end - lit_start) + match_len;
        let raw_fallback =
            !keeps_compressed_block(emitted_payload, source_len, self.scratch_state.strategy_tag);
        let cost = if raw_fallback {
            source_len
        } else {
            emitted_payload
        } + 3;
        // Real emit on raw fallback restores the entry state — see
        // `emit_single_sequence_block`'s saved-state restore branch. A table the
        // probe built stays in its slot for the next probe to build into.
        let post = if raw_fallback {
            if let HuffOutcome::New(table) = outcome {
                self.scratch_state.huff_weights.recycle(table);
            }
            entry.clone()
        } else {
            let huff = match outcome {
                HuffOutcome::Keep => entry.huff,
                HuffOutcome::New(table) => {
                    self.built_huff.push(table);
                    HuffRef::Built(self.built_huff.len() - 1)
                }
            };
            let tables = &mut self.scratch_state.fse_tables;
            let [ll, ml, of] = decisions;
            ProbeEntryState {
                huff,
                ll_previous: decided_previous(ll, &entry.ll_previous, &mut tables.ll_next),
                ml_previous: decided_previous(ml, &entry.ml_previous, &mut tables.ml_next),
                of_previous: decided_previous(of, &entry.of_previous, &mut tables.of_next),
                offset_hist: self.scratch_state.offset_hist,
            }
        };
        (cost, raw_fallback, post)
    }

    /// Pool the tables of a probe state nothing will read again.
    fn recycle_state(&mut self, state: ProbeEntryState) {
        self.workspace.recycle_previous(state.ll_previous);
        self.workspace.recycle_previous(state.ml_previous);
        self.workspace.recycle_previous(state.of_previous);
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
        let (full, full_raw_fallback, full_post) =
            self.estimate_subblock_size(start_idx, end_idx, &entry);
        self.recycle_state(full_post);
        // G3 — whole-block bail-out before partition split. Upstream zstd
        // `ZSTD_compressSubBlock_multi` (`zstd_compress_superblock.c:530-532`)
        // bails when `estBlockSize > srcSize` (strict). Our trigger is
        // the `raw_fallback` flag from `estimate_subblock_size`, which
        // fires on the **stricter** [`keeps_compressed_block`] condition
        // (`emitted_payload >= source_len - minGain`, upstream's per-strategy
        // block margin). So we bail in a narrow band
        // `[source_len - min_gain, source_len + 3]` where upstream zstd would
        // still recurse and *might* find a compressible split.
        //
        // Why this is safe ratio-wise:
        // - The bail-out routes to `compress_block_with_post_split`'s
        //   single-partition path → `emit_single_sequence_block`,
        //   which applies the SAME [`keeps_compressed_block`] fallback
        //   right before deciding raw-fallback. So for the
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
        //   upstream zstd band gives at most `min_gain` bytes of theoretical
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
        let exit = self.derive_block_splits_with_full(start_idx, end_idx, full, entry, partitions);
        self.recycle_state(exit);
    }

    /// Returns the post-emit state at `end_idx` produced by whichever
    /// partitioning the recursion settles on (single emit OR multiple
    /// nested splits). Callers thread this into the sibling probe so the
    /// right-hand recursion sees the actual upstream zstd-parity state the real
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
            self.recycle_state(entry);
            return post;
        }
        let mid_idx = (start_idx + end_idx) / 2;
        let (first, _, first_post) = self.estimate_subblock_size(start_idx, mid_idx, &entry);
        // Upstream zstd parity: score the right half from the left's post-state,
        // not from the parent's block-entry state. Without this propagation
        // `second` is evaluated as a fresh-block start, biasing the
        // `first + second < full` decision toward overly optimistic splits.
        let (second, _, second_post) = self.estimate_subblock_size(mid_idx, end_idx, &first_post);
        self.recycle_state(second_post);
        self.recycle_state(first_post);
        if first + second < full {
            // If the left side gets further split, the true state at
            // `mid_idx` is the left subtree's exit state, not `first_post`.
            // Thread the returned state into the right recursion so the
            // right subtree probes against actual upstream zstd-parity state.
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
        self.recycle_state(entry);
        post
    }
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
/// What a block decided to do for one FSE axis, mirroring upstream's
/// `ZSTD_symbolEncodingType_e`.
///
/// `Encoded` borrows the table rather than owning it. The table itself is built
/// into the axis's `*_next` slot, the way upstream builds its CTable straight
/// into the block state, so this enum stays two words instead of carrying
/// eleven kilobytes through every call that passes a mode along.
enum FseTableMode<'a> {
    Predefined(&'a FSETable),
    Encoded(&'a FSETable),
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

fn entropy_cost(counts: &SequenceHistogram, max_symbol: usize, total: usize) -> usize {
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

fn cross_entropy_cost(
    counts: &SequenceHistogram,
    max_symbol: usize,
    table: &FSETable,
) -> Option<usize> {
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

fn fse_bit_cost(counts: &SequenceHistogram, max_symbol: usize, table: &FSETable) -> Option<usize> {
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

/// Choose an FSE table mode from a stream's symbol counts, which every caller
/// has already built while visiting the codes once.
// The eight inputs are the cohesive FSE-table-selection set, each carrying its
// own perf / correctness rationale below (the `&mut` histogram for the no-copy
// emit build, the caller-tracked `max_symbol` / `last_code` that avoid a
// per-stream rescan). Bundling them into a struct would only relocate that
// documented rationale away from the signature without removing any input.
#[expect(
    clippy::too_many_arguments,
    reason = "cohesive FSE-selection inputs, each justified inline"
)]
fn choose_table_from_counts<'a>(
    previous: Option<&'a PreviousFseTable>,
    default_table: &'a FSETable,
    // `&mut` only so the emitted-table build can borrow the histogram, drop the
    // last symbol in place for its normalize, and restore it (see
    // `build_seq_ctable`) — no per-table copy. Every selection-time read
    // re-borrows it immutably; the value is unchanged on return.
    counts: &mut SequenceHistogram,
    total: usize,
    // The highest symbol code with a non-zero count, tracked by the caller as it
    // builds `counts`. The sequence-code alphabets are tiny (LL <= 35, ML <= 52,
    // OF <= 31) while `counts` is a fixed 256-wide array, so deriving this here
    // via `counts.iter().rposition(..)` would scan ~200 always-zero high slots
    // per stream per block — the dominant cost on small frames (profiled ~35% of
    // a 1 KiB dict-frame encode). The caller already visits every code once, so
    // it carries the running max for free. Equal to the old `rposition` result
    // (every counted code increments its slot, so the max code IS the highest
    // non-zero index), keeping the table selection byte-identical.
    max_symbol: usize,
    max_log: u8,
    strategy: crate::encoding::strategy::StrategyTag,
    // The stream code of the LAST sequence, when this call emits a table that
    // will be written to the frame (`Some`). Upstream zstd drops one occurrence
    // of that code from the histogram before normalizing the emitted custom
    // table (it is coded via the FSE init-state, not a transition). `None` for
    // the cost-estimator call sites, which — like upstream's `ZSTD_NCountCost` —
    // price the unadjusted histogram.
    last_code: Option<usize>,
    // Where a chosen custom table is built. Borrowed for the returned mode's
    // lifetime, so the slot stays put while the block writes the table it
    // describes.
    next: &'a mut Option<SharedFseTable>,
) -> FseTableMode<'a> {
    if total == 0 {
        return FseTableMode::Predefined(default_table);
    }

    // Distinctness over the live alphabet only (`..=max_symbol`); slots above
    // `max_symbol` are zero by construction, so bounding the scan there matches
    // the full-array result without touching the always-zero tail.
    let distinct_symbols = counts[..=max_symbol]
        .iter()
        .filter(|&&count| count > 0)
        .take(2)
        .count();
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

    // Below lazy upstream prices nothing (`ZSTD_selectEncodingType`,
    // zstd_compress_sequences.c:179-204): the default table when the block has
    // few sequences or no dominant code, a new table otherwise. Pricing all
    // three normalizes the counts a second time on every block.
    let cheap_strategy_rank = match strategy {
        crate::encoding::strategy::StrategyTag::Fast => Some(1usize),
        crate::encoding::strategy::StrategyTag::Dfast => Some(2),
        crate::encoding::strategy::StrategyTag::Greedy => Some(3),
        _ => None,
    };
    if let Some(rank) = cheap_strategy_rank {
        // `isDefaultAllowed`: the predefined table codes this stream's symbols.
        if default_table.symbol_probability(max_symbol as u8) != 0 {
            // A VALID previous table and fewer than 1000 sequences: repeat it
            // unpriced (zstd_compress_sequences.c:187-191). Valid is a
            // dictionary's table covering every symbol its stream can carry
            // (`FSE_repeat_valid`); a table a block built is only `check`,
            // which this band never repeats. The coverage holds by
            // construction, and the cost probe checks it anyway: a repeat can
            // never produce an invalid stream.
            if total < 1000
                && let Some(prev) = previous
                && prev.is_valid()
                && let Some(table) = prev.as_table(default_table)
                && fse_bit_cost(counts, max_symbol, table).is_some()
            {
                return FseTableMode::RepeatLast(prev);
            }
            let default_log = default_table.acc_log() as usize;
            let min_for_new_table = ((1usize << default_log) * (10 - rank)) >> 3;
            let most_frequent = counts[..=max_symbol].iter().copied().max().unwrap_or(0);
            if total < min_for_new_table || most_frequent < (total >> (default_log - 1)) {
                return FseTableMode::Predefined(default_table);
            }
        }
        build_into_slot(next, |dest| match last_code {
            Some(lc) => build_seq_ctable_into(&mut counts[..=max_symbol], max_log, lc, dest),
            None => build_table_from_symbol_counts_into(
                &counts[..=max_symbol],
                max_log,
                use_low_prob_count,
                dest,
            ),
        });
        return FseTableMode::Encoded(slot_table(next));
    }

    // Mirror upstream zstd `ZSTD_selectEncodingType()`: compare default
    // cross-entropy, repeat-table FSE bit cost, and the custom compressed
    // table (header + entropy-bound payload). The custom table's header is
    // priced from its normalized counts via `fse_header_bits_for_counts`
    // WITHOUT building the (often-discarded) state tables — the build only
    // runs in the `Choice::New` arm below, when the custom table actually
    // wins. The estimate equals the built table's `table_header_bits()`
    // exactly, so the selection is byte-identical.
    // A single-symbol stream returned above, so the histogram has the two
    // symbols a custom table needs. Plain `+`: both are bit-cost estimates
    // bounded by the block size (<= MAX_BLOCK_SIZE * 8 bits), far under the
    // integer's range.
    debug_assert!(distinct_symbols > 1);
    let compressed =
        fse_header_bits_for_counts(&counts[..=max_symbol], max_log, use_low_prob_count)
            + entropy_cost(counts, max_symbol, total);

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

    // Upstream's order of preference on ties (zstd_compress_sequences.c:219-230):
    // the predefined table when it costs no more than either other, then the
    // previous table when it costs no more than a new one, else a new one. An
    // unavailable option costs nothing it could win with.
    let unavailable = usize::MAX;
    let basic = predefined_cost.unwrap_or(unavailable);
    let repeat = previous_cost.unwrap_or(unavailable);
    let best = if predefined_cost.is_some() && basic <= repeat && basic <= compressed {
        Choice::Predefined
    } else if previous_cost.is_some() && repeat <= compressed {
        Choice::Previous
    } else {
        Choice::New
    };

    match best {
        Choice::Previous => previous
            .map(FseTableMode::RepeatLast)
            .unwrap_or(FseTableMode::Predefined(default_table)),
        Choice::Predefined => FseTableMode::Predefined(default_table),
        // The custom table won the cost comparison, so build it now (the only
        // place the state tables are constructed), into the slot it will
        // occupy rather than on the stack.
        Choice::New => {
            build_into_slot(next, |dest| match last_code {
                Some(lc) => build_seq_ctable_into(&mut counts[..=max_symbol], max_log, lc, dest),
                None => build_table_from_symbol_counts_into(
                    &counts[..=max_symbol],
                    max_log,
                    use_low_prob_count,
                    dest,
                ),
            });
            FseTableMode::Encoded(slot_table(next))
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
    [ll_last, ml_last, of_last]: [LastUsedTable; 3],
) {
    commit_last_used_table(
        &mut fse_tables.ll_previous,
        &mut fse_tables.ll_next,
        ll_last,
    );
    commit_last_used_table(
        &mut fse_tables.ml_previous,
        &mut fse_tables.ml_next,
        ml_last,
    );
    commit_last_used_table(
        &mut fse_tables.of_previous,
        &mut fse_tables.of_next,
        of_last,
    );
}

#[cfg(test)]
fn previous_table<'a>(
    previous: Option<&'a PreviousFseTable>,
    default: &'a FSETable,
) -> Option<&'a FSETable> {
    previous.and_then(|previous| previous.as_table(default))
}

/// The slot a block builds its table into, made writable.
///
/// Normally nothing else holds the handle parked here, so it is written in
/// place. A dictionary-seeded frame shares its tables with the entropy cache,
/// and one of those can end up parked here; a shared handle cannot be written
/// under its other holder, so the slot takes a fresh one and leaves it alone.
fn build_into_slot(slot: &mut Option<SharedFseTable>, build: impl FnOnce(&mut FSETable)) {
    // Reuse the handle parked here when nothing else holds it. Otherwise build
    // the table on the stack and move it into a handle, which is what the
    // allocate-per-block path did: taking a handle to a zeroed table and then
    // filling it writes every page twice and, on a compressor that does not
    // outlive its frame, faults them all in.
    if let Some(dest) = slot.as_mut().and_then(SharedFseTable::get_mut) {
        build(dest);
        return;
    }
    let mut built = FSETable::blank();
    build(&mut built);
    *slot = Some(SharedFseTable::new(built));
}

/// The table an axis just built, for the mode to borrow.
fn slot_table(slot: &Option<SharedFseTable>) -> &FSETable {
    slot.as_deref()
        .expect("a build always leaves the slot filled")
}

/// What a block decided for one axis, once its mode is no longer borrowing the
/// slot the table was built into.
#[derive(Clone, Copy, Debug)]
enum LastUsedTable {
    /// The axis repeated what was already there, so the slot keeps it.
    Keep,
    Default,
    Rle(u8),
    /// The table is in the axis's `*_next` slot, waiting to be committed.
    Encoded,
}

/// The table an axis repeats after a probe decided `decision` from `entry`:
/// what [`commit_last_used_table`] would leave in the previous slot, taken for
/// a probe state. Only a repeat takes a handle to the entry's table; a built
/// table moves out of `next`, which the next probe refills from the pool.
fn decided_previous(
    decision: LastUsedTable,
    entry: &Option<PreviousFseTable>,
    next: &mut Option<SharedFseTable>,
) -> Option<PreviousFseTable> {
    match decision {
        LastUsedTable::Keep => entry.clone(),
        LastUsedTable::Default => Some(PreviousFseTable::Default),
        LastUsedTable::Rle(symbol) => Some(PreviousFseTable::Rle(symbol)),
        LastUsedTable::Encoded => Some(PreviousFseTable::Custom(
            next.take()
                .expect("an encoded axis built its table into the slot"),
        )),
    }
}

fn into_last_used_table(mode: FseTableMode<'_>) -> LastUsedTable {
    match mode {
        FseTableMode::Encoded(_) => LastUsedTable::Encoded,
        FseTableMode::Predefined(_) => LastUsedTable::Default,
        FseTableMode::Rle(symbol) => LastUsedTable::Rle(symbol),
        FseTableMode::RepeatLast(_) => LastUsedTable::Keep,
    }
}

/// Make the block's decision for one axis the state the next block reads,
/// upstream's `ZSTD_blockState_confirmRepcodesAndEntropyTables` for one stream.
///
/// For a built table this is a handle swap: what the block wrote into the slot
/// becomes the previous table, and the handle the previous table was using
/// becomes the slot for the next block. Two handles per axis, alternating, so
/// after the first block no axis allocates again.
/// Keep a table handle a commit displaced, if it was one and the slot is free.
///
/// The slot already holding a handle wins: that one is the buffer the block
/// just built into, and it is the fresher of the two.
fn park_displaced_handle(displaced: Option<PreviousFseTable>, next: &mut Option<SharedFseTable>) {
    if next.is_none()
        && let Some(PreviousFseTable::Custom(handle) | PreviousFseTable::Dictionary(handle)) =
            displaced
    {
        *next = Some(handle);
    }
}

fn commit_last_used_table(
    previous: &mut Option<PreviousFseTable>,
    next: &mut Option<SharedFseTable>,
    decision: LastUsedTable,
) {
    match decision {
        LastUsedTable::Keep => {}
        // These two do not swap, so the handle the previous table was using
        // would go out with the assignment. Park it: a distribution that moves
        // between custom and predefined from block to block would otherwise
        // build into a freshly allocated table every time it came back, which
        // is the per-block allocation the two slots exist to remove.
        LastUsedTable::Default => {
            park_displaced_handle(previous.replace(PreviousFseTable::Default), next);
        }
        LastUsedTable::Rle(symbol) => {
            park_displaced_handle(previous.replace(PreviousFseTable::Rle(symbol)), next);
        }
        LastUsedTable::Encoded => {
            // Take the outgoing handle out first, so the swap needs no third
            // one; the slot is left empty when there was none, and the next
            // build fills it rather than a blank being made here for a block
            // that may not need one.
            let outgoing = match previous.take() {
                Some(PreviousFseTable::Custom(old) | PreviousFseTable::Dictionary(old)) => {
                    Some(old)
                }
                _ => None,
            };
            let built = core::mem::replace(next, outgoing)
                .expect("Encoded means the slot holds the table just built");
            *previous = Some(PreviousFseTable::Custom(built));
        }
    }
}

/// The sequence section's bitstream, shared by [`encode_sequences_scalar`] and
/// `encode_sequences_bmi2`. A macro so each tier's loop is compiled inside its
/// own function and the bit writer and the FSE lookups inline into it.
macro_rules! encode_sequences_body {
    ($sequences:expr, $writer:expr, $modes:expr, $defaults:expr) => {{
        let sequences: &[RawSequence] = $sequences;
        let writer: &mut BitWriter<&mut Vec<u8>> = $writer;
        let [ll_mode, ml_mode, of_mode]: [&FseTableMode<'_>; 3] = $modes;
        let [ll_default, ml_default, of_default]: [&FSETable; 3] = $defaults;

        let sequence = sequences[sequences.len() - 1];
        let (ll_code, ll_num_bits) = (sequence.ll_code(), sequence.ll_bits());
        let (ml_code, ml_num_bits) = (sequence.ml_code(), sequence.ml_bits());
        let of_code = sequence.of_code();
        let of_num_bits = of_code as usize;
        let ll_add_bits = low_bits(sequence.ll, ll_num_bits);
        let ml_add_bits = low_bits(sequence.ml - 3, ml_num_bits);
        let of_add_bits = low_bits(sequence.off_base, of_num_bits);
        let ll_table = ll_mode.as_table(ll_default);
        let ml_table = ml_mode.as_table(ml_default);
        let of_table = of_mode.as_table(of_default);
        // Carried as bare indices, not `Option`s. A component has a state
        // exactly when it has a table, and the tables are resolved once above
        // the loop, so wrapping the state too meant re-asking the same question
        // three times per sequence and re-wrapping the answer. An absent
        // component's index is never read: every site that touches one is
        // already inside its table's `Some`.
        let mut ll_state = ll_table.map_or(0, |table| table.start_state(ll_code).index);
        let mut ml_state = ml_table.map_or(0, |table| table.start_state(ml_code).index);
        let mut of_state = of_table.map_or(0, |table| table.start_state(of_code).index);

        writer.write_bits(ll_add_bits, ll_num_bits);
        writer.write_bits(ml_add_bits, ml_num_bits);
        writer.write_bits(of_add_bits, of_num_bits);

        // Upstream zstd-faithful sequence loop: write state diffs + extras via
        // unchecked adds with explicit flushes at safe boundaries, through a
        // burst that keeps the accumulator and the cursor in registers.
        // Per-sequence bit budget:
        //   state diffs: of (<=8) + ml (<=9) + ll (<=9) = 26 bits.
        //   extras:      ll (<=16) + ml (<=16) + of (<=31).
        //
        // One flush per sequence is the common case: the two inside the body
        // are conditional on the extras not fitting beside the diffs, which is
        // upstream's own accounting in `ZSTD_encodeSequences_body`
        // (`zstd_compress_sequences.c:303-367`), and the one at the end of the
        // body is what leaves the accumulator with under a byte for the next
        // round.
        if sequences.len() > 1 {
            // Streams 0 / 1 / 2 are LL / ML / OF; built only for a block that
            // runs the loop. A local on purpose, not scratch kept on the
            // compressor: addressed off the stack pointer it needs no base
            // register, and the loop keeps two of the three states in
            // registers. Held behind a pointer the same loop spilled one state
            // more, +0.4..0.6% instructions at level 1 against the per-block
            // copy this costs.
            let mut view = core::mem::MaybeUninit::uninit();
            let view = crate::fse::fse_encoder::SequenceStateTables::init(
                &mut view,
                [ll_table, ml_table, of_table],
            );
            // At most 63 bits a sequence leave as whole bytes, under 8 bytes;
            // 12 a sequence and 64 over covers that and the up to 7 bytes the
            // burst drains from the init-state writes above as it starts,
            // which also leaves it holding under 8 bits.
            let mut burst = writer.burst(sequences.len() * 12 + 64);
            // Walked as a slice rather than by index: the same order, without
            // the bounds check and the index arithmetic that `sequences[i]`
            // pays on every sequence. The last one is coded through the FSE
            // init states above, so it is not in this range.
            for &sequence in sequences[..sequences.len() - 1].iter().rev() {
                // The extra bits below are the low bits of `ll` and `ml - 3`;
                // the codes above them are never under the mask.
                let (ll_code, ll_num_bits) = (sequence.ll_code(), sequence.ll_bits());
                let (ml_code, ml_num_bits) = (sequence.ml_code(), sequence.ml_bits());
                let of_code = sequence.of_code();
                let of_num_bits = of_code as usize;
                let ll_add_bits = low_bits(sequence.ll, ll_num_bits);
                let ml_add_bits = low_bits(sequence.ml - 3, ml_num_bits);
                let of_add_bits = low_bits(sequence.off_base, of_num_bits);

                // State diffs burst: max 30 bits (10+10+9 worst case for
                // acc_log ≤ 9 ll/ml + acc_log ≤ 8 of) + ≤ 7 leftover from
                // prior flush = ≤ 37 bits total — well under 64.
                //
                // SAFETY (for every `burst.add` below):
                // - the prior flush left `bits_in_partial ≤ 7`;
                // - each FSE state diff has `next.num_bits ≤ acc_log ≤ 10`;
                //   three diffs back-to-back add ≤ 30 bits → total ≤ 37,
                //   well below the 64-bit accumulator cap.
                // - `diff = state.index - next.baseline` cannot exceed
                //   `(1 << num_bits) - 1`, so `diff >> num_bits == 0`.
                // And for every `burst.flush`: the burst was started with room
                // for 12 bytes a sequence, more than one sequence's flushes
                // commit.
                //
                // What the three diffs actually put in the accumulator, tallied
                // so the budget below is checked against the widths the FSE
                // tables produced rather than against the ceiling its
                // derivation assumed.
                #[cfg(debug_assertions)]
                let mut state_diff_bits = 0usize;
                {
                    let next = view.next_state::<2>(of_code, of_state);
                    let diff = crate::fse::fse_encoder::transition_bits(of_state, next.num_bits);
                    unsafe {
                        burst.add(diff as u64, next.num_bits as usize);
                    }
                    #[cfg(debug_assertions)]
                    {
                        state_diff_bits += next.num_bits as usize;
                    }
                    of_state = next.index;
                }
                {
                    let next = view.next_state::<1>(ml_code, ml_state);
                    let diff = crate::fse::fse_encoder::transition_bits(ml_state, next.num_bits);
                    unsafe {
                        burst.add(diff as u64, next.num_bits as usize);
                    }
                    #[cfg(debug_assertions)]
                    {
                        state_diff_bits += next.num_bits as usize;
                    }
                    ml_state = next.index;
                }
                {
                    let next = view.next_state::<0>(ll_code, ll_state);
                    let diff = crate::fse::fse_encoder::transition_bits(ll_state, next.num_bits);
                    unsafe {
                        burst.add(diff as u64, next.num_bits as usize);
                    }
                    #[cfg(debug_assertions)]
                    {
                        state_diff_bits += next.num_bits as usize;
                    }
                    ll_state = next.index;
                }
                // The three state diffs and the three extra-bit fields share
                // one 64-bit accumulator, and a flush here is only needed when
                // the extras that follow would not fit beside them. Upstream
                // asks exactly that question (`zstd_compress_sequences.c:350`):
                // `ofBits + mlBits + llBits >= 64 - 7 - (LLFSELog + MLFSELog +
                // OffFSELog)`, which with our accumulator logs (9 + 9 + 8 = 26)
                // is 31. Below it the accumulator holds at most 7 leftover + 26
                // diff bits + 30 extra bits = 63, so nothing has to leave yet.
                // Flushing unconditionally instead cost a second store, length
                // commit and shift on every sequence, and a level-1 block has
                // hundreds of thousands of them.
                let extra_bits_total = of_num_bits + ml_num_bits + ll_num_bits;
                // The thresholds are arithmetic on widths, so they are only
                // right while the widths are what they were derived from.
                // Pinned here, where the arithmetic happens, so a widened
                // encoder fails at its cause rather than as an accumulator
                // overflow further down or, in a release build, as a corrupted
                // stream with nothing to point at.
                #[cfg(debug_assertions)]
                {
                    debug_assert!(
                        state_diff_bits <= 26,
                        "state diffs took {state_diff_bits} bits; the thresholds below are \
                         derived from a 26-bit ceiling (LLFSELog 9 + MLFSELog 9 + OffFSELog 8)",
                    );
                    debug_assert!(
                        ll_num_bits <= 16 && ml_num_bits <= 16 && of_num_bits <= 31,
                        "extra-bit widths ll={ll_num_bits} ml={ml_num_bits} of={of_num_bits} \
                         exceed the 16 / 16 / 31 the thresholds are derived from",
                    );
                    // The bound the branch itself rests on: without a flush,
                    // the leftover, the diffs and all three extra fields have
                    // to fit.
                    debug_assert!(
                        extra_bits_total >= 31 || 7 + state_diff_bits + extra_bits_total < 64,
                        "no flush at {extra_bits_total} extra bits, but 7 leftover + \
                         {state_diff_bits} diff bits + {extra_bits_total} would fill the \
                         accumulator, which keeps a bit free",
                    );
                }
                if extra_bits_total >= 31 {
                    unsafe {
                        burst.flush();
                    }
                }

                // Extras burst: ll (≤16) + ml (≤16) + of (≤ 31). Whether the
                // accumulator was drained above or not, ll and ml fit: after a
                // flush it holds ≤ 7 + 32 = 39, and without one the three
                // extras together were under 31.
                //
                // SAFETY: `encode_literal_length` / `encode_match_len`
                // bound `*_num_bits ≤ 16` and return a clean `*_add_bits`
                // (low `num_bits` bits only). `encode_offset` bounds
                // `of_num_bits ≤ ilog2(of) ≤ 31`, and the two conditionals
                // (`>= 31` above, `> 56` below) are upstream's own accounting
                // for when the offset field still fits beside what is already
                // there.
                unsafe {
                    burst.add(ll_add_bits as u64, ll_num_bits);
                    burst.add(ml_add_bits as u64, ml_num_bits);
                }
                // Upstream `zstd_compress_sequences.c:355`: with the
                // accumulator drained before the diffs, 7 leftover plus all
                // three extra fields must stay under 64, so the offset needs
                // its own container only past 56 bits of extras.
                if extra_bits_total > 56 {
                    unsafe {
                        burst.flush();
                    }
                }
                unsafe {
                    burst.add(of_add_bits as u64, of_num_bits);
                    burst.flush();
                }
            }
            burst.finish();
        }
        // An RLE stream has no state to write.
        if let Some(table) = ml_table {
            writer.write_bits(ml_state as u64, table.table_size.ilog2() as usize);
        }
        if let Some(table) = of_table {
            writer.write_bits(of_state as u64, table.table_size.ilog2() as usize);
        }
        if let Some(table) = ll_table {
            writer.write_bits(ll_state as u64, table.table_size.ilog2() as usize);
        }

        let bits_to_fill = writer.misaligned();
        if bits_to_fill == 0 {
            writer.write_bits(1u32, 8);
        } else {
            writer.write_bits(1u32, bits_to_fill);
        }
    }};
}

/// Write the sequence section's bitstream for `sequences` under the three
/// table `modes`, in LL / ML / OF order.
///
/// Where the tier `K` has BMI2 the loop runs under it, as upstream's does
/// (`ZSTD_encodeSequences_bmi2`): every field is written at a width known only
/// at run time, and BMI2's `shlx` / `shrx` / `bzhi` take that width in any
/// register, where the base x86 shifts take it only in `CL` and stall on the
/// flags they leave. The bytes written are the same on every tier.
fn encode_sequences<K: EncodeTier>(
    // Each carrying its length codes from the pass that derived the offset
    // codes.
    sequences: &[RawSequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    modes: [&FseTableMode<'_>; 3],
    // The three predefined tables, in the same order as `modes`. Passed rather
    // than read off `FseTables`, whose accessors borrow the whole struct while
    // the caller is holding its slots.
    defaults: [&FSETable; 3],
) {
    crate::encoding::fastpath::tier_match!(K {
        #[cfg(all(
            any(target_arch = "x86", target_arch = "x86_64"),
            feature = "kernel-avx2"
        ))]
        // SAFETY: the tier is only selected after confirming BMI2 (and AVX2).
        crate::encoding::fastpath::FastpathKernel::Avx2Bmi2 => unsafe {
            encode_sequences_bmi2(sequences, writer, modes, defaults)
        },
        _ => encode_sequences_scalar(sequences, writer, modes, defaults),
    })
}

#[cfg(all(
    any(target_arch = "x86", target_arch = "x86_64"),
    feature = "kernel-avx2"
))]
#[target_feature(enable = "bmi2")]
unsafe fn encode_sequences_bmi2(
    sequences: &[RawSequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    modes: [&FseTableMode<'_>; 3],
    defaults: [&FSETable; 3],
) {
    encode_sequences_body!(sequences, writer, modes, defaults)
}

fn encode_sequences_scalar(
    sequences: &[RawSequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    modes: [&FseTableMode<'_>; 3],
    defaults: [&FSETable; 3],
) {
    encode_sequences_body!(sequences, writer, modes, defaults)
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

/// Literal-length code per length, for lengths below 64 (upstream `LL_Code`,
/// `zstd_compress_internal.h:586`). At or above 64 the code is the high bit
/// plus [`LL_DELTA_CODE`].
const LL_CODE: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20,
    20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 24, 24,
    24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
];
const LL_DELTA_CODE: u32 = 19;
/// Extra bits carried by each literal-length code (upstream `LL_bits`).
const LL_EXTRA_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];

/// Match-length code per `len - 3`, for values below 128 (upstream `ML_Code`,
/// `zstd_compress_internal.h:603`). At or above 128 the code is the high bit
/// plus [`ML_DELTA_CODE`].
const ML_CODE: [u8; 128] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31, 32, 32, 33, 33, 34, 34, 35, 35, 36, 36, 36, 36, 37, 37, 37, 37, 38, 38,
    38, 38, 38, 38, 38, 38, 39, 39, 39, 39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 40, 40, 40, 40,
    40, 40, 40, 40, 40, 40, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 42, 42,
    42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
    42, 42, 42, 42, 42, 42,
];
const ML_DELTA_CODE: u32 = 36;
/// Extra bits carried by each match-length code (upstream `ML_bits`).
const ML_EXTRA_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// A short length's code in the low byte and its extra-bit width in the high
/// byte, so one load answers both.
const fn fuse_code_bits<const N: usize>(codes: &[u8; N], bits: &[u8]) -> [u16; N] {
    let mut fused = [0u16; N];
    let mut i = 0;
    while i < N {
        fused[i] = codes[i] as u16 | (bits[codes[i] as usize] as u16) << 8;
        i += 1;
    }
    fused
}
const LL_CODE_BITS: [u16; 64] = fuse_code_bits(&LL_CODE, &LL_EXTRA_BITS);
const ML_CODE_BITS: [u16; 128] = fuse_code_bits(&ML_CODE, &ML_EXTRA_BITS);

/// Split a literal-length into its FSE symbol, the extra bits to append, and
/// how many of them (upstream `ZSTD_LLcode` plus the `LL_bits` lookup its
/// caller does).
///
/// Runs once per sequence inside the encoding loop, where a 22-arm range match
/// compiled to a chain of comparisons; upstream reaches the same answer with a
/// table index. Every code's baseline is a multiple of its own extra-bit width,
/// so masking off those bits is the same subtraction the ranges spelled out.
/// From 64 up a code carries exactly `ilog2(len)` extra bits, so that branch
/// needs no table at all.
#[inline]
pub(crate) fn encode_literal_length(len: u32) -> (u8, u32, usize) {
    let (code, bits) = if len < 64 {
        let fused = LL_CODE_BITS[len as usize];
        (fused as u8, (fused >> 8) as usize)
    } else {
        // RFC 8878 3.1.1.3.2.1.1: literal-length code 35 is the last, its
        // baseline 65536 with 16 extra bits.
        // No value in the message: formatting it keeps `len` on the stack
        // across the whole calling loop.
        assert!(len < 131_072, "literal length out of encodable range");
        let log = len.ilog2();
        ((log + LL_DELTA_CODE) as u8, log as usize)
    };
    (code, len & ((1u32 << bits) - 1), bits)
}

/// Split a match length into its FSE symbol, the extra bits to append, and how
/// many of them (upstream `ZSTD_MLcode` plus the `ML_bits` lookup its caller
/// does). Codes are keyed on `len - 3`, the form the sequence section stores.
///
/// Table-driven for the same reason as [`encode_literal_length`].
#[inline]
pub(crate) fn encode_match_len(len: u32) -> (u8, u32, usize) {
    debug_assert!(len >= 3, "match length {len} out of encodable range");
    let base = len - 3;
    let (code, bits) = if base < 128 {
        let fused = ML_CODE_BITS[base as usize];
        (fused as u8, (fused >> 8) as usize)
    } else {
        // RFC 8878 3.1.1.3.2.1.1: match-length code 52 is the last, its
        // baseline 65539 with 16 extra bits.
        assert!(base < 131_072, "match length out of encodable range");
        let log = base.ilog2();
        ((log + ML_DELTA_CODE) as u8, log as usize)
    };
    (code, base & ((1u32 << bits) - 1), bits)
}

/// Convert an actual byte offset into the encoded offset code, using repeat offset
/// history per RFC 8878 §3.1.2.5. Updates `offset_hist` in place.
///
/// Encoded offset codes: 1/2/3 = repeat offsets, N+3 = new absolute offset N.
pub(crate) fn encode_offset_with_history(
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

/// Whether blocks at `strategy` code offsets with
/// [`encode_offset_with_history_fast`] rather than the full repeat search of
/// [`encode_offset_with_history`].
///
/// Both bands store what upstream's matchers store: a repeat-1 hit as
/// `REPCODE1_TO_OFFBASE` and every hashed match as `OFFSET_TO_OFFBASE`, with
/// no search of the deeper repeats (`zstd_fast.c`; `zstd_double_fast.c:193`,
/// `:290`, `:316`). Neither band post-splits, so the offsets are coded as
/// collected.
#[inline]
pub(crate) fn uses_fast_offset_codes(strategy: crate::encoding::strategy::StrategyTag) -> bool {
    use crate::encoding::strategy::StrategyTag;
    matches!(strategy, StrategyTag::Fast | StrategyTag::Dfast)
}

/// Fast-matcher offset→offBase conversion, mirroring upstream zstd's
/// `ZSTD_compressBlock_fast`: emit offBase 1 only for the immediate repeat
/// offset (`rep[0]` when `lit_len > 0`, `rep[1]` when `lit_len == 0` — the
/// litLength-0 rotation per RFC 8878 §3.1.2.5), and an explicit offset
/// otherwise. Unlike [`encode_offset_with_history`] it never probes `rep[1]`
/// (`lit_len > 0`) or `rep[2]`/`rep[0]-1` (`lit_len == 0`), so it does not
/// rewrite an explicit offset that happens to coincide with a deeper repeat
/// into offBase 2/3. That keeps the negative/fast band's sequence stream
/// byte-identical to the C reference (the deeper-repeat rewrite both costs a
/// per-sequence probe the fast matcher never pays and can shift the FSE symbol
/// histogram the wrong way). The repeat-offset history update follows directly
/// from the emitted code, identical to the full converter's rules.
pub(crate) fn encode_offset_with_history_fast(
    actual_offset: u32,
    lit_len: u32,
    offset_hist: &mut [u32; 3],
) -> u32 {
    if lit_len > 0 {
        if actual_offset == offset_hist[0] {
            return 1; // rep[0] match: history unchanged
        }
    } else if actual_offset == offset_hist[1] {
        // litLength-0 offBase 1 decodes as rep[1]: promote it, demote rep[0].
        offset_hist[1] = offset_hist[0];
        offset_hist[0] = actual_offset;
        return 1;
    }
    // Explicit offset: rotate the full repeat-offset history.
    offset_hist[2] = offset_hist[1];
    offset_hist[1] = offset_hist[0];
    offset_hist[0] = actual_offset;
    actual_offset + 3
}

/// The low `bits` bits of `value`, the extra-bit field the sequence section
/// carries beside a code. Every baseline in the format is a multiple of its
/// code's field width, so masking is the subtraction of the baseline.
#[inline(always)]
fn low_bits(value: u32, bits: usize) -> u32 {
    value & ((1u32 << bits) - 1)
}

pub(crate) fn encode_offset(len: u32) -> (u8, u32, usize) {
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
/// upstream zstd preferRepeat gate short-circuits the rebuild path.
/// Mirrors the post-decide reuse branch at the bottom of
/// `compress_literals` byte-for-byte (same size_format ladder, same
/// min_gain raw-fallback gate) so the wire output is identical to
/// the size-comparison reuse path when both would pick reuse.
/// Write `literals` with the previous table (a treeless section), or raw when
/// that does not save `minGain`. The previous table stays in place either way.
fn emit_reuse_literals<K: EncodeTier>(
    literals: &[u8],
    last_table: &huff0_encoder::HuffmanTable,
    single_stream: bool,
    writer: &mut BitWriter<&mut Vec<u8>>,
    reset_idx: usize,
    strategy: crate::encoding::strategy::StrategyTag,
) {
    writer.write_bits(3u8, 2); // treeless compressed literals type
    assert!(
        literals.len() <= 262_143,
        "literals exceed RFC 8878 18-bit size limit (262143)"
    );
    let (size_format, size_bits) = literal_size_format(literals.len(), single_stream);
    writer.write_bits(size_format, 2);
    writer.write_bits(literals.len() as u32, size_bits);
    let size_index = writer.index();
    writer.write_bits(0u32, size_bits);
    let index_before = writer.index();
    let mut encoder = huff0_encoder::HuffmanEncoder::new(last_table, writer);
    if single_stream {
        encoder.encode(literals, false);
    } else {
        encoder.encode4x::<K>(literals, false);
    }
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    let compressed_header_len = compressed_literals_header_bytes(literals.len());
    let huf_section_size = total_len - compressed_header_len;
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
    }
}

/// Bytes taken from each end when a section is suspected incompressible
/// (upstream `SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE`, `huf_compress.c:1251`).
const SUSPECT_SAMPLE_SIZE: usize = 4096;
/// How many times the sample must fit in the section before sampling is worth
/// it at all (upstream `SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO`).
const SUSPECT_SAMPLE_RATIO: usize = 10;
/// Literals per sequence at or above which a section is suspected
/// incompressible even though the search did find something (upstream
/// `SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO`, `zstd_compress.c:2886`).
pub(crate) const SUSPECT_LITERAL_RATIO: usize = 20;

/// Whether the literals of a block that produced `sequence_count` sequences
/// should be probed by sample before the full histogram is paid for.
///
/// Upstream `zstd_compress.c:2924`. A block whose search found nothing, or
/// found so little that the literals dwarf it, is the shape that ends up
/// emitted raw, and the histogram it would otherwise pay for spans the whole
/// section.
pub(crate) fn literals_suspected_incompressible(
    literals_len: usize,
    sequence_count: usize,
) -> bool {
    sequence_count == 0 || literals_len / sequence_count >= SUSPECT_LITERAL_RATIO
}

/// Whether two end samples say the section is flat enough to go out raw
/// without paying for a histogram of the whole thing (upstream
/// `huf_compress.c:1367`).
///
/// Shared by the emitter and by the splitter's cost estimator: the estimator
/// mirrors the emitter's literal-mode branches in order so a probe cost matches
/// what the emitter will write, and a section costed as Huffman-compressed here
/// but emitted raw there would let the splitter choose a partition on a price
/// nobody can produce.
///
/// Both ends, not one: a section that is random at the front and structured at
/// the back must not be judged on the front alone. Each probe counts afresh, so
/// each reports its own most frequent symbol, which is what the summed
/// threshold is scaled for.
fn end_samples_look_flat(literals: &[u8]) -> bool {
    if literals.len() < SUSPECT_SAMPLE_SIZE * SUSPECT_SAMPLE_RATIO {
        return false;
    }
    let mut counts = core::mem::MaybeUninit::uninit();
    let (_, _, head_largest) =
        crate::histogram::count_bytes_uninit(&literals[..SUSPECT_SAMPLE_SIZE], &mut counts);
    let (_, _, tail_largest) = crate::histogram::count_bytes_uninit(
        &literals[literals.len() - SUSPECT_SAMPLE_SIZE..],
        &mut counts,
    );
    head_largest + tail_largest <= ((2 * SUSPECT_SAMPLE_SIZE) >> 7) + 4
}

/// Write `literals` as a Huffman-coded section (or raw, when coding does not
/// pay), in the order of upstream `HUF_compress_internal`. Returns the table a
/// block built and used, which becomes the previous table once the block is
/// kept; `None` when the section repeated the previous table or went out raw,
/// either of which leaves the previous table in place.
#[allow(clippy::too_many_arguments)]
fn compress_literals<K: EncodeTier>(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    repeat_valid: bool,
    writer: &mut BitWriter<&mut Vec<u8>>,
    strategy: crate::encoding::strategy::StrategyTag,
    huf_search: bool,
    weight_scratch: &mut huff0_encoder::WeightScratch,
    suspected_incompressible: bool,
) -> Option<huff0_encoder::HuffmanTable> {
    let reset_idx = writer.index();
    let single_stream = single_literal_stream(literals.len(), repeat_valid);

    // preferRepeat with a valid table (`huf_compress.c:1359`): repeat it before
    // any histogram. A valid table codes every byte value, so it needs no check.
    if prefer_repeat_eligible(strategy, literals.len())
        && repeat_valid
        && let Some(prev) = last_table
    {
        debug_assert!(prev.codes_every_byte());
        emit_reuse_literals::<K>(literals, prev, single_stream, writer, reset_idx, strategy);
        return None;
    }

    // Sample before committing to the full histogram (upstream
    // `huf_compress.c:1367`). The histogram spans the whole section, and for a
    // block the search found nothing in that section is the entire block — a
    // megabyte of random input costs a megabyte of counting only to be thrown
    // away by the flatness gate below. Two 4 KiB probes answer the same
    // question for 8 KiB of counting.
    if suspected_incompressible && end_samples_look_flat(literals) {
        raw_literals(literals, writer);
        return None;
    }

    let mut counts = core::mem::MaybeUninit::uninit();
    let (counts, max_symbol, largest_count) =
        crate::histogram::count_bytes_uninit(literals, &mut counts);
    // Upstream zstd pre-build incompressibility gate (`huf_compress.c:1384`):
    // a histogram this flat (`largest <= (srcSize >> 7) + 4`) is heuristically
    // not worth compressing — bail to raw BEFORE the tree build and the full
    // `encode4x` pass. The single-symbol case (`largest == srcSize`) never
    // reaches here: the block emitter routes all-identical sections to RLE
    // first.
    if largest_count <= (literals.len() >> 7) + 4 {
        raw_literals(literals, writer);
        return None;
    }

    // preferRepeat with a table that has to be checked (`huf_compress.c:1389-1398`):
    // repeat it when it codes this section, before any tree build.
    if prefer_repeat_eligible(strategy, literals.len())
        && let Some(prev) = last_table
        && prev
            .estimate_compressed_size_from_counts_checked(counts)
            .is_some()
    {
        emit_reuse_literals::<K>(literals, prev, single_stream, writer, reset_idx, strategy);
        return None;
    }

    // Mutable because the size query encodes the weight description into the
    // table's buffer; `write_table` below then reads it rather than repeating
    // the encode.
    let mut new_encoder_table = huff0_encoder::HuffmanTable::build_from_counts_gated_with(
        &counts[..=max_symbol],
        literals.len(),
        huf_search,
        weight_scratch,
    );

    let Some(new_table_description_size) =
        new_encoder_table.writeable_table_description_size(weight_scratch.weight_fse_table())
    else {
        raw_literals(literals, writer);
        weight_scratch.recycle(new_encoder_table);
        return None;
    };
    // Shared with the splitter cost estimator
    // (`estimate_literals_section_bytes`) so both code paths agree on which
    // table they would pick for a given `(new_table, last_table, literals)`
    // input.
    let choice = decide_huff_reuse_like_encoder(
        &new_encoder_table,
        last_table,
        new_table_description_size,
        literals,
        counts,
    );
    let new_table = match choice {
        HuffChoice::Raw => {
            raw_literals(literals, writer);
            weight_scratch.recycle(new_encoder_table);
            return None;
        }
        HuffChoice::New => true,
        HuffChoice::Repeat => false,
    };
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
    let (size_format, size_bits) = literal_size_format(literals.len(), single_stream);

    writer.write_bits(size_format, 2);
    writer.write_bits(literals.len() as u32, size_bits);
    let size_index = writer.index();
    writer.write_bits(0u32, size_bits);
    let index_before = writer.index();
    let mut encoder = huff0_encoder::HuffmanEncoder::new(encoder_table, writer);
    if single_stream {
        encoder.encode(literals, new_table)
    } else {
        encoder.encode4x::<K>(literals, new_table)
    };
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    // Upstream zstd `compress_literals` raw-fallback gate
    // (`zstd_compress_literals.c:187-188`):
    //   `cLitSize >= srcSize - minGain`
    // where upstream zstd's `cLitSize` is the encoded literals payload plus the
    // tree description (output of `HUF_compress*`, excluding the
    // surrounding `lhSize` literals header), and `srcSize` is the
    // literal-payload length. In our terms:
    //   - upstream zstd `cLitSize` ≡ `total_len - compressed_literals_header_bytes`
    //     (i.e. tree_desc + huf_payload, no lhSize)
    //   - upstream zstd `srcSize`  ≡ `literals.len()`
    // Comparing `total_len >= raw_section_bytes - minGain` (with the
    // compressed-section lhSize on the LHS and raw-section header on
    // the RHS) skews the threshold by `compressed_header - raw_header`
    // bytes and rejects compressed sections that upstream zstd would keep —
    // direct ratio loss. Mirror upstream zstd's payload-vs-srcSize form here.
    // `minGain` is strategy-aware (`min_gain` helper above; ~1.56% for
    // fast..btopt, ~0.78% for btultra, ~0.39% for btultra2). Saturating
    // subtraction covers tiny inputs where `literals.len() < minGain`.
    let compressed_header_len = compressed_literals_header_bytes(literals.len());
    let huf_section_size = total_len - compressed_header_len; // tree_desc + payload, no lhSize
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
        // The section goes out raw, so the table just built is dead; hand its
        // buffers to the next build instead of dropping them.
        weight_scratch.recycle(new_encoder_table);
        None
    } else if new_table {
        Some(new_encoder_table)
    } else {
        // The previous table was kept, so this one is dead — same as above.
        weight_scratch.recycle(new_encoder_table);
        None
    }
}

/// The literals section's `Size_Format` and the width of its two size fields
/// (RFC 8878 §3.1.1.3.1.1): one stream takes `0b00` with 10-bit sizes, which
/// holds up to 1023 bytes either way.
#[inline]
fn literal_size_format(len: usize, single_stream: bool) -> (u8, usize) {
    debug_assert!(!single_stream || len < 1024);
    match len {
        _ if single_stream => (0b00, 10),
        0..1024 => (0b01, 10),
        1024..16384 => (0b10, 14),
        _ => (0b11, 18),
    }
}

#[cfg(test)]
mod tests;
