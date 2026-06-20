//! Pure-Rust Zstandard codec with a production-grade decoder, dictionary
//! handle reuse, and an actively-improved encoder.
//!
//! The crate ships:
//!
//! * [`decoding`] — [RFC 8878] decoder ([`decoding::StreamingDecoder`],
//!   [`decoding::FrameDecoder`], dictionary-backed paths via
//!   [`decoding::DictionaryHandle`]).
//! * [`encoding`] — frame compressor, streaming encoder, named and numeric
//!   compression levels ([`encoding::CompressionLevel`]).
//! * [`dictionary`] (feature `dict_builder`) — COVER / FastCOVER training
//!   plus raw-to-finalized dictionary helpers.
//!
//! No FFI, no cmake, no system zstd. `no_std` builds are supported by
//! disabling the default `std` feature.
//!
//! # CPU kernel features
//!
//! The decode hot paths ship per-CPU-tier SIMD kernels. With `std` the tier
//! is chosen at runtime (CPU-feature detection, cached on first use); on
//! `no_std` it is chosen at compile time from `cfg(target_feature)`.
//! Each tier is gated by a cargo feature, all enabled by default (a universal
//! binary that picks the best available tier per the above): `kernel_scalar`,
//! `kernel_sse2`, `kernel_bmi2`, `kernel_avx2`, `kernel_vbmi2` (x86) and
//! `kernel_neon`, `kernel_sve` (aarch64). The chain mirrors the ISA
//! dependency (`kernel_avx2` implies `kernel_bmi2` implies `kernel_sse2`;
//! `kernel_sve` implies `kernel_neon`). The scalar kernel is always compiled,
//! so any subset is valid; a flag is inert on architectures it doesn't apply
//! to. Constrained targets can shrink the binary by trimming tiers, e.g.
//! `--no-default-features --features kernel_scalar` compiles out the per-tier
//! SIMD kernel dispatch, its BMI2/AVX2/VBMI2/NEON trampolines, and the
//! explicit SSE2/NEON intrinsics in the small fixed-size copy primitives —
//! all of which are gated on the matching `kernel_*` feature. The `kernel_*`
//! features control the crate's own explicit SIMD; they do not constrain the
//! compiler's autovectorizer, which may still emit vector instructions from
//! ordinary scalar code regardless of the enabled tiers.
//!
//! The packaged README is included below for the docs.rs landing page; the
//! API anchors above link straight into the per-module documentation.
//!
//! [RFC 8878]: https://www.rfc-editor.org/rfc/rfc8878
// Keep crate docs aligned with the packaged README via the crate-local symlink in `zstd/README.md`.
#![doc = include_str!("../README.md")]
#![no_std]
#![deny(trivial_casts, trivial_numeric_casts, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "rustc-dep-of-std"))]
extern crate alloc;

#[cfg(feature = "std")]
pub(crate) const VERBOSE: bool = false;

macro_rules! vprintln {
    ($($x:expr),*) => {
        #[cfg(feature = "std")]
        if crate::VERBOSE {
            std::println!($($x),*);
        }
    }
}

mod bit_io;
mod common;
/// Smallest accepted block-size target (the `ZSTD_TARGETCBLOCKSIZE_MIN`
/// bound): the single source of truth shared by the Rust setters
/// (`set_target_block_size`) and the C ABI parameter surface.
pub use common::MIN_TARGET_BLOCK_SIZE;
mod cpu_kernel;
pub mod decoding;
#[cfg(feature = "dict_builder")]
#[cfg_attr(docsrs, doc(cfg(feature = "dict_builder")))]
pub mod dictionary;
pub mod encoding;
mod histogram;

#[cfg(feature = "lsm")]
#[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
pub mod skippable;

pub(crate) mod blocks;

#[cfg(feature = "fuzz_exports")]
pub mod fse;
#[cfg(feature = "fuzz_exports")]
pub mod huff0;

// `pub fn init_state<K: CpuKernel>` and friends inside the
// fuzz_exports-public `huff0` module name `crate::cpu_kernel::CpuKernel`
// in their signatures. Without a publicly-reachable path to `CpuKernel`
// the bound triggers `private_bounds` / `private_interfaces`. Re-export
// under the same feature gate so the fuzz harness build is clean.
#[cfg(feature = "fuzz_exports")]
pub use crate::cpu_kernel::{CpuKernel, ScalarKernel};

/// Name of the active CPU kernel tier (entropy / sequence hot paths) for this
/// process — for diagnostics and benchmark/dashboard reporting. See
/// [`cpu_kernel::active_cpu_kernel_name`].
pub use crate::cpu_kernel::active_cpu_kernel_name;

#[cfg(not(feature = "fuzz_exports"))]
pub(crate) mod fse;
#[cfg(not(feature = "fuzz_exports"))]
pub(crate) mod huff0;

#[cfg(feature = "std")]
pub mod io_std;

#[cfg(feature = "std")]
pub use io_std as io;

#[cfg(not(feature = "std"))]
pub mod io_nostd;

#[cfg(not(feature = "std"))]
pub use io_nostd as io;

#[cfg(test)]
mod tests;

/// Re-exports of internal types used by benchmarks.
///
/// Gated behind the `bench_internals` feature so normal builds do not
/// widen the public API surface. Not part of the stable API; items may
/// change or disappear without notice.
#[cfg(feature = "bench_internals")]
#[doc(hidden)]
pub mod testing {
    /// Compression parameters selected for `(level, srcSize, dictSize)` →
    /// `(windowLog, chainLog, hashLog, searchLog, minMatch, targetLength,
    /// strategy)`. Facade for the `ffi-bench` parity test that diffs the
    /// selection against the reference `ZSTD_getCParams`.
    pub fn compression_params(
        level: i32,
        src: u64,
        dict: usize,
    ) -> (u32, u32, u32, u32, u32, u32, u32) {
        let cp = crate::encoding::cparams::get_cparams_public(level, src, dict);
        (
            cp.window_log,
            cp.chain_log,
            cp.hash_log,
            cp.search_log,
            cp.min_match,
            cp.target_length,
            cp.strategy,
        )
    }

    /// Force every HUF table build onto the cheap single-build path (skip the
    /// #167 table-log search) so a bench harness can A/B the search across
    /// levels. Measurement-only.
    pub fn set_force_cheap_huf(on: bool) {
        crate::huff0::huff0_encoder::set_force_cheap_huf(on);
    }

    pub use crate::bit_io::BitReaderReversed;
    // `BitReaderReversed` is generic over `K: CpuKernel = ScalarKernel`,
    // so both the trait bound and the default need a `pub` path to
    // match the re-exported type's visibility. Without this the
    // bench-build trips `private_bounds` / `private_interfaces`.
    pub use crate::cpu_kernel::{CpuKernel, ScalarKernel};

    /// Bench-only facade for the decoder wildcopy implementation.
    ///
    /// # Safety
    /// Caller must satisfy the same safety contract as
    /// `decoding::copy_bytes_overshooting_for_bench`.
    #[inline(always)]
    pub unsafe fn copy_bytes_overshooting_for_bench(
        src: (*const u8, usize),
        dst: (*mut u8, usize),
        copy_at_least: usize,
    ) {
        // Keep decoder internals crate-private and expose only this bench shim.
        unsafe { crate::decoding::copy_bytes_overshooting_for_bench(src, dst, copy_at_least) };
    }

    /// Maximum block size per RFC 8878 §3.1.1.2.3 (128 KiB).
    /// Exposed for parity tests that feed exactly-one-block chunks
    /// into the block-splitter comparator.
    pub const MAX_BLOCK_SIZE: u32 = crate::common::MAX_BLOCK_SIZE;

    /// Run our block splitter on a 128 KB chunk.
    ///
    /// `split_level` mirrors upstream zstd `ZSTD_splitBlock(level)`: `0` selects
    /// the borders heuristic (`ZSTD_splitBlock_fromBorders`), `1..=4`
    /// select `ZSTD_splitBlock_byChunks` at the corresponding sampling
    /// level. Returns the split position (or `block.len()` if no split).
    ///
    /// Crate-internal facade for the block-splitter parity comparator test —
    /// the underlying functions stay `fn` so they don't widen the
    /// stable API surface.
    pub fn block_splitter_decision(block: &[u8], split_level: usize) -> usize {
        crate::encoding::frame_compressor::block_splitter_decision_for_bench(block, split_level)
    }

    /// White-box capture of our Huffman weight description for `data`:
    /// `(description, weights)` where `description` is the length-prefixed
    /// FSE payload and `weights` the raw per-symbol weights. Facade for the
    /// `ffi-bench` conformance test that feeds it through the C `HUF_readStats`.
    pub fn huf_weight_description(data: &[u8]) -> (alloc::vec::Vec<u8>, alloc::vec::Vec<u8>) {
        crate::huff0::huff0_encoder::huf_weight_description_for_test(data)
    }

    /// White-box capture of our 4-stream Huffman payload for `data`. Facade for
    /// the `ffi-bench` conformance test that decodes it through the C HUF reader.
    pub fn huf_encode4x(data: &[u8]) -> alloc::vec::Vec<u8> {
        crate::huff0::huff0_encoder::huf_encode4x_for_test(data)
    }

    /// White-box capture of the level-22 sequence stream (literal-length,
    /// offset, match-length triples) our match generator emits for `data`.
    /// Facade for the sequence-conformance test in `ffi-bench`, which
    /// compares this stream against the C reference's `ZSTD_generateSequences`
    /// output. Pure Rust; the C side stays out of this crate.
    pub fn collect_level22_sequences(data: &[u8]) -> alloc::vec::Vec<(usize, usize, usize)> {
        crate::encoding::match_generator::collect_level22_sequences(data)
    }

    /// FastCOVER dictionary roundtrip fixture: `(finalized_dictionary,
    /// compressed_frame, original_payload)`. Facade for the `ffi-bench`
    /// conformance test that decodes `compressed_frame` against the dictionary
    /// through the C decoder and compares to `original_payload`.
    #[cfg(feature = "dict_builder")]
    pub fn dict_roundtrip_fixture() -> (
        alloc::vec::Vec<u8>,
        alloc::vec::Vec<u8>,
        alloc::vec::Vec<u8>,
    ) {
        crate::dictionary::dict_roundtrip_fixture()
    }

    pub use crate::blocks::block::BlockType;

    /// First block's type (raw / rle / compressed) in a frame. Facade over the
    /// internal block decoder for the FFI parity tests in `ffi-bench`.
    pub fn first_block_type(frame: &[u8]) -> BlockType {
        let (_, header_size) = crate::decoding::frame::read_frame_header_with_format(frame, false)
            .expect("frame header should parse");
        let mut decoder = crate::decoding::block_decoder::new();
        let (header, _) = decoder
            .read_block_header(&frame[header_size as usize..])
            .expect("block header should parse");
        header.block_type
    }

    /// `(single_segment_flag, frame_content_size, fcs_field_size_bytes)` parsed
    /// from a frame header. Facade for the FFI parity tests in `ffi-bench` so
    /// they need not reach into the internal `FrameHeader` type.
    pub fn frame_header_info(frame: &[u8]) -> (bool, u64, u8) {
        let (h, _) = crate::decoding::frame::read_frame_header_with_format(frame, false)
            .expect("frame header should parse");
        (
            h.descriptor.single_segment_flag(),
            h.frame_content_size(),
            h.descriptor.frame_content_size_bytes().unwrap_or(0),
        )
    }
}

/// SIMD wildcopy overshoot slack carried by every decoder backend
/// (currently **32 bytes**). Sized so the AVX2 chunked kernel in
/// `simd_copy::copy_bytes_overshooting` (32-byte stride on x86-64) can
/// fire on tail copies near the end of a fixed-capacity output buffer.
/// Upstream zstd's `WILDCOPY_OVERLENGTH` is also 32 bytes today; this
/// matches that contract.
///
/// Public so callers sizing an output slice for
/// [`crate::decoding::FrameDecoder::decode_all`] can size
/// `frame_content_size + WILDCOPY_OVERLENGTH` symbolically without
/// duplicating the value. Use the const reference rather than a
/// hardcoded literal — `simd_copy::copy_bytes_overshooting` already
/// ships an AVX-512 64-byte chunked kernel, and the slack may grow
/// further to reliably enable that wider kernel at buffer tails
/// (mirroring how the bump from 16 → 32 enabled the AVX2 32-byte
/// kernel at the tail).
pub const WILDCOPY_OVERLENGTH: usize = crate::decoding::buffer_backend::WILDCOPY_OVERLENGTH;
