//! Small round trips through every match-finder backend, with and without a
//! dictionary. Natively they are plain round trips; their purpose is Miri: the
//! backends hoist raw pointers into their tables above the hot loops, and
//! Miri's Stacked Borrows model checks those pointers stay valid while the
//! tables are used around them. Inputs are kept small so the interpreter
//! finishes in minutes.
//!
//! `cargo +nightly miri test -p structured-zstd --no-default-features
//! --features std,hash,kernel-scalar --test pointer_provenance`

use std::io::Write;

use structured_zstd::decoding::{Dictionary, DictionaryHandle, FrameDecoder};
use structured_zstd::encoding::{
    CompressionLevel, CompressionParameters, Strategy, StreamingEncoder,
};

/// Raw dictionary content: the tail of the repository's test dictionary,
/// past its header and entropy tables.
fn dictionary() -> &'static [u8] {
    let dict: &'static [u8] = include_bytes!("../dict_tests/dictionary");
    &dict[dict.len() - 4096..]
}

/// Every match finder, named by its strategy rather than reached through a
/// level: which strategy a level runs depends on the source and dictionary
/// size tier, and a few KiB of input moves most levels to another one. The
/// last entry is an accelerated negative level, the Fast finder's other loop.
///
/// The tables are kept small: the loops are the same at any size, and clearing
/// a level's full-size tables is most of what Miri would spend its time on.
/// The window still covers the dictionary and the input together.
fn configurations() -> impl Iterator<Item = (String, CompressionParameters)> {
    let forced = (1..=9).map(|ordinal| {
        let strategy = Strategy::from_ordinal(ordinal).unwrap();
        let params = CompressionParameters::builder(CompressionLevel::Level(3))
            .strategy(strategy)
            .window_log(13)
            .hash_log(10)
            .chain_log(10)
            .search_log(3)
            .build()
            .unwrap();
        (format!("{strategy:?}"), params)
    });
    let accelerated = CompressionParameters::builder(CompressionLevel::Level(-5))
        .build()
        .unwrap();
    forced.chain(core::iter::once((String::from("level -5"), accelerated)))
}

/// One frame of `input` under `params`, with `dictionary` attached if given.
fn compress(input: &[u8], params: &CompressionParameters, dictionary: Option<&[u8]>) -> Vec<u8> {
    let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(3));
    encoder.set_parameters(params).unwrap();
    if let Some(dictionary) = dictionary {
        encoder.set_dictionary_from_bytes(dictionary).unwrap();
    }
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

/// Input that both repeats and varies, so every backend finds matches and
/// misses, and reaches back into the dictionary.
fn sample() -> Vec<u8> {
    let dict = dictionary();
    let mut data = Vec::with_capacity(2048);
    let mut state = 0x9E37_79B9u32;
    while data.len() < 2048 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        match state % 3 {
            0 => data.extend_from_slice(&dict[(state as usize >> 4) % (dict.len() - 64)..][..48]),
            1 => data.extend_from_slice(b"label=orders|tenant=demo|segment=0001|"),
            _ => data.extend_from_slice(&state.to_le_bytes()),
        }
    }
    data
}

#[test]
fn every_backend_round_trips() {
    let input = sample();
    for (name, params) in configurations() {
        let frame = compress(&input, &params, None);
        let mut out = Vec::with_capacity(input.len());
        FrameDecoder::new()
            .decode_all_to_vec(&frame, &mut out)
            .unwrap();
        assert_eq!(out, input, "{name}");
    }
}

#[test]
fn every_backend_round_trips_with_a_dictionary() {
    let input = sample();
    let handle = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(0, dictionary().to_vec()).unwrap(),
    );
    for (name, params) in configurations() {
        let frame = compress(&input, &params, Some(dictionary()));
        let mut out = vec![0u8; input.len()];
        let written = FrameDecoder::new()
            .decode_all_with_dict_handle(&frame, &mut out, &handle)
            .unwrap();
        assert_eq!(&out[..written], input.as_slice(), "{name}");
    }
}
