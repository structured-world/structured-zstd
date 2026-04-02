//! Utilities and interfaces for encoding an entire frame. Allows reusing resources

use alloc::{boxed::Box, vec::Vec};
use core::convert::TryInto;
#[cfg(feature = "hash")]
use twox_hash::XxHash64;

#[cfg(feature = "hash")]
use core::hash::Hasher;

use super::{
    CompressionLevel, Matcher, block_header::BlockHeader, frame_header::FrameHeader, levels::*,
    match_generator::MatchGeneratorDriver,
};
use crate::fse::fse_encoder::{FSETable, default_ll_table, default_ml_table, default_of_table};

use crate::io::{Read, Write};

/// An interface for compressing arbitrary data with the ZStandard compression algorithm.
///
/// `FrameCompressor` will generally be used by:
/// 1. Initializing a compressor by providing a buffer of data using `FrameCompressor::new()`
/// 2. Starting compression and writing that compression into a vec using `FrameCompressor::begin`
///
/// # Examples
/// ```
/// use structured_zstd::encoding::{FrameCompressor, CompressionLevel};
/// let mock_data: &[_] = &[0x1, 0x2, 0x3, 0x4];
/// let mut output = std::vec::Vec::new();
/// // Initialize a compressor.
/// let mut compressor = FrameCompressor::new(CompressionLevel::Uncompressed);
/// compressor.set_source(mock_data);
/// compressor.set_drain(&mut output);
///
/// // `compress` writes the compressed output into the provided buffer.
/// compressor.compress();
/// ```
pub struct FrameCompressor<R: Read, W: Write, M: Matcher> {
    uncompressed_data: Option<R>,
    compressed_data: Option<W>,
    compression_level: CompressionLevel,
    dictionary: Option<crate::decoding::Dictionary>,
    dictionary_entropy_cache: Option<CachedDictionaryEntropy>,
    state: CompressState<M>,
    #[cfg(feature = "hash")]
    hasher: XxHash64,
}

#[derive(Clone, Default)]
struct CachedDictionaryEntropy {
    huff: Option<crate::huff0::huff0_encoder::HuffmanTable>,
    ll_previous: Option<PreviousFseTable>,
    ml_previous: Option<PreviousFseTable>,
    of_previous: Option<PreviousFseTable>,
}

#[derive(Clone)]
pub(crate) enum PreviousFseTable {
    // Default tables are immutable and already stored alongside the state, so
    // repeating them only needs a lightweight marker instead of cloning FSETable.
    Default,
    Custom(Box<FSETable>),
}

impl PreviousFseTable {
    pub(crate) fn as_table<'a>(&'a self, default: &'a FSETable) -> &'a FSETable {
        match self {
            Self::Default => default,
            Self::Custom(table) => table,
        }
    }
}

pub(crate) struct FseTables {
    pub(crate) ll_default: FSETable,
    pub(crate) ll_previous: Option<PreviousFseTable>,
    pub(crate) ml_default: FSETable,
    pub(crate) ml_previous: Option<PreviousFseTable>,
    pub(crate) of_default: FSETable,
    pub(crate) of_previous: Option<PreviousFseTable>,
}

impl FseTables {
    pub fn new() -> Self {
        Self {
            ll_default: default_ll_table(),
            ll_previous: None,
            ml_default: default_ml_table(),
            ml_previous: None,
            of_default: default_of_table(),
            of_previous: None,
        }
    }
}

pub(crate) struct CompressState<M: Matcher> {
    pub(crate) matcher: M,
    pub(crate) last_huff_table: Option<crate::huff0::huff0_encoder::HuffmanTable>,
    pub(crate) fse_tables: FseTables,
    /// Offset history for repeat offset encoding: [rep0, rep1, rep2].
    /// Initialized to [1, 4, 8] per RFC 8878 §3.1.2.5.
    pub(crate) offset_hist: [u32; 3],
}

impl<R: Read, W: Write> FrameCompressor<R, W, MatchGeneratorDriver> {
    /// Create a new `FrameCompressor`
    pub fn new(compression_level: CompressionLevel) -> Self {
        Self {
            uncompressed_data: None,
            compressed_data: None,
            compression_level,
            dictionary: None,
            dictionary_entropy_cache: None,
            state: CompressState {
                matcher: MatchGeneratorDriver::new(1024 * 128, 1),
                last_huff_table: None,
                fse_tables: FseTables::new(),
                offset_hist: [1, 4, 8],
            },
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
    }
}

impl<R: Read, W: Write, M: Matcher> FrameCompressor<R, W, M> {
    /// Create a new `FrameCompressor` with a custom matching algorithm implementation
    pub fn new_with_matcher(matcher: M, compression_level: CompressionLevel) -> Self {
        Self {
            uncompressed_data: None,
            compressed_data: None,
            dictionary: None,
            dictionary_entropy_cache: None,
            state: CompressState {
                matcher,
                last_huff_table: None,
                fse_tables: FseTables::new(),
                offset_hist: [1, 4, 8],
            },
            compression_level,
            #[cfg(feature = "hash")]
            hasher: XxHash64::with_seed(0),
        }
    }

    /// Before calling [FrameCompressor::compress] you need to set the source.
    ///
    /// This is the data that is compressed and written into the drain.
    pub fn set_source(&mut self, uncompressed_data: R) -> Option<R> {
        self.uncompressed_data.replace(uncompressed_data)
    }

    /// Before calling [FrameCompressor::compress] you need to set the drain.
    ///
    /// As the compressor compresses data, the drain serves as a place for the output to be writte.
    pub fn set_drain(&mut self, compressed_data: W) -> Option<W> {
        self.compressed_data.replace(compressed_data)
    }

    /// Compress the uncompressed data from the provided source as one Zstd frame and write it to the provided drain
    ///
    /// This will repeatedly call [Read::read] on the source to fill up blocks until the source returns 0 on the read call.
    /// Also [Write::write_all] will be called on the drain after each block has been encoded.
    ///
    /// To avoid endlessly encoding from a potentially endless source (like a network socket) you can use the
    /// [Read::take] function
    pub fn compress(&mut self) {
        // Clearing buffers to allow re-using of the compressor
        self.state.matcher.reset(self.compression_level);
        self.state.offset_hist = [1, 4, 8];
        let use_dictionary_state =
            !matches!(self.compression_level, CompressionLevel::Uncompressed)
                && self.state.matcher.supports_dictionary_priming();
        let cached_entropy = if use_dictionary_state {
            self.dictionary_entropy_cache.as_ref()
        } else {
            None
        };
        if use_dictionary_state && let Some(dict) = self.dictionary.as_ref() {
            // This state drives sequence encoding, while matcher priming below updates
            // the match generator's internal repeat-offset history for match finding.
            self.state.offset_hist = dict.offset_hist;
            self.state
                .matcher
                .prime_with_dictionary(dict.dict_content.as_slice(), dict.offset_hist);
        }
        if let Some(cache) = cached_entropy {
            self.state.last_huff_table.clone_from(&cache.huff);
        } else {
            self.state.last_huff_table = None;
        }
        // `clone_from` keeps frame-to-frame seeding cheap for reused compressors by
        // reusing existing allocations where possible instead of reallocating every frame.
        if let Some(cache) = cached_entropy {
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
        } else {
            self.state.fse_tables.ll_previous = None;
            self.state.fse_tables.ml_previous = None;
            self.state.fse_tables.of_previous = None;
        }
        #[cfg(feature = "hash")]
        {
            self.hasher = XxHash64::with_seed(0);
        }
        let source = self.uncompressed_data.as_mut().unwrap();
        let drain = self.compressed_data.as_mut().unwrap();
        // As the frame is compressed, it's stored here
        let output: &mut Vec<u8> = &mut Vec::with_capacity(1024 * 130);
        // First write the frame header
        let header = FrameHeader {
            frame_content_size: None,
            single_segment: false,
            content_checksum: cfg!(feature = "hash"),
            dictionary_id: if use_dictionary_state {
                self.dictionary.as_ref().map(|dict| dict.id as u64)
            } else {
                None
            },
            window_size: Some(self.state.matcher.window_size()),
        };
        header.serialize(output);
        // Now compress block by block
        loop {
            // Read a single block's worth of uncompressed data from the input
            let mut uncompressed_data = self.state.matcher.get_next_space();
            let mut read_bytes = 0;
            let last_block;
            'read_loop: loop {
                let new_bytes = source.read(&mut uncompressed_data[read_bytes..]).unwrap();
                if new_bytes == 0 {
                    last_block = true;
                    break 'read_loop;
                }
                read_bytes += new_bytes;
                if read_bytes == uncompressed_data.len() {
                    last_block = false;
                    break 'read_loop;
                }
            }
            uncompressed_data.resize(read_bytes, 0);
            // As we read, hash that data too
            #[cfg(feature = "hash")]
            self.hasher.write(&uncompressed_data);
            // Special handling is needed for compression of a totally empty file (why you'd want to do that, I don't know)
            if uncompressed_data.is_empty() {
                let header = BlockHeader {
                    last_block: true,
                    block_type: crate::blocks::block::BlockType::Raw,
                    block_size: 0,
                };
                // Write the header, then the block
                header.serialize(output);
                drain.write_all(output).unwrap();
                output.clear();
                break;
            }

            match self.compression_level {
                CompressionLevel::Uncompressed => {
                    let header = BlockHeader {
                        last_block,
                        block_type: crate::blocks::block::BlockType::Raw,
                        block_size: read_bytes.try_into().unwrap(),
                    };
                    // Write the header, then the block
                    header.serialize(output);
                    output.extend_from_slice(&uncompressed_data);
                }
                CompressionLevel::Fastest
                | CompressionLevel::Default
                | CompressionLevel::Better => {
                    // All compressed levels share this block-encoding pipeline;
                    // they differ only in the matcher backend and its parameters.
                    compress_block_encoded(&mut self.state, last_block, uncompressed_data, output)
                }
                _ => {
                    unimplemented!();
                }
            }
            drain.write_all(output).unwrap();
            output.clear();
            if last_block {
                break;
            }
        }

        // If the `hash` feature is enabled, then `content_checksum` is set to true in the header
        // and a 32 bit hash is written at the end of the data.
        #[cfg(feature = "hash")]
        {
            // Because we only have the data as a reader, we need to read all of it to calculate the checksum
            // Possible TODO: create a wrapper around self.uncompressed data that hashes the data as it's read?
            let content_checksum = self.hasher.finish();
            drain
                .write_all(&(content_checksum as u32).to_le_bytes())
                .unwrap();
        }
    }

    /// Get a mutable reference to the source
    pub fn source_mut(&mut self) -> Option<&mut R> {
        self.uncompressed_data.as_mut()
    }

    /// Get a mutable reference to the drain
    pub fn drain_mut(&mut self) -> Option<&mut W> {
        self.compressed_data.as_mut()
    }

    /// Get a reference to the source
    pub fn source(&self) -> Option<&R> {
        self.uncompressed_data.as_ref()
    }

    /// Get a reference to the drain
    pub fn drain(&self) -> Option<&W> {
        self.compressed_data.as_ref()
    }

    /// Retrieve the source
    pub fn take_source(&mut self) -> Option<R> {
        self.uncompressed_data.take()
    }

    /// Retrieve the drain
    pub fn take_drain(&mut self) -> Option<W> {
        self.compressed_data.take()
    }

    /// Before calling [FrameCompressor::compress] you can replace the matcher
    pub fn replace_matcher(&mut self, mut match_generator: M) -> M {
        core::mem::swap(&mut match_generator, &mut self.state.matcher);
        match_generator
    }

    /// Before calling [FrameCompressor::compress] you can replace the compression level
    pub fn set_compression_level(
        &mut self,
        compression_level: CompressionLevel,
    ) -> CompressionLevel {
        let old = self.compression_level;
        self.compression_level = compression_level;
        old
    }

    /// Get the current compression level
    pub fn compression_level(&self) -> CompressionLevel {
        self.compression_level
    }

    /// Attach a pre-parsed dictionary to be used for subsequent compressions.
    ///
    /// In compressed modes, the dictionary id is written only when the active
    /// matcher supports dictionary priming.
    /// Uncompressed mode and non-priming matchers ignore the attached dictionary
    /// at encode time.
    pub fn set_dictionary(
        &mut self,
        dictionary: crate::decoding::Dictionary,
    ) -> Result<Option<crate::decoding::Dictionary>, crate::decoding::errors::DictionaryDecodeError>
    {
        if dictionary.id == 0 {
            return Err(crate::decoding::errors::DictionaryDecodeError::ZeroDictionaryId);
        }
        if let Some(index) = dictionary.offset_hist.iter().position(|&rep| rep == 0) {
            return Err(
                crate::decoding::errors::DictionaryDecodeError::ZeroRepeatOffsetInDictionary {
                    index: index as u8,
                },
            );
        }
        self.dictionary_entropy_cache = Some(CachedDictionaryEntropy {
            huff: dictionary.huf.table.to_encoder_table(),
            ll_previous: dictionary
                .fse
                .literal_lengths
                .to_encoder_table()
                .map(|table| PreviousFseTable::Custom(Box::new(table))),
            ml_previous: dictionary
                .fse
                .match_lengths
                .to_encoder_table()
                .map(|table| PreviousFseTable::Custom(Box::new(table))),
            of_previous: dictionary
                .fse
                .offsets
                .to_encoder_table()
                .map(|table| PreviousFseTable::Custom(Box::new(table))),
        });
        Ok(self.dictionary.replace(dictionary))
    }

    /// Parse and attach a serialized dictionary blob.
    pub fn set_dictionary_from_bytes(
        &mut self,
        raw_dictionary: &[u8],
    ) -> Result<Option<crate::decoding::Dictionary>, crate::decoding::errors::DictionaryDecodeError>
    {
        let dictionary = crate::decoding::Dictionary::decode_dict(raw_dictionary)?;
        self.set_dictionary(dictionary)
    }

    /// Remove the attached dictionary.
    pub fn clear_dictionary(&mut self) -> Option<crate::decoding::Dictionary> {
        self.dictionary_entropy_cache = None;
        self.dictionary.take()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(feature = "dict_builder", feature = "std"))]
    use alloc::format;
    use alloc::vec;

    use super::FrameCompressor;
    use crate::common::MAGIC_NUM;
    use crate::decoding::FrameDecoder;
    use crate::encoding::{Matcher, Sequence};
    use alloc::vec::Vec;

    struct NoDictionaryMatcher {
        last_space: Vec<u8>,
        window_size: u64,
    }

    impl NoDictionaryMatcher {
        fn new(window_size: u64) -> Self {
            Self {
                last_space: Vec::new(),
                window_size,
            }
        }
    }

    impl Matcher for NoDictionaryMatcher {
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

        fn reset(&mut self, _level: super::CompressionLevel) {
            self.last_space.clear();
        }

        fn window_size(&self) -> u64 {
            self.window_size
        }
    }

    #[test]
    fn frame_starts_with_magic_num() {
        let mock_data = [1_u8, 2, 3].as_slice();
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data);
        compressor.set_drain(&mut output);

        compressor.compress();
        assert!(output.starts_with(&MAGIC_NUM.to_le_bytes()));
    }

    #[test]
    fn very_simple_raw_compress() {
        let mock_data = [1_u8, 2, 3].as_slice();
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data);
        compressor.set_drain(&mut output);

        compressor.compress();
    }

    #[test]
    fn very_simple_compress() {
        let mut mock_data = vec![0; 1 << 17];
        mock_data.extend(vec![1; (1 << 17) - 1]);
        mock_data.extend(vec![2; (1 << 18) - 1]);
        mock_data.extend(vec![2; 1 << 17]);
        mock_data.extend(vec![3; (1 << 17) - 1]);
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data.as_slice());
        compressor.set_drain(&mut output);

        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(mock_data.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(output.as_slice(), &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);
    }

    #[test]
    fn rle_compress() {
        let mock_data = vec![0; 1 << 19];
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data.as_slice());
        compressor.set_drain(&mut output);

        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(mock_data.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);
    }

    #[test]
    fn aaa_compress() {
        let mock_data = vec![0, 1, 3, 4, 5];
        let mut output: Vec<u8> = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor.set_source(mock_data.as_slice());
        compressor.set_drain(&mut output);

        compressor.compress();

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(mock_data.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);

        let mut decoded = Vec::new();
        zstd::stream::copy_decode(output.as_slice(), &mut decoded).unwrap();
        assert_eq!(mock_data, decoded);
    }

    #[test]
    fn dictionary_compression_sets_required_dict_id_and_roundtrips() {
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let dict_for_encoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
        let dict_for_decoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();

        let mut data = Vec::new();
        for _ in 0..8 {
            data.extend_from_slice(&dict_for_decoder.dict_content[..2048]);
        }

        let mut with_dict = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        let previous = compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");
        assert!(
            previous.is_none(),
            "first dictionary insert should return None"
        );
        assert_eq!(
            compressor
                .set_dictionary(dict_for_encoder)
                .expect("valid dictionary should attach")
                .expect("set_dictionary_from_bytes inserted previous dictionary")
                .id,
            dict_for_decoder.id
        );
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut with_dict);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
            .expect("encoded stream should have a frame header");
        assert_eq!(frame_header.dictionary_id(), Some(dict_for_decoder.id));

        let mut decoder = FrameDecoder::new();
        let mut missing_dict_target = Vec::with_capacity(data.len());
        let err = decoder
            .decode_all_to_vec(&with_dict, &mut missing_dict_target)
            .unwrap_err();
        assert!(
            matches!(
                &err,
                crate::decoding::errors::FrameDecoderError::DictNotProvided { .. }
            ),
            "dict-compressed stream should require dictionary id, got: {err:?}"
        );

        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(data.len());
        decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
        assert_eq!(decoded, data);

        let mut ffi_decoder = zstd::bulk::Decompressor::with_dictionary(dict_raw).unwrap();
        let mut ffi_decoded = Vec::with_capacity(data.len());
        let ffi_written = ffi_decoder
            .decompress_to_buffer(with_dict.as_slice(), &mut ffi_decoded)
            .unwrap();
        assert_eq!(ffi_written, data.len());
        assert_eq!(ffi_decoded, data);
    }

    #[cfg(all(feature = "dict_builder", feature = "std"))]
    #[test]
    fn dictionary_compression_roundtrips_with_dict_builder_dictionary() {
        use std::io::Cursor;

        let mut training = Vec::new();
        for idx in 0..256u32 {
            training.extend_from_slice(
                format!("tenant=demo table=orders key={idx} region=eu\n").as_bytes(),
            );
        }
        let mut raw_dict = Vec::new();
        crate::dictionary::create_raw_dict_from_source(
            Cursor::new(training.as_slice()),
            training.len(),
            &mut raw_dict,
            4096,
        );
        assert!(
            !raw_dict.is_empty(),
            "dict_builder produced an empty dictionary"
        );

        let dict_id = 0xD1C7_0008;
        let encoder_dict =
            crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();
        let decoder_dict =
            crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();

        let mut payload = Vec::new();
        for idx in 0..96u32 {
            payload.extend_from_slice(
                format!(
                    "tenant=demo table=orders op=put key={idx} value=aaaaabbbbbcccccdddddeeeee\n"
                )
                .as_bytes(),
            );
        }

        let mut without_dict = Vec::new();
        let mut baseline = FrameCompressor::new(super::CompressionLevel::Fastest);
        baseline.set_source(payload.as_slice());
        baseline.set_drain(&mut without_dict);
        baseline.compress();

        let mut with_dict = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(encoder_dict)
            .expect("valid dict_builder dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut with_dict);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
            .expect("encoded stream should have a frame header");
        assert_eq!(frame_header.dictionary_id(), Some(dict_id));
        let mut decoder = FrameDecoder::new();
        decoder.add_dict(decoder_dict).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
        assert!(
            with_dict.len() < without_dict.len(),
            "trained dictionary should improve compression for this small payload"
        );
    }

    #[test]
    fn set_dictionary_from_bytes_seeds_entropy_tables_for_first_block() {
        let dict_raw = include_bytes!("../../dict_tests/dictionary");
        let mut output = Vec::new();
        let input = b"";

        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        let previous = compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");
        assert!(previous.is_none());

        compressor.set_source(input.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        assert!(
            compressor.state.last_huff_table.is_some(),
            "dictionary entropy should seed previous huffman table before first block"
        );
        assert!(
            compressor.state.fse_tables.ll_previous.is_some(),
            "dictionary entropy should seed previous ll table before first block"
        );
        assert!(
            compressor.state.fse_tables.ml_previous.is_some(),
            "dictionary entropy should seed previous ml table before first block"
        );
        assert!(
            compressor.state.fse_tables.of_previous.is_some(),
            "dictionary entropy should seed previous of table before first block"
        );
    }

    #[test]
    fn set_dictionary_rejects_zero_dictionary_id() {
        let invalid = crate::decoding::Dictionary {
            id: 0,
            fse: crate::decoding::scratch::FSEScratch::new(),
            huf: crate::decoding::scratch::HuffmanScratch::new(),
            dict_content: vec![1, 2, 3],
            offset_hist: [1, 4, 8],
        };

        let mut compressor: FrameCompressor<
            &[u8],
            Vec<u8>,
            crate::encoding::match_generator::MatchGeneratorDriver,
        > = FrameCompressor::new(super::CompressionLevel::Fastest);
        let result = compressor.set_dictionary(invalid);
        assert!(matches!(
            result,
            Err(crate::decoding::errors::DictionaryDecodeError::ZeroDictionaryId)
        ));
    }

    #[test]
    fn set_dictionary_rejects_zero_repeat_offsets() {
        let invalid = crate::decoding::Dictionary {
            id: 1,
            fse: crate::decoding::scratch::FSEScratch::new(),
            huf: crate::decoding::scratch::HuffmanScratch::new(),
            dict_content: vec![1, 2, 3],
            offset_hist: [0, 4, 8],
        };

        let mut compressor: FrameCompressor<
            &[u8],
            Vec<u8>,
            crate::encoding::match_generator::MatchGeneratorDriver,
        > = FrameCompressor::new(super::CompressionLevel::Fastest);
        let result = compressor.set_dictionary(invalid);
        assert!(matches!(
            result,
            Err(
                crate::decoding::errors::DictionaryDecodeError::ZeroRepeatOffsetInDictionary {
                    index: 0
                }
            )
        ));
    }

    #[test]
    fn uncompressed_mode_does_not_require_dictionary() {
        let dict_id = 0xABCD_0001;
        let dict =
            crate::decoding::Dictionary::from_raw_content(dict_id, b"shared-history".to_vec())
                .expect("raw dictionary should be valid");

        let payload = b"plain-bytes-that-should-stay-raw";
        let mut output = Vec::new();
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
        compressor
            .set_dictionary(dict)
            .expect("dictionary should attach in uncompressed mode");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
            .expect("encoded frame should have a header");
        assert_eq!(
            frame_header.dictionary_id(),
            None,
            "raw/uncompressed frames must not advertise dictionary dependency"
        );

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn dictionary_roundtrip_stays_valid_after_output_exceeds_window() {
        use crate::encoding::match_generator::MatchGeneratorDriver;

        let dict_id = 0xABCD_0002;
        let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
            .expect("raw dictionary should be valid");
        let dict_for_decoder =
            crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
                .expect("raw dictionary should be valid");

        let payload = b"abcdefgh".repeat(512);
        let matcher = MatchGeneratorDriver::new(8, 1);

        let mut no_dict_output = Vec::new();
        let mut no_dict_compressor =
            FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
        no_dict_compressor.set_source(payload.as_slice());
        no_dict_compressor.set_drain(&mut no_dict_output);
        no_dict_compressor.compress();
        let (no_dict_frame_header, _) =
            crate::decoding::frame::read_frame_header(no_dict_output.as_slice())
                .expect("baseline frame should have a header");
        let no_dict_window = no_dict_frame_header
            .window_size()
            .expect("window size should be present");

        let mut output = Vec::new();
        let matcher = MatchGeneratorDriver::new(8, 1);
        let mut compressor =
            FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(dict)
            .expect("dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
            .expect("encoded frame should have a header");
        let advertised_window = frame_header
            .window_size()
            .expect("window size should be present");
        assert_eq!(
            advertised_window, no_dict_window,
            "dictionary priming must not inflate advertised window size"
        );
        assert!(
            payload.len() > advertised_window as usize,
            "test must cross the advertised window boundary"
        );

        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn custom_matcher_without_dictionary_priming_does_not_advertise_dict_id() {
        let dict_id = 0xABCD_0003;
        let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
            .expect("raw dictionary should be valid");
        let payload = b"abcdefghabcdefgh";

        let mut output = Vec::new();
        let matcher = NoDictionaryMatcher::new(64);
        let mut compressor =
            FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(dict)
            .expect("dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut output);
        compressor.compress();

        let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
            .expect("encoded frame should have a header");
        assert_eq!(
            frame_header.dictionary_id(),
            None,
            "matchers that do not support dictionary priming must not advertise dictionary dependency"
        );

        let mut decoder = FrameDecoder::new();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[cfg(feature = "hash")]
    #[test]
    fn checksum_two_frames_reused_compressor() {
        // Compress the same data twice using the same compressor and verify that:
        // 1. The checksum written in each frame matches what the decoder calculates.
        // 2. The hasher is correctly reset between frames (no cross-contamination).
        //    If the hasher were NOT reset, the second frame's calculated checksum
        //    would differ from the one stored in the frame data, causing assert_eq to fail.
        let data: Vec<u8> = (0u8..=255).cycle().take(1024).collect();

        let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);

        // --- Frame 1 ---
        let mut compressed1 = Vec::new();
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed1);
        compressor.compress();

        // --- Frame 2 (reuse the same compressor) ---
        let mut compressed2 = Vec::new();
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed2);
        compressor.compress();

        fn decode_and_collect(compressed: &[u8]) -> (Vec<u8>, Option<u32>, Option<u32>) {
            let mut decoder = FrameDecoder::new();
            let mut source = compressed;
            decoder.reset(&mut source).unwrap();
            while !decoder.is_finished() {
                decoder
                    .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                    .unwrap();
            }
            let mut decoded = Vec::new();
            decoder.collect_to_writer(&mut decoded).unwrap();
            (
                decoded,
                decoder.get_checksum_from_data(),
                decoder.get_calculated_checksum(),
            )
        }

        let (decoded1, chksum_from_data1, chksum_calculated1) = decode_and_collect(&compressed1);
        assert_eq!(decoded1, data, "frame 1: decoded data mismatch");
        assert_eq!(
            chksum_from_data1, chksum_calculated1,
            "frame 1: checksum mismatch"
        );

        let (decoded2, chksum_from_data2, chksum_calculated2) = decode_and_collect(&compressed2);
        assert_eq!(decoded2, data, "frame 2: decoded data mismatch");
        assert_eq!(
            chksum_from_data2, chksum_calculated2,
            "frame 2: checksum mismatch"
        );

        // Same data compressed twice must produce the same checksum.
        // If state leaked across frames, the second calculated checksum would differ.
        assert_eq!(
            chksum_from_data1, chksum_from_data2,
            "frame 1 and frame 2 should have the same checksum (same data, hash must reset per frame)"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn fuzz_targets() {
        use std::io::Read;
        fn decode_szstd(data: &mut dyn std::io::Read) -> Vec<u8> {
            let mut decoder = crate::decoding::StreamingDecoder::new(data).unwrap();
            let mut result: Vec<u8> = Vec::new();
            decoder.read_to_end(&mut result).expect("Decoding failed");
            result
        }

        fn decode_szstd_writer(mut data: impl Read) -> Vec<u8> {
            let mut decoder = crate::decoding::FrameDecoder::new();
            decoder.reset(&mut data).unwrap();
            let mut result = vec![];
            while !decoder.is_finished() || decoder.can_collect() > 0 {
                decoder
                    .decode_blocks(
                        &mut data,
                        crate::decoding::BlockDecodingStrategy::UptoBytes(1024 * 1024),
                    )
                    .unwrap();
                decoder.collect_to_writer(&mut result).unwrap();
            }
            result
        }

        fn encode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
            zstd::stream::encode_all(std::io::Cursor::new(data), 3)
        }

        fn encode_szstd_uncompressed(data: &mut dyn std::io::Read) -> Vec<u8> {
            let mut input = Vec::new();
            data.read_to_end(&mut input).unwrap();

            crate::encoding::compress_to_vec(
                input.as_slice(),
                crate::encoding::CompressionLevel::Uncompressed,
            )
        }

        fn encode_szstd_compressed(data: &mut dyn std::io::Read) -> Vec<u8> {
            let mut input = Vec::new();
            data.read_to_end(&mut input).unwrap();

            crate::encoding::compress_to_vec(
                input.as_slice(),
                crate::encoding::CompressionLevel::Fastest,
            )
        }

        fn decode_zstd(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
            let mut output = Vec::new();
            zstd::stream::copy_decode(data, &mut output)?;
            Ok(output)
        }
        if std::fs::exists("fuzz/artifacts/interop").unwrap_or(false) {
            for file in std::fs::read_dir("fuzz/artifacts/interop").unwrap() {
                if file.as_ref().unwrap().file_type().unwrap().is_file() {
                    let data = std::fs::read(file.unwrap().path()).unwrap();
                    let data = data.as_slice();
                    // Decoding
                    let compressed = encode_zstd(data).unwrap();
                    let decoded = decode_szstd(&mut compressed.as_slice());
                    let decoded2 = decode_szstd_writer(&mut compressed.as_slice());
                    assert!(
                        decoded == data,
                        "Decoded data did not match the original input during decompression"
                    );
                    assert_eq!(
                        decoded2, data,
                        "Decoded data did not match the original input during decompression"
                    );

                    // Encoding
                    // Uncompressed encoding
                    let mut input = data;
                    let compressed = encode_szstd_uncompressed(&mut input);
                    let decoded = decode_zstd(&compressed).unwrap();
                    assert_eq!(
                        decoded, data,
                        "Decoded data did not match the original input during compression"
                    );
                    // Compressed encoding
                    let mut input = data;
                    let compressed = encode_szstd_compressed(&mut input);
                    let decoded = decode_zstd(&compressed).unwrap();
                    assert_eq!(
                        decoded, data,
                        "Decoded data did not match the original input during compression"
                    );
                }
            }
        }
    }
}
