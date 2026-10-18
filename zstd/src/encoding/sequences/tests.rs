use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::decoding::FrameDecoder;
use crate::encoding::FrameCompressor;

fn seq(offset: u32, lit_len: u32, match_len: u32) -> ExternalSequence {
    ExternalSequence {
        offset,
        lit_len,
        match_len,
        rep: 0,
    }
}

fn decode(frame: &[u8]) -> Vec<u8> {
    // Decodes into spare capacity; every input here is far below this.
    let mut out = Vec::with_capacity(1 << 20);
    FrameDecoder::new()
        .decode_all_to_vec(frame, &mut out)
        .expect("frame decodes");
    out
}

fn default_compressor() -> FrameCompressor {
    FrameCompressor::new(CompressionLevel::Default)
}

/// Read the input `parse` and `literals` rebuild to, over `window`, in reads
/// of `chunk` bytes so a match is rebuilt across several of them. The bytes
/// held for the matches never pass two windows and one read.
fn rebuild(
    parse: &[ExternalSequence],
    literals: &[u8],
    dict: &[u8],
    window: usize,
    chunk: usize,
) -> Result<Vec<u8>, SequenceError> {
    let mut source = RebuiltSource::new(parse, literals, dict, window)?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = crate::io::Read::read(&mut source, &mut buf).unwrap();
        assert!(source.history.len() <= 2 * window + chunk);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out.len(), source.len());
    Ok(out)
}

fn replay_frame(input: &[u8], parse: &[ExternalSequence], max_block: Option<u32>) -> Vec<u8> {
    let mut frame = Vec::new();
    let mut compressor = FrameCompressor::new_with_matcher(
        SequenceReplay::new(parse, 1 << 20),
        CompressionLevel::Default,
    );
    compressor.set_max_block_size(max_block);
    compressor.set_source(input);
    compressor.set_drain(&mut frame);
    compressor.compress();
    frame
}

/// A periodic input and a parse of it whose matches run far past any block
/// size, so every block cut splits a match.
fn periodic_input_and_parse(len: usize, period: usize) -> (Vec<u8>, Vec<ExternalSequence>) {
    let input: Vec<u8> = (0..len).map(|i| (i % period) as u8 * 7 + 1).collect();
    let parse = vec![seq(period as u32, period as u32, (len - period) as u32)];
    (input, parse)
}

/// Replaying a parse through blocks smaller than its matches round-trips:
/// the cut splits each match and the frame still decodes to the input.
#[test]
fn replay_round_trips_across_block_cuts() {
    let (input, parse) = periodic_input_and_parse(10_000, 13);
    let frame = replay_frame(&input, &parse, Some(1024));
    assert_eq!(decode(&frame), input);
    assert!(frame.len() < input.len() / 4, "matches were used");
}

/// A cut that leaves a match piece shorter than MIN_MATCH emits it as
/// literals instead of an invalid short match.
#[test]
fn replay_turns_short_match_pieces_into_literals() {
    // The first block is 1024 bytes; the match starts at 1022, so the cut
    // leaves two of its bytes in the first block.
    let mut input: Vec<u8> = (0..1022u32).map(|i| (i * 31 % 251) as u8).collect();
    let tail: Vec<u8> = input[..100].to_vec();
    input.extend_from_slice(&tail);
    let parse = vec![seq(1022, 1022, 100)];
    let frame = replay_frame(&input, &parse, Some(1024));
    assert_eq!(decode(&frame), input);
}

/// Bytes past the end of a parse without delimiters go out as literals.
#[test]
fn replay_emits_uncovered_tail_as_literals() {
    let input = b"abcabcabcabc-tail bytes".to_vec();
    let parse = vec![seq(3, 3, 9)];
    let frame = replay_frame(&input, &parse, None);
    assert_eq!(decode(&frame), input);
}

/// Block delimiters inside the parse only pass their literals on.
#[test]
fn replay_follows_block_delimiters() {
    let input = b"xyzxyzxyz!qrsqrsqrs?".to_vec();
    let parse = vec![
        seq(3, 3, 6),
        ExternalSequence::delimiter(1),
        seq(3, 3, 6),
        ExternalSequence::delimiter(1),
    ];
    let frame = replay_frame(&input, &parse, None);
    assert_eq!(decode(&frame), input);
}

/// Capturing the default encoder's parse and replaying it gives a frame that
/// decodes to the same input.
#[test]
fn captured_parse_replays_to_the_input() {
    let input = b"the quick brown fox jumps over the lazy dog; ".repeat(400);
    let mut compressor = default_compressor();
    compressor.set_sequence_capture(true);
    let frame = compressor.compress_independent_frame(&input);
    let parse = compressor.take_captured_sequences();
    assert_eq!(decode(&frame), input);
    assert_eq!(
        validate_parse(&parse, input.len(), 1 << 20, 0, true, 1 << 17),
        Ok(())
    );
    let literals: Vec<u8> = {
        let mut lits = Vec::new();
        let mut at = 0usize;
        for s in &parse {
            lits.extend_from_slice(&input[at..at + s.lit_len as usize]);
            at += (s.lit_len + s.match_len) as usize;
        }
        lits
    };
    assert_eq!(
        rebuild(&parse, &literals, &[], 1 << 20, 4096).unwrap(),
        input
    );
    let replayed = replay_frame(&input, &parse, None);
    assert_eq!(decode(&replayed), input);
}

/// Validation rejects each malformed shape with its own error.
#[test]
fn validate_parse_reports_each_defect() {
    assert_eq!(
        validate_parse(
            &[seq(3, 3, 2), seq(3, 0, 4)],
            12,
            1 << 20,
            0,
            false,
            1 << 17
        ),
        Err(SequenceError::MatchTooShort { index: 0 })
    );
    assert_eq!(
        validate_parse(&[seq(0, 3, 4)], 12, 1 << 20, 0, false, 1 << 17),
        Err(SequenceError::ZeroOffset { index: 0 })
    );
    assert_eq!(
        validate_parse(&[seq(3, 3, 20)], 12, 1 << 20, 0, false, 1 << 17),
        Err(SequenceError::LengthMismatch)
    );
    // With explicit delimiters the parse must reach the end.
    assert_eq!(
        validate_parse(
            &[seq(3, 3, 4), ExternalSequence::delimiter(0)],
            12,
            1 << 20,
            0,
            true,
            1 << 17
        ),
        Err(SequenceError::LengthMismatch)
    );
    assert_eq!(
        validate_parse(
            &[seq(3, 3, 4), ExternalSequence::delimiter(5)],
            12,
            1 << 20,
            0,
            true,
            4
        ),
        Err(SequenceError::BlockTooLarge)
    );
    // The dictionary extends the reach until the output passes the window.
    assert_eq!(
        validate_parse(&[seq(10, 3, 4)], 12, 1 << 20, 8, false, 1 << 17),
        Ok(())
    );
    assert_eq!(
        validate_parse(&[seq(10, 20, 4)], 30, 16, 8, false, 1 << 17),
        Ok(())
    );
    assert_eq!(
        validate_parse(&[seq(20, 20, 4)], 30, 16, 8, false, 1 << 17),
        Err(SequenceError::OffsetTooFar { index: 0 })
    );
}

/// Rebuilding reads a match that starts in the dictionary on into the output,
/// repeats an overlapping match, and keeps unused literals as the tail,
/// whether a read takes one byte or all of it.
#[test]
fn rebuild_reads_dictionary_overlaps_and_tail() {
    let dict = b"0123456789";
    let parse = [seq(5, 2, 6), seq(1, 1, 4)];
    for chunk in [1, 3, 64] {
        assert_eq!(
            rebuild(&parse, b"abx!", dict, 1 << 10, chunk).unwrap(),
            b"ab789ab7xxxxx!"
        );
    }

    assert!(matches!(
        rebuild(&[seq(20, 2, 4)], b"ab", dict, 1 << 10, 8),
        Err(SequenceError::OffsetTooFar { index: 0 })
    ));
    // While the output is within the window a match reaches into the whole
    // dictionary, as `validate_parse` lets it; past the window, only the
    // window.
    assert_eq!(
        rebuild(&[seq(5, 2, 4)], b"ab", dict, 4, 8).unwrap(),
        b"ab789a"
    );
    assert_eq!(
        validate_parse(&[seq(5, 2, 4)], 6, 4, dict.len(), false, 1 << 17),
        Ok(())
    );
    assert!(matches!(
        rebuild(&[seq(5, 6, 4)], b"abcdef", dict, 4, 8),
        Err(SequenceError::OffsetTooFar { index: 0 })
    ));
    assert!(matches!(
        rebuild(&[seq(0, 2, 4)], b"ab", dict, 1 << 10, 8),
        Err(SequenceError::ZeroOffset { index: 0 })
    ));
    assert!(matches!(
        rebuild(&[seq(1, 5, 4)], b"ab", dict, 1 << 10, 8),
        Err(SequenceError::LengthMismatch)
    ));
}

/// A match reaching deep into the dictionary early on holds more than the
/// window only while it lasts: the long run of literals after it is held to
/// the window again.
#[test]
fn rebuild_lets_a_deep_match_go_after_it_ends() {
    let dict: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
    let literals: Vec<u8> = (0..10_000u32).map(|i| (i * 13 + 1) as u8).collect();
    let parse = [seq(900, 0, 20)];
    let mut expected = dict[100..120].to_vec();
    expected.extend_from_slice(&literals);
    assert_eq!(rebuild(&parse, &literals, &dict, 16, 8).unwrap(), expected);

    // While it lasts the match keeps its whole reach, even once it copies
    // from its own output and the history is trimmed under it.
    let parse = [seq(50, 0, 200)];
    let mut expected = dict[950..].to_vec();
    for i in 50..200 {
        expected.push(expected[i - 50]);
    }
    let mut source = RebuiltSource::new(&parse, &[], &dict, 16).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 8];
    loop {
        let n = crate::io::Read::read(&mut source, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, expected);
}

/// An input many windows long rebuilds whole while only about a window of it
/// is held: the frame `ZSTD_compressSequencesAndLiterals` builds needs no
/// copy of the whole input.
#[test]
fn rebuild_holds_a_window_not_the_input() {
    let input = b"a window of text that repeats; ".repeat(2_000);
    let window = 1usize << 10;
    let params = crate::encoding::CompressionParameters::builder(CompressionLevel::Default)
        .window_log(10)
        .build()
        .unwrap();
    let mut compressor = default_compressor();
    compressor.set_parameters(&params);
    compressor.set_sequence_capture(true);
    let _ = compressor.compress_independent_frame(&input);
    let parse = compressor.take_captured_sequences();
    let mut literals = Vec::new();
    let mut at = 0usize;
    for s in &parse {
        literals.extend_from_slice(&input[at..at + s.lit_len as usize]);
        at += (s.lit_len + s.match_len) as usize;
    }
    assert_eq!(rebuild(&parse, &literals, &[], window, 700).unwrap(), input);
}

/// Merging moves each delimiter's literals onto the sequence after it and
/// keeps the last delimiter's out of the array.
#[test]
fn merge_moves_delimiter_literals_forward() {
    let mut parse = [
        seq(4, 2, 5),
        ExternalSequence::delimiter(3),
        seq(9, 1, 4),
        ExternalSequence::delimiter(6),
    ];
    assert_eq!(merge_block_delimiters(&mut parse), 2);
    assert_eq!(parse[..2], [seq(4, 2, 5), seq(9, 4, 4)]);
}

/// With no literals before a match the repeat codes shift by one and code 3
/// is the first repeat offset less one (RFC 8878 3.1.1.5).
#[test]
fn plain_offset_follows_the_zero_literal_shift() {
    let mut history = [1, 4, 8];
    // Code 1 without literals is rep[1]; it moves to the front.
    assert_eq!(plain_offset(1, 0, &mut history), 4);
    assert_eq!(history, [4, 1, 8]);
    // Code 3 without literals is rep[0] - 1.
    assert_eq!(plain_offset(3, 0, &mut history), 3);
    assert_eq!(history, [3, 4, 1]);
    // Code 1 with literals is rep[0] and leaves the history alone.
    assert_eq!(plain_offset(1, 2, &mut history), 3);
    assert_eq!(history, [3, 4, 1]);
    // A plain offset code is the offset plus three.
    assert_eq!(plain_offset(40 + 3, 2, &mut history), 40);
    assert_eq!(history, [40, 3, 4]);
}

/// A captured parse numbers its repeat offsets against the history each
/// sequence follows, as a decoder replaying the parse would see it.
#[test]
fn captured_parse_numbers_repeat_offsets() {
    let mut input = Vec::new();
    for i in 0..3000u32 {
        input.extend_from_slice(b"ab");
        input.push((i % 7) as u8);
        input.extend_from_slice(b"cdefgh");
    }
    let mut compressor = default_compressor();
    compressor.set_sequence_capture(true);
    let _frame = compressor.compress_independent_frame(&input);
    let parse = compressor.take_captured_sequences();
    let mut history = [1u32, 4, 8];
    let mut repeats = 0;
    for s in parse.iter().filter(|s| !s.is_delimiter()) {
        let code =
            crate::encoding::blocks::encode_offset_with_history(s.offset, s.lit_len, &mut history);
        assert_eq!(s.rep, if code <= 3 { code } else { 0 }, "{s:?}");
        repeats += usize::from(s.rep != 0);
    }
    assert!(repeats > 0, "the input repeats its offsets");
}

/// sequence_bound covers a parse made only of minimal matches split into
/// minimal blocks.
#[test]
fn sequence_bound_covers_worst_case() {
    let src = 10_000usize;
    let matches = src / MIN_MATCH as usize;
    let blocks = src.div_ceil(crate::common::MIN_MAX_BLOCK_SIZE as usize);
    assert!(sequence_bound(src) >= matches + blocks);
}

/// Parses only the run of equal bytes a block starts with, as one literal
/// and a match at offset 1; the rest of the block is literals.
struct LeadingRun;

impl SequenceProducer for LeadingRun {
    fn produce(
        &mut self,
        block: &[u8],
        _window_size: u64,
        out: &mut Vec<ExternalSequence>,
    ) -> Result<(), SequenceProducerError> {
        let run = block.iter().take_while(|b| **b == block[0]).count();
        if run > 4 {
            out.push(seq(1, 1, run as u32 - 1));
            out.push(ExternalSequence::delimiter((block.len() - run) as u32));
        } else {
            out.push(ExternalSequence::delimiter(block.len() as u32));
        }
        Ok(())
    }
}

struct AlwaysFails;

impl SequenceProducer for AlwaysFails {
    fn produce(
        &mut self,
        _block: &[u8],
        _window_size: u64,
        _out: &mut Vec<ExternalSequence>,
    ) -> Result<(), SequenceProducerError> {
        Err(SequenceProducerError)
    }
}

/// A registered producer's parse is what the frame carries.
#[test]
fn producer_parse_builds_the_frame() {
    let mut input = vec![9u8; 50_000];
    input.extend((0..5_000u32).map(|i| (i * 131 % 253) as u8));
    let mut compressor = default_compressor();
    compressor.set_sequence_producer(Some(alloc::boxed::Box::new(LeadingRun)), false);
    let frame = compressor.compress_independent_frame(&input);
    assert_eq!(decode(&frame), input);
    assert!(!compressor.take_sequence_producer_failure());
    // The run collapsed into the producer's matches.
    assert!(frame.len() < input.len() / 5, "{}", frame.len());
}

/// A failing producer without fallback leaves blocks as literals only and
/// reports it; with fallback the built-in matcher finds the matches.
#[test]
fn producer_failure_falls_back_or_reports() {
    let input = b"fallback fallback fallback ".repeat(500);

    let mut strict = default_compressor();
    strict.set_sequence_producer(Some(alloc::boxed::Box::new(AlwaysFails)), false);
    let literal_frame = strict.compress_independent_frame(&input);
    assert_eq!(decode(&literal_frame), input);
    assert!(strict.take_sequence_producer_failure());
    assert!(!strict.take_sequence_producer_failure(), "taking clears it");

    let mut lenient = default_compressor();
    lenient.set_sequence_producer(Some(alloc::boxed::Box::new(AlwaysFails)), true);
    let matched_frame = lenient.compress_independent_frame(&input);
    assert_eq!(decode(&matched_frame), input);
    assert!(!lenient.take_sequence_producer_failure());
    // Literals alone only entropy-code the text; the matcher collapses it.
    assert!(matched_frame.len() * 20 < literal_frame.len());
}

/// A producer next to an attached dictionary at a Fast level keeps the frame
/// on the path that asks the producer: the in-place dictionary scan never
/// would, and refuses to run with one installed.
#[test]
fn producer_with_an_attached_dictionary_at_a_fast_level() {
    let dict_bytes = b"tenant=demo table=orders region=eu status=shipped\n".repeat(20);
    let dict =
        crate::encoding::EncoderDictionary::from_serialized_or_raw_content(&dict_bytes).unwrap();
    let mut input = vec![9u8; 1500];
    input.extend_from_slice(&dict_bytes[..500]);
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
    compressor.set_encoder_dictionary(dict).unwrap();
    compressor.set_sequence_producer(Some(alloc::boxed::Box::new(LeadingRun)), false);
    let frame = compressor.compress_independent_frame(&input);
    let raw = crate::decoding::Dictionary::from_raw_content(0, dict_bytes.clone()).unwrap();
    let handle = crate::decoding::DictionaryHandle::from_dictionary(raw);
    let mut out = vec![0u8; input.len()];
    let n = FrameDecoder::new()
        .decode_all_with_dict_handle(&frame, &mut out, &handle)
        .expect("frame decodes with the dictionary");
    assert_eq!(&out[..n], &input[..]);
    assert!(frame.len() < 700, "the run went to the producer's match");
}

/// Reserves room for `0` sequences in the parse buffer, then parses the
/// block as literals.
struct Reserving(usize);

impl SequenceProducer for Reserving {
    fn produce(
        &mut self,
        block: &[u8],
        _window_size: u64,
        out: &mut Vec<ExternalSequence>,
    ) -> Result<(), SequenceProducerError> {
        out.reserve(self.0);
        out.push(ExternalSequence::delimiter(block.len() as u32));
        Ok(())
    }
}

/// The parse buffer a producer leaves behind is held for the next block, so
/// the compressor's footprint counts it.
#[test]
fn heap_size_counts_the_producer_parse() {
    let input = b"some input for one block".repeat(40);
    let mut small = default_compressor();
    small.set_sequence_producer(Some(alloc::boxed::Box::new(Reserving(0))), false);
    let _ = small.compress_independent_frame(&input);
    let mut large = default_compressor();
    large.set_sequence_producer(Some(alloc::boxed::Box::new(Reserving(100_000))), false);
    let _ = large.compress_independent_frame(&input);
    // Less the handful of sequences the other compressor's buffer holds.
    let held = (100_000 - 16) * core::mem::size_of::<ExternalSequence>();
    assert!(
        large.heap_size() >= small.heap_size() + held,
        "{} vs {} + {held}",
        large.heap_size(),
        small.heap_size()
    );
}

/// An uncompressed frame's blocks are captured as delimiters alone, so the
/// parse still covers the input; an empty input captures nothing, as upstream
/// `ZSTD_generateSequences` returns no sequence for it (its block loop never
/// runs, zstd_compress.c:4610).
#[test]
fn capture_covers_an_uncompressed_frame() {
    let input = b"stored as it is ".repeat(300);
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Uncompressed);
    compressor.set_max_block_size(Some(1024));
    compressor.set_sequence_capture(true);
    let frame = compressor.compress_independent_frame(&input);
    let parse = compressor.take_captured_sequences();
    assert!(parse.iter().all(ExternalSequence::is_delimiter));
    let covered: usize = parse.iter().map(|s| s.lit_len as usize).sum();
    assert_eq!(covered, input.len());
    assert_eq!(parse.len(), block_count(&frame));

    let _ = compressor.compress_independent_frame(&[]);
    assert!(compressor.take_captured_sequences().is_empty());
}

/// A producer installed mid-frame at an optimal level with long-distance
/// matching on: the blocks before it left their long matches with the
/// optimal parser, and a piece between long matches that the producer fails
/// on is matched without them, so the frame still decodes to its input.
#[cfg(feature = "ldm")]
#[test]
fn producer_fallback_ignores_long_matches_of_earlier_blocks() {
    let mut state = 0x1357_9BDFu32;
    let mut noise = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    };
    let base = noise(300_000);
    let params = crate::encoding::CompressionParameters::builder(CompressionLevel::Level(19))
        .window_log(21)
        .enable_long_distance_matching(true)
        .build()
        .unwrap();
    let mut context = crate::encoding::CompressionContext::new(CompressionLevel::Level(19));
    context.set_parameters(&params).unwrap();
    let mut frame = Vec::new();
    let mut input = Vec::new();
    let mut feed = |context: &mut crate::encoding::CompressionContext, data: &[u8]| {
        let mut rest = data;
        while !rest.is_empty() {
            let n = context.write(&mut frame, rest).unwrap();
            rest = &rest[n..];
        }
        input.extend_from_slice(data);
    };
    feed(&mut context, &base);
    feed(&mut context, &base);
    context.set_sequence_producer(Some(alloc::boxed::Box::new(AlwaysFails)), true);
    let mut spliced = Vec::new();
    for (index, piece) in base.chunks(10_000).enumerate() {
        spliced.extend_from_slice(piece);
        spliced.extend(noise(97 + index));
    }
    feed(&mut context, &spliced);
    context.finish_frame(&mut frame).unwrap();
    let mut decoded = Vec::with_capacity(input.len());
    FrameDecoder::new()
        .decode_all_to_vec(&frame, &mut decoded)
        .expect("the frame decodes");
    assert!(decoded == input, "the frame decodes to other bytes");
}

/// The captured parse the compressor holds counts in its footprint.
#[test]
fn heap_size_counts_the_captured_parse() {
    let input = b"captured sequences take room ".repeat(400);
    let mut plain = default_compressor();
    let _ = plain.compress_independent_frame(&input);
    let mut capturing = default_compressor();
    capturing.set_sequence_capture(true);
    let _ = capturing.compress_independent_frame(&input);
    let holding = capturing.heap_size();
    let held = capturing.take_captured_sequences().len() * core::mem::size_of::<ExternalSequence>();
    assert!(held > 0);
    assert!(
        holding >= plain.heap_size() + held,
        "{holding} vs {} + {held}",
        plain.heap_size()
    );
}

/// Installing a producer again, as a caller does before every frame, keeps
/// the parse buffer the previous one grew instead of growing a new one.
#[test]
fn reinstalling_a_producer_keeps_the_parse_buffer() {
    let input = b"some input for one block".repeat(40);
    let mut compressor = default_compressor();
    compressor.set_sequence_producer(Some(alloc::boxed::Box::new(Reserving(100_000))), false);
    let _ = compressor.compress_independent_frame(&input);
    let grown = compressor.heap_size();
    compressor.set_sequence_producer(Some(alloc::boxed::Box::new(Reserving(0))), false);
    assert_eq!(compressor.heap_size(), grown);
}

/// With long-distance matching on, a producer that finds nothing still gets
/// the long matches: they are taken outright and the producer parses the
/// literals between them, at a lazy level and at an optimal one alike. One
/// that fails, with fallback, leaves those pieces to the built-in matcher.
#[cfg(feature = "ldm")]
#[test]
fn producer_keeps_long_distance_matches() {
    let mut state = 0x9E37_79B9u32;
    let half: Vec<u8> = (0..200_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let mut input = half.clone();
    input.extend_from_slice(&half);
    for (level, fails) in [
        (CompressionLevel::Default, false),
        (CompressionLevel::Level(19), false),
        (CompressionLevel::Default, true),
        (CompressionLevel::Level(19), true),
    ] {
        let params = crate::encoding::CompressionParameters::builder(level)
            .window_log(20)
            .enable_long_distance_matching(true)
            .build()
            .unwrap();
        let mut compressor = default_compressor();
        compressor.set_parameters(&params);
        let producer: alloc::boxed::Box<dyn SequenceProducer> = if fails {
            alloc::boxed::Box::new(AlwaysFails)
        } else {
            alloc::boxed::Box::new(Reserving(0))
        };
        compressor.set_sequence_producer(Some(producer), fails);
        let frame = compressor.compress_independent_frame(&input);
        assert_eq!(decode(&frame), input);
        assert!(!compressor.take_sequence_producer_failure());
        // The second half is one long match away from the first.
        assert!(
            frame.len() < half.len() + half.len() / 10,
            "{level:?}: {} bytes",
            frame.len()
        );
    }
}

/// Count the blocks of a frame by walking their headers.
fn block_count(frame: &[u8]) -> usize {
    let mut at = crate::decoding::frame_header_size(frame).unwrap();
    let mut blocks = 0;
    loop {
        let header =
            u32::from(frame[at]) | u32::from(frame[at + 1]) << 8 | u32::from(frame[at + 2]) << 16;
        // RFC 8878 3.1.1.2: Last_Block, Block_Type, Block_Size; an RLE block
        // carries one byte.
        let size = (header >> 3) as usize;
        let body = if (header >> 1) & 3 == 1 { 1 } else { size };
        at += 3 + body;
        blocks += 1;
        if header & 1 == 1 {
            return blocks;
        }
    }
}

/// A captured parse closes every block the frame holds with a delimiter,
/// even at the optimal levels that split a block of text then noise into
/// several: the parse describes the blocks as written.
#[test]
fn captured_parse_delimits_every_written_block() {
    let mut input = Vec::new();
    let mut state = 0x2545_F491u32;
    for _ in 0..2 {
        let mut i = 0u32;
        let start = input.len();
        while input.len() - start < 64 * 1024 {
            input.extend_from_slice(
                alloc::format!("record {i} status=shipped region=eu\n").as_bytes(),
            );
            i += 1;
        }
        input.truncate(start + 64 * 1024);
        for _ in 0..64 * 1024 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            input.push(state as u8);
        }
    }
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Level(19));
    compressor.set_sequence_capture(true);
    let frame = compressor.compress_independent_frame(&input);
    let parse = compressor.take_captured_sequences();
    let delimiters = parse.iter().filter(|s| s.is_delimiter()).count();
    assert_eq!(delimiters, block_count(&frame));
}

/// At the Fast level the parse is recorded from wire codes, and a block
/// written as RLE or raw without a search is a delimiter alone: through the
/// in-place one-shot path and through the reader path alike.
#[test]
fn fast_level_capture_covers_unsearched_blocks() {
    let mut input = vec![5u8; 2048];
    let mut state = 0x7F4A_7C15u32;
    input.extend((0..4096).map(|_| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as u8
    }));
    input.extend(b"fast level text fast level text ".repeat(64));

    let check = |parse: &[ExternalSequence], frame: &[u8]| {
        let covered: usize = parse
            .iter()
            .map(|s| (s.lit_len + s.match_len) as usize)
            .sum();
        assert_eq!(covered, input.len());
        assert_eq!(
            parse.iter().filter(|s| s.is_delimiter()).count(),
            block_count(frame)
        );
        let mut literals = Vec::new();
        let mut at = 0;
        for s in parse {
            literals.extend_from_slice(&input[at..at + s.lit_len as usize]);
            at += (s.lit_len + s.match_len) as usize;
        }
        assert_eq!(
            rebuild(parse, &literals, &[], 1 << 20, 4096).unwrap(),
            input
        );
    };

    let mut one_shot: FrameCompressor = FrameCompressor::new(CompressionLevel::Fastest);
    one_shot.set_max_block_size(Some(2048));
    one_shot.set_sequence_capture(true);
    let frame = one_shot.compress_independent_frame(&input);
    check(&one_shot.take_captured_sequences(), &frame);

    let mut frame = Vec::new();
    let mut reader: FrameCompressor<&[u8], &mut Vec<u8>> =
        FrameCompressor::new(CompressionLevel::Fastest);
    reader.set_max_block_size(Some(2048));
    reader.set_sequence_capture(true);
    reader.set_source(&input);
    reader.set_drain(&mut frame);
    reader.compress();
    let parse = reader.take_captured_sequences();
    drop(reader);
    check(&parse, &frame);
}

/// A sequence with no match carries no offset either: only a delimiter
/// (offset 0) may end a parse in literals (zstd.h, `ZSTD_Sequence`).
#[test]
fn a_final_literal_run_must_have_no_offset() {
    assert_eq!(
        validate_parse(&[seq(1, 12, 0)], 12, 1 << 20, 0, false, 1 << 17),
        Err(SequenceError::MatchTooShort { index: 0 })
    );
    assert_eq!(
        validate_parse(
            &[seq(3, 3, 6), seq(9, 3, 0)],
            12,
            1 << 20,
            0,
            false,
            1 << 17
        ),
        Err(SequenceError::MatchTooShort { index: 1 })
    );
    assert_eq!(
        validate_parse(
            &[ExternalSequence::delimiter(12)],
            12,
            1 << 20,
            0,
            true,
            1 << 17
        ),
        Ok(())
    );
}

#[test]
fn sequence_errors_read_as_what_went_wrong() {
    let cases = [
        (SequenceError::LengthMismatch, "cover"),
        (SequenceError::MatchTooShort { index: 3 }, "sequence 3"),
        (SequenceError::OffsetTooFar { index: 4 }, "too far"),
        (SequenceError::ZeroOffset { index: 5 }, "offset 0"),
        (SequenceError::BlockTooLarge, "block size"),
    ];
    for (error, text) in cases {
        assert!(alloc::format!("{error}").contains(text), "{error}");
    }
    let boxed: alloc::boxed::Box<dyn std::error::Error> =
        alloc::boxed::Box::new(SequenceError::BlockTooLarge);
    assert!(!alloc::format!("{boxed}").is_empty());
}

/// With block delimiters, each delimited region is a block of its own, the
/// layout the caller laid down (upstream `ZSTD_sf_explicitBlockDelimiters`).
#[test]
fn replay_keeps_delimited_blocks() {
    let (input, _) = periodic_input_and_parse(17_000, 13);
    let regions = [3_000u32, 5_000, 9_000];
    let mut parse = Vec::new();
    for len in regions {
        parse.push(seq(13, 13, len - 14));
        parse.push(ExternalSequence::delimiter(1));
    }
    let frame = replay_frame(&input, &parse, None);
    assert_eq!(decode(&frame), input);
    assert_eq!(block_count(&frame), regions.len());
}
