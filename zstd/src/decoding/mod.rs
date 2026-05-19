//! RFC 8878 Zstandard decoder.
//!
//! Three entry points are exposed, each with progressively lower-level
//! control:
//!
//! * [`StreamingDecoder`] — implements [`crate::io::Read`] over a compressed
//!   byte stream, transparently parsing the frame header and concatenated
//!   frames. The typical choice for application code.
//! * [`FrameDecoder`] — single-frame interface; use when the caller manages
//!   the input buffer manually (zero-copy slices, network framing, etc).
//! * [`DictionaryHandle`] — pre-parsed dictionary handle. Parse the
//!   dictionary bytes once with [`DictionaryHandle::decode_dict`] and reuse
//!   the handle across every subsequent decode; saves the per-frame
//!   dictionary parse cost when the same dictionary is used many times in a
//!   row.
//!
//! Both decoders expose dictionary-aware constructors / methods,
//! though the exact naming differs:
//!
//! * [`StreamingDecoder::new_with_dictionary_handle`] /
//!   [`StreamingDecoder::new_with_dictionary_bytes`]
//! * [`FrameDecoder::decode_all_with_dict_handle`] /
//!   [`FrameDecoder::decode_all_with_dict_bytes`]
//!
//! The `_handle` variants reuse a previously parsed
//! [`DictionaryHandle`]; the `_bytes` variants parse the dictionary
//! per call (suitable for one-off decodes).
//!
//! Errors surface through [`errors::FrameDecoderError`] and the per-decoder
//! error types in the [`errors`] submodule.

pub mod errors;
mod frame_decoder;
mod streaming_decoder;

pub use dictionary::{Dictionary, DictionaryHandle};
pub use frame_decoder::{BlockDecodingStrategy, FrameDecoder};
pub use streaming_decoder::StreamingDecoder;

pub(crate) mod block_decoder;
pub(crate) mod buffer_backend;
pub(crate) mod decode_buffer;
pub(crate) mod dictionary;
// FlatBuf is the compile-time-monomorphised "frame fits in window"
// backend selected via `DecodeBuffer<FlatBuf>`. Phase 1 of backlog
// item #132 (this PR) lands the generic split + trait scaffolding +
// the FlatBuf impl; Phase 2 wires `FrameDecoder` to actually pick the
// flat backend on `Single_Segment_flag` frames. Allow dead-code in
// the interim — the module is intentionally on disk so the type
// surface and unit tests are reviewed alongside the trait, not as a
// separate later drop.
#[allow(dead_code)]
pub(crate) mod flat_buf;
pub(crate) mod frame;
pub(crate) mod literals_section_decoder;
pub(crate) mod prefetch;
mod ringbuffer;
#[allow(dead_code)]
pub(crate) mod scratch;
pub(crate) mod sequence_execution;
pub(crate) mod sequence_section_decoder;
mod simd_copy;

#[cfg(feature = "bench_internals")]
pub(crate) use self::simd_copy::copy_bytes_overshooting_for_bench;
