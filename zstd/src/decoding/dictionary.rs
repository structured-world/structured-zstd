#[cfg(not(target_has_atomic = "ptr"))]
use alloc::rc::Rc;
#[cfg(target_has_atomic = "ptr")]
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryInto;

use crate::decoding::errors::DictionaryDecodeError;
use crate::decoding::scratch::FSEScratch;
use crate::decoding::scratch::HuffmanScratch;

/// Zstandard includes support for "raw content" dictionaries, that store bytes optionally used
/// during sequence execution.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#dictionary-format>
pub struct Dictionary {
    /// A 4 byte value used by decoders to check if they can use
    /// the correct dictionary. This value must not be zero.
    pub id: u32,
    /// A dictionary can contain an entropy table, either FSE or
    /// Huffman.
    pub fse: FSEScratch,
    /// A dictionary can contain an entropy table, either FSE or
    /// Huffman.
    pub huf: HuffmanScratch,
    /// The content of a dictionary acts as a "past" in front of data
    /// to compress or decompress,
    /// so it can be referenced in sequence commands.
    /// As long as the amount of data decoded from this frame is less than or
    /// equal to Window_Size, sequence commands may specify offsets longer than
    /// the total length of decoded output so far to reference back to the
    /// dictionary, even parts of the dictionary with offsets larger than Window_Size.
    /// After the total output has surpassed Window_Size however,
    /// this is no longer allowed and the dictionary is no longer accessible
    pub dict_content: Vec<u8>,
    /// The 3 most recent offsets are stored so that they can be used
    /// during sequence execution, see
    /// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#repeat-offsets>
    /// for more.
    pub offset_hist: [u32; 3],
}

#[cfg(target_has_atomic = "ptr")]
type SharedDictionary = Arc<Dictionary>;
#[cfg(not(target_has_atomic = "ptr"))]
type SharedDictionary = Rc<Dictionary>;

/// Shared pre-parsed dictionary handle for repeated decoding.
///
/// Uses `Arc` on targets with atomics and falls back to `Rc` otherwise.
#[derive(Clone)]
pub struct DictionaryHandle {
    inner: SharedDictionary,
}

/// This 4 byte (little endian) magic number refers to the start of a dictionary
pub const MAGIC_NUM: [u8; 4] = [0x37, 0xA4, 0x30, 0xEC];

impl Dictionary {
    /// Build a dictionary from raw content bytes (without entropy table sections).
    ///
    /// This is primarily intended for dictionaries produced by the `dict_builder`
    /// module, which currently emits raw-content dictionaries.
    pub fn from_raw_content(
        id: u32,
        dict_content: Vec<u8>,
    ) -> Result<Dictionary, DictionaryDecodeError> {
        if id == 0 {
            return Err(DictionaryDecodeError::ZeroDictionaryId);
        }
        if dict_content.is_empty() {
            return Err(DictionaryDecodeError::DictionaryTooSmall { got: 0, need: 1 });
        }

        Ok(Dictionary {
            id,
            fse: FSEScratch::new(),
            huf: HuffmanScratch::new(),
            dict_content,
            offset_hist: [1, 4, 8],
        })
    }

    /// Parses the dictionary from `raw`, initializes its tables,
    /// and returns a fully constructed [`Dictionary`] whose `id` can be
    /// checked against the frame's `dict_id`.
    pub fn decode_dict(raw: &[u8]) -> Result<Dictionary, DictionaryDecodeError> {
        const MIN_MAGIC_AND_ID_LEN: usize = 8;
        const OFFSET_HISTORY_LEN: usize = 12;

        if raw.len() < MIN_MAGIC_AND_ID_LEN {
            return Err(DictionaryDecodeError::DictionaryTooSmall {
                got: raw.len(),
                need: MIN_MAGIC_AND_ID_LEN,
            });
        }

        let mut new_dict = Dictionary {
            id: 0,
            fse: FSEScratch::new(),
            huf: HuffmanScratch::new(),
            dict_content: Vec::new(),
            offset_hist: [1, 4, 8],
        };

        let magic_num: [u8; 4] = raw[..4].try_into().expect("optimized away");
        if magic_num != MAGIC_NUM {
            return Err(DictionaryDecodeError::BadMagicNum { got: magic_num });
        }

        let dict_id = raw[4..8].try_into().expect("optimized away");
        let dict_id = u32::from_le_bytes(dict_id);
        if dict_id == 0 {
            return Err(DictionaryDecodeError::ZeroDictionaryId);
        }
        new_dict.id = dict_id;

        let raw_tables = &raw[8..];

        let huf_size = new_dict.huf.table.build_decoder(raw_tables)?;
        let raw_tables = &raw_tables[huf_size as usize..];

        let of_size = new_dict.fse.offsets.build_decoder(
            raw_tables,
            crate::decoding::sequence_section_decoder::OF_MAX_LOG,
        )?;
        let raw_tables = &raw_tables[of_size..];

        let ml_size = new_dict.fse.match_lengths.build_decoder(
            raw_tables,
            crate::decoding::sequence_section_decoder::ML_MAX_LOG,
        )?;
        let raw_tables = &raw_tables[ml_size..];

        let ll_size = new_dict.fse.literal_lengths.build_decoder(
            raw_tables,
            crate::decoding::sequence_section_decoder::LL_MAX_LOG,
        )?;
        let raw_tables = &raw_tables[ll_size..];

        if raw_tables.len() < OFFSET_HISTORY_LEN {
            return Err(DictionaryDecodeError::DictionaryTooSmall {
                got: raw_tables.len(),
                need: OFFSET_HISTORY_LEN,
            });
        }

        let offset1 = raw_tables[0..4].try_into().expect("optimized away");
        let offset1 = u32::from_le_bytes(offset1);

        let offset2 = raw_tables[4..8].try_into().expect("optimized away");
        let offset2 = u32::from_le_bytes(offset2);

        let offset3 = raw_tables[8..12].try_into().expect("optimized away");
        let offset3 = u32::from_le_bytes(offset3);

        if offset1 == 0 {
            return Err(DictionaryDecodeError::ZeroRepeatOffsetInDictionary { index: 0 });
        }
        if offset2 == 0 {
            return Err(DictionaryDecodeError::ZeroRepeatOffsetInDictionary { index: 1 });
        }
        if offset3 == 0 {
            return Err(DictionaryDecodeError::ZeroRepeatOffsetInDictionary { index: 2 });
        }

        new_dict.offset_hist[0] = offset1;
        new_dict.offset_hist[1] = offset2;
        new_dict.offset_hist[2] = offset3;

        let raw_content = &raw_tables[12..];
        new_dict.dict_content.extend(raw_content);

        Ok(new_dict)
    }

    /// Convert this parsed dictionary into a reusable shared handle.
    pub fn into_handle(self) -> DictionaryHandle {
        DictionaryHandle::from_dictionary(self)
    }
}

impl DictionaryHandle {
    /// Wrap an already-parsed dictionary in a shared handle.
    pub fn from_dictionary(dict: Dictionary) -> Self {
        Self {
            inner: SharedDictionary::new(dict),
        }
    }

    /// Parse a serialized dictionary and return a reusable shared handle.
    pub fn decode_dict(raw: &[u8]) -> Result<Self, DictionaryDecodeError> {
        Dictionary::decode_dict(raw).map(Self::from_dictionary)
    }

    pub fn id(&self) -> u32 {
        self.inner.id
    }

    pub fn as_dict(&self) -> &Dictionary {
        &self.inner
    }
}

impl AsRef<Dictionary> for DictionaryHandle {
    fn as_ref(&self) -> &Dictionary {
        self.as_dict()
    }
}

impl From<Dictionary> for DictionaryHandle {
    fn from(dict: Dictionary) -> Self {
        DictionaryHandle::from_dictionary(dict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn offset_history_start(raw: &[u8]) -> usize {
        let mut huf = crate::decoding::scratch::HuffmanScratch::new();
        let mut fse = crate::decoding::scratch::FSEScratch::new();
        let mut cursor = 8usize;

        let huf_size = huf
            .table
            .build_decoder(&raw[cursor..])
            .expect("reference dictionary huffman table should decode");
        cursor += huf_size as usize;

        let of_size = fse
            .offsets
            .build_decoder(
                &raw[cursor..],
                crate::decoding::sequence_section_decoder::OF_MAX_LOG,
            )
            .expect("reference dictionary OF table should decode");
        cursor += of_size;

        let ml_size = fse
            .match_lengths
            .build_decoder(
                &raw[cursor..],
                crate::decoding::sequence_section_decoder::ML_MAX_LOG,
            )
            .expect("reference dictionary ML table should decode");
        cursor += ml_size;

        let ll_size = fse
            .literal_lengths
            .build_decoder(
                &raw[cursor..],
                crate::decoding::sequence_section_decoder::LL_MAX_LOG,
            )
            .expect("reference dictionary LL table should decode");
        cursor += ll_size;

        cursor
    }

    #[test]
    fn decode_dict_rejects_short_buffer_before_magic_and_id() {
        let err = match Dictionary::decode_dict(&[]) {
            Ok(_) => panic!("expected short dictionary to fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            DictionaryDecodeError::DictionaryTooSmall { got: 0, need: 8 }
        ));
    }

    #[test]
    fn decode_dict_malformed_input_returns_error_instead_of_panicking() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&MAGIC_NUM);
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&[0u8; 7]);

        let result = std::panic::catch_unwind(|| Dictionary::decode_dict(&raw));
        assert!(
            result.is_ok(),
            "decode_dict must not panic on malformed input"
        );
        assert!(
            result.unwrap().is_err(),
            "malformed dictionary must return error"
        );
    }

    #[test]
    fn decode_dict_rejects_zero_repeat_offsets() {
        let mut raw = include_bytes!("../../dict_tests/dictionary").to_vec();
        let offset_start = offset_history_start(&raw);

        // Corrupt rep0 to zero.
        raw[offset_start..offset_start + 4].copy_from_slice(&0u32.to_le_bytes());
        let decoded = Dictionary::decode_dict(&raw);
        assert!(matches!(
            decoded,
            Err(DictionaryDecodeError::ZeroRepeatOffsetInDictionary { index: 0 })
        ));
    }

    #[test]
    fn from_raw_content_rejects_empty_dictionary_content() {
        let result = Dictionary::from_raw_content(1, Vec::new());
        assert!(matches!(
            result,
            Err(DictionaryDecodeError::DictionaryTooSmall { got: 0, need: 1 })
        ));
    }

    #[test]
    fn dictionary_handle_from_raw_content_supports_as_ref() {
        let dict = Dictionary::from_raw_content(7, vec![42]).expect("raw dict should build");
        let handle = dict.into_handle();

        assert_eq!(handle.as_ref().id, 7);
        assert_eq!(handle.as_ref().dict_content, vec![42]);
    }

    #[test]
    fn dictionary_handle_clones_share_inner() {
        let raw = include_bytes!("../../dict_tests/dictionary");
        let handle = DictionaryHandle::decode_dict(raw).expect("dictionary should parse");
        let clone = handle.clone();

        assert_eq!(handle.id(), clone.id());
        assert!(SharedDictionary::ptr_eq(&handle.inner, &clone.inner));
    }
}
