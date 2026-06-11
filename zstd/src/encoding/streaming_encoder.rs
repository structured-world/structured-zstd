use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem;

use crate::common::MAX_BLOCK_SIZE;
#[cfg(feature = "hash")]
use core::hash::Hasher;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

use crate::encoding::levels::compress_block_encoded;
use crate::encoding::{
    CompressionLevel, EncoderDictionary, MatchGeneratorDriver, Matcher, block_header::BlockHeader,
    frame_compressor::CachedDictionaryEntropy, frame_compressor::CompressState,
    frame_compressor::FseTables, frame_compressor::PreviousFseTable, frame_header::FrameHeader,
};
use crate::io::{Error, ErrorKind, Write};

/// Incremental frame encoder that implements [`Write`].
///
/// Data can be provided with multiple `write()` calls. Full blocks are compressed
/// automatically, `flush()` emits the currently buffered partial block as non-last,
/// and `finish()` closes the frame and returns the wrapped writer.
pub struct StreamingEncoder<W: Write, M: Matcher = MatchGeneratorDriver> {
    drain: Option<W>,
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
    pledged_content_size: Option<u64>,
    /// Whether a pledged size is written into the header's
    /// `Frame_Content_Size` field (upstream `ZSTD_c_contentSizeFlag`).
    /// Pledge *enforcement* is independent of this flag — upstream
    /// validates consumed bytes against the pledge at frame end even
    /// when the header omits the field. Default `true`.
    content_size_flag: bool,
    bytes_consumed: u64,
    /// Effective strategy tag when a public-parameter
    /// [`Strategy`](crate::encoding::Strategy) override (#27) is active, mirroring
    /// [`FrameCompressor`](crate::encoding::FrameCompressor)'s field. `Some`
    /// survives frame start so the literal-compression gates run the same
    /// strategy the matcher does; `None` keeps the level-derived tag.
    strategy_override: Option<crate::encoding::strategy::StrategyTag>,
    /// `ZSTD_f_zstd1_magicless` — omit the 4-byte magic number prefix.
    /// Default false. See [`Self::set_magicless`].
    magicless: bool,
    /// Whether to emit a trailing XXH64 content checksum and set the frame
    /// header's `Content_Checksum_flag` (upstream `ZSTD_c_checksumFlag`).
    /// Default `false`, matching the upstream library default; combined with
    /// the `hash` feature, so without `hash` no checksum is emitted
    /// regardless. See [`Self::set_content_checksum`].
    content_checksum: bool,
    /// Dictionary applied to the frame (donor `ZSTD_CCtx_loadDictionary` on a
    /// streaming context). `None` = no dictionary. Set before the first write.
    dictionary: Option<EncoderDictionary>,
    /// Encoder entropy tables (literals Huffman + LL/ML/OF FSE "previous"
    /// tables) the dictionary seeds into the first block, derived once when the
    /// dictionary is attached so each frame start is a cheap clone.
    dictionary_entropy_cache: Option<CachedDictionaryEntropy>,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
}

impl<W: Write> StreamingEncoder<W, MatchGeneratorDriver> {
    /// Creates a streaming encoder backed by the default match generator.
    ///
    /// The encoder writes compressed bytes into `drain` and applies `compression_level`
    /// to all subsequently written blocks.
    pub fn new(drain: W, compression_level: CompressionLevel) -> Self {
        Self::new_with_matcher(
            MatchGeneratorDriver::new(MAX_BLOCK_SIZE as usize, 1),
            drain,
            compression_level,
        )
    }

    /// Configure fine-grained compression parameters (#27): resets the level to
    /// the parameters' level and installs the per-knob overrides (window / hash
    /// / chain / search logs, strategy, long-distance matching) applied at the
    /// next frame. Mirrors [`FrameCompressor::set_parameters`]. Must be called
    /// before the first [`write`](Write::write). Only the built-in
    /// `MatchGeneratorDriver` exposes the override knobs, so this lives on the
    /// default-matcher impl.
    pub fn set_parameters(
        &mut self,
        params: &crate::encoding::CompressionParameters,
    ) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "compression parameters must be set before the first write",
            ));
        }
        self.compression_level = params.level();
        let overrides = params.overrides();
        // Persist the strategy override so `ensure_frame_started`'s level-based
        // resync does not discard it (matching `FrameCompressor::set_parameters`).
        self.strategy_override = overrides.strategy.map(|s| s.tag());
        self.state.strategy_tag = self.strategy_override.unwrap_or_else(|| {
            crate::encoding::strategy::StrategyTag::for_compression_level(self.compression_level)
        });
        self.state.matcher.set_param_overrides(Some(overrides));
        Ok(())
    }
}

impl<W: Write, M: Matcher> StreamingEncoder<W, M> {
    /// Creates a streaming encoder with an explicitly provided matcher implementation.
    ///
    /// This constructor is primarily intended for tests and advanced callers that need
    /// custom match-window behavior.
    pub fn new_with_matcher(matcher: M, drain: W, compression_level: CompressionLevel) -> Self {
        Self {
            drain: Some(drain),
            compression_level,
            state: CompressState {
                matcher,
                last_huff_table: None,
                huff_table_spare: None,
                fse_tables: FseTables::new(),
                block_scratch: crate::encoding::blocks::CompressedBlockScratch::new(),
                offset_hist: [1, 4, 8],
                strategy_tag: crate::encoding::strategy::StrategyTag::for_compression_level(
                    compression_level,
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
            content_size_flag: true,
            bytes_consumed: 0,
            strategy_override: None,
            magicless: false,
            content_checksum: false,
            dictionary: None,
            dictionary_entropy_cache: None,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
    }

    /// Set an upper bound on emitted block sizes (upstream
    /// `ZSTD_c_targetCBlockSize` semantics; clamped to
    /// `[MIN_TARGET_BLOCK_SIZE, MAX_BLOCK_SIZE]`). Must be set before the
    /// first write.
    pub fn set_target_block_size(&mut self, target: Option<u32>) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "the block-size target must be set before the first write",
            ));
        }
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
    /// before the first [`write`](Write::write); once the frame header is
    /// emitted the flag is fixed, so a late change returns an error rather
    /// than producing a header/trailer mismatch. Without the `hash` feature
    /// no checksum is emitted regardless.
    pub fn set_content_checksum(&mut self, emit: bool) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "content checksum must be set before the first write",
            ));
        }
        self.content_checksum = emit;
        Ok(())
    }

    /// Enable or disable magicless frame format (`ZSTD_f_zstd1_magicless`).
    ///
    /// When set to `true`, the frame header serialized by this encoder
    /// omits the 4-byte magic number prefix. Must be called BEFORE the
    /// first [`write`](Write::write) call; calling it after the frame
    /// header has already been emitted returns an error so the caller
    /// can't be misled into thinking they produced a magicless stream.
    pub fn set_magicless(&mut self, magicless: bool) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "magicless format must be set before the first write",
            ));
        }
        self.magicless = magicless;
        Ok(())
    }

    /// Pledge the total uncompressed content size for this frame.
    ///
    /// When set, the frame header will include a `Frame_Content_Size` field.
    /// This enables decoders to pre-allocate output buffers.
    /// The pledged size is also forwarded as a source-size hint to the
    /// matcher so small inputs can use smaller matching tables.
    ///
    /// Must be called **before** the first [`write`](Write::write) call;
    /// calling it after the frame header has already been emitted returns an
    /// error.
    pub fn set_pledged_content_size(&mut self, size: u64) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "pledged content size must be set before the first write",
            ));
        }
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
    /// called before the first [`write`](Write::write).
    pub fn set_content_size_flag(&mut self, emit: bool) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "content size flag must be set before the first write",
            ));
        }
        self.content_size_flag = emit;
        Ok(())
    }

    /// Provide a hint about the total uncompressed size for the next frame.
    ///
    /// Unlike [`set_pledged_content_size`](Self::set_pledged_content_size),
    /// this does **not** enforce that exactly `size` bytes are written; it
    /// may reduce matcher tables, advertised frame window, and block sizing
    /// for small inputs. Must be called before the first
    /// [`write`](Write::write).
    pub fn set_source_size_hint(&mut self, size: u64) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "source size hint must be set before the first write",
            ));
        }
        self.state.matcher.set_source_size_hint(size);
        Ok(())
    }

    /// Attach a serialized dictionary blob to the frame (donor
    /// `ZSTD_CCtx_loadDictionary` on a streaming context). The dictionary primes
    /// the match-finder and seeds the first block's entropy tables + repeat
    /// offsets, and its ID is written into the frame header. Must be called
    /// before the first [`write`](Write::write); the parsed dictionary must have
    /// a non-zero ID and non-zero repeat offsets.
    pub fn set_dictionary_from_bytes(&mut self, raw_dictionary: &[u8]) -> Result<(), Error> {
        let dict = EncoderDictionary::from_bytes(raw_dictionary)
            .map_err(|err| invalid_input_error(&alloc::format!("invalid dictionary: {err:?}")))?;
        self.set_encoder_dictionary(dict)
    }

    /// Attach an already-parsed [`EncoderDictionary`] to the frame. See
    /// [`set_dictionary_from_bytes`](Self::set_dictionary_from_bytes); must be
    /// called before the first write.
    pub fn set_encoder_dictionary(&mut self, dict: EncoderDictionary) -> Result<(), Error> {
        self.ensure_open()?;
        if self.frame_started {
            return Err(invalid_input_error(
                "dictionary must be attached before the first write",
            ));
        }
        let inner = &dict.inner;
        if inner.id == 0 {
            return Err(invalid_input_error("dictionary has a zero ID"));
        }
        if inner.offset_hist.contains(&0) {
            return Err(invalid_input_error(
                "dictionary carries a zero repeat offset",
            ));
        }
        self.dictionary_entropy_cache = Some(CachedDictionaryEntropy::from_dictionary(inner));
        self.dictionary = Some(dict);
        Ok(())
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

    /// Total heap bytes this encoder's allocations hold, excluding the
    /// inline struct and the drain `W` (whose footprint the owner can
    /// measure through [`get_ref`](Self::get_ref)): match-finder tables /
    /// history / recycled buffers, retained Huffman tables, the staging
    /// `pending` / `encoded_scratch` buffers, the retained dictionary
    /// content, and the cached dictionary entropy tables. Mirrors
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
        total += self.pending.capacity();
        total += self.encoded_scratch.capacity();
        total += self
            .dictionary
            .as_ref()
            .map_or(0, |d| d.inner.dict_content.capacity());
        total += self
            .dictionary_entropy_cache
            .as_ref()
            .map_or(0, CachedDictionaryEntropy::heap_size);
        total
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
    /// Calling this method consumes the encoder.
    pub fn finish(mut self) -> Result<W, Error> {
        self.ensure_open()?;

        // Validate the pledge before finalizing the frame. If finish() is
        // called before any writes, this also avoids emitting a header with
        // an incorrect FCS into the drain on mismatch.
        if let Some(pledged) = self.pledged_content_size
            && self.bytes_consumed != pledged
        {
            return Err(invalid_input_error(
                "pledged content size does not match bytes consumed",
            ));
        }

        self.ensure_frame_started()?;

        if self.pending.is_empty() {
            self.write_empty_last_block()
                .map_err(|err| self.fail(err))?;
        } else {
            self.emit_pending_block(true)?;
        }

        let mut drain = self
            .drain
            .take()
            .expect("streaming encoder drain must be present when finishing");

        #[cfg(feature = "hash")]
        if self.content_checksum {
            let checksum = self.hasher.finish() as u32;
            drain
                .write_all(&checksum.to_le_bytes())
                .map_err(|err| self.fail(err))?;
        }

        drain.flush().map_err(|err| self.fail(err))?;
        Ok(drain)
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.errored {
            return Err(self.sticky_error());
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

    fn drain_mut(&mut self) -> Result<&mut W, Error> {
        self.drain
            .as_mut()
            .ok_or_else(|| other_error("streaming encoder has no active drain"))
    }

    fn ensure_frame_started(&mut self) -> Result<(), Error> {
        if self.frame_started {
            return Ok(());
        }

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
        // The dictionary content size drives dict-tier match-finder sizing
        // (consumed inside `reset`), so hand it over BEFORE reset.
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            self.state
                .matcher
                .set_dictionary_size_hint(dict.inner.dict_content.len());
        }
        self.state.matcher.reset(self.compression_level);
        // Seed the repeat-offset history from the dictionary (donor
        // `ZSTD_compress_insertDictionary`), or the default rep codes otherwise.
        self.state.offset_hist = if use_dictionary_state {
            self.dictionary
                .as_ref()
                .map(|dict| dict.inner.offset_hist)
                .unwrap_or([1, 4, 8])
        } else {
            [1, 4, 8]
        };
        // Prime the match-finder with the dictionary content + offsets.
        // `dict` borrows `self.dictionary`; `self.state.matcher` is a disjoint
        // field, so the immutable dict borrow and the mutable matcher borrow
        // coexist (field-level borrow splitting) with no conflict.
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            let offset_hist = dict.inner.offset_hist;
            self.state
                .matcher
                .prime_with_dictionary(dict.inner.dict_content.as_slice(), offset_hist);
        }
        // Seed the first block's entropy from the dictionary's cached encoder
        // tables (donor `cdict->cBlockState`), or clear to defaults.
        if use_dictionary_state && let Some(cache) = self.dictionary_entropy_cache.as_ref() {
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
        // Sync `state.strategy_tag` from the active compression level so the
        // literal-compression gates (`min_literals_to_compress`, `min_gain`
        // in `encoding::blocks::compressed`) see the correct strategy for
        // every frame. Mirrors `FrameCompressor::compress` and keeps both
        // entry points byte-equivalent at the gate level. A public-parameter
        // strategy override (#27) wins over the level's derived tag so the
        // gates see the strategy the matcher actually runs.
        self.state.strategy_tag = self.strategy_override.unwrap_or_else(|| {
            crate::encoding::strategy::StrategyTag::for_compression_level(self.compression_level)
        });
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
            dictionary_id: if use_dictionary_state {
                self.dictionary.as_ref().map(|dict| dict.inner.id as u64)
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
        self.drain_mut()
            .and_then(|drain| drain.write_all(&encoded_header))
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

    fn emit_full_pending_block(
        &mut self,
        block_capacity: usize,
        consumed: usize,
    ) -> Option<Result<usize, Error>> {
        if self.pending.len() != block_capacity {
            return None;
        }

        let new_pending = self.allocate_pending_space(block_capacity);
        let full_block = mem::replace(&mut self.pending, new_pending);
        if let Err((err, restored_block)) = self.encode_block(full_block, false) {
            self.pending = restored_block;
            let err = self.fail(err);
            if consumed > 0 {
                return Some(Ok(consumed));
            }
            return Some(Err(err));
        }
        None
    }

    fn emit_pending_block(&mut self, last_block: bool) -> Result<(), Error> {
        let block = mem::take(&mut self.pending);
        if let Err((err, restored_block)) = self.encode_block(block, last_block) {
            self.pending = restored_block;
            return Err(self.fail(err));
        }
        if !last_block {
            let block_capacity = self.block_capacity();
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

    fn encode_block(
        &mut self,
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
                    // A primed dictionary makes "incompressible-looking" blocks
                    // matchable, so the raw-fast-path must NOT fire. But a dict is
                    // only PRIMED when the matcher supports priming — a non-priming
                    // matcher ignores the attached dictionary, so the raw-fast-path
                    // must stay enabled for it. (This arm is already non-Uncompressed.)
                    // Mirrors `FrameCompressor`'s `dict_active`.
                    let dict_active = self.dictionary.is_some()
                        && self.state.matcher.supports_dictionary_priming();
                    compress_block_encoded(
                        &mut self.state,
                        self.compression_level,
                        last_block,
                        block,
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

        if let Err(err) = self.drain_mut().and_then(|drain| drain.write_all(&encoded)) {
            encoded.clear();
            mem::swap(&mut encoded, &mut self.encoded_scratch);
            let restored = if moved_into_matcher {
                self.state.matcher.get_last_space().to_vec()
            } else {
                raw_block.unwrap_or_default()
            };
            return Err((err, restored));
        }

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

    fn write_empty_last_block(&mut self) -> Result<(), Error> {
        self.encode_block(Vec::new(), true).map_err(|(err, _)| err)
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

impl<W: Write, M: Matcher> Write for StreamingEncoder<W, M> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
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

        self.ensure_frame_started()?;

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
            if let Some(result) = self.emit_full_pending_block(block_capacity, consumed) {
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

            if let Some(result) = self.emit_full_pending_block(block_capacity, consumed) {
                if let Ok(n) = &result {
                    self.bytes_consumed += *n as u64;
                }
                return result;
            }
        }
        self.bytes_consumed += consumed as u64;
        Ok(consumed)
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.ensure_open()?;
        if self.pending.is_empty() {
            return self
                .drain_mut()
                .and_then(|drain| drain.flush())
                .map_err(|err| self.fail(err));
        }
        self.ensure_frame_started()?;
        self.emit_pending_block(false)?;
        self.drain_mut()
            .and_then(|drain| drain.flush())
            .map_err(|err| self.fail(err))
    }
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
mod tests {
    use crate::decoding::StreamingDecoder;
    use crate::encoding::{CompressionLevel, Matcher, Sequence, StreamingEncoder};
    use crate::io::{Error, ErrorKind, Read, Write};
    use alloc::vec;
    use alloc::vec::Vec;

    struct TinyMatcher {
        last_space: Vec<u8>,
        window_size: u64,
    }

    impl TinyMatcher {
        fn new(window_size: u64) -> Self {
            Self {
                last_space: Vec::new(),
                window_size,
            }
        }
    }

    impl Matcher for TinyMatcher {
        fn get_next_space(&mut self) -> Vec<u8> {
            vec![0; self.window_size as usize]
        }

        fn get_last_space(&mut self) -> &[u8] {
            self.last_space.as_slice()
        }

        fn commit_space(&mut self, space: Vec<u8>) {
            self.last_space = space;
        }

        fn skip_matching(&mut self) {}

        fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
            handle_sequence(Sequence::Literals {
                literals: self.last_space.as_slice(),
            });
        }

        fn reset(&mut self, _level: CompressionLevel) {
            self.last_space.clear();
        }

        fn window_size(&self) -> u64 {
            self.window_size
        }
    }

    struct FailingWriteOnce {
        writes: usize,
        fail_on_write_number: usize,
        sink: Vec<u8>,
    }

    impl FailingWriteOnce {
        fn new(fail_on_write_number: usize) -> Self {
            Self {
                writes: 0,
                fail_on_write_number,
                sink: Vec::new(),
            }
        }
    }

    impl Write for FailingWriteOnce {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            self.writes += 1;
            if self.writes == self.fail_on_write_number {
                return Err(super::other_error("injected write failure"));
            }
            self.sink.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    struct FailingWithKind {
        writes: usize,
        fail_on_write_number: usize,
        kind: ErrorKind,
    }

    impl FailingWithKind {
        fn new(fail_on_write_number: usize, kind: ErrorKind) -> Self {
            Self {
                writes: 0,
                fail_on_write_number,
                kind,
            }
        }
    }

    impl Write for FailingWithKind {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            self.writes += 1;
            if self.writes == self.fail_on_write_number {
                return Err(Error::from(self.kind));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    struct PartialThenFailWriter {
        writes: usize,
        fail_on_write_number: usize,
        partial_prefix_len: usize,
        terminal_failure: bool,
        sink: Vec<u8>,
    }

    impl PartialThenFailWriter {
        fn new(fail_on_write_number: usize, partial_prefix_len: usize) -> Self {
            Self {
                writes: 0,
                fail_on_write_number,
                partial_prefix_len,
                terminal_failure: false,
                sink: Vec::new(),
            }
        }
    }

    impl Write for PartialThenFailWriter {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            if self.terminal_failure {
                return Err(super::other_error("injected terminal write failure"));
            }

            self.writes += 1;
            if self.writes == self.fail_on_write_number {
                let written = core::cmp::min(self.partial_prefix_len, buf.len());
                if written > 0 {
                    self.sink.extend_from_slice(&buf[..written]);
                    self.terminal_failure = true;
                    return Ok(written);
                }
                return Err(super::other_error("injected terminal write failure"));
            }

            self.sink.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    /// Pre-write `set_magicless(true)` → emitted frame omits the
    /// magic prefix AND round-trips through a magicless-aware
    /// decoder.
    #[test]
    fn streaming_encoder_set_magicless_before_write_omits_magic_and_roundtrips() {
        use crate::common::MAGIC_NUM;
        let payload = b"streaming-magicless-roundtrip-".repeat(64);

        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder
            .set_magicless(true)
            .expect("set_magicless pre-write");
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();

        assert!(
            !compressed.starts_with(&MAGIC_NUM.to_le_bytes()),
            "magicless frame must omit the 4-byte magic prefix",
        );

        let mut decoder = crate::decoding::FrameDecoder::new();
        decoder.set_magicless(true);
        let mut cursor: &[u8] = compressed.as_slice();
        decoder.init(&mut cursor).expect("magicless init");
        decoder
            .decode_blocks(&mut cursor, crate::decoding::BlockDecodingStrategy::All)
            .expect("decode_blocks");
        let mut decoded: Vec<u8> = Vec::new();
        decoder
            .collect_to_writer(&mut decoded)
            .expect("collect_to_writer");
        assert_eq!(decoded, payload);
    }

    /// `set_magicless` after the first write MUST return an error
    /// (the frame header has already been emitted, flipping the flag
    /// can't affect the current frame). Mirrors
    /// `set_pledged_content_size` / `set_source_size_hint` semantics.
    #[test]
    fn streaming_encoder_set_magicless_after_first_write_errors() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"first-block").unwrap();
        let err = encoder
            .set_magicless(true)
            .expect_err("set_magicless after first write must error");
        assert_eq!(
            err.kind(),
            crate::io::ErrorKind::InvalidInput,
            "expected InvalidInput when setting magicless after frame_started, got {err:?}",
        );
    }

    #[test]
    fn streaming_encoder_roundtrip_multiple_writes() {
        let payload = b"streaming-encoder-roundtrip-".repeat(1024);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        for chunk in payload.chunks(313) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn flush_emits_nonempty_partial_output() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"partial-block").unwrap();
        encoder.flush().unwrap();
        let flushed_len = encoder.get_ref().len();
        assert!(
            flushed_len > 0,
            "flush should emit header+partial block bytes"
        );
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"partial-block");
    }

    #[test]
    fn flush_without_writes_does_not_emit_frame_header() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.flush().unwrap();
        assert!(encoder.get_ref().is_empty());
    }

    #[test]
    fn block_boundary_write_emits_block_in_same_call() {
        let mut boundary = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(4),
            Vec::new(),
            CompressionLevel::Uncompressed,
        );
        let mut below = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(4),
            Vec::new(),
            CompressionLevel::Uncompressed,
        );

        boundary.write_all(b"ABCD").unwrap();
        below.write_all(b"ABC").unwrap();

        let boundary_len = boundary.get_ref().len();
        let below_len = below.get_ref().len();
        assert!(
            boundary_len > below_len,
            "full block should be emitted immediately at block boundary"
        );
    }

    #[test]
    fn finish_consumes_encoder_and_emits_frame() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"abc").unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"abc");
    }

    #[test]
    fn finish_without_writes_emits_empty_frame() {
        let encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn write_empty_buffer_returns_zero() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        assert_eq!(encoder.write(&[]).unwrap(), 0);
        let _ = encoder.finish().unwrap();
    }

    #[test]
    fn uncompressed_level_roundtrip() {
        let payload = b"uncompressed-streaming-roundtrip".repeat(64);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Uncompressed);
        for chunk in payload.chunks(41) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn better_level_streaming_roundtrip() {
        let payload = b"better-level-streaming-test".repeat(256);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Better);
        for chunk in payload.chunks(53) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn zero_window_matcher_returns_invalid_input_error() {
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(0),
            Vec::new(),
            CompressionLevel::Fastest,
        );
        let err = encoder.write_all(b"payload").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn best_level_streaming_roundtrip() {
        // 200 KiB payload crosses the 128 KiB block boundary, exercising
        // multi-block emission and matcher state carry-over for Best.
        let payload = b"best-level-streaming-test".repeat(8 * 1024);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Best);
        for chunk in payload.chunks(53) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn write_failure_poisoning_is_sticky() {
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(4),
            FailingWriteOnce::new(1),
            CompressionLevel::Uncompressed,
        );

        assert!(encoder.write_all(b"ABCD").is_err());
        assert!(encoder.flush().is_err());
        assert!(encoder.write_all(b"EFGH").is_err());
        assert_eq!(encoder.get_ref().sink.len(), 0);
        assert!(encoder.finish().is_err());
    }

    #[test]
    fn poisoned_encoder_returns_original_error_kind() {
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(4),
            FailingWithKind::new(1, ErrorKind::BrokenPipe),
            CompressionLevel::Uncompressed,
        );

        let first_error = encoder.write_all(b"ABCD").unwrap_err();
        assert_eq!(first_error.kind(), ErrorKind::BrokenPipe);

        let second_error = encoder.write_all(b"EFGH").unwrap_err();
        assert_eq!(second_error.kind(), ErrorKind::BrokenPipe);
    }

    #[test]
    fn write_reports_progress_but_poisoning_is_sticky_after_later_block_failure() {
        let payload = b"ABCDEFGHIJKL";
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(4),
            FailingWriteOnce::new(3),
            CompressionLevel::Uncompressed,
        );

        let first_write = encoder.write(payload).unwrap();
        assert_eq!(first_write, 8);
        assert!(encoder.write(&payload[first_write..]).is_err());
        assert!(encoder.flush().is_err());
        assert!(encoder.write_all(b"EFGH").is_err());
    }

    #[test]
    fn partial_write_failure_after_progress_poisons_encoder() {
        let payload = b"ABCDEFGHIJKL";
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(4),
            PartialThenFailWriter::new(3, 1),
            CompressionLevel::Uncompressed,
        );

        let first_write = encoder.write(payload).unwrap();
        assert_eq!(first_write, 8);

        let second_write = encoder.write(&payload[first_write..]);
        assert!(second_write.is_err());
        assert!(encoder.flush().is_err());
        assert!(encoder.write_all(b"MNOP").is_err());
    }

    #[test]
    fn new_with_matcher_and_get_mut_work() {
        let matcher = TinyMatcher::new(128 * 1024);
        let mut encoder =
            StreamingEncoder::new_with_matcher(matcher, Vec::new(), CompressionLevel::Fastest);
        encoder.get_mut().extend_from_slice(b"");
        encoder.write_all(b"custom-matcher").unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"custom-matcher");
    }

    #[cfg(feature = "std")]
    #[test]
    fn streaming_encoder_output_decompresses_with_c_zstd() {
        let payload = b"tenant=demo op=put key=streaming value=abcdef\n".repeat(4096);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        for chunk in payload.chunks(1024) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        let mut decoded = Vec::with_capacity(payload.len());
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn pledged_content_size_written_in_header() {
        let payload = b"hello world, pledged size test";
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder
            .set_pledged_content_size(payload.len() as u64)
            .unwrap();
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();

        // Verify FCS is present and correct
        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(header.frame_content_size(), payload.len() as u64);

        // Verify roundtrip
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn pledged_content_size_mismatch_returns_error() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.set_pledged_content_size(100).unwrap();
        encoder.write_all(b"short payload").unwrap(); // 13 bytes != 100 pledged
        let err = encoder.finish().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn write_exceeding_pledge_returns_error() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.set_pledged_content_size(5).unwrap();
        let err = encoder.write_all(b"exceeds five bytes").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn write_straddling_pledge_reports_partial_progress() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.set_pledged_content_size(5).unwrap();
        // write() should accept exactly 5 bytes (partial progress)
        assert_eq!(encoder.write(b"abcdef").unwrap(), 5);
        // Next write should fail — pledge exhausted
        let err = encoder.write(b"g").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn encoded_scratch_capacity_is_reused_across_blocks() {
        let payload = vec![0xAB; 64 * 3];
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(64),
            Vec::new(),
            CompressionLevel::Uncompressed,
        );

        encoder.write_all(&payload[..64]).unwrap();
        let first_capacity = encoder.encoded_scratch.capacity();
        assert!(
            first_capacity >= 67,
            "expected encoded scratch to keep block header + payload capacity",
        );

        encoder.write_all(&payload[64..128]).unwrap();
        let second_capacity = encoder.encoded_scratch.capacity();
        assert!(
            second_capacity >= first_capacity,
            "encoded scratch capacity should be reused across block emits",
        );

        encoder.write_all(&payload[128..]).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn pledged_content_size_after_write_returns_error() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"already writing").unwrap();
        let err = encoder.set_pledged_content_size(15).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn source_size_hint_directly_reduces_window_header() {
        let payload = b"streaming-source-size-hint".repeat(64);

        let mut no_hint = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(11));
        no_hint.write_all(payload.as_slice()).unwrap();
        let no_hint_frame = no_hint.finish().unwrap();
        let no_hint_header = crate::decoding::frame::read_frame_header(no_hint_frame.as_slice())
            .unwrap()
            .0;
        let no_hint_window = no_hint_header.window_size().unwrap();

        let mut with_hint = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(11));
        with_hint
            .set_source_size_hint(payload.len() as u64)
            .unwrap();
        with_hint.write_all(payload.as_slice()).unwrap();
        let late_hint_err = with_hint
            .set_source_size_hint(payload.len() as u64)
            .unwrap_err();
        assert_eq!(late_hint_err.kind(), ErrorKind::InvalidInput);
        let with_hint_frame = with_hint.finish().unwrap();
        let with_hint_header =
            crate::decoding::frame::read_frame_header(with_hint_frame.as_slice())
                .unwrap()
                .0;
        let with_hint_window = with_hint_header.window_size().unwrap();

        assert!(
            with_hint_window <= no_hint_window,
            "source size hint should not increase advertised window"
        );

        let mut decoder = StreamingDecoder::new(with_hint_frame.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[cfg(feature = "std")]
    #[test]
    fn pledged_content_size_c_zstd_compatible() {
        let payload = b"tenant=demo op=put key=streaming value=abcdef\n".repeat(4096);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder
            .set_pledged_content_size(payload.len() as u64)
            .unwrap();
        for chunk in payload.chunks(1024) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        // FCS should be written
        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(header.frame_content_size(), payload.len() as u64);

        // C zstd should decompress successfully
        let mut decoded = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn single_segment_requires_pledged_to_fit_matcher_window() {
        let payload = b"streaming-window-gate-".repeat(60); // 1320 bytes
        let mut encoder = StreamingEncoder::new_with_matcher(
            TinyMatcher::new(1024),
            Vec::new(),
            CompressionLevel::Fastest,
        );
        encoder
            .set_pledged_content_size(payload.len() as u64)
            .unwrap();
        encoder.write_all(payload.as_slice()).unwrap();
        let compressed = encoder.finish().unwrap();

        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(header.frame_content_size(), payload.len() as u64);
        assert!(
            !header.descriptor.single_segment_flag(),
            "single-segment must stay off when pledged content size exceeds matcher window"
        );
        assert!(
            header.window_size().unwrap() >= 1024,
            "window descriptor should be present when single-segment is disabled"
        );
    }

    #[test]
    fn ensure_frame_started_refreshes_stale_strategy_tag_at_reset() {
        // The literal-compression gates (`min_literals_to_compress`,
        // `min_gain`) read `state.strategy_tag`. Regression: every
        // reset site MUST refresh that tag from the active compression
        // level — relying on construction-time initialization alone is
        // not enough, because later mutations or reuse patterns can
        // leave the tag stale.
        //
        // To exercise the RESET-time refresh (not just the
        // construction-time init that `StreamingEncoder::new` does for
        // free), this test deliberately corrupts `state.strategy_tag`
        // to a value that does NOT match the active level, then
        // triggers `ensure_frame_started` and asserts the reset path
        // wrote the correct tag back. If the sync line in
        // `ensure_frame_started` were deleted, the corrupted value
        // would survive the write and fail the assertion.
        use crate::encoding::strategy::StrategyTag;
        for level in [
            CompressionLevel::Fastest,
            CompressionLevel::Default,
            CompressionLevel::Better,
            CompressionLevel::Best,
        ] {
            let expected = StrategyTag::for_compression_level(level);
            let mut encoder = StreamingEncoder::new(Vec::new(), level);
            // Pick a sentinel that differs from the legitimate tag so
            // a missing reset-time sync is observable. BtUltra2 is the
            // most-aggressive variant; the four levels above resolve
            // to Fast/Dfast/Lazy/Lazy respectively, none equal to it.
            let sentinel = StrategyTag::BtUltra2;
            assert_ne!(
                expected, sentinel,
                "sentinel must differ from the legitimate tag at level {level:?}",
            );
            encoder.state.strategy_tag = sentinel;
            encoder.write_all(b"x").unwrap();
            assert_eq!(
                encoder.state.strategy_tag, expected,
                "reset-time strategy_tag sync missing at level {level:?}: \
                 sentinel survived `ensure_frame_started`",
            );
            let _ = encoder.finish().unwrap();
        }
    }

    /// Level 22 advertises the largest default window (`window_log 27` =
    /// 128 MiB). Because streaming omits FCS, that window is written verbatim
    /// into the frame header — so the encoder's max window MUST NOT exceed the
    /// decoder's [`crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE`], or our own
    /// decoder rejects our own frame with `WindowSizeTooBig`. Regression for
    /// the encoder↔decoder window-cap mismatch: streaming L22 must round-trip
    /// through `StreamingDecoder` (and, implicitly, any stock zstd decoder,
    /// which accepts up to the same 128 MiB default).
    #[test]
    fn level_22_streaming_window_roundtrips_in_our_decoder() {
        let payload = b"level-22-streaming-window-cap-".repeat(512);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::from_level(22));
        for chunk in payload.chunks(101) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        // The advertised window equals the L22 default (128 MiB) and must sit
        // at or below the decoder cap — otherwise the round-trip below fails.
        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        let window = header.window_size().unwrap();
        assert!(
            window <= crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE,
            "L22 advertised window {window} exceeds decoder cap {}",
            crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE,
        );

        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    /// `set_content_checksum(false)` before the first write must clear the
    /// frame header's `Content_Checksum_flag` and the frame must still
    /// round-trip through the decoder.
    #[test]
    fn streaming_encoder_set_content_checksum_false_clears_header_flag() {
        let payload = b"streaming-checksum-toggle-".repeat(64);
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder
            .set_content_checksum(false)
            .expect("set_content_checksum pre-write");
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert!(
            !header.descriptor.content_checksum_flag(),
            "content_checksum(false) must clear the frame header flag",
        );

        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    /// With the `hash` feature, disabling the checksum must drop exactly the
    /// 4-byte XXH64 trailer: the same payload encoded with the checksum on is
    /// 4 bytes longer and its header flag is set.
    #[cfg(feature = "hash")]
    #[test]
    fn streaming_encoder_set_content_checksum_false_omits_trailer() {
        let payload = b"streaming-checksum-trailer-".repeat(64);

        let mut with = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        // Explicit: the encoder default is off (upstream library parity).
        with.set_content_checksum(true)
            .expect("set_content_checksum pre-write");
        with.write_all(&payload).unwrap();
        let with_checksum = with.finish().unwrap();

        let mut without = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        without
            .set_content_checksum(false)
            .expect("set_content_checksum pre-write");
        without.write_all(&payload).unwrap();
        let without_checksum = without.finish().unwrap();

        assert!(
            crate::decoding::frame::read_frame_header(with_checksum.as_slice())
                .unwrap()
                .0
                .descriptor
                .content_checksum_flag(),
            "default checksum-on frame must set the header flag",
        );
        assert_eq!(
            with_checksum.len(),
            without_checksum.len() + 4,
            "checksum-on frame must carry exactly the 4-byte XXH64 trailer",
        );
    }

    /// `set_content_checksum` after the first write must error: the frame
    /// header (and its checksum flag) is already emitted, so a late flip would
    /// desync the header flag from the emitted trailer. Mirrors
    /// `set_magicless` / `set_pledged_content_size` semantics.
    #[test]
    fn streaming_encoder_set_content_checksum_after_first_write_errors() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"first-block").unwrap();
        let err = encoder
            .set_content_checksum(false)
            .expect_err("set_content_checksum after first write must error");
        assert_eq!(
            err.kind(),
            ErrorKind::InvalidInput,
            "expected InvalidInput when setting content checksum after frame_started, got {err:?}",
        );
    }

    #[test]
    fn no_pledged_size_omits_fcs_from_header() {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Fastest);
        encoder.write_all(b"no pledged size").unwrap();
        let compressed = encoder.finish().unwrap();

        // FCS should be omitted from the header; the decoder reports absent FCS as 0.
        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(header.frame_content_size(), 0);
        // Verify the descriptor confirms FCS field is truly absent (0 bytes),
        // not just FCS present with value 0.
        assert_eq!(header.descriptor.frame_content_size_bytes().unwrap(), 0);
    }

    #[test]
    fn streaming_encoder_with_dictionary_roundtrips_and_carries_dict_id() {
        use alloc::format;
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let dict_id = crate::decoding::Dictionary::decode_dict(dict_raw)
            .unwrap()
            .id;

        // Dictionary-resembling payload (the dict was trained on similar lines),
        // fed in many small writes so the dict + cross-block matching are both
        // exercised by the streaming path.
        let mut payload = Vec::new();
        for i in 0..400u32 {
            payload.extend_from_slice(
                format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccc\n")
                    .as_bytes(),
            );
        }

        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(19));
        encoder
            .set_dictionary_from_bytes(dict_raw)
            .expect("attach dictionary");
        for chunk in payload.chunks(777) {
            encoder.write_all(chunk).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        // The frame header advertises the dictionary ID (single-segment is
        // disabled for dictionary frames, so an explicit window is present).
        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(header.dictionary_id(), Some(dict_id));

        // Round-trip through a decoder primed with the SAME dictionary.
        let mut decoder =
            StreamingDecoder::new_with_dictionary_bytes(compressed.as_slice(), dict_raw).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);

        // The dictionary is actually used: the dict frame is no larger than the
        // no-dictionary frame on this dict-resembling payload (a dict that was
        // ignored could only ever make the frame the same size or bigger).
        let mut nodict = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(19));
        for chunk in payload.chunks(777) {
            nodict.write_all(chunk).unwrap();
        }
        let nodict_frame = nodict.finish().unwrap();
        assert!(
            compressed.len() <= nodict_frame.len(),
            "dict frame {} should not exceed no-dict frame {}",
            compressed.len(),
            nodict_frame.len()
        );
    }

    #[test]
    fn streaming_encoder_strategy_override_survives_frame_start() {
        // A `.strategy(...)` override must drive BOTH the matcher and the
        // literal-compression gates (`state.strategy_tag`) once the frame
        // starts. `ensure_frame_started` re-syncs the tag, so without persisting
        // the override it would silently fall back to the level's strategy and
        // diverge from `FrameCompressor` for the same parameters.
        use crate::encoding::{CompressionParameters, Strategy};
        let level = CompressionLevel::Fastest;
        let level_tag = crate::encoding::strategy::StrategyTag::for_compression_level(level);
        let override_tag = Strategy::Greedy.tag();
        assert_ne!(
            level_tag, override_tag,
            "test needs an override that changes the derived tag"
        );

        let params = CompressionParameters::builder(level)
            .strategy(Strategy::Greedy)
            .build()
            .unwrap();
        let payload = b"override must outlive the frame header";
        let mut encoder = StreamingEncoder::new(Vec::new(), level);
        encoder.set_parameters(&params).unwrap();
        encoder.write_all(payload).unwrap();
        assert_eq!(
            encoder.state.strategy_tag, override_tag,
            "strategy override was discarded when the frame started"
        );

        let compressed = encoder.finish().unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn streaming_encoder_uncompressed_with_dictionary_omits_dict_id() {
        // At `Uncompressed` the matcher cannot prime a dictionary, so an
        // attached dictionary must NOT be reflected in the frame: advertising a
        // `Dictionary_ID` would force a dictionary at decode time for a frame
        // that does not actually depend on one. Mirrors `FrameCompressor`'s
        // `use_dictionary_state` gate.
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let payload = b"tenant=demo table=orders region=eu payload=aaaaabbbbbccccc";
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Uncompressed);
        encoder
            .set_dictionary_from_bytes(dict_raw)
            .expect("attach dictionary");
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let header = crate::decoding::frame::read_frame_header(compressed.as_slice())
            .unwrap()
            .0;
        assert_eq!(
            header.dictionary_id(),
            None,
            "uncompressed frame must not require a dictionary at decode time"
        );

        // Decodes WITHOUT any dictionary.
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }
}
