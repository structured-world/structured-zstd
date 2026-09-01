//! Cross-implementation parity tests for the frame compressor: every case
//! compresses with `structured-zstd` and decodes the result through the C
//! `zstd` bindings, asserting byte-exact roundtrips (and, where relevant, the
//! on-wire header / first-block-type the encoder chose). Moved out of the
//! library crate so it never links the C bindings; the header/block
//! introspection goes through the `structured_zstd::testing` facade.
#![cfg(feature = "bench_internals")]

use structured_zstd::encoding::{CompressionLevel, FrameCompressor, compress_to_vec};
use structured_zstd::testing::{BlockType, first_block_type, frame_header_info};

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

fn compress_hinted(data: &[u8], level: CompressionLevel) -> Vec<u8> {
    let mut compressor = FrameCompressor::new(level);
    compressor.set_source_size_hint(data.len() as u64);
    compressor.set_source(data);
    let mut out = Vec::new();
    compressor.set_drain(&mut out);
    compressor.compress();
    out
}

fn c_decode(compressed: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    zstd::stream::copy_decode(compressed, &mut decoded).expect("C zstd must decode our output");
    decoded
}

/// The pre-splitter tier selected per strategy matches upstream's default
/// `splitLevels[strategy]`: on a homogeneous but periodic 512 KB log stream
/// the lazy2 / btlazy2 tier (byChunks rate 5) splits into more blocks than
/// the lazy tier, exactly as `ZSTD_compress2` does, so every level's output
/// is no larger than the reference's (a wrong tier shows up either as a
/// ballooned lazy2 frame the reference does not produce, or as a larger
/// frame on the levels where upstream splits and we would not).
#[test]
fn periodic_stream_presplit_matches_reference() {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let target = 512 * 1024usize;
    let mut data = Vec::with_capacity(target);
    let mut i = 0;
    while data.len() < target {
        let line = LINES[i % LINES.len()].as_bytes();
        let take = line.len().min(target - data.len());
        data.extend_from_slice(&line[..take]);
        i += 1;
    }
    for level in [3i32, 5, 7, 8, 11, 15, 16] {
        let ours = compress_to_vec(&data[..], CompressionLevel::Level(level));
        let reference = zstd::bulk::compress(&data[..], level).expect("C compress");
        assert!(
            ours.len() <= reference.len(),
            "L{level}: ours {} bytes > reference {} bytes on the periodic stream",
            ours.len(),
            reference.len()
        );
        assert_eq!(c_decode(&ours[..]), data, "L{level} roundtrip through C");
    }
}

/// Frame content size is written correctly and C zstd can decompress the output.
#[test]
fn fcs_header_written_and_c_zstd_compatible() {
    let levels = [
        CompressionLevel::Uncompressed,
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
    ];
    let fcs_2byte = vec![0xCDu8; 300]; // 300 bytes → 2-byte FCS (256..=65791 range)
    let large = vec![0xABu8; 100_000];
    let inputs: [&[u8]; 5] = [
        &[],
        &[0x00],
        b"abcdefghijklmnopqrstuvwxy\n",
        &fcs_2byte,
        &large,
    ];
    for level in levels {
        for data in &inputs {
            let compressed = compress_to_vec(*data, level);
            let (_, fcs, fcs_bytes) = frame_header_info(&compressed);
            assert_eq!(
                fcs,
                data.len() as u64,
                "FCS mismatch len={} level={level:?}",
                data.len()
            );
            assert_ne!(
                fcs_bytes,
                0,
                "FCS field must be present len={} level={level:?}",
                data.len()
            );
            assert_eq!(
                c_decode(&compressed).as_slice(),
                *data,
                "C roundtrip failed len={}",
                data.len()
            );
        }
    }
}

#[test]
fn source_size_hint_fastest_remains_ffi_compatible_small_input() {
    let data = vec![0xAB; 2047];
    let compressed = compress_hinted(&data, CompressionLevel::Fastest);
    assert_eq!(c_decode(&compressed), data);
}

#[test]
fn small_hinted_default_frame_uses_single_segment_header() {
    let data = generate_data(0xD15E_A5ED, 1024);
    let compressed = compress_hinted(&data, CompressionLevel::Default);
    let (single_segment, fcs, _) = frame_header_info(&compressed);
    assert!(
        single_segment,
        "small hinted default frames should use single-segment header"
    );
    assert_eq!(fcs, data.len() as u64);
    assert_eq!(c_decode(&compressed), data);
}

#[test]
fn small_hinted_numeric_default_levels_use_single_segment_header() {
    let data = generate_data(0xA11C_E003, 1024);
    for level in [CompressionLevel::Level(0), CompressionLevel::Level(3)] {
        let compressed = compress_hinted(&data, level);
        let (single_segment, fcs, _) = frame_header_info(&compressed);
        assert!(single_segment, "single-segment expected (level={level:?})");
        assert_eq!(fcs, data.len() as u64);
        assert_eq!(c_decode(&compressed), data);
    }
}

#[test]
fn source_size_hint_levels_remain_ffi_compatible_small_inputs_matrix() {
    let levels = [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
        CompressionLevel::Level(-1),
        CompressionLevel::Level(2),
        CompressionLevel::Level(3),
        CompressionLevel::Level(4),
        CompressionLevel::Level(11),
    ];
    let sizes = [
        511usize, 512, 513, 1023, 1024, 1536, 2047, 2048, 4095, 4096, 8191, 16_384, 16_385,
    ];
    for (seed_idx, seed) in [11u64, 23, 41].into_iter().enumerate() {
        for &size in &sizes {
            let data = generate_data(seed + seed_idx as u64, size);
            for &level in &levels {
                let compressed = compress_hinted(&data, level);
                // Known-size payloads that fit the window are single-segment
                // regardless of size (upstream policy: no lower size floor).
                let (single_segment, _, _) = frame_header_info(&compressed);
                assert!(
                    single_segment,
                    "hinted small frame should be single-segment level={level:?} size={size}"
                );
                assert_eq!(
                    c_decode(&compressed),
                    data,
                    "hinted roundtrip mismatch level={level:?} size={size}"
                );
            }
        }
    }
}

#[test]
fn hinted_levels_use_single_segment_header_symmetrically() {
    let levels = [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
        CompressionLevel::Level(0),
        CompressionLevel::Level(2),
        CompressionLevel::Level(3),
        CompressionLevel::Level(4),
        CompressionLevel::Level(11),
    ];
    for (seed_idx, seed) in [7u64, 23, 41].into_iter().enumerate() {
        let size = 1024 + seed_idx * 97;
        let data = generate_data(seed, size);
        for &level in &levels {
            let compressed = compress_hinted(&data, level);
            let (single_segment, fcs, _) = frame_header_info(&compressed);
            assert!(
                single_segment,
                "hinted frame should be single-segment level={level:?} size={}",
                data.len()
            );
            assert_eq!(fcs, data.len() as u64);
            assert_eq!(c_decode(&compressed), data);
        }
    }
}

#[test]
fn small_hinted_frames_are_single_segment_below_512() {
    // Regression for the dropped 512-byte single-segment floor: a known-size
    // payload that fits the window is single-segment at any size (matching
    // upstream `ZSTD_writeFrameHeader`). The old floor forced a windowed
    // header with a 4-byte FCS on sub-512 inputs.
    let levels = [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
        CompressionLevel::Level(0),
        CompressionLevel::Level(2),
        CompressionLevel::Level(3),
        CompressionLevel::Level(4),
        CompressionLevel::Level(11),
    ];
    for (seed_idx, seed) in [7u64, 23, 41].into_iter().enumerate() {
        for &size in &[1usize, 100, 255, 256, 511, 512] {
            let data = generate_data(seed + seed_idx as u64, size);
            for &level in &levels {
                let compressed = compress_hinted(&data, level);
                let (single_segment, fcs, _) = frame_header_info(&compressed);
                assert!(
                    single_segment,
                    "small hinted frame should be single-segment level={level:?} size={size}"
                );
                assert_eq!(fcs, size as u64, "FCS must equal len size={size}");
                assert_eq!(c_decode(&compressed), data);
            }
        }
    }
}

#[test]
fn small_frames_are_not_larger_than_c() {
    // The dropped 512 floor plus the post-hoc store-raw fallback together fix
    // a small-input ratio regression: sub-256 inputs paid a 4-byte FCS +
    // window descriptor that upstream avoids with a single-segment 1-byte FCS,
    // and incompressible small blocks were emitted as oversized compressed
    // blocks instead of raw. Guard that our one-shot frame is never larger
    // than the C reference across small sizes and content shapes, and that it
    // round-trips through the C decoder (a single-segment window equals the
    // content size, so a non-shrinking compressed block would fail to decode).
    for size in 1..=64usize {
        let varied = generate_data(0xBADC0FFE, size);
        let ramp: Vec<u8> = (0..size).map(|i| i as u8).collect();
        let all_same = vec![0x5Au8; size];
        for (label, input) in [("varied", &varied), ("ramp", &ramp), ("rle", &all_same)] {
            let ours = compress_to_vec(input.as_slice(), CompressionLevel::Fastest);
            let c = zstd::bulk::compress(input.as_slice(), 1).expect("C compress");
            assert!(
                ours.len() <= c.len(),
                "ours {} > C {} ({label}) size={size}",
                ours.len(),
                c.len()
            );
            assert_eq!(c_decode(&ours), *input, "roundtrip ({label}) size={size}");
        }
    }
}

fn assert_raw_fast_path(seed: u64, level: CompressionLevel) {
    let data = generate_data(seed, 10 * 1024);
    let compressed = compress_to_vec(data.as_slice(), level);
    assert_eq!(first_block_type(&compressed), BlockType::Raw);
    assert_eq!(c_decode(&compressed), data);
}

#[test]
fn fastest_random_block_uses_raw_fast_path() {
    assert_raw_fast_path(0xC0FF_EE11, CompressionLevel::Fastest);
}

#[test]
fn default_random_block_uses_raw_fast_path() {
    assert_raw_fast_path(0xD15E_A5ED, CompressionLevel::Default);
}

#[test]
fn best_random_block_uses_raw_fast_path() {
    assert_raw_fast_path(0xB35C_AFE1, CompressionLevel::Best);
}

#[test]
fn level2_random_block_uses_raw_fast_path() {
    assert_raw_fast_path(0xA11C_E222, CompressionLevel::Level(2));
}

#[test]
fn better_random_block_uses_raw_fast_path() {
    assert_raw_fast_path(0xBE77_E111, CompressionLevel::Better);
}

const LOG_LINE: &[u8] = b"ts=2026-04-10T00:00:00Z level=INFO tenant=demo op=flush table=orders\n";

fn fill_log_lines(target: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(target);
    while data.len() < target {
        let remaining = target - data.len();
        data.extend_from_slice(&LOG_LINE[..LOG_LINE.len().min(remaining)]);
    }
    data
}

#[test]
fn compressible_logs_do_not_fall_back_to_raw_fast_path() {
    let data = fill_log_lines(16 * 1024);
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Level(3),
        CompressionLevel::Better,
        CompressionLevel::Best,
    ] {
        let compressed = compress_to_vec(data.as_slice(), level);
        assert_ne!(
            first_block_type(&compressed),
            BlockType::Raw,
            "level={level:?}"
        );
        assert!(
            compressed.len() < data.len(),
            "compressible input should shrink level={level:?}"
        );
        assert_eq!(c_decode(&compressed), data);
    }
}

#[test]
fn hinted_small_compressible_frames_use_single_segment_across_levels() {
    let data = fill_log_lines(4 * 1024);
    for level in [
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
        CompressionLevel::Level(0),
        CompressionLevel::Level(3),
        CompressionLevel::Level(4),
        CompressionLevel::Level(11),
    ] {
        let compressed = compress_hinted(&data, level);
        let (single_segment, _, _) = frame_header_info(&compressed);
        assert!(
            single_segment,
            "hinted small compressible frame should use single-segment (level={level:?})"
        );
        assert_ne!(
            first_block_type(&compressed),
            BlockType::Raw,
            "level={level:?}"
        );
        assert!(
            compressed.len() < data.len(),
            "compressible hinted frame should shrink level={level:?}"
        );
        assert_eq!(c_decode(&compressed), data);
    }
}

/// The bench `small-4k-log-lines` fixture: four rotating log lines tiled to
/// `len` bytes (byte-identical to the `compare_ffi` scenario).
fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        for line in LINES {
            if bytes.len() == len {
                break;
            }
            let remaining = len - bytes.len();
            bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
        }
    }
    bytes
}

/// Regression: a one-shot frame whose source-size hint places it in a small
/// cParams tier must run the same parse strategy C resolves, so its literal
/// section is never larger than C's. For a <=16 KiB frame upstream
/// `ZSTD_getCParams` promotes levels 13-17 to btultra/btultra2, which enables
/// the HUF table-log search. The matcher already resolved that strategy, but
/// the literal-compression / HUF-search gate used to re-derive it from the bare
/// level (btlazy2/btopt) and skip the search, overshooting C by 4 bytes on the
/// 4 KiB log fixture. The gate now reads the matcher's resolved strategy.
#[test]
fn small_hinted_frames_match_c_literal_section_levels_13_to_17() {
    let data = repeated_log_lines(4 * 1024);
    for level in 13..=17 {
        let ours = compress_hinted(&data, CompressionLevel::Level(level));
        let c = zstd::bulk::compress(data.as_slice(), level).expect("C compress");
        assert!(
            ours.len() <= c.len(),
            "level {level}: ours {} > C {} (small-tier strategy must match C)",
            ours.len(),
            c.len()
        );
        assert_eq!(c_decode(&ours), data, "roundtrip level={level}");
    }
}
