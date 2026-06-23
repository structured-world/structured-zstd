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
    let mut raw = include_bytes!("../../../dict_tests/dictionary").to_vec();
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
    let dict_ref: &Dictionary = handle.as_ref();

    assert_eq!(dict_ref.id, 7);
    assert_eq!(dict_ref.dict_content.as_slice(), &[42]);
}

#[test]
fn dictionary_handle_clones_share_inner() {
    let raw = include_bytes!("../../../dict_tests/dictionary");
    let handle = DictionaryHandle::decode_dict(raw).expect("dictionary should parse");
    let clone = handle.clone();

    assert_eq!(handle.id(), clone.id());
    assert!(SharedDictionary::ptr_eq(&handle.inner, &clone.inner));
}
