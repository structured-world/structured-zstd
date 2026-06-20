//! Cross-implementation conformance for dictionary compression: a frame we
//! compress with a finalized FastCOVER dictionary must decode through the C
//! `zstd` dictionary decoder. The dictionary build + compression happen in the
//! `structured_zstd::testing` facade (pure Rust); only the C decode lives here.
#![cfg(all(feature = "bench_internals", feature = "dict_builder"))]

use structured_zstd::testing::dict_roundtrip_fixture;

#[test]
fn finalize_raw_dict_roundtrips_with_c_decoder() {
    let (finalized, compressed, payload) = dict_roundtrip_fixture();

    let mut decoder = zstd::bulk::Decompressor::with_dictionary(finalized.as_slice())
        .expect("C decoder should accept finalized dictionary");
    let mut decoded = Vec::with_capacity(payload.len());
    let written = decoder
        .decompress_to_buffer(compressed.as_slice(), &mut decoded)
        .expect("C decoder should decode payload");
    assert_eq!(written, payload.len());
    assert_eq!(decoded, payload);
}

/// A copy-mode Fast (level 1) dictionary frame must decode through the C
/// dictionary decoder. Inputs over the Fast 8 KiB cutoff route through the copy
/// path, whose prefix floor now reaches back to the dict start (upstream zstd
/// `ZSTD_getLowestPrefixIndex` with `isDictionary`). That makes the encoder emit
/// offsets reaching into the dictionary region (up to `window + dictSize`), a
/// new offset class for this path; this pins that the C reference decoder
/// accepts them given the same raw-content dictionary.
#[test]
fn copy_mode_fast_dict_frame_decodes_with_c() {
    use structured_zstd::decoding::Dictionary;
    use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

    // The same repeated-log-line structure the dict shares, so a >8 KiB input
    // matches the dict heavily and the copy path emits dict-reaching offsets.
    fn repeated_log_lines(len: usize) -> Vec<u8> {
        const LINES: &[&str] = &[
            "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders\n",
            "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders\n",
            "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders\n",
            "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders\n",
        ];
        let mut out = Vec::with_capacity(len);
        let mut i = 0usize;
        while out.len() < len {
            let line = LINES[i % LINES.len()].as_bytes();
            let take = line.len().min(len - out.len());
            out.extend_from_slice(&line[..take]);
            i += 1;
        }
        out
    }

    let dict = repeated_log_lines(8 * 1024);
    let payload = repeated_log_lines(16 * 1024); // > 8 KiB → Fast copy mode.

    // Raw-content dict with the id flag off, so the frame carries no dict id and
    // the C side can decode with the same raw blob (dict id 0).
    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(1));
    let dict_obj = Dictionary::from_raw_content(1, dict.clone()).expect("raw dict");
    cctx.set_dictionary_id_flag(false);
    cctx.set_dictionary(dict_obj).expect("attach dict");
    let compressed = cctx.compress_independent_frame(&payload);

    let mut decoder = zstd::bulk::Decompressor::with_dictionary(dict.as_slice())
        .expect("C decoder should accept raw-content dictionary");
    let mut decoded = Vec::with_capacity(payload.len());
    let written = decoder
        .decompress_to_buffer(compressed.as_slice(), &mut decoded)
        .expect("C decoder should decode copy-mode dict frame");
    assert_eq!(written, payload.len());
    assert_eq!(decoded, payload);
}
