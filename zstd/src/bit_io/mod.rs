//! Encoding agnostic ways to read and write binary data

mod bit_reader;
mod bit_reader_reverse;
mod bit_writer;

pub(crate) use bit_reader::*;
// `pub` only when `bench-internals` is active so `crate::testing` can
// re-export it for criterion benchmarks; `pub(crate)` otherwise to
// keep the type out of the public API in normal builds.
#[cfg(feature = "bench-internals")]
pub use bit_reader_reverse::BitReaderReversed;
#[cfg(not(feature = "bench-internals"))]
pub(crate) use bit_reader_reverse::BitReaderReversed;
pub(crate) use bit_writer::*;
