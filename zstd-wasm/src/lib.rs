//! WebAssembly bindings for [structured-zstd] — a pure-Rust Zstandard codec.
//!
//! Exposes a minimal one-shot API to JavaScript / TypeScript:
//! `compress(data, level)` and `decompress(data)`, both over `Uint8Array`.
//! The published npm package (`@structured-world/structured-zstd`) ships two
//! builds of this module — one with the wasm `simd128` SIMD tier enabled and
//! a scalar fallback — plus a loader that picks the right one from the host
//! engine's capabilities. See the package README.
//!
//! The entire surface is gated to `wasm32`: on the host target the crate is
//! empty, so the workspace `cargo check` stays clean and the core codec keeps
//! its no_std, bindgen-free build. wasm-bindgen and `std` live only here.
//!
//! [structured-zstd]: https://crates.io/crates/structured-zstd
#![cfg(target_arch = "wasm32")]

use structured_zstd::decoding::{BlockDecodingStrategy, FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{CompressionLevel, FrameCompressor, compress_slice_to_vec};
use structured_zstd::io::Read;
use wasm_bindgen::prelude::*;

/// Compress `data` into a standard Zstandard frame at compression `level`.
///
/// `level` follows the zstd scale: `1..=22` (higher = smaller/slower) and
/// negative levels (`-7..=-1`) for the ultra-fast tier. The returned frame
/// decodes in any compliant zstd decoder, including the native C library.
#[wasm_bindgen]
pub fn compress(data: &[u8], level: i32) -> Vec<u8> {
    compress_slice_to_vec(data, CompressionLevel::Level(level))
}

/// Decompress a complete Zstandard frame back into its original bytes.
///
/// Throws a JavaScript `Error` if the input is not a valid, complete frame.
#[wasm_bindgen]
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, JsError> {
    // Stream the frame so the output Vec grows to fit — works for frames with
    // or without a content-size header (the fixed-size `decode_all_to_vec`
    // requires the caller to know the decoded length up front).
    let mut decoder = StreamingDecoder::new(data)
        .map_err(|err| JsError::new(&format!("structured-zstd: invalid frame: {err:?}")))?;
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|err| JsError::new(&format!("structured-zstd: decompress failed: {err:?}")))?;
    Ok(out)
}

/// Compress `data` against a raw Zstandard dictionary at compression `level`.
///
/// Mirrors C `ZSTD_compress_usingDict`: the dictionary primes the encoder so
/// small, similar payloads compress far better. The dictionary is the raw
/// zstd dictionary blob (e.g. from `zstd --train`). Throws if it is invalid.
#[wasm_bindgen(js_name = compressUsingDict)]
pub fn compress_using_dict(data: &[u8], dict: &[u8], level: i32) -> Result<Vec<u8>, JsError> {
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
    enc.set_dictionary_from_bytes(dict)
        .map_err(|err| JsError::new(&format!("structured-zstd: invalid dictionary: {err:?}")))?;
    Ok(enc.compress_independent_frame(data))
}

/// Decompress a dictionary-encoded Zstandard frame.
///
/// Mirrors C `ZSTD_decompress_usingDict`: `dict` must be the same raw
/// dictionary the frame was compressed with. Throws on a malformed frame or a
/// dictionary mismatch.
#[wasm_bindgen(js_name = decompressUsingDict)]
pub fn decompress_using_dict(data: &[u8], dict: &[u8]) -> Result<Vec<u8>, JsError> {
    let mut decoder = StreamingDecoder::new_with_dictionary_bytes(data, dict).map_err(|err| {
        JsError::new(&format!(
            "structured-zstd: dict decode init failed: {err:?}"
        ))
    })?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|err| {
        JsError::new(&format!("structured-zstd: dict decompress failed: {err:?}"))
    })?;
    Ok(out)
}

/// Length of a standard zstd frame header in `buf`, plus whether the content
/// checksum flag is set — or `None` if `buf` does not yet hold the full
/// header. Standard RFC 8878 §3.1.1 layout; skippable frames are not handled
/// by the streaming decoder (they carry no compressed blocks).
fn frame_header_len(buf: &[u8]) -> Option<(usize, bool)> {
    // magic (4) + frame header descriptor (1)
    if buf.len() < 5 {
        return None;
    }
    let desc = buf[4];
    let fcs_flag = desc >> 6;
    let single_segment = (desc >> 5) & 1 == 1;
    let checksum = (desc >> 2) & 1 == 1;
    let dict_id_flag = (desc & 3) as usize;
    let window_bytes = usize::from(!single_segment);
    let dict_id_bytes = [0usize, 1, 2, 4][dict_id_flag];
    let fcs_bytes = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let len = 4 + 1 + window_bytes + dict_id_bytes + fcs_bytes;
    (buf.len() >= len).then_some((len, checksum))
}

/// Bytes the leading block in `buf` consumes — its 3-byte header, its body,
/// and (when it is the last block and the frame carries a content checksum)
/// the trailing 4-byte checksum — or `None` if the full block is not yet
/// buffered. RFC 8878 §3.1.1.2 block header: 21-bit size, 2-bit type, 1-bit
/// last-flag. Lets the streaming decoder hand `decode_blocks` only complete
/// blocks (it errors on a source that ends mid-block).
fn complete_block_len(buf: &[u8], checksum: bool) -> Option<usize> {
    if buf.len() < 3 {
        return None;
    }
    let v = buf[0] as u32 | (buf[1] as u32) << 8 | (buf[2] as u32) << 16;
    let last = v & 1 == 1;
    let block_type = (v >> 1) & 3;
    let block_size = (v >> 3) as usize;
    let body = match block_type {
        1 => 1,              // RLE: 1 byte in-stream, regenerated to block_size
        0 | 2 => block_size, // Raw / Compressed: block_size bytes follow
        _ => return None,    // 3 = reserved → let the decoder surface the error
    };
    let need = 3 + body + if last && checksum { 4 } else { 0 };
    (buf.len() >= need).then_some(need)
}

/// Incremental streaming decompressor: feed compressed chunks via
/// [`ZstdDecompressStream::push`] and receive decompressed bytes as they
/// become available, then [`ZstdDecompressStream::finish`]. The decoder window
/// is retained across chunks, so a large frame never needs to be fully
/// buffered — a surface the common npm wasm zstd packages do not offer.
#[wasm_bindgen]
pub struct ZstdDecompressStream {
    decoder: FrameDecoder,
    pending: Vec<u8>,
    header_done: bool,
    checksum: bool,
    finished: bool,
}

#[wasm_bindgen]
impl ZstdDecompressStream {
    #[wasm_bindgen(constructor)]
    pub fn new() -> ZstdDecompressStream {
        ZstdDecompressStream {
            decoder: FrameDecoder::new(),
            pending: Vec::new(),
            header_done: false,
            checksum: false,
            finished: false,
        }
    }

    /// Feed more compressed bytes; returns whatever decompressed output is now
    /// available (possibly empty while a block is still incomplete).
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, JsError> {
        self.pending.extend_from_slice(chunk);
        self.pump()
    }

    /// Signal end of input; returns the final decompressed bytes. Throws if the
    /// stream ended before the frame completed.
    pub fn finish(&mut self) -> Result<Vec<u8>, JsError> {
        let out = self.pump()?;
        if !self.finished {
            return Err(JsError::new(
                "structured-zstd: stream ended before the frame completed",
            ));
        }
        Ok(out)
    }
}

impl Default for ZstdDecompressStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ZstdDecompressStream {
    /// Decode as many complete blocks as `pending` holds, returning their
    /// output. `decode_blocks` is only ever handed a source that contains at
    /// least one full block (the `complete_block_len` gate) because it errors
    /// on a mid-block EOF; `UptoBlocks(1)` then stops at the block boundary so
    /// it consumes exactly one block's bytes (the `&[u8]` reader advances,
    /// giving the consumed count without cumulative bookkeeping).
    fn pump(&mut self) -> Result<Vec<u8>, JsError> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        if !self.header_done {
            let Some((_len, checksum)) = frame_header_len(&self.pending) else {
                return Ok(out);
            };
            let mut cursor: &[u8] = &self.pending;
            self.decoder.init(&mut cursor).map_err(|err| {
                JsError::new(&format!("structured-zstd: bad frame header: {err:?}"))
            })?;
            let advanced = self.pending.len() - cursor.len();
            self.pending.drain(..advanced);
            self.checksum = checksum;
            self.header_done = true;
        }
        while !self.finished && complete_block_len(&self.pending, self.checksum).is_some() {
            let mut cursor: &[u8] = &self.pending;
            let frame_done = self
                .decoder
                .decode_blocks(&mut cursor, BlockDecodingStrategy::UptoBlocks(1))
                .map_err(|err| JsError::new(&format!("structured-zstd: decode failed: {err:?}")))?;
            let advanced = self.pending.len() - cursor.len();
            self.pending.drain(..advanced);
            self.finished = frame_done;
            if let Some(bytes) = self.decoder.collect() {
                out.extend_from_slice(&bytes);
            }
        }
        Ok(out)
    }
}
