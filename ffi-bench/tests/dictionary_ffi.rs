//! Cross-implementation conformance for dictionary compression: a frame we
//! compress with a finalized FastCOVER dictionary must decode through the C
//! `zstd` dictionary decoder. The dictionary build + compression happen in the
//! `structured_zstd::testing` facade (pure Rust); only the C decode lives here.
#![cfg(all(feature = "bench-internals", feature = "dict-builder"))]

use structured_zstd::testing::dict_roundtrip_fixture;

/// Repeated log-line fixture shared by the dictionary cross-decode tests: a
/// dict built from this structure matches an input drawn from it heavily, so the
/// encoder emits dict-reaching offsets the C decoder must accept.
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

/// Dictionary frames must decode through the C reference decoder across every
/// strategy backend (Fast / dfast / Row / HashChain / BT), on REUSED
/// compressors (so the resident dict re-borrow + retained-budget bookkeeping is
/// exercised), and at both attach-mode (< cutoff) and copy-mode (> cutoff)
/// input sizes. This is the cross-level guard for the dict prefix-floor and
/// resident-budget changes: any over-window offset would make the C decoder
/// reject the frame.
#[test]
fn dict_frames_decode_with_c_across_levels_and_reuse() {
    use structured_zstd::decoding::Dictionary;
    use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

    let dict = repeated_log_lines(8 * 1024);
    // Levels spanning every backend: 1 (Fast), 3 (dfast), 6 (Row/lazy),
    // 9 (HashChain/lazy2), 12 (HashChain), 19 (BT). Sizes straddle the attach
    // (< cutoff) / copy (> cutoff) split.
    let levels = [1i32, 3, 6, 9, 12, 19];
    let sizes = [1024usize, 16 * 1024];

    for &size in &sizes {
        let payload = repeated_log_lines(size);
        for &level in &levels {
            let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
            let dict_obj = Dictionary::from_raw_content(1, dict.clone()).expect("raw dict");
            cctx.set_dictionary_id_flag(false);
            cctx.set_dictionary(dict_obj).expect("attach dict");

            // Three reused frames so the second/third exercise the resident dict
            // re-borrow + retained-budget retire path on the dict-bearing backends.
            for frame_idx in 0..3 {
                let compressed = cctx.compress_independent_frame(&payload);
                let mut decoder = zstd::bulk::Decompressor::with_dictionary(dict.as_slice())
                    .expect("C decoder accepts raw-content dictionary");
                let mut decoded = Vec::with_capacity(payload.len());
                let written = decoder
                    .decompress_to_buffer(compressed.as_slice(), &mut decoded)
                    .unwrap_or_else(|e| {
                        panic!("C decode failed: level={level} size={size} frame={frame_idx}: {e}")
                    });
                assert_eq!(written, payload.len(), "level={level} size={size}");
                assert_eq!(
                    decoded, payload,
                    "level={level} size={size} frame={frame_idx}"
                );
            }
        }
    }
}

/// A dictionary frame on the optimal band must not come out LARGER than the
/// reference's, on input the dictionary describes well.
///
/// The parser walks a binary tree it fills lazily: after a search it jumps its
/// insert cursor to where the match it found ENDS, on the reasoning that the
/// positions inside a match are covered. That end is the end of the match in
/// the SOURCE (upstream `matchEndIdx = matchIndex + matchLength`,
/// zstd_opt.c:747-748 and :794-795), and for a candidate drawn from the
/// dictionary the source lies BEFORE the position being searched. Measuring the
/// jump from the searched position instead skips the positions in between, they
/// never enter the tree, and a later search finds an empty bucket where the
/// reference finds a long match. It only shows with a dictionary attached,
/// because only a dictionary candidate sits that far back.
///
/// Level 11 at 4 KiB is where it bites hardest: upstream resolves it to btopt
/// with `searchLog` 3, so a search gets eight candidates and cannot afford to
/// look in an empty bucket.
#[test]
fn dict_frames_on_the_optimal_band_are_no_larger_than_the_reference() {
    use structured_zstd::decoding::Dictionary;
    use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

    // The benchmark's `small-4k-log-lines` scenario, byte for byte: its lines
    // carry a region field the shorter fixture above does not, and which long
    // matches the dictionary offers is exactly what this pins.
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    let payload = {
        let mut bytes = Vec::with_capacity(4 * 1024);
        while bytes.len() < 4 * 1024 {
            for line in LINES {
                let remaining = (4 * 1024) - bytes.len();
                if remaining == 0 {
                    break;
                }
                bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
            }
        }
        bytes
    };
    // Trained the way the benchmark trains it: 256-byte samples, an eighth of
    // the input as the size request.
    let samples: Vec<&[u8]> = payload.chunks(256).collect();
    let dict = zstd::dict::from_samples(&samples, payload.len() / 8)
        .expect("dictionary should train from the log-line samples");

    for level in [10i32, 11, 12, 13] {
        let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(level));
        cctx.set_dictionary_id_flag(false);
        cctx.set_dictionary(
            Dictionary::from_serialized_or_raw_content(dict.as_slice()).expect("dictionary parses"),
        )
        .expect("attach dict");
        let mut reference = zstd::bulk::Compressor::with_dictionary(level, dict.as_slice())
            .expect("reference accepts the dictionary");
        let theirs = reference
            .compress(&payload)
            .expect("reference compresses the payload");

        // Three frames on the SAME compressor. A reused dictionary context
        // advances the history base between frames, so anything the parser
        // keeps in absolute coordinates has to carry that base: a value left
        // relative to the live history reads correctly on the first frame and
        // silently stops working on the second.
        for frame in 0..3 {
            cctx.set_source_size_hint(payload.len() as u64);
            let ours = cctx.compress_independent_frame(&payload);
            assert!(
                ours.len() <= theirs.len(),
                "level {level} frame {frame}: {} bytes against the reference's {}",
                ours.len(),
                theirs.len(),
            );
        }
    }
}
