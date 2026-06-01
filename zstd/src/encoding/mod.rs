//! Zstandard encoder — frame compression, streaming, dictionary support.
//!
//! Four entry points cover the common use cases:
//!
//! * [`compress`] — one-shot helper that builds a self-contained
//!   Zstandard frame from a `Read` source to a `Write` sink. The
//!   input is consumed incrementally from `Read`, so input buffering
//!   stays bounded; however, the compressed output is buffered in
//!   memory until the frame is complete so the Frame Content Size
//!   field can be filled in the header — peak memory is
//!   `O(compressed_size)` (worst-case `O(input_size)` for
//!   incompressible payloads, plus a small frame overhead). The
//!   savings vs [`compress_to_vec`] come from not materialising the
//!   input alongside the output.
//! * [`compress_to_vec`] — same one-shot path as [`compress`] but
//!   the input is eagerly drained into an internal `Vec` first
//!   (`read_to_end`) so the encoder can be handed a `&[u8]` and a
//!   precise source-size hint. Peak memory is therefore ≈
//!   `input_size + output_size`; prefer [`compress`] or
//!   [`StreamingEncoder`] when the input is large or unbounded.
//! * [`StreamingEncoder`] — implements [`crate::io::Write`], which
//!   re-exports [`std::io::Write`] under the `std` feature and falls
//!   back to a `no_std`-friendly trait otherwise. Accepts bytes
//!   incrementally and flushes compressed output as blocks fill.
//!   Requires `set_pledged_content_size` before the first write if
//!   the Frame Content Size field is to be populated.
//! * [`FrameCompressor`] — lower-level builder that owns the matcher and
//!   the per-frame configuration; the streaming and one-shot helpers are
//!   thin wrappers over it. Reach for it when you need to swap in a custom
//!   [`Matcher`] implementation or share the matcher across frames.
//!
//! Compression intensity is selected via [`CompressionLevel`], which
//! provides both named presets (`Fastest`, `Default`, `Better`, `Best`) and
//! numeric levels (`from_level(n)`) that mirror C zstd's level numbering
//! (negative for ultra-fast, `0` = default, `1..=22` for the standard
//! range).
//!
//! All produced frames are valid RFC 8878 Zstandard streams and decode
//! through both this crate's [`crate::decoding`] module and upstream C zstd.

pub(crate) mod block_header;
pub(crate) mod blocks;
pub(crate) mod fastpath;
pub(crate) mod frame_header;
pub(crate) mod incompressible;
pub(crate) mod match_generator;
pub(crate) mod util;

// `#111` encoder architecture rewrite. `cost_model`, `opt`,
// `strategy`, `dfast`, `row`, and `simple` host the relocated
// cost-model types, the optimal-parser plain-data types, the
// const-generic [`strategy::Strategy`] trait + per-level [`strategy::
// StrategyTag`] dispatcher, and the Dfast / Row / Simple matchers
// respectively. `match_table::helpers` hosts the shared match-finder
// primitives. The rewrite plan is tracked in
// <https://github.com/structured-world/structured-zstd/issues/111>;
// per-phase boundaries are `perf/post-pr-110-baseline` (start),
// `perf/post-pr-121-baseline` (post-Phase-2).
pub(crate) mod bt;
pub(crate) mod cost_model;
pub(crate) mod dfast;
pub(crate) mod hc;
// LDM uses `twox_hash::XxHash64` (per-window XXH64 over the
// `min_match_length` byte slice, donor `zstd_ldm.c:315`). The
// `twox-hash` dependency is gated behind the `hash` feature so
// `default-features = false` builds (no_std, embedded) don't pull
// it in. `BtMatcher::ldm_producer` and the `cfg(feature = "hash")`
// blocks inside `BtMatcher::prepare_ldm_candidates` /
// `BtMatcher::reset` carry the same gate; the call site in
// `match_generator.rs::start_matching_optimal` invokes
// `prepare_ldm_candidates` unconditionally because the
// gating is internal to the method body (under
// `not(feature = "hash")` the method shrinks to the legacy
// `ldm_sequences.clear()` stub).
#[cfg(feature = "hash")]
pub(crate) mod ldm;
pub(crate) mod match_table;
pub(crate) mod opt;
pub(crate) mod row;
pub(crate) mod simple;
pub(crate) mod strategy;

pub(crate) mod frame_compressor;
#[cfg(feature = "lsm")]
pub mod frame_emit_info;
mod levels;
#[cfg(feature = "bench_internals")]
pub mod sequence_capture;
mod streaming_encoder;
pub use frame_compressor::{EncoderDictionary, FrameCompressor};
#[cfg(feature = "lsm")]
pub use frame_emit_info::{BlockType, FrameBlock, FrameEmitInfo};
pub use match_generator::MatchGeneratorDriver;
pub use streaming_encoder::StreamingEncoder;

use crate::io::{Read, Write};
use alloc::vec::Vec;

pub(crate) const BETTER_WINDOW_LOG: u8 = 23;

/// Worst-case compressed-size bound for `src_size` uncompressed
/// bytes. Mirrors the upstream `ZSTD_COMPRESSBOUND(srcSize)` macro
/// from `lib/zstd.h` (zstd 1.5.7) — the C *macro*, NOT the public
/// `ZSTD_compressBound()` function. The function adds frame/block-
/// header headroom in some code paths; the macro is a tighter
/// expression suitable for sizing an internal output Vec, which is
/// the only call site here. Callers that need a hard upper bound on
/// the wire-format compressed size should NOT reuse this — use the
/// `zstd-sys` function call directly. If the macro changes in a
/// future upstream revision, treat the upstream header as
/// authoritative and re-derive this function — do not trust the
/// inline copy without cross-checking.
///
/// Inline approximation (this revision):
///
/// ```text
/// srcSize
///   + (srcSize >> 8)
///   + (srcSize < 128 KiB ? ((128 KiB - srcSize) >> 11) : 0)
/// ```
///
/// Consulted by [`compress_slice_to_vec`] for the small-input branch
/// of its output-`Vec` seed: the seed is
/// `min(compress_bound(src), OUTPUT_BLOCK_CAP = 128 KiB)`, so for
/// inputs that fit within one donor block (`≤ ~128 KiB`) the
/// destination never reallocates inside the measured window. Without
/// the bound, `Vec::push` growth doubles in powers of two and pins
/// `~2 × final_compressed_size` resident at the last realloc, which
/// shows up as ~1 MiB of peak-RSS noise on 1 MiB inputs and prevents
/// FFI-parity on the memory bench. For inputs larger than the cap the
/// `Vec` still grows by amortized doubling — see
/// [`compress_slice_to_vec`] for the trade-off rationale.
#[inline]
pub(crate) const fn compress_bound(src_size: usize) -> usize {
    const SMALL_INPUT_THRESHOLD: usize = 128 * 1024;
    let tail = if src_size < SMALL_INPUT_THRESHOLD {
        (SMALL_INPUT_THRESHOLD - src_size) >> 11
    } else {
        0
    };
    // `saturating_add` guards against the corner case where `src_size`
    // is close to `usize::MAX`. The current sole caller
    // (`compress_slice_to_vec`) clamps the result with `.min(OUTPUT_BLOCK_CAP)`
    // so an overflow there would still produce a sensible cap, but
    // future `pub(crate)` callers may use the raw bound — keep the
    // arithmetic itself overflow-safe.
    src_size.saturating_add(src_size >> 8).saturating_add(tail)
}

/// Convenience function to compress some source into a target without reusing any resources of the compressor
/// ```rust
/// use structured_zstd::encoding::{compress, CompressionLevel};
/// let data: &[u8] = &[0,0,0,0,0,0,0,0,0,0,0,0];
/// let mut target = Vec::new();
/// compress(data, &mut target, CompressionLevel::Fastest);
/// ```
pub fn compress<R: Read, W: Write>(source: R, target: W, level: CompressionLevel) {
    let mut frame_enc = FrameCompressor::new(level);
    frame_enc.set_source(source);
    frame_enc.set_drain(target);
    frame_enc.compress();
}

/// Convenience function to compress some source into a Vec without reusing any resources of the compressor.
///
/// This helper eagerly buffers the full input (`Read`) before compression so it
/// can provide a source-size hint to the one-shot encoder path. Peak memory can
/// therefore be roughly `input_size + output_size`. For very large payloads or
/// tighter memory budgets, prefer streaming APIs such as [`StreamingEncoder`].
///
/// **Peak-memory shape change in this revision.** The implementation
/// delegates to [`compress_slice_to_vec`], which seeds the output
/// `Vec` with `min(compress_bound(input.len()), OUTPUT_BLOCK_CAP =
/// 128 KiB)` instead of the previous `Vec::new()` (zero-capacity +
/// power-of-two growth). For inputs in the few-KiB to ~128 KiB range
/// this is a strict improvement (no doubling spikes inside the
/// measured window). For inputs significantly larger than 128 KiB the
/// allocation curve still grows by amortized doubling but starts from
/// a 128 KiB floor rather than 0. Downstream consumers that measure
/// peak RSS on this entry point will see a different curve than
/// pre-revision; bench shape, not steady-state, is what changed.
///
/// **This is NOT a streaming API.** The source is fully buffered
/// into a `Vec<u8>` before any compression work begins, so peak input
/// memory is bounded by `source.len()` (not "constant regardless of
/// payload size" as a stream-shaped encoder would offer). The RSS
/// notes below apply to the materialization-then-compress shape; if
/// the source is large enough that holding it in memory is not
/// acceptable, use [`StreamingEncoder`] which consumes chunks
/// incrementally without the up-front Vec build.
///
/// The other side of the peak shape is the input buffering: this
/// helper drives `read_to_end` to materialize the full source into a
/// `Vec<u8>` before forwarding the slice to [`compress_slice_to_vec`].
/// For a `Read` whose size is unknown ahead of time, `read_to_end`
/// grows that input `Vec` via power-of-two doubling — peak input
/// allocation can be up to 2× the final source length transiently.
/// At the moment that input buffer crosses ~128 KiB the output Vec
/// seed kicks in concurrently. The total live working set on this
/// entry point is approximately
/// `input.capacity() + output_vec_seed + internal_accumulators`,
/// where `output_vec_seed` is `min(compress_bound(input.len()), 128
/// KiB)` and `internal_accumulators` covers
/// `FrameCompressor::all_blocks` (pre-reserved at frame start, up to
/// ~130 KiB at default block cap) plus per-block scratch (hash tables,
/// literal/sequence staging). Round the helper's RSS peak to
/// `input.capacity() + output_vec_seed + ~130 KiB internal +
/// per-block scratch` rather than the bare `input.capacity() + 128
/// KiB` figure quoted in earlier revisions, which only accounted for
/// the output seed. [`StreamingEncoder`] avoids the input
/// materialization step entirely and is the right entry point when
/// the source is large or unbounded.
///
/// ```rust
/// use structured_zstd::encoding::{compress_to_vec, CompressionLevel};
/// let data: &[u8] = &[0,0,0,0,0,0,0,0,0,0,0,0];
/// let compressed = compress_to_vec(data, CompressionLevel::Fastest);
/// ```
pub fn compress_to_vec<R: Read>(source: R, level: CompressionLevel) -> Vec<u8> {
    let mut source = source;
    let mut input = Vec::new();
    source.read_to_end(&mut input).unwrap();
    compress_slice_to_vec(input.as_slice(), level)
}

/// Compress a contiguous byte slice into a fresh `Vec<u8>` without
/// the input-buffering step that [`compress_to_vec`] performs to
/// adapt a `Read` source. Donor-parity peak-memory shape: the input
/// is read by reference, and the output `Vec` is seeded with
/// `min(compress_bound(src), OUTPUT_BLOCK_CAP = 128 KiB)`.
///
/// The seed is intentionally capped at one donor block
/// (`OUTPUT_BLOCK_CAP = 128 KiB`). Three regimes:
///
/// * Inputs up to ~127 KiB where `compress_bound(src) <
///   OUTPUT_BLOCK_CAP` — the `min` picks the tighter bound and the
///   `Vec` never reallocates inside the measured window (cheap, no
///   doubling spikes, no over-allocation).
/// * Inputs around 128 KiB where `compress_bound(src) >=
///   OUTPUT_BLOCK_CAP` — the `min` clamps to the cap, so the "no
///   reallocation" property holds only as long as the actual
///   compressed output also fits within `OUTPUT_BLOCK_CAP`. For
///   high-entropy inputs near the cap boundary the `Vec` may grow
///   by one doubling step before the next block lands.
/// * Larger inputs — the `Vec` grows via amortized doubling, with
///   peak transient memory ≈ 2× current compressed size at the last
///   realloc. Deliberate trade-off against pinning
///   `compress_bound(src)` upfront, which on the 100 MiB
///   `large-log-stream` scenario pinned 100.4 MiB even though the
///   actual compressed output was ≪ 1 MiB.
///
/// # Panics
///
/// Panics on encoder error (matches the failure surface of
/// [`compress_to_vec`], which this function backs). The internal
/// [`FrameCompressor::compress`] call propagates `io::Error` from the
/// output [`Vec`] writer; under the in-tree default writer that
/// channel is infallible and any error becomes a `panic`. Out-of-
/// memory during `Vec::with_capacity` or the encoder's per-block
/// scratch allocations is handled by the global allocator's abort
/// policy. Migrating to a fallible variant requires plumbing
/// `Result` through `FrameCompressor::compress` — out of scope for
/// the slice/Vec entry points which mirror the donor `ZSTD_compress`
/// shape (no error return on the bulk path).
///
/// ```rust
/// use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
/// let data: &[u8] = &[0,0,0,0,0,0,0,0,0,0,0,0];
/// let compressed = compress_slice_to_vec(data, CompressionLevel::Fastest);
/// ```
pub fn compress_slice_to_vec(source: &[u8], level: CompressionLevel) -> Vec<u8> {
    // Initial capacity sized to a single output block, matching donor
    // `ZSTD_CStreamOutSize()` (≈ `ZSTD_BLOCKSIZE_MAX` + headroom). For
    // small inputs `compress_bound(src)` is smaller than this block —
    // use the tighter bound to avoid over-allocation. For larger inputs
    // the `Vec` grows via amortized doubling, with peak ≈ 2×current
    // compressed size at the last realloc (donor streaming shape: the
    // caller buffer is one block, not `ZSTD_compressBound(srcSize)`).
    // Pre-allocating `compress_bound(src)` here would charge worst-case
    // expansion against peak even on highly compressible inputs — on
    // the 100 MiB `large-log-stream` scenario that single allocation
    // dominated the encoder's measured footprint (`compress_bound` ≈
    // 100.4 MiB pinned, actual compressed output ≪ 1 MiB).
    const OUTPUT_BLOCK_CAP: usize = 128 * 1024;
    let cap = compress_bound(source.len()).min(OUTPUT_BLOCK_CAP);
    let mut vec = Vec::with_capacity(cap);
    let mut frame_enc = FrameCompressor::new(level);
    frame_enc.set_source_size_hint(source.len() as u64);
    frame_enc.set_source(source);
    frame_enc.set_drain(&mut vec);
    frame_enc.compress();
    vec
}

/// The compression mode used impacts the speed of compression,
/// and resulting compression ratios. Faster compression will result
/// in worse compression ratios, and vice versa.
#[derive(Copy, Clone, Debug)]
pub enum CompressionLevel {
    /// This level does not compress the data at all, and simply wraps
    /// it in a Zstandard frame.
    Uncompressed,
    /// This level is roughly equivalent to Zstd compression level 1
    Fastest,
    /// This level uses the crate's dedicated `dfast`-style matcher to
    /// target a better speed/ratio tradeoff than [`CompressionLevel::Fastest`].
    ///
    /// It represents this crate's "default" compression setting and may
    /// evolve in future versions as the implementation moves closer to
    /// reference zstd level 3 behavior.
    Default,
    /// This level is roughly equivalent to Zstd level 7.
    ///
    /// Uses the hash-chain matcher with a lazy2 matching strategy: the encoder
    /// evaluates up to two positions ahead before committing to a match,
    /// trading speed for a better compression ratio than [`CompressionLevel::Default`].
    Better,
    /// This level is roughly equivalent to Zstd level 11.
    ///
    /// Uses the hash-chain matcher with a deep lazy2 matching strategy and
    /// a 16 MiB window. Compared to [`CompressionLevel::Better`], this level
    /// uses larger hash and chain tables (2 M / 1 M entries vs 1 M / 512 K),
    /// a deeper search (32 candidates vs 16), and a higher target match
    /// length (128 vs 48), trading speed for the best compression ratio
    /// available in this crate.
    Best,
    /// Numeric compression level.
    ///
    /// Levels 1–22 correspond to the C zstd level numbering.  Higher values
    /// produce smaller output at the cost of more CPU time.  Negative values
    /// select ultra-fast modes that trade ratio for speed.  Level 0 is
    /// treated as [`DEFAULT_LEVEL`](Self::DEFAULT_LEVEL), matching C zstd
    /// semantics.
    ///
    /// Named variants map to specific numeric levels:
    /// [`Fastest`](Self::Fastest) = 1, [`Default`](Self::Default) = 3,
    /// [`Better`](Self::Better) = 7, [`Best`](Self::Best) = 11.
    /// [`Best`](Self::Best) remains the highest-ratio named preset, but
    /// [`Level`](Self::Level) values above 11 can target stronger (slower)
    /// tuning than the named hierarchy.
    ///
    /// Levels above 11 use progressively larger windows and deeper search.
    /// Levels 16–17 use a `btopt`-style price parser, 18–19 use `btultra`,
    /// and 20–22 use a `btultra2`-style two-pass selection profile.
    ///
    /// Semver note: this variant was added after the initial enum shape and
    /// is a breaking API change for downstream crates that exhaustively
    /// `match` on [`CompressionLevel`] without a wildcard arm.
    Level(i32),
}

impl CompressionLevel {
    /// The minimum supported numeric compression level (ultra-fast mode).
    pub const MIN_LEVEL: i32 = -131072;
    /// The maximum supported numeric compression level.
    pub const MAX_LEVEL: i32 = 22;
    /// The default numeric compression level (equivalent to [`Default`](Self::Default)).
    pub const DEFAULT_LEVEL: i32 = 3;

    /// Create a compression level from a numeric value.
    ///
    /// Returns named variants for canonical levels (`0`/`3`, `1`, `7`, `11`)
    /// and [`Level`](Self::Level) for all other values.
    ///
    /// With the default matcher backend (`MatchGeneratorDriver`), values
    /// outside [`MIN_LEVEL`](Self::MIN_LEVEL)..=[`MAX_LEVEL`](Self::MAX_LEVEL)
    /// are silently clamped during built-in level parameter resolution.
    pub const fn from_level(level: i32) -> Self {
        match level {
            0 | Self::DEFAULT_LEVEL => Self::Default,
            1 => Self::Fastest,
            7 => Self::Better,
            11 => Self::Best,
            _ => Self::Level(level),
        }
    }
}

/// Trait used by the encoder that users can use to extend the matching facilities with their own algorithm
/// making their own tradeoffs between runtime, memory usage and compression ratio
///
/// This trait operates on buffers that represent the chunks of data the matching algorithm wants to work on.
/// Each one of these buffers is referred to as a *space*. One or more of these buffers represent the window
/// the decoder will need to decode the data again.
///
/// This library asks the Matcher for a new buffer using `get_next_space` to allow reusing of allocated buffers when they are no longer part of the
/// window of data that is being used for matching.
///
/// The library fills the buffer with data that is to be compressed and commits them back to the matcher using `commit_space`.
///
/// Then it will either call `start_matching` or, if the space is deemed not worth compressing, `skip_matching` is called.
///
/// This is repeated until no more data is left to be compressed.
pub trait Matcher {
    /// Get a space where we can put data to be matched on. Will be encoded as one block. The maximum allowed size is 128 kB.
    fn get_next_space(&mut self) -> alloc::vec::Vec<u8>;
    /// Get a reference to the last committed space
    fn get_last_space(&mut self) -> &[u8];
    /// Commit a space to the matcher so it can be matched against
    fn commit_space(&mut self, space: alloc::vec::Vec<u8>);
    /// Just process the data in the last committed space for future matching.
    fn skip_matching(&mut self);
    /// Hint-aware skip path used internally to thread a precomputed block
    /// incompressibility verdict to matcher backends.
    ///
    /// Default implementation preserves backwards compatibility for external
    /// custom matchers by delegating to [`skip_matching`](Self::skip_matching).
    fn skip_matching_with_hint(&mut self, _incompressible_hint: Option<bool>) {
        self.skip_matching();
    }
    /// Process the data in the last committed space for future matching AND generate matches for the data
    fn start_matching(&mut self, handle_sequence: impl for<'a> FnMut(Sequence<'a>));
    /// Reset this matcher so it can be used for the next new frame
    fn reset(&mut self, level: CompressionLevel);
    /// Provide a hint about the total uncompressed size for the next frame.
    ///
    /// Implementations may use this to select smaller hash tables and windows
    /// for small inputs, matching the C zstd source-size-class behavior.
    /// Called before [`reset`](Self::reset) when the caller knows the input
    /// size (e.g. from pledged content size or file metadata).
    ///
    /// The default implementation is a no-op for custom matchers and
    /// test stubs. The built-in runtime matcher (`MatchGeneratorDriver`)
    /// overrides this hook and applies the hint during level resolution.
    fn set_source_size_hint(&mut self, _size: u64) {}
    /// Prime matcher state with dictionary history before compressing the next frame.
    /// Default implementation is a no-op for custom matchers that do not support this.
    fn prime_with_dictionary(&mut self, _dict_content: &[u8], _offset_hist: [u32; 3]) {}
    /// Seed matcher cost model with dictionary entropy tables before the next frame.
    /// Default implementation is a no-op for custom matchers.
    fn seed_dictionary_entropy(
        &mut self,
        _huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        _ll: Option<&crate::fse::fse_encoder::FSETable>,
        _ml: Option<&crate::fse::fse_encoder::FSETable>,
        _of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
    }
    /// Returns whether this matcher can consume dictionary priming state and produce
    /// dictionary-dependent sequences. Defaults to `false` for custom matchers.
    fn supports_dictionary_priming(&self) -> bool {
        false
    }
    /// The size of the window the decoder will need to execute all sequences produced by this matcher.
    ///
    /// Must return a positive (non-zero) value; returning 0 causes
    /// [`StreamingEncoder`] to reject the first write with an invalid-input error
    /// (`InvalidInput` with `std`, `Other` with `no_std`).
    ///
    /// Must remain stable for the lifetime of a frame.
    /// It may change only after `reset()` is called for the next frame
    /// (for example because the compression level changed).
    fn window_size(&self) -> u64;
}

#[derive(PartialEq, Eq, Debug)]
/// Sequences that a [`Matcher`] can produce
pub enum Sequence<'data> {
    /// Is encoded as a sequence for the decoder sequence execution.
    ///
    /// First the literals will be copied to the decoded data,
    /// then `match_len` bytes are copied from `offset` bytes back in the decoded data
    Triple {
        literals: &'data [u8],
        offset: usize,
        match_len: usize,
    },
    /// This is returned as the last sequence in a block
    ///
    /// These literals will just be copied at the end of the sequence execution by the decoder
    Literals { literals: &'data [u8] },
}
