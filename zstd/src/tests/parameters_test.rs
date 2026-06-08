//! Integration tests for the fine-grained compression parameter API (#27):
//! builder → [`compress_with_parameters`] → decode round-trip, the LDM
//! activation surface, the `Greedy` strategy override, and the
//! default-equivalence invariant (an empty override reproduces plain
//! level-based output byte-for-byte).

extern crate std;

use alloc::vec::Vec;

use crate::decoding::StreamingDecoder;
use crate::encoding::{
    CompressionLevel, CompressionParameters, FrameCompressor, Strategy, compress_slice_to_vec,
    compress_with_parameters,
};
use crate::io::Read;

/// Deterministic LCG byte stream (mirrors `roundtrip_integrity`).
fn generate_data(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut data = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push((state >> 33) as u8);
    }
    data
}

/// A genuinely compressible long-range-repetitive fixture: a
/// low-entropy base block (a short motif tiled to 4 KiB, so the cheap
/// incompressibility sampler classifies it compressible) repeated many
/// times with a few bytes of per-copy noise. Back-references at the
/// 4 KiB base distance dominate, so the encoder compresses it well below
/// input size (unlike a random base, which the sampler raw-stores).
fn long_range_repetitive(total_len: usize) -> Vec<u8> {
    // 16-byte motif → tiled to a 4 KiB base. Low entropy: the sampler
    // sees the repetition and the block is compressed, not raw-stored.
    let motif = generate_data(0xABCD_1234, 16);
    let mut base = Vec::with_capacity(4096);
    while base.len() < 4096 {
        base.extend_from_slice(&motif);
    }
    base.truncate(4096);

    let mut data = Vec::with_capacity(total_len);
    let mut counter = 0u64;
    while data.len() < total_len {
        data.extend_from_slice(&base);
        // A few bytes of per-copy noise so each repeat is a distinct
        // long-range back-reference rather than one giant run.
        counter = counter.wrapping_add(1);
        data.extend_from_slice(&counter.to_le_bytes());
    }
    data.truncate(total_len);
    data
}

fn decode(compressed: &[u8]) -> Vec<u8> {
    let mut decoder = StreamingDecoder::new(compressed).unwrap();
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    out
}

/// A parameter set that overrides nothing must produce byte-identical
/// output to plain level-based compression — the default path is
/// untouched.
#[test]
fn empty_override_is_byte_identical_to_level() {
    let data = generate_data(7, 64 * 1024);
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
        CompressionLevel::Level(5),
        CompressionLevel::Level(19),
    ] {
        let params = CompressionParameters::builder(level).build().unwrap();
        let via_params = compress_with_parameters(&data, &params);
        let via_level = compress_slice_to_vec(&data, level);
        assert_eq!(
            via_params, via_level,
            "empty override diverged from level {level:?}",
        );
    }
}

/// Custom parameters must produce valid (decodable) frames that
/// reproduce the input.
#[test]
fn custom_parameters_round_trip() {
    let data = generate_data(99, 96 * 1024);

    let cases = [
        CompressionParameters::builder(CompressionLevel::Level(3))
            .window_log(18)
            .strategy(Strategy::Dfast)
            .build()
            .unwrap(),
        CompressionParameters::builder(CompressionLevel::Level(9))
            .strategy(Strategy::Lazy2)
            .search_log(6)
            .target_length(64)
            .build()
            .unwrap(),
        CompressionParameters::builder(CompressionLevel::Level(19))
            .window_log(22)
            .hash_log(23)
            .chain_log(24)
            .strategy(Strategy::Btultra2)
            .build()
            .unwrap(),
    ];

    for params in cases {
        let compressed = compress_with_parameters(&data, &params);
        assert_eq!(
            decode(&compressed),
            data,
            "round-trip failed for {params:?}"
        );
    }
}

/// `set_parameter(Strategy, Greedy)` produces a greedy-parsed frame at any
/// level, and it decodes back to the input.
#[test]
fn greedy_strategy_override_round_trips() {
    let data = generate_data(0x5151, 80 * 1024);
    // Force greedy onto a level the table would resolve to lazy (7).
    let params = CompressionParameters::builder(CompressionLevel::Level(7))
        .strategy(Strategy::Greedy)
        .build()
        .unwrap();
    let compressed = compress_with_parameters(&data, &params);
    assert_eq!(decode(&compressed), data);
    // Greedy on a highly-repetitive fixture compresses well below input
    // size (the back-references dominate) and still round-trips.
    let compressible = long_range_repetitive(80 * 1024);
    let greedy = compress_with_parameters(
        &compressible,
        &CompressionParameters::builder(CompressionLevel::Level(5))
            .strategy(Strategy::Greedy)
            .build()
            .unwrap(),
    );
    assert!(
        greedy.len() < compressible.len() / 2,
        "greedy did not compress repetitive fixture: {} vs {}",
        greedy.len(),
        compressible.len(),
    );
    assert_eq!(decode(&greedy), compressible);
}

/// LDM-on activation: `enable_long_distance_matching(true)` on an optimal
/// strategy round-trips correctly through our decoder.
#[cfg(feature = "hash")]
#[test]
fn ldm_on_round_trips() {
    let data = long_range_repetitive(512 * 1024);
    let params = CompressionParameters::builder(CompressionLevel::Level(19))
        .enable_long_distance_matching(true)
        .build()
        .unwrap();
    let compressed = compress_with_parameters(&data, &params);
    assert_eq!(decode(&compressed), data);
}

/// LDM ratio: on a long-range-repetitive fixture, the LDM-on variant must
/// be no larger than LDM-off. The LDM producer only adds candidates to the
/// optimal parser, never removes valid regular matches, so enabling it can
/// only keep or improve the ratio (within the regular window the two are
/// often equal, hence `<=` rather than strict `<`).
#[cfg(feature = "hash")]
#[test]
fn ldm_on_is_not_larger_than_ldm_off() {
    let data = long_range_repetitive(1024 * 1024);

    let off = CompressionParameters::builder(CompressionLevel::Level(19))
        .build()
        .unwrap();
    let on = CompressionParameters::builder(CompressionLevel::Level(19))
        .enable_long_distance_matching(true)
        .build()
        .unwrap();

    let off_sz = compress_with_parameters(&data, &off).len();
    let on_compressed = compress_with_parameters(&data, &on);
    let on_sz = on_compressed.len();

    // Correctness first: LDM-on must still decode to the input.
    assert_eq!(decode(&on_compressed), data);
    assert!(
        on_sz <= off_sz,
        "LDM-on ({on_sz}) larger than LDM-off ({off_sz})",
    );
}

/// Custom LDM knobs override the strategy-derived defaults and still
/// produce a decodable frame.
#[cfg(feature = "hash")]
#[test]
fn ldm_custom_knobs_round_trip() {
    let data = long_range_repetitive(512 * 1024);
    let params = CompressionParameters::builder(CompressionLevel::Level(19))
        .ldm_hash_log(20)
        .ldm_min_match(48)
        .ldm_bucket_size_log(4)
        .ldm_hash_rate_log(6)
        .build()
        .unwrap();
    assert_eq!(decode(&compress_with_parameters(&data, &params)), data);
}

/// `FrameCompressor::set_parameters` on the streaming path reuses one
/// compressor across frames and produces decodable output.
#[test]
fn set_parameters_streaming_round_trip() {
    let data = generate_data(0xC0FFEE, 70 * 1024);
    let params = CompressionParameters::builder(CompressionLevel::Level(11))
        .strategy(Strategy::Lazy2)
        .window_log(20)
        .build()
        .unwrap();

    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_parameters(&params);
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut compressed);
    compressor.compress();

    assert_eq!(decode(&compressed), data);
}

/// A window_log override shrinks the advertised frame window: a frame
/// built with window_log 17 must decode with a smaller declared window
/// than one at the level default. We assert the round-trip and that the
/// override path is honoured by decoding successfully under our decoder
/// (which enforces the advertised window).
#[test]
fn window_log_override_round_trips() {
    let data = long_range_repetitive(256 * 1024);
    let params = CompressionParameters::builder(CompressionLevel::Level(9))
        .window_log(17)
        .build()
        .unwrap();
    let compressed = compress_with_parameters(&data, &params);
    assert_eq!(decode(&compressed), data);
}
/// LDM at a large window (`window_log >= 24`) over a multi-block,
/// multi-segment input. Regression for the LDM↔optimal-parser segment
/// misalignment: `ldm_sequences` are block-relative, but the optimal
/// parser runs per-segment with segment-relative positions, so every
/// segment after the first must fast-forward the raw LDM seq-store by its
/// block offset. Before the fix this emitted matches copying the wrong
/// bytes (frame rejected by both our decoder and upstream `zstd -d`).
#[cfg(feature = "hash")]
#[test]
fn ldm_large_window_multi_segment_round_trips() {
    let data = long_range_repetitive(512 * 1024);
    for window_log in [24, 25, 26] {
        let params = CompressionParameters::builder(CompressionLevel::Level(19))
            .window_log(window_log)
            .enable_long_distance_matching(true)
            .build()
            .unwrap();
        let compressed = compress_with_parameters(&data, &params);
        assert_eq!(
            decode(&compressed),
            data,
            "LDM round-trip failed at window_log {window_log}",
        );
    }
}

/// Streaming decode of a frame whose content far exceeds its window, so the
/// decoder's `RingBuffer` cycles many times. Exercises the bounded-ring drain
/// loop in `StreamingDecoder::read` (decode one block, drain into the caller's
/// buffer, repeat) instead of accumulating the whole `read_to_end` request in
/// the ring. A fast level keeps the decode dominated by match copies through
/// the wrapped window.
#[test]
fn streaming_decode_cycles_small_window_round_trips() {
    // 1 MiB content, 128 KiB window (window_log 17): the ring wraps ~8 times.
    let data = long_range_repetitive(1024 * 1024);
    let params = CompressionParameters::builder(CompressionLevel::Level(1))
        .window_log(17)
        .build()
        .unwrap();
    let compressed = compress_with_parameters(&data, &params);
    assert_eq!(
        decode(&compressed),
        data,
        "streaming decode over a cycling window must round-trip exactly",
    );
}

/// Reverting to a plain level via `set_compression_level` after a
/// customized frame must drop the parameter overrides. Otherwise the
/// strategy/LDM/log overrides stay sticky and the "plain" frame is still
/// encoded with the previous tuning. The second frame must be
/// byte-identical to a fresh plain-level compression of the same input.
#[test]
fn set_compression_level_clears_parameter_overrides() {
    // Compressible fixture so the strategy choice actually changes the
    // output (incompressible data raw-stores regardless of strategy).
    let data = long_range_repetitive(64 * 1024);

    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    // First frame: a custom strategy override (greedy) far from the
    // level-19 default (btultra2).
    let params = CompressionParameters::builder(CompressionLevel::Level(19))
        .strategy(Strategy::Greedy)
        .build()
        .unwrap();
    compressor.set_parameters(&params);
    let _first = compressor.compress_independent_frame(&data);

    // Revert to a plain level and compress again.
    compressor.set_compression_level(CompressionLevel::Level(19));
    let reverted = compressor.compress_independent_frame(&data);

    // Must match a brand-new compressor at plain level 19 (no overrides).
    let expected = compress_slice_to_vec(&data, CompressionLevel::Level(19));
    assert_eq!(
        reverted, expected,
        "set_compression_level did not clear sticky parameter overrides",
    );
    assert_eq!(decode(&reverted), data);
}
