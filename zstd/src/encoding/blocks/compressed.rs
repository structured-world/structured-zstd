use alloc::{boxed::Box, vec::Vec};

use crate::{
    bit_io::BitWriter,
    blocks::block::BlockType,
    decoding::simd_copy::ExactCopyTier,
    encoding::block_header::BlockHeader,
    encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable, SharedFseTable},
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

#[derive(Default)]
struct EncodedBlockParts {
    literals: Vec<u8>,
    sequences: Vec<RawSequence>,
}

#[derive(Default)]
pub(crate) struct CompressedBlockScratch {
    parts: EncodedBlockParts,
    /// One packed [`SequenceCode`] per sequence of the partition being
    /// encoded, filled by the pass that derives the offset codes and read by
    /// the bit writer. Kept here so it is allocated once for the compressor
    /// rather than per block.
    sequence_codes: Vec<u32>,
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
    /// Persistent slot for `compress_block_encoded`'s pre-block entropy
    /// rollback snapshot. `clone_from` into this slot reuses its `Vec`
    /// buffers across blocks; a fresh `.clone()` per block paid a
    /// malloc + free pair on both Huffman code containers every block.
    pub(crate) huff_rollback: Option<huff0_encoder::HuffmanTable>,
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
    pub(crate) fn retained_heap_size(&self) -> usize {
        self.parts.literals.capacity()
            + self.parts.sequences.capacity() * core::mem::size_of::<RawSequence>()
            + self.partitions.capacity() * core::mem::size_of::<usize>()
            + self.prefix_sums.heap_size()
            + self.compressed.capacity()
            + self
                .estimator_workspace
                .as_ref()
                .map_or(0, EstimatorWorkspace::heap_size)
            + self
                .huff_rollback
                .as_ref()
                .map_or(0, |table| table.heap_size())
            + self.estimator_inner.as_ref().map_or(0, |inner| {
                core::mem::size_of::<CompressedBlockScratch>() + inner.retained_heap_size()
            })
    }

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
    fn heap_size(&self) -> usize {
        (self.lit.capacity() + self.ml.capacity()) * core::mem::size_of::<usize>()
    }

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

/// One collected sequence.
///
/// `off_base` holds the offset the matcher found until [`fill_wire_offsets`]
/// runs over the sequence, and the wire code from then on: 1/2/3 for the repeat
/// offsets, N+3 for an explicit N. It is one field rather than two because the
/// found offset has no reader once its code exists, and a fourth word would
/// widen every sequence in the block for a value with a lifetime of one pass.
/// Upstream keeps the same single slot, filled at match time
/// (`SeqDef::offBase`, written by `ZSTD_storeSeq`); ours cannot be filled that
/// early because the code depends on a repeat-offset history that must not
/// advance across a partition the emitter ends up writing raw.
#[derive(Clone, Copy)]
struct RawSequence {
    ll: u32,
    ml: u32,
    off_base: u32,
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
    let decisions = encode_block_parts(
        state,
        &scratch.parts.literals,
        &mut scratch.parts.sequences,
        &mut scratch.sequence_codes,
        output,
    );
    // This path writes the block it just encoded, so the tables it chose are
    // what the next block reads.
    remember_last_used_tables(&mut state.fse_tables, decisions);
    state.block_scratch = scratch;
}

pub(crate) fn compress_block_with_post_split<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    output: &mut Vec<u8>,
    #[cfg(feature = "lsm")] mut block_decompressed_sizes: Option<&mut Vec<u32>>,
    #[cfg(all(feature = "lsm", feature = "hash"))] mut block_checksums: Option<&mut Vec<u32>>,
) {
    let mut scratch = core::mem::take(&mut state.block_scratch);
    collect_block_parts(state, &mut scratch.parts);
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
            codes: &mut scratch.sequence_codes,
        };
        let emitted_raw = emit_single_sequence_block(
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
            last_huff_table: state.last_huff_table.clone(),
            ll_previous: state.fse_tables.ll_previous.clone(),
            ml_previous: state.fse_tables.ml_previous.clone(),
            of_previous: state.fse_tables.of_previous.clone(),
            offset_hist: state.offset_hist,
        },
        scratch_state: CompressState {
            matcher: EntropyOnlyMatcher,
            // The splitter's scratch state never reaches the raw-skip, which
            // is decided one level up on the whole block.
            seen_content: Default::default(),
            // Inherited rather than re-resolved: this scratch state stands in
            // for the same compressor on the same CPU.
            copy_tier: state.copy_tier,
            last_huff_table: state.last_huff_table.clone(),
            huff_table_spare: None,
            huff_rollback: None,
            // Lent, not created: the estimator builds a table per split
            // candidate, and a fresh scratch here would take its buffers again
            // on every post-split block and drop them at the end of it. Handed
            // back below, so the emitter that follows keeps using the same one.
            huff_weights: core::mem::take(&mut state.huff_weights),
            fse_tables: clone_fse_tables(&state.fse_tables),
            block_scratch: inner_scratch,
            offset_hist: state.offset_hist,
            strategy_tag: state.strategy_tag,
            pre_split: state.pre_split,
            huf_optimal_search: state.huf_optimal_search,
            literal_compression_disabled: state.literal_compression_disabled,
        },
        workspace,
    };
    estimator.derive_block_splits(0, scratch.parts.sequences.len(), &mut scratch.partitions);
    scratch.partitions.push(scratch.parts.sequences.len());
    workspace = estimator.workspace;
    scratch.estimator_workspace = Some(workspace);
    // Stash the inner scratch back for the next frame (its buffers stay
    // allocated; the estimator clears them per use), and take the weight
    // builder's buffers back so the emitter and the next block reuse them.
    let CompressState {
        block_scratch: inner_block_scratch,
        huff_weights,
        ..
    } = estimator.scratch_state;
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
            codes: &mut scratch.sequence_codes,
        };
        let emitted_raw = emit_single_sequence_block(
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

/// Literal-run length at or above which `append_literals` hands off to
/// `Vec::extend_from_slice` (libc `memcpy` → ERMS `rep movsb` on x86).
/// Below it the inline exact-copy loop wins (no libc call + ERMS startup
/// cost); at/above it the copy is bandwidth-bound and ERMS is faster.
/// Mirrors `simd_copy::BULK_MEMCPY_THRESHOLD` (the match-copy crossover).
const LITERAL_INLINE_COPY_MAX: usize = 2048;

/// Append `lits` to `dst` using inline copy ops, avoiding the libc
/// memcpy call overhead that `Vec::extend_from_slice` lowers to for
/// runtime-sized `ptr::copy_nonoverlapping`. Fast L1 emits literal runs
/// of 1-10 bytes typically — at thousands of sequences per block, the
/// per-emit libc call dominated the hot path (flamegraph:
/// `__memmove_avx_unaligned_erms` chain ≈ 16 % of L1 encode CPU).
///
/// - `len ≤ 32`: `simd_copy::copy_bytes_overshooting` with
///   `src.1 == dst.1 == lit_len` (no overshoot READ — the caller's slice
///   readable slack is unknown), which drops into the byte / overlapping-
///   u64 path, fully inlineable.
/// - `32 < len < 2048`: `simd_copy::copy_exact_medium` — the widest
///   available SIMD tier (AVX2 32B / SSE2 16B / NEON / scalar) doing an
///   EXACT copy (floor bulk + overlapping tier-width tail), the safe
///   upstream zstd-wildcopy analog: matches glibc's store width but drops the
///   libc call, and never overshoots reads (borrowed-input safe).
/// - `len ≥ 2048`: `extend_from_slice` — bandwidth-bound, ERMS wins.
///
/// Called, not inlined, even though upstream inlines the equivalent
/// (`ZSTD_storeSeq` stores sixteen bytes on the spot) and even though the runs
/// are short enough for it — a level-3 decodecorpus frame averages about eight
/// bytes. Splitting the short case out to `#[inline(always)]` and leaving the
/// ladder behind a tail cost **2.16% in cycles** at level 3 while removing 1.04%
/// of the program's instructions. The match loop calls this from a dozen-odd
/// sites, so inlining even a short body there buys decode pressure in the
/// loop worth more than the calls it saves.
#[inline]
fn append_literals(dst: &mut Vec<u8>, lits: &[u8], copy_tier: ExactCopyTier) {
    let lit_len = lits.len();
    if lit_len == 0 {
        return;
    }
    if lit_len >= LITERAL_INLINE_COPY_MAX {
        dst.extend_from_slice(lits);
        return;
    }
    // Production callers (`collect_block_parts`) pre-reserve `src_len` of
    // spare capacity, so the sum of all literal runs across a block fits
    // without grow. This is a SAFE fn, so enforce the precondition in
    // release too — a future caller skipping the pre-reserve would
    // otherwise get an OOB write past the `Vec`'s allocation. The branch
    // is cold on the production hot path.
    let cur_len = dst.len();
    if dst.capacity() - cur_len < lit_len {
        dst.reserve(lit_len);
    }
    let dst_ptr = unsafe { dst.as_mut_ptr().add(cur_len) };
    // SAFETY: `lits` is a valid slice (reading `lit_len` bytes from
    // `lits.as_ptr()` is in-bounds); the `dst.reserve(lit_len)` above
    // guarantees `dst_ptr` has `lit_len` bytes of spare capacity. Both
    // paths write EXACTLY `lit_len` bytes (no overshoot).
    unsafe {
        if lit_len <= 32 {
            crate::decoding::simd_copy::copy_bytes_overshooting(
                (lits.as_ptr(), lit_len),
                (dst_ptr, lit_len),
                lit_len,
            );
        } else {
            crate::decoding::simd_copy::copy_exact_medium(
                lits.as_ptr(),
                dst_ptr,
                lit_len,
                copy_tier,
            );
        }
        dst.set_len(cur_len + lit_len);
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
    // Hoisted out of the closure: the tier was settled when the compressor was
    // built, and the emit loop just carries the value.
    let copy_tier = state.copy_tier;
    state.matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => {
            append_literals(&mut parts.literals, literals, copy_tier)
        }
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            let ll = literals.len() as u32;
            append_literals(&mut parts.literals, literals, copy_tier);
            parts.sequences.push(RawSequence {
                ll,
                ml: match_len as u32,
                // The found offset. `fill_wire_offsets` replaces it with its
                // code once the partition this sequence lands in is about to be
                // encoded, since the code depends on a history that partition
                // boundaries can rewind.
                off_base: offset as u32,
            });
        }
    });
}

fn encode_block_parts<M: Matcher>(
    state: &mut CompressState<M>,
    literals_vec: &[u8],
    raw_sequences: &mut [RawSequence],
    // Scratch for the packed per-sequence codes, carried by the caller so it is
    // allocated once rather than per block.
    codes: &mut Vec<u32>,
    output: &mut Vec<u8>,
    // What each axis decided, for the caller to apply once it knows the block
    // is kept. LL, ML, OF.
) -> [LastUsedTable; 3] {
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
    let has_huf_table = state.last_huff_table.is_some();
    let min_lits = min_literals_to_compress(strategy, has_huf_table);
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
    if state.literal_compression_disabled {
        // Upstream zstd `ZSTD_literalsCompressionIsDisabled` (auto mode:
        // `strategy == ZSTD_fast && targetLength > 0`, i.e. the negative levels):
        // emit RAW literals, skipping the Huffman pass entirely
        // (`ZSTD_noCompressLiterals`). Trades ratio for encode speed and matches
        // C's negative-band frames byte-for-byte (the literals section there is
        // Raw, not Compressed/RLE).
        raw_literals(literals_vec, &mut writer);
        state.clear_huff_table();
    } else if !literals_vec.is_empty() && all_bytes_identical(literals_vec) {
        rle_literals(literals_vec, &mut writer);
        state.clear_huff_table();
    } else if literals_vec.len() >= min_lits {
        match compress_literals(
            literals_vec,
            state.last_huff_table.as_ref(),
            &mut writer,
            strategy,
            state.huf_optimal_search,
            &mut state.huff_weights,
            literals_suspected_incompressible(literals_vec.len(), raw_sequences.len()),
        ) {
            HuffmanTableUpdate::New(table) => {
                state.replace_huff_table(table);
            }
            HuffmanTableUpdate::Reused => {}
            HuffmanTableUpdate::Cleared => {
                state.clear_huff_table();
            }
        }
    } else {
        raw_literals(literals_vec, &mut writer);
        state.clear_huff_table();
    }

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
        let mut ll_counts = [0usize; 256];
        let mut ml_counts = [0usize; 256];
        let mut of_counts = [0usize; 256];
        // The offset codes are derived in this same pass rather than by a
        // walk of their own ahead of the literals section: both passes read
        // every sequence, and the second one was reading back what the first
        // had just written.
        let counts = SequenceCodeCounts {
            ll: &mut ll_counts,
            ml: &mut ml_counts,
            of: &mut of_counts,
        };
        let (ll_max, ml_max, of_max) = if matches!(
            state.strategy_tag,
            crate::encoding::strategy::StrategyTag::Fast
        ) {
            fill_and_count::<true>(raw_sequences, &mut state.offset_hist, counts, codes)
        } else {
            fill_and_count::<false>(raw_sequences, &mut state.offset_hist, counts, codes)
        };
        let raw_sequences: &[RawSequence] = raw_sequences;
        let codes: &[u32] = codes;
        let total = raw_sequences.len();

        // Stream codes of the LAST sequence: upstream zstd codes the final symbol
        // of each stream via the FSE init-state and drops one occurrence of it
        // from the emitted table's histogram (see `build_seq_ctable`). `Some`
        // here because these modes are written to the frame.
        let (last_ll, last_ml, last_of) = raw_sequences.last().map_or((0, 0, 0), |seq| {
            (
                encode_literal_length(seq.ll).0 as usize,
                encode_match_len(seq.ml).0 as usize,
                encode_offset(seq.off_base).0 as usize,
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

        encode_sequences(
            raw_sequences,
            codes,
            &mut writer,
            &ll_mode,
            &ml_mode,
            &of_mode,
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
    decisions
}

/// Workspace shared across estimator probes so per-probe cost computation never
/// allocates. Counts are zeroed at the top of every probe.
struct EstimatorWorkspace {
    lit_counts: Box<[usize; 256]>,
    ll_counts: Box<[usize; 256]>,
    ml_counts: Box<[usize; 256]>,
    of_counts: Box<[usize; 256]>,
    sequences: Vec<RawSequence>,
}

impl EstimatorWorkspace {
    /// The four boxed count tables plus whatever the sequence buffer has grown
    /// to. All four boxes are always present once the workspace exists.
    fn heap_size(&self) -> usize {
        4 * core::mem::size_of::<[usize; 256]>()
            + self.sequences.capacity() * core::mem::size_of::<RawSequence>()
    }
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

/// Dry-run analog of [`encode_block_parts`]: mirrors the
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
    // The probe cannot fill in place: it walks sub-ranges of the block's
    // sequences repeatedly, from a scratch history, while the array itself is
    // borrowed immutably by the estimator for the whole search. So it keeps a
    // copy — which costs nothing that matters, since block splitting only runs
    // from level 11 up and never on the band this array's copy was hurting.
    workspace.sequences.clear();
    if workspace.sequences.capacity() < raw_sequences.len() {
        workspace
            .sequences
            .reserve_exact(raw_sequences.len() - workspace.sequences.len());
    }
    workspace.sequences.extend_from_slice(raw_sequences);
    fill_wire_offsets(
        &mut workspace.sequences,
        &mut state.offset_hist,
        matches!(
            state.strategy_tag,
            crate::encoding::strategy::StrategyTag::Fast
        ),
    );

    let lit_bytes = estimate_literals_section_bytes(
        literals_vec,
        &mut state.last_huff_table,
        &mut workspace.lit_counts,
        state.strategy_tag,
        state.huf_optimal_search,
        state.literal_compression_disabled,
        &mut state.huff_weights,
        literals_suspected_incompressible(literals_vec.len(), raw_sequences.len()),
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
            state.strategy_tag,
        )
    };

    lit_bytes + seq_bytes
}

// One argument over the lint's threshold. Every one of them is a distinct
// decision the emitter makes about this section, and this function exists to
// reproduce those decisions in the same order; bundling them into a struct
// would hide exactly the correspondence that has to stay visible.
#[allow(clippy::too_many_arguments)]
fn estimate_literals_section_bytes(
    literals: &[u8],
    last_huff: &mut Option<huff0_encoder::HuffmanTable>,
    counts: &mut [usize; 256],
    strategy: crate::encoding::strategy::StrategyTag,
    huf_search: bool,
    lit_disabled: bool,
    weight_scratch: &mut huff0_encoder::WeightScratch,
    suspected_incompressible: bool,
) -> usize {
    // Mirror `encode_block_parts` literal-mode branches
    // **in the same order**. The disabled gate (negative levels: raw literals,
    // no Huffman) is checked FIRST exactly as the emitter does.
    if lit_disabled {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
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
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + 1;
    }
    let min_lits = min_literals_to_compress(strategy, last_huff.is_some());
    if literals.len() < min_lits {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    }

    // Upstream zstd preferRepeat fast-path: skip the histogram +
    // `build_from_counts` cost. Mirrors upstream zstd's
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

    // Mirror the emitter's end-sample shortcut, in the same position. Without
    // it a section with flat ends and a biased interior costs as
    // Huffman-compressed here and is emitted raw there, and the splitter picks
    // a partition on a price the emitter cannot produce.
    if suspected_incompressible && end_samples_look_flat(literals, counts) {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    }

    let (max_sym, largest_count) = crate::histogram::count_bytes(literals, counts);
    // Mirror `compress_literals`' upstream zstd pre-build incompressibility gate
    // byte-for-byte (flat histogram → raw section, no tree build) so
    // splitter probe costs match what the emitter writes.
    if largest_count <= (literals.len() >> 7) + 4 {
        *last_huff = None;
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    }
    // Mutable because the size query is what encodes the weight description
    // into the table's own buffer, so the emitter that follows reads it.
    let mut new_table = huff0_encoder::HuffmanTable::build_from_counts_gated_with(
        &counts[..=max_sym],
        huf_search,
        weight_scratch,
    );

    let Some(new_desc) = new_table.writeable_table_description_size() else {
        *last_huff = None;
        // Nothing downstream reads this table; hand its buffers to the next
        // build rather than dropping them.
        weight_scratch.recycle(new_table);
        return uncompressed_literals_header_bytes(literals.len()) + literals.len();
    };
    // For lit_size ≥ 256, upstream zstd `compress_literals` calls `encoder.encode4x`
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
        counts,
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
    let raw_section_bytes = uncompressed_literals_header_bytes(literals.len()) + literals.len();
    let huf_section_size = total - compressed_header; // tree_desc + payload, no lhSize
    if use_raw_literal_fallback(huf_section_size, literals.len(), strategy) {
        *last_huff = None;
        weight_scratch.recycle(new_table);
        return raw_section_bytes;
    }

    if use_new {
        // The table this displaces is the one to recycle; the new one is kept.
        if let Some(displaced) = last_huff.replace(new_table) {
            weight_scratch.recycle(displaced);
        }
    } else {
        weight_scratch.recycle(new_table);
    }
    total
}

fn estimate_sequences_section_bytes(
    sequences: &[RawSequence],
    fse_tables: &mut FseTables,
    ll_counts: &mut [usize; 256],
    ml_counts: &mut [usize; 256],
    of_counts: &mut [usize; 256],
    strategy: crate::encoding::strategy::StrategyTag,
) -> usize {
    ll_counts.fill(0);
    ml_counts.fill(0);
    of_counts.fill(0);
    let mut extra_bits: usize = 0;
    for seq in sequences {
        let (ll, _, ll_bits) = encode_literal_length(seq.ll);
        let (ml, _, ml_bits) = encode_match_len(seq.ml);
        let (of, _, _) = encode_offset(seq.off_base);
        ll_counts[ll as usize] += 1;
        ml_counts[ml as usize] += 1;
        of_counts[of as usize] += 1;
        // Upstream zstd: OF code's value equals its additional-bits width.
        extra_bits += ll_bits + ml_bits + of as usize;
    }

    // Destructured for the same reason as the emitter: the default accessors
    // borrow the whole struct, which would collide with the `*_next` slots.
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
    } = fse_tables;
    let ll_default: &FSETable = ll_default;
    let ml_default: &FSETable = ml_default;
    let of_default: &FSETable = of_default;

    // Same `choose_table` calls as the real encoder — counts the iterator
    // internally, identical decision path.
    let ll_mode = choose_table(
        ll_previous.as_ref(),
        ll_default,
        sequences.iter().map(|seq| encode_literal_length(seq.ll).0),
        9,
        strategy,
        ll_next,
    );
    let ml_mode = choose_table(
        ml_previous.as_ref(),
        ml_default,
        sequences.iter().map(|seq| encode_match_len(seq.ml).0),
        9,
        strategy,
        ml_next,
    );
    let of_mode = choose_table(
        of_previous.as_ref(),
        of_default,
        sequences.iter().map(|seq| encode_offset(seq.off_base).0),
        8,
        strategy,
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

    // Mirror state mutation done by `encode_block_parts`.
    let decisions = [
        into_last_used_table(ll_mode),
        into_last_used_table(ml_mode),
        into_last_used_table(of_mode),
    ];
    remember_last_used_tables(fse_tables, decisions);
    // The emitter keeps the handle a commit displaces, to build the next
    // block's table into. A probe must not: the splitter holds many of these
    // states at once, and a spare per axis per probe doubles the tables alive
    // at any moment. Dropping it leaves a probe with exactly what it needs,
    // which is what the allocate-per-table form gave it.
    fse_tables.ll_next = None;
    fse_tables.ml_next = None;
    fse_tables.of_next = None;

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

fn decide_huff_reuse_like_encoder(
    new_table: &huff0_encoder::HuffmanTable,
    last_table: Option<&huff0_encoder::HuffmanTable>,
    new_desc: usize,
    literals: &[u8],
    counts: &[usize; 256],
    strategy: crate::encoding::strategy::StrategyTag,
) -> bool {
    let Some(prev) = last_table else {
        return true;
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
    let Some(old_estimate) = prev.estimate_compressed_size_from_counts_checked(counts) else {
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
        .estimate_compressed_size_from_counts_checked(counts)
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
    codes: &'a mut Vec<u32>,
}

fn emit_single_sequence_block<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    source_len: usize,
    literals: &[u8],
    sequences: &mut [RawSequence],
    buffers: &mut SingleSequenceEmitBuffers<'_>,
) -> bool {
    let saved_offset_hist = state.offset_hist;
    // Copy into the rollback slot rather than a fresh `Option`: the slot keeps
    // its buffers between blocks, so this reuses them instead of taking two
    // `Vec`s per block. `had_huff_table` carries what an `Option` copy would
    // have carried, without discarding the buffer when there is nothing to
    // copy.
    let had_huff_table = state.last_huff_table.is_some();
    if let Some(src) = &state.last_huff_table {
        match &mut state.huff_rollback {
            Some(dst) => dst.clone_from(src),
            slot => *slot = Some(src.clone()),
        }
    }
    // The FSE tables need no copy: the block builds into the `*_next` slots and
    // the previous ones are only read, so rolling back is simply not applying
    // the decisions below. Copying them was also what kept the built table's
    // handle shared, which forced a fresh one per block.
    buffers.compressed.clear();
    let fse_decisions = encode_block_parts(
        state,
        literals,
        sequences,
        buffers.codes,
        buffers.compressed,
    );
    let min_gain = (source_len >> 8) + 2;
    if buffers.compressed.len() >= source_len.saturating_sub(min_gain) {
        state.offset_hist = saved_offset_hist;
        if had_huff_table {
            // Swap, not assign: the table this block built goes back into the
            // rollback slot and becomes the next block's copy buffer.
            core::mem::swap(&mut state.last_huff_table, &mut state.huff_rollback);
        } else {
            // Nothing was carried in, so whatever this block built is dropped
            // from the state; park it for the next build rather than freeing.
            state.clear_huff_table();
        }
        // A partition that built a table and then chose raw or RLE literals
        // parked it on the way there, so the swap above had nothing to give
        // back: the slot would go into the next partition empty, which is the
        // allocation of both code buffers it exists to avoid — on exactly the
        // run of partitions that keeps failing the size test. Take the parked
        // table; the slot is asked for one per partition, the spare's other
        // reader once per frame.
        if state.huff_rollback.is_none() {
            state.huff_rollback = state.huff_table_spare.take();
        }
        // The FSE decisions are simply not applied, so the previous tables are
        // whatever the last kept block left.
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
        remember_last_used_tables(&mut state.fse_tables, fse_decisions);
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

/// One sequence's three FSE symbols and the two extra-bit widths that are not
/// derivable from a symbol alone, packed into a word.
///
/// The derivation pass has all five in hand, and the bit writer needs all five
/// again a moment later; recomputing them there costs two table lookups with
/// their bounds checks, a bit scan and the branches that pick between the
/// small-value tables and the logarithmic form. Upstream keeps the same values
/// between the same two passes, as the three byte arrays `ZSTD_seqToCodes`
/// writes.
///
/// The offset code is its own extra-bit width, so only two widths are stored.
/// Layout, low bits first: ll code 6, ml code 6, of code 5, ll bits 5, ml bits
/// 5 — 27 bits, and every field's range is fixed by the sequence-section
/// format.
struct SequenceCode(u32);

impl SequenceCode {
    #[inline(always)]
    fn pack(ll_code: u8, ml_code: u8, of_code: u8, ll_bits: usize, ml_bits: usize) -> u32 {
        debug_assert!(ll_code < 64 && ml_code < 64 && of_code < 32);
        debug_assert!(ll_bits < 32 && ml_bits < 32);
        ll_code as u32
            | (ml_code as u32) << 6
            | (of_code as u32) << 12
            | (ll_bits as u32) << 17
            | (ml_bits as u32) << 22
    }

    #[inline(always)]
    fn ll_code(&self) -> u8 {
        (self.0 & 63) as u8
    }

    #[inline(always)]
    fn ml_code(&self) -> u8 {
        (self.0 >> 6 & 63) as u8
    }

    #[inline(always)]
    fn of_code(&self) -> u8 {
        (self.0 >> 12 & 31) as u8
    }

    #[inline(always)]
    fn ll_bits(&self) -> usize {
        (self.0 >> 17 & 31) as usize
    }

    #[inline(always)]
    fn ml_bits(&self) -> usize {
        (self.0 >> 22 & 31) as usize
    }

    /// The offset code doubles as its own extra-bit width (upstream's
    /// `ofBits = ofCode`).
    #[inline(always)]
    fn of_bits(&self) -> usize {
        self.of_code() as usize
    }
}

/// The three sequence-code histograms, passed as one argument so the pass that
/// fills them stays under the register-pressure of six.
struct SequenceCodeCounts<'a> {
    ll: &'a mut [usize; 256],
    ml: &'a mut [usize; 256],
    of: &'a mut [usize; 256],
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
/// `FAST_REPCODE` picks the offBase policy once per block instead of per
/// sequence. Upstream's fast matcher emits only offBase 1 (rep[0] when
/// litLength > 0, rep[1] when litLength == 0 via the secondary-position check)
/// or an explicit offset, and never 2/3; greedy and above search all three
/// repeat offsets, which is what the full `encode_offset_with_history` mirrors.
///
/// Per PARTITION, not per block: the emitter can write a partition raw, and
/// when it does it restores the history, so the partition after it must be
/// filled from the restored one. Filling here, just before each partition is
/// encoded, is what keeps that true.
fn fill_and_count<const FAST_REPCODE: bool>(
    raw_sequences: &mut [RawSequence],
    offset_hist: &mut [u32; 3],
    counts: SequenceCodeCounts<'_>,
    codes: &mut Vec<u32>,
) -> (usize, usize, usize) {
    let SequenceCodeCounts {
        ll: ll_counts,
        ml: ml_counts,
        of: of_counts,
    } = counts;
    // Written through the spare capacity rather than pushed: the length is
    // known, so a push's capacity test per sequence buys nothing, and resizing
    // first would zero the buffer only to overwrite all of it.
    codes.clear();
    codes.reserve(raw_sequences.len());
    let code_slots = &mut codes.spare_capacity_mut()[..raw_sequences.len()];
    // The history is rotated by every sequence and read by the next one. Held
    // behind the caller's reference it was three stores into the compressor per
    // sequence, because the loop also writes through the sequence slice and the
    // optimiser would not keep the array in registers across that. A local copy
    // written back once is the same three words, moved once.
    let mut hist = *offset_hist;
    for (slot, seq) in code_slots.iter_mut().zip(raw_sequences.iter_mut()) {
        let off_base = if FAST_REPCODE {
            encode_offset_with_history_fast(seq.off_base, seq.ll, &mut hist)
        } else {
            encode_offset_with_history(seq.off_base, seq.ll, &mut hist)
        };
        seq.off_base = off_base;
        let (ll_code, _, ll_bits) = encode_literal_length(seq.ll);
        let (ml_code, _, ml_bits) = encode_match_len(seq.ml);
        let (of_code, _, _) = encode_offset(off_base);
        slot.write(SequenceCode::pack(
            ll_code, ml_code, of_code, ll_bits, ml_bits,
        ));
        ll_counts[ll_code as usize] += 1;
        ml_counts[ml_code as usize] += 1;
        of_counts[of_code as usize] += 1;
    }
    *offset_hist = hist;
    // SAFETY: the loop wrote every one of the `raw_sequences.len()` slots it
    // took from the spare capacity, which `reserve` above guaranteed.
    unsafe {
        codes.set_len(raw_sequences.len());
    }
    (
        highest_used_code(ll_counts),
        highest_used_code(ml_counts),
        highest_used_code(of_counts),
    )
}

/// The highest code with a non-zero count, which the table selector needs and
/// would otherwise find by scanning all 256 slots.
///
/// Carried as a running maximum through the counting loop until it was three
/// compares a sequence there against one bounded scan a stream a block. The
/// bound is the format's: the three sequence alphabets end at 35, 52 and 31, so
/// nothing above 63 is ever counted.
fn highest_used_code(counts: &[usize; 256]) -> usize {
    debug_assert!(
        counts[64..].iter().all(|&count| count == 0),
        "a sequence code above 63 was counted; the alphabets end at 35 / 52 / 31",
    );
    counts[..64]
        .iter()
        .rposition(|&count| count != 0)
        .unwrap_or(0)
}

/// [`fill_and_count`] without the histogram, for the block-split estimator: it
/// prices sub-ranges repeatedly from a scratch history and counts them itself.
fn fill_wire_offsets(
    raw_sequences: &mut [RawSequence],
    offset_hist: &mut [u32; 3],
    fast_repcode: bool,
) {
    // Local copy for the same reason as `fill_and_count`.
    let mut hist = *offset_hist;
    if fast_repcode {
        for seq in raw_sequences.iter_mut() {
            seq.off_base = encode_offset_with_history_fast(seq.off_base, seq.ll, &mut hist);
        }
    } else {
        for seq in raw_sequences.iter_mut() {
            seq.off_base = encode_offset_with_history(seq.off_base, seq.ll, &mut hist);
        }
    }
    *offset_hist = hist;
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
        // G3 — whole-block bail-out before partition split. Upstream zstd
        // `ZSTD_compressSubBlock_multi` (`zstd_compress_superblock.c:530-532`)
        // bails when `estBlockSize > srcSize` (strict). Our trigger is
        // the `raw_fallback` flag from `estimate_subblock_size`, which
        // fires on the **stricter** `emitted_payload >= source_len -
        // min_gain` condition (where `min_gain = (source_len >> 8) + 2`,
        // ≈0.4% margin — see the `min_gain` computation inside
        // `estimate_subblock_size` above). So we bail in a narrow band
        // `[source_len - min_gain, source_len + 3]` where upstream zstd would
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
        self.derive_block_splits_with_full(start_idx, end_idx, full, entry, partitions);
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
            return post;
        }
        let mid_idx = (start_idx + end_idx) / 2;
        let (first, _, first_post) = self.estimate_subblock_size(start_idx, mid_idx, &entry);
        // Upstream zstd parity: score the right half from the left's post-state,
        // not from the parent's block-entry state. Without this propagation
        // `second` is evaluated as a fresh-block start, biasing the
        // `first + second < full` decision toward overly optimistic splits.
        let (second, _, _) = self.estimate_subblock_size(mid_idx, end_idx, &first_post);
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
    strategy: crate::encoding::strategy::StrategyTag,
    next: &'a mut Option<SharedFseTable>,
) -> FseTableMode<'a> {
    // Collect symbol distribution, tracking the highest code so the selector
    // skips the full-256 reverse scan (see `choose_table_from_counts`).
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    let mut max_symbol = 0usize;
    for symbol in data {
        let symbol = symbol as usize;
        counts[symbol] += 1;
        total += 1;
        max_symbol = max_symbol.max(symbol);
    }
    choose_table_from_counts(
        previous,
        default_table,
        &mut counts,
        total,
        max_symbol,
        max_log,
        strategy,
        // Estimator-only path (no emitted table): price the unadjusted histogram,
        // matching upstream's `ZSTD_NCountCost`.
        None,
        next,
    )
}

/// Same decision logic as [`choose_table`] but takes pre-computed
/// symbol counts and total directly. Hot-path callers in
/// `compress_literals_and_sequences` use this overload to avoid
/// re-iterating the sequence vec three times (one pass per
/// ll/ml/of stream); the iterator form is kept for the cost
/// estimator's call sites where the data is already in iterator
/// form.
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
    counts: &mut [usize; 256],
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

    // Fast-band preferRepeat (upstream zstd `ZSTD_selectEncodingType`,
    // `zstd_compress_sequences.c:179-204`): for fast/dfast/greedy with a
    // valid previous table and `< 1000` sequences, reuse it without building
    // a new one. Trades a negligible ratio loss for skipping the per-block
    // FSE table build + header descriptor — the dominant per-sub-block cost
    // when these cheap-match strategies split a block. The validity probe
    // (`fse_bit_cost` is finite) guarantees the previous table covers every
    // symbol in this block, so the reuse can never produce an invalid stream.
    if matches!(
        strategy,
        crate::encoding::strategy::StrategyTag::Fast
            | crate::encoding::strategy::StrategyTag::Dfast
            | crate::encoding::strategy::StrategyTag::Greedy
    ) && total < 1000
        && let Some(prev) = previous
        && let Some(table) = prev.as_table(default_table)
        && fse_bit_cost(counts, max_symbol, table).is_some()
    {
        return FseTableMode::RepeatLast(prev);
    }

    let use_low_prob_count = total >= 2048;

    // Mirror upstream zstd `ZSTD_selectEncodingType()`: compare default
    // cross-entropy, repeat-table FSE bit cost, and the custom compressed
    // table (header + entropy-bound payload). The custom table's header is
    // priced from its normalized counts via `fse_header_bits_for_counts`
    // WITHOUT building the (often-discarded) state tables — the build only
    // runs in the `Choice::New` arm below, when the custom table actually
    // wins. The estimate equals the built table's `table_header_bits()`
    // exactly, so the selection is byte-identical.
    let new_total_cost = (distinct_symbols > 1).then(|| {
        // Plain `+`: both are bit-cost estimates bounded by the block size
        // (<= MAX_BLOCK_SIZE * 8 bits), far under the integer's range.
        fse_header_bits_for_counts(&counts[..=max_symbol], max_log, use_low_prob_count)
            + entropy_cost(counts, max_symbol, total)
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
        // The custom table won the cost comparison — build it now (the only
        // place the state tables are constructed). `distinct_symbols > 1`
        // held when `new_total_cost` was computed, so the histogram has the
        // two-sample minimum `build_table_from_symbol_counts` requires.
        // The custom table won the cost comparison, so build it into the slot
        // it will occupy rather than on the stack.
        Some(Choice::New) => {
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
        None => {
            let fallback_counts = [counts[0], 0];
            build_into_slot(next, |dest| {
                if max_symbol == 0 {
                    // The builder needs at least two entries, so single-symbol
                    // streams use a phantom zero-count second slot here.
                    build_table_from_symbol_counts_into(
                        &fallback_counts,
                        max_log,
                        use_low_prob_count,
                        dest,
                    );
                } else {
                    build_table_from_symbol_counts_into(
                        &counts[..=max_symbol],
                        max_log,
                        use_low_prob_count,
                        dest,
                    );
                }
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
        && let Some(PreviousFseTable::Custom(handle)) = displaced
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
                Some(PreviousFseTable::Custom(old)) => Some(old),
                _ => None,
            };
            let built = core::mem::replace(next, outgoing)
                .expect("Encoded means the slot holds the table just built");
            *previous = Some(PreviousFseTable::Custom(built));
        }
    }
}

fn encode_sequences(
    sequences: &[RawSequence],
    // One packed [`SequenceCode`] per sequence, in the same order, from the
    // pass that derived the offset codes.
    codes: &[u32],
    writer: &mut BitWriter<&mut Vec<u8>>,
    ll_mode: &FseTableMode<'_>,
    ml_mode: &FseTableMode<'_>,
    of_mode: &FseTableMode<'_>,
    // The three predefined tables, in LL / ML / OF order. Passed rather than
    // read off `FseTables`, whose accessors borrow the whole struct while the
    // caller is holding its slots.
    defaults: [&FSETable; 3],
) {
    fn mode_table<'a>(mode: &'a FseTableMode<'_>, default: &'a FSETable) -> Option<&'a FSETable> {
        mode.as_table(default)
    }

    debug_assert_eq!(codes.len(), sequences.len());
    let sequence = sequences[sequences.len() - 1];
    let code = SequenceCode(codes[codes.len() - 1]);
    let (ll_code, ll_num_bits) = (code.ll_code(), code.ll_bits());
    let (ml_code, ml_num_bits) = (code.ml_code(), code.ml_bits());
    let (of_code, of_num_bits) = (code.of_code(), code.of_bits());
    let ll_add_bits = low_bits(sequence.ll, ll_num_bits);
    let ml_add_bits = low_bits(sequence.ml - 3, ml_num_bits);
    let of_add_bits = low_bits(sequence.off_base, of_num_bits);
    let [ll_default, ml_default, of_default] = defaults;
    let ll_table = mode_table(ll_mode, ll_default);
    let ml_table = mode_table(ml_mode, ml_default);
    let of_table = mode_table(of_mode, of_default);
    // Carried as bare indices, not `Option`s. A component has a state exactly
    // when it has a table, and the tables are resolved once above the loop, so
    // wrapping the state too meant re-asking the same question three times per
    // sequence and re-wrapping the answer. An absent component's index is never
    // read: every site that touches one is already inside its table's `Some`.
    let mut ll_state = ll_table.map_or(0, |table| table.start_state(ll_code).index);
    let mut ml_state = ml_table.map_or(0, |table| table.start_state(ml_code).index);
    let mut of_state = of_table.map_or(0, |table| table.start_state(of_code).index);

    writer.write_bits(ll_add_bits, ll_num_bits);
    writer.write_bits(ml_add_bits, ml_num_bits);
    writer.write_bits(of_add_bits, of_num_bits);

    // Upstream zstd-faithful sequence loop: write state diffs + extras via
    // unchecked fast-path adds with explicit `flush_bulk` calls at
    // safe burst boundaries. Per-sequence bit budget:
    //   state diffs: of (<=8) + ml (<=9) + ll (<=9) = 26 bits.
    //   extras:      ll (<=16) + ml (<=16) + of (<=31).
    //
    // One flush per sequence is the common case: the two inside the body are
    // conditional on the extras not fitting beside the diffs, which is
    // upstream's own accounting in `ZSTD_encodeSequences_body`
    // (`zstd_compress_sequences.c:303-367`), and the one at the end of the
    // body is what leaves the accumulator with under a byte for the next
    // round.
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
        // Walked as a slice rather than by index: the same order, without the
        // bounds check and the index arithmetic that `sequences[i]` pays on
        // every sequence. The last one is coded through the FSE init states
        // above, so it is not in this range.
        for (&sequence, &code) in sequences[..sequences.len() - 1]
            .iter()
            .zip(codes[..codes.len() - 1].iter())
            .rev()
        {
            let code = SequenceCode(code);
            let (ll_code, ll_num_bits) = (code.ll_code(), code.ll_bits());
            let (ml_code, ml_num_bits) = (code.ml_code(), code.ml_bits());
            let (of_code, of_num_bits) = (code.of_code(), code.of_bits());
            let ll_add_bits = low_bits(sequence.ll, ll_num_bits);
            let ml_add_bits = low_bits(sequence.ml - 3, ml_num_bits);
            let of_add_bits = low_bits(sequence.off_base, of_num_bits);

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
            //
            // What the three diffs actually put in the accumulator, tallied so
            // the budget below is checked against the widths the FSE tables
            // produced rather than against the ceiling its derivation assumed.
            #[cfg(debug_assertions)]
            let mut state_diff_bits = 0usize;
            if let Some(table) = of_table {
                let next = table.next_state(of_code, of_state);
                let diff = crate::fse::fse_encoder::transition_bits(of_state, next.num_bits);
                unsafe {
                    writer.write_bits_64_no_check(diff as u64, next.num_bits as usize);
                }
                #[cfg(debug_assertions)]
                {
                    state_diff_bits += next.num_bits as usize;
                }
                of_state = next.index;
            }
            if let Some(table) = ml_table {
                let next = table.next_state(ml_code, ml_state);
                let diff = crate::fse::fse_encoder::transition_bits(ml_state, next.num_bits);
                unsafe {
                    writer.write_bits_64_no_check(diff as u64, next.num_bits as usize);
                }
                #[cfg(debug_assertions)]
                {
                    state_diff_bits += next.num_bits as usize;
                }
                ml_state = next.index;
            }
            if let Some(table) = ll_table {
                let next = table.next_state(ll_code, ll_state);
                let diff = crate::fse::fse_encoder::transition_bits(ll_state, next.num_bits);
                unsafe {
                    writer.write_bits_64_no_check(diff as u64, next.num_bits as usize);
                }
                #[cfg(debug_assertions)]
                {
                    state_diff_bits += next.num_bits as usize;
                }
                ll_state = next.index;
            }
            // The three state diffs and the three extra-bit fields share one
            // 64-bit accumulator, and a flush here is only needed when the
            // extras that follow would not fit beside them. Upstream asks
            // exactly that question (`zstd_compress_sequences.c:350`):
            // `ofBits + mlBits + llBits >= 64 - 7 - (LLFSELog + MLFSELog +
            // OffFSELog)`, which with our accumulator logs (9 + 9 + 8 = 26) is
            // 31. Below it the accumulator holds at most 7 leftover + 26 diff
            // bits + 30 extra bits = 63, so nothing has to leave yet. Flushing
            // unconditionally instead cost a second store, length commit and
            // shift on every sequence, and a level-1 block has hundreds of
            // thousands of them.
            let extra_bits_total = of_num_bits + ml_num_bits + ll_num_bits;
            // The thresholds are arithmetic on widths, so they are only right
            // while the widths are what they were derived from. Pinned here,
            // where the arithmetic happens, so a widened encoder fails at its
            // cause rather than as an accumulator overflow further down or, in
            // a release build, as a corrupted stream with nothing to point at.
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
                // The bound the branch itself rests on: without a flush, the
                // leftover, the diffs and all three extra fields have to fit.
                debug_assert!(
                    extra_bits_total >= 31 || 7 + state_diff_bits + extra_bits_total <= 64,
                    "no flush at {extra_bits_total} extra bits, but 7 leftover + \
                     {state_diff_bits} diff bits + {extra_bits_total} would overflow the \
                     64-bit accumulator",
                );
            }
            if extra_bits_total >= 31 {
                unsafe {
                    writer.flush_bulk();
                }
            }

            // Extras burst: ll (≤16) + ml (≤16) + of (≤ 31). Whether the
            // accumulator was drained above or not, ll and ml fit: after a
            // flush it holds ≤ 7 + 32 = 39, and without one the three extras
            // together were under 31.
            //
            // SAFETY: `encode_literal_length` / `encode_match_len`
            // bound `*_num_bits ≤ 16` and return a clean `*_add_bits`
            // (low `num_bits` bits only). `encode_offset` bounds
            // `of_num_bits ≤ ilog2(of) ≤ 31`, and the two conditionals
            // (`>= 31` above, `> 56` below) are upstream's own accounting for
            // when the offset field still fits beside what is already there.
            unsafe {
                writer.write_bits_64_no_check(ll_add_bits as u64, ll_num_bits);
                writer.write_bits_64_no_check(ml_add_bits as u64, ml_num_bits);
            }
            // Upstream `zstd_compress_sequences.c:355`: with the accumulator
            // drained before the diffs, 7 leftover plus all three extra fields
            // must stay under 64, so the offset needs its own container only
            // past 56 bits of extras.
            if extra_bits_total > 56 {
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

/// Split a literal-length into its FSE symbol, the extra bits to append, and
/// how many of them (upstream `ZSTD_LLcode` plus the `LL_bits` lookup its
/// caller does).
///
/// Runs once per sequence inside the encoding loop, where a 22-arm range match
/// compiled to a chain of comparisons; upstream reaches the same answer with a
/// table index. Every code's baseline is a multiple of its own extra-bit width,
/// so masking off those bits is the same subtraction the ranges spelled out.
#[inline]
fn encode_literal_length(len: u32) -> (u8, u32, usize) {
    debug_assert!(len < 131_072, "literal length {len} out of encodable range");
    let code = if len < 64 {
        LL_CODE[len as usize]
    } else {
        (len.ilog2() + LL_DELTA_CODE) as u8
    };
    let bits = LL_EXTRA_BITS[code as usize] as usize;
    (code, len & ((1u32 << bits) - 1), bits)
}

/// Split a match length into its FSE symbol, the extra bits to append, and how
/// many of them (upstream `ZSTD_MLcode` plus the `ML_bits` lookup its caller
/// does). Codes are keyed on `len - 3`, the form the sequence section stores.
///
/// Table-driven for the same reason as [`encode_literal_length`].
#[inline]
fn encode_match_len(len: u32) -> (u8, u32, usize) {
    debug_assert!(
        (3..131_075).contains(&len),
        "match length {len} out of encodable range",
    );
    let base = len - 3;
    let code = if base < 128 {
        ML_CODE[base as usize]
    } else {
        (base.ilog2() + ML_DELTA_CODE) as u8
    };
    let bits = ML_EXTRA_BITS[code as usize] as usize;
    (code, base & ((1u32 << bits) - 1), bits)
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
pub(in crate::encoding) fn encode_offset_with_history_fast(
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
/// upstream zstd preferRepeat gate short-circuits the rebuild path.
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
/// the back must not be judged on the front alone. `counts` is cleared between
/// the probes so each reports its own most frequent symbol, which is what the
/// summed threshold is scaled for, and again on the way out so the caller
/// receives it zeroed either way.
fn end_samples_look_flat(literals: &[u8], counts: &mut [usize; 256]) -> bool {
    if literals.len() < SUSPECT_SAMPLE_SIZE * SUSPECT_SAMPLE_RATIO {
        return false;
    }
    counts.fill(0);
    let (_, head_largest) = crate::histogram::count_bytes(&literals[..SUSPECT_SAMPLE_SIZE], counts);
    counts.fill(0);
    let (_, tail_largest) =
        crate::histogram::count_bytes(&literals[literals.len() - SUSPECT_SAMPLE_SIZE..], counts);
    counts.fill(0);
    head_largest + tail_largest <= ((2 * SUSPECT_SAMPLE_SIZE) >> 7) + 4
}

fn compress_literals(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    writer: &mut BitWriter<&mut Vec<u8>>,
    strategy: crate::encoding::strategy::StrategyTag,
    huf_search: bool,
    weight_scratch: &mut huff0_encoder::WeightScratch,
    suspected_incompressible: bool,
) -> HuffmanTableUpdate {
    let reset_idx = writer.index();

    // Upstream zstd preferRepeat fast-path: when Fast/Dfast/Greedy on
    // <=1024-byte literals AND the prior table can encode this
    // input (`estimate_compressed_size` returns Some), skip the
    // expensive `HuffmanTable::build_from_data` and route the
    // emit straight through the reuse path. Mirrors upstream zstd's
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

    let mut counts = [0usize; 256];

    // Sample before committing to the full histogram (upstream
    // `huf_compress.c:1367`). The histogram spans the whole section, and for a
    // block the search found nothing in that section is the entire block — a
    // megabyte of random input costs a megabyte of counting only to be thrown
    // away by the flatness gate below. Two 4 KiB probes answer the same
    // question for 8 KiB of counting.
    if suspected_incompressible && end_samples_look_flat(literals, &mut counts) {
        raw_literals(literals, writer);
        return HuffmanTableUpdate::Cleared;
    }

    let (max_symbol, largest_count) = crate::histogram::count_bytes(literals, &mut counts);
    // Upstream zstd pre-build incompressibility gate (`huf_compress.c`,
    // `HUF_compress_internal`): a histogram this flat
    // (`largest <= (srcSize >> 7) + 4`) is heuristically not worth
    // compressing — bail to raw BEFORE the tree build and the full
    // `encode4x` pass. Without it, near-random literals paid histogram +
    // sort + tree + a full encode of the section only for the post-hoc
    // `use_raw_literal_fallback` below to throw it all away (~65% of
    // frame time on the random-payload dict scenarios). The single-symbol
    // case (`largest == srcSize`) never reaches here: the block emitter
    // routes all-identical sections to RLE first.
    if largest_count <= (literals.len() >> 7) + 4 {
        raw_literals(literals, writer);
        return HuffmanTableUpdate::Cleared;
    }

    // Mutable because the size query encodes the weight description into the
    // table's buffer; `write_table` below then reads it rather than repeating
    // the encode.
    let mut new_encoder_table = huff0_encoder::HuffmanTable::build_from_counts_gated_with(
        &counts[..=max_symbol],
        huf_search,
        weight_scratch,
    );

    let Some(new_table_description_size) = new_encoder_table.writeable_table_description_size()
    else {
        raw_literals(literals, writer);
        weight_scratch.recycle(new_encoder_table);
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
        &counts,
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
        HuffmanTableUpdate::Cleared
    } else if new_table {
        HuffmanTableUpdate::New(new_encoder_table)
    } else {
        // The previous table was kept, so this one is dead — same as above.
        weight_scratch.recycle(new_encoder_table);
        HuffmanTableUpdate::Reused
    }
}

#[cfg(test)]
mod tests;
