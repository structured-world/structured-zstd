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
}
