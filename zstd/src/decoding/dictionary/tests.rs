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

/// A repeat offset reaching past the dictionary's content names no byte a
/// frame could repeat from, and upstream refuses the dictionary for it on both
/// sides (`zstd_compress.c:5142-5146`, `zstd_decompress.c:1528-1532`). An
/// offset of exactly the content's length reaches its first byte and stays
/// valid.
#[test]
fn decode_dict_rejects_repeat_offsets_beyond_the_content() {
    let reference = include_bytes!("../../../dict_tests/dictionary").to_vec();
    let offset_start = offset_history_start(&reference);
    let content_len = reference.len() - (offset_start + 12);
    let rep2 = offset_start + 8;

    let mut raw = reference.clone();
    raw[rep2..rep2 + 4].copy_from_slice(&(content_len as u32 + 1).to_le_bytes());
    let refused = Dictionary::decode_dict(&raw).err();
    assert!(
        matches!(
            refused,
            Some(DictionaryDecodeError::RepeatOffsetBeyondContent { index: 2, offset, content_len: len })
                if offset as usize == content_len + 1 && len == content_len
        ),
        "a repeat offset past the content must be refused, got {refused:?}"
    );
    // The message names the offset and what it reaches past.
    let message = alloc::format!("{}", refused.unwrap());
    assert_eq!(
        message,
        alloc::format!(
            "Dictionary repeat offset rep2 = {} reaches past its {content_len}-byte content",
            content_len + 1
        )
    );

    let mut raw = reference;
    raw[rep2..rep2 + 4].copy_from_slice(&(content_len as u32).to_le_bytes());
    let decoded =
        Dictionary::decode_dict(&raw).expect("an offset reaching the first byte is valid");
    assert_eq!(decoded.offset_hist[2] as usize, content_len);
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

/// `ZSTD_createDDict` loads in `ZSTD_dct_auto` mode (zstd_ddict.c:102-107): a
/// buffer without the magic is raw content with no id and no entropy tables.
/// The handle is the shared form the decoder is handed, so it takes the same
/// two kinds the `Dictionary` constructor does.
#[test]
fn dictionary_handle_takes_serialized_or_raw_content() {
    let serialized = include_bytes!("../../../dict_tests/dictionary");
    let parsed = DictionaryHandle::from_serialized_or_raw_content(serialized)
        .expect("a magic-prefixed blob parses as a full dictionary");
    assert_ne!(parsed.id(), 0, "a full dictionary carries its id");

    let raw = b"tenant=demo table=orders op=put".repeat(8);
    let handle = DictionaryHandle::from_serialized_or_raw_content(&raw)
        .expect("anything else is raw content");
    assert_eq!(handle.id(), 0, "raw content has no header to carry an id");
    assert_eq!(handle.as_dict().dict_content.as_slice(), raw.as_slice());
}

/// Under eight bytes there is no room for the magic and an id, so even a
/// magic-prefixed blob is raw content, as upstream reads it
/// (zstd_ddict.c:97, zstd_decompress.c:1541); from eight bytes on the magic
/// names a serialized dictionary.
#[test]
fn a_blob_under_eight_bytes_is_raw_content() {
    assert!(!Dictionary::is_serialized(&MAGIC_NUM));
    assert!(Dictionary::is_serialized(&[
        0x37, 0xA4, 0x30, 0xEC, 1, 0, 0, 0
    ]));
    assert!(!Dictionary::is_serialized(b"raw content of any length"));
    let magic_only = Dictionary::from_serialized_or_raw_content(&MAGIC_NUM)
        .expect("too short to be serialized, so raw content");
    assert_eq!(magic_only.dict_content.as_slice(), &MAGIC_NUM[..]);
    let encoder = crate::encoding::EncoderDictionary::from_serialized_or_raw_content(&MAGIC_NUM)
        .expect("the encoder reads it the same way");
    assert_eq!(encoder.content(), &MAGIC_NUM[..]);
}

/// A zero-sized buffer is a dictionary with nothing in it, not a malformed
/// one: `ZSTD_createDDict(NULL, 0)` builds a usable `DDict` that references no
/// content (zstd_ddict.c:123-140), and a caller handed an empty file gets the
/// same nothing here rather than an error.
#[test]
fn an_empty_buffer_is_a_dictionary_with_no_content() {
    let dict = Dictionary::from_serialized_or_raw_content(&[])
        .expect("an empty buffer is a dictionary with no content");
    assert_eq!(dict.id, 0);
    assert!(dict.dict_content.is_empty());
    assert_eq!(dict.offset_hist, [1, 4, 8]);

    let handle = DictionaryHandle::from_serialized_or_raw_content(&[])
        .expect("the handle takes it the same way");
    assert_eq!(handle.id(), 0);
    assert!(handle.as_dict().dict_content.is_empty());

    // The constructor that names raw content still refuses it: there its
    // emptiness is the caller asking for a dictionary that cannot exist.
    assert!(Dictionary::from_raw_content(1, Vec::new()).is_err());
}

/// The bound callers budget a handle by leaves room, beyond the parsed tables
/// at their largest, for the bytes a serialized dictionary keeps in front of
/// its content.
#[cfg(feature = "legacy")]
#[test]
fn the_heap_bound_covers_the_kept_serialized_header() {
    let raw = include_bytes!("../../../dict_tests/dictionary");
    let parsed = DictionaryHandle::decode_dict(raw).unwrap();
    let content_len = parsed.as_dict().dict_content.len();
    let beyond_tables = DictionaryHandle::max_heap_size(content_len)
        - DictionaryHandle::shared_allocation_size()
        - Dictionary::max_heap_bytes(content_len);
    let kept = parsed.inner.serialized_header.capacity();
    assert!(kept > 0);
    assert!(beyond_tables >= kept);
}

/// The header bound adds the largest description each table can take: the
/// 4-bit log, then per symbol a count and two bits of zero-run flags.
#[cfg(feature = "legacy")]
#[test]
fn the_serialized_header_bound_sums_its_parts() {
    // 32 offset codes at log 8: 4 + 32 * 11 bits, 45 bytes.
    assert_eq!(max_fse_description_len(31, 8), 45);
    let sum = 8
        + 128
        + max_fse_description_len(31, 8)
        + max_fse_description_len(52, 9)
        + max_fse_description_len(35, 9)
        + 12;
    assert_eq!(sum, MAX_SERIALIZED_HEADER_LEN);
}

/// A handle reports the shared allocation with the dictionary, stays within
/// the bound callers budget by before parsing, and a set of handles counts a
/// dictionary once and only when nothing outside the set holds it.
#[test]
fn a_handle_accounts_for_what_it_keeps() {
    let raw = include_bytes!("../../../dict_tests/dictionary");
    let parsed = DictionaryHandle::decode_dict(raw).unwrap();
    let content_len = parsed.as_dict().dict_content.len();
    // The two counts ahead of the value are heap too.
    let shared = 2 * core::mem::size_of::<usize>() + core::mem::size_of::<Dictionary>();
    assert!(parsed.heap_size() >= shared + parsed.as_dict().heap_bytes());
    assert!(parsed.heap_size() <= DictionaryHandle::max_heap_size(content_len));
    assert!(parsed.as_dict().heap_bytes() <= Dictionary::max_heap_bytes(content_len));

    let raw_content = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(0, vec![7u8; 4096]).unwrap(),
    );
    assert!(raw_content.heap_size() <= DictionaryHandle::max_heap_size(4096));

    let copy = parsed.clone();
    // Both handles in the set, and the same one twice: counted once.
    assert_eq!(
        DictionaryHandle::exclusive_heap_size([&parsed, &copy, &parsed]),
        parsed.heap_size()
    );
    // `copy` is held outside the set: nothing is the set's alone.
    assert_eq!(DictionaryHandle::exclusive_heap_size([&parsed]), 0);
    // Two different dictionaries, each held only here.
    assert_eq!(
        DictionaryHandle::exclusive_heap_size([&copy, &raw_content, &parsed]),
        parsed.heap_size() + raw_content.heap_size()
    );
    assert_eq!(DictionaryHandle::exclusive_heap_size([]), 0);
}

#[test]
fn dictionary_handle_clones_share_inner() {
    let raw = include_bytes!("../../../dict_tests/dictionary");
    let handle = DictionaryHandle::decode_dict(raw).expect("dictionary should parse");
    let clone = handle.clone();

    assert_eq!(handle.id(), clone.id());
    assert!(SharedDictionary::ptr_eq(&handle.inner, &clone.inner));
}

/// Frames compressed with the reference dictionary, at a level of each matcher
/// family, over content the dictionary covers.
#[cfg(target_has_atomic = "ptr")]
fn frames_with_the_reference_dictionary(raw: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    let content = Dictionary::decode_dict(raw).unwrap().dict_content;
    let mut payload = Vec::new();
    for chunk in content.chunks(700).take(12) {
        payload.extend_from_slice(chunk);
        payload.extend_from_slice(b"; a few bytes of the frame's own");
    }
    [1, 3, 9, 19]
        .into_iter()
        .map(|level| {
            let mut compressor: FrameCompressor =
                FrameCompressor::new(CompressionLevel::Level(level));
            compressor.set_dictionary_from_bytes(raw).unwrap();
            (
                compressor.compress_independent_frame(&payload),
                payload.clone(),
            )
        })
        .collect()
}

/// A dictionary parsed over a shared or a referenced buffer keeps its content
/// in that buffer, owns none of it, and decodes every frame as the copying
/// parse does: this is what `ZSTD_createDDict_byReference` builds upstream.
#[cfg(target_has_atomic = "ptr")]
#[test]
fn a_dictionary_over_the_callers_buffer_decodes_without_a_copy() {
    let raw: &[u8] = include_bytes!("../../../dict_tests/dictionary");
    let shared: Arc<[u8]> = Arc::from(raw);
    let copied = DictionaryHandle::decode_dict(raw).unwrap();
    let over_shared =
        DictionaryHandle::decode_dict_bytes(DictionaryBytes::shared(shared.clone())).unwrap();
    // SAFETY: `raw` is a static and never changes.
    let over_reference =
        DictionaryHandle::decode_dict_bytes(unsafe { DictionaryBytes::referenced(raw) }).unwrap();

    let content_start = raw.len() - copied.as_dict().dict_content.len();
    assert_eq!(
        over_shared.as_dict().dict_content.as_ptr(),
        shared[content_start..].as_ptr()
    );
    assert_eq!(
        over_reference.as_dict().dict_content.as_ptr(),
        raw[content_start..].as_ptr()
    );
    for handle in [&over_shared, &over_reference] {
        assert_eq!(handle.as_dict().dict_content, copied.as_dict().dict_content);
    }
    // A referenced buffer is the caller's, so the handle reports it out, as
    // upstream's `ZSTD_sizeof_DDict` leaves a referenced buffer out.
    assert_eq!(over_reference.as_dict().dict_content.heap_bytes(), 0);
    assert_eq!(
        over_reference.heap_size() + copied.as_dict().dict_content.heap_bytes(),
        copied.heap_size()
    );

    for (frame, payload) in frames_with_the_reference_dictionary(raw) {
        for handle in [&copied, &over_shared, &over_reference] {
            let mut decoder = crate::decoding::FrameDecoder::new();
            let mut out = vec![0u8; payload.len()];
            let written = decoder
                .decode_all_with_dict_handle(&frame, &mut out, handle)
                .unwrap();
            assert_eq!(&out[..written], payload.as_slice());
        }
    }
}

/// A shared buffer is held alive by the dictionary that holds it, so its
/// allocation counts in the handle's footprint even after the caller dropped
/// its own reference; and a set of handles counts it as theirs alone only while
/// nothing outside them holds the buffer.
#[cfg(target_has_atomic = "ptr")]
#[test]
fn a_shared_buffer_counts_in_the_footprint_of_what_keeps_it_alive() {
    let raw: &[u8] = include_bytes!("../../../dict_tests/dictionary");
    let shared: Arc<[u8]> = Arc::from(raw);
    let handle =
        DictionaryHandle::decode_dict_bytes(DictionaryBytes::shared(shared.clone())).unwrap();
    let held = handle.as_dict().dict_content.heap_bytes();
    assert!(
        held > raw.len(),
        "the shared allocation is {held} bytes, the buffer {}",
        raw.len()
    );
    // The bound callers budget by before parsing still holds.
    let content_len = handle.as_dict().dict_content.len();
    assert!(handle.heap_size() <= DictionaryHandle::max_heap_size(content_len));
    assert!(handle.as_dict().heap_bytes() <= Dictionary::max_heap_bytes(content_len));

    // The caller still holds the buffer: it is not this handle's alone.
    assert_eq!(
        DictionaryHandle::exclusive_heap_size([&handle]),
        handle.heap_size() - held
    );
    // Dropped: the handle is what keeps it alive.
    drop(shared);
    assert_eq!(
        DictionaryHandle::exclusive_heap_size([&handle]),
        handle.heap_size()
    );
}

/// Raw content handed over shared or by reference stays where it is too, and
/// the empty buffer still loads as no dictionary.
#[cfg(target_has_atomic = "ptr")]
#[test]
fn raw_content_over_the_callers_buffer_is_not_copied() {
    let shared: Arc<[u8]> = Arc::from(&b"raw content a frame refers back to"[..]);
    let handle =
        DictionaryHandle::from_serialized_or_raw_bytes(DictionaryBytes::shared(shared.clone()))
            .unwrap();
    assert_eq!(handle.as_dict().dict_content.as_ptr(), shared.as_ptr());
    assert_eq!(handle.id(), 0);

    let empty = Dictionary::from_serialized_or_raw_bytes(DictionaryBytes::default()).unwrap();
    assert!(empty.dict_content.is_empty());
    assert!(Dictionary::from_raw_content(1, DictionaryBytes::shared(Arc::from(&[][..]))).is_err());
}

/// A clone of owned bytes narrowed to a tail copies the buffer and keeps the
/// same range of the copy; a shared clone points at the same bytes.
#[cfg(target_has_atomic = "ptr")]
#[test]
fn dictionary_bytes_clones_keep_their_range() {
    let owned = DictionaryBytes::owned(b"header|content".to_vec()).tail(7);
    let clone = owned.clone();
    assert_eq!(&*clone, b"content");
    assert_ne!(clone.as_ptr(), owned.as_ptr());

    let shared = DictionaryBytes::shared(Arc::from(&b"header|content"[..])).tail(7);
    let clone = shared.clone();
    assert_eq!(&*clone, b"content");
    assert_eq!(clone.as_ptr(), shared.as_ptr());
}
