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

use structured_zstd::decoding::StreamingDecoder;
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
