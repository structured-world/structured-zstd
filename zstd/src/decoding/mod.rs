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
// backend selected via `DecodeBuffer<FlatBuf>`. `FrameDecoder`'s
// `DecoderScratchKind` picks it when the frame header has
// `Single_Segment_flag` set; the ring backend remains the default
// for multi-segment frames. See backlog item #132 for the wiring
// rationale.
pub(crate) mod flat_buf;
pub(crate) mod frame;
pub(crate) mod literals_section_decoder;
pub(crate) mod prefetch;
mod ringbuffer;
#[allow(dead_code)]
pub(crate) mod scratch;
pub(crate) mod sequence_execution;
pub(crate) mod sequence_section_decoder;
pub(crate) mod simd_copy;
// `UserSliceBackend` is the compile-time-monomorphised backend that
// writes directly into the caller's `&mut [u8]` output slice, used
// by the `FrameDecoder::decode_to_slice_trusted` direct-decode path. It
// eliminates the `FlatBuf` drain copy + anonymous-page-fault cost
// on large literal sections. Wiring happens via
// `DecodeBuffer<UserSliceBackend<'a>>`; the lifetime binds the
// backend to the caller's slice for the call duration.
pub(crate) mod user_slice_buf;

#[cfg(feature = "bench_internals")]
pub(crate) use self::simd_copy::copy_bytes_overshooting_for_bench;
