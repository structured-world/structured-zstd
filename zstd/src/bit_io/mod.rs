//! Encoding agnostic ways to read and write binary data

mod bit_reader;
mod bit_reader_reverse;
mod bit_writer;

pub(crate) use bit_reader::*;
// `BitReaderReversed` is the only public type in `bit_reader_reverse`.
// Re-exported as `pub` so `crate::testing` can surface it for benchmarks;
// the containing `bit_io` module is private, limiting external reach.
pub use bit_reader_reverse::BitReaderReversed;
pub(crate) use bit_writer::*;
