// `format!` is used by ungated tests (e.g. the btlazy2 dict-reuse
// byte-identity test), so the import must not be feature-gated — under
// default features (no `dict_builder`) the gated form left `format!`
// unresolved when the test module is compiled.
use alloc::format;
use alloc::vec;

use super::FrameCompressor;
use crate::common::{MAGIC_NUM, MAX_BLOCK_SIZE};
use crate::decoding::FrameDecoder;
use crate::encoding::{Matcher, Sequence};
use alloc::vec::Vec;

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

// Cross-implementation parity tests (compress here, decode through the C
// bindings) moved to `ffi-bench/tests/frame_compressor_ffi.rs` so the
// library crate never links libzstd.

struct NoDictionaryMatcher {
    last_space: Vec<u8>,
    window_size: u64,
}

impl NoDictionaryMatcher {
    fn new(window_size: u64) -> Self {
        Self {
            last_space: Vec::new(),
            window_size,
        }
    }
}

impl Matcher for NoDictionaryMatcher {
    fn get_next_space(&mut self) -> Vec<u8> {
        vec![0; self.window_size as usize]
    }

    fn get_last_space(&mut self) -> &[u8] {
        self.last_space.as_slice()
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        self.last_space = space;
    }

    fn skip_matching(&mut self) {}

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        handle_sequence(Sequence::Literals {
            literals: self.last_space.as_slice(),
        });
    }

    fn reset(&mut self, _level: super::CompressionLevel) {
        self.last_space.clear();
    }

    fn window_size(&self) -> u64 {
        self.window_size
    }
}

#[test]
fn frame_starts_with_magic_num() {
    let mock_data = [1_u8, 2, 3].as_slice();
    let mut output: Vec<u8> = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
    compressor.set_source(mock_data);
    compressor.set_drain(&mut output);

    compressor.compress();
    assert!(output.starts_with(&MAGIC_NUM.to_le_bytes()));
}

#[test]
fn very_simple_raw_compress() {
    let mock_data = [1_u8, 2, 3].as_slice();
    let mut output: Vec<u8> = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
    compressor.set_source(mock_data);
    compressor.set_drain(&mut output);

    compressor.compress();
}

#[test]
fn very_simple_compress() {
    let mut mock_data = vec![0; 1 << 17];
    mock_data.extend(vec![1; (1 << 17) - 1]);
    mock_data.extend(vec![2; (1 << 18) - 1]);
    mock_data.extend(vec![2; 1 << 17]);
    mock_data.extend(vec![3; (1 << 17) - 1]);
    let mut output: Vec<u8> = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
    compressor.set_source(mock_data.as_slice());
    compressor.set_drain(&mut output);

    compressor.compress();

    let mut decoder = FrameDecoder::new();
    let mut decoded = Vec::with_capacity(mock_data.len());
    decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
    assert_eq!(mock_data, decoded);
}

#[test]
fn rle_compress() {
    let mock_data = vec![0; 1 << 19];
    let mut output: Vec<u8> = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
    compressor.set_source(mock_data.as_slice());
    compressor.set_drain(&mut output);

    compressor.compress();

    let mut decoder = FrameDecoder::new();
    let mut decoded = Vec::with_capacity(mock_data.len());
    decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
    assert_eq!(mock_data, decoded);
}

#[test]
fn aaa_compress() {
    let mock_data = vec![0, 1, 3, 4, 5];
    let mut output: Vec<u8> = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
    compressor.set_source(mock_data.as_slice());
    compressor.set_drain(&mut output);

    compressor.compress();

    let mut decoder = FrameDecoder::new();
    let mut decoded = Vec::with_capacity(mock_data.len());
    decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
    assert_eq!(mock_data, decoded);
}

#[test]
fn dictionary_compression_sets_required_dict_id_and_roundtrips() {
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let dict_for_encoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
    let dict_for_decoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();

    let mut data = Vec::new();
    for _ in 0..8 {
        data.extend_from_slice(&dict_for_decoder.dict_content[..2048]);
    }

    let mut with_dict = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let previous = compressor
        .set_dictionary_from_bytes(dict_raw)
        .expect("dictionary bytes should parse");
    assert!(
        previous.is_none(),
        "first dictionary insert should return None"
    );
    assert_eq!(
        compressor
            .set_dictionary(dict_for_encoder)
            .expect("valid dictionary should attach")
            .expect("set_dictionary_from_bytes inserted previous dictionary")
            .id(),
        dict_for_decoder.id
    );
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut with_dict);
    compressor.compress();

    let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
        .expect("encoded stream should have a frame header");
    assert_eq!(frame_header.dictionary_id(), Some(dict_for_decoder.id));

    let mut decoder = FrameDecoder::new();
    let mut missing_dict_target = Vec::with_capacity(data.len());
    let err = decoder
        .decode_all_to_vec(&with_dict, &mut missing_dict_target)
        .unwrap_err();
    assert!(
        matches!(
            &err,
            crate::decoding::errors::FrameDecoderError::DictNotProvided { .. }
        ),
        "dict-compressed stream should require dictionary id, got: {err:?}"
    );

    let mut decoder = FrameDecoder::new();
    decoder.add_dict(dict_for_decoder).unwrap();
    let mut decoded = Vec::with_capacity(data.len());
    decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
    assert_eq!(decoded, data);
}

#[cfg(all(feature = "dict_builder", feature = "std"))]
#[test]
fn dictionary_compression_roundtrips_with_dict_builder_dictionary() {
    use std::io::Cursor;

    let mut training = Vec::new();
    for idx in 0..256u32 {
        training.extend_from_slice(
            format!("tenant=demo table=orders key={idx} region=eu\n").as_bytes(),
        );
    }
    let mut raw_dict = Vec::new();
    crate::dictionary::create_raw_dict_from_source(
        Cursor::new(training.as_slice()),
        training.len(),
        &mut raw_dict,
        4096,
    )
    .expect("dict_builder training should succeed");
    assert!(
        !raw_dict.is_empty(),
        "dict_builder produced an empty dictionary"
    );

    let dict_id = 0xD1C7_0008;
    let encoder_dict =
        crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();
    let decoder_dict =
        crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();

    // Payload that the trained dict actually covers (same line shape as the
    // training corpus, just unseen `idx` values). The dict primes the whole
    // `tenant=demo table=orders key=… region=eu` line, so its benefit is the
    // first occurrence's literals — substantial and unambiguous — rather than
    // the 24-byte shared prefix of an otherwise-different payload, where the
    // marginal gain is below the dict-id frame overhead. (The deeper
    // partial-match dict ratio gap is tracked separately, not gated here.)
    let mut payload = Vec::new();
    for idx in 1000..1096u32 {
        payload.extend_from_slice(
            format!("tenant=demo table=orders key={idx} region=eu\n").as_bytes(),
        );
    }

    let mut without_dict = Vec::new();
    let mut baseline = FrameCompressor::new(super::CompressionLevel::Fastest);
    baseline.set_source(payload.as_slice());
    baseline.set_drain(&mut without_dict);
    baseline.compress();

    let mut with_dict = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    compressor
        .set_dictionary(encoder_dict)
        .expect("valid dict_builder dictionary should attach");
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut with_dict);
    compressor.compress();

    let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
        .expect("encoded stream should have a frame header");
    assert_eq!(frame_header.dictionary_id(), Some(dict_id));
    let mut decoder = FrameDecoder::new();
    decoder.add_dict(decoder_dict).unwrap();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
    assert_eq!(decoded, payload);
    assert!(
        with_dict.len() < without_dict.len(),
        "trained dictionary should improve compression for this small payload (with_dict={}, without_dict={})",
        with_dict.len(),
        without_dict.len(),
    );
}

#[test]
fn set_dictionary_from_bytes_seeds_entropy_tables_for_first_block() {
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let mut output = Vec::new();
    let input = b"";

    let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let previous = compressor
        .set_dictionary_from_bytes(dict_raw)
        .expect("dictionary bytes should parse");
    assert!(previous.is_none());

    compressor.set_source(input.as_slice());
    compressor.set_drain(&mut output);
    compressor.compress();

    assert!(
        compressor.state.last_huff_table.is_some(),
        "dictionary entropy should seed previous huffman table before first block"
    );
    assert!(
        compressor.state.fse_tables.ll_previous.is_some(),
        "dictionary entropy should seed previous ll table before first block"
    );
    assert!(
        compressor.state.fse_tables.ml_previous.is_some(),
        "dictionary entropy should seed previous ml table before first block"
    );
    assert!(
        compressor.state.fse_tables.of_previous.is_some(),
        "dictionary entropy should seed previous of table before first block"
    );
}

// `set_content_size_flag(false)`: the header must omit the FCS field
// (and the single-segment layout that requires it) while the frame
// still round-trips through our decoder.
#[test]
fn content_size_flag_off_omits_fcs_and_roundtrips() {
    let payload = alloc::vec![0x42u8; 4096];

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let mut with_fcs = Vec::new();
    compressor.compress_independent_frame_into(&payload, &mut with_fcs);

    compressor.set_content_size_flag(false);
    let mut without_fcs = Vec::new();
    compressor.compress_independent_frame_into(&payload, &mut without_fcs);

    let parsed_with = crate::decoding::frame::read_frame_header(with_fcs.as_slice())
        .expect("flag-on frame header must parse")
        .0;
    assert_eq!(parsed_with.frame_content_size(), 4096);

    let parsed_without = crate::decoding::frame::read_frame_header(without_fcs.as_slice())
        .expect("flag-off frame header must parse")
        .0;
    // 0 is the decoder's "unknown content size" sentinel...
    assert_eq!(
        parsed_without.frame_content_size(),
        0,
        "FCS must be omitted with the content-size flag off"
    );
    // ...and the descriptor must confirm the field is ABSENT (0 bytes),
    // not present with an explicit zero value.
    assert_eq!(
        parsed_without
            .descriptor
            .frame_content_size_bytes()
            .expect("descriptor must parse"),
        0,
        "the FCS field itself must be omitted, not written as zero"
    );

    let mut decoder = crate::decoding::FrameDecoder::new();
    // `decode_all_to_vec` fills existing capacity (no FCS to pre-size
    // from with the flag off), so reserve the expected payload upfront.
    let mut decoded = Vec::with_capacity(payload.len() + 64);
    decoder
        .decode_all_to_vec(&without_fcs, &mut decoded)
        .expect("flag-off frame must decode");
    assert_eq!(decoded, payload);
}

// `set_dictionary_id_flag(false)`: a dict-compressed frame must omit
// the dictionary ID and still decode when the dictionary is handed to
// the decoder explicitly.
#[test]
fn dict_id_flag_off_omits_dictionary_id_and_roundtrips() {
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let payload = b"dictionary-keyed payload dictionary-keyed payload".repeat(8);

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    compressor
        .set_dictionary_from_bytes(dict_raw)
        .expect("dictionary bytes should parse");
    compressor.set_dictionary_id_flag(false);
    let mut frame = Vec::new();
    compressor.compress_independent_frame_into(&payload, &mut frame);

    let parsed = crate::decoding::frame::read_frame_header(frame.as_slice())
        .expect("frame header must parse")
        .0;
    assert_eq!(
        parsed.dictionary_id(),
        None,
        "dictionary id must be omitted with the dict-id flag off"
    );

    // With the ID omitted the decoder cannot look the dictionary up by
    // header; hand it explicitly (the `reset_with_dict_handle` path).
    let mut sd =
        crate::decoding::StreamingDecoder::new_with_dictionary_bytes(frame.as_slice(), dict_raw)
            .expect("decoder must accept the dictionary");
    let mut dec = Vec::new();
    std::io::Read::read_to_end(&mut sd, &mut dec)
        .expect("frame must decode with the dictionary handed explicitly");
    assert_eq!(dec, payload);
}

// The output reservation must track the observed compression ratio, not
// the whole-input `compress_bound`: a multi-MiB compressible stream's
// output buffer stays at output scale (the old up-front bound held an
// input-sized allocation for the whole frame). Incompressible input may
// still re-estimate to ~the full bound — that is the genuine worst case.
#[test]
fn compressible_stream_output_capacity_stays_at_output_scale() {
    // 4 MiB of highly repetitive log-like lines.
    let line = b"ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo\n";
    let mut input = Vec::with_capacity(4 << 20);
    while input.len() < (4 << 20) {
        let take = line.len().min((4 << 20) - input.len());
        input.extend_from_slice(&line[..take]);
    }

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let mut out = Vec::new();
    compressor.compress_independent_frame_into(&input, &mut out);

    assert!(!out.is_empty());
    assert!(
        out.capacity() < input.len() / 4,
        "capacity {} must stay at output scale (input {}, output {})",
        out.capacity(),
        input.len(),
        out.len()
    );

    // Round-trip: the adaptive reservation must not affect the bytes.
    let mut decoder = crate::decoding::FrameDecoder::new();
    let mut decoded = Vec::with_capacity(input.len() + 64);
    decoder
        .decode_all_to_vec(&out, &mut decoded)
        .expect("frame must decode");
    assert_eq!(decoded, input);
}

// A dictionary frame with a known content size that fits the window
// must take the single-segment layout (reference parity): the
// dictionary is decoder setup state, not part of the regenerated
// segment, so it must not force the windowed multi-segment layout.
#[test]
fn dict_frame_with_known_size_is_single_segment() {
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let payload = b"dictionary-keyed payload dictionary-keyed payload".repeat(64);

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    compressor
        .set_dictionary_from_bytes(dict_raw)
        .expect("dictionary bytes should parse");
    let mut frame = Vec::new();
    compressor.compress_independent_frame_into(&payload, &mut frame);

    let parsed = crate::decoding::frame::read_frame_header(frame.as_slice())
        .expect("frame header must parse")
        .0;
    assert!(
        parsed.descriptor.single_segment_flag(),
        "dict frame with known size <= window must be single-segment"
    );
    assert!(parsed.dictionary_id().is_some());
    assert_eq!(parsed.frame_content_size(), payload.len() as u64);

    // Round-trip through our own decoder with the dictionary.
    let mut decoder = crate::decoding::FrameDecoder::new();
    decoder
        .add_dict_from_bytes(dict_raw)
        .expect("decoder must accept the dictionary");
    let mut decoded = Vec::with_capacity(payload.len() + 64);
    decoder
        .decode_all_to_vec(&frame, &mut decoded)
        .expect("single-segment dict frame must decode");
    assert_eq!(decoded, payload);
}

// Regression: after `clear_dictionary()` a reused compressor must fully
// deactivate the dictionary state. The dict-active matcher reset rewinds the
// hash/chain tables to the origin (`position_base = 0`) and DEFERS the table
// clear to the next prime/restore. If `clear_dictionary` leaves the matcher
// marked dictionary-active, a subsequent NO-dictionary frame hits that
// deferred-clear branch but never runs prime/restore, so stale dict-region
// entries (old absolute positions) survive at the rewound base and can
// surface as bogus matches. The no-dict frame must still round-trip without
// the dictionary. Uses Level(16) (btopt → HashChain backend, whose storage
// owns the deferred-clear path).
#[test]
fn clear_dictionary_then_nodict_frame_roundtrips() {
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    // Payload B embeds dictionary bytes up front so a surviving dict-region
    // chain entry would be a tempting (wrong) match candidate.
    let mut payload_b = Vec::new();
    payload_b.extend_from_slice(&dict_raw[..dict_raw.len().min(2048)]);
    payload_b.extend_from_slice(
        b"no-dictionary tail no-dictionary tail"
            .repeat(16)
            .as_slice(),
    );

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(16));
    compressor
        .set_dictionary_from_bytes(dict_raw)
        .expect("dictionary bytes should parse");
    // Frame A primes the dictionary (sets the matcher dictionary-active).
    let mut frame_a = Vec::new();
    compressor.compress_independent_frame_into(
        b"dictionary-keyed payload dictionary-keyed payload"
            .repeat(8)
            .as_slice(),
        &mut frame_a,
    );
    // Remove the dictionary, then compress a no-dictionary frame on the
    // SAME reused compressor.
    compressor.clear_dictionary();
    let mut frame_b = Vec::new();
    compressor.compress_independent_frame_into(&payload_b, &mut frame_b);

    // Frame B must decode WITHOUT any dictionary.
    let mut decoder = crate::decoding::FrameDecoder::new();
    let mut decoded = Vec::with_capacity(payload_b.len() + 64);
    decoder
        .decode_all_to_vec(&frame_b, &mut decoded)
        .expect("no-dict frame after clear_dictionary must decode");
    assert_eq!(
        decoded, payload_b,
        "no-dict frame after clear_dictionary must round-trip exactly"
    );
}

// Regression test: `heap_size()` must count the retained Huffman tables
// (the active `last_huff_table` and the recycled `huff_table_spare`).
// A reused context that parks a table would otherwise under-report its
// footprint through the public size API.
#[test]
fn heap_size_counts_active_and_spare_huffman_tables() {
    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let base = compressor.heap_size();

    let active = crate::huff0::huff0_encoder::HuffmanTable::build_from_data(
        b"abacabadabacabaeabacabadabacaba",
    );
    let active_bytes = active.heap_size();
    assert!(active_bytes > 0, "built table must own heap buffers");
    compressor.state.last_huff_table = Some(active);
    assert_eq!(
        compressor.heap_size(),
        base + active_bytes,
        "heap_size must include the active last_huff_table"
    );

    let spare = crate::huff0::huff0_encoder::HuffmanTable::build_from_data(
        b"the quick brown fox jumps over the lazy dog",
    );
    let spare_bytes = spare.heap_size();
    assert!(spare_bytes > 0, "built table must own heap buffers");
    compressor.state.huff_table_spare = Some(spare);
    assert_eq!(
        compressor.heap_size(),
        base + active_bytes + spare_bytes,
        "heap_size must include the parked huff_table_spare"
    );
}

#[test]
fn set_encoder_dictionary_reattaches_prepared_dict_without_reparse() {
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let payload = b"tenant=demo table=orders op=put key=1 value=aaaaabbbbbcccccdddddeeeee\n\
              tenant=demo table=orders op=put key=2 value=aaaaabbbbbcccccdddddeeeee\n";

    // Prepare the EncoderDictionary once, then attach it via the prepared-
    // dictionary API (no raw-blob reparse at attach time).
    let prepared = super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
    let dict_id = prepared.id();

    let mut with_dict = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let previous = compressor
        .set_encoder_dictionary(prepared)
        .expect("prepared dictionary should attach");
    assert!(previous.is_none());
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut with_dict);
    compressor.compress();
    // clear_dictionary hands the prepared dictionary back (last use of
    // `compressor`, so its `&mut with_dict` drain borrow ends here).
    let returned = compressor
        .clear_dictionary()
        .expect("dictionary was attached");
    assert_eq!(returned.id(), dict_id);

    // The reattached dictionary drives the frame: its id is advertised and
    // the stream round-trips through a decoder primed with the same dict.
    let (frame_header, _) = crate::decoding::frame::read_frame_header(with_dict.as_slice())
        .expect("encoded stream should have a frame header");
    assert_eq!(frame_header.dictionary_id(), Some(dict_id));
    let decoder_dict = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
    let mut decoder = FrameDecoder::new();
    decoder.add_dict(decoder_dict).unwrap();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
    assert_eq!(decoded.as_slice(), payload.as_slice());

    // The dictionary handed back by clear_dictionary reattaches to another
    // compressor without touching the raw bytes again, producing an
    // identical frame.
    let mut with_dict2 = Vec::new();
    let mut compressor2 = FrameCompressor::new(super::CompressionLevel::Fastest);
    compressor2
        .set_encoder_dictionary(returned)
        .expect("returned dictionary should reattach");
    compressor2.set_source(payload.as_slice());
    compressor2.set_drain(&mut with_dict2);
    compressor2.compress();
    assert_eq!(
        with_dict2, with_dict,
        "reattached prepared dict must produce an identical frame"
    );
}

#[test]
fn dict_primed_matcher_snapshot_reused_across_frames_is_byte_identical() {
    // CDict-equivalent: a compressor reused across frames with the same
    // dictionary restores the primed matcher snapshot on frames 2..N
    // (a table copy) instead of re-hashing the dictionary. The restored
    // state must reproduce the first-frame (freshly-primed) output
    // byte-for-byte, and every frame must round-trip through a
    // dict-primed decoder.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    // Source must exceed the Fast strategy's 8 KiB attach cutoff so the
    // copy-snapshot (restore) path is taken on frame 2 — at or below the
    // cutoff the upstream zstd attaches by reference and we fall back to re-prime,
    // which would not exercise restore.
    let mut payload = Vec::new();
    while payload.len() < 16 * 1024 {
        payload.extend_from_slice(
            b"tenant=demo table=orders op=put key=1 value=aaaaabbbbbcccccdddddeeeee\n",
        );
    }

    let prepared = super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
    let dict_id = prepared.id();
    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    compressor
        .set_encoder_dictionary(prepared)
        .expect("prepared dictionary should attach");

    // Frame 1 primes + captures the snapshot; frame 2 restores it.
    let frame1 = compressor.compress_independent_frame(payload.as_slice());
    let frame2 = compressor.compress_independent_frame(payload.as_slice());
    assert_eq!(
        frame1, frame2,
        "restored prime snapshot must reproduce the freshly-primed frame byte-for-byte"
    );

    // Both frames advertise the dict id and round-trip through a
    // dict-primed decoder.
    for frame in [&frame1, &frame2] {
        let (hdr, _) =
            crate::decoding::frame::read_frame_header(frame.as_slice()).expect("frame header");
        assert_eq!(hdr.dictionary_id(), Some(dict_id));
        let mut decoder = FrameDecoder::new();
        decoder
            .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
            .unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(frame, &mut decoded).unwrap();
        assert_eq!(decoded.as_slice(), payload.as_slice());
    }
}

#[test]
fn dict_primed_matcher_cache_reused_across_small_attach_frames_is_byte_identical() {
    // CDict-equivalent ATTACH path (small source, at/below the Fast 8 KiB
    // attach cutoff): frames 2..N re-prime — re-committing the dict bytes
    // to history — but reuse the already-built dict table instead of
    // re-hashing it. The cached-table frame must reproduce the
    // freshly-primed first frame byte-for-byte, and a fresh single-frame
    // compressor (no prior dict cache) must produce the identical bytes
    // too, proving the cache changes timing, not output.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    // Stay under the 8 KiB cutoff so the attach (re-prime) path is taken
    // every frame rather than the copy-snapshot restore.
    let mut payload = Vec::new();
    while payload.len() < 2 * 1024 {
        payload.extend_from_slice(b"tenant=demo op=put key=1 value=aaaaabbbbbcccccddddd\n");
    }

    let prepared = super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
    let dict_id = prepared.id();
    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    compressor
        .set_encoder_dictionary(prepared)
        .expect("prepared dictionary should attach");

    // Frame 1 builds + marks the dict table; frame 2 reuses it.
    let frame1 = compressor.compress_independent_frame(payload.as_slice());
    let frame2 = compressor.compress_independent_frame(payload.as_slice());
    assert_eq!(
        frame1, frame2,
        "reused dict table (attach path) must reproduce the freshly-built frame byte-for-byte"
    );

    // A fresh compressor (cold dict cache) must emit the same bytes — the
    // cache is a timing optimization, never a content change.
    let fresh_prepared =
        super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse");
    let mut fresh: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    fresh
        .set_encoder_dictionary(fresh_prepared)
        .expect("prepared dictionary should attach");
    let fresh_frame = fresh.compress_independent_frame(payload.as_slice());
    assert_eq!(
        fresh_frame, frame1,
        "cold-cache compressor must match the warm-cache frame byte-for-byte"
    );

    for frame in [&frame1, &frame2] {
        let (hdr, _) =
            crate::decoding::frame::read_frame_header(frame.as_slice()).expect("frame header");
        assert_eq!(hdr.dictionary_id(), Some(dict_id));
        let mut decoder = FrameDecoder::new();
        decoder
            .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
            .unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(frame, &mut decoded).unwrap();
        assert_eq!(decoded.as_slice(), payload.as_slice());
    }
}

#[test]
fn dict_reused_across_many_lazy_frames_stays_applied() {
    // Regression: a reused HashChain-backed dictionary frame (lazy levels)
    // re-primes the dict at the live `history_abs_start` every frame. The
    // floor-advance reset let that base climb frame-over-frame until the
    // freshly-primed dict region dropped below `window_low`, after which
    // every dict match was silently lost and the output ballooned to the
    // no-dict size (observed at frame 3-4 of a reused compressor). Drive
    // many frames and require every one to stay byte-identical to the
    // first — the dict must keep applying, not decay after a few frames.
    // Multiple distinct lines so the dictionary is load-bearing: without it
    // frame 0 must emit each distinct line as literals; with it those lines
    // match the primed dict immediately. A single repeated line matches
    // in-frame regardless and would hide the regression.
    let lines: &[&[u8]] = &[
        b"ts=2026 level=INFO msg=\"flush memtable\" tenant=demo table=orders\n",
        b"ts=2026 level=INFO msg=\"rotate segment\" tenant=demo table=orders\n",
        b"ts=2026 level=INFO msg=\"compact level\" tenant=demo table=orders\n",
        b"ts=2026 level=INFO msg=\"write block\" tenant=demo table=orders\n",
    ];
    let fill = |n: usize| -> Vec<u8> {
        let mut b = Vec::with_capacity(n);
        while b.len() < n {
            for l in lines {
                if b.len() >= n {
                    break;
                }
                let take = (n - b.len()).min(l.len());
                b.extend_from_slice(&l[..take]);
            }
        }
        b
    };
    let dict = fill(8 * 1024);
    let payload = fill(16 * 1024);
    let dict_obj =
        crate::decoding::Dictionary::from_raw_content(1, dict).expect("raw dict should build");

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(6));
    compressor.set_dictionary_id_flag(false);
    compressor
        .set_dictionary(dict_obj)
        .expect("dict should attach");

    let first = compressor.compress_independent_frame(payload.as_slice());

    // No-dict baseline at the same level: the dictionary must be
    // load-bearing, so the dict-applied frame has to beat it. Without this
    // anchor the equal-length loop below would also pass if EVERY frame
    // decayed to the no-dict size in lockstep.
    let mut nodict: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(6));
    let no_dict_frame = nodict.compress_independent_frame(payload.as_slice());
    assert!(
        first.len() < no_dict_frame.len(),
        "dict must be load-bearing: dict frame {} should beat the no-dict baseline {}",
        first.len(),
        no_dict_frame.len(),
    );

    for i in 1..16 {
        let frame = compressor.compress_independent_frame(payload.as_slice());
        // Byte-identity, not just equal length: a same-size divergence (e.g.
        // a different match decision once the resident dict bookkeeping
        // drifts) would slip past a length-only check.
        assert_eq!(
            frame, first,
            "frame {i} of a reused dict compressor must stay byte-identical to \
                 the first (dict still applied, no decay or bookkeeping drift)"
        );
    }
}

#[test]
fn dict_fast_epoch_reset_many_frames_and_attach_copy_alternation_byte_identical() {
    // The Fast attach path invalidates the main hash table between
    // frames with an epoch-bias advance instead of a memset. Two things
    // need proving against a fresh-compressor reference:
    // 1. the bias accumulates across MANY reused frames without ever
    //    letting a stale entry through (every frame byte-identical);
    // 2. crossing the 8 KiB attach/copy cutoff in both directions
    //    (attach → copy clears the bias for the raw-slice kernel,
    //    copy → attach re-enters epoch mode) stays byte-identical.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let mut small = Vec::new();
    while small.len() < 2 * 1024 {
        small.extend_from_slice(b"tenant=demo op=put key=1 value=aaaaabbbbbcccccddddd\n");
    }
    // Over the Fast 8 KiB attach cutoff → copy-mode frame.
    let mut large = Vec::new();
    while large.len() < 64 * 1024 {
        large.extend_from_slice(b"tenant=demo op=scan range=[k0,k9) limit=500 order=asc\n");
    }

    let mut reused: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    reused
        .set_encoder_dictionary(
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse"),
        )
        .expect("prepared dictionary should attach");

    let reference = |payload: &[u8]| -> alloc::vec::Vec<u8> {
        let mut fresh: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        fresh
            .set_encoder_dictionary(
                super::EncoderDictionary::from_bytes(dict_raw).expect("dict bytes should parse"),
            )
            .expect("prepared dictionary should attach");
        fresh.compress_independent_frame(payload)
    };

    let small_expected = reference(&small);
    let large_expected = reference(&large);

    // 1. Long attach-only run: every frame advances the epoch bias.
    for i in 0..32 {
        let frame = reused.compress_independent_frame(small.as_slice());
        assert_eq!(
            frame, small_expected,
            "attach frame {i} diverged from the fresh-compressor reference"
        );
    }
    // 2. Cutoff alternation: attach → copy → attach → copy.
    for i in 0..4 {
        let frame = reused.compress_independent_frame(large.as_slice());
        assert_eq!(
            frame, large_expected,
            "copy frame {i} diverged from the fresh-compressor reference"
        );
        let frame = reused.compress_independent_frame(small.as_slice());
        assert_eq!(
            frame, small_expected,
            "attach frame after copy {i} diverged from the fresh-compressor reference"
        );
    }
}

#[test]
fn dict_primed_btlazy2_reused_across_attach_and_copy_boundary_is_byte_identical() {
    // Btlazy2 (Level 15) uses the 32 KiB dict attach/copy cutoff in
    // prepare_frame. Exercise BOTH sides of that boundary on a reused
    // compressor: a sub-cutoff payload (re-prime/attach path) and an
    // over-cutoff payload (copy-snapshot restore path). In each case the
    // warm-cache second frame must reproduce the cold-cache first frame
    // byte-for-byte (the dict cache is a timing optimization, never a
    // content change), and every frame must round-trip.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let dict_id = super::EncoderDictionary::from_bytes(dict_raw)
        .expect("dict bytes should parse")
        .id();
    // Distinct lines so the payload does not trivially self-compress; the
    // BT finder + dict dual-probe both get exercised.
    let make_payload = |target: usize| {
        let mut p = Vec::with_capacity(target);
        let mut i = 0u64;
        while p.len() < target {
            p.extend_from_slice(
                format!(
                    "tenant=demo op=put key={i} value=aaaaabbbbbcccccddddd-{}\n",
                    i % 97
                )
                .as_bytes(),
            );
            i += 1;
        }
        p
    };
    // Below the 32 KiB cutoff (attach/re-prime) and above it (copy-snapshot).
    for target in [16 * 1024usize, 64 * 1024usize] {
        let payload = make_payload(target);
        let mut warm: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(15));
        warm.set_encoder_dictionary(
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict parse"),
        )
        .expect("dict attach");
        // Frame 1 builds + marks the dict tables; frame 2 reuses them.
        let frame1 = warm.compress_independent_frame(payload.as_slice());
        let frame2 = warm.compress_independent_frame(payload.as_slice());
        assert_eq!(
            frame1, frame2,
            "reused dict cache must reproduce the freshly-primed frame byte-for-byte \
                 (Level 15, target={target})"
        );
        // Cold-cache compressor: must match the warm-cache bytes.
        let mut cold: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(15));
        cold.set_encoder_dictionary(
            super::EncoderDictionary::from_bytes(dict_raw).expect("dict parse"),
        )
        .expect("dict attach");
        let cold_frame = cold.compress_independent_frame(payload.as_slice());
        assert_eq!(
            cold_frame, frame1,
            "cold-cache compressor must match warm-cache frame (Level 15, target={target})"
        );
        // Round-trip through a decoder primed with the same dict.
        for frame in [&frame1, &frame2] {
            let (hdr, _) =
                crate::decoding::frame::read_frame_header(frame.as_slice()).expect("frame header");
            assert_eq!(hdr.dictionary_id(), Some(dict_id));
            let mut decoder = FrameDecoder::new();
            decoder
                .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
                .unwrap();
            let mut decoded = Vec::with_capacity(payload.len());
            decoder.decode_all_to_vec(frame, &mut decoded).unwrap();
            assert_eq!(decoded.as_slice(), payload.as_slice());
        }
    }
}

#[test]
fn dict_primed_btultra2_restore_is_floor_safe_and_byte_identical() {
    // Regression guard for the dictionary primed-snapshot RESTORE path on
    // the binary-tree (btultra2 / Level 22) backend — the path a minimal /
    // decoupled prepared-dict refactor rewrites.
    //
    // The trap it pins: a reused compressor compresses frame A (which fills
    // the live hash/chain tables with frame-A positions and advances the
    // window floor), then frame B of the SAME resolved shape (same size →
    // same PrimedKey → the snapshot RESTORE path) but DIFFERENT content. The
    // restore must reinstate the clean post-prime dict state with NO live
    // frame-A entries surviving above the restored floor; a restore that
    // leaks stale frame-A positions would surface FALSE matches and produce
    // a different (or undecodable) frame B. The invariant: a snapshot
    // restore is a pure timing optimization and MUST be byte-identical to a
    // cold compressor compressing frame B from scratch, and must round-trip.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let dict_id = super::EncoderDictionary::from_bytes(dict_raw)
        .expect("dict bytes should parse")
        .id();
    // 48 KiB > the btultra2 8 KiB attach cutoff → the copy-snapshot
    // capture/restore path. Two distinct payloads of the SAME size so frame
    // B resolves to frame A's snapshot key and takes the restore path.
    let make_payload = |seed: u64, target: usize| {
        let mut p = Vec::with_capacity(target);
        let mut i = seed;
        while p.len() < target {
            p.extend_from_slice(
                format!(
                    "tenant=demo op=put key={i} value=aaaaabbbbbcccccddddd-{}\n",
                    i % 89
                )
                .as_bytes(),
            );
            i = i.wrapping_add(1);
        }
        p.truncate(target);
        p
    };
    let size = 48 * 1024usize;
    let frame_a = make_payload(0, size);
    let frame_b = make_payload(1_000_000, size);

    let mut warm: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(22));
    warm.set_encoder_dictionary(
        super::EncoderDictionary::from_bytes(dict_raw).expect("dict parse"),
    )
    .expect("dict attach");
    // Frame A: cold cache — primes the dict + captures the snapshot, and
    // fills the live tables with frame-A positions.
    let _wa = warm.compress_independent_frame(frame_a.as_slice());
    // Frame B: warm cache — takes the snapshot RESTORE path (same size).
    let warm_b = warm.compress_independent_frame(frame_b.as_slice());

    // Cold compressor compressing frame B from scratch: the ground truth.
    let mut cold: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(22));
    cold.set_encoder_dictionary(
        super::EncoderDictionary::from_bytes(dict_raw).expect("dict parse"),
    )
    .expect("dict attach");
    let cold_b = cold.compress_independent_frame(frame_b.as_slice());

    assert_eq!(
        warm_b, cold_b,
        "frame B via snapshot restore must be byte-identical to a cold compress \
             (a restore that leaks frame-A live-table entries would diverge here)"
    );

    // Round-trip frame B through a dict-primed decoder.
    let (hdr, _) =
        crate::decoding::frame::read_frame_header(warm_b.as_slice()).expect("frame header");
    assert_eq!(hdr.dictionary_id(), Some(dict_id));
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
        .unwrap();
    let mut decoded = Vec::with_capacity(frame_b.len());
    decoder
        .decode_all_to_vec(warm_b.as_slice(), &mut decoded)
        .unwrap();
    assert_eq!(decoded.as_slice(), frame_b.as_slice());
}

#[test]
fn dict_primed_btultra2_ldm_restore_is_byte_identical() {
    // Same restore-path byte-identity guard as
    // `dict_primed_btultra2_restore_is_floor_safe_and_byte_identical`, but
    // with long-distance matching ENABLED. The BtMatcher's LDM producer is
    // part of the snapshot; a refactor that decouples it (so the snapshot
    // does not retain the empty LDM table) must reinstate an equivalent
    // empty producer on restore. This pins that the warm-cache (restore)
    // frame stays byte-identical to a cold compress when LDM is on.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let dict_id = super::EncoderDictionary::from_bytes(dict_raw)
        .expect("dict bytes should parse")
        .id();
    let make_payload = |seed: u64, target: usize| {
        let mut p = Vec::with_capacity(target);
        let mut i = seed;
        while p.len() < target {
            p.extend_from_slice(
                format!(
                    "tenant=demo op=put key={i} value=aaaaabbbbbcccccddddd-{}\n",
                    i % 89
                )
                .as_bytes(),
            );
            i = i.wrapping_add(1);
        }
        p.truncate(target);
        p
    };
    let ldm_params =
        crate::encoding::CompressionParameters::builder(super::CompressionLevel::Level(22))
            .enable_long_distance_matching(true)
            .build()
            .expect("LDM-only params build");
    let size = 48 * 1024usize;
    let frame_a = make_payload(0, size);
    let frame_b = make_payload(1_000_000, size);

    let mut warm: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(22));
    warm.set_parameters(&ldm_params);
    warm.set_encoder_dictionary(
        super::EncoderDictionary::from_bytes(dict_raw).expect("dict parse"),
    )
    .expect("dict attach");
    let _wa = warm.compress_independent_frame(frame_a.as_slice());
    let warm_b = warm.compress_independent_frame(frame_b.as_slice());

    let mut cold: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Level(22));
    cold.set_parameters(&ldm_params);
    cold.set_encoder_dictionary(
        super::EncoderDictionary::from_bytes(dict_raw).expect("dict parse"),
    )
    .expect("dict attach");
    let cold_b = cold.compress_independent_frame(frame_b.as_slice());

    assert_eq!(
        warm_b, cold_b,
        "LDM-on frame B via snapshot restore must be byte-identical to a cold compress"
    );

    let (hdr, _) =
        crate::decoding::frame::read_frame_header(warm_b.as_slice()).expect("frame header");
    assert_eq!(hdr.dictionary_id(), Some(dict_id));
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict(crate::decoding::Dictionary::decode_dict(dict_raw).unwrap())
        .unwrap();
    let mut decoded = Vec::with_capacity(frame_b.len());
    decoder
        .decode_all_to_vec(warm_b.as_slice(), &mut decoded)
        .unwrap();
    assert_eq!(decoded.as_slice(), frame_b.as_slice());
}

#[test]
fn set_dictionary_from_bytes_matches_full_decode_byte_for_byte() {
    // The encoder-only dict parse (`decode_dict_for_encoding`, used by
    // `set_dictionary_from_bytes`) skips the FSE/HUF decoder-table build and
    // the enrich passes. The encoder entropy tables are derived purely from
    // the symbol probabilities / Huffman weights, so the compressed output
    // MUST be byte-identical to the full-decode path. This pins the
    // load-bearing equivalence so a future FSE/HUF parsing refactor that
    // still round-trips but silently diverges on the probabilities/weights
    // fails loudly here instead of producing a different (but valid) frame.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let payload = b"tenant=demo table=orders op=put key=1 value=aaaaabbbbbcccccdddddeeeee\n\
              tenant=demo table=orders op=put key=2 value=aaaaabbbbbcccccdddddeeeee\n";

    // Path A: encoder-only parse straight from the raw blob.
    let mut from_bytes_out = Vec::new();
    {
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut from_bytes_out);
        compressor.compress();
    }

    // Path B: full decode (builds the decoder tables too), then attach for
    // encoding via the `Dictionary` setter.
    let full_decode = crate::decoding::Dictionary::decode_dict(dict_raw)
        .expect("dictionary bytes should fully decode");
    let mut full_decode_out = Vec::new();
    {
        let mut compressor = FrameCompressor::new(super::CompressionLevel::Fastest);
        compressor
            .set_dictionary(full_decode)
            .expect("full-decode dictionary should attach");
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut full_decode_out);
        compressor.compress();
    }

    assert_eq!(
        from_bytes_out, full_decode_out,
        "encoder-only dict parse must produce byte-identical output to the full decode"
    );
}

#[test]
fn set_dictionary_rejects_zero_dictionary_id() {
    let invalid = crate::decoding::Dictionary {
        id: 0,
        fse: crate::decoding::scratch::FSEScratch::new(),
        huf: crate::decoding::scratch::HuffmanScratch::new(),
        dict_content: vec![1, 2, 3],
        offset_hist: [1, 4, 8],
    };

    let mut compressor: FrameCompressor<
        &[u8],
        Vec<u8>,
        crate::encoding::match_generator::MatchGeneratorDriver,
    > = FrameCompressor::new(super::CompressionLevel::Fastest);
    let result = compressor.set_dictionary(invalid);
    assert!(matches!(
        result,
        Err(crate::decoding::errors::DictionaryDecodeError::ZeroDictionaryId)
    ));
}

#[test]
fn set_dictionary_rejects_zero_repeat_offsets() {
    let invalid = crate::decoding::Dictionary {
        id: 1,
        fse: crate::decoding::scratch::FSEScratch::new(),
        huf: crate::decoding::scratch::HuffmanScratch::new(),
        dict_content: vec![1, 2, 3],
        offset_hist: [0, 4, 8],
    };

    let mut compressor: FrameCompressor<
        &[u8],
        Vec<u8>,
        crate::encoding::match_generator::MatchGeneratorDriver,
    > = FrameCompressor::new(super::CompressionLevel::Fastest);
    let result = compressor.set_dictionary(invalid);
    assert!(matches!(
        result,
        Err(
            crate::decoding::errors::DictionaryDecodeError::ZeroRepeatOffsetInDictionary {
                index: 0
            }
        )
    ));
}

#[test]
fn uncompressed_mode_does_not_require_dictionary() {
    let dict_id = 0xABCD_0001;
    let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"shared-history".to_vec())
        .expect("raw dictionary should be valid");

    let payload = b"plain-bytes-that-should-stay-raw";
    let mut output = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);
    compressor
        .set_dictionary(dict)
        .expect("dictionary should attach in uncompressed mode");
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut output);
    compressor.compress();

    let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
        .expect("encoded frame should have a header");
    assert_eq!(
        frame_header.dictionary_id(),
        None,
        "raw/uncompressed frames must not advertise dictionary dependency"
    );

    let mut decoder = FrameDecoder::new();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn default_level_tiny_raw_dict_compresses_cleanly() {
    // Coverage for the dfast dict-attach fast path with a
    // sub-min-match raw-content dictionary: the dict-table probe in
    // `start_matching_fast_loop` is gated on the dict table actually
    // existing (`table().is_some()`), not merely on `is_attached()`,
    // so a dictionary whose hashable region is shorter than the
    // short-hash lookahead (where `prime_dict_tables_for_range`
    // returns before allocating the tables) never dereferences a
    // null dict pointer. Compressing at the default (dfast) level
    // with such a dict must succeed.
    let dict_id = 0xABCD_0009;
    let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abc".to_vec())
        .expect("raw dictionary should be valid");
    let payload = b"the quick brown fox jumps over the lazy dog, repeatedly and at length";
    let mut output = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Default);
    compressor
        .set_dictionary(dict)
        .expect("tiny raw dictionary should attach");
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut output);
    compressor.compress();
    assert!(!output.is_empty(), "compression should produce a frame");

    // The emitted frame must advertise the attached dictionary id, proving
    // the tiny-dict path stayed active (the payload round-trips either way,
    // so without this the test would also pass on a silent no-dict frame).
    let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
        .expect("encoded frame should have a readable header");
    assert_eq!(
        frame_header.dictionary_id(),
        Some(dict_id),
        "tiny raw dict frame should still advertise its dictionary id",
    );

    // Full roundtrip: decode the dict-compressed frame with the SAME
    // dictionary attached and confirm byte-exact recovery — proves the
    // tiny-dict fast path produces a correct frame, not just a non-empty
    // one.
    let decode_dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abc".to_vec())
        .expect("raw dictionary should be valid");
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict(decode_dict)
        .expect("decoder dict should attach");
    let mut decoded = Vec::with_capacity(payload.len());
    decoder
        .decode_all_to_vec(&output, &mut decoded)
        .expect("dict roundtrip should decode");
    assert_eq!(decoded, payload, "tiny-dict roundtrip mismatch");
}

/// Exercises the dictionary dual-probe (live + immutable dict tables)
/// in the Fast / dfast / Row match finders with a dict whose content
/// the payload actually reuses, so each backend's dict long/short
/// probe (and the dfast `ip+1` dict-long retry) is reached and the
/// dict-compressed frame round-trips through a decoder primed with the
/// same dict. The 3-byte-dict test above only proves the null-table
/// guard; this proves the full attach path produces correct frames.
#[test]
fn dict_attach_roundtrips_across_backends_with_matching_payload() {
    let dict_id = 0xD1C7_0001;
    // Distinct lines so the payload does NOT self-compress: each line
    // appears exactly once in the payload, so without the dictionary there
    // are no in-frame back-references to exploit. The dictionary holds the
    // SAME lines, so the only way the output shrinks is if the dict probe
    // actually fires. A no-dict baseline below pins that the dict path ran
    // (self-compressible payloads would round-trip + stay small via
    // in-frame matches alone, proving nothing).
    let line = |i: u32| {
        alloc::format!(
            "ts=2026-03-26T21:{:02}:{:02}Z level=INFO msg=\"event {i:05}\" tenant=t{i} region=eu\n",
            i / 60 % 60,
            i % 60,
        )
        .into_bytes()
    };
    let mut dict_content = Vec::new();
    for i in 0..256u32 {
        dict_content.extend_from_slice(&line(i));
    }
    // Payload = the same distinct lines in a different (stride) order, each
    // once → no self-repeats, every line is a dictionary match.
    let mut payload = Vec::new();
    let mut i = 0u32;
    for _ in 0..256u32 {
        payload.extend_from_slice(&line(i));
        i = (i + 97) % 256; // coprime stride → permutation, no adjacency
    }

    let compress_at = |level, dict: Option<Vec<u8>>| -> Vec<u8> {
        let mut compressor = FrameCompressor::new(level);
        if let Some(bytes) = dict {
            let d = crate::decoding::Dictionary::from_raw_content(dict_id, bytes)
                .expect("raw dictionary should be valid");
            compressor
                .set_dictionary(d)
                .expect("dictionary should attach");
        }
        let mut out = Vec::new();
        compressor.set_source(payload.as_slice());
        compressor.set_drain(&mut out);
        compressor.compress();
        out
    };

    for level in [
        super::CompressionLevel::Level(-5), // Fast (negative)
        super::CompressionLevel::Level(1),  // Fast
        super::CompressionLevel::Default,   // dfast (L3)
        super::CompressionLevel::Level(8),  // Row-backed lazy2
    ] {
        let out = compress_at(level, Some(dict_content.clone()));
        let no_dict = compress_at(level, None);
        // The dict path MUST measurably beat no-dict on this
        // non-self-compressible payload — otherwise the dict probe never
        // fired and the roundtrip below would prove nothing.
        assert!(
            out.len() < no_dict.len(),
            "level {level:?}: dict-primed output ({}) must beat no-dict ({}) — dict probe did not fire",
            out.len(),
            no_dict.len(),
        );

        let ddict = crate::decoding::Dictionary::from_raw_content(dict_id, dict_content.clone())
            .expect("raw dictionary should be valid");
        let mut decoder = FrameDecoder::new();
        decoder.add_dict(ddict).expect("decoder dict should attach");
        let mut decoded = Vec::with_capacity(payload.len());
        decoder
            .decode_all_to_vec(&out, &mut decoded)
            .unwrap_or_else(|e| panic!("level {level:?}: dict roundtrip decode failed: {e:?}"));
        assert_eq!(decoded, payload, "level {level:?}: dict roundtrip mismatch");
    }
}

/// Reusing one compressor across independent frames with DIFFERENT
/// dictionaries must drop the per-backend dict cache on each swap
/// (Simple/Dfast/Row keep the attach index across frames). Without the
/// invalidation a later frame would reuse the previous dict's rows.
/// Each frame round-trips through a decoder primed with its own dict.
#[test]
fn dict_swap_across_reused_compressor_roundtrips() {
    // Distinct lines per dict (not a single repeated line) so payloads do
    // NOT self-compress: each line appears once, so a frame only shrinks if
    // the dict probe fires, and — crucially for the invalidation check — if
    // frame B reused dict A's stale rows it would emit offsets into A's
    // distinct content, which decode under dict B reconstructs as WRONG
    // bytes (caught by the roundtrip). A single repeated line would hide
    // pollution behind in-frame matches.
    let lines = |tag: &str| -> (Vec<u8>, Vec<u8>) {
        let line = |i: u32| alloc::format!("{tag} record {i:05} field=value{i} end\n").into_bytes();
        let mut dict = Vec::new();
        for i in 0..256u32 {
            dict.extend_from_slice(&line(i));
        }
        let mut payload = Vec::new();
        let mut i = 0u32;
        for _ in 0..256u32 {
            payload.extend_from_slice(&line(i));
            i = (i + 97) % 256;
        }
        (dict, payload)
    };
    let (dict_a, payload_a) = lines("alpha");
    let (dict_b, payload_b) = lines("bravo");

    for level in [
        super::CompressionLevel::Default,
        super::CompressionLevel::Level(8),
    ] {
        let no_dict = |payload: &[u8]| -> usize {
            let mut c: FrameCompressor = FrameCompressor::new(level);
            c.compress_independent_frame(payload).len()
        };
        let no_dict_a = no_dict(&payload_a);
        let no_dict_b = no_dict(&payload_b);

        let mut compressor: FrameCompressor = FrameCompressor::new(level);
        for (dict_bytes, payload, no_dict_len) in [
            (&dict_a, &payload_a, no_dict_a),
            (&dict_b, &payload_b, no_dict_b),
        ] {
            let dict =
                crate::decoding::Dictionary::from_raw_content(0xD1C7_0002, dict_bytes.clone())
                    .expect("raw dictionary should be valid");
            compressor
                .set_dictionary(dict)
                .expect("dictionary should attach");
            let out = compressor.compress_independent_frame(payload.as_slice());
            assert!(
                out.len() < no_dict_len,
                "level {level:?}: dict frame ({}) must beat no-dict ({}) — dict probe did not fire",
                out.len(),
                no_dict_len,
            );

            let ddict =
                crate::decoding::Dictionary::from_raw_content(0xD1C7_0002, dict_bytes.clone())
                    .expect("raw dictionary should be valid");
            let mut decoder = FrameDecoder::new();
            decoder.add_dict(ddict).expect("decoder dict should attach");
            let mut decoded = Vec::with_capacity(payload.len());
            decoder
                .decode_all_to_vec(&out, &mut decoded)
                .unwrap_or_else(|e| panic!("level {level:?}: dict-swap decode failed: {e:?}"));
            assert_eq!(
                decoded, *payload,
                "level {level:?}: dict-swap roundtrip mismatch (stale dict rows?)"
            );
        }
    }
}

#[test]
fn dictionary_roundtrip_stays_valid_after_output_exceeds_window() {
    use crate::encoding::match_generator::MatchGeneratorDriver;

    let dict_id = 0xABCD_0002;
    let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
        .expect("raw dictionary should be valid");
    let dict_for_decoder =
        crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
            .expect("raw dictionary should be valid");

    // Payload must exceed the encoder's advertised window (512 KiB
    // for Fastest after `window_log = 19` alignment with upstream zstd's
    // L1 fast row in `clevels.h`) so the test actually exercises
    // cross-window-boundary behavior.
    let payload = b"abcdefgh".repeat(512 * 1024 / 8 + 64);
    let matcher = MatchGeneratorDriver::new(1024, 1);

    let mut no_dict_output = Vec::new();
    let mut no_dict_compressor =
        FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
    no_dict_compressor.set_source(payload.as_slice());
    no_dict_compressor.set_drain(&mut no_dict_output);
    no_dict_compressor.compress();
    let (no_dict_frame_header, _) =
        crate::decoding::frame::read_frame_header(no_dict_output.as_slice())
            .expect("baseline frame should have a header");
    let no_dict_window = no_dict_frame_header
        .window_size()
        .expect("window size should be present");

    let mut output = Vec::new();
    let matcher = MatchGeneratorDriver::new(1024, 1);
    let mut compressor =
        FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
    compressor
        .set_dictionary(dict)
        .expect("dictionary should attach");
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut output);
    compressor.compress();

    let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
        .expect("encoded frame should have a header");
    let advertised_window = frame_header
        .window_size()
        .expect("window size should be present");
    assert_eq!(
        advertised_window, no_dict_window,
        "dictionary priming must not inflate advertised window size"
    );
    assert!(
        payload.len() > advertised_window as usize,
        "test must cross the advertised window boundary"
    );

    let mut decoder = FrameDecoder::new();
    decoder.add_dict(dict_for_decoder).unwrap();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn source_size_hint_with_dictionary_keeps_roundtrip_and_nonincreasing_window() {
    let dict_id = 0xABCD_0004;
    let dict_content = b"abcd".repeat(1024); // 4 KiB dictionary history
    let dict = crate::decoding::Dictionary::from_raw_content(dict_id, dict_content).unwrap();
    let dict_for_decoder =
        crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024)).unwrap();
    let payload = b"abcdabcdabcdabcd".repeat(128);

    let mut hinted_output = Vec::new();
    let mut hinted = FrameCompressor::new(super::CompressionLevel::Fastest);
    hinted.set_dictionary(dict).unwrap();
    hinted.set_source_size_hint(1);
    hinted.set_source(payload.as_slice());
    hinted.set_drain(&mut hinted_output);
    hinted.compress();

    let mut no_hint_output = Vec::new();
    let mut no_hint = FrameCompressor::new(super::CompressionLevel::Fastest);
    no_hint
        .set_dictionary(
            crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024)).unwrap(),
        )
        .unwrap();
    no_hint.set_source(payload.as_slice());
    no_hint.set_drain(&mut no_hint_output);
    no_hint.compress();

    let hinted_window = crate::decoding::frame::read_frame_header(hinted_output.as_slice())
        .expect("encoded frame should have a header")
        .0
        .window_size()
        .expect("window size should be present");
    let no_hint_window = crate::decoding::frame::read_frame_header(no_hint_output.as_slice())
        .expect("encoded frame should have a header")
        .0
        .window_size()
        .expect("window size should be present");
    assert!(
        hinted_window <= no_hint_window,
        "source-size hint should not increase advertised window with dictionary priming",
    );

    let mut decoder = FrameDecoder::new();
    decoder.add_dict(dict_for_decoder).unwrap();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder
        .decode_all_to_vec(&hinted_output, &mut decoded)
        .unwrap();
    assert_eq!(decoded, payload);
}

/// A dictionary segment embedded ONCE in otherwise-incompressible
/// input must be matched against the dictionary. Before the fix the
/// raw-fast-path (which skips matching) fired on the
/// incompressible-looking block and the dictionary was never searched,
/// so `with_dict` came out the same size as `no_dict` (the embedded
/// match was lost). Now the block compresses against the dict.
#[test]
fn dictionary_segment_in_incompressible_input_is_matched() {
    // Deterministic LCG bytes: high-entropy, so the only compressible
    // content is the embedded dictionary segment.
    fn lcg(seed: u64, n: usize) -> alloc::vec::Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 56) as u8
            })
            .collect()
    }
    let dict_id = 0x00DC_7777;
    let r = lcg(1, 512); // the dictionary content
    let mut payload = lcg(2, 2000); // incompressible filler before
    payload.extend_from_slice(&r); // the single dict-matchable segment
    payload.extend_from_slice(&lcg(3, 1500)); // filler after

    // Precondition: the payload must actually look incompressible so
    // that the raw-fast-path WOULD fire (and skip matching) without
    // the fix. If the heuristic ever changes and this no longer holds,
    // the test below would pass vacuously — assert it up front.
    assert!(
        crate::encoding::incompressible::block_looks_incompressible(&payload),
        "test payload must look incompressible to exercise the raw-fast-path",
    );

    let compress = |level: super::CompressionLevel, dict: Option<&[u8]>| -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::new();
        let mut c = FrameCompressor::new(level);
        if let Some(d) = dict {
            c.set_dictionary(
                crate::decoding::Dictionary::from_raw_content(dict_id, d.to_vec()).unwrap(),
            )
            .unwrap();
        }
        c.set_source(payload.as_slice());
        c.set_drain(&mut out);
        c.compress();
        out
    };

    for lvl in [
        super::CompressionLevel::Level(2),
        super::CompressionLevel::Level(6),
        super::CompressionLevel::Level(19),
    ] {
        let with_dict = compress(lvl, Some(&r));
        let no_dict = compress(lvl, None);
        // The 512-byte dict segment should be matched, saving most of
        // its length (generous slack for sequence/header coding).
        assert!(
            with_dict.len() + 300 < no_dict.len(),
            "{lvl:?}: dict segment not matched (with_dict={}, no_dict={})",
            with_dict.len(),
            no_dict.len(),
        );
        // The dict-compressed frame must round-trip through the decoder.
        let mut decoder = FrameDecoder::new();
        decoder
            .add_dict(crate::decoding::Dictionary::from_raw_content(dict_id, r.clone()).unwrap())
            .unwrap();
        let mut decoded = Vec::with_capacity(payload.len());
        decoder.decode_all_to_vec(&with_dict, &mut decoded).unwrap();
        assert_eq!(decoded, payload, "{lvl:?}: dict round-trip mismatch");

        // A dictionary that does NOT appear in the input must not make
        // the output larger than the no-dict (raw) encoding: the
        // post-compress raw fallback covers incompressible-with-dict.
        let unrelated = lcg(99, 512);
        let with_bad_dict = compress(lvl, Some(&unrelated));
        assert!(
            with_bad_dict.len() <= no_dict.len() + 16,
            "{lvl:?}: unhelpful dict expanded output (with={}, no_dict={})",
            with_bad_dict.len(),
            no_dict.len(),
        );
    }
}

#[test]
fn source_size_hint_with_dictionary_keeps_roundtrip_for_larger_payload() {
    let dict_id = 0xABCD_0005;
    let dict_content = b"abcd".repeat(1024); // 4 KiB dictionary history
    let dict = crate::decoding::Dictionary::from_raw_content(dict_id, dict_content).unwrap();
    let dict_for_decoder =
        crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024)).unwrap();
    let payload = b"abcd".repeat(1024); // 4 KiB payload
    let payload_len = payload.len() as u64;

    let mut hinted_output = Vec::new();
    let mut hinted = FrameCompressor::new(super::CompressionLevel::Fastest);
    hinted.set_dictionary(dict).unwrap();
    hinted.set_source_size_hint(payload_len);
    hinted.set_source(payload.as_slice());
    hinted.set_drain(&mut hinted_output);
    hinted.compress();

    let mut no_hint_output = Vec::new();
    let mut no_hint = FrameCompressor::new(super::CompressionLevel::Fastest);
    no_hint
        .set_dictionary(
            crate::decoding::Dictionary::from_raw_content(dict_id, b"abcd".repeat(1024)).unwrap(),
        )
        .unwrap();
    no_hint.set_source(payload.as_slice());
    no_hint.set_drain(&mut no_hint_output);
    no_hint.compress();

    let hinted_window = crate::decoding::frame::read_frame_header(hinted_output.as_slice())
        .expect("encoded frame should have a header")
        .0
        .window_size()
        .expect("window size should be present");
    let no_hint_window = crate::decoding::frame::read_frame_header(no_hint_output.as_slice())
        .expect("encoded frame should have a header")
        .0
        .window_size()
        .expect("window size should be present");
    assert!(
        hinted_window <= no_hint_window,
        "source-size hint should not increase advertised window with dictionary priming",
    );

    let mut decoder = FrameDecoder::new();
    decoder.add_dict(dict_for_decoder).unwrap();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder
        .decode_all_to_vec(&hinted_output, &mut decoded)
        .unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn custom_matcher_without_dictionary_priming_does_not_advertise_dict_id() {
    let dict_id = 0xABCD_0003;
    let dict = crate::decoding::Dictionary::from_raw_content(dict_id, b"abcdefgh".to_vec())
        .expect("raw dictionary should be valid");
    let payload = b"abcdefghabcdefgh";

    let mut output = Vec::new();
    let matcher = NoDictionaryMatcher::new(64);
    let mut compressor =
        FrameCompressor::new_with_matcher(matcher, super::CompressionLevel::Fastest);
    compressor
        .set_dictionary(dict)
        .expect("dictionary should attach");
    compressor.set_source(payload.as_slice());
    compressor.set_drain(&mut output);
    compressor.compress();

    let (frame_header, _) = crate::decoding::frame::read_frame_header(output.as_slice())
        .expect("encoded frame should have a header");
    assert_eq!(
        frame_header.dictionary_id(),
        None,
        "matchers that do not support dictionary priming must not advertise dictionary dependency"
    );

    let mut decoder = FrameDecoder::new();
    let mut decoded = Vec::with_capacity(payload.len());
    decoder.decode_all_to_vec(&output, &mut decoded).unwrap();
    assert_eq!(decoded, payload);
}

#[cfg(feature = "hash")]
#[test]
fn checksum_two_frames_reused_compressor() {
    // Compress the same data twice using the same compressor and verify that:
    // 1. The checksum written in each frame matches what the decoder calculates.
    // 2. The hasher is correctly reset between frames (no cross-contamination).
    //    If the hasher were NOT reset, the second frame's calculated checksum
    //    would differ from the one stored in the frame data, causing assert_eq to fail.
    let data: Vec<u8> = (0u8..=255).cycle().take(1024).collect();

    let mut compressor = FrameCompressor::new(super::CompressionLevel::Uncompressed);

    // --- Frame 1 ---
    let mut compressed1 = Vec::new();
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut compressed1);
    compressor.compress();

    // --- Frame 2 (reuse the same compressor) ---
    let mut compressed2 = Vec::new();
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut compressed2);
    compressor.compress();

    fn decode_and_collect(compressed: &[u8]) -> (Vec<u8>, Option<u32>, Option<u32>) {
        let mut decoder = FrameDecoder::new();
        let mut source = compressed;
        decoder.reset(&mut source).unwrap();
        while !decoder.is_finished() {
            decoder
                .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                .unwrap();
        }
        let mut decoded = Vec::new();
        decoder.collect_to_writer(&mut decoded).unwrap();
        (
            decoded,
            decoder.get_checksum_from_data(),
            decoder.get_calculated_checksum(),
        )
    }

    let (decoded1, chksum_from_data1, chksum_calculated1) = decode_and_collect(&compressed1);
    assert_eq!(decoded1, data, "frame 1: decoded data mismatch");
    assert_eq!(
        chksum_from_data1, chksum_calculated1,
        "frame 1: checksum mismatch"
    );

    let (decoded2, chksum_from_data2, chksum_calculated2) = decode_and_collect(&compressed2);
    assert_eq!(decoded2, data, "frame 2: decoded data mismatch");
    assert_eq!(
        chksum_from_data2, chksum_calculated2,
        "frame 2: checksum mismatch"
    );

    // Same data compressed twice must produce the same checksum.
    // If state leaked across frames, the second calculated checksum would differ.
    assert_eq!(
        chksum_from_data1, chksum_from_data2,
        "frame 1 and frame 2 should have the same checksum (same data, hash must reset per frame)"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn frame_emit_info_decompressed_ranges_match_decoded_output() {
    // Part A correctness: the per-block `decompressed_size` captured during
    // encode (and the `decompressed_byte_range` prefix sum derived from it)
    // must describe the real decoded output exactly — one entry per
    // physical block, contiguous, summing to the full decompressed length.
    // A multi-block compressible payload exercises the Compressed-block
    // path (whose regenerated size is NOT on the wire, so it relies on the
    // encode-side capture this test guards).
    let data = emit_info_fixture_data();

    // Cover both the single-block-per-chunk path (Default) and the
    // Level(16..=22) post-split path (multiple physical partitions per
    // input chunk), since lsm-tree compresses at zstd:22 and post-split
    // is the riskiest capture site (per-partition `src_size`).
    for level in [
        super::CompressionLevel::Default,
        super::CompressionLevel::Level(22),
    ] {
        let mut compressed = Vec::new();
        let mut compressor = FrameCompressor::new(level);
        // Pledge the source size so the high-level (22) window shrinks to
        // fit the payload, keeping the frame compact (no oversized window
        // descriptor for a small input). Still >= 128 KiB, so post-split
        // eligibility is preserved.
        compressor.set_source_size_hint(data.len() as u64);
        compressor.set_source(data.as_slice());
        compressor.set_drain(&mut compressed);
        compressor.compress();

        let info = compressor
            .last_frame_emit_info()
            .expect("emit info populated after compress")
            .clone();

        // Reference: full decode of the same frame.
        let mut decoder = FrameDecoder::new();
        let mut source = compressed.as_slice();
        decoder.reset(&mut source).unwrap();
        while !decoder.is_finished() {
            decoder
                .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
                .unwrap();
        }
        let mut decoded = Vec::new();
        decoder.collect_to_writer(&mut decoded).unwrap();
        assert_eq!(decoded, data, "sanity: frame must round-trip ({level:?})");

        assert!(
            info.blocks.len() >= 2,
            "fixture must span multiple blocks to exercise the mapping ({level:?}, got {})",
            info.blocks.len()
        );
        assert!(
            info.blocks.last().unwrap().last_block,
            "final block must carry last_block ({level:?})"
        );

        // Pin the Level(22) post-split path: the owned loop feeds the
        // encoder MAX_BLOCK_SIZE input chunks, so without post-split the
        // block count cannot exceed the chunk count. More blocks than
        // chunks proves at least one chunk was split into multiple physical
        // partitions (the per-partition `src_size` capture under test).
        if matches!(level, super::CompressionLevel::Level(22)) {
            let max_block = crate::common::MAX_BLOCK_SIZE as usize;
            let n_chunks = data.len().div_ceil(max_block);
            assert!(
                info.blocks.len() > n_chunks,
                "Level(22) must exercise post-split: {} blocks for {} input chunks",
                info.blocks.len(),
                n_chunks
            );
        }

        // Per-block ranges: contiguous, zero-based, summing to the full output.
        let mut expected_start = 0u64;
        for i in 0..info.blocks.len() {
            let range = info
                .decompressed_byte_range(i)
                .expect("in-bounds block has a range");
            assert_eq!(
                range.start, expected_start,
                "block {i} range must start where the previous ended ({level:?})"
            );
            assert_eq!(
                u64::from(info.blocks[i].decompressed_size),
                range.end - range.start,
                "block {i} decompressed_size must equal its range width ({level:?})"
            );
            // Validate the mapping against REAL per-block bytes, not just
            // prefix-sum consistency: decode block `i` alone and require it
            // to equal the corresponding slice of the full decode. A
            // sidecar that swapped sizes between adjacent blocks (same sum,
            // same contiguity) would fail here.
            let mut psrc = compressed.as_slice();
            let mut pdec = FrameDecoder::new();
            pdec.reset(&mut psrc).unwrap();
            let pd = pdec
                .decode_blocks_partial(&mut psrc, i as u32, i as u32 + 1, None, false)
                .unwrap();
            assert!(
                pd.stopped_at.is_none(),
                "block {i} must decode cleanly ({level:?})"
            );
            assert_eq!(
                pd.data.as_slice(),
                &decoded[range.start as usize..range.end as usize],
                "block {i} partial-decode bytes must equal the full-decode slice ({level:?})"
            );
            expected_start = range.end;
        }
        assert_eq!(
            expected_start,
            decoded.len() as u64,
            "block decompressed sizes must sum to the full decoded length ({level:?})"
        );
        assert_eq!(
            info.decompressed_byte_range(info.blocks.len()),
            None,
            "out-of-range index yields None ({level:?})"
        );
    }
}

/// ~400 KiB semi-repetitive payload (long runs interleaved with a stride
/// phrase) that compresses into several multi-block frames across levels.
#[cfg(feature = "lsm")]
fn emit_info_fixture_data() -> Vec<u8> {
    let mut data: Vec<u8> = Vec::with_capacity(400 * 1024);
    let mut x = 0x9E37_79B9u32;
    while data.len() < 400 * 1024 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let run = 16 + (x as usize % 48);
        let byte = (x >> 24) as u8;
        for _ in 0..run {
            data.push(byte);
        }
        data.extend_from_slice(b"the quick brown fox jumps over the lazy dog\n");
    }
    data
}

#[cfg(feature = "lsm")]
#[test]
fn frame_emit_info_decompressed_ranges_match_on_borrowed_oneshot_path() {
    // The borrowed one-shot path (`compress_independent_frame` ->
    // `run_borrowed_block_loop` -> `compress_block_encoded_borrowed`)
    // threads the decompressed-size sidecar through a DIFFERENT emit site
    // than the owned/streaming loop, so it needs its own per-block mapping
    // check. A Fast level keeps the encoder on the borrowed-eligible
    // (Simple matcher) path.
    let data = emit_info_fixture_data();

    let mut compressor: FrameCompressor = FrameCompressor::new(super::CompressionLevel::Fastest);
    let compressed = compressor.compress_independent_frame(data.as_slice());
    let info = compressor
        .last_frame_emit_info()
        .expect("emit info populated after compress_independent_frame")
        .clone();
    // Pin the compressed-block path: without this the fixture could regress
    // into the raw-fast fallback and still pass via the Raw wire-size
    // fallback in populate_frame_emit_info, never exercising the borrowed
    // compressed-block sidecar capture this test targets.
    assert!(
        info.blocks
            .iter()
            .any(|b| matches!(b.block_type, crate::blocks::block::BlockType::Compressed)),
        "borrowed-path fixture must emit at least one compressed block"
    );
    assert!(
        info.blocks.len() >= 2,
        "borrowed fixture must span multiple blocks (got {})",
        info.blocks.len()
    );
    assert!(info.blocks.last().unwrap().last_block);

    // Full decode reference.
    let mut decoder = FrameDecoder::new();
    let mut source = compressed.as_slice();
    decoder.reset(&mut source).unwrap();
    while !decoder.is_finished() {
        decoder
            .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
            .unwrap();
    }
    let mut decoded = Vec::new();
    decoder.collect_to_writer(&mut decoded).unwrap();
    assert_eq!(decoded, data, "borrowed one-shot frame must round-trip");

    // Each block's mapping must match real per-block bytes.
    let mut expected_start = 0u64;
    for i in 0..info.blocks.len() {
        let range = info.decompressed_byte_range(i).unwrap();
        assert_eq!(range.start, expected_start, "block {i} range contiguity");
        let mut psrc = compressed.as_slice();
        let mut pdec = FrameDecoder::new();
        pdec.reset(&mut psrc).unwrap();
        let pd = pdec
            .decode_blocks_partial(&mut psrc, i as u32, i as u32 + 1, None, false)
            .unwrap();
        assert!(pd.stopped_at.is_none(), "block {i} must decode cleanly");
        assert_eq!(
            pd.data.as_slice(),
            &decoded[range.start as usize..range.end as usize],
            "borrowed block {i} partial-decode bytes must equal the full-decode slice"
        );
        expected_start = range.end;
    }
    assert_eq!(
        expected_start,
        decoded.len() as u64,
        "ranges sum to full length"
    );
}

// The fuzz-artifact interop replay (C-compress -> our-decode and
// our-compress -> C-decode) moved to `ffi-bench/tests/fuzz_interop.rs` so
// the library crate never links libzstd.

/// Homogeneous input — every byte the same — must NOT be split:
/// both border histograms are identical (all 512 hits on a single
/// slot), so `presplit_fingerprints_differ` returns `false` and the
/// function takes the early-return path at
/// `zstd_preSplit.c:214` returning `blockSize`.
#[test]
fn split_block_from_borders_keeps_homogeneous_block() {
    let block = vec![0xAAu8; MAX_BLOCK_SIZE as usize];
    let split = super::split_block_from_borders(&block);
    assert_eq!(split, MAX_BLOCK_SIZE as usize);
}

/// Heterogeneous input — first half all zeros, second half a
/// counter sequence — has clearly distinguishable border
/// histograms, so the borders heuristic decides to split.
///
/// The transition sits at exactly the block midpoint, so the
/// middle 512-byte sample (`block[mid-256..mid+256]`) is half
/// zeros + half counter values. That makes it roughly
/// equidistant from both border fingerprints — the
/// `abs_diff(dist_from_begin, dist_from_end) < min_distance`
/// branch fires and the heuristic returns the midpoint (64 KiB)
/// per `zstd_preSplit.c:222`. The test asserts the exact value
/// rather than just "one of {32K, 64K, 96K}" so a regression
/// to a different quantised arm cannot silently slip through.
#[test]
fn split_block_from_borders_returns_midpoint_for_centred_transition() {
    let mut block = vec![0u8; MAX_BLOCK_SIZE as usize];
    for (i, byte) in block
        .iter_mut()
        .enumerate()
        .skip(MAX_BLOCK_SIZE as usize / 2)
    {
        *byte = (i % 251 + 1) as u8;
    }
    let split = super::split_block_from_borders(&block);
    assert_eq!(
        split,
        64 * 1024,
        "centred-transition fixture must take the symmetric \
             midpoint arm (`abs_diff < min_distance`), got {split}"
    );
}

/// `level_pre_split` resolves the per-level split knob through the
/// `LevelParams` table, returning the EFFECTIVE upstream `ZSTD_splitBlock`
/// level (`splitLevels[strategy] - 2`, clamped at 0) that
/// `ZSTD_optimalBlockSize` dispatches: fast/dfast/greedy/lazy(d1) → 0
/// (from-borders), lazy2/btlazy2 → 1 (byChunks rate 43),
/// btopt/btultra/btultra2 → 2 (byChunks rate 11). `Uncompressed` has no
/// numeric level so it stays `None`.
#[test]
fn pre_split_level_dispatches_by_compression_level() {
    use crate::encoding::CompressionLevel;
    use crate::encoding::levels::config::level_pre_split;
    assert_eq!(level_pre_split(CompressionLevel::Uncompressed), None);
    // Fastest = level 1 (fast) → 0 (from-borders).
    assert_eq!(level_pre_split(CompressionLevel::Fastest), Some(0));
    // Default = level 3 (dfast) → 0 (splitLevels 1 - 2, clamped).
    assert_eq!(level_pre_split(CompressionLevel::Default), Some(0));
    // Better is a pure alias for level 7 (lazy): same as Level(7).
    assert_eq!(
        level_pre_split(CompressionLevel::Better),
        level_pre_split(CompressionLevel::Level(7)),
    );
    // Best resolves to the level-13 table row (btlazy2): pin it to that
    // numeric route so the named path can't drift from the pre-split
    // table.
    assert_eq!(
        level_pre_split(CompressionLevel::Best),
        level_pre_split(CompressionLevel::Level(13)),
    );
    assert_eq!(level_pre_split(CompressionLevel::Level(2)), Some(0)); // fast
    assert_eq!(level_pre_split(CompressionLevel::Level(4)), Some(0)); // dfast
    assert_eq!(level_pre_split(CompressionLevel::Level(5)), Some(0)); // greedy
    assert_eq!(level_pre_split(CompressionLevel::Level(7)), Some(0)); // lazy (depth 1)
    // lazy2 / btlazy2: splitLevels 3 - 2 = 1 (byChunks rate 43, hashLog 8).
    // The coarse byte-histogram tier is robust to the periodic-input
    // phantom-split the rate-5/hashLog-10 tier suffered, so it matches
    // upstream AND stays whole on periodic input (`periodic_stream_not_oversplit`).
    assert_eq!(level_pre_split(CompressionLevel::Level(8)), Some(1)); // lazy2 lower bound
    assert_eq!(level_pre_split(CompressionLevel::Level(11)), Some(1)); // lazy2 (depth 2)
    assert_eq!(level_pre_split(CompressionLevel::Level(12)), Some(1)); // lazy2 upper bound
    assert_eq!(level_pre_split(CompressionLevel::Level(13)), Some(1)); // btlazy2 lower bound
    assert_eq!(level_pre_split(CompressionLevel::Level(15)), Some(1)); // btlazy2 (depth 2)
    assert_eq!(level_pre_split(CompressionLevel::Level(16)), Some(2)); // btopt
    assert_eq!(level_pre_split(CompressionLevel::Level(22)), Some(2)); // btultra2
}

/// Regression: a homogeneous but periodic multi-block stream must not be
/// pre-split into tiny blocks at the lazy2 / btlazy2 levels. The rate-5
/// chunk sampler used to phantom-split such input at every 8 KB chunk,
/// cascading a large stream into hundreds of tiny blocks whose per-block
/// headers ballooned the output (~5x vs the lazy level next door). With
/// the rate-1 full-scan splitter the periodic stream is seen as uniform
/// and stays a few full blocks. We assert the lazy2 (L8) and btlazy2 (L15)
/// outputs stay within 2x of the lazy (L7) output on the same input, and
/// that every output round-trips.
#[test]
fn periodic_stream_not_oversplit() {
    use crate::encoding::{CompressionLevel, compress_slice_to_vec};
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];
    // 512 KB = 4 upstream zstd blocks, enough for the cascade to manifest.
    let target = 512 * 1024usize;
    let mut data = Vec::with_capacity(target);
    let mut i = 0;
    while data.len() < target {
        let line = LINES[i % LINES.len()].as_bytes();
        let take = line.len().min(target - data.len());
        data.extend_from_slice(&line[..take]);
        i += 1;
    }
    let l7 = compress_slice_to_vec(&data, CompressionLevel::Level(7)); // lazy depth1
    let l8 = compress_slice_to_vec(&data, CompressionLevel::Level(8)); // lazy2
    let l15 = compress_slice_to_vec(&data, CompressionLevel::Level(15)); // btlazy2
    assert!(
        l8.len() < l7.len() * 2,
        "lazy2 over-split periodic stream: l7={} l8={}",
        l7.len(),
        l8.len()
    );
    assert!(
        l15.len() < l7.len() * 2,
        "btlazy2 over-split periodic stream: l7={} l15={}",
        l7.len(),
        l15.len()
    );
    for out in [&l7, &l8, &l15] {
        let mut decoder = FrameDecoder::new();
        let mut round = Vec::with_capacity(data.len());
        decoder
            .decode_all_to_vec(out, &mut round)
            .expect("decode periodic stream");
        assert_eq!(round, data, "periodic stream roundtrip mismatch");
    }
}

/// End-to-end: a 256 KB payload whose SECOND 128 KB upstream zstd block carries
/// an intra-block fingerprint transition, compressed at Level(5)
/// (greedy, the pre-split path this revision routes through the cheap
/// chunk splitter), round-trips through the crate's own decoder.
///
/// The transition lives in the second block on purpose: the upstream zstd
/// `savings < 3` gate skips splitting the first block (savings start at
/// 0), so the first block is a homogeneous compressible run that banks
/// savings, and the second block is the one whose intra-block transition
/// `split_block_by_chunks()` resolves into a sub-block boundary (the
/// `pending_input.split_off(...)` path). The test asserts that split
/// decision directly so it cannot silently stop exercising the path if
/// the fixture or params drift, then proves the emitted split frame
/// round-trips. Level 13 (lazy) no longer pre-splits, hence Level 5.
#[test]
fn greedy_chunk_split_roundtrips_through_own_decoder() {
    use crate::encoding::CompressionLevel;
    let mut data = vec![0u8; 256 * 1024];
    // First 128 KB: homogeneous low-entropy run (compressible, banks
    // the savings the upstream zstd gate needs). Second 128 KB: low-entropy run
    // for its first half, then a counter sequence: a clear intra-block
    // fingerprint transition at the 192 KB midpoint for the chunk
    // splitter to find.
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = if i < 192 * 1024 {
            (i & 0x07) as u8
        } else {
            (i % 251 + 1) as u8
        };
    }

    // Directly assert the chunk splitter resolves the second block's
    // intra-block transition into a sub-block boundary once savings have
    // accrued (the compressible first block banks well over the gate).
    let second_block = &data[128 * 1024..];
    let split = super::optimal_block_size(
        CompressionLevel::Level(5),
        second_block,
        second_block.len(),
        MAX_BLOCK_SIZE as usize,
        100,
    );
    assert!(
        split < MAX_BLOCK_SIZE as usize,
        "second upstream zstd block must chunk-split at its intra-block transition, got {split}",
    );

    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Level(5));
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut decoder = FrameDecoder::new();
    let mut source = compressed.as_slice();
    decoder
        .reset(&mut source)
        .expect("frame header should parse");
    while !decoder.is_finished() {
        decoder
            .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
            .expect("decode should succeed");
    }
    let mut decoded = Vec::with_capacity(data.len());
    decoder.collect_to_writer(&mut decoded).unwrap();
    assert_eq!(decoded, data, "roundtrip must reproduce the input verbatim");
}

/// Outside-diff coverage for the FAST one-shot path.
/// `compress_slice_to_vec` / `compress_independent_frame` on a Fast level
/// routes through `run_borrowed_block_loop` (not the owned loop the test
/// above covers), which must honour `optimal_block_size` and emit a
/// sub-`MAX_BLOCK_SIZE` boundary rather than fixed 128 KiB blocks. A
/// 256 KiB input is two 128 KiB blocks when unsplit; a chunk boundary in
/// the second block yields >= 3 decoded blocks, asserted on the round-trip.
#[test]
fn fast_oneshot_borrowed_split_emits_subblock() {
    use crate::encoding::CompressionLevel;
    // First 192 KiB: homogeneous zero run (banks the savings the split
    // gate needs). The second 128 KiB block flips to a counter sequence
    // at its 64 KiB midpoint (the 192 KiB mark) — a fingerprint
    // transition the Fast from-borders splitter (split level 0) resolves
    // into a sub-block boundary.
    let mut data = vec![0u8; 256 * 1024];
    for (i, byte) in data.iter_mut().enumerate() {
        if i >= 192 * 1024 {
            *byte = (i % 251 + 1) as u8;
        }
    }

    // Pin the splitter decision for the Fast path directly (mirrors the
    // greedy test): the second upstream zstd block must resolve to a sub-block
    // boundary, so the >= 3 block count below cannot pass vacuously.
    let second_block = &data[128 * 1024..];
    assert!(
        super::optimal_block_size(
            CompressionLevel::Fastest,
            second_block,
            second_block.len(),
            MAX_BLOCK_SIZE as usize,
            100,
        ) < MAX_BLOCK_SIZE as usize,
        "fixture must resolve to a sub-block split in the second upstream zstd block",
    );

    // Drive the borrowed one-shot route explicitly (Fast level ->
    // run_borrowed_block_loop via compress_independent_frame).
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
    let frame = compressor.compress_independent_frame(&data);

    let mut decoder = FrameDecoder::new();
    let mut source = frame.as_slice();
    decoder
        .reset(&mut source)
        .expect("frame header should parse");
    while !decoder.is_finished() {
        decoder
            .decode_blocks(&mut source, crate::decoding::BlockDecodingStrategy::All)
            .expect("decode should succeed");
    }
    let mut decoded = Vec::with_capacity(data.len());
    decoder.collect_to_writer(&mut decoded).unwrap();
    assert_eq!(decoded, data, "roundtrip must reproduce the input verbatim");
    assert!(
        decoder.blocks_decoded() >= 3,
        "fast one-shot borrowed path must split the second upstream zstd block \
             (256 KiB unsplit = 2 blocks), got {} blocks",
        decoder.blocks_decoded(),
    );
}

/// Regression: `set_parameters` must key `literal_compression_disabled` off the
/// RESOLVED strategy / target length (C `ZSTD_literalsCompressionIsDisabled` =
/// `strategy == fast && targetLength > 0`), not the signed level. A negative
/// level overridden onto a non-fast strategy must keep literal (Huffman)
/// compression enabled.
#[cfg(feature = "std")]
#[test]
fn set_parameters_keeps_literals_compressed_under_nonfast_strategy_override() {
    use super::CompressionLevel;
    use crate::encoding::parameters::{CompressionParameters, Strategy};
    let data = vec![0xABu8; 256];
    let mut out = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Level(3));
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut out);
    let params = CompressionParameters::builder(CompressionLevel::Level(-5))
        .strategy(Strategy::Btultra2)
        .build()
        .expect("valid params");
    compressor.set_parameters(&params);
    assert!(
        !compressor.state.literal_compression_disabled,
        "a non-fast strategy override on a negative level must keep literals compressed",
    );
}

/// Regression: `set_compression_level` must resync
/// `state.literal_compression_disabled` so reusing a compressor and switching to
/// a negative level emits raw literals (matching C
/// `ZSTD_literalsCompressionIsDisabled`), not the Huffman-compressed literals
/// carried over from the prior non-negative level.
#[cfg(feature = "std")]
#[test]
fn set_compression_level_resyncs_literal_disable_for_negatives() {
    use super::CompressionLevel;
    let data = vec![0xABu8; 256];
    let mut out = Vec::new();
    // Construction at a non-negative level keeps literal (Huffman) compression on.
    let mut compressor = FrameCompressor::new(CompressionLevel::Level(3));
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut out);
    assert!(
        !compressor.state.literal_compression_disabled,
        "L3 construction must leave literal compression enabled",
    );
    // Switching to a negative level must immediately disable it.
    compressor.set_compression_level(CompressionLevel::Level(-5));
    assert!(
        compressor.state.literal_compression_disabled,
        "set_compression_level to a negative level must disable literal compression",
    );
}

/// Regression: `set_compression_level` followed by `compress()` must
/// refresh `state.strategy_tag` through the reset-time sync so the
/// literal-compression gates (`min_literals_to_compress`,
/// `min_gain`) use the NEW level's strategy. Picks a level pair
/// that genuinely crosses strategy bands — `Fastest` resolves to
/// `Fast`, `Level(20)` resolves to `BtUltra2` — so a missed sync
/// would leave the construction-time tag visible and trip the
/// assertion. `CompressionLevel::Best` would also pass type-wise
/// but resolves to `Lazy` today, which keeps `min_literals_to_compress`
/// in the same `shift=3 → 64-byte` band as `Fast` and weakens the
/// signal that the gate floor actually moved.
#[cfg(feature = "std")]
#[test]
fn set_compression_level_then_compress_refreshes_strategy_tag() {
    use super::CompressionLevel;
    use crate::encoding::strategy::StrategyTag;

    let data = vec![0xABu8; 256];
    let mut out = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
    let initial_tag = compressor.state.strategy_tag;
    assert_eq!(
        initial_tag,
        StrategyTag::for_compression_level(CompressionLevel::Fastest),
        "construction-time strategy_tag must reflect initial level",
    );

    // Switch to a level whose resolved strategy lives in a different
    // band, then run a full compress cycle — the matcher.reset()
    // inside `compress` is the only site that can refresh the tag.
    let new_level = CompressionLevel::Level(20);
    compressor.set_compression_level(new_level);
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut out);
    compressor.compress();

    let new_tag = compressor.state.strategy_tag;
    let expected = StrategyTag::for_compression_level(new_level);
    assert_eq!(
        new_tag, expected,
        "strategy_tag must follow set_compression_level → compress, \
             got {new_tag:?} expected {expected:?}",
    );
    assert_eq!(
        expected,
        StrategyTag::BtUltra2,
        "test fixture invariant: Level(20) must resolve to BtUltra2 \
             so the post-switch tag visibly crosses the band boundary",
    );
    assert_ne!(
        new_tag, initial_tag,
        "test fixture invariant: chosen levels must resolve to \
             different StrategyTag variants",
    );
}

/// Magicless mode (`ZSTD_f_zstd1_magicless`): encoded frame
/// MUST NOT start with the 4-byte magic prefix, AND must
/// round-trip through a magicless-aware decoder.
#[test]
fn magicless_frame_omits_magic_and_roundtrips() {
    use crate::common::MAGIC_NUM;
    let input: alloc::vec::Vec<u8> = (0..512u32).map(|i| (i ^ 0xA5) as u8).collect();

    // Encode with magicless = true.
    let mut output: Vec<u8> = Vec::new();
    let mut compressor = FrameCompressor::new(super::CompressionLevel::Default);
    compressor.set_magicless(true);
    compressor.set_source(input.as_slice());
    compressor.set_drain(&mut output);
    compressor.compress();

    // 1. Encoded output must NOT begin with the zstd magic number.
    assert!(
        !output.starts_with(&MAGIC_NUM.to_le_bytes()),
        "magicless frame must omit the 4-byte magic prefix",
    );

    // 2. A magicless-aware decoder must round-trip the payload.
    let mut decoder = crate::decoding::FrameDecoder::new();
    decoder.set_magicless(true);
    let mut cursor: &[u8] = output.as_slice();
    decoder.init(&mut cursor).expect("magicless init");
    decoder
        .decode_blocks(&mut cursor, crate::decoding::BlockDecodingStrategy::All)
        .expect("decode_blocks");
    let mut decoded: Vec<u8> = Vec::new();
    decoder
        .collect_to_writer(&mut decoded)
        .expect("collect_to_writer");
    assert_eq!(decoded, input, "magicless roundtrip must preserve bytes");

    // 3. A standard (magicful) decoder MUST reject a magicless
    //    frame at the header-read step — the first 4 bytes are
    //    the frame-header descriptor + window / dictionary / FCS
    //    metadata, not the magic. We accept either
    //    `BadMagicNumber` (typical case: first 4 bytes don't
    //    match `MAGIC_NUM` and don't fall in the skippable-frame
    //    magic range) or `SkipFrame` (rare: the first 4 bytes
    //    coincidentally land in `0x184D2A50..=0x184D2A5F`). Both
    //    prove the standard decoder did not treat the bytes as a
    //    real magicful frame.
    use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    let mut std_decoder = crate::decoding::FrameDecoder::new();
    let std_init = std_decoder.init(output.as_slice());
    match std_init {
        Err(FrameDecoderError::ReadFrameHeaderError(
            ReadFrameHeaderError::BadMagicNumber(_) | ReadFrameHeaderError::SkipFrame { .. },
        )) => {}
        other => panic!(
            "standard decoder must reject a magicless frame with \
                 ReadFrameHeaderError::BadMagicNumber or SkipFrame, got {other:?}",
        ),
    }
}

/// A reused `FrameCompressor` must emit byte-identical frames to a
/// fresh compressor per input across both the borrowed (Fast) and
/// owned (Dfast/Lazy/Greedy/Uncompressed) backends. This proves
/// `prepare_frame` fully resets the per-frame state (matcher window,
/// content hasher, FSE/Huffman seeds) between independent frames; a
/// missed reset would corrupt frame N>=2's header checksum or matches.
/// Each emitted frame must also round-trip.
#[test]
fn compress_independent_frame_reuse_matches_fresh_and_roundtrips() {
    use crate::encoding::{CompressionLevel, compress_slice_to_vec};
    let levels = [
        CompressionLevel::Uncompressed,
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Better,
        CompressionLevel::Best,
        CompressionLevel::Level(5),
    ];
    let inputs: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x00],
        b"the quick brown fox jumps over the lazy dog\n".to_vec(),
        vec![0x7Eu8; 50_000],          // highly compressible
        generate_data(0xABCD, 70_000), // pseudo-random
        generate_data(0x1234, 200_000),
    ];
    for level in levels {
        let mut cctx: FrameCompressor = FrameCompressor::new(level);
        for data in &inputs {
            let reused = cctx.compress_independent_frame(data);
            let fresh = compress_slice_to_vec(data, level);
            assert_eq!(
                reused,
                fresh,
                "reused frame != fresh frame for len={} level={:?}",
                data.len(),
                level,
            );
            let mut decoder = FrameDecoder::new();
            let mut decoded = Vec::with_capacity(data.len());
            decoder.decode_all_to_vec(&reused, &mut decoded).unwrap();
            assert_eq!(
                decoded,
                *data,
                "roundtrip failed for len={} level={:?}",
                data.len(),
                level,
            );
        }
    }
}

/// `compress_independent_frame_into` must replace (not append to) the
/// caller's buffer each call, so a smaller frame after a larger one
/// yields exactly the smaller frame, and the reused buffer's content
/// matches a fresh compression of the same input.
#[test]
fn compress_independent_frame_into_replaces_buffer_contents() {
    use crate::encoding::{CompressionLevel, compress_slice_to_vec};
    let large = vec![0x11u8; 40_000];
    let small = b"short payload".to_vec();
    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    let mut out = Vec::new();
    cctx.compress_independent_frame_into(&large, &mut out);
    let frame_large = out.clone();
    // Reusing the same buffer for a smaller frame must clear it first.
    cctx.compress_independent_frame_into(&small, &mut out);
    assert_eq!(
        out,
        compress_slice_to_vec(&small, CompressionLevel::Default),
        "reused buffer must hold exactly the second frame",
    );
    // The first frame, captured before reuse, still round-trips.
    let mut decoder = FrameDecoder::new();
    let mut decoded = Vec::with_capacity(large.len());
    decoder
        .decode_all_to_vec(&frame_large, &mut decoded)
        .unwrap();
    assert_eq!(decoded, large);
}

/// A sticky dictionary set once on a reused compressor must be primed
/// into every independent frame (mirroring `ZSTD_CCtx_loadDictionary`):
/// each frame decodes with the dictionary and is byte-identical to a
/// fresh compressor carrying the same dictionary. This proves
/// `prepare_frame` re-primes the dictionary (matcher content + offset
/// history + entropy seed) every call rather than only on the first.
#[test]
fn compress_independent_frame_reuses_sticky_dictionary() {
    use crate::encoding::CompressionLevel;
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let dict_content = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
    let mut payload_a = Vec::new();
    for _ in 0..8 {
        payload_a.extend_from_slice(&dict_content.dict_content[..2048]);
    }
    let payload_b = b"a different second frame payload, still dict-attached".to_vec();
    let inputs = [payload_a, payload_b];

    let mut cctx: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
    cctx.set_dictionary_from_bytes(dict_raw)
        .expect("dictionary bytes should parse");

    for data in &inputs {
        let reused = cctx.compress_independent_frame(data);
        // Fresh compressor carrying the same sticky dictionary.
        let mut fresh_enc: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
        fresh_enc
            .set_dictionary_from_bytes(dict_raw)
            .expect("dictionary bytes should parse");
        let fresh = fresh_enc.compress_independent_frame(data);
        assert_eq!(
            reused,
            fresh,
            "reused dict frame != fresh dict frame, len={}",
            data.len(),
        );
        // Round-trip with the dictionary on the decode side.
        let dict_for_decoder = crate::decoding::Dictionary::decode_dict(dict_raw).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.add_dict(dict_for_decoder).unwrap();
        let mut decoded = Vec::with_capacity(data.len());
        decoder.decode_all_to_vec(&reused, &mut decoded).unwrap();
        assert_eq!(&decoded, data, "dict roundtrip failed, len={}", data.len());
    }
}

/// Walk a frame's block list, returning `(block_type, block_size, last)` per
/// physical block. `block_type`: 0 = Raw, 1 = RLE, 2 = Compressed.
fn frame_block_list(frame: &[u8]) -> Vec<(u8, usize, bool)> {
    let desc = frame[4];
    let fcs_flag = desc >> 6;
    let single_segment = (desc >> 5) & 1 == 1;
    let checksum = (desc >> 2) & 1 == 1;
    let dict_id_bytes = match desc & 3 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let fcs_bytes = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut pos = 4 + 1 + usize::from(!single_segment) + dict_id_bytes + fcs_bytes;
    let end = frame.len() - if checksum { 4 } else { 0 };
    let mut blocks = Vec::new();
    while pos + 3 <= end {
        let h =
            frame[pos] as usize | (frame[pos + 1] as usize) << 8 | (frame[pos + 2] as usize) << 16;
        let last = h & 1 == 1;
        let btype = ((h >> 1) & 3) as u8;
        let bsize = h >> 3;
        let advance = if btype == 1 { 1 } else { bsize };
        blocks.push((btype, bsize, last));
        pos += 3 + advance;
        if last {
            break;
        }
    }
    blocks
}

/// An input that is an exact multiple of `MAX_BLOCK_SIZE` must NOT emit a
/// spurious trailing empty Raw block: the last REAL block carries the
/// `last_block` flag, matching the C encoder on `ZSTD_e_end`. Exercises both
/// the one-shot slice loop (`compress_independent_frame`) and the streaming
/// `Read` loop (`set_source` + `compress`), on incompressible input (Raw
/// blocks) and highly compressible input (Compressed/RLE blocks). Before the
/// fix, each frame ended with an extra `R0!` block (3 wasted bytes).
#[test]
fn exact_block_multiple_marks_last_real_block() {
    // The fix is driven by `block_capacity`, so cover both the default 128 KiB
    // cap (`None`) and a smaller configured `target_block_size` — the EOF path
    // must mark the last real block in both.
    const CUSTOM_CAP: u32 = 16 * 1024;
    for &(cap, target) in &[
        (MAX_BLOCK_SIZE as usize, None),
        (CUSTOM_CAP as usize, Some(CUSTOM_CAP)),
    ] {
        for &nblk in &[1usize, 2, 3] {
            for &compressible in &[false, true] {
                let input: Vec<u8> = if compressible {
                    vec![0x7Au8; cap * nblk]
                } else {
                    generate_data(0xC0FF_EE11, cap * nblk)
                };

                // One-shot slice path.
                let mut oneshot: FrameCompressor =
                    FrameCompressor::new(super::CompressionLevel::Default);
                oneshot.set_target_block_size(target);
                let frame_os = oneshot.compress_independent_frame(&input);
                let blocks_os = frame_block_list(&frame_os);
                let last_os = *blocks_os.last().expect("at least one block");
                assert!(
                    last_os.2,
                    "one-shot last block must set last_block (cap={cap}, nblk={nblk}, compressible={compressible}): {blocks_os:?}"
                );
                assert!(
                    !(last_os.0 == 0 && last_os.1 == 0),
                    "one-shot must not emit a trailing empty Raw block (cap={cap}, nblk={nblk}, compressible={compressible}): {blocks_os:?}"
                );

                // Streaming Read loop.
                let mut output = Vec::new();
                let mut streaming = FrameCompressor::new(super::CompressionLevel::Default);
                streaming.set_target_block_size(target);
                streaming.set_source(input.as_slice());
                streaming.set_drain(&mut output);
                streaming.compress();
                let blocks_st = frame_block_list(&output);
                let last_st = *blocks_st.last().expect("at least one block");
                assert!(
                    last_st.2,
                    "streaming last block must set last_block (cap={cap}, nblk={nblk}, compressible={compressible}): {blocks_st:?}"
                );
                assert!(
                    !(last_st.0 == 0 && last_st.1 == 0),
                    "streaming must not emit a trailing empty Raw block (cap={cap}, nblk={nblk}, compressible={compressible}): {blocks_st:?}"
                );

                // Both frames must round-trip back to the original bytes.
                for frame in [&frame_os, &output] {
                    let mut decoder = FrameDecoder::new();
                    let mut decoded = Vec::with_capacity(input.len());
                    decoder.decode_all_to_vec(frame, &mut decoded).unwrap();
                    assert_eq!(
                        decoded, input,
                        "roundtrip mismatch (cap={cap}, nblk={nblk}, compressible={compressible})"
                    );
                }
            }
        }
    }
}

#[test]
fn dict_compress_bt_level_tiny_source_round_trips_through_prime_dms_bt() {
    // End-to-end cover for the dictionary match binary-tree (`prime_dms_bt`, a
    // ZSTD_dictMatchState analog built only on the BT strategies, level >= 13):
    // compress a tiny source with a small raw-content dictionary at level 19
    // (BtUltra2) and round-trip it. This drives the BT dict-prime + dict-match
    // search path that non-BT levels (Fast/Dfast/Lazy) never reach.
    //
    // The `prime_dms_bt` dms-table sizing previously used
    // `ceil_log2(region).clamp(10, hash_log)`, which panicked ("min > max") when
    // the matcher's `hash_log` adjusted below the 10 floor — the exact bound
    // arithmetic is pinned directly by `storage::dms_hash_log_tests`. On this
    // build the window-log floor keeps `hash_log >= 10` so this end-to-end path
    // stays above the boundary; the unit test exercises `hash_log < 10`.
    let raw_dict: Vec<u8> = (0..100u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let dict_id = 1u32;
    let dict_for_encoder =
        crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict.clone()).unwrap();
    let dict_for_decoder =
        crate::decoding::Dictionary::from_raw_content(dict_id, raw_dict).unwrap();

    let data = b"hello world".to_vec();

    let mut compressor: FrameCompressor =
        FrameCompressor::new(super::CompressionLevel::from_level(19));
    compressor
        .set_dictionary(dict_for_encoder)
        .expect("raw-content dictionary should attach");
    // Runs the BT dict-prime + dict-match path end to end.
    let out = compressor.compress_independent_frame(data.as_slice());

    let mut decoder = FrameDecoder::new();
    decoder.add_dict(dict_for_decoder).unwrap();
    let mut decoded = Vec::with_capacity(data.len());
    decoder
        .decode_all_to_vec(&out, &mut decoded)
        .expect("dict BT-level frame should round-trip");
    assert_eq!(decoded, data);
}
