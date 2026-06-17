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
