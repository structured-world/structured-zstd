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
    pub use crate::bit_io::BitReaderReversed;

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
    /// into the donor splitter comparator.
    pub const MAX_BLOCK_SIZE: u32 = crate::common::MAX_BLOCK_SIZE;

    /// Run our donor-port block splitter on a 128 KB chunk.
    ///
    /// `split_level` mirrors donor `ZSTD_splitBlock(level)`: `0` selects
    /// the borders heuristic (`ZSTD_splitBlock_fromBorders`), `1..=4`
    /// select `ZSTD_splitBlock_byChunks` at the corresponding sampling
    /// level. Returns the split position (or `block.len()` if no split).
    ///
    /// Crate-internal facade for the donor-parity comparator test —
    /// the underlying functions stay `fn` so they don't widen the
    /// stable API surface.
    pub fn block_splitter_decision(block: &[u8], split_level: usize) -> usize {
        crate::encoding::frame_compressor::block_splitter_decision_for_bench(block, split_level)
    }
}

/// SIMD wildcopy overshoot slack carried by every decoder backend.
/// Mirrors donor zstd's `WILDCOPY_OVERLENGTH` (16 bytes). Public so
/// callers sizing an output slice for
/// [`crate::decoding::FrameDecoder::decode_to_slice`] can size
/// `frame_content_size + WILDCOPY_OVERLENGTH` without duplicating
/// the constant.
pub const WILDCOPY_OVERLENGTH: usize = crate::decoding::buffer_backend::WILDCOPY_OVERLENGTH;
