use super::*;
use crate::decoding::dictionary::Dictionary;

#[test]
fn init_from_dict_marks_fse_ddict_is_cold() {
    // Upstream zstd parity: `ZSTD_decompressBegin_usingDDict` sets
    // `dctx->ddictIsCold = 1`. Mirror: `init_from_dict` must
    // leave `fse.ddict_is_cold = true` so the first
    // sequence-section decode of the frame engages the prefetch
    // pipeline regardless of `offsets_long_share`.
    extern crate std;
    let dict_raw =
        std::fs::read("./dict_tests/dictionary").expect("dictionary fixture should load");
    let dict = DictionaryHandle::from_dictionary(
        Dictionary::decode_dict(&dict_raw).expect("dictionary should parse"),
    );
    let mut scratch: DecoderScratch = DecoderScratch::new(1024);
    assert!(
        !scratch.fse.ddict_is_cold,
        "fresh DecoderScratch must not advertise a cold dict"
    );
    scratch.init_from_dict(&dict);
    assert!(
        scratch.fse.ddict_is_cold,
        "init_from_dict must set ddict_is_cold = true"
    );
}

#[test]
fn reinit_from_clears_cold_dict_flag() {
    // `reinit_from` materialises a local-only snapshot (LSM resume
    // export/restore). It detaches the dict and forces every axis to
    // Local, so it must also clear `ddict_is_cold` — otherwise a stale
    // cold-dict pipeline gate from a prior dictionary frame would be
    // carried into restored entropy state that has no dictionary.
    let mut dst = FSEScratch::new();
    dst.ddict_is_cold = true; // simulate a prior cold-dict frame's flag
    let src = FSEScratch::new(); // clean local-only source, not cold
    dst.reinit_from(&src, None);
    assert!(
        !dst.ddict_is_cold,
        "reinit_from must clear ddict_is_cold on a local-only snapshot"
    );
}

#[test]
fn init_from_dict_is_zero_copy_cow_then_reset_detaches() {
    // Copy-on-write contract: `init_from_dict` must NOT copy the
    // dictionary's sequence FSE table bytes into the per-frame local
    // scratch; the live accessor resolves straight to the shared
    // dictionary's table. `reset` must then detach so a reused
    // scratch never reads the previous frame's dictionary tables.
    extern crate std;
    let dict_raw =
        std::fs::read("./dict_tests/dictionary").expect("dictionary fixture should load");
    let dict = DictionaryHandle::from_dictionary(
        Dictionary::decode_dict(&dict_raw).expect("dictionary should parse"),
    );
    let mut scratch: DecoderScratch = DecoderScratch::new(1024);
    scratch.init_from_dict(&dict);

    let dict_ll_len = dict.as_dict().fse.literal_lengths.decode().len();
    assert!(
        dict_ll_len > 0,
        "dict fixture should carry a built LL table"
    );
    // Dict-sourced axis resolves to the dictionary's table...
    assert_eq!(
        scratch.fse.ll_table(Some(dict.as_dict())).decode().len(),
        dict_ll_len,
        "Dict-sourced axis must resolve to the shared dictionary's table"
    );
    // ...with the local buffer left untouched (no eager copy).
    assert!(
        scratch.fse.literal_lengths.decode().is_empty(),
        "init_from_dict must not copy table bytes into local scratch (COW)"
    );

    // Same copy-on-write contract for the Huffman literals table: the
    // accessor resolves to the dictionary's built table while the
    // local table stays untouched (max_num_bits == 0 means unbuilt).
    let dict_huf_bits = dict.as_dict().huf.table.max_num_bits;
    assert!(
        dict_huf_bits > 0,
        "dict fixture should carry a built HUF table"
    );
    assert_eq!(
        scratch.huf.huf_table(Some(dict.as_dict())).max_num_bits,
        dict_huf_bits,
        "Dict-sourced HUF axis must resolve to the dictionary's table"
    );
    assert_eq!(
        scratch.huf.table.max_num_bits, 0,
        "init_from_dict must not copy the HUF table into local scratch (COW)"
    );

    scratch.reset(1024);
    assert!(
        scratch.fse.ll_table(None).decode().is_empty(),
        "reset must detach the dictionary copy-on-write source"
    );
    assert_eq!(
        scratch.huf.huf_table(None).max_num_bits,
        0,
        "reset must detach the HUF dictionary copy-on-write source"
    );
}
