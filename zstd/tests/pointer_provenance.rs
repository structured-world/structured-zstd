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
use structured_zstd::encoding::{CompressionLevel, StreamingEncoder, compress_to_vec};

/// Raw dictionary content: the tail of the repository's test dictionary,
/// past its header and entropy tables.
fn dictionary() -> &'static [u8] {
    let dict: &'static [u8] = include_bytes!("../dict_tests/dictionary");
    &dict[dict.len() - 4096..]
}

/// One level per backend: Fast (negative and positive), dfast, greedy/lazy,
/// lazy2, btlazy2, btopt, btultra2.
const LEVELS: [i32; 8] = [-5, 1, 3, 5, 9, 13, 16, 19];

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
    for level in LEVELS {
        let frame = compress_to_vec(input.as_slice(), CompressionLevel::Level(level));
        let mut out = Vec::with_capacity(input.len());
        FrameDecoder::new()
            .decode_all_to_vec(&frame, &mut out)
            .unwrap();
        assert_eq!(out, input, "level {level}");
    }
}

#[test]
fn every_backend_round_trips_with_a_dictionary() {
    let input = sample();
    let handle = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(0, dictionary().to_vec()).unwrap(),
    );
    for level in LEVELS {
        let mut encoder = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(level));
        encoder.set_dictionary_from_bytes(dictionary()).unwrap();
        encoder.write_all(&input).unwrap();
        let frame = encoder.finish().unwrap();
        let mut out = vec![0u8; input.len()];
        let written = FrameDecoder::new()
            .decode_all_with_dict_handle(&frame, &mut out, &handle)
            .unwrap();
        assert_eq!(&out[..written], input.as_slice(), "level {level}");
    }
}
