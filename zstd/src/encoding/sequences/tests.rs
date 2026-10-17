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
    let mut rebuilt = Vec::new();
    let literals: Vec<u8> = {
        let mut lits = Vec::new();
        let mut at = 0usize;
        for s in &parse {
            lits.extend_from_slice(&input[at..at + s.lit_len as usize]);
            at += (s.lit_len + s.match_len) as usize;
        }
        lits
    };
    rebuild_source(&parse, &literals, &[], &mut rebuilt).unwrap();
    assert_eq!(rebuilt, input);
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
/// repeats an overlapping match, and keeps unused literals as the tail.
#[test]
fn rebuild_reads_dictionary_overlaps_and_tail() {
    let dict = b"0123456789";
    let parse = [seq(5, 2, 6), seq(1, 1, 4)];
    let mut out = b"prefix:".to_vec();
    rebuild_source(&parse, b"abx!", dict, &mut out).unwrap();
    assert_eq!(out, b"prefix:ab789ab7xxxxx!");

    let mut out = Vec::new();
    assert_eq!(
        rebuild_source(&[seq(20, 2, 4)], b"ab", dict, &mut out),
        Err(SequenceError::OffsetTooFar { index: 0 })
    );
    assert_eq!(
        rebuild_source(&[seq(0, 2, 4)], b"ab", dict, &mut out),
        Err(SequenceError::ZeroOffset { index: 0 })
    );
    assert_eq!(
        rebuild_source(&[seq(1, 5, 4)], b"ab", dict, &mut out),
        Err(SequenceError::LengthMismatch)
    );
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
