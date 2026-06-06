//! Utilities and interfaces for encoding an entire frame. Allows reusing resources

use alloc::vec::Vec;
use core::convert::TryInto;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

#[cfg(feature = "hash")]
use core::hash::Hasher;

use super::{
    CompressionLevel, Matcher, block_header::BlockHeader, frame_header::FrameHeader, levels::*,
    match_generator::MatchGeneratorDriver,
};
use crate::common::MAX_BLOCK_SIZE;
use crate::fse::fse_encoder::{FSETable, default_ll_table, default_ml_table, default_of_table};

use crate::io::{Read, Write};

/// A dictionary prepared for the ENCODER side, analogous to zstd's `CDict`
/// (vs the decoder's [`Dictionary`](crate::decoding::Dictionary) / `DDict`).
///
/// It carries the entropy tables, content, and repeat-offset history the
/// compressor needs, but is a distinct type with **no decode path**: there is
/// no way to turn it into a [`DictionaryHandle`](crate::decoding::DictionaryHandle)
/// or feed it to a [`FrameDecoder`](crate::decoding::FrameDecoder). That keeps
/// the compress-only state (which may have been parsed without building the
/// decode lookup tables, see
/// [`set_dictionary_from_bytes`](FrameCompressor::set_dictionary_from_bytes))
/// from ever reaching the decode side — the encoder/decoder dictionary split
/// mirrors C zstd's `CDict` / `DDict`.
pub struct EncoderDictionary {
    pub(crate) inner: crate::decoding::Dictionary,
}

impl EncoderDictionary {
    /// Wrap an already-parsed [`Dictionary`](crate::decoding::Dictionary) for
    /// encoder use. A fully-decoded dictionary is valid here; only the encoder
    /// entropy tables, content, and offset history are read.
    pub fn from_dictionary(dictionary: crate::decoding::Dictionary) -> Self {
        Self { inner: dictionary }
    }

    /// Parse a serialized dictionary blob for encoder use, skipping the decode
    /// lookup-table build the encoder never reads (see
    /// `Dictionary::decode_dict_for_encoding`). The encoder entropy tables — and
    /// thus the emitted frame — are identical to a full parse.
    pub fn from_bytes(
        raw_dictionary: &[u8],
    ) -> Result<Self, crate::decoding::errors::DictionaryDecodeError> {
        Ok(Self {
            inner: crate::decoding::Dictionary::decode_dict_for_encoding(raw_dictionary)?,
        })
    }

    /// The dictionary id.
    ///
    /// A dictionary attached for encoding always has a non-zero id (the
    /// `set_dictionary*` / `set_encoder_dictionary` attach path rejects a
    /// zero id). This getter, however, reflects the wrapped dictionary as-is:
    /// an `EncoderDictionary` built via [`Self::from_dictionary`] from a raw
    /// `Dictionary` with `id == 0` reports `0` here until it is attached.
    pub fn id(&self) -> u32 {
        self.inner.id
    }
}

/// An interface for compressing arbitrary data with the ZStandard compression algorithm.
///
/// `FrameCompressor` will generally be used by:
/// 1. Initializing a compressor by providing a buffer of data using `FrameCompressor::new()`
/// 2. Starting compression and writing that compression into a vec using `FrameCompressor::begin`
///
/// # Examples
/// ```
/// use structured_zstd::encoding::{FrameCompressor, CompressionLevel};
/// let mock_data: &[_] = &[0x1, 0x2, 0x3, 0x4];
/// let mut output = std::vec::Vec::new();
/// // Initialize a compressor.
/// let mut compressor = FrameCompressor::new(CompressionLevel::Uncompressed);
/// compressor.set_source(mock_data);
/// compressor.set_drain(&mut output);
///
/// // `compress` writes the compressed output into the provided buffer.
/// compressor.compress();
/// ```
pub struct FrameCompressor<
    R: Read = &'static [u8],
    W: Write = Vec<u8>,
    M: Matcher = MatchGeneratorDriver,
> {
    uncompressed_data: Option<R>,
    compressed_data: Option<W>,
    compression_level: CompressionLevel,
    dictionary: Option<EncoderDictionary>,
    dictionary_entropy_cache: Option<CachedDictionaryEntropy>,
    source_size_hint: Option<u64>,
    state: CompressState<M>,
    /// When true, emitted frames omit the 4-byte magic number prefix
    /// (`ZSTD_f_zstd1_magicless`). Default false. The caller is
    /// responsible for ensuring the decoder is configured for the
    /// matching format — wire-format only round-trips with a
    /// magicless-aware decoder.
    magicless: bool,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
    /// Block-layout introspection populated at the end of every
    /// successful `compress()`. `None` until the first call.
    /// Behind the `lsm` feature gate.
    #[cfg(feature = "lsm")]
    frame_emit_info: Option<crate::encoding::frame_emit_info::FrameEmitInfo>,
    /// When `true`, `compress()` XXH64-hashes each block's
    /// uncompressed bytes and appends the low-32-bit digest to
    /// `block_checksums`. Default `false` (zero cost). Gated on
    /// `all(lsm, hash)` because XXH64 lives behind the `hash`
    /// feature; an `lsm`-only build has no way to compute digests.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    per_block_checksums_enabled: bool,
    /// Per-block XXH64 (low 32 bits) digests captured during
    /// `compress()` when `per_block_checksums_enabled` is set. Ordered
    /// by block-emit order. `None` until the first call after enabling.
    /// Gated on `all(lsm, hash)` (see `per_block_checksums_enabled`).
    #[cfg(all(feature = "lsm", feature = "hash"))]
    block_checksums: Option<alloc::vec::Vec<u32>>,
    /// Per-physical-block decompressed (regenerated) sizes captured
    /// during `compress()`, in block-emit order (1:1 with
    /// `frame_emit_info.blocks`). Always captured under `lsm` (no
    /// opt-in, unlike `block_checksums`) because `FrameEmitInfo` is
    /// always built under `lsm` and `decompressed_byte_range` needs
    /// the per-block sizes. Cleared and refilled per frame.
    #[cfg(feature = "lsm")]
    block_decompressed_sizes: alloc::vec::Vec<u32>,
}

#[derive(Clone, Default)]
struct CachedDictionaryEntropy {
    huff: Option<crate::huff0::huff0_encoder::HuffmanTable>,
    ll_previous: Option<PreviousFseTable>,
    ml_previous: Option<PreviousFseTable>,
    of_previous: Option<PreviousFseTable>,
}

/// Shared owner for a custom "previous" FSE encoder table. `Arc` on
/// atomic-pointer targets, `Rc` otherwise (keeps `no_std` no-atomics
/// builds compiling, single-thread there anyway), mirroring
/// `decoding::dictionary::SharedDictionary`. Cloning the cached
/// dictionary entropy into the per-frame state is then a refcount bump,
/// not a full `FSETable` copy — the donor references `cdict->cBlockState`
/// instead of rebuilding it per frame.
#[cfg(target_has_atomic = "ptr")]
pub(crate) type SharedFseTable = alloc::sync::Arc<FSETable>;
#[cfg(not(target_has_atomic = "ptr"))]
pub(crate) type SharedFseTable = alloc::rc::Rc<FSETable>;

#[derive(Clone)]
pub(crate) enum PreviousFseTable {
    // Default tables are immutable and already stored alongside the state, so
    // repeating them only needs a lightweight marker instead of cloning FSETable.
    Default,
    // Shared handle: cloning (per-frame dictionary entropy seed) is a refcount
    // bump. The table is only ever read or REPLACED wholesale (a block that
    // builds a new table swaps in a fresh `SharedFseTable`), never mutated in
    // place, so sharing is sound.
    Custom(SharedFseTable),
    Rle(u8),
}

impl PreviousFseTable {
    pub(crate) fn as_table<'a>(&'a self, default: &'a FSETable) -> Option<&'a FSETable> {
        match self {
            Self::Default => Some(default),
            Self::Custom(table) => Some(table),
            Self::Rle(_) => None,
        }
    }
}

pub(crate) struct FseTables {
    /// The three predefined LL/ML/OF tables are functions of
    /// compile-time-constant distributions. The
    /// [`fse_encoder::FseDefaultTable`] type alias resolves to
    /// `&'static FSETable` when a process-wide cache is available
    /// (atomic-pointer targets, or no-atomic targets with the
    /// `critical-section` feature) and to `Box<FSETable>` on the
    /// cache-less no-atomic path (one per-frame allocation, dropped
    /// with the compressor — no `Box::leak`, no unbounded growth).
    /// Both arms `Deref` to `FSETable`, so consumers in
    /// `encoding/blocks/compressed.rs` borrow through `&` uniformly
    /// without seeing the per-target divergence.
    pub(crate) ll_default: crate::fse::fse_encoder::FseDefaultTable,
    pub(crate) ll_previous: Option<PreviousFseTable>,
    pub(crate) ml_default: crate::fse::fse_encoder::FseDefaultTable,
    pub(crate) ml_previous: Option<PreviousFseTable>,
    pub(crate) of_default: crate::fse::fse_encoder::FseDefaultTable,
    pub(crate) of_previous: Option<PreviousFseTable>,
}

impl FseTables {
    pub fn new() -> Self {
        Self {
            ll_default: default_ll_table(),
            ll_previous: None,
            ml_default: default_ml_table(),
            ml_previous: None,
            of_default: default_of_table(),
            of_previous: None,
        }
    }

    /// Borrow the LL default table as `&FSETable`. Abstracts the cfg
    /// split in [`crate::fse::fse_encoder::FseDefaultTable`] —
    /// `&'static FSETable` (atomic / `critical-section`) auto-derefs
    /// directly; `Box<FSETable>` (cache-less no-atomic) derefs
    /// through `Box`. Both arms yield `&FSETable` uniformly so
    /// downstream consumers can stay cfg-agnostic.
    #[inline]
    #[allow(clippy::borrow_deref_ref)]
    pub(crate) fn ll_default_ref(&self) -> &FSETable {
        &*self.ll_default
    }

    /// Borrow the ML default table as `&FSETable`. See [`Self::ll_default_ref`].
    #[inline]
    #[allow(clippy::borrow_deref_ref)]
    pub(crate) fn ml_default_ref(&self) -> &FSETable {
        &*self.ml_default
    }

    /// Borrow the OF default table as `&FSETable`. See [`Self::ll_default_ref`].
    #[inline]
    #[allow(clippy::borrow_deref_ref)]
    pub(crate) fn of_default_ref(&self) -> &FSETable {
        &*self.of_default
    }
}

const PRESPLIT_BLOCK_MIN: usize = 3500;
const PRESPLIT_THRESHOLD_PENALTY_RATE: u64 = 16;
const PRESPLIT_THRESHOLD_BASE: u64 = PRESPLIT_THRESHOLD_PENALTY_RATE - 2;
const PRESPLIT_THRESHOLD_PENALTY: i32 = 3;
const PRESPLIT_CHUNK_SIZE: usize = 8 << 10;
const PRESPLIT_HASH_LOG_MAX: usize = 10;
const PRESPLIT_HASH_TABLE_SIZE: usize = 1 << PRESPLIT_HASH_LOG_MAX;
const PRESPLIT_KNUTH: u32 = 0x9E37_79B9;
/// Donor `SEGMENT_SIZE` in `ZSTD_splitBlock_fromBorders` (`zstd_preSplit.c:201`).
/// Two `SEGMENT_SIZE`-byte fingerprints — one from the start, one from the end —
/// drive the cheap border heuristic; a third one from the middle disambiguates
/// where in the block the transition sits.
const PRESPLIT_BORDERS_SEGMENT: usize = 512;

#[derive(Clone)]
struct PreSplitFingerprint {
    events: [u32; PRESPLIT_HASH_TABLE_SIZE],
    nb_events: usize,
}

impl Default for PreSplitFingerprint {
    fn default() -> Self {
        Self {
            events: [0; PRESPLIT_HASH_TABLE_SIZE],
            nb_events: 0,
        }
    }
}

fn presplit_hash2(bytes: &[u8], hash_log: usize) -> usize {
    debug_assert!(hash_log >= 8);
    if hash_log == 8 {
        return bytes[0] as usize;
    }
    debug_assert!(hash_log <= PRESPLIT_HASH_LOG_MAX);
    let value = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
    (value.wrapping_mul(PRESPLIT_KNUTH) >> (32 - hash_log)) as usize
}

fn presplit_record_fingerprint(
    fp: &mut PreSplitFingerprint,
    src: &[u8],
    sampling_rate: usize,
    hash_log: usize,
) {
    fp.events.fill(0);
    fp.nb_events = 0;
    if src.len() < 2 {
        return;
    }
    let limit = src.len() - 1;
    let mut n = 0usize;
    while n < limit {
        fp.events[presplit_hash2(&src[n..], hash_log)] += 1;
        n += sampling_rate;
    }
    // Donor parity: zstd_preSplit.c records the integer division, not the
    // rounded-up number of sampled events from the loop above.
    fp.nb_events += limit / sampling_rate;
}

/// Single-byte histogram pass — matches donor `HIST_add` over a small
/// segment with `hashLog == 8` (the `hash2` shortcut at
/// `zstd_preSplit.c:36` returns the raw byte). The byChunks path uses
/// 2-byte hashing for `hashLog >= 9`; this helper exists so the borders
/// heuristic doesn't pay for that wider hash on its 512-byte windows.
fn presplit_record_byte_histogram(fp: &mut PreSplitFingerprint, src: &[u8]) {
    fp.events.fill(0);
    for &b in src {
        fp.events[b as usize] += 1;
    }
    // Donor `HIST_add` returns the maximum symbol; the caller then sets
    // `nbEvents = SEGMENT_SIZE` explicitly (see `zstd_preSplit.c:213`).
    fp.nb_events = src.len();
}

fn presplit_distance(lhs: &PreSplitFingerprint, rhs: &PreSplitFingerprint, hash_log: usize) -> u64 {
    let slots = 1usize << hash_log;
    let mut distance = 0u64;
    for idx in 0..slots {
        let left = lhs.events[idx] as i128 * rhs.nb_events as i128;
        let right = rhs.events[idx] as i128 * lhs.nb_events as i128;
        distance = distance.saturating_add(left.abs_diff(right) as u64);
    }
    distance
}

fn presplit_fingerprints_differ(
    reference: &PreSplitFingerprint,
    new_fp: &PreSplitFingerprint,
    penalty: i32,
    hash_log: usize,
) -> bool {
    debug_assert!(reference.nb_events > 0);
    debug_assert!(new_fp.nb_events > 0);
    let p50 = reference.nb_events as u64 * new_fp.nb_events as u64;
    let deviation = presplit_distance(reference, new_fp, hash_log);
    let threshold = p50.saturating_mul(PRESPLIT_THRESHOLD_BASE + penalty as u64)
        / PRESPLIT_THRESHOLD_PENALTY_RATE;
    deviation >= threshold
}

fn presplit_merge_events(acc: &mut PreSplitFingerprint, new_fp: &PreSplitFingerprint) {
    for idx in 0..PRESPLIT_HASH_TABLE_SIZE {
        acc.events[idx] = acc.events[idx].saturating_add(new_fp.events[idx]);
    }
    acc.nb_events = acc.nb_events.saturating_add(new_fp.nb_events);
}

fn split_block_by_chunks(block: &[u8], level: usize) -> usize {
    debug_assert_eq!(block.len(), MAX_BLOCK_SIZE as usize);
    debug_assert!((1..=4).contains(&level));
    let (sampling_rate, hash_log) = match level - 1 {
        0 => (43, 8),
        1 => (11, 9),
        2 => (5, 10),
        _ => (1, 10),
    };

    let mut past = PreSplitFingerprint::default();
    let mut new_events = PreSplitFingerprint::default();
    let mut penalty = PRESPLIT_THRESHOLD_PENALTY;
    presplit_record_fingerprint(
        &mut past,
        &block[..PRESPLIT_CHUNK_SIZE],
        sampling_rate,
        hash_log,
    );
    let mut pos = PRESPLIT_CHUNK_SIZE;
    while pos <= block.len() - PRESPLIT_CHUNK_SIZE {
        presplit_record_fingerprint(
            &mut new_events,
            &block[pos..pos + PRESPLIT_CHUNK_SIZE],
            sampling_rate,
            hash_log,
        );
        if presplit_fingerprints_differ(&past, &new_events, penalty, hash_log) {
            return pos;
        }
        presplit_merge_events(&mut past, &new_events);
        if penalty > 0 {
            penalty -= 1;
        }
        pos += PRESPLIT_CHUNK_SIZE;
    }
    block.len()
}

/// Donor port of `ZSTD_splitBlock_fromBorders` (`zstd_preSplit.c:198`).
/// Records two 512-byte byte-histograms — one from each end of a 128 KB
/// block — and a third from the middle as a tie-breaker; returns either
/// a quantised split point (32 KB / 64 KB / 96 KB) or the full block
/// size when the two ends look indistinguishable. Cheaper than the
/// chunk-based path because it touches at most 1.5 KB of input
/// regardless of block size.
fn split_block_from_borders(block: &[u8]) -> usize {
    debug_assert_eq!(block.len(), MAX_BLOCK_SIZE as usize);
    let block_size = block.len();
    let mut past = PreSplitFingerprint::default();
    let mut new_fp = PreSplitFingerprint::default();
    presplit_record_byte_histogram(&mut past, &block[..PRESPLIT_BORDERS_SEGMENT]);
    presplit_record_byte_histogram(&mut new_fp, &block[block_size - PRESPLIT_BORDERS_SEGMENT..]);
    // Donor uses `penalty = 0, hash_log = 8` — i.e. raw byte histogram
    // distance with no threshold padding (`zstd_preSplit.c:214`).
    if !presplit_fingerprints_differ(&past, &new_fp, 0, 8) {
        return block_size;
    }

    let mut middle = PreSplitFingerprint::default();
    let mid_start = block_size / 2 - PRESPLIT_BORDERS_SEGMENT / 2;
    presplit_record_byte_histogram(
        &mut middle,
        &block[mid_start..mid_start + PRESPLIT_BORDERS_SEGMENT],
    );

    let dist_from_begin = presplit_distance(&past, &middle, 8);
    let dist_from_end = presplit_distance(&new_fp, &middle, 8);
    // Donor `SEGMENT_SIZE * SEGMENT_SIZE / 3` (`zstd_preSplit.c:221`):
    // if the middle is roughly equidistant from both ends, the change
    // sits near the centre — split at the midpoint.
    let min_distance = (PRESPLIT_BORDERS_SEGMENT as u64) * (PRESPLIT_BORDERS_SEGMENT as u64) / 3;
    if dist_from_begin.abs_diff(dist_from_end) < min_distance {
        return 64 * 1024;
    }
    // Larger `dist_from_begin` (i.e. `middle` farther from the head
    // fingerprint, equivalently closer to the tail) means the new
    // statistics already dominate the centre — the transition
    // happened EARLY → emit a small 32 KB head and let the 96 KB
    // tail absorb the rest. Inverse case: `dist_from_end` larger
    // (middle still resembles the head) means the transition is
    // LATE → emit a 96 KB head so the trailing 32 KB carries the
    // new statistics alone.
    if dist_from_begin > dist_from_end {
        32 * 1024
    } else {
        96 * 1024
    }
}

/// XXH64 (low 32 bits, seed 0) over `data`. Shared helper for the
/// per-physical-block checksum sidecar so encoder and decoder hash
/// the exact same byte ranges with the exact same parameters. Gated
/// at `all(lsm, hash)` because the only consumer is the lsm-side
/// `block_checksums` sidecar; non-lsm builds carry no reference to
/// this helper at all.
#[cfg(all(feature = "lsm", feature = "hash"))]
#[inline]
pub(crate) fn xxh64_block_low32(data: &[u8]) -> u32 {
    let mut h = XxHash64::with_seed(0);
    h.write(data);
    h.finish() as u32
}

/// Bench-only entry point for the donor-parity comparator test in
/// `tests/block_splitter_donor_parity.rs`. Dispatches to the same
/// `_from_borders` (split_level == 0) / `_by_chunks` (split_level ∈
/// 1..=4) ports that `optimal_block_size` itself routes
/// through. Caller is responsible for passing exactly
/// `MAX_BLOCK_SIZE` bytes (per donor `ZSTD_splitBlock` contract —
/// "@blockSize must be == 128 KB" in `zstd_preSplit.h`).
#[cfg(feature = "bench_internals")]
pub(crate) fn block_splitter_decision_for_bench(block: &[u8], split_level: usize) -> usize {
    assert_eq!(
        block.len(),
        MAX_BLOCK_SIZE as usize,
        "block_splitter_decision_for_bench expects exactly MAX_BLOCK_SIZE bytes"
    );
    assert!(
        split_level <= 4,
        "block_splitter_decision_for_bench: split_level must be in 0..=4, got {split_level}"
    );
    if split_level == 0 {
        split_block_from_borders(block)
    } else {
        split_block_by_chunks(block, split_level)
    }
}

pub(crate) fn optimal_block_size(
    level: CompressionLevel,
    block: &[u8],
    remaining_src_size: usize,
    block_size_max: usize,
    savings: i64,
) -> usize {
    let Some(split_level) = crate::encoding::match_generator::level_pre_split(level) else {
        return remaining_src_size.min(block_size_max);
    };
    if remaining_src_size < MAX_BLOCK_SIZE as usize || block_size_max < MAX_BLOCK_SIZE as usize {
        return remaining_src_size.min(block_size_max);
    }
    if savings < 3 {
        return MAX_BLOCK_SIZE as usize;
    }
    if block.len() < MAX_BLOCK_SIZE as usize {
        return remaining_src_size.min(block_size_max);
    }
    // Donor `ZSTD_splitBlock` dispatch (`zstd_preSplit.c:234`):
    // `split_level == 0` → cheap borders heuristic;
    // `split_level == 1..=4` → byChunks with internal sampling level
    // `split_level - 1`.
    let raw_split = if split_level == 0 {
        split_block_from_borders(&block[..MAX_BLOCK_SIZE as usize])
    } else {
        split_block_by_chunks(&block[..MAX_BLOCK_SIZE as usize], split_level)
    };
    raw_split
        .max(PRESPLIT_BLOCK_MIN)
        .min(MAX_BLOCK_SIZE as usize)
}

pub(crate) struct CompressState<M: Matcher> {
    pub(crate) matcher: M,
    pub(crate) last_huff_table: Option<crate::huff0::huff0_encoder::HuffmanTable>,
    pub(crate) fse_tables: FseTables,
    pub(crate) block_scratch: crate::encoding::blocks::CompressedBlockScratch,
    /// Offset history for repeat offset encoding: [rep0, rep1, rep2].
    /// Initialized to [1, 4, 8] per RFC 8878 §3.1.2.5.
    pub(crate) offset_hist: [u32; 3],
    /// Strategy tag resolved from the current `CompressionLevel` at every
    /// `matcher.reset()` call. Used by the literal-compression gates
    /// (`min_literals_to_compress`, `min_gain`) in
    /// `encoding::blocks::compressed` to mirror donor's strategy-aware
    /// thresholds (`zstd_compress_literals.c:114-127, 187-188`).
    ///
    /// **Invariant (required of every construction site):** must be
    /// initialized from the active `CompressionLevel` via
    /// `StrategyTag::for_compression_level`, and re-synced from the
    /// active level alongside every `matcher.reset()` call so the
    /// level-aware gates stay correct after a level change. The two
    /// reset sites that own this sync are `FrameCompressor::compress`
    /// and `StreamingEncoder::ensure_frame_started`. There is no
    /// `Default` impl — production constructors
    /// (`FrameCompressor::new`, `new_with_matcher`, the streaming
    /// encoder constructor) plumb this explicitly. Tests that build
    /// `CompressState` by hand must also supply a value.
    pub(crate) strategy_tag: crate::encoding::strategy::StrategyTag,
}

/// Per-frame setup resolved once by [`FrameCompressor::prepare_frame`] and
/// consumed by the block loop + [`FrameCompressor::finish_frame`]. Lets the
/// owned `compress()` and the borrowed one-shot path share identical
/// reset / dict-prime / entropy-seed setup and frame-tail emission.
struct FramePrep {
    window_size: u64,
    use_dictionary_state: bool,
    source_size_hint_known: bool,
    initial_size_hint: Option<u64>,
}

/// Initial capacity for the `all_blocks` accumulator, by source-size hint.
/// The frame header is written only after all input is read (so
/// Frame_Content_Size is known), so compressed blocks accumulate in memory
/// first. Seed-size tiers (mirrors donor `ZSTD_CStreamOutSize` naming):
/// - tiny (`<= 4 KiB` hint): payload-bound seed, `>=` anything a tiny input's
///   compressed output could need.
/// - small (`<= 64 KiB` hint): absorbs one or two `Vec::extend` doublings
///   without over-allocating.
/// - default (one donor block, `130 KiB`): the value the rest of the encoder
///   is sized around; larger inputs amortise the first doublings cheaply and
///   the residue is dominated by internal `compress_block_encoded` buffers.
///
/// Shared by the owned (`run_owned_block_loop`) and borrowed
/// (`run_borrowed_block_loop`) paths so the tier table can't drift between them.
fn initial_all_blocks_cap(initial_size_hint: Option<u64>) -> usize {
    const TINY_THRESHOLD: u64 = 4 * 1024;
    const SMALL_THRESHOLD: u64 = 64 * 1024;
    const TINY_CAP: usize = 4 * 1024;
    const SMALL_CAP: usize = 16 * 1024;
    const DEFAULT_CAP: usize = 130 * 1024;
    match initial_size_hint {
        Some(h) if h <= TINY_THRESHOLD => TINY_CAP,
        Some(h) if h <= SMALL_THRESHOLD => SMALL_CAP,
        _ => DEFAULT_CAP,
    }
}

impl<R: Read, W: Write> FrameCompressor<R, W, MatchGeneratorDriver> {
    /// Create a new `FrameCompressor`
    pub fn new(compression_level: CompressionLevel) -> Self {
        Self {
            uncompressed_data: None,
            compressed_data: None,
            compression_level,
            dictionary: None,
            dictionary_entropy_cache: None,
            source_size_hint: None,
            state: CompressState {
                matcher: MatchGeneratorDriver::new(1024 * 128, 1),
                last_huff_table: None,
                fse_tables: FseTables::new(),
                block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: crate::encoding::strategy::StrategyTag::for_compression_level(
                    compression_level,
                ),
            },
            magicless: false,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
            #[cfg(feature = "lsm")]
            frame_emit_info: None,
            #[cfg(all(feature = "lsm", feature = "hash"))]
            per_block_checksums_enabled: false,
            #[cfg(all(feature = "lsm", feature = "hash"))]
            block_checksums: None,
            #[cfg(feature = "lsm")]
            block_decompressed_sizes: alloc::vec::Vec::new(),
        }
    }

    /// Whether the borrowed (no per-block history copy) one-shot loop is
    /// valid for an `input_len`-byte slice under the resolved `prep`.
    ///
    /// `Uncompressed` resolves to `StrategyTag::Fast` but must emit stored
    /// Raw blocks, which the borrowed loop's
    /// `compress_block_encoded_borrowed` (RLE/raw-fast/compressed) does NOT
    /// do, so exclude it; it then takes the owned path's dedicated
    /// Uncompressed arm.
    ///
    /// No window-size gate: over-window inputs are handled too. The owned
    /// path bounds matches to the last `advertised_window` bytes via
    /// `window_low` and evicts/rehashes its history; the borrowed path
    /// computes the identical `window_low = block_end - advertised_window`
    /// and the kernel rejects any hash candidate below it, while the
    /// per-position `put` during the scan keeps in-window slots current,
    /// so it produces byte-identical output to the owned (evicting) path
    /// without ever copying the input into `history`, even when the input
    /// far exceeds the window.
    ///
    /// BUT gate on `input_len <= u32::MAX`: the Fast kernel stores ABSOLUTE
    /// positions in a `u32` hash table, and the borrowed scan walks
    /// absolute input offsets up to `block_end == input.len()`. Past 4 GiB
    /// those offsets truncate / overflow the `u32` position math
    /// (`base_off + ip0 as u32`, `window_low`), panicking or corrupting.
    /// The owned/evicting path keeps the scanned window bounded (positions
    /// stay small), so >4 GiB inputs fall back to it.
    fn borrowed_eligible(&self, input_len: usize, prep: &FramePrep) -> bool {
        use crate::encoding::strategy::StrategyTag;
        !prep.use_dictionary_state
            && !matches!(self.compression_level, CompressionLevel::Uncompressed)
            && self.state.strategy_tag == StrategyTag::Fast
            && input_len <= u32::MAX as usize
    }

    /// Compress `input` as one frame's worth of blocks: the borrowed
    /// in-place loop when [`Self::borrowed_eligible`], else the owned
    /// (history-copying) loop fed an in-place `&[u8]` cursor. Returns
    /// `(all_blocks, total_uncompressed)`; the caller emits the frame tail
    /// (`finish_frame` for a configured drain, `write_frame_to_vec` for a
    /// returned buffer).
    fn run_one_frame(&mut self, input: &[u8], prep: &FramePrep) -> (Vec<u8>, u64) {
        if self.borrowed_eligible(input.len(), prep) {
            self.run_borrowed_block_loop(input, prep.initial_size_hint)
        } else {
            let mut cursor: &[u8] = input;
            self.run_owned_block_loop(&mut cursor, prep.initial_size_hint)
        }
    }

    /// Compress one contiguous `&[u8]` as a single independent Zstd frame,
    /// writing the frame bytes into `out` (its previous contents are
    /// replaced and its allocation reused), reusing this compressor's heavy
    /// state across calls.
    ///
    /// This is the reusable-compression-context (CCtx-equivalent) entry
    /// point, mirroring C `ZSTD_compress2` over a reused `ZSTD_CCtx`:
    /// construct ONE `FrameCompressor` and call this in a loop to emit N
    /// independent, self-describing frames (each carrying its own header,
    /// blocks, and checksum, decodable in isolation, with no cross-frame
    /// match history). Every call resets the per-frame state via
    /// [`Self::prepare_frame`]: only the allocations are kept, so the
    /// dominant per-frame setup cost (table allocation + dictionary prime)
    /// is paid once instead of N times. Passing the same `out` buffer each
    /// call additionally reuses the output allocation, matching C's
    /// caller-owned `dst` buffer (no per-frame output allocation).
    ///
    /// Reusing the context + `out` across many small frames (the typical
    /// per-block-frame workload) is far cheaper than a fresh
    /// [`compress_slice_to_vec`](crate::encoding::compress_slice_to_vec)
    /// per block, which allocates and primes from scratch each time.
    ///
    /// The input is read in place: no [`Self::set_source`] /
    /// [`Self::set_drain`] setup is required, and the input lifetime is not
    /// baked into the compressor type, so successive calls may pass slices
    /// with unrelated lifetimes. When the Fast (Simple) backend is active
    /// and no dictionary is set, the matcher references the input directly
    /// (no per-block history copy); other backends / dictionary use copy
    /// each block into history exactly as the streaming
    /// [`compress`](Self::compress) path does. The source-size hint is
    /// derived from the input length on every call, so per-frame table
    /// sizing tracks each frame's actual size regardless of any earlier
    /// hint.
    ///
    /// A sticky dictionary set via
    /// [`set_dictionary`](Self::set_dictionary) (or its variants) is primed
    /// into every frame, mirroring `ZSTD_CCtx_loadDictionary` /
    /// `ZSTD_CCtx_refCDict`.
    ///
    /// # Panics
    ///
    /// Panics on encoder error, matching [`Self::compress`] and
    /// [`compress_slice_to_vec`](crate::encoding::compress_slice_to_vec).
    pub fn compress_independent_frame_into(&mut self, input: &[u8], out: &mut Vec<u8>) {
        // Size the next frame from the actual payload, not a stale hint a
        // previous call may have left behind (a wrong hint would change the
        // resolved window/header and could flip borrowed eligibility).
        self.source_size_hint = Some(input.len() as u64);
        let prep = self.prepare_frame();
        let (all_blocks, total_uncompressed) = self.run_one_frame(input, &prep);
        self.write_frame_to_vec(out, all_blocks, total_uncompressed, &prep);
    }

    /// Convenience wrapper over [`Self::compress_independent_frame_into`]
    /// that allocates and returns a fresh `Vec` per call. Prefer the
    /// `_into` form in tight per-block-frame loops to reuse one output
    /// buffer across frames (the CCtx-equivalent zero-per-call-alloc
    /// output, matching C's caller-owned `dst`).
    ///
    /// ```rust
    /// use structured_zstd::encoding::{FrameCompressor, CompressionLevel};
    /// let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    /// let frame_a = cctx.compress_independent_frame(b"first block payload");
    /// let frame_b = cctx.compress_independent_frame(b"second block payload");
    /// assert!(!frame_a.is_empty() && !frame_b.is_empty());
    /// ```
    pub fn compress_independent_frame(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.compress_independent_frame_into(input, &mut out);
        out
    }

    /// Borrowed one-shot block loop: walks `input` in `MAX_BLOCK_SIZE`
    /// strides (the Fast backend never pre-splits, so boundaries match the
    /// owned loop), scanning each block range in place against the
    /// borrowed window via `compress_block_encoded_borrowed` — no
    /// per-block `commit_space` copy. Returns `(all_blocks,
    /// total_uncompressed)`. Caller guarantees Fast backend + no
    /// dictionary; over-window inputs are fine (matches are bounded by
    /// `window_low` exactly as the owned evicting path).
    fn run_borrowed_block_loop(
        &mut self,
        input: &[u8],
        initial_size_hint: Option<u64>,
    ) -> (Vec<u8>, u64) {
        let mut all_blocks: Vec<u8> = Vec::with_capacity(initial_all_blocks_cap(initial_size_hint));
        let total_uncompressed = input.len() as u64;
        // Empty input: emit a single empty last Raw block (mirrors the
        // owned loop's empty-file special case).
        if input.is_empty() {
            let header = BlockHeader {
                last_block: true,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size: 0,
            };
            header.serialize(&mut all_blocks);
            #[cfg(feature = "lsm")]
            self.block_decompressed_sizes.push(0);
            #[cfg(all(feature = "lsm", feature = "hash"))]
            if let Some(checksums) = self.block_checksums.as_mut() {
                checksums.push(xxh64_block_low32(&[]));
            }
            return (all_blocks, total_uncompressed);
        }
        // SAFETY: `input` outlives this call (held by the caller across
        // the call) and is not mutated. Only the Simple backend is active
        // (gated by `compress_oneshot_borrowed`).
        unsafe {
            self.state.matcher.set_borrowed_window(input);
        }
        // Panic-safety: clear the borrowed `(ptr, len)` on EVERY exit,
        // including an unwind from an `assert!` inside the block loop, so
        // a caught-and-reused compressor never retains a dangling window.
        // (The next frame's `reset()` also clears it before any read, but
        // this guard makes the invariant local and unwind-proof.)
        struct ClearBorrowedOnDrop(*mut MatchGeneratorDriver);
        impl Drop for ClearBorrowedOnDrop {
            fn drop(&mut self) {
                // SAFETY: at drop (normal return or unwind) the loop's
                // borrows of the matcher have ended, so this is the only
                // access. `addr_of_mut!` produced this pointer without an
                // intermediate `&mut`, so the interleaved `&mut` uses in
                // the loop did not invalidate it.
                unsafe { (*self.0).clear_borrowed_window() };
            }
        }
        let _clear_guard = ClearBorrowedOnDrop(core::ptr::addr_of_mut!(self.state.matcher));
        let block_capacity = MAX_BLOCK_SIZE as usize;
        let mut start = 0usize;
        while start < input.len() {
            // Donor `ZSTD_compress_frameChunk`: size each block via the cheap
            // fingerprint pre-splitter so a full 128 KiB block is cut at a
            // statistical boundary when it pays. `savings = consumed -
            // produced` mirrors the donor gate (the first block and
            // incompressible input keep the full 128 KiB). The borrowed window
            // already spans the whole input, so a smaller block is just a
            // narrower `(block_start, block_end)` range into it.
            let savings = start as i64 - all_blocks.len() as i64;
            let block_len = optimal_block_size(
                self.compression_level,
                &input[start..],
                input.len() - start,
                block_capacity,
                savings,
            );
            let end = (start + block_len).min(input.len());
            let block = &input[start..end];
            let last_block = end == input.len();
            #[cfg(feature = "hash")]
            self.hasher.write(block);
            crate::encoding::levels::compress_block_encoded_borrowed(
                &mut self.state,
                self.compression_level,
                last_block,
                block,
                start,
                end,
                &mut all_blocks,
                #[cfg(feature = "lsm")]
                Some(&mut self.block_decompressed_sizes),
                #[cfg(all(feature = "lsm", feature = "hash"))]
                self.block_checksums.as_mut(),
            );
            start = end;
        }
        // `_clear_guard` drops here, clearing the borrowed window.
        (all_blocks, total_uncompressed)
    }
}

impl<R: Read, W: Write, M: Matcher> FrameCompressor<R, W, M> {
    /// Create a new `FrameCompressor` with a custom matching algorithm implementation
    pub fn new_with_matcher(matcher: M, compression_level: CompressionLevel) -> Self {
        Self {
            uncompressed_data: None,
            compressed_data: None,
            dictionary: None,
            dictionary_entropy_cache: None,
            source_size_hint: None,
            state: CompressState {
                matcher,
                last_huff_table: None,
                fse_tables: FseTables::new(),
                block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: crate::encoding::strategy::StrategyTag::for_compression_level(
                    compression_level,
                ),
            },
            compression_level,
            magicless: false,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
            #[cfg(feature = "lsm")]
            frame_emit_info: None,
            #[cfg(all(feature = "lsm", feature = "hash"))]
            per_block_checksums_enabled: false,
            #[cfg(all(feature = "lsm", feature = "hash"))]
            block_checksums: None,
            #[cfg(feature = "lsm")]
            block_decompressed_sizes: alloc::vec::Vec::new(),
        }
    }

    /// Enable or disable magicless frame format (`ZSTD_f_zstd1_magicless`).
    ///
    /// When set to `true`, emitted frames omit the 4-byte magic number
    /// prefix. The matching decoder must be configured to expect a
    /// magicless stream — wire-format only round-trips with a
    /// magicless-aware decoder.
    pub fn set_magicless(&mut self, magicless: bool) {
        self.magicless = magicless;
    }

    /// Before calling [FrameCompressor::compress] you need to set the source.
    ///
    /// This is the data that is compressed and written into the drain.
    pub fn set_source(&mut self, uncompressed_data: R) -> Option<R> {
        self.uncompressed_data.replace(uncompressed_data)
    }

    /// Before calling [FrameCompressor::compress] you need to set the drain.
    ///
    /// As the compressor compresses data, the drain serves as a place for the output to be writte.
    pub fn set_drain(&mut self, compressed_data: W) -> Option<W> {
        self.compressed_data.replace(compressed_data)
    }

    /// Provide a hint about the total uncompressed size for the next frame.
    ///
    /// When set, the encoder selects smaller hash tables and windows for
    /// small inputs, matching the C zstd source-size-class behavior.
    ///
    /// This hint applies only to frame payload bytes (`size`). Dictionary
    /// history is primed separately and does not inflate the hinted size or
    /// advertised frame window.
    /// Must be called before [`compress`](Self::compress).
    pub fn set_source_size_hint(&mut self, size: u64) {
        self.source_size_hint = Some(size);
    }

    /// Compress the uncompressed data from the provided source as one Zstd frame and write it to the provided drain
    ///
    /// This will repeatedly call [Read::read] on the source to fill up blocks until the source returns 0 on the read call.
    /// All compressed blocks are buffered in memory so that the frame header can include the
    /// `Frame_Content_Size` field (which requires knowing the total uncompressed size). The
    /// entire frame — header, blocks, and optional checksum — is then written to the drain
    /// at the end. This means peak memory usage is O(compressed_size).
    ///
    /// To avoid endlessly encoding from a potentially endless source (like a network socket) you can use the
    /// [Read::take] function
    /// Per-frame setup values resolved by [`Self::prepare_frame`] and
    /// consumed by the block loop + [`Self::finish_frame`]. Lets the
    /// owned `compress()` and the borrowed one-shot path share the exact
    /// same reset / dict-prime / entropy-seed setup and frame tail.
    pub fn compress(&mut self) {
        let prep = self.prepare_frame();
        // Take the reader out so `run_owned_block_loop` can borrow it
        // mutably alongside `&mut self` (the rest of the loop touches
        // `self.state` / `self.hasher`, disjoint from the reader). Restored
        // before the frame tail so a reused compressor keeps its source.
        let mut source = self
            .uncompressed_data
            .take()
            .expect("source must be set via set_source before compress()");
        let (all_blocks, total_uncompressed) =
            self.run_owned_block_loop(&mut source, prep.initial_size_hint);
        self.uncompressed_data = Some(source);
        self.finish_frame(all_blocks, total_uncompressed, &prep);
    }

    fn prepare_frame(&mut self) -> FramePrep {
        // Reset per-frame introspection state so a re-used compressor
        // doesn't carry over the previous frame's layout/checksums.
        #[cfg(feature = "lsm")]
        {
            self.frame_emit_info = None;
            // Always captured under lsm (drives `decompressed_byte_range`);
            // clear, keep the allocation for a reused compressor.
            self.block_decompressed_sizes.clear();
        }
        #[cfg(all(feature = "lsm", feature = "hash"))]
        {
            if self.per_block_checksums_enabled {
                self.block_checksums = Some(alloc::vec::Vec::new());
            } else {
                self.block_checksums = None;
            }
        }
        let initial_size_hint = self.source_size_hint;
        let source_size_hint_known = initial_size_hint.is_some();
        let use_dictionary_state =
            !matches!(self.compression_level, CompressionLevel::Uncompressed)
                && self.state.matcher.supports_dictionary_priming()
                && self.dictionary.is_some();
        if let Some(size_hint) = self.source_size_hint.take() {
            // Keep source-size hint scoped to payload bytes; dictionary priming
            // is applied separately and should not force larger matcher sizing.
            self.state.matcher.set_source_size_hint(size_hint);
        }
        // Clearing buffers to allow re-using of the compressor
        self.state.matcher.reset(self.compression_level);
        self.state.offset_hist = [1, 4, 8];
        // Sync `state.strategy_tag` to the level resolved at this reset so
        // the literal-compression gates (`min_literals_to_compress` /
        // `min_gain` in `encoding::blocks::compressed`) see the correct
        // strategy for the next frame. Frame-by-frame level changes go
        // through this same `compress()` entry point, so re-syncing here
        // covers level switches without touching the matcher dispatch.
        self.state.strategy_tag =
            crate::encoding::strategy::StrategyTag::for_compression_level(self.compression_level);
        let cached_entropy = if use_dictionary_state {
            self.dictionary_entropy_cache.as_ref()
        } else {
            None
        };
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            // This state drives sequence encoding, while matcher priming below updates
            // the match generator's internal repeat-offset history for match finding.
            self.state.offset_hist = dict.inner.offset_hist;
            // Donor `ZSTD_shouldAttachDict` (`zstd_compress.c`): a
            // precomputed-dictionary table is COPIED into the working context
            // only when the source is larger than a per-strategy cutoff; at or
            // below it (and for unknown size) the donor ATTACHES the dictionary
            // tables by reference (no per-frame table touch at all). We don't
            // have an attach-by-reference path yet, so:
            //   - large source (> cutoff): reuse the captured prime snapshot
            //     (a table copy) instead of re-hashing the dictionary — the
            //     donor COPY regime, where the copy is cheaper than re-priming;
            //   - small / unknown source: re-prime (the snapshot copy of the
            //     whole table would cost MORE than the sparse re-prime here,
            //     which is exactly why the donor attaches by reference instead).
            // `attachDictSizeCutoffs` per strategy: fast 8K, dfast 16K,
            // greedy/lazy/btopt 32K, btultra/btultra2 8K. Expressed as the
            // ceil-log bucket (8K = 2^13, 16K = 2^14, 32K = 2^15) so the
            // decision uses the SAME bucketed representation as the driver's
            // attach/copy gate (`reset_size_log`) — comparing
            // `source_size_ceil_log(hint)` on the full u64 avoids the `as usize`
            // truncation that could diverge from the driver on 32-bit targets.
            // For a power-of-two cutoff `2^k`, `ceil_log2(hint) > k` is exactly
            // `hint > 2^k`, so this is identical to the raw `hint > cutoff` on
            // 64-bit.
            let cutoff_log = match self.state.strategy_tag {
                crate::encoding::strategy::StrategyTag::Fast
                | crate::encoding::strategy::StrategyTag::BtUltra
                | crate::encoding::strategy::StrategyTag::BtUltra2 => 13,
                crate::encoding::strategy::StrategyTag::Dfast => 14,
                crate::encoding::strategy::StrategyTag::Greedy
                | crate::encoding::strategy::StrategyTag::Lazy
                | crate::encoding::strategy::StrategyTag::BtOpt => 15,
            };
            let prefer_copy_snapshot = initial_size_hint.is_some_and(|s| {
                crate::encoding::match_generator::source_size_ceil_log(s) > cutoff_log
            });
            let restored = prefer_copy_snapshot
                && self
                    .state
                    .matcher
                    .restore_primed_dictionary(self.compression_level);
            if !restored {
                self.state.matcher.prime_with_dictionary(
                    dict.inner.dict_content.as_slice(),
                    dict.inner.offset_hist,
                );
                if prefer_copy_snapshot {
                    self.state
                        .matcher
                        .capture_primed_dictionary(self.compression_level);
                }
            }
        }
        if let Some(cache) = cached_entropy {
            self.state.last_huff_table.clone_from(&cache.huff);
        } else {
            self.state.last_huff_table = None;
        }
        // `clone_from` keeps frame-to-frame seeding cheap for reused compressors by
        // reusing existing allocations where possible instead of reallocating every frame.
        if let Some(cache) = cached_entropy {
            self.state
                .fse_tables
                .ll_previous
                .clone_from(&cache.ll_previous);
            self.state
                .fse_tables
                .ml_previous
                .clone_from(&cache.ml_previous);
            self.state
                .fse_tables
                .of_previous
                .clone_from(&cache.of_previous);
        } else {
            self.state.fse_tables.ll_previous = None;
            self.state.fse_tables.ml_previous = None;
            self.state.fse_tables.of_previous = None;
        }
        let ll_entropy = cached_entropy.and_then(|cache| match cache.ll_previous.as_ref() {
            Some(PreviousFseTable::Custom(table)) => Some(table.as_ref()),
            _ => None,
        });
        let ml_entropy = cached_entropy.and_then(|cache| match cache.ml_previous.as_ref() {
            Some(PreviousFseTable::Custom(table)) => Some(table.as_ref()),
            _ => None,
        });
        let of_entropy = cached_entropy.and_then(|cache| match cache.of_previous.as_ref() {
            Some(PreviousFseTable::Custom(table)) => Some(table.as_ref()),
            _ => None,
        });
        self.state.matcher.seed_dictionary_entropy(
            self.state.last_huff_table.as_ref(),
            ll_entropy,
            ml_entropy,
            of_entropy,
        );
        #[cfg(feature = "hash")]
        {
            self.hasher = XxHash64::with_seed(0);
        }
        let window_size = self.state.matcher.window_size();
        assert!(
            window_size != 0,
            "matcher reported window_size == 0, which is invalid"
        );
        FramePrep {
            window_size,
            use_dictionary_state,
            source_size_hint_known,
            initial_size_hint,
        }
    }

    /// Owned streaming block loop: reads blocks from the caller-provided
    /// `source` reader, optionally pre-splits, hashes for the content
    /// checksum, and emits each block via `compress_block_encoded`,
    /// accumulating the block bytes. Returns `(all_blocks,
    /// total_uncompressed)`. The source is passed in (rather than read
    /// from `self.uncompressed_data`) so the streaming `compress` path can
    /// feed the configured reader while the slice paths
    /// (`compress_oneshot_borrowed`, `compress_independent_frame`) feed an
    /// in-place `&[u8]` cursor without baking its lifetime into the
    /// compressor type.
    fn run_owned_block_loop<Rd: Read>(
        &mut self,
        source: &mut Rd,
        initial_size_hint: Option<u64>,
    ) -> (Vec<u8>, u64) {
        // Accumulate all compressed blocks; the frame header is written
        // after all input has been read so Frame_Content_Size is known.
        // Seed capacity by source-size hint — see `initial_all_blocks_cap`.
        let mut all_blocks: Vec<u8> = Vec::with_capacity(initial_all_blocks_cap(initial_size_hint));
        let mut total_uncompressed: u64 = 0;
        let mut pending_input: Vec<u8> = Vec::new();
        let mut reached_eof = false;
        let mut savings = 0i64;
        // Compress block by block
        loop {
            // Read up to one donor block. When the pre-block splitter keeps a
            // suffix, top it back up before compressing the next block, matching
            // ZSTD_compress_frameChunk() over a contiguous input buffer.
            let block_capacity = MAX_BLOCK_SIZE as usize;
            // Always draw the block buffer from the matcher's recycled pool
            // (its capacity already covers the block size, so the resize below
            // stays in-place). Any carried pre-split suffix is copied in, and
            // `pending_input` is retained as a reusable carry buffer. The prior
            // approach `split_off`'d a fresh suffix Vec per pre-split and
            // `reserve_exact`-grew it to `block_capacity` every block; on a
            // heavily pre-split frame that churned one block-sized allocation
            // per split (~12 MB over ~90 splits on a 1 MiB corpus input).
            let mut uncompressed_data = self.state.matcher.get_next_space();
            uncompressed_data.clear();
            uncompressed_data.extend_from_slice(&pending_input);
            let mut filled = pending_input.len();
            pending_input.clear();
            // Size the read buffer to the bytes this block actually expects
            // rather than always zero-filling a full MAX_BLOCK_SIZE: a small
            // frame otherwise pays a 128 KiB `resize(_, 0)` memset per block
            // just to read a few KiB (the zero-fill past `filled` is then
            // truncated away). Bound the initial fill by the source-size hint
            // (exact for the slice entry points), and grow toward
            // `block_capacity` only if the hint under-counted.
            //
            // Overflow-free by construction (no `saturating_*` masking):
            // `filled <= block_capacity` always (the read only ever targets
            // `[filled..len]` with `len <= block_capacity`, and a carried-over
            // `pending_input` is a `split_off` below `block_capacity`), so
            // `block_capacity - filled` never underflows; pinning `remaining`
            // to `block_capacity` before the `usize` cast keeps the cast and
            // the final add within `usize` on every target.
            let initial_target = match initial_size_hint {
                Some(hint) if hint > total_uncompressed => {
                    let remaining = (hint - total_uncompressed).min(block_capacity as u64) as usize;
                    filled + remaining.min(block_capacity - filled)
                }
                // Unknown hint, or an inexact hint already met by prior blocks:
                // read against the full block window.
                _ => block_capacity,
            };
            if uncompressed_data.len() < initial_target {
                uncompressed_data.resize(initial_target, 0);
            }
            'read_loop: loop {
                if reached_eof || filled == block_capacity {
                    break 'read_loop;
                }
                if filled == uncompressed_data.len() {
                    // Hint under-counted the block; grow toward block_capacity
                    // (doubling, capped) so reading continues without paying a
                    // full-buffer zero up front. `len <= block_capacity` so the
                    // double stays well within `usize`; `filled < block_capacity`
                    // here (the `== block_capacity` break fired otherwise), so
                    // `filled + 1 <= block_capacity`.
                    let grow_to = (uncompressed_data.len() * 2).clamp(filled + 1, block_capacity);
                    uncompressed_data.resize(grow_to, 0);
                }
                let read_end = uncompressed_data.len();
                let new_bytes = source
                    .read(&mut uncompressed_data[filled..read_end])
                    .unwrap();
                if new_bytes == 0 {
                    reached_eof = true;
                    break 'read_loop;
                }
                filled += new_bytes;
                total_uncompressed += new_bytes as u64;
            }
            uncompressed_data.truncate(filled);
            let mut last_block = reached_eof;
            let remaining_for_split = if reached_eof {
                uncompressed_data.len()
            } else {
                block_capacity
            };
            if !matches!(self.compression_level, CompressionLevel::Uncompressed)
                && uncompressed_data.len() == block_capacity
            {
                let block_len = optimal_block_size(
                    self.compression_level,
                    &uncompressed_data,
                    remaining_for_split,
                    block_capacity,
                    savings,
                );
                if block_len < uncompressed_data.len() {
                    // Carry the kept suffix into the reusable `pending_input`
                    // buffer (cleared, capacity retained) instead of allocating
                    // a fresh Vec via `split_off`. Next iteration copies it back
                    // into a pooled block buffer. The block currently being
                    // compressed is truncated to the chosen split length.
                    pending_input.clear();
                    pending_input.extend_from_slice(&uncompressed_data[block_len..]);
                    uncompressed_data.truncate(block_len);
                    last_block = false;
                }
            }
            // As we read, hash that data too
            #[cfg(feature = "hash")]
            self.hasher.write(&uncompressed_data);
            // Per-physical-block XXH64 (low 32 bits) for the optional
            // per-block checksum sidecar. Hashing happens INSIDE the
            // block emitters (RLE / Raw fast-path / Compressed /
            // post-split partitions), so the digests vector has
            // exactly one entry per physical Block_Header written to
            // `all_blocks` — 1:1 with `FrameEmitInfo.blocks`. See
            // `enable_per_block_checksums` rustdoc.
            // Special handling is needed for compression of a totally empty file
            if uncompressed_data.is_empty() {
                let header = BlockHeader {
                    last_block: true,
                    block_type: crate::blocks::block::BlockType::Raw,
                    block_size: 0,
                };
                header.serialize(&mut all_blocks);
                #[cfg(feature = "lsm")]
                self.block_decompressed_sizes.push(0);
                #[cfg(all(feature = "lsm", feature = "hash"))]
                if let Some(checksums) = self.block_checksums.as_mut() {
                    checksums.push(xxh64_block_low32(&[]));
                }
                break;
            }

            match self.compression_level {
                CompressionLevel::Uncompressed => {
                    let header = BlockHeader {
                        last_block,
                        block_type: crate::blocks::block::BlockType::Raw,
                        block_size: uncompressed_data.len().try_into().unwrap(),
                    };
                    header.serialize(&mut all_blocks);
                    #[cfg(feature = "lsm")]
                    self.block_decompressed_sizes
                        .push(uncompressed_data.len() as u32);
                    #[cfg(all(feature = "lsm", feature = "hash"))]
                    if let Some(checksums) = self.block_checksums.as_mut() {
                        checksums.push(xxh64_block_low32(&uncompressed_data));
                    }
                    all_blocks.extend_from_slice(&uncompressed_data);
                    savings +=
                        uncompressed_data.len() as i64 - (3 + uncompressed_data.len()) as i64;
                }
                CompressionLevel::Fastest
                | CompressionLevel::Default
                | CompressionLevel::Better
                | CompressionLevel::Best
                | CompressionLevel::Level(_) => {
                    let before_len = all_blocks.len();
                    let block_len = uncompressed_data.len();
                    // A primed dictionary makes "incompressible-looking"
                    // blocks matchable against the dict, so the raw-fast-
                    // path inside must be bypassed (it skips matching).
                    // Mirror prepare_frame's `use_dictionary_state`: a dict
                    // is only PRIMED (and thus matchable) when the matcher
                    // supports priming — a non-priming matcher ignores an
                    // attached dictionary, so the raw-fast-path must stay
                    // enabled for it. (This arm is already non-Uncompressed.)
                    let dict_active = self.dictionary.is_some()
                        && self.state.matcher.supports_dictionary_priming();
                    compress_block_encoded(
                        &mut self.state,
                        self.compression_level,
                        last_block,
                        uncompressed_data,
                        &mut all_blocks,
                        dict_active,
                        #[cfg(feature = "lsm")]
                        Some(&mut self.block_decompressed_sizes),
                        #[cfg(all(feature = "lsm", feature = "hash"))]
                        self.block_checksums.as_mut(),
                    );
                    savings += block_len as i64 - (all_blocks.len() - before_len) as i64;
                }
            }
            if last_block && pending_input.is_empty() {
                break;
            }
        }
        (all_blocks, total_uncompressed)
    }

    /// Build the frame header bytes once the total payload size is known
    /// (so `Frame_Content_Size` / `single_segment` can be set). Shared by
    /// the drain (`finish_frame`) and returned-buffer
    /// (`finish_frame_to_vec`) tails.
    fn build_frame_header(&self, total_uncompressed: u64, prep: &FramePrep) -> Vec<u8> {
        // Match the donor framing policy for pledged one-shot inputs: use a
        // single-segment frame whenever the source fits the active window.
        let single_segment = !prep.use_dictionary_state
            && prep.source_size_hint_known
            && total_uncompressed >= 512
            && total_uncompressed <= prep.window_size;
        let header = FrameHeader {
            frame_content_size: Some(total_uncompressed),
            single_segment,
            content_checksum: cfg!(feature = "hash"),
            dictionary_id: if prep.use_dictionary_state {
                self.dictionary.as_ref().map(|dict| dict.inner.id as u64)
            } else {
                None
            },
            window_size: if single_segment {
                None
            } else {
                Some(prep.window_size)
            },
            magicless: self.magicless,
        };
        let mut header_buf: Vec<u8> = Vec::with_capacity(14);
        header.serialize(&mut header_buf);
        header_buf
    }

    /// Write the frame header, accumulated block bytes, and optional
    /// trailing content checksum to the configured drain; populate
    /// `frame_emit_info` (lsm). Header and blocks are written separately to
    /// avoid shifting `all_blocks` to prepend the header. Used by
    /// `compress` and `compress_oneshot_borrowed`.
    fn finish_frame(&mut self, all_blocks: Vec<u8>, total_uncompressed: u64, prep: &FramePrep) {
        let header_buf = self.build_frame_header(total_uncompressed, prep);
        // Snapshot the checksum before borrowing the drain field so the
        // `self.hasher` read and the `self.compressed_data` write don't
        // both need `&mut self` simultaneously.
        #[cfg(feature = "hash")]
        let checksum_bytes = (self.hasher.finish() as u32).to_le_bytes();
        let drain = self.compressed_data.as_mut().unwrap();
        drain.write_all(&header_buf).unwrap();
        drain.write_all(&all_blocks).unwrap();
        // If the `hash` feature is enabled, `content_checksum` is set in the
        // header and the 32-bit digest is written at the end of the frame.
        #[cfg(feature = "hash")]
        drain.write_all(&checksum_bytes).unwrap();
        #[cfg(feature = "lsm")]
        self.populate_frame_emit_info(header_buf.len(), &all_blocks);
    }

    /// Assemble the frame (header + blocks + optional checksum) into the
    /// caller-provided `out` buffer, replacing its contents, and populate
    /// `frame_emit_info` (lsm). `out` is cleared first (its allocation is
    /// reused, the CCtx-equivalent zero-per-call-alloc output path) then
    /// grown once to the exact frame size. Used by
    /// `compress_independent_frame_into`. The single `all_blocks` copy into
    /// `out` is the same one copy `finish_frame` performs writing
    /// `all_blocks` into a `Vec` drain, no extra buffering vs the drain
    /// path.
    fn write_frame_to_vec(
        &mut self,
        out: &mut Vec<u8>,
        all_blocks: Vec<u8>,
        total_uncompressed: u64,
        prep: &FramePrep,
    ) {
        let header_buf = self.build_frame_header(total_uncompressed, prep);
        let checksum_len = if cfg!(feature = "hash") { 4 } else { 0 };
        out.clear();
        out.reserve(header_buf.len() + all_blocks.len() + checksum_len);
        out.extend_from_slice(&header_buf);
        out.extend_from_slice(&all_blocks);
        #[cfg(feature = "hash")]
        out.extend_from_slice(&(self.hasher.finish() as u32).to_le_bytes());
        #[cfg(feature = "lsm")]
        self.populate_frame_emit_info(header_buf.len(), &all_blocks);
    }

    /// Walk `all_blocks` to recover per-block layout and store it in
    /// `frame_emit_info`. Each Block_Header is 3 bytes LE packing
    /// `(block_size << 3) | (block_type << 1) | last_block`. Physical body
    /// size differs by type: RLE bodies are always 1 byte (the repeated
    /// byte), Raw/Compressed bodies span `block_size`. `header_len` is the
    /// serialized frame-header length (frame offset of the first block).
    #[cfg(feature = "lsm")]
    fn populate_frame_emit_info(&mut self, header_len: usize, all_blocks: &[u8]) {
        use crate::blocks::block::BlockType as BT;
        use crate::encoding::frame_emit_info::{FrameBlock, FrameEmitInfo};
        // All frame-offset arithmetic below is bounded by u32 on the wire
        // (Block_Size is a 21-bit field, frames bounded by MAX_BLOCK_SIZE *
        // #blocks). A pathologically large frame whose total emitted size
        // exceeds u32::MAX would overflow the cast; bail out by leaving
        // `frame_emit_info` at `None` rather than handing the caller a
        // silently-truncated layout. The overflow path is statically
        // unreachable on every realistic frame so the predictor amortises
        // the branch to zero cost.
        let frame_header_len: u32 = match u32::try_from(header_len) {
            Ok(v) => v,
            Err(_) => return,
        };
        let all_blocks_len_u32: u32 = match u32::try_from(all_blocks.len()) {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut blocks: Vec<FrameBlock> = Vec::new();
        let mut cursor: usize = 0;
        while cursor + 3 <= all_blocks.len() {
            let mut header_u32 = [0u8; 4];
            header_u32[..3].copy_from_slice(&all_blocks[cursor..cursor + 3]);
            let raw = u32::from_le_bytes(header_u32);
            let last_block = (raw & 1) != 0;
            let block_type = match (raw >> 1) & 0b11 {
                0 => BT::Raw,
                1 => BT::RLE,
                2 => BT::Compressed,
                _ => BT::Reserved,
            };
            let block_size_field = raw >> 3;
            // RLE bodies are always 1 byte physical on the wire (the single
            // repeated byte); the spec's Block_Size field carries the
            // logical repeat count. Raw and Compressed bodies physically
            // span block_size_field bytes. Store the physical length in
            // body_size so the 'offset + header + body_size' arithmetic
            // always lands on the next block boundary, and surface the raw
            // spec field separately as block_size_field.
            let physical_body: u32 = match block_type {
                BT::RLE => 1,
                _ => block_size_field,
            };
            let cursor_u32: u32 = match u32::try_from(cursor) {
                Ok(v) => v,
                Err(_) => return,
            };
            let offset_in_frame = match frame_header_len.checked_add(cursor_u32) {
                Some(v) => v,
                None => return,
            };
            // Decompressed (regenerated) size, captured per physical block
            // during emit (1:1 with the wire blocks scanned here). Raw/RLE
            // are also wire-derivable (`block_size_field`); fall back to that
            // if the sidecar is somehow short, and to 0 for a Compressed
            // block whose size is not on the wire.
            let decompressed_size = self
                .block_decompressed_sizes
                .get(blocks.len())
                .copied()
                .unwrap_or(match block_type {
                    BT::Raw | BT::RLE => block_size_field,
                    _ => 0,
                });
            blocks.push(FrameBlock {
                offset_in_frame,
                header_size: 3,
                body_size: physical_body,
                block_size_field,
                block_type,
                last_block,
                decompressed_size,
            });
            cursor += 3 + physical_body as usize;
            if last_block {
                break;
            }
        }
        let checksum_range = if cfg!(feature = "hash") {
            let cs_start = match frame_header_len.checked_add(all_blocks_len_u32) {
                Some(v) => v,
                None => return,
            };
            let cs_end = match cs_start.checked_add(4) {
                Some(v) => v,
                None => return,
            };
            Some(cs_start..cs_end)
        } else {
            None
        };
        let body_total = match frame_header_len.checked_add(all_blocks_len_u32) {
            Some(v) => v,
            None => return,
        };
        let total_size = if checksum_range.is_some() {
            match body_total.checked_add(4) {
                Some(v) => v,
                None => return,
            }
        } else {
            body_total
        };
        self.frame_emit_info = Some(FrameEmitInfo {
            frame_header_range: 0..frame_header_len,
            blocks,
            checksum_range,
            total_size,
        });
    }

    /// Layout of the most recently emitted frame.
    ///
    /// Returns `None` if [`compress`](Self::compress) has not been
    /// called yet on this compressor. After a successful `compress()`
    /// the returned `FrameEmitInfo` describes the frame header range,
    /// every emitted block's offset / size / type, and the optional
    /// trailing content-checksum range — all in frame-absolute byte
    /// offsets matching the bytes written to the drain.
    ///
    /// Behind the `lsm` Cargo feature.
    #[cfg(feature = "lsm")]
    pub fn last_frame_emit_info(&self) -> Option<&crate::encoding::frame_emit_info::FrameEmitInfo> {
        self.frame_emit_info.as_ref()
    }

    /// Opt in to per-block XXH64 checksum computation during
    /// [`compress`](Self::compress). Default off; zero cost when
    /// disabled. The captured digests are accessible via
    /// [`last_frame_block_checksums`](Self::last_frame_block_checksums).
    ///
    /// One checksum is emitted per physical FrameBlock written to
    /// the drain: 1:1 cardinality with
    /// [`last_frame_emit_info`](Self::last_frame_emit_info)'s
    /// `blocks` vector. On the post-split optimization path
    /// (Level 16-22 with large window) the per-partition decompressed
    /// range is hashed inside the partition loop so the digest count
    /// still matches the emitted block count. The decoder collects
    /// per-physical-block digests on the same granularity, so
    /// element-wise equality holds round-trip.
    ///
    /// Behind `all(feature = "lsm", feature = "hash")` — the XXH64
    /// primitive lives behind the `hash` feature, so this method only
    /// compiles when both are enabled.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    pub fn enable_per_block_checksums(&mut self) {
        self.per_block_checksums_enabled = true;
    }

    /// Per-block XXH64 (low 32 bits) digests captured during the most
    /// recent `compress()` call. `None` unless
    /// [`enable_per_block_checksums`](Self::enable_per_block_checksums)
    /// was called before `compress()`.
    ///
    /// Behind `all(feature = "lsm", feature = "hash")`.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    pub fn last_frame_block_checksums(&self) -> Option<&[u32]> {
        self.block_checksums.as_deref()
    }

    /// Get a mutable reference to the source
    pub fn source_mut(&mut self) -> Option<&mut R> {
        self.uncompressed_data.as_mut()
    }

    /// Get a mutable reference to the drain
    pub fn drain_mut(&mut self) -> Option<&mut W> {
        self.compressed_data.as_mut()
    }

    /// Get a reference to the source
    pub fn source(&self) -> Option<&R> {
        self.uncompressed_data.as_ref()
    }

    /// Get a reference to the drain
    pub fn drain(&self) -> Option<&W> {
        self.compressed_data.as_ref()
    }

    /// Retrieve the source
    pub fn take_source(&mut self) -> Option<R> {
        self.uncompressed_data.take()
    }

    /// Retrieve the drain
    pub fn take_drain(&mut self) -> Option<W> {
        self.compressed_data.take()
    }

    /// Before calling [FrameCompressor::compress] you can replace the matcher
    pub fn replace_matcher(&mut self, mut match_generator: M) -> M {
        core::mem::swap(&mut match_generator, &mut self.state.matcher);
        match_generator
    }

    /// Before calling [FrameCompressor::compress] you can replace the compression level
    pub fn set_compression_level(
        &mut self,
        compression_level: CompressionLevel,
    ) -> CompressionLevel {
        let old = self.compression_level;
        self.compression_level = compression_level;
        old
    }

    /// Get the current compression level
    pub fn compression_level(&self) -> CompressionLevel {
        self.compression_level
    }

    /// Attach a pre-parsed dictionary to be used for subsequent compressions.
    ///
    /// In compressed modes, the dictionary id is written only when the active
    /// matcher supports dictionary priming.
    /// Uncompressed mode and non-priming matchers ignore the attached dictionary
    /// at encode time.
    pub fn set_dictionary(
        &mut self,
        dictionary: crate::decoding::Dictionary,
    ) -> Result<Option<EncoderDictionary>, crate::decoding::errors::DictionaryDecodeError> {
        self.attach_dictionary(EncoderDictionary::from_dictionary(dictionary))
    }

    /// Parse and attach a serialized dictionary blob.
    ///
    /// Parses with the encoder-only path (skips the FSE/HUF decode lookup-table
    /// build the encoder never reads); the entropy ENCODER tables — and thus
    /// the emitted frame — are identical to a full parse.
    pub fn set_dictionary_from_bytes(
        &mut self,
        raw_dictionary: &[u8],
    ) -> Result<Option<EncoderDictionary>, crate::decoding::errors::DictionaryDecodeError> {
        self.attach_dictionary(EncoderDictionary::from_bytes(raw_dictionary)?)
    }

    /// Attach an already-parsed [`EncoderDictionary`] without reparsing a raw
    /// blob.
    ///
    /// Accepts an `EncoderDictionary` produced once via
    /// [`EncoderDictionary::from_bytes`] / [`EncoderDictionary::from_dictionary`]
    /// or handed back by [`Self::clear_dictionary`] / the `set_dictionary*`
    /// return value, so callers can reattach or reuse a prepared dictionary
    /// across compressions without re-running the dictionary parse each time.
    /// Returns the previously-attached dictionary, if any.
    pub fn set_encoder_dictionary(
        &mut self,
        dictionary: EncoderDictionary,
    ) -> Result<Option<EncoderDictionary>, crate::decoding::errors::DictionaryDecodeError> {
        self.attach_dictionary(dictionary)
    }

    /// Remove the attached dictionary, returning it as an [`EncoderDictionary`].
    pub fn clear_dictionary(&mut self) -> Option<EncoderDictionary> {
        self.dictionary_entropy_cache = None;
        // Drop the CDict prime snapshot — it is keyed to the dictionary
        // being removed and must not be restored against a different (or no)
        // dictionary on the next frame.
        self.state.matcher.invalidate_primed_dictionary();
        self.dictionary.take()
    }

    /// Validate `enc`, build the encoder entropy cache from it, store it, and
    /// return the previously-attached dictionary. Shared by every public
    /// attach entry point: `set_dictionary`, `set_dictionary_from_bytes`, and
    /// `set_encoder_dictionary`.
    fn attach_dictionary(
        &mut self,
        enc: EncoderDictionary,
    ) -> Result<Option<EncoderDictionary>, crate::decoding::errors::DictionaryDecodeError> {
        let dictionary = &enc.inner;
        if dictionary.id == 0 {
            return Err(crate::decoding::errors::DictionaryDecodeError::ZeroDictionaryId);
        }
        if let Some(index) = dictionary.offset_hist.iter().position(|&rep| rep == 0) {
            return Err(
                crate::decoding::errors::DictionaryDecodeError::ZeroRepeatOffsetInDictionary {
                    index: index as u8,
                },
            );
        }
        self.dictionary_entropy_cache = Some(CachedDictionaryEntropy {
            huff: dictionary.huf.table.to_encoder_table(),
            ll_previous: dictionary
                .fse
                .literal_lengths
                .to_encoder_table()
                .map(|table| PreviousFseTable::Custom(SharedFseTable::new(table))),
            ml_previous: dictionary
                .fse
                .match_lengths
                .to_encoder_table()
                .map(|table| PreviousFseTable::Custom(SharedFseTable::new(table))),
            of_previous: dictionary
                .fse
                .offsets
                .to_encoder_table()
                .map(|table| PreviousFseTable::Custom(SharedFseTable::new(table))),
        });
        // A previously-captured CDict prime snapshot belongs to the OLD
        // dictionary; drop it so the first frame with the new dictionary
        // re-primes (and re-captures) instead of restoring stale tables.
        self.state.matcher.invalidate_primed_dictionary();
        Ok(self.dictionary.replace(enc))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(feature = "dict_builder", feature = "std"))]
    use alloc::format;
    use alloc::vec;

    use super::FrameCompressor;
    use crate::blocks::block::BlockType;
    use crate::common::{MAGIC_NUM, MAX_BLOCK_SIZE};
    use crate::decoding::{FrameDecoder, block_decoder, frame::read_frame_header};
    use crate::encoding::{Matcher, Sequence};
    use alloc::vec::Vec;

    fn generate_data(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.push((state >> 33) as u8);
        }
        data
    }

    fn first_block_type(frame: &[u8]) -> BlockType {
        let (_, header_size) = read_frame_header(frame).expect("frame header should parse");
        let mut decoder = block_decoder::new();
        let (header, _) = decoder
            .read_block_header(&frame[header_size as usize..])
            .expect("block header should parse");
        header.block_type
    }

    /// Frame content size is written correctly and C zstd can decompress the output.
    #[cfg(feature = "std")]
    #[test]
    fn fcs_header_written_and_c_zstd_compatible() {
        let levels = [
            crate::encoding::CompressionLevel::Uncompressed,
            crate::encoding::CompressionLevel::Fastest,
            crate::encoding::CompressionLevel::Default,
            crate::encoding::CompressionLevel::Better,
            crate::encoding::CompressionLevel::Best,
        ];
        let fcs_2byte = vec![0xCDu8; 300]; // 300 bytes → 2-byte FCS (256..=65791 range)
        let large = vec![0xABu8; 100_000];
        let inputs: [&[u8]; 5] = [
            &[],
            &[0x00],
            b"abcdefghijklmnopqrstuvwxy\n",
            &fcs_2byte,
            &large,
        ];
        for level in levels {
            for data in &inputs {
                let compressed = crate::encoding::compress_to_vec(*data, level);
                // Verify FCS is present and correct
                let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
                    .unwrap()
                    .0;
                assert_eq!(
                    header.frame_content_size(),
                    data.len() as u64,
                    "FCS mismatch for len={} level={:?}",
                    data.len(),
                    level,
                );
                // Confirm the FCS field is actually present in the header
                // (not just the decoder returning 0 for absent FCS).
                assert_ne!(
                    header.descriptor.frame_content_size_bytes().unwrap(),
                    0,
                    "FCS field must be present for len={} level={:?}",
                    data.len(),
                    level,
                );
                // Verify C zstd can decompress
                let mut decoded = Vec::new();
                zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap_or_else(
                    |e| {
                        panic!(
                            "C zstd decode failed for len={} level={level:?}: {e}",
                            data.len()
                        )
                    },
                );
                assert_eq!(
                    decoded.as_slice(),
                    *data,
                    "C zstd roundtrip failed for len={}",
                    data.len()
                );
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn source_size_hint_fastest_remains_ffi_compatible_small_input() {
        let data = vec![0xAB; 2047];
        let compressed = {
            let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
            compressor.set_source_size_hint(data.len() as u64);
            compressor.set_source(data.as_slice());
            let mut out = Vec::new();
            compressor.set_drain(&mut out);
            compressor.compress();
            out
        };

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn small_hinted_default_frame_uses_single_segment_header() {
        let data = generate_data(0xD15E_A5ED, 1024);
        let compressed = {
            let mut compressor = FrameCompressor::new(super::CompressionLevel::Default);
            compressor.set_source_size_hint(data.len() as u64);
            compressor.set_source(data.as_slice());
            let mut out = Vec::new();
            compressor.set_drain(&mut out);
            compressor.compress();
            out
        };

        let (frame_header, _) = read_frame_header(compressed.as_slice()).unwrap();
        assert!(
            frame_header.descriptor.single_segment_flag(),
            "small hinted default frames should use single-segment header for Rust/FFI parity"
        );
        assert_eq!(frame_header.frame_content_size(), data.len() as u64);
        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded)
            .expect("ffi decoder must accept single-segment small hinted default frame");
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn small_hinted_numeric_default_levels_use_single_segment_header() {
        let data = generate_data(0xA11C_E003, 1024);
        for level in [
            super::CompressionLevel::Level(0),
            super::CompressionLevel::Level(3),
        ] {
            let compressed = {
                let mut compressor = FrameCompressor::new(level);
                compressor.set_source_size_hint(data.len() as u64);
                compressor.set_source(data.as_slice());
                let mut out = Vec::new();
                compressor.set_drain(&mut out);
                compressor.compress();
                out
            };

            let (frame_header, _) = read_frame_header(compressed.as_slice()).unwrap();
            assert!(
                frame_header.descriptor.single_segment_flag(),
                "small hinted numeric default level frames should use single-segment header (level={level:?})"
            );
            assert_eq!(frame_header.frame_content_size(), data.len() as u64);
            let mut decoded = Vec::new();
            zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap_or_else(|e| {
                panic!(
                    "ffi decoder must accept single-segment small hinted numeric default level frame (level={level:?}): {e}"
                )
            });
            assert_eq!(decoded, data);
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn source_size_hint_levels_remain_ffi_compatible_small_inputs_matrix() {
        let levels = [
            super::CompressionLevel::Fastest,
            super::CompressionLevel::Default,
            super::CompressionLevel::Better,
            super::CompressionLevel::Best,
            super::CompressionLevel::Level(-1),
            super::CompressionLevel::Level(2),
            super::CompressionLevel::Level(3),
            super::CompressionLevel::Level(4),
            super::CompressionLevel::Level(11),
        ];
        let sizes = [
            511usize, 512, 513, 1023, 1024, 1536, 2047, 2048, 4095, 4096, 8191, 16_384, 16_385,
        ];

        for (seed_idx, seed) in [11u64, 23, 41].into_iter().enumerate() {
            for &size in &sizes {
                let data = generate_data(seed + seed_idx as u64, size);
                for &level in &levels {
                    let compressed = {
                        let mut compressor = FrameCompressor::new(level);
                        compressor.set_source_size_hint(data.len() as u64);
                        compressor.set_source(data.as_slice());
                        let mut out = Vec::new();
                        compressor.set_drain(&mut out);
                        compressor.compress();
                        out
                    };
                    if matches!(size, 511 | 512) {
                        let (frame_header, _) = read_frame_header(compressed.as_slice()).unwrap();
                        assert_eq!(
                            frame_header.descriptor.single_segment_flag(),
                            size == 512,
                            "single_segment 511/512 boundary mismatch: level={level:?} size={size}"
                        );
                    }

                    let mut decoded = Vec::new();
                    zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap_or_else(
                        |e| {
                            panic!(
                                "ffi decode failed with source-size hint: level={level:?} size={size} seed={} err={e}",
                                seed + seed_idx as u64
                            )
                        },
                    );
                    assert_eq!(
                        decoded,
                        data,
                        "hinted ffi roundtrip mismatch: level={level:?} size={size} seed={}",
                        seed + seed_idx as u64
                    );
                }
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn hinted_levels_use_single_segment_header_symmetrically() {
        let levels = [
            super::CompressionLevel::Fastest,
            super::CompressionLevel::Default,
            super::CompressionLevel::Better,
            super::CompressionLevel::Best,
            super::CompressionLevel::Level(0),
            super::CompressionLevel::Level(2),
            super::CompressionLevel::Level(3),
            super::CompressionLevel::Level(4),
            super::CompressionLevel::Level(11),
        ];
        for (seed_idx, seed) in [7u64, 23, 41].into_iter().enumerate() {
            let size = 1024 + seed_idx * 97;
            let data = generate_data(seed, size);
            for &level in &levels {
                let compressed = {
                    let mut compressor = FrameCompressor::new(level);
                    compressor.set_source_size_hint(data.len() as u64);
                    compressor.set_source(data.as_slice());
                    let mut out = Vec::new();
                    compressor.set_drain(&mut out);
                    compressor.compress();
                    out
                };
                let (frame_header, _) = read_frame_header(compressed.as_slice()).unwrap();
                assert!(
                    frame_header.descriptor.single_segment_flag(),
                    "hinted frame should be single-segment for level={level:?} size={}",
                    data.len()
                );
                assert_eq!(frame_header.frame_content_size(), data.len() as u64);
                let mut decoded = Vec::new();
                zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap_or_else(|e| {
                    panic!(
                        "ffi decode failed for hinted single-segment parity: level={level:?} size={} err={e}",
                        data.len()
                    )
                });
                assert_eq!(decoded, data);
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn hinted_levels_pin_511_512_single_segment_boundary() {
        let levels = [
            super::CompressionLevel::Fastest,
            super::CompressionLevel::Default,
            super::CompressionLevel::Better,
            super::CompressionLevel::Best,
            super::CompressionLevel::Level(0),
            super::CompressionLevel::Level(2),
            super::CompressionLevel::Level(3),
            super::CompressionLevel::Level(4),
            super::CompressionLevel::Level(11),
        ];
        for (seed_idx, seed) in [7u64, 23, 41].into_iter().enumerate() {
            for &size in &[511usize, 512] {
                let data = generate_data(seed + seed_idx as u64, size);
                for &level in &levels {
                    let compressed = {
                        let mut compressor = FrameCompressor::new(level);
                        compressor.set_source_size_hint(data.len() as u64);
                        compressor.set_source(data.as_slice());
                        let mut out = Vec::new();
                        compressor.set_drain(&mut out);
                        compressor.compress();
                        out
                    };
                    let (frame_header, _) = read_frame_header(compressed.as_slice()).unwrap();
                    assert_eq!(
                        frame_header.descriptor.single_segment_flag(),
                        size == 512,
                        "single_segment 511/512 boundary mismatch: level={level:?} size={size}"
                    );
                    let mut decoded = Vec::new();
                    zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap_or_else(
                        |e| {
                            panic!(
                                "ffi decode failed at single-segment boundary: level={level:?} size={size} seed={} err={e}",
                                seed + seed_idx as u64
                            )
                        },
                    );
                    assert_eq!(decoded, data);
                }
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn fastest_random_block_uses_raw_fast_path() {
        let data = generate_data(0xC0FF_EE11, 10 * 1024);
        let compressed =
            crate::encoding::compress_to_vec(data.as_slice(), super::CompressionLevel::Fastest);

        assert_eq!(first_block_type(&compressed), BlockType::Raw);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn default_random_block_uses_raw_fast_path() {
        let data = generate_data(0xD15E_A5ED, 10 * 1024);
        let compressed =
            crate::encoding::compress_to_vec(data.as_slice(), super::CompressionLevel::Default);

        assert_eq!(first_block_type(&compressed), BlockType::Raw);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn best_random_block_uses_raw_fast_path() {
        let data = generate_data(0xB35C_AFE1, 10 * 1024);
        let compressed =
            crate::encoding::compress_to_vec(data.as_slice(), super::CompressionLevel::Best);

        assert_eq!(first_block_type(&compressed), BlockType::Raw);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn level2_random_block_uses_raw_fast_path() {
        let data = generate_data(0xA11C_E222, 10 * 1024);
        let compressed =
            crate::encoding::compress_to_vec(data.as_slice(), super::CompressionLevel::Level(2));

        assert_eq!(first_block_type(&compressed), BlockType::Raw);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn better_random_block_uses_raw_fast_path() {
        let data = generate_data(0xBE77_E111, 10 * 1024);
        let compressed =
            crate::encoding::compress_to_vec(data.as_slice(), super::CompressionLevel::Better);

        assert_eq!(first_block_type(&compressed), BlockType::Raw);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[cfg(feature = "std")]
    #[test]
    fn compressible_logs_do_not_fall_back_to_raw_fast_path() {
        let mut data = Vec::with_capacity(16 * 1024);
        const LINE: &[u8] =
            b"ts=2026-04-10T00:00:00Z level=INFO tenant=demo op=flush table=orders\n";
        while data.len() < 16 * 1024 {
            let remaining = 16 * 1024 - data.len();
            data.extend_from_slice(&LINE[..LINE.len().min(remaining)]);
        }

        fn assert_not_raw_for_level(data: &[u8], level: super::CompressionLevel) {
            let compressed = crate::encoding::compress_to_vec(data, level);
            assert_ne!(first_block_type(&compressed), BlockType::Raw);
            assert!(
                compressed.len() < data.len(),
                "compressible input should remain compressible for level={level:?}"
            );
            let mut decoded = Vec::new();
            zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
            assert_eq!(decoded, data);
        }

        assert_not_raw_for_level(data.as_slice(), super::CompressionLevel::Fastest);
        assert_not_raw_for_level(data.as_slice(), super::CompressionLevel::Default);
        assert_not_raw_for_level(data.as_slice(), super::CompressionLevel::Level(3));
        assert_not_raw_for_level(data.as_slice(), super::CompressionLevel::Better);
        assert_not_raw_for_level(data.as_slice(), super::CompressionLevel::Best);
    }

    #[cfg(feature = "std")]
    #[test]
    fn hinted_small_compressible_frames_use_single_segment_across_levels() {
        let mut data = Vec::with_capacity(4 * 1024);
        const LINE: &[u8] =
            b"ts=2026-04-10T00:00:00Z level=INFO tenant=demo op=flush table=orders\n";
        while data.len() < 4 * 1024 {
            let remaining = 4 * 1024 - data.len();
            data.extend_from_slice(&LINE[..LINE.len().min(remaining)]);
        }

        for level in [
            super::CompressionLevel::Fastest,
            super::CompressionLevel::Default,
            super::CompressionLevel::Better,
            super::CompressionLevel::Best,
            super::CompressionLevel::Level(0),
            super::CompressionLevel::Level(3),
            super::CompressionLevel::Level(4),
            super::CompressionLevel::Level(11),
        ] {
            let compressed = {
                let mut compressor = FrameCompressor::new(level);
                compressor.set_source_size_hint(data.len() as u64);
                compressor.set_source(data.as_slice());
                let mut out = Vec::new();
                compressor.set_drain(&mut out);
                compressor.compress();
                out
            };
            let (frame_header, _) = read_frame_header(compressed.as_slice()).unwrap();
            assert!(
                frame_header.descriptor.single_segment_flag(),
                "hinted small compressible frame should use single-segment (level={level:?})"
            );
            assert_ne!(
                first_block_type(&compressed),
                BlockType::Raw,
                "compressible hinted frame should stay off raw fast path (level={level:?})"
            );
            assert!(
                compressed.len() < data.len(),
                "compressible hinted frame should still shrink (level={level:?})"
            );
            let mut decoded = Vec::new();
            zstd::stream::copy_decode(compressed.as_slice(), &mut decoded)
                .unwrap_or_else(|e| panic!("ffi decode failed (level={level:?}): {e}"));
            assert_eq!(decoded, data);
        }
    }

    struct NoDictionaryMatcher {
        last_space: Vec<u8>,
        window_size: u64,
    }

    impl NoDictionaryMatcher {
        fn new(window_size: u64) -> Self {
            Self {
                last_space: Vec::new(),
                window_size,
            }
        }
    }

    impl Matcher for NoDictionaryMatcher {
        fn get_next_space(&mut self) -> Vec<u8> {
            vec![0; self.window_size as usize]
        }

        fn get_last_space(&mut self) -> &[u8] {
            self.last_space.as_slice()
        }

        fn commit_space(&mut self, space: Vec<u8>) {
            self.last_space = space;
        }

        fn skip_matching(&mut self) {}

        fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
            handle_sequence(Sequence::Literals {
                literals: self.last_space.as_slice(),
            });
        }

        fn reset(&mut self, _level: super::CompressionLevel) {
            self.last_space.clear();
        }

        fn window_size(&self) -> u64 {
            self.window_size
        }
    }

    #[test]
    fn frame_starts_with_magic_num() {
        let mock_data = [1_u8, 2, 3].as_slice();
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data);
        compressor.set_drain(&mut output);

        compressor.compress();
        assert!(output.starts_with(&MAGIC_NUM.to_le_bytes()));
    }

    #[test]
    fn very_simple_raw_compress() {
        let mock_data = [1_u8, 2, 3].as_slice();
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data);
        compressor.set_drain(&mut output);

        compressor.compress();
    }

    #[test]
    fn very_simple_compress() {
        let mut mock_data = vec![0; 1 << 17];
        mock_data.extend(vec![1; (1 << 17) - 1]);
        mock_data.extend(vec![2; (1 << 18) - 1]);
        mock_data.extend(vec![2; 1 << 17]);
        mock_data.extend(vec![3; (1 << 17) - 1]);
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data.as_slice());
        compressor.set_drain(&mut output);

        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(mock_data.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(output.as_slice(), &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);
    }

    #[test]
    fn rle_compress() {
        let mock_data = vec![0; 1 << 19];
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data.as_slice());
        compressor.set_drain(&mut output);

        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(mock_data.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);
    }

    #[test]
    fn aaa_compress() {
        let mock_data = vec![0, 1, 3, 4, 5];
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data.as_slice());
        compressor.set_drain(&mut output);

        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(mock_data.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(output.as_slice(), &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);
    }

    #[test]
    fn dictionary_compression_sets_required_dict_id_and_roundtrips() {
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let dict_for_encoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
        let dict_for_decoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();

        let mut data = Vec::new();
        for _ in 0..8 {
            data.extend_from_slice(&dict_for_decoder.dict_content[..2048]);
        }

        let mut with_dict = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        let previous = compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");
        assert!(
            previous.is_none(),
            "first dictionary insert should return None"
        );
        assert_eq!(
            compressor
                .set_dictionary(dict_for_encoder)
                .expect("valid dictionary should attach")
                .expect("set_dictionary_from_bytes inserted previous dictionary")
                .id(),
            dict_for_decoder.id
        );
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut with_dict);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
            .expect("encoded stream should have a frame header");
        assert_eq!(frame_header.dictionary_id(), Some(dict_for_decoder.id));

        let mut decoder = FrameDecoder::new();
        let mut missing_dict_target = Vec::with_capacity(data.len());
        let err = decoder
            .decode_all_to_vec(&with_dict, &mut missing_dict_target)
            .unwrap_err();
        assert!(
            matches!(
                &err,
                crate::decoding::errors::FrameDecoderError::DictNotProvided { .. }
            ),
            "dict-compressed stream should require dictionary id, got: {err:?}"
        );

        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(data.len());
        decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
        assert_eq!(decoded, data);

        let mut ffi_decoder = zstd::bulk::Decompressor::with_dictionary(dict_raw).unwrap();
        let mut ffi_decoded = Vec::with_capacity(data.len());
        let ffi_written = ffi_decoder
            .decompress_to_buffer(with_dict.as_slice(), &mut ffi_decoded)
            .unwrap();
        assert_eq!(ffi_written, data.len());
        assert_eq!(ffi_decoded, data);
    }

    #[cfg(all(feature = "dict_builder", feature = "std"))]
    #[test]
    fn dictionary_compression_roundtrips_with_dict_builder_dictionary() {
        use std::io::Cursor;

        let mut training = Vec::new();
        for idx in 0..256u32 {
            training.extend_from_slice(
                format!("tenant=demo table=orders key={idx} region=eu\n").as_bytes(),
            );
        }
        let mut raw_dict = Vec::new();
        crate::dictionary::create_raw_dict_from_source(
            Cursor::new(training.as_slice()),
            training.len(),
            &mut raw_dict,
            4096,
        )
        .expect("dict_builder training should succeed");
        assert!(
            !raw_dict.is_empty(),
            "dict_builder produced an empty dictionary"
        );

        let dict_id = 0xD1C7_0008;
        let encoder_dict =
            crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();
        let decoder_dict =
            crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();

        let mut payload = Vec::new();
        for idx in 0..96u32 {
            payload.extend_from_slice(
                format!(
                    "tenant=demo table=orders op=put key={idx} value=aaaaabbbbbcccccdddddeeeee\n"
                )
                .as_bytes(),
            );
        }

        let mut without_dict = Vec::new();
        let mut baseline = FrameCompressor::new(super::CompressionLevel::Fastest);
        baseline.set_source(payload.as_slice());
        baseline.set_drain(&mut without_dict);
        baseline.compress();

        let mut with_dict = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(encoder_dict)
            .expect("valid dict_builder dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut with_dict);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
            .expect("encoded stream should have a frame header");
        assert_eq!(frame_header.dictionary_id(), Some(dict_id));
        let mut decoder = FrameDecoder::new();
        decoder.add_dict(decoder_dict).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
        assert!(
            with_dict.len() < without_dict.len(),
            "trained dictionary should improve compression for this small payload"
        );
    }

    #[test]
    fn set_dictionary_from_bytes_seeds_entropy_tables_for_first_block() {
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let mut output = Vec::new();
        let input = b"";

        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        let previous = compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");
        assert!(previous.is_none());

        compressor.set_source(input.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        assert!(
            compressor.state.last_huff_table.is_some(),
            "dictionary entropy should seed previous huffman table before first block"
        );
        assert!(
            compressor.state.fse_tables.ll_previous.is_some(),
            "dictionary entropy should seed previous ll table before first block"
        );
        assert!(
            compressor.state.fse_tables.ml_previous.is_some(),
            "dictionary entropy should seed previous ml table before first block"
        );
        assert!(
            compressor.state.fse_tables.of_previous.is_some(),
            "dictionary entropy should seed previous of table before first block"
        );
    }

    #[test]
    fn set_encoder_dictionary_reattaches_prepared_dict_without_reparse() {
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let payload = b"tenant=demo table=orders op=put key=1 value=aaaaabbbbbcccccdddddeeeee\n\
              tenant=demo table=orders op=put key=2 value=aaaaabbbbbcccccdddddeeeee\n";

        // Prepare the EncoderDictionary once, then attach it via the prepared-
        // dictionary API (no raw-blob reparse at attach time).
        let prepared =
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
        let dict_id = prepared.id();

        let mut with_dict = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        let previous = compressor
            .set_encoder_dictionary(prepared)
            .expect("prepared dictionary should attach");
        assert!(previous.is_none());
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut with_dict);
        compressor.compress();
        // clear_dictionary hands the prepared dictionary back (last use of
        // `compressor`, so its `&mut with_dict` drain borrow ends here).
        let returned = compressor
            .clear_dictionary()
            .expect("dictionary was attached");
        assert_eq!(returned.id(), dict_id);

        // The reattached dictionary drives the frame: its id is advertised and
        // the stream round-trips through a decoder primed with the same dict.
        let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
            .expect("encoded stream should have a frame header");
        assert_eq!(frame_header.dictionary_id(), Some(dict_id));
        let decoder_dict = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.add_dict(decoder_dict).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
        assert_eq!(decoded.as_slice(), payload.as_slice());

        // The dictionary handed back by clear_dictionary reattaches to another
        // compressor without touching the raw bytes again, producing an
        // identical frame.
        let mut with_dict2 = Vec::new();
        let mut compressor2 = FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor2
            .set_encoder_dictionary(returned)
            .expect("returned dictionary should reattach");
        compressor2.set_source(payload.as_slice());
        compressor2.set_drain(&mut with_dict2);
        compressor2.compress();
        assert_eq!(
            with_dict2, with_dict,
            "reattached prepared dict must produce an identical frame"
        );
    }

    #[test]
    fn dict_primed_matcher_snapshot_reused_across_frames_is_byte_identical() {
        // CDict-equivalent: a compressor reused across frames with the same
        // dictionary restores the primed matcher snapshot on frames 2..N
        // (a table copy) instead of re-hashing the dictionary. The restored
        // state must reproduce the first-frame (freshly-primed) output
        // byte-for-byte, and every frame must round-trip through a
        // dict-primed decoder.
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        // Source must exceed the Fast strategy's 8 KiB attach cutoff so the
        // copy-snapshot (restore) path is taken on frame 2 — at or below the
        // cutoff the donor attaches by reference and we fall back to re-prime,
        // which would not exercise restore.
        let mut payload = Vec::new();
        while payload.len() < 16 * 1024 {
            payload.extend_from_slice(
                b"tenant=demo table=orders op=put key=1 value=aaaaabbbbbcccccdddddeeeee\n",
            );
        }

        let prepared =
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
        let dict_id = prepared.id();
        let mut compressor: FrameCompressor =
            FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor
            .set_encoder_dictionary(prepared)
            .expect("prepared dictionary should attach");

        // Frame 1 primes + captures the snapshot; frame 2 restores it.
        let frame1 = compressor.compress_independent_frame(payload.as_slice());
        let frame2 = compressor.compress_independent_frame(payload.as_slice());
        assert_eq!(
            frame1, frame2,
            "restored prime snapshot must reproduce the freshly-primed frame byte-for-byte"
        );

        // Both frames advertise the dict id and round-trip through a
        // dict-primed decoder.
        for frame in [&frame1, &frame2] {
            let (hdr, _) =
                crate::decoding::frame::read_frame_header(frame.as_slice()).expect("frame header");
            assert_eq!(hdr.dictionary_id(), Some(dict_id));
            let mut decoder = FrameDecoder::new();
            decoder
                .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
                .unwrap();
            let mut decoded = Vec::with_capacity(payload.len());
            decoder.decode_all_to_vec(frame, &mut decoded).unwrap();
            assert_eq!(decoded.as_slice(), payload.as_slice());
        }
    }

    #[test]
    fn dict_primed_matcher_cache_reused_across_small_attach_frames_is_byte_identical() {
        // CDict-equivalent ATTACH path (small source, at/below the Fast 8 KiB
        // attach cutoff): frames 2..N re-prime — re-committing the dict bytes
        // to history — but reuse the already-built dict table instead of
        // re-hashing it. The cached-table frame must reproduce the
        // freshly-primed first frame byte-for-byte, and a fresh single-frame
        // compressor (no prior dict cache) must produce the identical bytes
        // too, proving the cache changes timing, not output.
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        // Stay under the 8 KiB cutoff so the attach (re-prime) path is taken
        // every frame rather than the copy-snapshot restore.
        let mut payload = Vec::new();
        while payload.len() < 2 * 1024 {
            payload.extend_from_slice(b"tenant=demo op=put key=1 value=aaaaabbbbbcccccddddd\n");
        }

        let prepared =
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
        let dict_id = prepared.id();
        let mut compressor: FrameCompressor =
            FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor
            .set_encoder_dictionary(prepared)
            .expect("prepared dictionary should attach");

        // Frame 1 builds + marks the dict table; frame 2 reuses it.
        let frame1 = compressor.compress_independent_frame(payload.as_slice());
        let frame2 = compressor.compress_independent_frame(payload.as_slice());
        assert_eq!(
            frame1, frame2,
            "reused dict table (attach path) must reproduce the freshly-built frame byte-for-byte"
        );

        // A fresh compressor (cold dict cache) must emit the same bytes — the
        // cache is a timing optimization, never a content change.
        let fresh_prepared =
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
        let mut fresh: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        fresh
            .set_encoder_dictionary(fresh_prepared)
            .expect("prepared dictionary should attach");
        let fresh_frame = fresh.compress_independent_frame(payload.as_slice());
        assert_eq!(
            fresh_frame, frame1,
            "cold-cache compressor must match the warm-cache frame byte-for-byte"
        );

        for frame in [&frame1, &frame2] {
            let (hdr, _) =
                crate::decoding::frame::read_frame_header(frame.as_slice()).expect("frame header");
            assert_eq!(hdr.dictionary_id(), Some(dict_id));
            let mut decoder = FrameDecoder::new();
            decoder
                .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
                .unwrap();
            let mut decoded = Vec::with_capacity(payload.len());
            decoder.decode_all_to_vec(frame, &mut decoded).unwrap();
            assert_eq!(decoded.as_slice(), payload.as_slice());
        }
    }

    #[test]
    fn set_dictionary_from_bytes_matches_full_decode_byte_for_byte() {
        // The encoder-only dict parse (`decode_dict_for_encoding`, used by
        // `set_dictionary_from_bytes`) skips the FSE/HUF decoder-table build and
        // the enrich passes. The encoder entropy tables are derived purely from
        // the symbol probabilities / Huffman weights, so the compressed output
        // MUST be byte-identical to the full-decode path. This pins the
        // load-bearing equivalence so a future FSE/HUF parsing refactor that
        // still round-trips but silently diverges on the probabilities/weights
        // fails loudly here instead of producing a different (but valid) frame.
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let payload = b"tenant=demo table=orders op=put key=1 value=aaaaabbbbbcccccdddddeeeee\n\
              tenant=demo table=orders op=put key=2 value=aaaaabbbbbcccccdddddeeeee\n";

        // Path A: encoder-only parse straight from the raw blob.
        let mut from_bytes_out = Vec::new();
        {
            let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
            compressor
                .set_dictionary_from_bytes(dict_raw)
                .expect("dictionary bytes should parse");
            compressor.set_source(payload.as_slice());
            compressor.set_drain(&mut from_bytes_out);
            compressor.compress();
        }

        // Path B: full decode (builds the decoder tables too), then attach for
        // encoding via the `Dictionary` setter.
        let full_decode = crate::decoding::Dictionary::decode_dict(dict_raw)
            .expect("dictionary bytes should fully decode");
        let mut full_decode_out = Vec::new();
        {
            let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
            compressor
                .set_dictionary(full_decode)
                .expect("full-decode dictionary should attach");
            compressor.set_source(payload.as_slice());
            compressor.set_drain(&mut full_decode_out);
            compressor.compress();
        }

        assert_eq!(
            from_bytes_out, full_decode_out,
            "encoder-only dict parse must produce byte-identical output to the full decode"
        );
    }

    #[test]
    fn set_dictionary_rejects_zero_dictionary_id() {
        let invalid = crate::decoding::Dictionary {
            id: 0,
            fse: crate::decoding::scratch::FSEScratch::new(),
            huf: crate::decoding::scratch::HuffmanScratch::new(),
            dict_content: vec![1, 2, 3],
            offset_hist: [1, 4, 8],
        };

        let mut compressor: FrameCompressor<
            &[u8],
            Vec<u8>,
            crate::encoding::match_generator::MatchGeneratorDriver,
        > = FrameCompressor::new(super::CompressionLevel::Fastest);
        let result = compressor.set_dictionary(invalid);
        assert!(matches!(
            result,
            Err(crate::decoding::errors::DictionaryDecodeError::ZeroDictionaryId)
        ));
    }

    #[test]
    fn set_dictionary_rejects_zero_repeat_offsets() {
        let invalid = crate::decoding::Dictionary {
            id: 1,
            fse: crate::decoding::scratch::FSEScratch::new(),
            huf: crate::decoding::scratch::HuffmanScratch::new(),
            dict_content: vec![1, 2, 3],
            offset_hist: [0, 4, 8],
        };

        let mut compressor: FrameCompressor<
            &[u8],
            Vec<u8>,
            crate::encoding::match_generator::MatchGeneratorDriver,
        > = FrameCompressor::new(super::CompressionLevel::Fastest);
        let result = compressor.set_dictionary(invalid);
        assert!(matches!(
            result,
            Err(
                crate::decoding::errors::DictionaryDecodeError::ZeroRepeatOffsetInDictionary {
                    index: 0
                }
            )
        ));
    }

    #[test]
    fn uncompressed_mode_does_not_require_dictionary() {
        let dict_id = 0xABCD_0001;
        let dict =
            crate::decoding::Dictionary::from_raw_content(dict_id, b"shared-history".to_vec())
                .expect("raw dictionary should be valid");

        let payload = b"plain-bytes-that-should-stay-raw";
        let mut output = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor
            .set_dictionary(dict)
            .expect("dictionary should attach in uncompressed mode");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
            .expect("encoded frame should have a header");
        assert_eq!(
            frame_header.dictionary_id(),
            None,
            "raw/uncompressed frames must not advertise dictionary dependency"
        );

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn dictionary_roundtrip_stays_valid_after_output_exceeds_window() {
        use crate::encoding::match_generator::MatchGeneratorDriver;

        let dict_id = 0xABCD_0002;
        let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
            .expect("raw dictionary should be valid");
        let dict_for_decoder =
            crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
                .expect("raw dictionary should be valid");

        // Payload must exceed the encoder's advertised window (512 KiB
        // for Fastest after `window_log = 19` alignment with donor's
        // L1 fast row in `clevels.h`) so the test actually exercises
        // cross-window-boundary behavior.
        let payload = b"abcdefgh".repeat(512 * 1024 / 8 + 64);
        let matcher = MatchGeneratorDriver::new(1024, 1);

        let mut no_dict_output = Vec::new();
        let mut no_dict_compressor =
            FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
        no_dict_compressor.set_source(payload.as_slice());
        no_dict_compressor.set_drain(&mut no_dict_output);
        no_dict_compressor.compress();
        let (no_dict_frame_header, _) =
            crate::decoding::frame::read_frame_header(no_dict_output.as_slice())
                .expect("baseline frame should have a header");
        let no_dict_window = no_dict_frame_header
            .window_size()
            .expect("window size should be present");

        let mut output = Vec::new();
        let matcher = MatchGeneratorDriver::new(1024, 1);
        let mut compressor =
            FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(dict)
            .expect("dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
            .expect("encoded frame should have a header");
        let advertised_window = frame_header
            .window_size()
            .expect("window size should be present");
        assert_eq!(
            advertised_window, no_dict_window,
            "dictionary priming must not inflate advertised window size"
        );
        assert!(
            payload.len() > advertised_window as usize,
            "test must cross the advertised window boundary"
        );

        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn source_size_hint_with_dictionary_keeps_roundtrip_and_nonincreasing_window() {
        let dict_id = 0xABCD_0004;
        let dict_content = b"abcd".repeat(1024); // 4 KiB dictionary history
        let dict = crate::decoding::Dictionary::from_raw_content(dict_id, dict_content).unwrap();
        let dict_for_decoder =
            crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024)).unwrap();
        let payload = b"abcdabcdabcdabcd".repeat(128);

        let mut hinted_output = Vec::new();
        let mut hinted = FrameCompressor::new(super::CompressionLevel::Fastest);
        hinted.set_dictionary(dict).unwrap();
        hinted.set_source_size_hint(1);
        hinted.set_source(payload.as_slice());
        hinted.set_drain(&mut hinted_output);
        hinted.compress();

        let mut no_hint_output = Vec::new();
        let mut no_hint = FrameCompressor::new(super::CompressionLevel::Fastest);
        no_hint
            .set_dictionary(
                crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024))
                    .unwrap(),
            )
            .unwrap();
        no_hint.set_source(payload.as_slice());
        no_hint.set_drain(&mut no_hint_output);
        no_hint.compress();

        let hinted_window = crate::decoding::frame::read_frame_header(hinted_output.as_slice())
            .expect("encoded frame should have a header")
            .0
            .window_size()
            .expect("window size should be present");
        let no_hint_window = crate::decoding::frame::read_frame_header(no_hint_output.as_slice())
            .expect("encoded frame should have a header")
            .0
            .window_size()
            .expect("window size should be present");
        assert!(
            hinted_window <= no_hint_window,
            "source-size hint should not increase advertised window with dictionary priming",
        );

        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder
            .decode_all_to_vec(&hinted_output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, payload);
    }

    /// A dictionary segment embedded ONCE in otherwise-incompressible
    /// input must be matched against the dictionary. Before the fix the
    /// raw-fast-path (which skips matching) fired on the
    /// incompressible-looking block and the dictionary was never searched,
    /// so `with_dict` came out the same size as `no_dict` (the embedded
    /// match was lost). Now the block compresses against the dict.
    #[test]
    fn dictionary_segment_in_incompressible_input_is_matched() {
        // Deterministic LCG bytes: high-entropy, so the only compressible
        // content is the embedded dictionary segment.
        fn lcg(seed: u64, n: usize) -> alloc::vec::Vec<u8> {
            let mut s = seed;
            (0..n)
                .map(|_| {
                    s = s
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (s >> 56) as u8
                })
                .collect()
        }
        let dict_id = 0x00DC_7777;
        let r = lcg(1, 512); // the dictionary content
        let mut payload = lcg(2, 2000); // incompressible filler before
        payload.extend_from_slice(&r); // the single dict-matchable segment
        payload.extend_from_slice(&lcg(3, 1500)); // filler after

        // Precondition: the payload must actually look incompressible so
        // that the raw-fast-path WOULD fire (and skip matching) without
        // the fix. If the heuristic ever changes and this no longer holds,
        // the test below would pass vacuously — assert it up front.
        assert!(
            crate::encoding::incompressible::block_looks_incompressible(&payload),
            "test payload must look incompressible to exercise the raw-fast-path",
        );

        let compress =
            |level: super::CompressionLevel, dict: Option<&[u8]>| -> alloc::vec::Vec<u8> {
                let mut out = alloc::vec::Vec::new();
                let mut c = FrameCompressor::new(level);
                if let Some(d) = dict {
                    c.set_dictionary(
                        crate::decoding::Dictionary::from_raw_content(dict_id, d.to_vec()).unwrap(),
                    )
                    .unwrap();
                }
                c.set_source(payload.as_slice());
                c.set_drain(&mut out);
                c.compress();
                out
            };

        for lvl in [
            super::CompressionLevel::Level(2),
            super::CompressionLevel::Level(6),
            super::CompressionLevel::Level(19),
        ] {
            let with_dict = compress(lvl, Some(&r));
            let no_dict = compress(lvl, None);
            // The 512-byte dict segment should be matched, saving most of
            // its length (generous slack for sequence/header coding).
            assert!(
                with_dict.len() + 300 < no_dict.len(),
                "{lvl:?}: dict segment not matched (with_dict={}, no_dict={})",
                with_dict.len(),
                no_dict.len(),
            );
            // The dict-compressed frame must round-trip through the decoder.
            let mut decoder = FrameDecoder::new();
            decoder
                .add_dict(
                    crate::decoding::Dictionary::from_raw_content(dict_id, r.clone()).unwrap(),
                )
                .unwrap();
            let mut decoded = Vec::with_capacity(payload.len());
            decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
            assert_eq!(decoded, payload, "{lvl:?}: dict round-trip mismatch");

            // A dictionary that does NOT appear in the input must not make
            // the output larger than the no-dict (raw) encoding: the
            // post-compress raw fallback covers incompressible-with-dict.
            let unrelated = lcg(99, 512);
            let with_bad_dict = compress(lvl, Some(&unrelated));
            assert!(
                with_bad_dict.len() <= no_dict.len() + 16,
                "{lvl:?}: unhelpful dict expanded output (with={}, no_dict={})",
                with_bad_dict.len(),
                no_dict.len(),
            );
        }
    }

    #[test]
    fn source_size_hint_with_dictionary_keeps_roundtrip_for_larger_payload() {
        let dict_id = 0xABCD_0005;
        let dict_content = b"abcd".repeat(1024); // 4 KiB dictionary history
        let dict = crate::decoding::Dictionary::from_raw_content(dict_id, dict_content).unwrap();
        let dict_for_decoder =
            crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024)).unwrap();
        let payload = b"abcd".repeat(1024); // 4 KiB payload
        let payload_len = payload.len() as u64;

        let mut hinted_output = Vec::new();
        let mut hinted = FrameCompressor::new(super::CompressionLevel::Fastest);
        hinted.set_dictionary(dict).unwrap();
        hinted.set_source_size_hint(payload_len);
        hinted.set_source(payload.as_slice());
        hinted.set_drain(&mut hinted_output);
        hinted.compress();

        let mut no_hint_output = Vec::new();
        let mut no_hint = FrameCompressor::new(super::CompressionLevel::Fastest);
        no_hint
            .set_dictionary(
                crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024))
                    .unwrap(),
            )
            .unwrap();
        no_hint.set_source(payload.as_slice());
        no_hint.set_drain(&mut no_hint_output);
        no_hint.compress();

        let hinted_window = crate::decoding::frame::read_frame_header(hinted_output.as_slice())
            .expect("encoded frame should have a header")
            .0
            .window_size()
            .expect("window size should be present");
        let no_hint_window = crate::decoding::frame::read_frame_header(no_hint_output.as_slice())
            .expect("encoded frame should have a header")
            .0
            .window_size()
            .expect("window size should be present");
        assert!(
            hinted_window <= no_hint_window,
            "source-size hint should not increase advertised window with dictionary priming",
        );

        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder
            .decode_all_to_vec(&hinted_output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn custom_matcher_without_dictionary_priming_does_not_advertise_dict_id() {
        let dict_id = 0xABCD_0003;
        let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
            .expect("raw dictionary should be valid");
        let payload = b"abcdefghabcdefgh";

        let mut output = Vec::new();
        let matcher = NoDictionaryMatcher::new(64);
        let mut compressor =
            FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(dict)
            .expect("dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
            .expect("encoded frame should have a header");
        assert_eq!(
            frame_header.dictionary_id(),
            None,
            "matchers that do not support dictionary priming must not advertise dictionary dependency"
        );

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[cfg(feature = "hash")]
    #[test]
    fn checksum_two_frames_reused_compressor() {
        // Compress the same data twice using the same compressor and verify that:
        // 1. The checksum written in each frame matches what the decoder calculates.
        // 2. The hasher is correctly reset between frames (no cross-contamination).
        //    If the hasher were NOT reset, the second frame's calculated checksum
        //    would differ from the one stored in the frame data, causing assert_eq to fail.
        let data: Vec<u8> = (0u8..=255).cycle().take(1024).collect();

        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);

        // --- Frame 1 ---
        let mut compressed1 = Vec::new();
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed1);
        compressor.compress();

        // --- Frame 2 (reuse the same compressor) ---
        let mut compressed2 = Vec::new();
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed2);
        compressor.compress();

        fn decode_and_collect(compressed: &[u8]) -> (Vec<u8>, Option<u32>, Option<u32>) {
            let mut decoder = FrameDecoder::new();
            let mut source = compressed;
            decoder.reset(&mut source).unwrap();
            while !decoder.is_finished() {
                decoder
                    .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                    .unwrap();
            }
            let mut decoded = Vec::new();
            decoder.collect_to_writer(&mut decoded).unwrap();
            (
                decoded,
                decoder.get_checksum_from_data(),
                decoder.get_calculated_checksum(),
            )
        }

        let (decoded1, chksum_from_data1, chksum_calculated1) = decode_and_collect(&compressed1);
        assert_eq!(decoded1, data, "frame 1: decoded data mismatch");
        assert_eq!(
            chksum_from_data1, chksum_calculated1,
            "frame 1: checksum mismatch"
        );

        let (decoded2, chksum_from_data2, chksum_calculated2) = decode_and_collect(&compressed2);
        assert_eq!(decoded2, data, "frame 2: decoded data mismatch");
        assert_eq!(
            chksum_from_data2, chksum_calculated2,
            "frame 2: checksum mismatch"
        );

        // Same data compressed twice must produce the same checksum.
        // If state leaked across frames, the second calculated checksum would differ.
        assert_eq!(
            chksum_from_data1, chksum_from_data2,
            "frame 1 and frame 2 should have the same checksum (same data, hash must reset per frame)"
        );
    }

    #[cfg(feature = "lsm")]
    #[test]
    fn frame_emit_info_decompressed_ranges_match_decoded_output() {
        // Part A correctness: the per-block `decompressed_size` captured during
        // encode (and the `decompressed_byte_range` prefix sum derived from it)
        // must describe the real decoded output exactly — one entry per
        // physical block, contiguous, summing to the full decompressed length.
        // A multi-block compressible payload exercises the Compressed-block
        // path (whose regenerated size is NOT on the wire, so it relies on the
        // encode-side capture this test guards).
        let mut data: Vec<u8> = Vec::with_capacity(400 * 1024);
        let mut x = 0x9E37_79B9u32;
        // Semi-repetitive: long runs interleaved with stride patterns so the
        // encoder produces several Compressed blocks rather than one giant Raw.
        while data.len() < 400 * 1024 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let run = 16 + (x as usize % 48);
            let byte = (x >> 24) as u8;
            for _ in 0..run {
                data.push(byte);
            }
            data.extend_from_slice(b"the quick brown fox jumps over the lazy dog\n");
        }

        // Cover both the single-block-per-chunk path (Default) and the
        // Level(16..=22) post-split path (multiple physical partitions per
        // input chunk), since lsm-tree compresses at zstd:22 and post-split
        // is the riskiest capture site (per-partition `src_size`).
        for level in [
            super::CompressionLevel::Default,
            super::CompressionLevel::Level(22),
        ] {
            let mut compressed = Vec::new();
            let mut compressor = FrameCompressor::new(level);
            // Pledge the source size so the high-level (22) window shrinks to
            // fit the payload — otherwise level 22's default 128 MiB window
            // exceeds the decoder's MAXIMUM_ALLOWED_WINDOW_SIZE cap. Still
            // >= 128 KiB, so post-split eligibility is preserved.
            compressor.set_source_size_hint(data.len() as u64);
            compressor.set_source(data.as_slice());
            compressor.set_drain(&mut compressed);
            compressor.compress();

            let info = compressor
                .last_frame_emit_info()
                .expect("emit info populated after compress")
                .clone();

            // Reference: full decode of the same frame.
            let mut decoder = FrameDecoder::new();
            let mut source = compressed.as_slice();
            decoder.reset(&mut source).unwrap();
            while !decoder.is_finished() {
                decoder
                    .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                    .unwrap();
            }
            let mut decoded = Vec::new();
            decoder.collect_to_writer(&mut decoded).unwrap();
            assert_eq!(decoded, data, "sanity: frame must round-trip ({level:?})");

            assert!(
                info.blocks.len() >= 2,
                "fixture must span multiple blocks to exercise the mapping ({level:?}, got {})",
                info.blocks.len()
            );
            assert!(
                info.blocks.last().unwrap().last_block,
                "final block must carry last_block ({level:?})"
            );

            // Per-block ranges: contiguous, zero-based, summing to the full output.
            let mut expected_start = 0u64;
            for i in 0..info.blocks.len() {
                let range = info
                    .decompressed_byte_range(i)
                    .expect("in-bounds block has a range");
                assert_eq!(
                    range.start, expected_start,
                    "block {i} range must start where the previous ended ({level:?})"
                );
                assert_eq!(
                    u64::from(info.blocks[i].decompressed_size),
                    range.end - range.start,
                    "block {i} decompressed_size must equal its range width ({level:?})"
                );
                expected_start = range.end;
            }
            assert_eq!(
                expected_start,
                decoded.len() as u64,
                "block decompressed sizes must sum to the full decoded length ({level:?})"
            );
            assert_eq!(
                info.decompressed_byte_range(info.blocks.len()),
                None,
                "out-of-range index yields None ({level:?})"
            );
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn fuzz_targets() {
        use std::io::Read;
        fn decode_szstd(data: &mut dyn std::io::Read) -> Vec<u8> {
            let mut decoder = crate::decoding::StreamingDecoder::new(data).unwrap();
            let mut result: Vec<u8> = Vec::new();
            decoder.read_to_end(&mut result).expect("Decoding failed");
            result
        }

        fn decode_szstd_writer(mut data: impl Read) -> Vec<u8> {
            let mut decoder = crate::decoding::FrameDecoder::new();
            decoder.reset(&mut data).unwrap();
            let mut result = vec![];
            while !decoder.is_finished() || decoder.can_collect() > 0 {
                decoder
                    .decode_blocks(
                        &mut data,
                        crate::decoding::BlockDecodingStrategy::UptoBytes(1024 * 1024),
                    )
                    .unwrap();
                decoder.collect_to_writer(&mut result).unwrap();
            }
            result
        }

        fn encode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
            zstd::stream::encode_all(std::io::Cursor::new(data), 3)
        }

        fn encode_szstd_uncompressed(data: &mut dyn std::io::Read) -> Vec<u8> {
            let mut input = Vec::new();
            data.read_to_end(&mut input).unwrap();

            crate::encoding::compress_to_vec(
                input.as_slice(),
                crate::encoding::CompressionLevel::Uncompressed,
            )
        }

        fn encode_szstd_compressed(data: &mut dyn std::io::Read) -> Vec<u8> {
            let mut input = Vec::new();
            data.read_to_end(&mut input).unwrap();

            crate::encoding::compress_to_vec(
                input.as_slice(),
                crate::encoding::CompressionLevel::Fastest,
            )
        }

        fn decode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
            let mut output = Vec::new();
            zstd::stream::copy_decode(data, &mut output)?;
            Ok(output)
        }
        if std::fs::exists("fuzz/artifacts/interop").unwrap_or(false) {
            for file in std::fs::read_dir("fuzz/artifacts/interop").unwrap() {
                if file.as_ref().unwrap().file_type().unwrap().is_file() {
                    let data = std::fs::read(file.unwrap().path()).unwrap();
                    let data = data.as_slice();
                    // Decoding
                    let compressed = encode_zstd(data).unwrap();
                    let decoded = decode_szstd(&mut compressed.as_slice());
                    let decoded2 = decode_szstd_writer(&mut compressed.as_slice());
                    assert!(
                        decoded == data,
                        "Decoded data did not match the original input during decompression"
                    );
                    assert_eq!(
                        decoded2, data,
                        "Decoded data did not match the original input during decompression"
                    );

                    // Encoding
                    // Uncompressed encoding
                    let mut input = data;
                    let compressed = encode_szstd_uncompressed(&mut input);
                    let decoded = decode_zstd(&compressed).unwrap();
                    assert_eq!(
                        decoded, data,
                        "Decoded data did not match the original input during compression"
                    );
                    // Compressed encoding
                    let mut input = data;
                    let compressed = encode_szstd_compressed(&mut input);
                    let decoded = decode_zstd(&compressed).unwrap();
                    assert_eq!(
                        decoded, data,
                        "Decoded data did not match the original input during compression"
                    );
                }
            }
        }
    }

    /// Homogeneous input — every byte the same — must NOT be split:
    /// both border histograms are identical (all 512 hits on a single
    /// slot), so `presplit_fingerprints_differ` returns `false` and the
    /// function takes the early-return path at
    /// `zstd_preSplit.c:214` returning `blockSize`.
    #[test]
    fn split_block_from_borders_keeps_homogeneous_block() {
        let block = vec![0xAAu8; MAX_BLOCK_SIZE as usize];
        let split = super::split_block_from_borders(&block);
        assert_eq!(split, MAX_BLOCK_SIZE as usize);
    }

    /// Heterogeneous input — first half all zeros, second half a
    /// counter sequence — has clearly distinguishable border
    /// histograms, so the borders heuristic decides to split.
    ///
    /// The transition sits at exactly the block midpoint, so the
    /// middle 512-byte sample (`block[mid-256..mid+256]`) is half
    /// zeros + half counter values. That makes it roughly
    /// equidistant from both border fingerprints — the
    /// `abs_diff(dist_from_begin, dist_from_end) < min_distance`
    /// branch fires and the heuristic returns the midpoint (64 KiB)
    /// per `zstd_preSplit.c:222`. The test asserts the exact value
    /// rather than just "one of {32K, 64K, 96K}" so a regression
    /// to a different quantised arm cannot silently slip through.
    #[test]
    fn split_block_from_borders_returns_midpoint_for_centred_transition() {
        let mut block = vec![0u8; MAX_BLOCK_SIZE as usize];
        for (i, byte) in block
            .iter_mut()
            .enumerate()
            .skip(MAX_BLOCK_SIZE as usize / 2)
        {
            *byte = (i % 251 + 1) as u8;
        }
        let split = super::split_block_from_borders(&block);
        assert_eq!(
            split,
            64 * 1024,
            "centred-transition fixture must take the symmetric \
             midpoint arm (`abs_diff < min_distance`), got {split}"
        );
    }

    /// `level_pre_split` resolves the per-level split knob through the
    /// `LevelParams` table, mirroring the donor `splitLevels[]` by strategy
    /// (`ZSTD_optimalBlockSize`): fast → 0 (from-borders), dfast → 1,
    /// greedy/lazy → 2, lazy2/btlazy2 (Lazy tag at depth 2) → 3,
    /// btopt/btultra/btultra2 → 4. `Uncompressed` has no numeric level so it
    /// stays `None`.
    #[test]
    fn pre_split_level_dispatches_by_compression_level() {
        use crate::encoding::CompressionLevel;
        use crate::encoding::match_generator::level_pre_split;
        assert_eq!(level_pre_split(CompressionLevel::Uncompressed), None);
        // Fastest = level 1 (fast) → 0 (from-borders).
        assert_eq!(level_pre_split(CompressionLevel::Fastest), Some(0));
        // Default = level 3 (dfast) → 1.
        assert_eq!(level_pre_split(CompressionLevel::Default), Some(1));
        // Better is a pure alias for level 7 (lazy): same as Level(7).
        assert_eq!(
            level_pre_split(CompressionLevel::Better),
            level_pre_split(CompressionLevel::Level(7)),
        );
        // Best is a pure alias for level 11 (lazy): pin it to the numeric
        // route so the named path can't drift from the pre-split table.
        assert_eq!(
            level_pre_split(CompressionLevel::Best),
            level_pre_split(CompressionLevel::Level(11)),
        );
        assert_eq!(level_pre_split(CompressionLevel::Level(2)), Some(0)); // fast
        assert_eq!(level_pre_split(CompressionLevel::Level(4)), Some(1)); // dfast
        assert_eq!(level_pre_split(CompressionLevel::Level(5)), Some(2)); // greedy
        assert_eq!(level_pre_split(CompressionLevel::Level(7)), Some(2)); // lazy (depth 1)
        assert_eq!(level_pre_split(CompressionLevel::Level(8)), Some(3)); // lazy2 lower bound
        assert_eq!(level_pre_split(CompressionLevel::Level(11)), Some(3)); // lazy2 (depth 2)
        assert_eq!(level_pre_split(CompressionLevel::Level(12)), Some(3)); // lazy2 upper bound
        assert_eq!(level_pre_split(CompressionLevel::Level(13)), Some(3)); // btlazy2 lower bound
        assert_eq!(level_pre_split(CompressionLevel::Level(15)), Some(3)); // btlazy2 (depth 2)
        assert_eq!(level_pre_split(CompressionLevel::Level(16)), Some(4)); // btopt
        assert_eq!(level_pre_split(CompressionLevel::Level(22)), Some(4)); // btultra2
    }

    /// End-to-end: a 256 KB payload whose SECOND 128 KB donor block carries
    /// an intra-block fingerprint transition, compressed at Level(5)
    /// (greedy, the pre-split path this revision routes through the cheap
    /// chunk splitter), round-trips through the crate's own decoder.
    ///
    /// The transition lives in the second block on purpose: the donor
    /// `savings < 3` gate skips splitting the first block (savings start at
    /// 0), so the first block is a homogeneous compressible run that banks
    /// savings, and the second block is the one whose intra-block transition
    /// `split_block_by_chunks()` resolves into a sub-block boundary (the
    /// `pending_input.split_off(...)` path). The test asserts that split
    /// decision directly so it cannot silently stop exercising the path if
    /// the fixture or params drift, then proves the emitted split frame
    /// round-trips. Level 13 (lazy) no longer pre-splits, hence Level 5.
    #[test]
    fn greedy_chunk_split_roundtrips_through_own_decoder() {
        use crate::encoding::CompressionLevel;
        let mut data = vec![0u8; 256 * 1024];
        // First 128 KB: homogeneous low-entropy run (compressible, banks
        // the savings the donor gate needs). Second 128 KB: low-entropy run
        // for its first half, then a counter sequence: a clear intra-block
        // fingerprint transition at the 192 KB midpoint for the chunk
        // splitter to find.
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = if i < 192 * 1024 {
                (i & 0x07) as u8
            } else {
                (i % 251 + 1) as u8
            };
        }

        // Directly assert the chunk splitter resolves the second block's
        // intra-block transition into a sub-block boundary once savings have
        // accrued (the compressible first block banks well over the gate).
        let second_block = &data[128 * 1024..];
        let split = super::optimal_block_size(
            CompressionLevel::Level(5),
            second_block,
            second_block.len(),
            MAX_BLOCK_SIZE as usize,
            100,
        );
        assert!(
            split < MAX_BLOCK_SIZE as usize,
            "second donor block must chunk-split at its intra-block transition, got {split}",
        );

        let mut compressed = Vec::new();
        let mut compressor = FrameCompressor::new(CompressionLevel::Level(5));
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut source = compressed.as_slice();
        decoder
            .reset(&mut source)
            .expect("frame header should parse");
        while !decoder.is_finished() {
            decoder
                .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                .expect("decode should succeed");
        }
        let mut decoded = Vec::with_capacity(data.len());
        decoder.collect_to_writer(&mut decoded).unwrap();
        assert_eq!(decoded, data, "roundtrip must reproduce the input verbatim");
    }

    /// Outside-diff coverage for the FAST one-shot path.
    /// `compress_slice_to_vec` / `compress_independent_frame` on a Fast level
    /// routes through `run_borrowed_block_loop` (not the owned loop the test
    /// above covers), which must honour `optimal_block_size` and emit a
    /// sub-`MAX_BLOCK_SIZE` boundary rather than fixed 128 KiB blocks. A
    /// 256 KiB input is two 128 KiB blocks when unsplit; a chunk boundary in
    /// the second block yields >= 3 decoded blocks, asserted on the round-trip.
    #[test]
    fn fast_oneshot_borrowed_split_emits_subblock() {
        use crate::encoding::CompressionLevel;
        // First 192 KiB: homogeneous zero run (banks the savings the split
        // gate needs). The second 128 KiB block flips to a counter sequence
        // at its 64 KiB midpoint (the 192 KiB mark) — a fingerprint
        // transition the Fast from-borders splitter (split level 0) resolves
        // into a sub-block boundary.
        let mut data = vec![0u8; 256 * 1024];
        for (i, byte) in data.iter_mut().enumerate() {
            if i >= 192 * 1024 {
                *byte = (i % 251 + 1) as u8;
            }
        }

        // Pin the splitter decision for the Fast path directly (mirrors the
        // greedy test): the second donor block must resolve to a sub-block
        // boundary, so the >= 3 block count below cannot pass vacuously.
        let second_block = &data[128 * 1024..];
        assert!(
            super::optimal_block_size(
                CompressionLevel::Fastest,
                second_block,
                second_block.len(),
                MAX_BLOCK_SIZE as usize,
                100,
            ) < MAX_BLOCK_SIZE as usize,
            "fixture must resolve to a sub-block split in the second donor block",
        );

        // Drive the borrowed one-shot route explicitly (Fast level ->
        // run_borrowed_block_loop via compress_independent_frame).
        let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
        let frame = compressor.compress_independent_frame(&data);

        let mut decoder = FrameDecoder::new();
        let mut source = frame.as_slice();
        decoder
            .reset(&mut source)
            .expect("frame header should parse");
        while !decoder.is_finished() {
            decoder
                .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                .expect("decode should succeed");
        }
        let mut decoded = Vec::with_capacity(data.len());
        decoder.collect_to_writer(&mut decoded).unwrap();
        assert_eq!(decoded, data, "roundtrip must reproduce the input verbatim");
        assert!(
            decoder.blocks_decoded() >= 3,
            "fast one-shot borrowed path must split the second donor block \
             (256 KiB unsplit = 2 blocks), got {} blocks",
            decoder.blocks_decoded(),
        );
    }

    /// Regression: `set_compression_level` followed by `compress()` must
    /// refresh `state.strategy_tag` through the reset-time sync so the
    /// literal-compression gates (`min_literals_to_compress`,
    /// `min_gain`) use the NEW level's strategy. Picks a level pair
    /// that genuinely crosses strategy bands — `Fastest` resolves to
    /// `Fast`, `Level(20)` resolves to `BtUltra2` — so a missed sync
    /// would leave the construction-time tag visible and trip the
    /// assertion. `CompressionLevel::Best` would also pass type-wise
    /// but resolves to `Lazy` today, which keeps `min_literals_to_compress`
    /// in the same `shift=3 → 64-byte` band as `Fast` and weakens the
    /// signal that the gate floor actually moved.
    #[cfg(feature = "std")]
    #[test]
    fn set_compression_level_then_compress_refreshes_strategy_tag() {
        use super::CompressionLevel;
        use crate::encoding::strategy::StrategyTag;

        let data = vec![0xABu8; 256];
        let mut out = Vec::new();
        let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
        let initial_tag = compressor.state.strategy_tag;
        assert_eq!(
            initial_tag,
            StrategyTag::for_compression_level(CompressionLevel::Fastest),
            "construction-time strategy_tag must reflect initial level",
        );

        // Switch to a level whose resolved strategy lives in a different
        // band, then run a full compress cycle — the matcher.reset()
        // inside `compress` is the only site that can refresh the tag.
        let new_level = CompressionLevel::Level(20);
        compressor.set_compression_level(new_level);
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut out);
        compressor.compress();

        let new_tag = compressor.state.strategy_tag;
        let expected = StrategyTag::for_compression_level(new_level);
        assert_eq!(
            new_tag, expected,
            "strategy_tag must follow set_compression_level → compress, \
             got {new_tag:?} expected {expected:?}",
        );
        assert_eq!(
            expected,
            StrategyTag::BtUltra2,
            "test fixture invariant: Level(20) must resolve to BtUltra2 \
             so the post-switch tag visibly crosses the band boundary",
        );
        assert_ne!(
            new_tag, initial_tag,
            "test fixture invariant: chosen levels must resolve to \
             different StrategyTag variants",
        );
    }

    /// Magicless mode (`ZSTD_f_zstd1_magicless`): encoded frame
    /// MUST NOT start with the 4-byte magic prefix, AND must
    /// round-trip through a magicless-aware decoder.
    #[test]
    fn magicless_frame_omits_magic_and_roundtrips() {
        use crate::common::MAGIC_NUM;
        let input: alloc::vec::Vec<u8> = (0..512u32).map(|i| (i ^ 0xA5) as u8).collect();

        // Encode with magicless = true.
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Default);
        compressor.set_magicless(true);
        compressor.set_source(input.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        // 1. Encoded output must NOT begin with the zstd magic number.
        assert!(
            !output.starts_with(&MAGIC_NUM.to_le_bytes()),
            "magicless frame must omit the 4-byte magic prefix",
        );

        // 2. A magicless-aware decoder must round-trip the payload.
        let mut decoder = crate::decoding::FrameDecoder::new();
        decoder.set_magicless(true);
        let mut cursor: &[u8] = output.as_slice();
        decoder.init(&mut cursor).expect("magicless init");
        decoder
            .decode_blocks(&mut cursor, crate::decoding::BlockDecodingStrategy::All)
            .expect("decode_blocks");
        let mut decoded: Vec<u8> = Vec::new();
        decoder
            .collect_to_writer(&mut decoded)
            .expect("collect_to_writer");
        assert_eq!(decoded, input, "magicless roundtrip must preserve bytes");

        // 3. A standard (magicful) decoder MUST reject a magicless
        //    frame at the header-read step — the first 4 bytes are
        //    the frame-header descriptor + window / dictionary / FCS
        //    metadata, not the magic. We accept either
        //    `BadMagicNumber` (typical case: first 4 bytes don't
        //    match `MAGIC_NUM` and don't fall in the skippable-frame
        //    magic range) or `SkipFrame` (rare: the first 4 bytes
        //    coincidentally land in `0x184D2A50..=0x184D2A5F`). Both
        //    prove the standard decoder did not treat the bytes as a
        //    real magicful frame.
        use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        let mut std_decoder = crate::decoding::FrameDecoder::new();
        let std_init = std_decoder.init(output.as_slice());
        match std_init {
            Err(FrameDecoderError::ReadFrameHeaderError(
                ReadFrameHeaderError::BadMagicNumber(_) | ReadFrameHeaderError::SkipFrame { .. },
            )) => {}
            other => panic!(
                "standard decoder must reject a magicless frame with \
                 ReadFrameHeaderError::BadMagicNumber or SkipFrame, got {other:?}",
            ),
        }
    }

    /// A reused `FrameCompressor` must emit byte-identical frames to a
    /// fresh compressor per input across both the borrowed (Fast) and
    /// owned (Dfast/Lazy/Greedy/Uncompressed) backends. This proves
    /// `prepare_frame` fully resets the per-frame state (matcher window,
    /// content hasher, FSE/Huffman seeds) between independent frames; a
    /// missed reset would corrupt frame N>=2's header checksum or matches.
    /// Each emitted frame must also round-trip.
    #[test]
    fn compress_independent_frame_reuse_matches_fresh_and_roundtrips() {
        use crate::encoding::{CompressionLevel, compress_slice_to_vec};
        let levels = [
            CompressionLevel::Uncompressed,
            CompressionLevel::Fastest,
            CompressionLevel::Default,
            CompressionLevel::Better,
            CompressionLevel::Best,
            CompressionLevel::Level(5),
        ];
        let inputs: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0x00],
            b"the quick brown fox jumps over the lazy dog\n".to_vec(),
            vec![0x7Eu8; 50_000],          // highly compressible
            generate_data(0xABCD, 70_000), // pseudo-random
            generate_data(0x1234, 200_000),
        ];
        for level in levels {
            let mut cctx: FrameCompressor = FrameCompressor::new(level);
            for data in &inputs {
                let reused = cctx.compress_independent_frame(data);
                let fresh = compress_slice_to_vec(data, level);
                assert_eq!(
                    reused,
                    fresh,
                    "reused frame != fresh frame for len={} level={:?}",
                    data.len(),
                    level,
                );
                let mut decoder = FrameDecoder::new();
                let mut decoded = Vec::with_capacity(data.len());
                decoder.decode_all_to_vec(&reused, &mut decoded).unwrap();
                assert_eq!(
                    decoded,
                    *data,
                    "roundtrip failed for len={} level={:?}",
                    data.len(),
                    level,
                );
            }
        }
    }

    /// `compress_independent_frame_into` must replace (not append to) the
    /// caller's buffer each call, so a smaller frame after a larger one
    /// yields exactly the smaller frame, and the reused buffer's content
    /// matches a fresh compression of the same input.
    #[test]
    fn compress_independent_frame_into_replaces_buffer_contents() {
        use crate::encoding::{CompressionLevel, compress_slice_to_vec};
        let large = vec![0x11u8; 40_000];
        let small = b"short payload".to_vec();
        let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
        let mut out = Vec::new();
        cctx.compress_independent_frame_into(&large, &mut out);
        let frame_large = out.clone();
        // Reusing the same buffer for a smaller frame must clear it first.
        cctx.compress_independent_frame_into(&small, &mut out);
        assert_eq!(
            out,
            compress_slice_to_vec(&small, CompressionLevel::Default),
            "reused buffer must hold exactly the second frame",
        );
        // The first frame, captured before reuse, still round-trips.
        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(large.len());
        decoder
            .decode_all_to_vec(&frame_large, &mut decoded)
            .unwrap();
        assert_eq!(decoded, large);
    }

    /// A sticky dictionary set once on a reused compressor must be primed
    /// into every independent frame (mirroring `ZSTD_CCtx_loadDictionary`):
    /// each frame decodes with the dictionary and is byte-identical to a
    /// fresh compressor carrying the same dictionary. This proves
    /// `prepare_frame` re-primes the dictionary (matcher content + offset
    /// history + entropy seed) every call rather than only on the first.
    #[test]
    fn compress_independent_frame_reuses_sticky_dictionary() {
        use crate::encoding::CompressionLevel;
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let dict_content = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
        let mut payload_a = Vec::new();
        for _ in 0..8 {
            payload_a.extend_from_slice(&dict_content.dict_content[..2048]);
        }
        let payload_b = b"a different second frame payload, still dict-attached".to_vec();
        let inputs = [payload_a, payload_b];

        let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
        cctx.set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");

        for data in &inputs {
            let reused = cctx.compress_independent_frame(data);
            // Fresh compressor carrying the same sticky dictionary.
            let mut fresh_enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
            fresh_enc
                .set_dictionary_from_bytes(dict_raw)
                .expect("dictionary bytes should parse");
            let fresh = fresh_enc.compress_independent_frame(data);
            assert_eq!(
                reused,
                fresh,
                "reused dict frame != fresh dict frame, len={}",
                data.len(),
            );
            // Round-trip with the dictionary on the decode side.
            let dict_for_decoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
            let mut decoder = FrameDecoder::new();
            decoder.add_dict(dict_for_decoder).unwrap();
            let mut decoded = Vec::with_capacity(data.len());
            decoder.decode_all_to_vec(&reused, &mut decoded).unwrap();
            assert_eq!(&decoded, data, "dict roundtrip failed, len={}", data.len());
        }
    }
}
