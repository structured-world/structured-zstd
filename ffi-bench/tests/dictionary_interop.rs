//! Dictionary frames against libzstd, across every way this crate makes one.
//!
//! A dictionary frame is only right when encoder and decoder agree on where
//! every dictionary byte sits, and the encoder takes the dictionary in pieces
//! sized by the window, which the API path decides: a one-shot compressor, a
//! stream with or without its size pledged or hinted, a context reused for
//! frame after frame. So the matrix crosses those paths with dictionary
//! lengths just past a whole number of pieces, inputs that reach the
//! dictionary's start and its end, and a representative level of every
//! strategy, and checks each frame through the C decoder and ours. The other
//! direction, libzstd's frames through our decoders, runs over the same
//! dictionaries.

use std::io::{Read, Write};

use structured_zstd::decoding::{DictionaryHandle, FrameDecoder, StreamingDecoder};
use structured_zstd::encoding::{
    CompressionContext, CompressionLevel, EncoderDictionary, FrameCompressor, StreamingEncoder,
};

/// One level per strategy: fast, dfast (two tiers), greedy, lazy, lazy2,
/// btlazy2, btopt, btultra, btultra2 and the top of the scale.
const LEVELS: [i32; 11] = [1, 2, 3, 5, 6, 8, 13, 16, 18, 19, 22];

/// Compressible bytes over a small alphabet with runs, so matches fire both
/// within the input and into a dictionary drawn the same way.
fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let symbol = b'a' + (state % 40) as u8;
        let run = 1 + (state >> 40) as usize % 4;
        for _ in 0..run.min(len - out.len()) {
            out.push(symbol);
        }
    }
    out
}

/// The inputs a dictionary is tried with: its first bytes (matches reach the
/// farthest back a frame can look), its last bytes (matches reach the bytes a
/// short last piece holds), and a stream of several blocks that mixes
/// dictionary stretches with fresh bytes.
fn inputs(dict: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let small = dict.len().min(11_000);
    let mut mixed = Vec::new();
    let mut seed = 0x51EDu64;
    while mixed.len() < 120_000 {
        let at = (seed as usize * 7919) % dict.len();
        let take = (dict.len() - at).min(3_000);
        mixed.extend_from_slice(&dict[at..at + take]);
        mixed.extend_from_slice(&bytes(seed, 1_500));
        seed += 1;
    }
    vec![
        ("start", dict[..small].to_vec()),
        ("end", dict[dict.len() - small..].to_vec()),
        ("mixed", mixed),
    ]
}

/// Ways a frame is made here.
#[derive(Clone, Copy, Debug)]
enum Path {
    OneShot,
    Streamed,
    Pledged,
    Hinted,
    /// Three frames on one context, each with its size pledged; the second and
    /// third reuse the dictionary already in place.
    Reused,
}

const PATHS: [Path; 5] = [
    Path::OneShot,
    Path::Streamed,
    Path::Pledged,
    Path::Hinted,
    Path::Reused,
];

fn stream_frame(context: &mut CompressionContext, data: &[u8], pledge: bool) -> Vec<u8> {
    let mut encoder = StreamingEncoder::with_context(Vec::new(), context);
    if pledge {
        encoder.set_pledged_content_size(data.len() as u64).unwrap();
    }
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// The frames `path` makes of `data` at `level` with `dictionary`.
fn compress(path: Path, level: i32, dictionary: &EncoderDictionary, data: &[u8]) -> Vec<Vec<u8>> {
    let level = CompressionLevel::from_level(level);
    if let Path::OneShot = path {
        let mut compressor: FrameCompressor = FrameCompressor::new(level);
        compressor
            .set_encoder_dictionary(dictionary.clone())
            .unwrap();
        return vec![compressor.compress_independent_frame(data)];
    }
    let mut context = CompressionContext::new(level);
    context.set_content_checksum(true).unwrap();
    context.set_encoder_dictionary(dictionary.clone()).unwrap();
    match path {
        Path::Streamed => vec![stream_frame(&mut context, data, false)],
        Path::Pledged => vec![stream_frame(&mut context, data, true)],
        Path::Hinted => {
            context.set_source_size_hint(data.len() as u64).unwrap();
            vec![stream_frame(&mut context, data, false)]
        }
        Path::Reused => (0..3)
            .map(|_| stream_frame(&mut context, data, true))
            .collect(),
        Path::OneShot => unreachable!("handled above"),
    }
}

/// Decode `frame` through libzstd and through both of our decoders; a
/// description of the first that does not give back `data`.
fn check(frame: &[u8], dict_raw: &[u8], data: &[u8]) -> Option<String> {
    let mut c_decoder = zstd::bulk::Decompressor::with_dictionary(dict_raw).unwrap();
    match c_decoder.decompress(frame, data.len()) {
        Ok(decoded) if decoded == data => {}
        Ok(_) => return Some("libzstd decoded other bytes".into()),
        Err(err) => return Some(format!("libzstd refused: {err}")),
    }
    let handle = DictionaryHandle::from_serialized_or_raw_content(dict_raw).unwrap();
    let mut decoder = FrameDecoder::new();
    let mut out = vec![0u8; data.len()];
    match decoder.decode_all_with_dict_handle(frame, &mut out, &handle) {
        Ok(n) if out[..n] == *data => {}
        Ok(_) => return Some("FrameDecoder decoded other bytes".into()),
        Err(err) => return Some(format!("FrameDecoder refused: {err:?}")),
    }
    let mut stream =
        StreamingDecoder::new_with_decoder_and_dictionary_handle(frame, &mut decoder, &handle)
            .unwrap_or_else(|_| panic!("the dictionary is usable"));
    let mut streamed = Vec::new();
    match stream.read_to_end(&mut streamed) {
        Ok(_) if streamed == data => None,
        Ok(_) => Some("StreamingDecoder decoded other bytes".into()),
        Err(err) => Some(format!("StreamingDecoder refused: {err}")),
    }
}

/// Every path, input and level for `dict_raw`, ours to libzstd and libzstd to
/// ours; the failures, labelled.
fn matrix(name: &str, dict_raw: &[u8]) -> Vec<String> {
    let dictionary = EncoderDictionary::from_serialized_or_raw_content(dict_raw).unwrap();
    let mut failures = Vec::new();
    for (input, data) in inputs(dict_raw) {
        for level in LEVELS {
            for path in PATHS {
                for (frame_no, frame) in
                    compress(path, level, &dictionary, &data).iter().enumerate()
                {
                    if let Some(why) = check(frame, dict_raw, &data) {
                        failures.push(format!(
                            "{name} {input} level {level} {path:?} frame {frame_no}: {why}"
                        ));
                    }
                }
            }
            let mut c_encoder = zstd::bulk::Compressor::with_dictionary(level, dict_raw).unwrap();
            c_encoder
                .set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
                .unwrap();
            let theirs = c_encoder.compress(&data).unwrap();
            if let Some(why) = check(&theirs, dict_raw, &data) {
                failures.push(format!("{name} {input} level {level} libzstd frame: {why}"));
            }
        }
    }
    failures
}

fn assert_clean(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A raw-content dictionary `base + tail` bytes long.
fn raw_dictionary(base: usize, tail: usize) {
    let dict = bytes(0xD1C7 ^ (base + tail) as u64, base + tail);
    assert_clean(matrix(&format!("raw {base}+{tail}"), &dict));
}

/// One test per dictionary length, so the matrix runs in parallel. The bases
/// are whole numbers of every piece size up to them, except 110 KiB (the
/// command line's default dictionary size), a whole number of 1 KiB and 2 KiB
/// pieces only.
macro_rules! raw_dictionary_tests {
    ($($name:ident: $base:expr, $tail:expr;)*) => {
        $(
            #[test]
            fn $name() {
                raw_dictionary($base, $tail);
            }
        )*
    };
}

raw_dictionary_tests! {
    raw_4_kib_plus_0: 4 * 1024, 0;
    raw_4_kib_plus_1: 4 * 1024, 1;
    raw_4_kib_plus_2: 4 * 1024, 2;
    raw_4_kib_plus_3: 4 * 1024, 3;
    raw_4_kib_plus_4: 4 * 1024, 4;
    raw_16_kib_plus_0: 16 * 1024, 0;
    raw_16_kib_plus_1: 16 * 1024, 1;
    raw_16_kib_plus_2: 16 * 1024, 2;
    raw_16_kib_plus_3: 16 * 1024, 3;
    raw_16_kib_plus_4: 16 * 1024, 4;
    raw_64_kib_plus_0: 64 * 1024, 0;
    raw_64_kib_plus_1: 64 * 1024, 1;
    raw_64_kib_plus_2: 64 * 1024, 2;
    raw_64_kib_plus_3: 64 * 1024, 3;
    raw_64_kib_plus_4: 64 * 1024, 4;
    raw_110_kib_plus_0: 112_640, 0;
    raw_110_kib_plus_1: 112_640, 1;
    raw_110_kib_plus_2: 112_640, 2;
    raw_110_kib_plus_3: 112_640, 3;
    raw_110_kib_plus_4: 112_640, 4;
    raw_128_kib_plus_0: 128 * 1024, 0;
    raw_128_kib_plus_1: 128 * 1024, 1;
    raw_128_kib_plus_2: 128 * 1024, 2;
    raw_128_kib_plus_3: 128 * 1024, 3;
    raw_128_kib_plus_4: 128 * 1024, 4;
}

/// A dictionary libzstd trained, so its content length is whatever training
/// left, with entropy tables and repeat offsets of its own.
#[test]
fn trained_dictionary_interoperates() {
    let corpus = bytes(0x7EA1, 400_000);
    let samples: Vec<&[u8]> = corpus.chunks(1_000).collect();
    let dict = zstd::dict::from_samples(&samples, 100_000).unwrap();
    assert_clean(matrix("trained", &dict));
}
