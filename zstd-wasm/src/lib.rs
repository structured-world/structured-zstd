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

use structured_zstd::decoding::{
    BlockDecodingStrategy, Dictionary, DictionaryHandle, FrameDecoder, StreamingDecoder,
};
use structured_zstd::encoding::{
    CompressionLevel, EncoderDictionary, FrameCompressor, StreamingEncoder,
};
use structured_zstd::io::{Read, Write};
use wasm_bindgen::prelude::*;

/// How the decoder treats a frame's optional content checksum. Mirrors the
/// core `structured_zstd::decoding::ContentChecksum`.
#[wasm_bindgen]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ContentChecksum {
    /// Skip the XXH64 pass entirely (fastest; no verification).
    None = 0,
    /// Compute the checksum and expose it via accessors; does not error on a mismatch.
    EmitOnly = 1,
    /// Compute and verify; a mismatch throws on decode.
    Verify = 2,
}

/// Resolve an optional JS-supplied mode to the core enum. Defaults to `None`
/// (skip the XXH64 pass) when omitted: this matches the package's prior
/// behaviour (it shipped without the checksum code at all) and libzstd's
/// `checksumFlag = 0` default, and avoids paying the XXH64 cost on a digest no
/// wasm accessor even exposes. Callers opt into `Verify` explicitly.
fn core_checksum(mode: Option<ContentChecksum>) -> structured_zstd::decoding::ContentChecksum {
    use structured_zstd::decoding::ContentChecksum as Core;
    match mode.unwrap_or(ContentChecksum::None) {
        ContentChecksum::None => Core::None,
        ContentChecksum::EmitOnly => Core::EmitOnly,
        ContentChecksum::Verify => Core::Verify,
    }
}

/// Compress `data` into a standard Zstandard frame at compression `level`.
///
/// `level` follows the zstd scale: `1..=22` (higher = smaller/slower) and
/// negative levels (`-7..=-1`) for the ultra-fast tier. The returned frame
/// decodes in any compliant zstd decoder, including the native C library.
///
/// `checksum` is optional (default `false`, matching libzstd's
/// `ZSTD_c_checksumFlag = 0`): pass `true` to append the trailing XXH64 content
/// checksum.
#[wasm_bindgen]
pub fn compress(data: &[u8], level: i32, checksum: Option<bool>) -> Vec<u8> {
    // Set the checksum flag explicitly in both cases rather than leaning on the
    // encoder's own default, so the wasm default stays decoupled from it.
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
    enc.set_content_checksum(checksum.unwrap_or(false));
    enc.compress_independent_frame(data)
}

/// Decompress a complete Zstandard frame back into its original bytes.
///
/// Throws a JavaScript `Error` if the input is not a valid, complete frame,
/// or (when `checksum` is `Verify`) if the content checksum does not match.
/// `checksum` is optional (default `None` — skip the XXH64 pass for speed);
/// pass `ContentChecksum.Verify` to validate, or `EmitOnly` to compute without
/// erroring on mismatch.
#[wasm_bindgen]
pub fn decompress(data: &[u8], checksum: Option<ContentChecksum>) -> Result<Vec<u8>, JsError> {
    // Stream the frame so the output Vec grows to fit — works for frames with
    // or without a content-size header (the fixed-size `decode_all_to_vec`
    // requires the caller to know the decoded length up front).
    let mut decoder = StreamingDecoder::new(data)
        .map_err(|err| JsError::new(&format!("structured-zstd: invalid frame: {err:?}")))?;
    decoder
        .decoder
        .set_content_checksum(core_checksum(checksum));
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
///
/// `checksum` is optional (default `false`, matching libzstd's
/// `ZSTD_c_checksumFlag = 0`); pass `true` to append the XXH64 trailer.
#[wasm_bindgen(js_name = compressUsingDict)]
pub fn compress_using_dict(
    data: &[u8],
    dict: &[u8],
    level: i32,
    checksum: Option<bool>,
) -> Result<Vec<u8>, JsError> {
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
    enc.set_content_checksum(checksum.unwrap_or(false));
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
pub fn decompress_using_dict(
    data: &[u8],
    dict: &[u8],
    checksum: Option<ContentChecksum>,
) -> Result<Vec<u8>, JsError> {
    let mut decoder = StreamingDecoder::new_with_dictionary_bytes(data, dict).map_err(|err| {
        JsError::new(&format!(
            "structured-zstd: dict decode init failed: {err:?}"
        ))
    })?;
    decoder
        .decoder
        .set_content_checksum(core_checksum(checksum));
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|err| {
        JsError::new(&format!("structured-zstd: dict decompress failed: {err:?}"))
    })?;
    Ok(out)
}

/// A dictionary prepared once for repeated use across compressions,
/// decompressions, and streams — the wasm analogue of C's `ZSTD_CDict` +
/// `ZSTD_DDict` pair behind one object.
///
/// Construction parses the dictionary a single time (entropy decode tables +
/// encoder conversion). The hot paths then avoid every per-call setup cost:
///
/// * [`compress`](Self::compress) keeps a cached frame compressor whose
///   dictionary-primed match-finder snapshot is reused across calls — the
///   dominant per-frame cost for small payloads;
/// * [`decompress`](Self::decompress) reuses one decoder workspace;
/// * the stream constructors seed from the prepared tables (a table copy on
///   the encode side, a reference-count bump on the decode side) instead of
///   re-parsing the blob per stream.
///
/// Raw content (bytes without the `0xEC30A437` dictionary magic) is accepted
/// like upstream's auto mode: such dictionaries have ID 0 and produced frames
/// carry no dictionary ID.
#[wasm_bindgen]
pub struct ZstdDictionary {
    /// Decode-side shared handle (`Arc`): per-use cost is a refcount bump.
    handle: DictionaryHandle,
    /// Encode-side template: streams clone its tables instead of re-parsing.
    enc_template: EncoderDictionary,
    /// Raw-content dictionaries carry a synthetic non-zero internal ID that
    /// must never reach the wire.
    suppress_id: bool,
    /// Cached one-shot compressor (dictionary attached + primed snapshot),
    /// rebuilt only when the requested level changes.
    cached_enc: Option<FrameCompressor>,
    cached_level: i32,
    cached_checksum: bool,
    /// Reusable one-shot decoder workspace.
    decoder: FrameDecoder,
    id: u32,
}

#[wasm_bindgen]
impl ZstdDictionary {
    /// Parse `dict` once for repeated use. Magic-prefixed blobs (e.g. from
    /// `zstd --train`) parse as full dictionaries; anything else is raw
    /// content. Throws if a magic-prefixed blob is corrupt.
    #[wasm_bindgen(constructor)]
    pub fn new(dict: &[u8]) -> Result<ZstdDictionary, JsError> {
        if dict.is_empty() {
            return Err(JsError::new("structured-zstd: empty dictionary"));
        }
        let has_magic = dict.len() >= 8
            && u32::from_le_bytes([dict[0], dict[1], dict[2], dict[3]]) == 0xEC30_A437;
        let parsed = if has_magic {
            Dictionary::decode_dict(dict).map_err(|err| {
                JsError::new(&format!("structured-zstd: invalid dictionary: {err:?}"))
            })?
        } else {
            // Synthetic non-zero ID (the encoder attach path requires one);
            // never emitted — see `suppress_id`.
            Dictionary::from_raw_content(u32::MAX, dict.to_vec()).map_err(|err| {
                JsError::new(&format!("structured-zstd: invalid raw dictionary: {err:?}"))
            })?
        };
        let id = if has_magic { parsed.id } else { 0 };
        let enc_template = EncoderDictionary::from_dictionary(parsed.clone());
        let handle = DictionaryHandle::from_dictionary(parsed);
        Ok(ZstdDictionary {
            handle,
            enc_template,
            suppress_id: !has_magic,
            cached_enc: None,
            cached_level: i32::MIN,
            cached_checksum: false,
            decoder: FrameDecoder::new(),
            id,
        })
    }

    /// The dictionary ID (0 for raw content).
    #[wasm_bindgen(getter)]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Compress `data` with this dictionary at `level` — the hot path for
    /// many small frames against one dictionary: the first call at a given
    /// level attaches the prepared tables and primes the match finder, and
    /// every following call reuses that primed snapshot.
    ///
    /// `checksum` is optional (default `false`).
    pub fn compress(
        &mut self,
        data: &[u8],
        level: i32,
        checksum: Option<bool>,
    ) -> Result<Vec<u8>, JsError> {
        let checksum = checksum.unwrap_or(false);
        if self.cached_enc.is_none()
            || self.cached_level != level
            || self.cached_checksum != checksum
        {
            let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
            enc.set_content_checksum(checksum);
            enc.set_encoder_dictionary(self.enc_template.clone())
                .map_err(|err| {
                    JsError::new(&format!("structured-zstd: invalid dictionary: {err:?}"))
                })?;
            if self.suppress_id {
                enc.set_dictionary_id_flag(false);
            }
            self.cached_enc = Some(enc);
            self.cached_level = level;
            self.cached_checksum = checksum;
        }
        let enc = self.cached_enc.as_mut().expect("just ensured Some");
        Ok(enc.compress_independent_frame(data))
    }

    /// Decompress a frame produced with this dictionary, reusing the
    /// prepared tables and one decoder workspace across calls. `checksum`
    /// behaves as in the module-level `decompress`.
    pub fn decompress(
        &mut self,
        data: &[u8],
        checksum: Option<ContentChecksum>,
    ) -> Result<Vec<u8>, JsError> {
        self.decoder.set_content_checksum(core_checksum(checksum));
        let mut cursor: &[u8] = data;
        self.decoder
            .reset_with_dict_handle(&mut cursor, &self.handle)
            .map_err(|err| JsError::new(&format!("structured-zstd: bad frame header: {err:?}")))?;
        self.decoder
            .decode_blocks(&mut cursor, BlockDecodingStrategy::All)
            .map_err(|err| {
                JsError::new(&format!("structured-zstd: dict decompress failed: {err:?}"))
            })?;
        let out = self.decoder.collect().unwrap_or_default();
        self.decoder
            .verify_content_checksum()
            .map_err(|err| JsError::new(&format!("structured-zstd: {err}")))?;
        Ok(out)
    }
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
    /// Prepared dictionary forced onto every frame of this stream
    /// (`withPreparedDictionary`); `None` decodes via the registry /
    /// dictionary-less path.
    dict: Option<DictionaryHandle>,
    pending: Vec<u8>,
    header_done: bool,
    checksum: bool,
    finished: bool,
}

#[wasm_bindgen]
impl ZstdDecompressStream {
    /// `checksum` is optional (default `None` — skip the XXH64 pass, matching
    /// the one-shot `decompress` default) and applies to the whole stream, so
    /// set it here rather than mid-stream: `Verify` validates the content
    /// checksum at [`Self::finish`], `EmitOnly` computes it without erroring.
    #[wasm_bindgen(constructor)]
    pub fn new(checksum: Option<ContentChecksum>) -> ZstdDecompressStream {
        let mut decoder = FrameDecoder::new();
        decoder.set_content_checksum(core_checksum(checksum));
        ZstdDecompressStream {
            decoder,
            dict: None,
            pending: Vec::new(),
            header_done: false,
            checksum: false,
            finished: false,
        }
    }

    /// Open a streaming decompressor primed with a raw zstd dictionary blob
    /// (`ZSTD_DCtx_loadDictionary`): `dict` must be the same dictionary the
    /// frame was compressed with (e.g. via [`ZstdCompressStream`]'s
    /// `withDictionary`). `checksum` behaves as in [`Self::new`]. Throws if the
    /// dictionary is malformed.
    #[wasm_bindgen(js_name = withDictionary)]
    pub fn with_dictionary(
        dict: &[u8],
        checksum: Option<ContentChecksum>,
    ) -> Result<ZstdDecompressStream, JsError> {
        let mut decoder = FrameDecoder::new();
        decoder.set_content_checksum(core_checksum(checksum));
        decoder.add_dict_from_bytes(dict).map_err(|err| {
            JsError::new(&format!("structured-zstd: invalid dictionary: {err:?}"))
        })?;
        Ok(ZstdDecompressStream {
            decoder,
            dict: None,
            pending: Vec::new(),
            header_done: false,
            checksum: false,
            finished: false,
        })
    }

    /// Open a streaming decompressor against a [`ZstdDictionary`] prepared
    /// once: per-stream setup is a reference-count bump, no dictionary
    /// re-parse. Unlike [`Self::with_dictionary`] this also decodes frames
    /// whose headers omit the dictionary ID (raw-content dictionaries).
    #[wasm_bindgen(js_name = withPreparedDictionary)]
    pub fn with_prepared_dictionary(
        dict: &ZstdDictionary,
        checksum: Option<ContentChecksum>,
    ) -> ZstdDecompressStream {
        let mut decoder = FrameDecoder::new();
        decoder.set_content_checksum(core_checksum(checksum));
        ZstdDecompressStream {
            decoder,
            dict: Some(dict.handle.clone()),
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
    /// stream ended before the frame completed, or (in `Verify` mode) if the
    /// content checksum does not match.
    pub fn finish(&mut self) -> Result<Vec<u8>, JsError> {
        let out = self.pump()?;
        if !self.finished {
            return Err(JsError::new(
                "structured-zstd: stream ended before the frame completed",
            ));
        }
        // The frame is fully decoded and drained (pump collects every block),
        // so the running digest is final: validate it in Verify mode (no-op
        // otherwise). The Display of `ChecksumMismatch` names it a corrupt
        // frame, so the thrown JS error reads clearly on the TS side.
        self.decoder
            .verify_content_checksum()
            .map_err(|err| JsError::new(&format!("structured-zstd: {err}")))?;
        Ok(out)
    }

    /// The content checksum stored in the frame's 4-byte trailer, or
    /// `undefined` if the frame carried none. Meaningful after [`Self::finish`].
    #[wasm_bindgen(js_name = storedChecksum)]
    pub fn stored_checksum(&self) -> Option<u32> {
        self.decoder.get_checksum_from_data()
    }

    /// The XXH64 digest the decoder computed over the output (low 32 bits), or
    /// `undefined` when the mode is `None` or the frame carried no checksum.
    /// Meaningful after [`Self::finish`]; lets callers verify manually under
    /// `EmitOnly` without enabling the throwing `Verify` mode.
    #[wasm_bindgen(js_name = calculatedChecksum)]
    pub fn calculated_checksum(&self) -> Option<u32> {
        self.decoder.get_calculated_checksum()
    }
}

impl Default for ZstdDecompressStream {
    fn default() -> Self {
        Self::new(None)
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
            match &self.dict {
                Some(handle) => self.decoder.init_with_dict_handle(&mut cursor, handle),
                None => self.decoder.init(&mut cursor),
            }
            .map_err(|err| JsError::new(&format!("structured-zstd: bad frame header: {err:?}")))?;
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

/// Incremental streaming compressor: feed plaintext chunks via
/// [`ZstdCompressStream::push`] and receive complete compressed blocks as the
/// matcher window fills, then [`ZstdCompressStream::finish`] to seal the frame
/// (final block, plus the XXH64 trailer only when `checksum` was enabled).
/// Peak working set is O(window), not
/// O(input) — emitted blocks are flushed to the caller while only the matcher
/// window is retained — so a large payload never has to be buffered whole. The
/// produced frame omits `Frame_Content_Size` (the total is unknown while
/// streaming) and decodes in any compliant zstd decoder. Mirrors
/// [`ZstdDecompressStream`], making the wasm streaming API symmetric.
#[wasm_bindgen]
pub struct ZstdCompressStream {
    // `None` once `finish` has consumed the encoder; further calls then throw
    // instead of silently producing a second (empty) frame.
    encoder: Option<StreamingEncoder<Vec<u8>>>,
}

#[wasm_bindgen]
impl ZstdCompressStream {
    /// Open a streaming compressor at `level` (zstd scale: `1..=22`, negatives
    /// for the ultra-fast tier). `checksum` is optional (default `false`,
    /// matching libzstd's `ZSTD_c_checksumFlag = 0`): pass `true` to seal the
    /// frame with a trailing content checksum.
    #[wasm_bindgen(constructor)]
    pub fn new(level: i32, checksum: Option<bool>) -> ZstdCompressStream {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(level));
        // Provably Ok: the encoder is fresh, so no frame header has been
        // emitted yet (the only failure mode of this setter).
        encoder
            .set_content_checksum(checksum.unwrap_or(false))
            .expect("fresh streaming encoder accepts the content-checksum toggle");
        ZstdCompressStream {
            encoder: Some(encoder),
        }
    }

    /// Open a streaming compressor at `level` seeded with a raw zstd dictionary
    /// blob (e.g. from `zstd --train`), mirroring `ZSTD_CCtx_loadDictionary` on
    /// a streaming context: the dictionary primes the matcher and seeds the
    /// first block's entropy + repeat offsets, and its ID is written into the
    /// frame header. Decode the produced frames with [`ZstdDecompressStream`]
    /// opened via its matching `withDictionary` constructor (or the one-shot
    /// `decompressUsingDict`). Throws if the dictionary is invalid.
    #[wasm_bindgen(js_name = withDictionary)]
    pub fn with_dictionary(
        level: i32,
        dict: &[u8],
        checksum: Option<bool>,
    ) -> Result<ZstdCompressStream, JsError> {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(level));
        encoder
            .set_content_checksum(checksum.unwrap_or(false))
            .expect("fresh streaming encoder accepts the content-checksum toggle");
        encoder.set_dictionary_from_bytes(dict).map_err(|err| {
            JsError::new(&format!("structured-zstd: invalid dictionary: {err:?}"))
        })?;
        Ok(ZstdCompressStream {
            encoder: Some(encoder),
        })
    }

    /// Open a streaming compressor at `level` seeded from a [`ZstdDictionary`]
    /// prepared once: per-stream setup copies the prepared tables instead of
    /// re-parsing the dictionary blob (no FSE/HUF table rebuild).
    #[wasm_bindgen(js_name = withPreparedDictionary)]
    pub fn with_prepared_dictionary(
        level: i32,
        dict: &ZstdDictionary,
        checksum: Option<bool>,
    ) -> Result<ZstdCompressStream, JsError> {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(level));
        encoder
            .set_content_checksum(checksum.unwrap_or(false))
            .expect("fresh streaming encoder accepts the content-checksum toggle");
        encoder
            .set_encoder_dictionary(dict.enc_template.clone())
            .map_err(|err| {
                JsError::new(&format!("structured-zstd: invalid dictionary: {err:?}"))
            })?;
        if dict.suppress_id {
            encoder
                .set_dictionary_id_flag(false)
                .expect("flag set before the first write on a fresh encoder");
        }
        Ok(ZstdCompressStream {
            encoder: Some(encoder),
        })
    }

    /// Feed more plaintext; returns whatever compressed bytes are now complete
    /// (possibly empty while the current block is still filling).
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, JsError> {
        let enc = self
            .encoder
            .as_mut()
            .ok_or_else(|| JsError::new("structured-zstd: compress stream already finished"))?;
        enc.write_all(chunk)
            .map_err(|err| JsError::new(&format!("structured-zstd: compress failed: {err:?}")))?;
        // Drain the bytes flushed into the backing Vec since the last call,
        // leaving an empty Vec for the encoder to keep appending to.
        Ok(core::mem::take(enc.get_mut()))
    }

    /// Seal the frame; returns the final block plus the content checksum. Throws
    /// if the stream was already finished.
    pub fn finish(&mut self) -> Result<Vec<u8>, JsError> {
        let enc = self
            .encoder
            .take()
            .ok_or_else(|| JsError::new("structured-zstd: compress stream already finished"))?;
        enc.finish()
            .map_err(|err| JsError::new(&format!("structured-zstd: finish failed: {err:?}")))
    }
}
