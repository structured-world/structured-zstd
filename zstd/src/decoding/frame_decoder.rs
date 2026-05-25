//! Framedecoder is the main low-level struct users interact with to decode zstd frames
//!
//! Zstandard compressed data is made of one or more frames. Each frame is independent and can be
//! decompressed independently of other frames. This module contains structures
//! and utilities that can be used to decode a frame.

use super::frame;
use crate::decoding;
use crate::decoding::block_decoder::BlockDecoder;
use crate::decoding::decode_buffer::DecodeBuffer;
use crate::decoding::dictionary::{Dictionary, DictionaryHandle};
use crate::decoding::errors::{DecodeBlockContentError, FrameDecoderError};
use crate::decoding::flat_buf::FlatBuf;
use crate::decoding::ringbuffer::RingBuffer;
use crate::decoding::scratch::DecoderScratch;
use crate::io::{Error, Read, Write};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::convert::TryInto;

use crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE;

/// Low level Zstandard decoder that can be used to decompress frames with fine control over when and how many bytes are decoded.
///
/// This decoder is able to decode frames only partially and gives control
/// over how many bytes/blocks will be decoded at a time (so you don't have to decode a 10GB file into memory all at once).
/// It reads bytes as needed from a provided source and can be read from to collect partial results.
///
/// If you want to just read the whole frame with an `io::Read` without having to deal with manually calling [FrameDecoder::decode_blocks]
/// you can use the provided [crate::decoding::StreamingDecoder] wich wraps this FrameDecoder.
///
/// Workflow is as follows:
/// ```
/// use structured_zstd::decoding::BlockDecodingStrategy;
///
/// # #[cfg(feature = "std")]
/// use std::io::{Read, Write};
///
/// // no_std environments can use the crate's own Read traits
/// # #[cfg(not(feature = "std"))]
/// use structured_zstd::io::{Read, Write};
///
/// fn decode_this(mut file: impl Read) {
///     //Create a new decoder
///     let mut frame_dec = structured_zstd::decoding::FrameDecoder::new();
///     let mut result = Vec::new();
///
///     // Use reset or init to make the decoder ready to decode the frame from the io::Read
///     frame_dec.reset(&mut file).unwrap();
///
///     // Loop until the frame has been decoded completely
///     while !frame_dec.is_finished() {
///         // decode (roughly) batch_size many bytes
///         frame_dec.decode_blocks(&mut file, BlockDecodingStrategy::UptoBytes(1024)).unwrap();
///
///         // read from the decoder to collect bytes from the internal buffer
///         let bytes_read = frame_dec.read(result.as_mut_slice()).unwrap();
///
///         // then do something with it
///         do_something(&result[0..bytes_read]);
///     }
///
///     // handle the last chunk of data
///     while frame_dec.can_collect() > 0 {
///         let x = frame_dec.read(result.as_mut_slice()).unwrap();
///
///         do_something(&result[0..x]);
///     }
/// }
///
/// fn do_something(data: &[u8]) {
/// # #[cfg(feature = "std")]
///     std::io::stdout().write_all(data).unwrap();
/// }
/// ```
pub struct FrameDecoder {
    state: Option<FrameDecoderState>,
    owned_dicts: BTreeMap<u32, Dictionary>,
    #[cfg(target_has_atomic = "ptr")]
    shared_dicts: BTreeMap<u32, DictionaryHandle>,
    #[cfg(not(target_has_atomic = "ptr"))]
    shared_dicts: (),
    /// `ZSTD_f_zstd1_magicless` — when true, [`init`] / [`reset`]
    /// expect frames without the 4-byte magic number prefix.
    /// Default false (standard zstd format).
    magicless: bool,
    /// Pinned `Dictionary_ID` expectation set via
    /// [`Self::expect_dict_id`]. `None` (default) disables the
    /// check; `Some(0)` matches frames whose header omits the
    /// optional dict_id (treated as "no dictionary"). Validated in
    /// [`Self::reset`] AFTER the frame header parses successfully
    /// and BEFORE any block decode work.
    #[cfg(feature = "lsm")]
    expect_dict_id: Option<u32>,
    /// Pinned `Window_Descriptor` byte expectation set via
    /// [`Self::expect_window_descriptor`]. `None` (default)
    /// disables the check. Validated in [`Self::reset`] AFTER the
    /// frame header parses successfully and BEFORE any block
    /// decode work. Single-segment frames (which omit the
    /// `Window_Descriptor` byte from the wire) surface as
    /// [`crate::decoding::errors::FrameDecoderError::UnexpectedWindowDescriptor`]
    /// with `found: None`.
    #[cfg(feature = "lsm")]
    expect_window_descriptor: Option<u8>,
}

/// Backend-tagged decode scratch — chosen at frame-reset time based
/// on the parsed `FrameHeader.descriptor.single_segment_flag()` and
/// kept stable through the lifetime of the frame. The match in each
/// helper below dispatches **once per call** (e.g. once per block in
/// `decode_block_content`, once per drain in `drain_to_writer`) —
/// never inside the hot push/repeat loop, which is fully
/// monomorphised through the `DecoderScratch<B>` generic.
enum DecoderScratchKind {
    Ring(DecoderScratch<RingBuffer>),
    Flat(DecoderScratch<FlatBuf>),
}

impl DecoderScratchKind {
    fn new_ring(window_size: usize) -> Self {
        let mut s = DecoderScratch::<RingBuffer>::new(window_size);
        s.buffer.reserve(window_size);
        Self::Ring(s)
    }

    /// Construct a flat-backed scratch sized for a single-segment
    /// frame. `frame_content_size` is the upcoming output size in
    /// bytes (== `window_size` when the flag is set).
    fn new_flat(frame_content_size: usize) -> Self {
        let flat = FlatBuf::with_capacity(frame_content_size);
        // DecoderScratch's default ctor would discard the pre-sized
        // FlatBuf — go through from_backend so the buffer carries the
        // capacity the constructor wants.
        let mut s = DecoderScratch::<FlatBuf>::new(frame_content_size);
        s.buffer = DecodeBuffer::from_backend(flat, frame_content_size);
        Self::Flat(s)
    }

    /// Reset (or transition between) backends for a new frame.
    /// Reuses the existing `DecoderScratch` allocations (FSE / HUF
    /// tables, sequence vec, etc.) when the backend kind is unchanged
    /// — only the underlying buffer is re-sized for the new frame.
    /// Building a fresh `DecoderScratch` on every frame would
    /// re-allocate everything and was measured at +255 % vs ring on
    /// small frames; reusing it keeps the small-frame cost flat.
    fn reset(&mut self, frame: &frame::FrameHeader, window_size: usize) {
        if frame.descriptor.single_segment_flag() {
            match self {
                Self::Flat(s) => {
                    s.reset(window_size);
                    // DecodeBuffer::reset clears + reserves
                    // window_size; FlatBuf's reserve grows the
                    // backing Vec if the new FCS is larger than
                    // what's already allocated. No alloc when the
                    // previous flat frame had >= this capacity.
                }
                Self::Ring(_) => *self = Self::new_flat(window_size),
            }
        } else {
            match self {
                Self::Ring(s) => s.reset(window_size),
                Self::Flat(_) => *self = Self::new_ring(window_size),
            }
        }
    }

    fn init_from_dict(&mut self, dict: &Dictionary) {
        match self {
            Self::Ring(s) => s.init_from_dict(dict),
            Self::Flat(s) => s.init_from_dict(dict),
        }
    }

    #[inline]
    fn buffer_len(&self) -> usize {
        match self {
            Self::Ring(s) => s.buffer.len(),
            Self::Flat(s) => s.buffer.len(),
        }
    }

    fn buffer_drain(&mut self) -> Vec<u8> {
        match self {
            Self::Ring(s) => s.buffer.drain(),
            Self::Flat(s) => s.buffer.drain(),
        }
    }

    fn buffer_drain_to_window_size(&mut self) -> Option<Vec<u8>> {
        match self {
            Self::Ring(s) => s.buffer.drain_to_window_size(),
            Self::Flat(s) => s.buffer.drain_to_window_size(),
        }
    }

    fn buffer_drain_to_writer(&mut self, sink: impl Write) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.drain_to_writer(sink),
            Self::Flat(s) => s.buffer.drain_to_writer(sink),
        }
    }

    fn buffer_drain_to_window_size_writer(&mut self, sink: impl Write) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.drain_to_window_size_writer(sink),
            Self::Flat(s) => s.buffer.drain_to_window_size_writer(sink),
        }
    }

    fn buffer_can_drain(&self) -> usize {
        match self {
            Self::Ring(s) => s.buffer.can_drain(),
            Self::Flat(s) => s.buffer.can_drain(),
        }
    }

    fn buffer_can_drain_to_window_size(&self) -> Option<usize> {
        match self {
            Self::Ring(s) => s.buffer.can_drain_to_window_size(),
            Self::Flat(s) => s.buffer.can_drain_to_window_size(),
        }
    }

    fn buffer_read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.read(target),
            Self::Flat(s) => s.buffer.read(target),
        }
    }

    fn buffer_read_all(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        match self {
            Self::Ring(s) => s.buffer.read_all(target),
            Self::Flat(s) => s.buffer.read_all(target),
        }
    }

    fn decode_block_content<R: Read>(
        &mut self,
        decoder: &mut BlockDecoder,
        header: &crate::blocks::block::BlockHeader,
        source: R,
    ) -> Result<u64, DecodeBlockContentError> {
        match self {
            Self::Ring(s) => decoder.decode_block_content(header, s, source),
            Self::Flat(s) => decoder.decode_block_content(header, s, source),
        }
    }

    #[cfg(feature = "hash")]
    fn hash_finish(&self) -> u64 {
        use core::hash::Hasher;
        match self {
            Self::Ring(s) => s.buffer.hash.finish(),
            Self::Flat(s) => s.buffer.hash.finish(),
        }
    }
}

struct FrameDecoderState {
    pub frame_header: frame::FrameHeader,
    decoder_scratch: DecoderScratchKind,
    frame_finished: bool,
    block_counter: usize,
    bytes_read_counter: u64,
    check_sum: Option<u32>,
    using_dict: Option<u32>,
}

pub enum BlockDecodingStrategy {
    All,
    UptoBlocks(usize),
    UptoBytes(usize),
}

impl FrameDecoderState {
    /// Construct a new frame decoder state, reading the frame header
    /// from `source`. When `magicless` is `true`, the 4-byte magic
    /// number prefix is NOT consumed (donor `ZSTD_f_zstd1_magicless`).
    /// Crate-internal — reached only via `FrameDecoder::init` /
    /// `FrameDecoder::init_with_dict_handle`. Pre-allocates the
    /// decode buffer to `window_size` so the first block does not
    /// trigger incremental growth from zero capacity.
    pub(crate) fn new_with_format(
        source: impl Read,
        magicless: bool,
    ) -> Result<FrameDecoderState, FrameDecoderError> {
        let (frame, header_size) = frame::read_frame_header_with_format(source, magicless)?;
        let window_size = frame.window_size()?;

        if window_size > MAXIMUM_ALLOWED_WINDOW_SIZE {
            return Err(FrameDecoderError::WindowSizeTooBig {
                requested: window_size,
            });
        }

        let decoder_scratch = if frame.descriptor.single_segment_flag() {
            DecoderScratchKind::new_flat(window_size as usize)
        } else {
            DecoderScratchKind::new_ring(window_size as usize)
        };
        Ok(FrameDecoderState {
            frame_header: frame,
            frame_finished: false,
            block_counter: 0,
            decoder_scratch,
            bytes_read_counter: u64::from(header_size),
            check_sum: None,
            using_dict: None,
        })
    }

    /// Reset this state for a new frame read from `source`, reusing
    /// existing allocations. When `magicless` is `true`, the frame
    /// header is read WITHOUT expecting a magic-number prefix
    /// (donor `ZSTD_f_zstd1_magicless`). Crate-internal — reached
    /// only via `FrameDecoder::reset`.
    ///
    /// `DecodeBuffer::reset` reserves `window_size` internally, so
    /// no additional frame-level reservation is needed here.
    /// Further buffer growth during decoding is performed on demand
    /// by the active block path.
    pub(crate) fn reset_with_format(
        &mut self,
        source: impl Read,
        magicless: bool,
    ) -> Result<(), FrameDecoderError> {
        let (frame_header, header_size) = frame::read_frame_header_with_format(source, magicless)?;
        let window_size = frame_header.window_size()?;

        if window_size > MAXIMUM_ALLOWED_WINDOW_SIZE {
            return Err(FrameDecoderError::WindowSizeTooBig {
                requested: window_size,
            });
        }

        self.decoder_scratch
            .reset(&frame_header, window_size as usize);
        self.frame_header = frame_header;
        self.frame_finished = false;
        self.block_counter = 0;
        self.bytes_read_counter = u64::from(header_size);
        self.check_sum = None;
        self.using_dict = None;
        Ok(())
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    /// This will create a new decoder without allocating anything yet.
    /// init()/reset() will allocate all needed buffers if it is the first time this decoder is used
    /// else they just reset these buffers with not further allocations
    pub fn new() -> FrameDecoder {
        FrameDecoder {
            state: None,
            owned_dicts: BTreeMap::new(),
            #[cfg(target_has_atomic = "ptr")]
            shared_dicts: BTreeMap::new(),
            #[cfg(not(target_has_atomic = "ptr"))]
            shared_dicts: (),
            magicless: false,
            #[cfg(feature = "lsm")]
            expect_dict_id: None,
            #[cfg(feature = "lsm")]
            expect_window_descriptor: None,
        }
    }

    /// Pin the expected `Dictionary_ID` for the next frame.
    ///
    /// When `expected` is set, [`Self::init`] / [`Self::reset`]
    /// validate it against the parsed frame header BEFORE any
    /// block decode work runs. A mismatch returns
    /// [`crate::decoding::errors::FrameDecoderError::UnexpectedDictId`]
    /// before any block decode and before any output is produced.
    /// Scratch buffer allocation / reservation for the decode
    /// pipeline happens during frame-header parsing, which is
    /// already complete when this validation fires — the cost of
    /// scratch sizing is paid even on a mismatched header. The
    /// guarantee is "no block decode, no XXH64 init, no partial
    /// output", not "zero allocation".
    ///
    /// `Some(0)` is treated as "no dictionary expected": a frame
    /// whose header omits the optional `Dictionary_ID` field
    /// (flag value 0) passes the check; a frame that carries an
    /// explicit non-zero id fails.
    ///
    /// `None` (default) disables the check.
    ///
    /// Primary use case: post-AEAD-decrypt sanity check in
    /// wire-format consumers (e.g. lsm-tree's encrypted block
    /// format pins the `dict_id` baked into the AAD against the
    /// inner zstd frame's `dict_id` to defeat dict-substitution
    /// attacks).
    ///
    /// NOT a replacement for AEAD authentication. NOT the same
    /// semantic as donor `ZSTD_d_windowLogMax` (which is a
    /// ceiling-style limit, separate concern).
    #[cfg(feature = "lsm")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
    pub fn expect_dict_id(&mut self, expected: Option<u32>) {
        self.expect_dict_id = expected;
    }

    /// Pin the expected raw `Window_Descriptor` byte (RFC 8878
    /// §3.1.1.1.2 layout: `(exp << 3) | mantissa`) for the next
    /// frame.
    ///
    /// When `expected` is set, [`Self::init`] / [`Self::reset`]
    /// validate it against the parsed frame header BEFORE any
    /// block decode work runs. A mismatch returns
    /// [`crate::decoding::errors::FrameDecoderError::UnexpectedWindowDescriptor`].
    ///
    /// Single-segment frames omit the `Window_Descriptor` byte
    /// from the wire entirely. Setting an expectation while
    /// receiving a single-segment frame fails the check with
    /// `found: None` — there is no on-wire byte to match against,
    /// which is reported explicitly rather than silently passing.
    ///
    /// `None` (default) disables the check.
    ///
    /// Byte-exact equality, NOT a ceiling. Donor
    /// `ZSTD_d_windowLogMax` is a separate ceiling-style limit
    /// available through the C FFI surface; this method is for
    /// strict equality validation against a pinned expectation
    /// (e.g. lsm-tree's wire format pins the window descriptor
    /// from the AAD to defeat decompression-bomb-swap attacks).
    #[cfg(feature = "lsm")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lsm")))]
    pub fn expect_window_descriptor(&mut self, expected: Option<u8>) {
        self.expect_window_descriptor = expected;
    }

    /// Validate the just-parsed frame header against any pinned
    /// expectations set via [`Self::expect_dict_id`] /
    /// [`Self::expect_window_descriptor`].
    ///
    /// Returns the typed error variant on mismatch and leaves
    /// `self.state` in a re-resettable shape — a subsequent
    /// `reset()` will overwrite `frame_header` from the new source
    /// without needing intermediate cleanup.
    #[cfg(feature = "lsm")]
    fn validate_expectations(
        &self,
        frame_header: &frame::FrameHeader,
    ) -> Result<(), FrameDecoderError> {
        if let Some(expected) = self.expect_dict_id {
            let found = frame_header.dictionary_id();
            // `Some(0)` is the "no dictionary expected" sentinel —
            // matches a frame whose header omits the optional
            // dict_id field (which is reported as `None` by the
            // parser). All other values must match exactly.
            let matches = match (expected, found) {
                (0, None) => true,
                (e, Some(f)) => e == f,
                _ => false,
            };
            if !matches {
                return Err(FrameDecoderError::UnexpectedDictId {
                    expected: Some(expected),
                    found,
                });
            }
        }
        if let Some(expected) = self.expect_window_descriptor {
            let found = frame_header.window_descriptor();
            if found != Some(expected) {
                return Err(FrameDecoderError::UnexpectedWindowDescriptor { expected, found });
            }
        }
        Ok(())
    }

    /// Enable or disable magicless frame format
    /// (`ZSTD_f_zstd1_magicless`). When set to `true`, subsequent
    /// [`init`] / [`reset`] calls expect the frame header to begin
    /// directly with the frame-header descriptor — no 4-byte magic
    /// number prefix. Default false. Must match the encoder's
    /// magicless setting; the format is unambiguous only when the
    /// caller knows it out-of-band.
    ///
    /// Note: magicless mode also disables skippable-frame detection.
    /// The `0x184D2A50..=0x184D2A5F` skippable-frame magic range is
    /// only recognised when the 4-byte magic prefix is consumed, so
    /// `decode_all` / `init` / `reset` will treat a skippable frame
    /// at the head of a magicless stream as a malformed frame header
    /// (bad descriptor / window-size error) instead of skipping it.
    /// Mixed-format streams that interleave skippable frames must be
    /// pre-split by the caller; `set_magicless(true)` is only safe
    /// when the entire stream is known to be magicless zstd frames.
    pub fn set_magicless(&mut self, magicless: bool) {
        self.magicless = magicless;
    }

    #[cfg(target_has_atomic = "ptr")]
    fn shared_dict_exists(&self, dict_id: u32) -> bool {
        self.shared_dicts.contains_key(&dict_id)
    }

    #[cfg(not(target_has_atomic = "ptr"))]
    fn shared_dict_exists(&self, _dict_id: u32) -> bool {
        false
    }

    fn validate_registered_dictionary(dict: &Dictionary) -> Result<(), FrameDecoderError> {
        use crate::decoding::errors::DictionaryDecodeError as dict_err;

        if dict.id == 0 {
            return Err(FrameDecoderError::from(dict_err::ZeroDictionaryId));
        }
        if let Some(index) = dict.offset_hist.iter().position(|&rep| rep == 0) {
            return Err(FrameDecoderError::from(
                dict_err::ZeroRepeatOffsetInDictionary { index: index as u8 },
            ));
        }
        Ok(())
    }

    /// init() will allocate all needed buffers if it is the first time this decoder is used
    /// else they just reset these buffers with not further allocations
    ///
    /// Note that all bytes currently in the decodebuffer from any previous frame will be lost. Collect them with collect()/collect_to_writer()
    ///
    /// equivalent to reset()
    pub fn init(&mut self, source: impl Read) -> Result<(), FrameDecoderError> {
        self.reset(source)
    }

    /// Initialize the decoder for a new frame using a pre-parsed dictionary handle.
    ///
    /// If the frame header has a dictionary ID, this validates it against
    /// `dict.id()` and returns [`FrameDecoderError::DictIdMismatch`] on mismatch.
    ///
    /// If the header omits the optional dictionary ID, this still applies the
    /// provided dictionary handle.
    ///
    /// # Warning
    ///
    /// This method always applies `dict` unless the frame header contains a
    /// non-matching dictionary ID. Callers must only use this API when they
    /// already know the frame was encoded with the provided dictionary, even if
    /// the frame header omits the dictionary ID or encodes an explicit
    /// dictionary ID of `0`.
    ///
    /// Passing a dictionary for a frame that was not encoded with it can
    /// silently corrupt the decoded output.
    pub fn init_with_dict_handle(
        &mut self,
        source: impl Read,
        dict: &DictionaryHandle,
    ) -> Result<(), FrameDecoderError> {
        self.reset_with_dict_handle(source, dict)
    }

    /// reset() will allocate all needed buffers if it is the first time this decoder is used
    /// else they just reset these buffers with not further allocations
    ///
    /// Note that all bytes currently in the decodebuffer from any previous frame will be lost. Collect them with collect()/collect_to_writer()
    ///
    /// equivalent to init()
    pub fn reset(&mut self, source: impl Read) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        let magicless = self.magicless;
        let dict_id = match &mut self.state {
            Some(s) => {
                s.reset_with_format(source, magicless)?;
                s.frame_header.dictionary_id()
            }
            None => {
                self.state = Some(FrameDecoderState::new_with_format(source, magicless)?);
                self.state
                    .as_ref()
                    .and_then(|state| state.frame_header.dictionary_id())
            }
        };
        // Validate any pinned expectations BEFORE block decode work
        // runs. Catches dict_id substitution / window-descriptor
        // tampering on inputs already authenticated by an outer
        // layer (e.g. AEAD). Returning here leaves `self.state` in
        // a re-resettable shape — next `reset()` re-parses the
        // frame header without intermediate cleanup.
        #[cfg(feature = "lsm")]
        if let Some(state) = self.state.as_ref() {
            self.validate_expectations(&state.frame_header)?;
        }
        if let Some(dict_id) = dict_id {
            let state = self.state.as_mut().expect("state initialized");
            let owned_dicts = &self.owned_dicts;
            #[cfg(target_has_atomic = "ptr")]
            let shared_dicts = &self.shared_dicts;
            let dict = owned_dicts
                .get(&dict_id)
                .or_else(|| {
                    #[cfg(target_has_atomic = "ptr")]
                    {
                        shared_dicts.get(&dict_id).map(DictionaryHandle::as_dict)
                    }
                    #[cfg(not(target_has_atomic = "ptr"))]
                    {
                        None
                    }
                })
                .ok_or(err::DictNotProvided { dict_id })?;
            state.decoder_scratch.init_from_dict(dict);
            state.using_dict = Some(dict_id);
        }
        Ok(())
    }

    /// Reset this decoder for a new frame using a pre-parsed dictionary handle.
    ///
    /// If the frame header has a dictionary ID, this validates it against
    /// `dict.id()` and returns [`FrameDecoderError::DictIdMismatch`] on mismatch.
    ///
    /// If the header omits the optional dictionary ID, this still applies the
    /// provided dictionary handle.
    ///
    /// # Warning
    ///
    /// This method always applies `dict` unless the frame header contains a
    /// non-matching dictionary ID. Callers must only use this API when they
    /// already know the frame was encoded with the provided dictionary, even if
    /// the frame header omits the dictionary ID or encodes an explicit
    /// dictionary ID of `0`.
    ///
    /// Passing a dictionary for a frame that was not encoded with it can
    /// silently corrupt the decoded output.
    pub fn reset_with_dict_handle(
        &mut self,
        source: impl Read,
        dict: &DictionaryHandle,
    ) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        Self::validate_registered_dictionary(dict.as_dict())?;
        let magicless = self.magicless;
        // Scope the &mut borrow of `self.state` to the header parse
        // alone, so the subsequent `validate_expectations(&self, ...)`
        // call below can take a fresh shared borrow of self without
        // tripping the borrow checker.
        match &mut self.state {
            Some(s) => s.reset_with_format(source, magicless)?,
            None => {
                self.state = Some(FrameDecoderState::new_with_format(source, magicless)?);
            }
        }
        // Single source of truth: route through the same
        // `validate_expectations` used by `reset()`. Routing through
        // the helper keeps the two code paths from drifting (e.g.,
        // if expect-semantics or error wiring changes later).
        #[cfg(feature = "lsm")]
        {
            let header = &self
                .state
                .as_ref()
                .expect("state populated by reset_with_format/new_with_format")
                .frame_header;
            self.validate_expectations(header)?;
        }
        let state = self
            .state
            .as_mut()
            .expect("state populated by reset_with_format/new_with_format");
        if let Some(dict_id) = state.frame_header.dictionary_id()
            && dict_id != dict.id()
        {
            return Err(err::DictIdMismatch {
                expected: dict_id,
                provided: dict.id(),
            });
        }
        state.decoder_scratch.init_from_dict(dict.as_dict());
        state.using_dict = Some(dict.id());
        Ok(())
    }

    /// Add a dictionary that can be selected dynamically by frame dictionary ID.
    ///
    /// Returns [`FrameDecoderError::DictAlreadyRegistered`] if the ID is already
    /// registered (either as owned or shared).
    pub fn add_dict(&mut self, dict: Dictionary) -> Result<(), FrameDecoderError> {
        Self::validate_registered_dictionary(&dict)?;
        let dict_id = dict.id;
        if self.owned_dicts.contains_key(&dict_id) || self.shared_dict_exists(dict_id) {
            return Err(FrameDecoderError::DictAlreadyRegistered { dict_id });
        }
        self.owned_dicts.insert(dict_id, dict);
        Ok(())
    }

    /// Parse and add a serialized dictionary blob.
    pub fn add_dict_from_bytes(&mut self, raw_dictionary: &[u8]) -> Result<(), FrameDecoderError> {
        let dict = Dictionary::decode_dict(raw_dictionary)?;
        self.add_dict(dict)
    }

    /// Add a pre-parsed dictionary handle for reuse across decoders.
    ///
    /// This API is available on targets with pointer-width atomics
    /// (`target_has_atomic = "ptr"`).
    ///
    /// Returns [`FrameDecoderError::DictAlreadyRegistered`] if the ID is already
    /// registered (either as owned or shared).
    #[cfg(target_has_atomic = "ptr")]
    pub fn add_dict_handle(&mut self, dict: DictionaryHandle) -> Result<(), FrameDecoderError> {
        Self::validate_registered_dictionary(dict.as_dict())?;
        let dict_id = dict.id();
        if self.owned_dicts.contains_key(&dict_id) || self.shared_dicts.contains_key(&dict_id) {
            return Err(FrameDecoderError::DictAlreadyRegistered { dict_id });
        }
        self.shared_dicts.insert(dict_id, dict);
        Ok(())
    }

    pub fn force_dict(&mut self, dict_id: u32) -> Result<(), FrameDecoderError> {
        use FrameDecoderError as err;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;
        let owned_dicts = &self.owned_dicts;
        #[cfg(target_has_atomic = "ptr")]
        let shared_dicts = &self.shared_dicts;

        let dict = owned_dicts
            .get(&dict_id)
            .or_else(|| {
                #[cfg(target_has_atomic = "ptr")]
                {
                    shared_dicts.get(&dict_id).map(DictionaryHandle::as_dict)
                }
                #[cfg(not(target_has_atomic = "ptr"))]
                {
                    None
                }
            })
            .ok_or(err::DictNotProvided { dict_id })?;
        state.decoder_scratch.init_from_dict(dict);
        state.using_dict = Some(dict_id);

        Ok(())
    }

    /// Returns how many bytes the frame contains after decompression
    pub fn content_size(&self) -> u64 {
        match &self.state {
            None => 0,
            Some(s) => s.frame_header.frame_content_size(),
        }
    }

    /// Returns the checksum that was read from the data. Only available after all bytes have been read. It is the last 4 bytes of a zstd-frame
    pub fn get_checksum_from_data(&self) -> Option<u32> {
        let state = self.state.as_ref()?;

        state.check_sum
    }

    /// Returns the checksum that was calculated while decoding.
    /// Only a sensible value after all decoded bytes have been collected/read from the FrameDecoder
    #[cfg(feature = "hash")]
    pub fn get_calculated_checksum(&self) -> Option<u32> {
        let state = self.state.as_ref()?;
        let cksum_64bit = state.decoder_scratch.hash_finish();
        //truncate to lower 32bit because reasons...
        Some(cksum_64bit as u32)
    }

    /// Counter for how many bytes have been consumed while decoding the frame
    pub fn bytes_read_from_source(&self) -> u64 {
        let state = match &self.state {
            None => return 0,
            Some(s) => s,
        };
        state.bytes_read_counter
    }

    /// Whether the current frames last block has been decoded yet
    /// If this returns true you can call the drain* functions to get all content
    /// (the read() function will drain automatically if this returns true)
    pub fn is_finished(&self) -> bool {
        let state = match &self.state {
            None => return true,
            Some(s) => s,
        };
        if state.frame_header.descriptor.content_checksum_flag() {
            state.frame_finished && state.check_sum.is_some()
        } else {
            state.frame_finished
        }
    }

    /// Counter for how many blocks have already been decoded
    pub fn blocks_decoded(&self) -> usize {
        let state = match &self.state {
            None => return 0,
            Some(s) => s,
        };
        state.block_counter
    }

    /// Decodes blocks from a reader. It requires that the framedecoder has been initialized first.
    /// The Strategy influences how many blocks will be decoded before the function returns
    /// This is important if you want to manage memory consumption carefully. If you don't care
    /// about that you can just choose the strategy "All" and have all blocks of the frame decoded into the buffer
    pub fn decode_blocks(
        &mut self,
        mut source: impl Read,
        strat: BlockDecodingStrategy,
    ) -> Result<bool, FrameDecoderError> {
        use FrameDecoderError as err;
        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;

        let mut block_dec = decoding::block_decoder::new();

        let buffer_size_before = state.decoder_scratch.buffer_len();
        let block_counter_before = state.block_counter;
        loop {
            vprintln!("################");
            vprintln!("Next Block: {}", state.block_counter);
            vprintln!("################");
            let (block_header, block_header_size) = block_dec
                .read_block_header(&mut source)
                .map_err(err::FailedToReadBlockHeader)?;
            state.bytes_read_counter += u64::from(block_header_size);

            vprintln!();
            vprintln!(
                "Found {} block with size: {}, which will be of size: {}",
                block_header.block_type,
                block_header.content_size,
                block_header.decompressed_size
            );

            let bytes_read_in_block_body = state
                .decoder_scratch
                .decode_block_content(&mut block_dec, &block_header, &mut source)
                .map_err(err::FailedToReadBlockBody)?;
            state.bytes_read_counter += bytes_read_in_block_body;

            state.block_counter += 1;

            vprintln!("Output: {}", state.decoder_scratch.buffer_len());

            if block_header.last_block {
                state.frame_finished = true;
                if state.frame_header.descriptor.content_checksum_flag() {
                    let mut chksum = [0u8; 4];
                    source
                        .read_exact(&mut chksum)
                        .map_err(err::FailedToReadChecksum)?;
                    state.bytes_read_counter += 4;
                    let chksum = u32::from_le_bytes(chksum);
                    state.check_sum = Some(chksum);
                }
                break;
            }

            match strat {
                BlockDecodingStrategy::All => { /* keep going */ }
                BlockDecodingStrategy::UptoBlocks(n) => {
                    if state.block_counter - block_counter_before >= n {
                        break;
                    }
                }
                BlockDecodingStrategy::UptoBytes(n) => {
                    if state.decoder_scratch.buffer_len() - buffer_size_before >= n {
                        break;
                    }
                }
            }
        }

        Ok(state.frame_finished)
    }

    /// Collect bytes and retain window_size bytes while decoding is still going on.
    /// After decoding of the frame (is_finished() == true) has finished it will collect all remaining bytes
    pub fn collect(&mut self) -> Option<Vec<u8>> {
        let finished = self.is_finished();
        let state = self.state.as_mut()?;
        if finished {
            Some(state.decoder_scratch.buffer_drain())
        } else {
            state.decoder_scratch.buffer_drain_to_window_size()
        }
    }

    /// Collect bytes and retain window_size bytes while decoding is still going on.
    /// After decoding of the frame (is_finished() == true) has finished it will collect all remaining bytes
    pub fn collect_to_writer(&mut self, w: impl Write) -> Result<usize, Error> {
        let finished = self.is_finished();
        let state = match &mut self.state {
            None => return Ok(0),
            Some(s) => s,
        };
        if finished {
            state.decoder_scratch.buffer_drain_to_writer(w)
        } else {
            state.decoder_scratch.buffer_drain_to_window_size_writer(w)
        }
    }

    /// How many bytes can currently be collected from the decodebuffer, while decoding is going on this will be lower than the actual decodbuffer size
    /// because window_size bytes need to be retained for decoding.
    /// After decoding of the frame (is_finished() == true) has finished it will report all remaining bytes
    pub fn can_collect(&self) -> usize {
        let finished = self.is_finished();
        let state = match &self.state {
            None => return 0,
            Some(s) => s,
        };
        if finished {
            state.decoder_scratch.buffer_can_drain()
        } else {
            state
                .decoder_scratch
                .buffer_can_drain_to_window_size()
                .unwrap_or(0)
        }
    }

    /// Decodes as many blocks as possible from the source slice and reads from the decodebuffer into the target slice
    /// The source slice may contain only parts of a frame but must contain at least one full block to make progress
    ///
    /// By all means use decode_blocks if you have a io.Reader available. This is just for compatibility with other decompressors
    /// which try to serve an old-style c api
    ///
    /// Returns (read, written), if read == 0 then the source did not contain a full block and further calls with the same
    /// input will not make any progress!
    ///
    /// Note that no kind of block can be bigger than 128kb.
    /// So to be safe use at least 128*1024 (max block content size) + 3 (block_header size) + 18 (max frame_header size) bytes as your source buffer
    ///
    /// You may call this function with an empty source after all bytes have been decoded. This is equivalent to just call decoder.read(&mut target)
    pub fn decode_from_to(
        &mut self,
        source: &[u8],
        target: &mut [u8],
    ) -> Result<(usize, usize), FrameDecoderError> {
        use FrameDecoderError as err;
        let bytes_read_at_start = match &self.state {
            Some(s) => s.bytes_read_counter,
            None => 0,
        };

        if !self.is_finished() || self.state.is_none() {
            let mut mt_source = source;

            if self.state.is_none() {
                self.init(&mut mt_source)?;
            }

            //pseudo block to scope "state" so we can borrow self again after the block
            {
                let state = match &mut self.state {
                    Some(s) => s,
                    None => panic!("Bug in library"),
                };
                let mut block_dec = decoding::block_decoder::new();

                if state.frame_header.descriptor.content_checksum_flag()
                    && state.frame_finished
                    && state.check_sum.is_none()
                {
                    //this block is needed if the checksum were the only 4 bytes that were not included in the last decode_from_to call for a frame
                    if mt_source.len() >= 4 {
                        let chksum = mt_source[..4].try_into().expect("optimized away");
                        state.bytes_read_counter += 4;
                        let chksum = u32::from_le_bytes(chksum);
                        state.check_sum = Some(chksum);
                    }
                    return Ok((4, 0));
                }

                loop {
                    //check if there are enough bytes for the next header
                    if mt_source.len() < 3 {
                        break;
                    }
                    let (block_header, block_header_size) = block_dec
                        .read_block_header(&mut mt_source)
                        .map_err(err::FailedToReadBlockHeader)?;

                    // check the needed size for the block before updating counters.
                    // If not enough bytes are in the source, the header will have to be read again, so act like we never read it in the first place
                    if mt_source.len() < block_header.content_size as usize {
                        break;
                    }
                    state.bytes_read_counter += u64::from(block_header_size);

                    let bytes_read_in_block_body = state
                        .decoder_scratch
                        .decode_block_content(&mut block_dec, &block_header, &mut mt_source)
                        .map_err(err::FailedToReadBlockBody)?;
                    state.bytes_read_counter += bytes_read_in_block_body;
                    state.block_counter += 1;

                    if block_header.last_block {
                        state.frame_finished = true;
                        if state.frame_header.descriptor.content_checksum_flag() {
                            //if there are enough bytes handle this here. Else the block at the start of this function will handle it at the next call
                            if mt_source.len() >= 4 {
                                let chksum = mt_source[..4].try_into().expect("optimized away");
                                state.bytes_read_counter += 4;
                                let chksum = u32::from_le_bytes(chksum);
                                state.check_sum = Some(chksum);
                            }
                        }
                        break;
                    }
                }
            }
        }

        let result_len = self.read(target).map_err(err::FailedToDrainDecodebuffer)?;
        let bytes_read_at_end = match &mut self.state {
            Some(s) => s.bytes_read_counter,
            None => panic!("Bug in library"),
        };
        let read_len = bytes_read_at_end - bytes_read_at_start;
        Ok((read_len as usize, result_len))
    }

    /// Decode multiple frames into the output slice.
    ///
    /// `input` must contain an exact number of frames. Skippable frames are allowed and will be
    /// skipped during decode.
    ///
    /// `output` must be large enough to hold the decompressed data. If you don't know
    /// how large the output will be, use [`FrameDecoder::decode_blocks`] instead.
    ///
    /// This calls [`FrameDecoder::init`], and all bytes currently in the decoder will be lost.
    ///
    /// Returns the number of bytes written to `output`.
    pub fn decode_all(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, FrameDecoderError> {
        self.decode_all_impl(input, output, |this, src| this.init(src))
    }

    /// Decode multiple frames into the output slice using a pre-parsed dictionary handle.
    ///
    /// `input` must contain an exact number of frames. Skippable frames are allowed and will be
    /// skipped during decode.
    ///
    /// `output` must be large enough to hold the decompressed data. If you don't know
    /// how large the output will be, use [`FrameDecoder::decode_blocks`] instead.
    ///
    /// This calls [`FrameDecoder::init_with_dict_handle`], and all bytes currently in the
    /// decoder will be lost.
    ///
    /// # Warning
    ///
    /// Each decoded frame is initialized with `dict`, even when a frame header
    /// omits the optional dictionary ID. Callers must only use this API when
    /// they already know the input frames were encoded with the provided
    /// dictionary; otherwise decoded output can be silently corrupted.
    pub fn decode_all_with_dict_handle(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        dict: &DictionaryHandle,
    ) -> Result<usize, FrameDecoderError> {
        self.decode_all_impl(input, output, |this, src| {
            this.init_with_dict_handle(src, dict)
        })
    }

    fn decode_all_impl(
        &mut self,
        mut input: &[u8],
        mut output: &mut [u8],
        mut init_frame: impl FnMut(&mut Self, &mut &[u8]) -> Result<(), FrameDecoderError>,
    ) -> Result<usize, FrameDecoderError> {
        let mut total_bytes_written = 0;
        while !input.is_empty() {
            match init_frame(self, &mut input) {
                Ok(_) => {}
                Err(FrameDecoderError::ReadFrameHeaderError(
                    crate::decoding::errors::ReadFrameHeaderError::SkipFrame { length, .. },
                )) => {
                    input = input
                        .get(length as usize..)
                        .ok_or(FrameDecoderError::FailedToSkipFrame)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            loop {
                self.decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1024 * 1024))?;
                let bytes_written = self
                    .read(output)
                    .map_err(FrameDecoderError::FailedToDrainDecodebuffer)?;
                output = &mut output[bytes_written..];
                total_bytes_written += bytes_written;
                if self.can_collect() != 0 {
                    return Err(FrameDecoderError::TargetTooSmall);
                }
                if self.is_finished() {
                    break;
                }
            }
        }

        Ok(total_bytes_written)
    }

    /// Decode multiple frames into the output slice using a serialized dictionary.
    ///
    /// # Warning
    ///
    /// Each decoded frame is initialized with the parsed dictionary, even when a
    /// frame header omits the optional dictionary ID. Callers must only use this
    /// API when they already know the input frames were encoded with that
    /// dictionary; otherwise decoded output can be silently corrupted.
    pub fn decode_all_with_dict_bytes(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        raw_dictionary: &[u8],
    ) -> Result<usize, FrameDecoderError> {
        let dict = DictionaryHandle::decode_dict(raw_dictionary)?;
        self.decode_all_with_dict_handle(input, output, &dict)
    }

    /// Decode multiple frames into the extra capacity of the output vector.
    ///
    /// `input` must contain an exact number of frames.
    ///
    /// `output` must have enough extra capacity to hold the decompressed data.
    /// This function will not reallocate or grow the vector. If you don't know
    /// how large the output will be, use [`FrameDecoder::decode_blocks`] instead.
    ///
    /// This calls [`FrameDecoder::init`], and all bytes currently in the decoder will be lost.
    ///
    /// The length of the output vector is updated to include the decompressed data.
    /// The length is not changed if an error occurs.
    pub fn decode_all_to_vec(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<(), FrameDecoderError> {
        let len = output.len();
        let cap = output.capacity();
        output.resize(cap, 0);
        match self.decode_all(input, &mut output[len..]) {
            Ok(bytes_written) => {
                let new_len = core::cmp::min(len + bytes_written, cap); // Sanitizes `bytes_written`.
                output.resize(new_len, 0);
                Ok(())
            }
            Err(e) => {
                output.resize(len, 0);
                Err(e)
            }
        }
    }

    /// Decode a single zstd frame from `input` directly into
    /// `output`, bypassing the internal `DecodeBuffer` -> `read()`
    /// drain copy when the frame is eligible. Donor parity with the
    /// `ZSTD_in_dst` litBuffer placement strategy.
    ///
    /// Eligibility requires all of:
    /// - `frame_content_size` is present in the header (> 0).
    /// - `output.len() >= frame_content_size + WILDCOPY_OVERLENGTH`
    ///   (room for the SIMD wildcopy overshoot slack).
    /// - No active dictionary on `self.state` (dict_content is not
    ///   carried into the stack-local DecodeBuffer this method
    ///   builds).
    ///
    /// `content_checksum_flag` is NOT a disqualifier: when set,
    /// the direct path hashes the decoded `output[..content_size]`
    /// once at the end of decode and propagates the digest into
    /// the persistent scratch's `hash` so
    /// [`Self::get_calculated_checksum`] returns the right value.
    ///
    /// Multi-segment frames are supported via a per-block
    /// `DecodeBuffer::drop_to_window_size` call that caps the
    /// visible buffer at `window_size` at block boundaries. The
    /// discarded bytes stay physically in the user slice (they're
    /// the frame's already-decoded output); only their
    /// `BufferBackend::head` visibility moves forward.
    ///
    /// Note: `drop_to_window_size` runs only BETWEEN blocks, so
    /// within a single block `buffer.len()` can temporarily exceed
    /// `window_size`. `DecodeBuffer::repeat` validates match
    /// offsets against `buffer.len()` (not against `window_size`),
    /// so corrupted streams with `offset > window_size` but
    /// `offset <= current buffer.len()` are NOT rejected by this
    /// gate. Strict spec compliance for offsets in multi-segment
    /// frames would require an in-block offset bound that we don't
    /// currently enforce on either the direct or the fallback path.
    ///
    /// Non-eligible frames fall back transparently to the existing
    /// `decode_blocks` + `read` drain path.
    ///
    /// `input` is expected to contain a single zstd frame. Bytes
    /// past the end of that frame are NOT validated and are silently
    /// ignored — this differs from [`Self::decode_all`], which loops
    /// until `input` is fully consumed and will attempt to parse a
    /// second frame (or error) on trailing bytes. Multi-frame
    /// streams must use [`Self::decode_all`].
    ///
    /// On the direct path the literal pushes and
    /// sequence-execution match copies write straight into
    /// `output`, eliminating the FlatBuf-as-intermediate `read()`
    /// drain that dominates poorly-compressed L-7-class corpora
    /// (~28% of decode time on
    /// `level_-7_fast/decodecorpus-z000033/rust_stream`). Both
    /// single-segment and multi-segment frames take the direct
    /// path; multi-segment frames cap the visible buffer at
    /// `window_size` between blocks via
    /// `DecodeBuffer::drop_to_window_size`.
    ///
    /// Frames that aren't eligible (zero `frame_content_size`,
    /// active dictionary, undersized `output`) transparently fall
    /// back to the internal block-decode + read drain loop. The
    /// fallback is NOT [`Self::decode_all`] semantics: it decodes
    /// exactly one frame and returns; trailing bytes past the
    /// frame are silently ignored. Use [`Self::decode_all`] for
    /// multi-frame input or streams that may contain skippable
    /// frames.
    ///
    /// `input` is expected to contain exactly ONE non-skippable
    /// zstd frame. **Skippable frames are rejected with
    /// `ReadFrameHeaderError::SkipFrame` from `init`** — this
    /// method does NOT skip them. Multi-frame input or input that
    /// might contain skippable frames must go through
    /// [`Self::decode_all`], which iterates `init` and handles
    /// `SkipFrame` by advancing past the skippable payload.
    ///
    /// # State observability after this call
    ///
    /// On the direct path, decoded bytes are written into `output`
    /// via a stack-local `DecodeBuffer<UserSliceBackend>` that is
    /// dropped before this function returns. The persistent
    /// `state.decoder_scratch.buffer` stays empty. Consequently,
    /// after `decode_to_slice_trusted` returns:
    ///
    /// - [`Self::is_finished`] returns `true`,
    /// - [`Self::can_collect`] returns `0`,
    /// - [`Self::read`] (the crate's `io::Read` impl, which under
    ///   `feature = "std"` is `std::io::Read`) reads 0 bytes,
    /// - [`Self::collect`] returns `Some(Vec::new())`,
    /// - [`Self::get_calculated_checksum`] returns the correct
    ///   value when the frame had `content_checksum_flag` set —
    ///   the direct path walks the output once at end of decode
    ///   and propagates the digest into the persistent scratch's
    ///   hasher so this accessor reads the right state.
    ///
    /// Callers must use the bytes from `output[..n]` (where `n`
    /// is the returned count); do not mix `decode_to_slice_trusted` with
    /// `read`/`collect` on the same `FrameDecoder`.
    ///
    /// When the frame is NOT eligible (no FCS in the header, or
    /// output buffer too small for the WILDCOPY slack, or active
    /// dictionary), this method falls back to a single-frame
    /// `decode_blocks` + `read` drain loop, draining into the
    /// caller's `output` slice. This is NOT `decode_all`: it
    /// processes only one frame (no trailing-frame iteration, no
    /// silent skippable-frame skip) and returns
    /// [`FrameDecoderError::TargetTooSmall`] if the decoded
    /// output does not fit in `output`.
    ///
    /// # Panic / DoS surface
    ///
    /// **For trusted input only.** On the direct path
    /// `UserSliceBackend` uses release-mode `assert!` for capacity
    /// checks across all three write entry points (`extend`,
    /// `extend_and_fill`, `extend_from_within_unchecked`). A
    /// malformed Compressed block whose payload expands past the
    /// declared `frame_content_size` (and beyond the
    /// `WILDCOPY_OVERLENGTH` slack the caller sized into `output`)
    /// will panic mid-block rather than returning a structured
    /// error. The per-block `produced > content_size` guard catches
    /// the overshoot AFTER the block, but cannot prevent the
    /// in-block writes from running first.
    ///
    /// The trade-off is deliberate for this PR. Making the writes
    /// fallible requires extending the `BufferBackend` trait
    /// surface, touching every backend implementation, and
    /// propagating `Result<_, _>` through the entire sequence
    /// executor — a refactor too large to fold into the direct
    /// decode wiring without losing review tractability. The
    /// follow-up issue tracking that work (referenced below in
    /// "Fallible BufferBackend writes") is a hard prerequisite
    /// before this entry point becomes safe to expose on
    /// untrusted streams.
    ///
    /// Callers handling untrusted input must use [`Self::decode_all`]
    /// which routes through `FlatBuf` / `RingBuffer`. Those
    /// backends grow via `Vec::reserve` (succeeds or aborts on
    /// alloc failure — not error-returning), but the growable Vec
    /// capacity absorbs a malformed block's overshoot inside the
    /// allocation; the frame-level checks then turn the size
    /// mismatch into `FrameContentSizeMismatch` instead of OOB
    /// writes into a fixed-size user slice. Fallible
    /// `BufferBackend` writes that would let `decode_to_slice_trusted`
    /// remain safe on adversarial input are tracked in issue #246.
    // The `_trusted` suffix is part of the API contract: this
    // entry point is for trusted input only. Adversarial /
    // malformed input can panic via release-mode `assert!` inside
    // `UserSliceBackend`. Callers handling untrusted data MUST use
    // `decode_all` instead. The fallible-`BufferBackend` refactor
    // that would let this entry point be safe on adversarial
    // input is tracked as a follow-up.
    #[doc(alias = "decode_to_slice")]
    #[must_use = "decode_to_slice_trusted returns the decoded byte count; ignoring it leaves the output's effective length ambiguous"]
    pub fn decode_to_slice_trusted(
        &mut self,
        mut input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, FrameDecoderError> {
        use super::block_decoder;
        use super::buffer_backend::WILDCOPY_OVERLENGTH;
        use super::decode_buffer::DecodeBuffer;
        use super::scratch::DirectScratch;
        use super::user_slice_buf::UserSliceBackend;
        use crate::io::Read;
        use FrameDecoderError as err;

        // Parse the frame header. This populates `self.state` with
        // the frame descriptor + resets the per-frame scratch
        // (DecoderScratchKind::Flat for single-segment, ::Ring
        // otherwise).
        //
        // Skippable frames are reported by `init` as
        // `ReadFrameHeaderError::SkipFrame`. The direct path
        // doesn't have a state model for "skip + decode next" since
        // it processes a single frame at most — propagate the error
        // unchanged so callers learn this input needs `decode_all`
        // (which iterates init and advances past skippable
        // payloads).
        self.init(&mut input)?;

        let state = self.state.as_mut().ok_or(err::NotYetInitialized)?;
        let content_size = state.frame_header.frame_content_size();
        let needed = content_size.saturating_add(WILDCOPY_OVERLENGTH as u64);
        // Eligibility independent of `single_segment_flag`:
        // multi-segment frames work via a coarse, block-boundary
        // cap on the visible buffer. The post-block
        // `drop_to_window_size` call in the loop below advances the
        // backend's `head` so `buffer.len()` doesn't grow past
        // `window_size` between blocks — bytes physically remain
        // in the user slice, just leave `len()`'s visible range so
        // a subsequent block's match-offset cannot reach back
        // arbitrarily far via stale history.
        //
        // This is NOT strict spec enforcement of
        // `offset <= window_size`: within a single block
        // `buffer.len()` can temporarily exceed `window_size` and
        // `DecodeBuffer::repeat` validates against `buffer.len()`
        // (not `window_size`), so an in-block match with
        // `offset > window_size` but `offset <= current
        // buffer.len()` is accepted on both direct and fallback
        // paths. See the `decode_to_slice_trusted` doc for the full
        // limitation note.
        //
        // Disabled when a dictionary is active: the persistent
        // `DecoderScratch::buffer.dict_content` seeded by
        // `init_from_dict` is NOT carried into the stack-local
        // `DecodeBuffer<UserSliceBackend>` we build below, so any
        // match that reaches into the external dictionary would
        // decode against an empty prefix. Fall back to the regular
        // path which keeps the dict in the persistent buffer.
        let dict_active = state.using_dict.is_some();
        // `content_checksum_flag` is no longer a disqualifier. The
        // direct path writes the decoded bytes into the user's
        // `output` slice, and we hash that slice once at the end of
        // decode (single sequential pass over cache-hot data). The
        // computed digest is then propagated into the persistent
        // `state.decoder_scratch.buffer.hash` so the public
        // `get_calculated_checksum()` accessor reads the correct
        // value just like on the `decode_all` path.
        let eligible = content_size > 0 && !dict_active && (output.len() as u64) >= needed;

        if !eligible {
            // Frame doesn't qualify for direct decode (empty
            // frame_content_size — header lacks FCS — or output too
            // small for the WILDCOPY slack). Fall through to the
            // existing per-block decode + drain path — `init`
            // already populated `self.state`, so `decode_blocks` +
            // `read` pick up from there.
            let mut output_tail: &mut [u8] = output;
            let mut total_bytes_written = 0;
            loop {
                self.decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1024 * 1024))?;
                let bytes_written = self
                    .read(output_tail)
                    .map_err(err::FailedToDrainDecodebuffer)?;
                output_tail = &mut output_tail[bytes_written..];
                total_bytes_written += bytes_written;
                if self.can_collect() != 0 {
                    return Err(err::TargetTooSmall);
                }
                if self.is_finished() {
                    break;
                }
            }
            // When the frame header declares a `frame_content_size`,
            // ensure the drained byte count actually matches it.
            // Without this check a corrupt frame that sets FCS but
            // ends early (e.g. last-block flag on a sub-FCS payload)
            // would silently return success here, while the
            // eligible-frame direct path catches the same condition
            // via `FrameContentSizeMismatch`. The fallback path now
            // matches that behaviour.
            //
            // Use `fcs_declared()` (NOT `content_size > 0`) as the
            // "is FCS on the wire" gate. The two diverge on the
            // legitimate edge case of an empty frame with an
            // EXPLICIT FCS=0 on the wire (FCS_flag>=1 with bytes
            // reading 0, or single_segment+FCS_flag=0 with the
            // 1-byte FCS=0): `content_size` is 0 in BOTH the
            // "absent" and "explicitly zero" cases, while
            // `fcs_declared()` returns false only in the truly
            // absent case.
            let state = self.state.as_ref().expect("state populated by init");
            if state.frame_header.fcs_declared() && (total_bytes_written as u64) != content_size {
                return Err(err::FrameContentSizeMismatch {
                    declared: content_size,
                    produced: total_bytes_written as u64,
                });
            }
            return Ok(total_bytes_written);
        }

        // Direct decode path. Borrow the persistent fields
        // (HUF/FSE tables, offset_hist, scratch Vecs) out of the
        // existing single-segment Flat scratch; we keep them
        // populated across `decode_to_slice_trusted` calls (HUF table reuse
        // is the main scratch-reuse win on small frames). Then
        // construct a stack-local DecodeBuffer<UserSliceBackend<'o>>
        // over `output` and bundle into a `DirectScratch`.
        // Borrow persistent fields out of whichever scratch variant
        // `init` produced (Flat for single_segment, Ring for
        // multi-segment) — both expose the same set of HUF/FSE/Vec
        // fields; only `buffer` differs and we don't use that here.
        // Macro-style binding to avoid the closure / generic
        // gymnastics of returning multiple &mut from a match arm.
        let (huf, fse, offset_hist, literals_buffer, sequences, block_content_buffer, window_size) =
            match &mut state.decoder_scratch {
                DecoderScratchKind::Flat(s) => (
                    &mut s.huf,
                    &mut s.fse,
                    &mut s.offset_hist,
                    &mut s.literals_buffer,
                    &mut s.sequences,
                    &mut s.block_content_buffer,
                    s.buffer.window_size,
                ),
                DecoderScratchKind::Ring(s) => (
                    &mut s.huf,
                    &mut s.fse,
                    &mut s.offset_hist,
                    &mut s.literals_buffer,
                    &mut s.sequences,
                    &mut s.block_content_buffer,
                    s.buffer.window_size,
                ),
            };
        let backend = UserSliceBackend::from_slice(output);
        let buffer = DecodeBuffer::from_backend(backend, window_size);
        let mut direct = DirectScratch {
            huf,
            fse,
            offset_hist,
            literals_buffer,
            sequences,
            block_content_buffer,
            buffer,
        };

        // Block loop. Mirrors `decode_blocks` (without the
        // strategy-bounded early exit — we always decode the whole
        // frame in one shot for the direct path). Keeps
        // `state.bytes_read_counter` / `state.block_counter` in
        // sync with `decode_blocks` so post-call accessors
        // (`bytes_read_from_source`, `blocks_decoded`) return
        // accurate values.
        let mut block_dec = block_decoder::new();
        // Track total output bytes against the declared
        // `frame_content_size` via the buffer's actual write
        // counter — `BlockHeader.decompressed_size` is 0 for
        // Compressed blocks (the header parser can't know the
        // expanded size before decoding the body), so per-header
        // tracking would always count 0 for those blocks and
        // miscount frames that aren't pure Raw/RLE.
        let mut produced: u64 = 0;
        loop {
            let (block_header, hsize) = block_dec
                .read_block_header(&mut input)
                .map_err(err::FailedToReadBlockHeader)?;
            state.bytes_read_counter += u64::from(hsize);
            // Pre-flight FCS check ONLY for Raw / RLE blocks where
            // `decompressed_size` is the actual block output size.
            // For Compressed blocks the header field is 0; the
            // post-decode check below catches overflow via the
            // backend's actual write counter delta.
            let block_upper = u64::from(block_header.decompressed_size);
            if block_upper > 0 && produced + block_upper > content_size {
                // Frame is corrupt — Raw/RLE block headers claim
                // more output than the FCS allows. Caller's buffer
                // was sized against FCS, so this is decoder-side
                // corruption, not user sizing.
                return Err(err::FrameContentSizeMismatch {
                    declared: content_size,
                    produced: produced + block_upper,
                });
            }
            // Slice-source fast path: consume the block body
            // straight from `input` without copying into the
            // persistent `block_content_buffer`.
            let before = direct.buffer.total_produced();
            let body_consumed = match block_dec.decode_block_content_from_slice(
                &block_header,
                &mut direct,
                &mut input,
            ) {
                Ok(n) => n,
                // Defense-in-depth: RLE / Raw block whose declared
                // `decompressed_size` slipped past the per-block
                // pre-flight above (e.g. due to overflow arithmetic)
                // and tripped the backend's fallible write surface.
                // The pre-flight catches this case via
                // `FrameContentSizeMismatch` already; convert here
                // for the residual paths to keep the public surface
                // panic-free.
                Err(crate::decoding::errors::DecodeBlockContentError::BackendOverflow {
                    step: _,
                }) => {
                    // Use saturating_add — this arm is reached when the
                    // block content exceeded the backend's capacity, i.e.
                    // exactly the case where naive `produced +
                    // decompressed_size` is most at risk of overflowing
                    // u64 itself on adversarial header values (e.g.
                    // `decompressed_size == u64::MAX`). Reporting
                    // `u64::MAX` is informative enough for the caller
                    // and avoids a panic-in-debug / wrap-in-release
                    // bug in the error-path itself, which would
                    // undermine the defense-in-depth this conversion
                    // exists for.
                    return Err(err::FrameContentSizeMismatch {
                        declared: content_size,
                        produced: produced
                            .saturating_add(u64::from(block_header.decompressed_size)),
                    });
                }
                Err(e) => return Err(err::FailedToReadBlockBody(e)),
            };
            produced = direct.buffer.total_produced();
            // Post-decode FCS overflow check. Works uniformly for
            // Raw/RLE/Compressed since it reads the actual bytes
            // written by the backend rather than the header's
            // (possibly zero) `decompressed_size`.
            if produced > content_size {
                return Err(err::FrameContentSizeMismatch {
                    declared: content_size,
                    produced,
                });
            }
            // Silence unused-binding warning when this delta isn't
            // consulted — it's there so future debug builds can
            // assert (produced - before) <= MAX_BLOCK_SIZE if the
            // spec invariant ever needs an explicit gate.
            let _ = before;
            state.bytes_read_counter += body_consumed;
            state.block_counter += 1;
            // Cap the visible buffer at window_size between blocks
            // so the next block's match-offset validation matches
            // the spec's `offset <= window_size` rule. Bytes
            // physically stay in the user slice; we just narrow
            // the visible range via `head`. For single-segment
            // frames `content_size <= window_size` so this is a
            // no-op on every iteration; for multi-segment frames
            // it advances `head` once `tail - head` outgrows the
            // window.
            direct.buffer.drop_to_window_size();
            if block_header.last_block {
                if state.frame_header.descriptor.content_checksum_flag() {
                    let mut chksum = [0u8; 4];
                    input
                        .read_exact(&mut chksum)
                        .map_err(err::FailedToReadChecksum)?;
                    state.bytes_read_counter += 4;
                    state.check_sum = Some(u32::from_le_bytes(chksum));
                }
                break;
            }
        }
        // Final sanity: blocks summed to exactly `content_size`. A
        // malformed frame with `last_block` set early (or one whose
        // block headers under-count) would land here. Distinct from
        // TargetTooSmall — the caller did their part, the frame
        // itself is corrupt.
        if produced != content_size {
            return Err(err::FrameContentSizeMismatch {
                declared: content_size,
                produced,
            });
        }

        // `direct.buffer.len()` would only show the visible (post
        // head-advance) range, not the total bytes physically
        // written into the user slice. The decode succeeded so
        // `content_size` is the authoritative byte count — the
        // backend wrote exactly that many bytes starting at
        // `output[0]`.
        let written = content_size as usize;
        state.frame_finished = true;
        // Drop the stack-local DirectScratch (and its DecodeBuffer
        // borrow on `output`) so we can re-borrow `output` for the
        // hash pass below. After this point `direct` is gone.
        drop(direct);
        #[cfg(feature = "hash")]
        {
            // Direct path bypasses the per-write hash accounting
            // (DecodeBuffer hashes during drain; the direct path
            // never drains because the user slice IS the buffer).
            // Walk the decoded output once and propagate the
            // resulting hasher state into the persistent scratch's
            // buffer so `get_calculated_checksum()` returns the
            // right value. Cost: ~330 us / MiB at xxhash's
            // ~3 GB/s throughput on x86_64, against cache-hot data.
            //
            // Done unconditionally for every successful direct
            // decode (not just frames with `content_checksum_flag`)
            // so `get_calculated_checksum()` returns the running
            // digest path-independently — matches what
            // `decode_all`'s drain-time hashing produces on
            // checksumless frames too.
            use core::hash::Hasher;
            let mut hasher = twox_hash::XxHash64::with_seed(0);
            hasher.write(&output[..written]);
            match &mut state.decoder_scratch {
                DecoderScratchKind::Flat(s) => s.buffer.hash = hasher,
                DecoderScratchKind::Ring(s) => s.buffer.hash = hasher,
            }
        }
        Ok(written)
    }
}

/// Read bytes from the decode_buffer that are no longer needed. While the frame is not yet finished
/// this will retain window_size bytes, else it will drain it completely
impl Read for FrameDecoder {
    fn read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        let state = match &mut self.state {
            None => return Ok(0),
            Some(s) => s,
        };
        if state.frame_finished {
            state.decoder_scratch.buffer_read_all(target)
        } else {
            state.decoder_scratch.buffer_read(target)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{DictionaryHandle, FrameDecoder};
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec::Vec;

    #[test]
    fn decode_to_slice_trusted_matches_decode_all_on_single_segment_frame() {
        // Roundtrip a small payload through the encoder, then decode
        // it via both `decode_all` and `decode_to_slice_trusted`. Both paths
        // must produce identical output bytes — the only difference
        // is the internal buffer/drain shape, not the decoded
        // semantics. This is the regression gate for the
        // direct-decode wiring.
        let payload: Vec<u8> = (0..4096u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        // Baseline: decode_all.
        let mut dec_a = FrameDecoder::new();
        let mut out_a = alloc::vec![0u8; payload.len()];
        let n_a = dec_a
            .decode_all(compressed.as_slice(), &mut out_a)
            .expect("decode_all should succeed");
        assert_eq!(n_a, payload.len());
        assert_eq!(&out_a[..n_a], payload.as_slice());

        // Direct: decode_to_slice_trusted with WILDCOPY slack.
        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec_b = FrameDecoder::new();
        let mut out_b = alloc::vec![0u8; payload.len() + slack];
        let n_b = dec_b
            .decode_to_slice_trusted(compressed.as_slice(), &mut out_b)
            .expect("decode_to_slice_trusted should succeed");
        assert_eq!(
            n_b,
            payload.len(),
            "direct decode produced wrong byte count"
        );
        assert_eq!(&out_b[..n_b], payload.as_slice());
    }

    #[test]
    fn decode_to_slice_trusted_multi_segment_frame_decodes_correctly() {
        // Multi-segment frame: payload large enough that the
        // encoder's default frame layout has `single_segment_flag =
        // false` and `window_size < frame_content_size`. The direct
        // path must cap the visible buffer at window_size after each
        // block (drop_to_window_size) so match-offset validation
        // matches the spec rule `offset <= window_size`, and still
        // produce the same bytes as decode_all on the
        // FlatBuf/Ring-backed path.
        //
        // Make the payload structured so multi-segment behavior
        // actually kicks in: 2 MiB of repeating + random-ish bytes
        // forces window_size lower than content_size at the encoder.
        let mut payload: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024);
        for i in 0..payload.capacity() {
            payload.push((i.wrapping_mul(2_654_435_761) & 0xFF) as u8);
        }
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        // Baseline: decode_all through the FlatBuf+drain path.
        let mut dec_a = FrameDecoder::new();
        let mut out_a = alloc::vec![0u8; payload.len()];
        let n_a = dec_a
            .decode_all(compressed.as_slice(), &mut out_a)
            .expect("decode_all should succeed");
        assert_eq!(n_a, payload.len());
        assert_eq!(&out_a[..n_a], payload.as_slice());

        // Direct path: must give identical bytes via UserSliceBackend
        // + per-block drop_to_window_size.
        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec_b = FrameDecoder::new();
        let mut out_b = alloc::vec![0u8; payload.len() + slack];
        let n_b = dec_b
            .decode_to_slice_trusted(compressed.as_slice(), &mut out_b)
            .expect("decode_to_slice_trusted should succeed on multi-segment frame");
        assert_eq!(n_b, payload.len(), "wrong byte count on direct path");
        assert_eq!(&out_b[..n_b], payload.as_slice());

        // Sanity-check: confirm the encoded frame really IS
        // multi-segment. If a future encoder default changes,
        // catching the assumption here is better than silently
        // testing single_segment on this name.
        let mut sanity = FrameDecoder::new();
        sanity.init(&mut compressed.as_slice()).unwrap();
        assert!(
            !sanity
                .state
                .as_ref()
                .unwrap()
                .frame_header
                .descriptor
                .single_segment_flag(),
            "test precondition violated: frame is single-segment, rename or resize"
        );
    }

    #[cfg(feature = "hash")]
    #[test]
    fn decode_to_slice_trusted_propagates_checksum_into_persistent_scratch() {
        // Direct path on a checksum-flagged frame: the FrameCompressor
        // under `feature = "hash"` sets content_checksum_flag, so the
        // decoded frame has a recorded checksum. After
        // decode_to_slice_trusted we must be able to verify it matches via
        // the public get_calculated_checksum() accessor — the digest
        // is computed by walking output at end of decode and stored
        // into the persistent scratch's hasher.
        let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; payload.len() + slack];
        let n = dec
            .decode_to_slice_trusted(compressed.as_slice(), &mut out)
            .expect("decode_to_slice_trusted with checksum must succeed");
        assert_eq!(n, payload.len());
        assert_eq!(&out[..n], payload.as_slice());

        // Both sides must report the same checksum: the frame header
        // carries the stored u32, and get_calculated_checksum reads
        // the running digest the direct path just propagated.
        let stored = dec.get_checksum_from_data();
        let calculated = dec.get_calculated_checksum();
        assert!(stored.is_some(), "frame must carry stored checksum");
        assert!(
            calculated.is_some(),
            "direct path must propagate calculated checksum"
        );
        assert_eq!(
            stored, calculated,
            "stored vs calculated checksum mismatch on direct path"
        );
    }

    #[test]
    fn decode_to_slice_trusted_fcs_overflow_via_corrupt_frame_returns_structured_error() {
        // Hand-build a corrupt frame that declares
        // frame_content_size = 4 but the (last) block carries a
        // larger Raw payload. The pre-flight FCS check inside the
        // direct path's block loop catches this and returns the
        // structured FrameContentSizeMismatch variant — not a
        // panic, not a generic TargetTooSmall.
        //
        // Frame layout (single_segment, FCS=4):
        //   magic            4 bytes  0xFD2FB528
        //   FHD              1 byte   single_segment=1, no checksum,
        //                              FCS field size = 0 (-> 1-byte FCS)
        //   FCS              1 byte   0x04
        //   block_header     3 bytes  last=1, type=Raw, block_size=10
        //   block_payload    10 bytes 0xAA repeated
        let mut frame = alloc::vec::Vec::new();
        // magic
        frame.extend_from_slice(&0xFD2FB528u32.to_le_bytes());
        // FHD: single_segment=1, fcs_flag=0 (1-byte FCS), no checksum,
        // no dict. Bit layout: FCS(7-6)=0, single_segment(5)=1,
        // reserved/uncs(4)=0, content_checksum(2)=0, dict(0-1)=00.
        frame.push(0b0010_0000);
        // FCS: 1 byte
        frame.push(4);
        // Block header: cBlockSize=10, type=Raw (0), last=1
        // 3-byte LE: bit0=last, bits1-2=type(2 bits), bits3-23=size
        let cblock_size: u32 = 10;
        let bh: u32 = 1 | (cblock_size << 3); // last=1, type=Raw=0
        frame.push((bh & 0xFF) as u8);
        frame.push((bh >> 8) as u8);
        frame.push((bh >> 16) as u8);
        // Payload — 10 bytes that, if decoded, would exceed FCS=4.
        frame.extend(core::iter::repeat_n(0xAAu8, 10));

        let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; 4 + slack];
        let err = dec
            .decode_to_slice_trusted(&frame, &mut out)
            .expect_err("FCS-overflow frame must fail decode");
        assert!(
            matches!(
                err,
                super::FrameDecoderError::FrameContentSizeMismatch { .. }
            ),
            "expected FrameContentSizeMismatch, got {:?}",
            err
        );
    }

    #[test]
    fn decode_to_slice_trusted_falls_back_when_output_too_small_for_wildcopy_slack() {
        // Output sized exactly to frame_content_size (no
        // WILDCOPY_OVERLENGTH slack) must NOT trigger the direct
        // path — the burst's `extend_from_within_unchecked` writes
        // past `tail` into the slack region. Direct dispatcher
        // recognises this and falls back to the FlatBuf + drain
        // path which still produces the right output.
        let payload: Vec<u8> = (0..2048u32)
            .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
            .collect();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let mut dec = FrameDecoder::new();
        // Exactly payload.len(), no slack — direct path is gated out.
        let mut out = alloc::vec![0u8; payload.len()];
        let n = dec
            .decode_to_slice_trusted(compressed.as_slice(), &mut out)
            .expect("decode_to_slice_trusted should still succeed via fallback");
        assert_eq!(n, payload.len());
        assert_eq!(&out[..n], payload.as_slice());
    }

    #[test]
    fn decode_to_slice_trusted_fallback_validates_fcs_against_total_output() {
        // Synthetic single-segment frame: FCS = 20 bytes, but the
        // last-block flag fires after only 4 bytes of raw payload.
        // On the direct path this would trip the post-block
        // `produced > content_size` check; the fallback path
        // (eligible=false because output is sized exactly to FCS,
        // no WILDCOPY slack) used to silently return Ok(4). With
        // the fix it now surfaces `FrameContentSizeMismatch`
        // matching the direct path.
        //
        // Frame layout: 4 B magic | 1 B FHD (single_segment=1,
        // FCS_flag=3 → 8-byte FCS) | 8 B FCS=20 | block header
        // (Raw, last, size=4) | 4 raw bytes.
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes()); // magic
        // FHD: FCS_flag=3 (8-byte FCS) <<6 | single_segment=1 <<5.
        wire.push(0b1110_0000);
        wire.extend_from_slice(&20u64.to_le_bytes()); // declared FCS
        // Block header: (size << 3) | (block_type << 1) | last_block.
        // Raw block (block_type=0), last_block=1, size=4 → 0b00100001 = 0x21.
        wire.push(0x21);
        wire.push(0x00);
        wire.push(0x00);
        wire.extend_from_slice(&[1u8, 2, 3, 4]);

        let mut dec = FrameDecoder::new();
        // Size output exactly at declared FCS (no WILDCOPY slack)
        // so the eligibility check gates the direct path out.
        let mut out = alloc::vec![0u8; 20];
        let err = dec
            .decode_to_slice_trusted(wire.as_slice(), &mut out)
            .expect_err("fallback must reject corrupt FCS underflow");
        match err {
            crate::decoding::errors::FrameDecoderError::FrameContentSizeMismatch {
                declared,
                produced,
            } => {
                assert_eq!(declared, 20);
                assert_eq!(produced, 4);
            }
            other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_to_slice_trusted_fallback_treats_explicit_fcs_zero_as_declared() {
        // Synthetic multi-segment frame with FCS_flag=2 (4-byte
        // FCS) explicitly set to 0. The header DECLARES zero
        // content, but the body carries a 5-byte raw last-block.
        // `fcs_declared()` must return true (the field is on the
        // wire) so the fallback's post-decode size check sees the
        // mismatch — even though `frame_content_size == 0`. This
        // is exactly the FCS=0 edge case where the previous
        // `content_size > 0` proxy would have silently accepted
        // the corrupt frame.
        //
        // Frame layout:
        //   4 B magic            — 28 B5 2F FD
        //   1 B FHD              — FCS_flag=2 (bits 7-6), no
        //                          single_segment, content_checksum=0,
        //                          dict_id_flag=0 → 0b1000_0000
        //   1 B window_descriptor — exp=10, mantissa=0 → window=1 MiB
        //   4 B FCS              — 0 LE
        //   3 B block header     — raw, last, size=5 → 0x29 0x00 0x00
        //   5 B raw payload      — anything non-empty
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        wire.push(0b1000_0000); // FHD: FCS_flag=2, others 0.
        wire.push(0x50); // window_descriptor: exp=10, mantissa=0.
        wire.extend_from_slice(&0u32.to_le_bytes()); // FCS = 0.
        // Block header (24-bit LE): (size << 3) | (block_type << 1) | last_block
        // = (5 << 3) | (0 << 1) | 1 = 0x29.
        wire.push(0x29);
        wire.push(0x00);
        wire.push(0x00);
        wire.extend_from_slice(&[1u8, 2, 3, 4, 5]);

        let mut dec = FrameDecoder::new();
        // FCS=0 declared, so eligibility (`content_size > 0`)
        // false — falls through to the drain loop. Output buffer
        // size doesn't matter for the eligibility check here;
        // give it some room so `read()` can drain the block.
        let mut out = alloc::vec![0u8; 16];
        let err = dec
            .decode_to_slice_trusted(wire.as_slice(), &mut out)
            .expect_err("corrupt FCS=0 + 5-byte block must error");
        match err {
            crate::decoding::errors::FrameDecoderError::FrameContentSizeMismatch {
                declared,
                produced,
            } => {
                assert_eq!(declared, 0);
                assert_eq!(produced, 5);
            }
            other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_to_slice_trusted_fallback_accepts_honest_explicit_fcs_zero() {
        // Companion to the corrupt-FCS=0 test above: an HONEST
        // empty frame with FCS_flag=2 (4-byte FCS) explicitly set
        // to 0 AND a 0-byte raw last-block. `fcs_declared()`
        // returns true and `content_size == 0 == total_written`,
        // so the fallback validation accepts the frame instead of
        // misreporting a mismatch.
        //
        // (Single-segment FCS=0 would test a similar invariant
        // but trips header-stage validation: `window_size =
        // frame_content_size = 0 < MIN_WINDOW_SIZE` fails the
        // window-size sanity check before decode runs. Use the
        // multi-segment shape where `window_size` comes from
        // `window_descriptor` independently of FCS.)
        //
        // Frame layout:
        //   4 B magic
        //   1 B FHD              — FCS_flag=2, others 0 → 0x80
        //   1 B window_descriptor — exp=10 → 1 MiB window
        //   4 B FCS              — 0 LE
        //   3 B block header     — raw, last, size=0 → 0x01 0x00 0x00
        let mut wire = Vec::new();
        wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        wire.push(0b1000_0000);
        wire.push(0x50);
        wire.extend_from_slice(&0u32.to_le_bytes());
        // Block header: (0 << 3) | (0 << 1) | 1 = 0x01.
        wire.push(0x01);
        wire.push(0x00);
        wire.push(0x00);

        let mut dec = FrameDecoder::new();
        let mut out = alloc::vec![0u8; 16];
        let n = dec
            .decode_to_slice_trusted(wire.as_slice(), &mut out)
            .expect("honest FCS=0 + empty block must succeed");
        assert_eq!(n, 0);
    }

    #[test]
    fn reset_with_dict_handle_applies_dict_when_no_dict_id() {
        let payload = b"reset-without-dict-id";
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload.as_slice());
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let handle = DictionaryHandle::decode_dict(dict_raw).expect("dictionary should parse");

        let mut decoder = FrameDecoder::new();
        decoder
            .reset_with_dict_handle(compressed.as_slice(), &handle)
            .expect("reset should succeed");
        let state = decoder.state.as_ref().expect("state should be initialized");
        assert!(state.frame_header.dictionary_id().is_none());
        assert_eq!(state.using_dict, Some(handle.id()));
    }

    #[cfg(feature = "lsm")]
    mod expect_validation {
        use super::*;
        use crate::decoding::errors::FrameDecoderError;

        fn compress(payload: &[u8]) -> Vec<u8> {
            let mut compressor = FrameCompressor::new(CompressionLevel::Default);
            compressor.set_source(payload);
            let mut compressed = Vec::new();
            compressor.set_drain(&mut compressed);
            compressor.compress();
            compressed
        }

        fn compress_with_dict(payload: &[u8], dict_raw: &[u8]) -> Vec<u8> {
            let mut compressor = FrameCompressor::new(CompressionLevel::Default);
            compressor
                .set_dictionary_from_bytes(dict_raw)
                .expect("dict load");
            compressor.set_source(payload);
            let mut compressed = Vec::new();
            compressor.set_drain(&mut compressed);
            compressor.compress();
            compressed
        }

        #[test]
        fn expect_dict_id_none_default_allows_anything() {
            let compressed = compress(b"hello-no-expect");
            let mut decoder = FrameDecoder::new();
            decoder
                .reset(compressed.as_slice())
                .expect("default None passes");
        }

        #[test]
        fn expect_dict_id_zero_matches_frame_without_dict_id() {
            // Default-encoded frame has no dict_id; pinning Some(0)
            // ("no dictionary expected") must accept it.
            let compressed = compress(b"payload");
            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(0));
            decoder
                .reset(compressed.as_slice())
                .expect("Some(0) ~ None");
        }

        #[test]
        fn expect_dict_id_matching_value_passes() {
            let dict_raw = include_bytes!("../../dict_tests/dictionary");
            let handle = DictionaryHandle::decode_dict(dict_raw).expect("dict parse");
            let actual_id = handle.id();

            let compressed = compress_with_dict(b"payload-with-dict", dict_raw);

            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(actual_id));
            // Decode requires the dict to be registered; using
            // reset_with_dict_handle for that.
            decoder
                .reset_with_dict_handle(compressed.as_slice(), &handle)
                .expect("matching dict_id passes");
        }

        #[test]
        fn expect_dict_id_mismatching_value_fails_before_decode() {
            let dict_raw = include_bytes!("../../dict_tests/dictionary");
            let handle = DictionaryHandle::decode_dict(dict_raw).expect("dict parse");
            let actual_id = handle.id();
            let wrong_id = actual_id.wrapping_add(1);

            let compressed = compress_with_dict(b"payload-with-dict", dict_raw);

            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(wrong_id));
            let err = decoder
                .reset_with_dict_handle(compressed.as_slice(), &handle)
                .expect_err("mismatch must fail");
            match err {
                FrameDecoderError::UnexpectedDictId { expected, found } => {
                    assert_eq!(expected, Some(wrong_id));
                    assert_eq!(found, Some(actual_id));
                }
                other => panic!("expected UnexpectedDictId, got {other:?}"),
            }
        }

        #[test]
        fn expect_dict_id_nonzero_fails_on_frame_without_dict_id() {
            // Frame has no dict_id; expecting Some(42) (non-zero)
            // must fail with found = None.
            let compressed = compress(b"no-dict-frame");
            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(42));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("nonzero expectation on dictless frame must fail");
            match err {
                FrameDecoderError::UnexpectedDictId { expected, found } => {
                    assert_eq!(expected, Some(42));
                    assert_eq!(found, None);
                }
                other => panic!("expected UnexpectedDictId, got {other:?}"),
            }
        }

        #[test]
        fn expect_window_descriptor_none_default_allows_anything() {
            let compressed = compress(b"hello-no-wd-expect");
            let mut decoder = FrameDecoder::new();
            decoder
                .reset(compressed.as_slice())
                .expect("default None passes");
        }

        #[test]
        fn expect_window_descriptor_mismatch_fails_before_decode() {
            // Compress a payload large enough to force a
            // multi-segment frame (window_descriptor on wire).
            // Default compression at >256 KiB produces multi-
            // segment frames with a real window_descriptor byte.
            let payload = alloc::vec![0xABu8; 512 * 1024];
            let compressed = compress(&payload);

            // Read the actual window_descriptor by decoding once
            // without expectations, then pin a wrong value.
            let mut probe_decoder = FrameDecoder::new();
            probe_decoder.reset(compressed.as_slice()).unwrap();
            let probe_state = probe_decoder.state.as_ref().unwrap();
            let actual_wd = probe_state
                .frame_header
                .window_descriptor()
                .expect("multi-segment frame should expose window_descriptor");
            let wrong_wd = actual_wd.wrapping_add(0x10); // bump exponent

            let mut decoder = FrameDecoder::new();
            decoder.expect_window_descriptor(Some(wrong_wd));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("wrong window_descriptor must fail");
            match err {
                FrameDecoderError::UnexpectedWindowDescriptor { expected, found } => {
                    assert_eq!(expected, wrong_wd);
                    assert_eq!(found, Some(actual_wd));
                }
                other => panic!("expected UnexpectedWindowDescriptor, got {other:?}"),
            }
        }

        /// Build a minimal synthetic single-segment zstd frame
        /// carrying a 4-byte raw payload. RFC 8878 §3.1.1.1
        /// layout, hand-rolled because our default
        /// `FrameCompressor` settings don't emit
        /// `single_segment_flag` for tiny inputs.
        ///
        /// Wire bytes (13 total for 4-byte payload):
        /// ```text
        /// 28 B5 2F FD       magic
        /// 20                FHD: single_segment=1, FCS_flag=0
        /// 04                FCS (single byte, value = payload.len())
        /// 21 00 00          block header: raw, last, size=4
        /// .. .. .. ..       payload bytes
        /// ```
        fn synth_single_segment_frame(payload: &[u8]) -> Vec<u8> {
            assert!(payload.len() <= 255, "1-byte FCS field caps at 255");
            assert!(payload.len() < (1usize << 21), "block size 21-bit max");
            let mut out = Vec::new();
            // Magic 0xFD2FB528 LE.
            out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
            // FHD: single_segment_flag (bit 5) set, everything
            // else zero. With single_segment + FCS_flag=0 the FCS
            // field is 1 byte. No window_descriptor on wire.
            out.push(0b0010_0000);
            // 1-byte FCS = payload length.
            out.push(payload.len() as u8);
            // Block header (3 bytes LE):
            // last_block=1, block_type=0 (Raw), block_size=payload.len().
            // Encoded: (size << 3) | (block_type << 1) | last_block.
            // Block header: last_block flag in bit 0, block_type
            // (0 = Raw) in bits 1-2, block size in bits 3+.
            let bh: u32 = ((payload.len() as u32) << 3) | 1;
            out.push((bh & 0xFF) as u8);
            out.push(((bh >> 8) & 0xFF) as u8);
            out.push(((bh >> 16) & 0xFF) as u8);
            // Raw payload.
            out.extend_from_slice(payload);
            out
        }

        #[test]
        fn expect_window_descriptor_on_single_segment_frame_fails_with_found_none() {
            // Single-segment frames omit the window_descriptor
            // byte from the wire entirely. Setting an expectation
            // here must surface `found: None` so callers
            // distinguish "wrong descriptor" from "no descriptor
            // on the wire" — never silently pass.
            let compressed = synth_single_segment_frame(b"tiny");

            // First sanity-check: the synthetic frame decodes
            // cleanly without any expectation.
            {
                let mut probe = FrameDecoder::new();
                probe
                    .reset(compressed.as_slice())
                    .expect("synth frame parses");
                let probe_state = probe.state.as_ref().unwrap();
                assert!(
                    probe_state.frame_header.window_descriptor().is_none(),
                    "synth frame must be single-segment"
                );
            }

            let mut decoder = FrameDecoder::new();
            decoder.expect_window_descriptor(Some(0x40));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("single-segment + expectation must fail");
            match err {
                FrameDecoderError::UnexpectedWindowDescriptor { expected, found } => {
                    assert_eq!(expected, 0x40);
                    assert_eq!(found, None);
                }
                other => panic!("expected UnexpectedWindowDescriptor, got {other:?}"),
            }
        }

        #[test]
        fn validation_failure_leaves_decoder_re_resettable() {
            // After UnexpectedDictId on a wrong-expectation reset,
            // clearing the expectation and re-calling reset must
            // succeed on the same source — no lingering failed
            // state.
            let compressed = compress(b"re-resettable");

            let mut decoder = FrameDecoder::new();
            decoder.expect_dict_id(Some(42));
            let err = decoder
                .reset(compressed.as_slice())
                .expect_err("first reset fails");
            assert!(matches!(err, FrameDecoderError::UnexpectedDictId { .. }));

            // Clear expectation and retry on a fresh source.
            decoder.expect_dict_id(None);
            decoder
                .reset(compressed.as_slice())
                .expect("retry after clearing expectation should succeed");
        }
    }
}
