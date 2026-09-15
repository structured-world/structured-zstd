use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::borrow::BorrowMut;
use core::marker::PhantomData;
use core::mem;

use crate::common::MAX_BLOCK_SIZE;
#[cfg(feature = "hash")]
use core::hash::Hasher;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

use crate::encoding::levels::compress_block_encoded;
use crate::encoding::{
    CompressionLevel, EncoderDictionary, MatchGeneratorDriver, Matcher, block_header::BlockHeader,
    frame_compressor::CompressState, frame_compressor::FrameTuning, frame_compressor::FseTables,
    frame_compressor::PreviousFseTable, frame_header::FrameHeader,
};
use crate::io::{Error, ErrorKind, Write};

/// Incremental frame encoder that implements [`Write`].
///
/// Data can be provided with multiple `write()` calls. Full blocks are compressed
/// automatically, `flush()` emits the currently buffered partial block as non-last,
/// and `finish()` closes the frame and returns the wrapped writer.
///
/// One encoder writes one frame into the drain it owns, through a
/// [`CompressionContext`] it owns ([`new`](StreamingEncoder::new)) or borrows
/// ([`with_context`](Self::with_context)). Borrowing is how frame after frame
/// is compressed with the same settings, dictionary and match-finder
/// allocations: the context outlives each encoder and is ready for the next
/// frame once [`finish`](Self::finish) returns.
pub struct StreamingEncoder<
    W: Write,
    M: Matcher = MatchGeneratorDriver,
    C: BorrowMut<CompressionContext<M>> = CompressionContext<M>,
> {
    drain: Option<W>,
    context: C,
    matcher: PhantomData<M>,
}

/// A reusable streaming compression context: the settings, the attached
/// dictionary, the match finder and its buffers, kept from one frame to the
/// next, with the output handed in on each call rather than owned. The
/// counterpart of upstream zstd's `ZSTD_CCtx` driven by `ZSTD_compressStream2`.
///
/// Settings apply from the next frame on and must be made before its first
/// [`write`](Self::write); [`finish_frame`](Self::finish_frame) closes the
/// frame and readies the context for another. A pledged size belongs to one
/// frame; every other setting, the dictionary included, stays until replaced.
/// A frame that fails leaves the context failed: every later call reports
/// that failure, and a new context is needed.
///
/// Reusing a context produces the same frames as a fresh
/// [`StreamingEncoder`] per frame, without rebuilding the match finder's
/// tables or re-attaching the dictionary for each of them.
///
/// # Examples
/// ```
/// use structured_zstd::encoding::{CompressionContext, CompressionLevel};
///
/// let mut context = CompressionContext::new(CompressionLevel::Default);
/// let mut frames = Vec::new();
/// for payload in [&b"first frame"[..], b"second frame"] {
///     let mut frame = Vec::new();
///     context.set_pledged_content_size(payload.len() as u64).unwrap();
///     context.write(&mut frame, payload).unwrap();
///     context.finish_frame(&mut frame).unwrap();
///     frames.push(frame);
/// }
/// use std::io::Read;
/// let mut decoder = structured_zstd::decoding::StreamingDecoder::new(&frames[1][..]).unwrap();
/// let mut decoded = Vec::new();
/// decoder.read_to_end(&mut decoded).unwrap();
/// assert_eq!(decoded, b"second frame");
/// ```
pub struct CompressionContext<M: Matcher = MatchGeneratorDriver> {
    compression_level: CompressionLevel,
    state: CompressState<M>,
    pending: Vec<u8>,
    encoded_scratch: Vec<u8>,
    errored: bool,
    last_error_kind: Option<ErrorKind>,
    last_error_message: Option<String>,
    frame_started: bool,
    /// Upper bound on emitted block sizes (upstream `ZSTD_c_targetCBlockSize`
    /// semantics; see `FrameCompressor::set_target_block_size`). `None` =
    /// the format's 128 KiB ceiling.
    target_block_size: Option<u32>,
    /// The pledged size of the frame in progress; cleared when it ends.
    pledged_content_size: Option<u64>,
    /// Advisory source-size hint from [`set_source_size_hint`](Self::set_source_size_hint).
    /// Unlike `pledged_content_size` it carries no end-of-frame enforcement, but
    /// it still feeds the small-input gates (matcher sizing AND the Fast HUF
    /// fast-path gate) so `set_source_size_hint(small)` reduces work the same way
    /// a pledge does. The HUF gate reads `pledged_content_size.or(source_size_hint)`.
    /// A parameter like upstream `ZSTD_c_srcSizeHint`, so it outlives a frame.
    source_size_hint: Option<u64>,
    /// Whether a pledged size is written into the header's
    /// `Frame_Content_Size` field (upstream `ZSTD_c_contentSizeFlag`).
    /// Pledge *enforcement* is independent of this flag — upstream
    /// validates consumed bytes against the pledge at frame end even
    /// when the header omits the field. Default `true`.
    content_size_flag: bool,
    bytes_consumed: u64,
    /// Upstream `ZSTD_compress_frameChunk` `savings`: bytes consumed minus
    /// bytes produced so far in this frame; the block pre-splitter only cuts
    /// full blocks once the frame has saved enough.
    savings: i64,
    tuning: FrameTuning,
    /// `ZSTD_f_zstd1_magicless` — omit the 4-byte magic number prefix.
    /// Default false. See [`Self::set_magicless`].
    magicless: bool,
    /// Whether to emit a trailing XXH64 content checksum and set the frame
    /// header's `Content_Checksum_flag` (upstream `ZSTD_c_checksumFlag`).
    /// Default `false`, matching the upstream library default; combined with
    /// the `hash` feature, so without `hash` no checksum is emitted
    /// regardless. See [`Self::set_content_checksum`].
    content_checksum: bool,
    /// Dictionary applied to each frame (upstream zstd `ZSTD_CCtx_loadDictionary`
    /// on a streaming context), with the entropy tables it seeds. `None` = no
    /// dictionary. Set before a frame's first write.
    dictionary: Option<EncoderDictionary>,
    /// Whether the frame header records the attached dictionary's ID
    /// (upstream `ZSTD_c_dictIDFlag`). Default `true`. Raw-content
    /// dictionaries (upstream `ZSTD_CCtx_refPrefix`) carry a synthetic
    /// non-zero ID that must not reach the wire, so their attach path
    /// turns this off. See [`Self::set_dictionary_id_flag`].
    dictionary_id_flag: bool,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
}

impl<W: Write> StreamingEncoder<W, MatchGeneratorDriver> {
    /// Creates a streaming encoder backed by the default match generator.
    ///
    /// The encoder writes compressed bytes into `drain` and applies `compression_level`
    /// to all subsequently written blocks.
    pub fn new(drain: W, compression_level: CompressionLevel) -> Self {
        Self::with_context(drain, CompressionContext::new(compression_level))
    }
}

impl<W: Write, C: BorrowMut<CompressionContext>> StreamingEncoder<W, MatchGeneratorDriver, C> {
    /// Configure fine-grained compression parameters; see
    /// [`CompressionContext::set_parameters`]. Must be called before the first
    /// [`write`](Write::write).
    pub fn set_parameters(
        &mut self,
        params: &crate::encoding::CompressionParameters,
    ) -> Result<(), Error> {
        self.context.borrow_mut().set_parameters(params)
    }
}

impl<W: Write, M: Matcher> StreamingEncoder<W, M> {
    /// Creates a streaming encoder with an explicitly provided matcher implementation.
    ///
    /// This constructor is primarily intended for tests and advanced callers that need
    /// custom match-window behavior.
    pub fn new_with_matcher(matcher: M, drain: W, compression_level: CompressionLevel) -> Self {
        Self::with_context(
            drain,
            CompressionContext::new_with_matcher(matcher, compression_level),
        )
    }
}

impl<W: Write, M: Matcher, C: BorrowMut<CompressionContext<M>>> StreamingEncoder<W, M, C> {
    /// Write one frame into `drain` through `context`: owned, or borrowed from
    /// a caller that keeps it for the next frame with every setting, the
    /// dictionary and the allocations it has.
    ///
    /// # Examples
    /// ```
    /// use std::io::Write;
    /// use structured_zstd::encoding::{CompressionContext, CompressionLevel, StreamingEncoder};
    ///
    /// let mut context = CompressionContext::new(CompressionLevel::Default);
    /// for payload in [&b"first frame"[..], b"second frame"] {
    ///     let mut encoder = StreamingEncoder::with_context(Vec::new(), &mut context);
    ///     encoder.write_all(payload).unwrap();
    ///     let frame = encoder.finish().unwrap();
    ///     assert!(!frame.is_empty());
    /// }
    /// ```
    pub fn with_context(drain: W, context: C) -> Self {
        Self {
            drain: Some(drain),
            context,
            matcher: PhantomData,
        }
    }

    /// Bound each block's payload; see
    /// [`CompressionContext::set_target_block_size`]. Must be set before the
    /// first write.
    pub fn set_target_block_size(&mut self, target: Option<u32>) -> Result<(), Error> {
        self.context.borrow_mut().set_target_block_size(target)
    }

    /// Enable or disable the trailing XXH64 content checksum; see
    /// [`CompressionContext::set_content_checksum`]. Must be called before the
    /// first write.
    pub fn set_content_checksum(&mut self, emit: bool) -> Result<(), Error> {
        self.context.borrow_mut().set_content_checksum(emit)
    }

    /// Enable or disable the magicless frame format; see
    /// [`CompressionContext::set_magicless`]. Must be called before the first
    /// write.
    pub fn set_magicless(&mut self, magicless: bool) -> Result<(), Error> {
        self.context.borrow_mut().set_magicless(magicless)
    }

    /// Pledge the total uncompressed content size of the frame; see
    /// [`CompressionContext::set_pledged_content_size`]. Must be called before
    /// the first write.
    pub fn set_pledged_content_size(&mut self, size: u64) -> Result<(), Error> {
        self.context.borrow_mut().set_pledged_content_size(size)
    }

    /// Control whether a pledged size reaches the header; see
    /// [`CompressionContext::set_content_size_flag`]. Must be called before
    /// the first write.
    pub fn set_content_size_flag(&mut self, emit: bool) -> Result<(), Error> {
        self.context.borrow_mut().set_content_size_flag(emit)
    }

    /// Provide an advisory size for the frame; see
    /// [`CompressionContext::set_source_size_hint`]. Must be called before the
    /// first write.
    pub fn set_source_size_hint(&mut self, size: u64) -> Result<(), Error> {
        self.context.borrow_mut().set_source_size_hint(size)
    }

    /// Attach a dictionary blob to the frame; see
    /// [`CompressionContext::set_dictionary_from_bytes`]. Must be called before
    /// the first write.
    pub fn set_dictionary_from_bytes(&mut self, raw_dictionary: &[u8]) -> Result<(), Error> {
        self.context
            .borrow_mut()
            .set_dictionary_from_bytes(raw_dictionary)
    }

    /// Whether the header records the dictionary ID; see
    /// [`CompressionContext::set_dictionary_id_flag`]. Must be set before the
    /// first write.
    pub fn set_dictionary_id_flag(&mut self, emit: bool) -> Result<(), Error> {
        self.context.borrow_mut().set_dictionary_id_flag(emit)
    }

    /// Attach an already-parsed [`EncoderDictionary`] to the frame; see
    /// [`CompressionContext::set_encoder_dictionary`]. Must be called before
    /// the first write.
    pub fn set_encoder_dictionary(&mut self, dict: EncoderDictionary) -> Result<(), Error> {
        self.context.borrow_mut().set_encoder_dictionary(dict)
    }

    /// Returns an immutable reference to the wrapped output drain.
    ///
    /// The drain remains available for the encoder lifetime; [`finish`](Self::finish)
    /// consumes the encoder and returns ownership of the drain.
    pub fn get_ref(&self) -> &W {
        self.drain
            .as_ref()
            .expect("streaming encoder drain is present until finish consumes self")
    }

    /// Total heap bytes this encoder's allocations hold, excluding the inline
    /// struct and the drain `W` (whose footprint the owner can measure through
    /// [`get_ref`](Self::get_ref)); see [`CompressionContext::heap_size`].
    pub fn heap_size(&self) -> usize {
        self.context.borrow().heap_size()
    }

    /// Returns a mutable reference to the wrapped output drain.
    ///
    /// It is inadvisable to directly write to the underlying writer, as doing
    /// so would corrupt the zstd frame being assembled by the encoder.
    ///
    /// The drain remains available for the encoder lifetime; [`finish`](Self::finish)
    /// consumes the encoder and returns ownership of the drain.
    pub fn get_mut(&mut self) -> &mut W {
        self.drain
            .as_mut()
            .expect("streaming encoder drain is present until finish consumes self")
    }

    /// Finalizes the current zstd frame and returns the wrapped output drain.
    ///
    /// If no payload was written yet, this still emits a valid empty frame.
    /// Calling this method consumes the encoder; a borrowed context is then
    /// ready for the next frame.
    pub fn finish(mut self) -> Result<W, Error> {
        let mut drain = self
            .drain
            .take()
            .expect("streaming encoder drain must be present when finishing");
        self.context.borrow_mut().finish_frame(&mut drain)?;
        Ok(drain)
    }

    fn drain_mut(&mut self) -> Result<(&mut W, &mut CompressionContext<M>), Error> {
        match self.drain.as_mut() {
            Some(drain) => Ok((drain, self.context.borrow_mut())),
            None => Err(other_error("streaming encoder has no active drain")),
        }
    }
}

impl<W: Write, M: Matcher, C: BorrowMut<CompressionContext<M>>> Write
    for StreamingEncoder<W, M, C>
{
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
        let (drain, context) = self.drain_mut()?;
        context.write(drain, buf)
    }

    fn flush(&mut self) -> Result<(), Error> {
        let (drain, context) = self.drain_mut()?;
        context.flush(drain)
    }
}

impl CompressionContext<MatchGeneratorDriver> {
    /// Creates a context backed by the default match generator, compressing
    /// at `compression_level`.
    pub fn new(compression_level: CompressionLevel) -> Self {
        Self::new_with_matcher(
            MatchGeneratorDriver::new(MAX_BLOCK_SIZE as usize, 1),
            compression_level,
        )
    }

    /// Configure fine-grained compression parameters (#27): resets the level to
    /// the parameters' level and installs the per-knob overrides (window / hash
    /// / chain / search logs, strategy, long-distance matching) applied at the
    /// next frame. Mirrors [`FrameCompressor::set_parameters`](crate::encoding::FrameCompressor::set_parameters).
    /// Must be called before the frame's first [`write`](Self::write). Only the
    /// built-in `MatchGeneratorDriver` exposes the override knobs, so this
    /// lives on the default-matcher impl.
    pub fn set_parameters(
        &mut self,
        params: &crate::encoding::CompressionParameters,
    ) -> Result<(), Error> {
        self.ensure_settable("compression parameters must be set before the first write")?;
        self.compression_level = params.level();
        let overrides = params.overrides();
        // Persist the strategy override so `ensure_frame_started`'s level-based
        // resync does not discard it (matching `FrameCompressor::set_parameters`).
        self.tuning = FrameTuning::from_overrides(&overrides);
        self.state.strategy_tag = self.tuning.strategy.map_or_else(
            || {
                crate::encoding::strategy::StrategyTag::for_compression_level(
                    self.compression_level,
                )
            },
            |(tag, _)| tag,
        );
        self.state.huf_optimal_search = crate::encoding::frame_compressor::huf_search_enabled(
            self.state.strategy_tag,
            self.pledged_content_size.or(self.source_size_hint),
        );
        self.state.matcher.set_param_overrides(Some(overrides));
        Ok(())
    }
}

impl<M: Matcher> CompressionContext<M> {
    /// Creates a context with an explicitly provided matcher implementation.
    ///
    /// This constructor is primarily intended for tests and advanced callers that need
    /// custom match-window behavior.
    pub fn new_with_matcher(matcher: M, compression_level: CompressionLevel) -> Self {
        Self {
            compression_level,
            state: CompressState {
                matcher,
                copy_tier: crate::decoding::simd_copy::ExactCopyTier::resolve(),
                last_huff_table: None,
                huff_table_spare: None,
                huff_rollback: None,
                huff_weights: Default::default(),
                seen_content: Default::default(),
                fse_tables: FseTables::new(),
                block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: crate::encoding::strategy::StrategyTag::for_compression_level(
                    compression_level,
                ),
                pre_split: crate::encoding::levels::config::level_pre_split(compression_level)
                    .map(|tier| tier as u8),
                huf_optimal_search: true,
                literal_compression_disabled: matches!(
                    compression_level,
                    CompressionLevel::Level(n) if n < 0
                ),
            },
            pending: Vec::new(),
            encoded_scratch: Vec::new(),
            errored: false,
            last_error_kind: None,
            last_error_message: None,
            frame_started: false,
            target_block_size: None,
            pledged_content_size: None,
            source_size_hint: None,
            content_size_flag: true,
            bytes_consumed: 0,
            savings: 0,
            tuning: FrameTuning::default(),
            magicless: false,
            content_checksum: false,
            dictionary: None,
            dictionary_id_flag: true,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
    }

    /// Compress the next frames at `level`, with the level's own tuning: any
    /// parameter override installed by
    /// [`set_parameters`](CompressionContext::set_parameters) is dropped, as
    /// [`FrameCompressor::set_compression_level`](crate::encoding::FrameCompressor::set_compression_level)
    /// drops it. Must be called before the frame's first [`write`](Self::write).
    ///
    /// # Examples
    /// ```
    /// use structured_zstd::encoding::{CompressionContext, CompressionLevel};
    ///
    /// let mut context = CompressionContext::new(CompressionLevel::Fastest);
    /// context.set_compression_level(CompressionLevel::Better).unwrap();
    /// let mut frame = Vec::new();
    /// context.write(&mut frame, b"compressed at the new level").unwrap();
    /// context.finish_frame(&mut frame).unwrap();
    /// ```
    pub fn set_compression_level(&mut self, level: CompressionLevel) -> Result<(), Error> {
        self.ensure_settable("the compression level must be set before the first write")?;
        self.compression_level = level;
        self.tuning = FrameTuning::default();
        self.state.matcher.clear_param_overrides();
        Ok(())
    }

    /// Set an upper bound on each physical block's payload (semantics of
    /// upstream `ZSTD_c_targetCBlockSize`): every block carries at most
    /// `target` payload bytes, +3-byte block header on the wire — the
    /// upstream knob is likewise a convergence target for block sizing,
    /// not a cap on header-inclusive wire bytes. Clamped to
    /// `[MIN_TARGET_BLOCK_SIZE, MAX_BLOCK_SIZE]`; mirrors
    /// `FrameCompressor::set_target_block_size`. Must be set before the
    /// frame's first write.
    pub fn set_target_block_size(&mut self, target: Option<u32>) -> Result<(), Error> {
        self.ensure_settable("the block-size target must be set before the first write")?;
        self.target_block_size = target.map(|t| {
            t.clamp(
                crate::common::MIN_TARGET_BLOCK_SIZE,
                crate::common::MAX_BLOCK_SIZE,
            )
        });
        Ok(())
    }

    /// Enable or disable the trailing XXH64 content checksum
    /// (upstream `ZSTD_c_checksumFlag`). Default `false`, matching the
    /// upstream library default (`ZSTD_c_checksumFlag = 0`). Must be called
    /// before the frame's first [`write`](Self::write); once the frame header
    /// is emitted the flag is fixed, so a late change returns an error rather
    /// than producing a header/trailer mismatch. Without the `hash` feature
    /// no checksum is emitted regardless.
    pub fn set_content_checksum(&mut self, emit: bool) -> Result<(), Error> {
        self.ensure_settable("content checksum must be set before the first write")?;
        self.content_checksum = emit;
        Ok(())
    }

    /// Enable or disable magicless frame format (`ZSTD_f_zstd1_magicless`).
    ///
    /// When set to `true`, the frame header omits the 4-byte magic number
    /// prefix. Must be called BEFORE the frame's first [`write`](Self::write)
    /// call; calling it after the frame header has already been emitted
    /// returns an error so the caller can't be misled into thinking they
    /// produced a magicless stream.
    pub fn set_magicless(&mut self, magicless: bool) -> Result<(), Error> {
        self.ensure_settable("magicless format must be set before the first write")?;
        self.magicless = magicless;
        Ok(())
    }

    /// Pledge the total uncompressed content size of the next frame.
    ///
    /// When set, the frame header will include a `Frame_Content_Size` field.
    /// This enables decoders to pre-allocate output buffers.
    /// The pledged size is also forwarded as a source-size hint to the
    /// matcher so small inputs can use smaller matching tables.
    ///
    /// Must be called **before** the frame's first [`write`](Self::write);
    /// calling it after the frame header has already been emitted returns an
    /// error. The pledge ends with the frame.
    pub fn set_pledged_content_size(&mut self, size: u64) -> Result<(), Error> {
        self.ensure_settable("pledged content size must be set before the first write")?;
        self.pledged_content_size = Some(size);
        // Also use pledged size as source-size hint so the matcher
        // can select smaller tables for small inputs.
        self.state.matcher.set_source_size_hint(size);
        Ok(())
    }

    /// Control whether the pledged size is written into the header's
    /// `Frame_Content_Size` field (upstream `ZSTD_c_contentSizeFlag`,
    /// default on). With the flag off the header omits the field, but a
    /// pledge set via [`set_pledged_content_size`](Self::set_pledged_content_size)
    /// is still enforced against the bytes actually written. Must be
    /// called before the frame's first [`write`](Self::write).
    pub fn set_content_size_flag(&mut self, emit: bool) -> Result<(), Error> {
        self.ensure_settable("content size flag must be set before the first write")?;
        self.content_size_flag = emit;
        Ok(())
    }

    /// Provide a hint about the total uncompressed size of each frame.
    ///
    /// Unlike [`set_pledged_content_size`](Self::set_pledged_content_size),
    /// this does **not** enforce that exactly `size` bytes are written; it
    /// may reduce matcher tables, advertised frame window, and block sizing
    /// for small inputs. A parameter, like upstream `ZSTD_c_srcSizeHint`: it
    /// applies to every frame until replaced. Must be called before the
    /// frame's first [`write`](Self::write).
    pub fn set_source_size_hint(&mut self, size: u64) -> Result<(), Error> {
        self.ensure_settable("source size hint must be set before the first write")?;
        self.state.matcher.set_source_size_hint(size);
        // Feed the same hint to the Fast HUF fast-path gate (resolved in
        // `set_parameters` / `ensure_frame_started` via
        // `pledged_content_size.or(source_size_hint)`), so a small advisory size
        // also lifts Fast streams off the expensive optimal-HUF search.
        self.source_size_hint = Some(size);
        Ok(())
    }

    /// Attach a dictionary blob to each frame (upstream zstd
    /// `ZSTD_CCtx_loadDictionary` on a streaming context, which loads in
    /// `ZSTD_dct_auto` mode): a blob prefixed with
    /// [`DICTIONARY_MAGIC`](crate::decoding::DICTIONARY_MAGIC) is a serialized
    /// dictionary, anything else is raw content. The dictionary primes the
    /// match-finder and seeds the first block's entropy tables + repeat
    /// offsets; a serialized one's ID is written into the frame header, while
    /// raw content has none to write, so the decoder must be given the same
    /// bytes explicitly. Must be called before the frame's first
    /// [`write`](Self::write); repeat offsets must be non-zero.
    pub fn set_dictionary_from_bytes(&mut self, raw_dictionary: &[u8]) -> Result<(), Error> {
        if raw_dictionary.is_empty() {
            // An empty buffer is how the same upstream entry point is told
            // there is no dictionary: it clears and succeeds. Still refused
            // once the frame is open, like any other attach.
            self.ensure_settable("dictionary must be attached before the first write")?;
            // What the match finder kept of it across frames (its primed
            // snapshot, the copy left resident) goes with it, entropy included.
            self.state.matcher.invalidate_primed_dictionary();
            self.dictionary = None;
            return Ok(());
        }
        let dict = EncoderDictionary::from_serialized_or_raw_content(raw_dictionary)
            .map_err(|err| invalid_input_error(&alloc::format!("invalid dictionary: {err:?}")))?;
        self.set_encoder_dictionary(dict)
    }

    /// Whether the frame header records the attached dictionary's ID
    /// (upstream `ZSTD_c_dictIDFlag` semantics; default `true`).
    /// Mirrors [`FrameCompressor::set_dictionary_id_flag`](crate::encoding::FrameCompressor::set_dictionary_id_flag).
    /// Decoders can still decode such frames by supplying the dictionary
    /// explicitly.
    pub fn set_dictionary_id_flag(&mut self, emit: bool) -> Result<(), Error> {
        self.ensure_settable("dictionary ID flag must be set before the first write")?;
        self.dictionary_id_flag = emit;
        Ok(())
    }

    /// Attach an already-parsed [`EncoderDictionary`] to each frame. See
    /// [`set_dictionary_from_bytes`](Self::set_dictionary_from_bytes); must be
    /// called before the frame's first write. The entropy tables it seeds were
    /// built when it was prepared, so attaching builds nothing.
    pub fn set_encoder_dictionary(&mut self, dict: EncoderDictionary) -> Result<(), Error> {
        self.ensure_settable("dictionary must be attached before the first write")?;
        // A zero id marks a raw-content dictionary, which carries no header to
        // hold one; the frame then records no dictionary ID and the decoder
        // must be given the same bytes explicitly.
        if dict.inner.offset_hist.contains(&0) {
            return Err(invalid_input_error(
                "dictionary carries a zero repeat offset",
            ));
        }
        // The match finder's primed snapshot and resident copy belong to the
        // dictionary being replaced; the next frame primes the new one.
        self.state.matcher.invalidate_primed_dictionary();
        self.dictionary = Some(dict);
        Ok(())
    }

    /// Total heap bytes this context's allocations hold, excluding the inline
    /// struct: match-finder tables / history / recycled buffers, retained
    /// Huffman tables, the staging `pending` / `encoded_scratch` buffers, the
    /// retained dictionary content, and its entropy tables. Mirrors
    /// `FrameCompressor::heap_size` so a context can report its true
    /// footprint through `ZSTD_sizeof_CCtx`.
    pub fn heap_size(&self) -> usize {
        let mut total = self.state.matcher.heap_size();
        total += self
            .state
            .last_huff_table
            .as_ref()
            .map_or(0, |table| table.heap_size());
        total += self
            .state
            .huff_table_spare
            .as_ref()
            .map_or(0, |table| table.heap_size());
        // Kept between blocks and frames; see `FrameCompressor::heap_size`.
        total += self.state.huff_weights.heap_size();
        total += self.state.retained_scratch_heap_size();
        total += self.state.seen_content.heap_size();
        total += self.pending.capacity();
        total += self.encoded_scratch.capacity();
        total += self
            .dictionary
            .as_ref()
            .map_or(0, EncoderDictionary::heap_size);
        total
    }

    /// Compress `buf` into the frame in progress, starting one (and writing
    /// its header to `drain`) if none is. Full blocks are compressed as they
    /// fill and written to `drain`; the rest stays buffered for the next call,
    /// [`flush`](Self::flush) or [`finish_frame`](Self::finish_frame).
    ///
    /// Returns how much of `buf` was taken, which is all of it unless a pledge
    /// set with [`set_pledged_content_size`](Self::set_pledged_content_size)
    /// allows less.
    pub fn write<D: Write + ?Sized>(&mut self, drain: &mut D, buf: &[u8]) -> Result<usize, Error> {
        self.ensure_open()?;
        if buf.is_empty() {
            return Ok(0);
        }

        // Check pledge before emitting the frame header so that a misuse
        // like set_pledged_content_size(0) + write(non_empty) doesn't leave
        // a partially-written header in the drain.
        if let Some(pledged) = self.pledged_content_size
            && self.bytes_consumed >= pledged
        {
            return Err(invalid_input_error(
                "write would exceed pledged content size",
            ));
        }

        self.ensure_frame_started(drain)?;

        // Enforce pledged upper bound: truncate the accepted slice to the
        // remaining allowance so that partial-write semantics are honored
        // (return Ok(n) with n < buf.len()) instead of failing the full call.
        let buf = if let Some(pledged) = self.pledged_content_size {
            let remaining_allowed = pledged
                .checked_sub(self.bytes_consumed)
                .ok_or_else(|| invalid_input_error("bytes consumed exceed pledged content size"))?;
            if remaining_allowed == 0 {
                return Err(invalid_input_error(
                    "write would exceed pledged content size",
                ));
            }
            let accepted = core::cmp::min(
                buf.len(),
                usize::try_from(remaining_allowed).unwrap_or(usize::MAX),
            );
            &buf[..accepted]
        } else {
            buf
        };

        let block_capacity = self.block_capacity();
        if self.pending.capacity() == 0 {
            self.pending = self.allocate_pending_space(block_capacity);
        }
        let mut remaining = buf;
        let mut consumed = 0usize;

        while !remaining.is_empty() {
            if let Some(result) = self.emit_full_pending_block(drain, block_capacity, consumed) {
                return result;
            }

            let available = block_capacity - self.pending.len();
            let to_take = core::cmp::min(remaining.len(), available);
            if to_take == 0 {
                break;
            }
            self.pending.extend_from_slice(&remaining[..to_take]);
            remaining = &remaining[to_take..];
            consumed += to_take;

            if let Some(result) = self.emit_full_pending_block(drain, block_capacity, consumed) {
                if let Ok(n) = &result {
                    self.bytes_consumed += *n as u64;
                }
                return result;
            }
        }
        self.bytes_consumed += consumed as u64;
        Ok(consumed)
    }

    /// Emit the buffered partial block as a non-last block and flush `drain`.
    pub fn flush<D: Write + ?Sized>(&mut self, drain: &mut D) -> Result<(), Error> {
        self.ensure_open()?;
        if self.pending.is_empty() {
            return drain.flush().map_err(|err| self.fail(err));
        }
        self.ensure_frame_started(drain)?;
        self.emit_pending_block(drain, false)?;
        drain.flush().map_err(|err| self.fail(err))
    }

    /// Close the frame in progress into `drain`: its last block, then its
    /// checksum when enabled. A frame nothing was written to is still a valid
    /// empty frame. The context is then ready for the next frame, with every
    /// setting and the dictionary as they were and the pledge cleared.
    pub fn finish_frame<D: Write + ?Sized>(&mut self, drain: &mut D) -> Result<(), Error> {
        self.ensure_open()?;

        // Validate the pledge before finalizing the frame. If this is called
        // before any writes, this also avoids emitting a header with an
        // incorrect FCS into the drain on mismatch.
        if let Some(pledged) = self.pledged_content_size
            && self.bytes_consumed != pledged
        {
            return Err(invalid_input_error(
                "pledged content size does not match bytes consumed",
            ));
        }

        self.ensure_frame_started(drain)?;

        if self.pending.is_empty() {
            self.write_empty_last_block(drain)
                .map_err(|err| self.fail(err))?;
        } else {
            self.emit_pending_block(drain, true)?;
        }

        #[cfg(feature = "hash")]
        if self.content_checksum {
            let checksum = self.hasher.finish() as u32;
            drain
                .write_all(&checksum.to_le_bytes())
                .map_err(|err| self.fail(err))?;
        }

        drain.flush().map_err(|err| self.fail(err))?;
        // What belongs to the frame goes with it; the settings, the dictionary
        // and every allocation stay for the next one.
        self.frame_started = false;
        self.bytes_consumed = 0;
        self.pledged_content_size = None;
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.errored {
            return Err(self.sticky_error());
        }
        Ok(())
    }

    /// Refuse a setting once the frame it would change is under way.
    fn ensure_settable(&self, too_late: &str) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(too_late));
        }
        Ok(())
    }

    // Cold path (only reached after poisoning). The format!() calls still allocate
    // in no_std even though error_with_kind_message/other_error_owned drop the
    // message; this is acceptable on an error recovery path to keep match arms simple.
    fn sticky_error(&self) -> Error {
        match (self.last_error_kind, self.last_error_message.as_deref()) {
            (Some(kind), Some(message)) => error_with_kind_message(
                kind,
                format!(
                    "streaming encoder is in an errored state due to previous {kind:?} failure: {message}"
                ),
            ),
            (Some(kind), None) => error_from_kind(kind),
            (None, Some(message)) => other_error_owned(format!(
                "streaming encoder is in an errored state: {message}"
            )),
            (None, None) => other_error("streaming encoder is in an errored state"),
        }
    }

    fn ensure_frame_started<D: Write + ?Sized>(&mut self, drain: &mut D) -> Result<(), Error> {
        if self.frame_started {
            return Ok(());
        }

        // Frames are independent, so the raw-skip's memory of emitted content
        // starts empty; the allocation is kept across frames.
        self.state.seen_content.reset_for_frame();
        // Same reason as the frame compressor's start: what the last frame
        // ended on is about to be replaced, and it is exactly the buffer this
        // frame wants to build into.
        self.state.fse_tables.park_previous_before_frame();
        self.ensure_level_supported()?;
        // A dictionary is only active when it can actually be primed: the level
        // compresses (not `Uncompressed`) AND the matcher supports priming AND a
        // dictionary is attached. Mirrors `FrameCompressor`'s `use_dictionary_state`
        // so a streaming frame never advertises a `Dictionary_ID`, disables
        // single-segment, or seeds dict entropy/offsets unless the dictionary is
        // genuinely in play (otherwise it would emit frames that needlessly
        // require a dictionary at decode time).
        let use_dictionary_state =
            !matches!(self.compression_level, CompressionLevel::Uncompressed)
                && self.state.matcher.supports_dictionary_priming()
                && self.dictionary.is_some();
        // The dictionary sizes select the CDict cParams tier (consumed inside
        // `reset`), so hand them over BEFORE reset.
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            self.state.matcher.set_dictionary_size_hint(dict.sizes());
        }
        // The matcher resolves the frame from the LAST hint it was handed, so
        // re-forward the authoritative size (`pledge.or(advisory)`, the same
        // value the gates below read) right before the reset: without this a
        // pledge followed by a different advisory hint (or vice versa) left
        // the matcher and the frame gates on different size tiers.
        if let Some(size) = self.pledged_content_size.or(self.source_size_hint) {
            self.state.matcher.set_source_size_hint(size);
        }
        self.state.matcher.reset(self.compression_level);
        // Sync `state.strategy_tag` / `state.pre_split` to the strategy the
        // matcher's reset resolved (size- and dictionary-adaptive; a public
        // strategy override wins on a plain frame, a dictionary frame runs the
        // CDict's strategy) so the literal-compression gates, the block
        // pre-splitter and the dictionary load below agree with the parse.
        // Mirrors `FrameCompressor::compress` and keeps both entry points
        // byte-equivalent.
        let hint = self.pledged_content_size.or(self.source_size_hint);
        let (params, dict_frame) = crate::encoding::frame_compressor::resolve_frame_params(
            self.compression_level,
            hint,
            self.dictionary.as_ref().filter(|_| use_dictionary_state),
        );
        crate::encoding::frame_compressor::sync_effective_strategy(
            &mut self.state,
            self.compression_level,
            &params,
            self.tuning.strategy.filter(|_| !dict_frame),
        );
        self.state.huf_optimal_search =
            crate::encoding::frame_compressor::huf_search_enabled(self.state.strategy_tag, hint);
        self.state.literal_compression_disabled =
            crate::encoding::frame_compressor::literal_compression_disabled(
                self.state.strategy_tag,
                self.compression_level,
                self.tuning.target_length.filter(|_| !dict_frame),
                self.tuning.literal_compression,
            );
        // Seed the repeat-offset history from the dictionary (upstream zstd
        // `ZSTD_compress_insertDictionary`), or the default rep codes
        // otherwise, and load the dictionary into the match finder: primed,
        // restored from a snapshot, or, on a reused context whose reset kept
        // it resident, left in place with its offsets reapplied.
        // `dict` borrows `self.dictionary`; `self.state` is a disjoint field.
        self.state.offset_hist = [1, 4, 8];
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            self.state.offset_hist = dict.inner.offset_hist;
            crate::encoding::frame_compressor::load_frame_dictionary(
                &mut self.state,
                self.compression_level,
                dict,
                hint,
            );
        }
        // Seed the first block's entropy from the dictionary's encoder tables
        // (upstream zstd `cdict->cBlockState`), or clear to defaults.
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            let cache = &dict.inner.entropy;
            self.state.last_huff_table.clone_from(&cache.huff);
            self.state
                .fse_tables
                .ll_previous
                .clone_from(&cache.ll_previous);
            self.state
                .fse_tables
                .ml_previous
                .clone_from(&cache.ml_previous);
            self.state
                .fse_tables
                .of_previous
                .clone_from(&cache.of_previous);
            let ll_entropy = match cache.ll_previous.as_ref() {
                Some(PreviousFseTable::Custom(table)) => Some(table.as_ref()),
                _ => None,
            };
            let ml_entropy = match cache.ml_previous.as_ref() {
                Some(PreviousFseTable::Custom(table)) => Some(table.as_ref()),
                _ => None,
            };
            let of_entropy = match cache.of_previous.as_ref() {
                Some(PreviousFseTable::Custom(table)) => Some(table.as_ref()),
                _ => None,
            };
            self.state.matcher.seed_dictionary_entropy(
                self.state.last_huff_table.as_ref(),
                ll_entropy,
                ml_entropy,
                of_entropy,
            );
        } else {
            self.state.last_huff_table = None;
            self.state.fse_tables.ll_previous = None;
            self.state.fse_tables.ml_previous = None;
            self.state.fse_tables.of_previous = None;
        }
        self.savings = 0;
        #[cfg(feature = "hash")]
        {
            self.hasher = XxHash64::with_seed(0);
        }

        let window_size = self.state.matcher.window_size();
        if window_size == 0 {
            return Err(invalid_input_error(
                "matcher reported window_size == 0, which is invalid",
            ));
        }

        // Single-segment is incompatible with a dictionary (the dictionary
        // pushes referenceable history before the content, so the frame needs
        // an explicit window descriptor); gate it off when a dict is attached,
        // mirroring `FrameCompressor`'s `!use_dictionary_state` guard.
        // Single-segment also requires the FCS field to be present
        // (`content_size_flag`): the layout drops the window descriptor,
        // so the header must carry the content size for decoders to size
        // their window.
        let single_segment = self.content_size_flag
            && !use_dictionary_state
            && self
                .pledged_content_size
                .map(|size| (512..=(1 << 14)).contains(&size) && size <= window_size)
                .unwrap_or(false);

        let header = FrameHeader {
            frame_content_size: if self.content_size_flag {
                self.pledged_content_size
            } else {
                None
            },
            single_segment,
            content_checksum: cfg!(feature = "hash") && self.content_checksum,
            dictionary_id: if use_dictionary_state && self.dictionary_id_flag {
                // Id 0 is a raw-content dictionary: RFC 8878 spells "no
                // dictionary ID" as an absent field, not as a stored zero.
                self.dictionary
                    .as_ref()
                    .map(|dict| dict.inner.id)
                    .filter(|id| *id != 0)
                    .map(u64::from)
            } else {
                None
            },
            window_size: if single_segment {
                None
            } else {
                Some(window_size)
            },
            magicless: self.magicless,
        };
        let mut encoded_header = Vec::new();
        header.serialize(&mut encoded_header);
        drain
            .write_all(&encoded_header)
            .map_err(|err| self.fail(err))?;

        self.frame_started = true;
        Ok(())
    }

    fn block_capacity(&self) -> usize {
        let matcher_window = self.state.matcher.window_size() as usize;
        let ceiling = self
            .target_block_size
            .map_or(MAX_BLOCK_SIZE as usize, |t| t as usize);
        core::cmp::max(1, core::cmp::min(matcher_window, ceiling))
    }

    fn allocate_pending_space(&mut self, block_capacity: usize) -> Vec<u8> {
        let mut space = match self.compression_level {
            CompressionLevel::Fastest
            | CompressionLevel::Default
            | CompressionLevel::Better
            | CompressionLevel::Best
            | CompressionLevel::Level(_) => self.state.matcher.get_next_space(),
            CompressionLevel::Uncompressed => Vec::new(),
        };
        space.clear();
        if space.capacity() > block_capacity {
            space.shrink_to(block_capacity);
        }
        if space.capacity() < block_capacity {
            space.reserve(block_capacity - space.capacity());
        }
        space
    }

    /// Where the full pending block is cut (upstream `ZSTD_compress_frameChunk`
    /// sizing every block with `ZSTD_optimalBlockSize`): the pre-splitter's
    /// boundary once the frame has saved enough, else the whole block.
    /// `remaining` is the input still to come as far as the splitter knows:
    /// a full block again while writes continue, the buffered bytes at the
    /// end of the frame.
    fn pre_split_len(&self, block_capacity: usize, remaining: usize) -> usize {
        if matches!(self.compression_level, CompressionLevel::Uncompressed) {
            return self.pending.len();
        }
        crate::encoding::frame_compressor::optimal_block_size_with(
            self.state.pre_split.map(usize::from),
            &self.pending,
            remaining,
            block_capacity,
            self.savings,
        )
        .min(self.pending.len())
    }

    /// Emit the first `block_len` pending bytes as a non-last block; the
    /// suffix stays pending (the next block starts with it, as the frame
    /// compressor's reader path carries a pre-split suffix). On a drain
    /// error the whole pending buffer is restored so no input is lost.
    fn emit_pending_prefix<D: Write + ?Sized>(
        &mut self,
        drain: &mut D,
        block_len: usize,
        block_capacity: usize,
    ) -> Result<(), Error> {
        let mut suffix = self.allocate_pending_space(block_capacity);
        suffix.extend_from_slice(&self.pending[block_len..]);
        let mut block = mem::replace(&mut self.pending, suffix);
        block.truncate(block_len);
        if let Err((err, mut restored_block)) = self.encode_block(drain, block, false) {
            restored_block.extend_from_slice(&self.pending);
            self.pending = restored_block;
            return Err(err);
        }
        Ok(())
    }

    fn emit_full_pending_block<D: Write + ?Sized>(
        &mut self,
        drain: &mut D,
        block_capacity: usize,
        consumed: usize,
    ) -> Option<Result<usize, Error>> {
        if self.pending.len() != block_capacity {
            return None;
        }
        let block_len = self.pre_split_len(block_capacity, block_capacity);
        if let Err(err) = self.emit_pending_prefix(drain, block_len, block_capacity) {
            let err = self.fail(err);
            if consumed > 0 {
                return Some(Ok(consumed));
            }
            return Some(Err(err));
        }
        None
    }

    fn emit_pending_block<D: Write + ?Sized>(
        &mut self,
        drain: &mut D,
        last_block: bool,
    ) -> Result<(), Error> {
        let block_capacity = self.block_capacity();
        if last_block {
            // A full final buffer is cut like any other block (the reader
            // path splits it with `remaining = len`); each cut prefix goes
            // out as a non-last block and the suffix is re-examined.
            while self.pending.len() == block_capacity {
                let block_len = self.pre_split_len(block_capacity, self.pending.len());
                if block_len == self.pending.len() {
                    break;
                }
                self.emit_pending_prefix(drain, block_len, block_capacity)
                    .map_err(|err| self.fail(err))?;
            }
        }
        let block = mem::take(&mut self.pending);
        if let Err((err, restored_block)) = self.encode_block(drain, block, last_block) {
            self.pending = restored_block;
            return Err(self.fail(err));
        }
        if !last_block {
            self.pending = self.allocate_pending_space(block_capacity);
        }
        Ok(())
    }

    // Exhaustive match kept intentionally: adding a new CompressionLevel
    // variant will produce a compile error here, forcing the developer to
    // decide whether the streaming encoder supports it before shipping.
    fn ensure_level_supported(&self) -> Result<(), Error> {
        match self.compression_level {
            CompressionLevel::Uncompressed
            | CompressionLevel::Fastest
            | CompressionLevel::Default
            | CompressionLevel::Better
            | CompressionLevel::Best
            | CompressionLevel::Level(_) => Ok(()),
        }
    }

    fn encode_block<D: Write + ?Sized>(
        &mut self,
        drain: &mut D,
        uncompressed_data: Vec<u8>,
        last_block: bool,
    ) -> Result<(), (Error, Vec<u8>)> {
        let mut raw_block = Some(uncompressed_data);
        let mut encoded = Vec::new();
        mem::swap(&mut encoded, &mut self.encoded_scratch);
        encoded.clear();
        let needed_capacity = self.block_capacity() + 3;
        if encoded.capacity() < needed_capacity {
            encoded.reserve(needed_capacity.saturating_sub(encoded.len()));
        }
        let mut moved_into_matcher = false;
        let raw_len = raw_block.as_ref().map_or(0, Vec::len);
        if raw_block.as_ref().is_some_and(|block| block.is_empty()) {
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size: 0,
            };
            header.serialize(&mut encoded);
        } else {
            match self.compression_level {
                CompressionLevel::Uncompressed => {
                    let block = raw_block.as_ref().expect("raw block missing");
                    let header = BlockHeader {
                        last_block,
                        block_type: crate::blocks::block::BlockType::Raw,
                        block_size: block.len() as u32,
                    };
                    header.serialize(&mut encoded);
                    encoded.extend_from_slice(block);
                }
                CompressionLevel::Fastest
                | CompressionLevel::Default
                | CompressionLevel::Better
                | CompressionLevel::Best
                | CompressionLevel::Level(_) => {
                    let block = raw_block.take().expect("raw block missing");
                    debug_assert!(!block.is_empty(), "empty blocks handled above");
                    let dict_active = self.dictionary.is_some()
                        && self.state.matcher.supports_dictionary_priming();
                    compress_block_encoded(
                        &mut self.state,
                        self.compression_level,
                        last_block,
                        crate::encoding::levels::BlockInput::Staged(block),
                        &mut encoded,
                        dict_active,
                        // No FrameEmitInfo on the streaming encoder path — it
                        // does not surface per-block layout, so no sidecar.
                        #[cfg(feature = "lsm")]
                        None,
                        #[cfg(all(feature = "lsm", feature = "hash"))]
                        None,
                    );
                    moved_into_matcher = true;
                }
            }
        }

        if let Err(err) = drain.write_all(&encoded) {
            encoded.clear();
            mem::swap(&mut encoded, &mut self.encoded_scratch);
            let restored = if moved_into_matcher {
                self.state.matcher.get_last_space().to_vec()
            } else {
                raw_block.unwrap_or_default()
            };
            return Err((err, restored));
        }
        // `savings` counts the block header too, as upstream's
        // `ZSTD_compress_frameChunk` does (`cSize` includes it).
        self.savings += raw_len as i64 - encoded.len() as i64;

        if moved_into_matcher {
            #[cfg(feature = "hash")]
            if self.content_checksum {
                self.hasher.write(self.state.matcher.get_last_space());
            }
        } else {
            self.hash_block(raw_block.as_deref().unwrap_or(&[]));
        }
        encoded.clear();
        mem::swap(&mut encoded, &mut self.encoded_scratch);
        Ok(())
    }

    fn write_empty_last_block<D: Write + ?Sized>(&mut self, drain: &mut D) -> Result<(), Error> {
        self.encode_block(drain, Vec::new(), true)
            .map_err(|(err, _)| err)
    }

    fn fail(&mut self, err: Error) -> Error {
        self.errored = true;
        if self.last_error_kind.is_none() {
            self.last_error_kind = Some(err.kind());
        }
        if self.last_error_message.is_none() {
            self.last_error_message = Some(err.to_string());
        }
        err
    }

    #[cfg(feature = "hash")]
    fn hash_block(&mut self, uncompressed_data: &[u8]) {
        if self.content_checksum {
            self.hasher.write(uncompressed_data);
        }
    }

    #[cfg(not(feature = "hash"))]
    fn hash_block(&mut self, _uncompressed_data: &[u8]) {}
}

fn error_from_kind(kind: ErrorKind) -> Error {
    Error::from(kind)
}

fn error_with_kind_message(kind: ErrorKind, message: String) -> Error {
    #[cfg(feature = "std")]
    {
        Error::new(kind, message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(kind, alloc::boxed::Box::new(message))
    }
}

fn invalid_input_error(message: &str) -> Error {
    #[cfg(feature = "std")]
    {
        Error::new(ErrorKind::InvalidInput, message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(
            ErrorKind::Other,
            alloc::boxed::Box::new(alloc::string::String::from(message)),
        )
    }
}

fn other_error_owned(message: String) -> Error {
    #[cfg(feature = "std")]
    {
        Error::other(message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(ErrorKind::Other, alloc::boxed::Box::new(message))
    }
}

fn other_error(message: &str) -> Error {
    #[cfg(feature = "std")]
    {
        Error::other(message)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(
            ErrorKind::Other,
            alloc::boxed::Box::new(alloc::string::String::from(message)),
        )
    }
}

#[cfg(test)]
mod tests;
