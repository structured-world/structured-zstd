//! Cross-validation: structured-zstd ↔ C FFI zstd roundtrip integrity.
//!
//! Tests 1000 iterations in both directions:
//! - Pure Rust compress → C FFI decompress
//! - C FFI compress → Pure Rust decompress

use structured_zstd::decoding::StreamingDecoder;
use structured_zstd::encoding::{CompressionLevel, FrameCompressor, compress_to_vec};
use structured_zstd::io::Read;

/// Generate deterministic pseudo-random data using a simple LCG.
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

/// Generate data with limited alphabet for Huffman-friendly compression.
fn generate_huffman_friendly(seed: u64, len: usize, alphabet_size: u8) -> Vec<u8> {
    assert!(alphabet_size > 0, "alphabet_size must be non-zero");
    let mut state = seed;
    let mut data = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push(((state >> 33) as u8) % alphabet_size);
    }
    data
}

#[test]
fn cross_rust_compress_ffi_decompress_1000() {
    for i in 0..1000u64 {
        let len = (i * 89 % 16384) as usize;
        let data = generate_data(i, len);

        let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
        let result = zstd::decode_all(compressed.as_slice()).unwrap_or_else(|e| {
            panic!("rust→ffi decode failed at iteration {i}, len={len}: {e}");
        });
        assert_eq!(
            data, result,
            "rust→ffi roundtrip failed at iteration {i}, len={len}"
        );
    }
}

/// Regression for the rebase-style `reset`: a reused compressor advances
/// the absolute-position floor across independent frames instead of zeroing
/// the matcher tables. Every frame must still decode through C zstd, proving
/// the previous frame's stale table entries never leak into the next frame's
/// match decisions. Level 22 (optimal parser, binary-tree backend) is the
/// most sensitive path; the frames are sized into one source-size tier so the
/// matcher reuses the same tables and the floor-advance path (not a realloc)
/// is the one exercised.
#[test]
fn cross_rust_reused_compressor_level22_ffi_decompress() {
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(22));
    for i in 0..64u64 {
        // ~4 KiB, same tier each iteration; distinct, compressible content.
        let data = generate_huffman_friendly(i.wrapping_add(1), 4096, 24);
        let compressed = enc.compress_independent_frame(&data);
        let result = zstd::decode_all(compressed.as_slice()).unwrap_or_else(|e| {
            panic!("reused-compressor rust→ffi decode failed at frame {i}: {e}");
        });
        assert_eq!(
            data, result,
            "reused-compressor rust→ffi roundtrip failed at frame {i}"
        );
    }
}

/// Companion to the level-22 reuse test that also varies the frame size so
/// the source-size tier (and therefore the table dimensions) changes between
/// frames. This exercises the path where `reset` advances the floor but a
/// later `ensure_tables` reallocates the tables clean, alongside the pure
/// floor-advance path.
#[test]
fn cross_rust_reused_compressor_varied_sizes_ffi_decompress() {
    let mut enc: FrameCompressor = FrameCompressor::new(CompressionLevel::from_level(19));
    for i in 0..64u64 {
        let len = (1024 + (i * 1531) % 96_000) as usize;
        let data = generate_huffman_friendly(i.wrapping_add(7), len, 40);
        let compressed = enc.compress_independent_frame(&data);
        let result = zstd::decode_all(compressed.as_slice()).unwrap_or_else(|e| {
            panic!("varied-size reuse rust→ffi decode failed at frame {i}, len={len}: {e}");
        });
        assert_eq!(
            data, result,
            "varied-size reuse rust→ffi roundtrip failed at frame {i}, len={len}"
        );
    }
}

/// Regression for the hoisted hash/chain fill: a single long match over a
/// highly repetitive input drives `insert_positions` to fill an entire
/// block in one tight loop (the path that skips the per-position rebase
/// guard). Compress the repeated pattern at a lazy level and decode through
/// C zstd to prove the fast fill produces a valid, correct frame.
#[test]
fn cross_rust_repetitive_pattern_lazy_ffi_decompress() {
    let pattern = b"coordinode:segment:0001|tenant=demo|label=orders|";
    let mut data = Vec::with_capacity(1 << 20);
    while data.len() < (1 << 20) {
        let remaining = (1 << 20) - data.len();
        data.extend_from_slice(&pattern[..pattern.len().min(remaining)]);
    }
    for level in [6i32, 9, 12] {
        let compressed = compress_to_vec(&data[..], CompressionLevel::from_level(level));
        let result = zstd::decode_all(compressed.as_slice()).unwrap_or_else(|e| {
            panic!("repetitive lazy L{level} rust->ffi decode failed: {e}");
        });
        assert_eq!(
            data, result,
            "repetitive lazy L{level} rust->ffi roundtrip failed"
        );
    }
}

#[test]
fn cross_rust_fastest_with_source_hint_ffi_decompress_iteration_23() {
    let i = 23u64;
    let len = (i * 89 % 16384) as usize;
    let data = generate_data(i, len);

    let compressed = {
        let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
        compressor.set_source_size_hint(data.len() as u64);
        compressor.set_source(data.as_slice());
        let mut out = Vec::new();
        compressor.set_drain(&mut out);
        compressor.compress();
        out
    };

    let mut rust_decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut rust_result = Vec::new();
    rust_decoder.read_to_end(&mut rust_result).unwrap();
    assert_eq!(data, rust_result, "rust decoder must accept hinted stream");

    let result = zstd::decode_all(compressed.as_slice()).unwrap_or_else(|e| {
        panic!("hinted rust→ffi decode failed at iteration {i}, len={len}: {e}");
    });
    assert_eq!(data, result, "ffi decoder must accept hinted stream");
}

#[test]
fn cross_ffi_compress_rust_decompress_1000() {
    for i in 0..1000u64 {
        let len = (i * 89 % 16384) as usize;
        let data = generate_data(i.wrapping_add(0xBEEF), len);

        let compressed = zstd::encode_all(&data[..], 1).unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            data, result,
            "ffi→rust roundtrip failed at iteration {i}, len={len}"
        );
    }
}

/// Cross-validate large inputs (1KB–512KB) that produce large literal sections,
/// verifying C zstd can decompress what our encoder produces.
#[test]
fn cross_rust_compress_ffi_decompress_large_blocks() {
    let sizes = [1025, 16384, 65536, 128 * 1024];
    for (i, &size) in sizes.iter().enumerate() {
        let data = generate_huffman_friendly(i as u64 + 200, size, 48);

        let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
        let result = zstd::decode_all(compressed.as_slice()).unwrap();
        assert_eq!(
            data, result,
            "rust→ffi large block roundtrip failed at size={size}"
        );
    }

    // Multi-block: 512KB forces multiple blocks, each with large literals
    let data = generate_huffman_friendly(300, 512 * 1024, 48);
    let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust→ffi multi-block roundtrip failed");
}

/// Cross-validate C FFI compress → Rust decompress for large blocks.
#[test]
fn cross_ffi_compress_rust_decompress_large_blocks() {
    let sizes = [1025, 16384, 65536, 128 * 1024];
    for (i, &size) in sizes.iter().enumerate() {
        let data = generate_huffman_friendly(i as u64 + 400, size, 48);

        let compressed = zstd::encode_all(&data[..], 1).unwrap();
        let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            data, result,
            "ffi→rust large block roundtrip failed at size={size}"
        );
    }

    // Multi-block: 512KB
    let data = generate_huffman_friendly(500, 512 * 1024, 48);
    let compressed = zstd::encode_all(&data[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "ffi→rust multi-block roundtrip failed");
}

/// Cross-validate Rust compress (seed=100, 512KB) → C FFI decompress for the
/// same Huffman-heavy multi-block input used in roundtrip_multi_block_large_literals.
#[test]
fn cross_rust_compress_ffi_decompress_huffman_seed100() {
    let data = generate_huffman_friendly(100, 512 * 1024, 48);
    let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust→ffi seed=100 512KB roundtrip failed");
}

/// Cross-validate the same Huffman-heavy 512KB input in the opposite direction:
/// C FFI compress (seed=100) → Rust decompress.
#[test]
fn cross_ffi_compress_rust_decompress_huffman_seed100() {
    let data = generate_huffman_friendly(100, 512 * 1024, 48);
    let compressed = zstd::encode_all(&data[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "ffi→rust seed=100 512KB roundtrip failed");
}

/// Cross-validate repeat offset encoding: Rust compress → C FFI decompress.
/// Exercises repeat offset codes (1/2/3) and offset history across blocks.
#[test]
fn cross_rust_compress_ffi_decompress_repeat_offsets() {
    // Single-block: repeating pattern at fixed offset
    let pattern = b"ABCDE12345";
    let mut data = Vec::with_capacity(50_000);
    for _ in 0..5_000 {
        data.extend_from_slice(pattern);
    }
    let compressed = compress_to_vec(&data[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust→ffi repeat offset roundtrip failed");

    // Multi-block: 512KB with repeating patterns spanning block boundaries
    let mut multi_block = Vec::with_capacity(512 * 1024);
    while multi_block.len() < 512 * 1024 {
        multi_block.extend_from_slice(pattern);
    }
    multi_block.truncate(512 * 1024);
    let compressed = compress_to_vec(&multi_block[..], CompressionLevel::Fastest);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(
        multi_block, result,
        "rust→ffi multi-block repeat offset roundtrip failed"
    );
}

/// Cross-validate repeat-offset-friendly inputs in the opposite direction:
/// C FFI compress → Rust decompress.
#[test]
fn cross_ffi_compress_rust_decompress_repeat_offsets() {
    let pattern = b"ABCDE12345";

    let mut data = Vec::with_capacity(50_000);
    for _ in 0..5_000 {
        data.extend_from_slice(pattern);
    }
    let compressed = zstd::encode_all(&data[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(data, result, "ffi→rust repeat offset roundtrip failed");

    let mut multi_block = Vec::with_capacity(512 * 1024);
    while multi_block.len() < 512 * 1024 {
        multi_block.extend_from_slice(pattern);
    }
    multi_block.truncate(512 * 1024);
    let compressed = zstd::encode_all(&multi_block[..], 1).unwrap();
    let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
    let mut result = Vec::new();
    decoder.read_to_end(&mut result).unwrap();
    assert_eq!(
        multi_block, result,
        "ffi→rust multi-block repeat offset roundtrip failed"
    );
}

#[test]
fn cross_rust_default_compress_ffi_decompress_regression() {
    let data = generate_huffman_friendly(900, 64 * 1024, 32);
    let compressed = compress_to_vec(&data[..], CompressionLevel::Default);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(data, result, "rust default→ffi roundtrip failed");
}

#[test]
fn default_level_beats_fastest_on_corpus_proxy() {
    // Keep this strict: issue #5 requires Default to be a real step up from Fastest,
    // not just an alias that happens to roundtrip.
    let data = include_bytes!("../decodecorpus_files/z000033");
    let fastest = compress_to_vec(data.as_slice(), CompressionLevel::Fastest);
    let default = compress_to_vec(data.as_slice(), CompressionLevel::Default);

    assert!(
        default.len() < fastest.len(),
        "Default should compress better than Fastest on corpus proxy. default={} fastest={}",
        default.len(),
        fastest.len()
    );
}

#[test]
fn default_level_stays_within_twenty_five_percent_of_ffi_level3_on_corpus_proxy() {
    // Performance-first phase: keep only a broad ratio sanity guard so
    // throughput-focused Dfast iterations are not blocked by tight ratio parity.
    let data = include_bytes!("../decodecorpus_files/z000033");
    let default = compress_to_vec(data.as_slice(), CompressionLevel::Default);
    let ffi_level3 = zstd::encode_all(data.as_slice(), 3).unwrap();

    assert!(
        (default.len() as u64) * 4 <= (ffi_level3.len() as u64) * 5,
        "Default should stay within 25% of zstd level 3 on corpus proxy. default={} ffi_l3={}",
        default.len(),
        ffi_level3.len()
    );
}

#[test]
fn cross_rust_better_compress_ffi_decompress_regression() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let compressed = compress_to_vec(data.as_slice(), CompressionLevel::Better);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(
        data.as_slice(),
        result.as_slice(),
        "rust better→ffi roundtrip failed"
    );
}

/// Verify that Better compresses better than Default on the corpus proxy.
/// The hash-chain matcher with lazy2 should find longer matches than Dfast on
/// this reference input.
#[test]
fn better_level_beats_default_on_corpus_proxy() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let default = compress_to_vec(data.as_slice(), CompressionLevel::Default);
    let better = compress_to_vec(data.as_slice(), CompressionLevel::Better);

    assert!(
        better.len() < default.len(),
        "Better should compress better than Default on corpus proxy. better={} default={}",
        better.len(),
        default.len()
    );
}

#[test]
fn cross_rust_best_compress_ffi_decompress_regression() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let compressed = compress_to_vec(data.as_slice(), CompressionLevel::Best);
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(
        data.as_slice(),
        result.as_slice(),
        "rust best→ffi roundtrip failed"
    );
}

/// Verify that Best compresses strictly better than Better on the corpus proxy.
/// Deeper search and larger tables should find longer matches.
#[test]
fn best_level_beats_better_on_corpus_proxy() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let better = compress_to_vec(data.as_slice(), CompressionLevel::Better);
    let best = compress_to_vec(data.as_slice(), CompressionLevel::Best);

    assert!(
        best.len() < better.len(),
        "Best should compress strictly better than Better on corpus proxy. best={} better={}",
        best.len(),
        better.len()
    );
}

#[test]
fn cross_rust_level22_compress_ffi_decompress_regression() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let compressed = compress_to_vec(data.as_slice(), CompressionLevel::Level(22));
    let result = zstd::decode_all(compressed.as_slice()).unwrap();
    assert_eq!(
        data.as_slice(),
        result.as_slice(),
        "rust level22→ffi roundtrip failed"
    );
}

#[test]
fn level22_beats_best_on_corpus_proxy() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let best = compress_to_vec(data.as_slice(), CompressionLevel::Best);
    let level22 = compress_to_vec(data.as_slice(), CompressionLevel::Level(22));
    assert!(
        level22.len() <= best.len(),
        "Level(22) should not be worse than Best on corpus proxy. level22={} best={}",
        level22.len(),
        best.len()
    );
}

#[test]
fn level22_stays_within_ffi_level22_on_corpus_proxy() {
    let data = include_bytes!("../decodecorpus_files/z000033");
    let ffi_level22 = zstd::encode_all(data.as_slice(), 22).unwrap();
    let level22 = compress_to_vec(data.as_slice(), CompressionLevel::Level(22));
    assert!(
        level22.len() <= ffi_level22.len(),
        "Rust Level(22) should not be worse than upstream zstd level 22 on corpus proxy. rust_level22={} ffi_level22={}",
        level22.len(),
        ffi_level22.len()
    );
}

/// RLE-mode sequence tables: C zstd emits Compression_Mode = RLE for an
/// LL/ML/OF axis when a block's sequences all share one code (uniform /
/// highly-repetitive data). This exercises the fused RLE decode path —
/// the degenerate 1-state FSE table built by `FSETableImpl::build_rle`.
/// All-same-byte input is the canonical RLE producer: C encodes it as a
/// single long match, so every sequence axis has exactly one code and
/// C switches the table to RLE mode. A wrong RLE table build or a wrong
/// 1-state decode would corrupt the output here.
#[test]
fn cross_ffi_compress_rust_decompress_rle_mode_tables() {
    let mut period4: Vec<u8> = Vec::with_capacity(8192);
    while period4.len() < 8192 {
        period4.extend_from_slice(b"abcd");
    }
    let mut two_runs: Vec<u8> = vec![0x11u8; 4096];
    two_runs.extend_from_slice(&[0x22u8; 4096]);

    // Periodic unit = a fixed 15-byte pattern that matches the previous
    // unit (offset 16) + one varying literal byte. Every unit yields the
    // SAME (lit_len, match_len, offset) sequence, so the block has many
    // sequences all sharing one code per axis → RLE-mode sequence tables
    // with MULTI-sequence blocks (exercises update_state transitions on
    // the 1-state table, unlike the single-match inputs above).
    // 9000 units * 16 B = 144 KiB → spans more than one 128 KiB block,
    // every block RLE-mode.
    let mut periodic: Vec<u8> = Vec::with_capacity(16 * 9000);
    for i in 0..9000u32 {
        periodic.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        periodic.push((i & 0xFF) as u8);
    }
    // RLE blocks followed by FSE blocks in ONE frame: the scratch FSE
    // tables are reused across blocks, so a stale RLE-table field leaking
    // into the next FSE-mode block's build is caught here (regression for
    // the max_symbol-clobber bug).
    let mut mixed: Vec<u8> = periodic.clone();
    mixed.extend_from_slice(include_bytes!("../decodecorpus_files/z000033"));

    let inputs: Vec<Vec<u8>> = vec![
        periodic.clone(),   // periodic units → multi-sequence RLE tables
        mixed,              // RLE blocks → FSE blocks in one frame
        vec![0x5Au8; 4096], // all-same byte → single long match → RLE axes
        vec![0u8; 70_000],  // multi-block uniform
        period4,            // period-4 repeat
        two_runs,           // two long single-byte runs
        // Decode corpus at negative/low levels produces MULTI-sequence
        // RLE blocks (many sequences sharing one code), so the 1-state
        // table's `update_state` IS exercised between sequences — the
        // single-sequence inputs above leave that transition untested.
        include_bytes!("../decodecorpus_files/z000033").to_vec(),
    ];
    for level in [-6i32, -1, 1, 3, 9, 19] {
        for (idx, data) in inputs.iter().enumerate() {
            let compressed = zstd::encode_all(&data[..], level).unwrap();
            let mut decoder = StreamingDecoder::new(compressed.as_slice()).unwrap();
            let mut result = Vec::new();
            decoder.read_to_end(&mut result).unwrap();
            // `assert_eq!(*data, result, ...)` borrows both operands
            // (`match (&*data, &result)`), so dereferencing the
            // `&Vec<u8>` from `inputs.iter()` does not move out of it.
            assert_eq!(
                *data, result,
                "RLE-mode ffi→rust decode mismatch (input {idx}, level {level})",
            );
        }
    }
}

/// Every level, both directions, on inputs that exercise different matcher
/// paths. The per-level tests above cover a handful of levels each and the
/// `ffi→rust` direction is otherwise almost entirely `Fastest`, so a change to
/// one strategy's matcher could ship without any level-matched interop check.
/// Byte-identity against an earlier build proves we did not change; this proves
/// the two implementations still agree.
#[test]
fn cross_every_level_roundtrips_through_the_c_codec_both_ways() {
    // Random resists matching, the repeating pattern feeds the match finders,
    // and the small alphabet drives the entropy stage.
    let random = generate_data(0xA11CE, 200 * 1024);
    let repetitive: Vec<u8> = generate_data(0xB0B, 4096)
        .iter()
        .cycle()
        .take(200 * 1024)
        .copied()
        .collect();
    let low_entropy = generate_huffman_friendly(0xC0FFEE, 200 * 1024, 12);
    let fixtures: [(&str, &[u8]); 3] = [
        ("random", &random),
        ("repetitive", &repetitive),
        ("low-entropy", &low_entropy),
    ];

    for (name, data) in fixtures {
        for level in 1i32..=22 {
            let ours = compress_to_vec(data, CompressionLevel::from_level(level));
            let theirs_decoded = zstd::decode_all(ours.as_slice())
                .unwrap_or_else(|e| panic!("C decoder rejected our level {level} {name}: {e}"));
            assert_eq!(
                data, theirs_decoded,
                "rust→ffi mismatch at level {level} on {name}"
            );

            let theirs = zstd::encode_all(data, level).unwrap();
            let mut decoder = StreamingDecoder::new(theirs.as_slice()).unwrap();
            let mut ours_decoded = Vec::with_capacity(data.len());
            decoder
                .read_to_end(&mut ours_decoded)
                .unwrap_or_else(|e| panic!("our decoder rejected C level {level} {name}: {e}"));
            assert_eq!(
                data, ours_decoded,
                "ffi→rust mismatch at level {level} on {name}"
            );
        }
    }
}

/// The same ladder crossed with the two features that change what the matcher
/// is allowed to reference: long-distance matching (which admits matches far
/// outside the ordinary window) and a dictionary (which seeds the window with
/// bytes the frame never contains). Both alter the offsets we emit, so both
/// need the level-matched interop check the plain ladder gives.
#[test]
fn cross_every_level_with_ldm_and_dictionary_roundtrips_both_ways() {
    // Long-range repeats: the pattern recurs at a distance only LDM reaches,
    // and the dictionary bytes are drawn from the same alphabet so dictionary
    // matches actually fire.
    let period = generate_huffman_friendly(0x5EED, 64 * 1024, 24);
    let data: Vec<u8> = period.iter().cycle().take(1024 * 1024).copied().collect();
    let dict_raw = include_bytes!("../dict_tests/dictionary");

    // Guards against the LDM arm silently degenerating into the plain one: if
    // the flag ever stopped reaching the encoder, every pair below would match
    // and this matrix would be testing the same thing twice.
    let mut sizes: Vec<usize> = Vec::new();
    let mut plain_idx = 0usize;
    let mut ldm_changed_something = false;

    for ldm in [false, true] {
        for level in 1i32..=22 {
            for with_dict in [false, true] {
                let label = format!("level {level}, ldm={ldm}, dict={with_dict}");

                // Ours → C.
                let mut compressor: FrameCompressor<&[u8], &mut Vec<u8>> =
                    FrameCompressor::new(CompressionLevel::from_level(level));
                if ldm {
                    let params = structured_zstd::encoding::CompressionParameters::builder(
                        CompressionLevel::from_level(level),
                    )
                    .enable_long_distance_matching(true)
                    .build()
                    .expect("parameters within bounds");
                    compressor.set_parameters(&params);
                }
                if with_dict {
                    compressor
                        .set_dictionary_from_bytes(dict_raw)
                        .expect("dictionary bytes should parse");
                }
                let mut ours = Vec::new();
                compressor.set_source(data.as_slice());
                compressor.set_drain(&mut ours);
                compressor.compress();

                let theirs_decoded = if with_dict {
                    let mut d = zstd::bulk::Decompressor::with_dictionary(dict_raw).unwrap();
                    d.decompress(ours.as_slice(), data.len())
                        .unwrap_or_else(|e| panic!("C decoder rejected our output at {label}: {e}"))
                } else {
                    zstd::decode_all(ours.as_slice())
                        .unwrap_or_else(|e| panic!("C decoder rejected our output at {label}: {e}"))
                };
                assert_eq!(data, theirs_decoded, "rust→ffi mismatch at {label}");
                if ldm {
                    // Same traversal order as the plain pass, so the counter
                    // lines the two up entry for entry.
                    ldm_changed_something |= ours.len() != sizes[plain_idx];
                    plain_idx += 1;
                } else {
                    sizes.push(ours.len());
                }

                // C → ours.
                let mut c_enc = if with_dict {
                    zstd::bulk::Compressor::with_dictionary(level, dict_raw).unwrap()
                } else {
                    zstd::bulk::Compressor::new(level).unwrap()
                };
                if ldm {
                    c_enc
                        .set_parameter(zstd::zstd_safe::CParameter::EnableLongDistanceMatching(
                            true,
                        ))
                        .unwrap();
                }
                let theirs = c_enc.compress(data.as_slice()).unwrap();

                let mut decoder = structured_zstd::decoding::FrameDecoder::new();
                if with_dict {
                    decoder
                        .add_dict(
                            structured_zstd::decoding::Dictionary::decode_dict(dict_raw).unwrap(),
                        )
                        .unwrap();
                }
                let mut ours_decoded = Vec::with_capacity(data.len());
                decoder
                    .decode_all_to_vec(theirs.as_slice(), &mut ours_decoded)
                    .unwrap_or_else(|e| panic!("our decoder rejected C output at {label}: {e:?}"));
                assert_eq!(data, ours_decoded, "ffi→rust mismatch at {label}");
            }
        }
    }

    assert!(
        ldm_changed_something,
        "long-distance matching never changed the output, so the LDM half of \
         this matrix was not exercising anything"
    );
}
