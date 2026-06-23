use alloc::{string::ToString, vec};

use super::{
    BlockTypeError, DecodeBlockContentError, DecodeBufferError, DecodeSequenceError,
    DecompressBlockError, DecompressLiteralsError, ExecuteSequencesError, FSETableError,
    FrameDecoderError, HuffmanTableError,
};

#[test]
fn execute_sequences_output_overflow_requested_covers_all_arms() {
    // #246: `run_direct_decode` folds a Compressed-block overshoot into
    // `FrameContentSizeMismatch` by reading `requested` from whichever
    // overflow shape the executor produced. Cover all three arms:
    //   1. inline-sequence path -> `OutputBufferOverflow` directly,
    //   2. match-repeat path -> `DecodebufferError(OutputBufferOverflow)`,
    //   3. any other variant -> None (no fold).
    let inline = ExecuteSequencesError::OutputBufferOverflow {
        tail: 10,
        requested: 7,
        capacity: 12,
    };
    assert_eq!(inline.output_overflow_requested(), Some(7));

    let repeat =
        ExecuteSequencesError::DecodebufferError(DecodeBufferError::OutputBufferOverflow {
            tail: 3,
            requested: 99,
            capacity: 4,
        });
    assert_eq!(repeat.output_overflow_requested(), Some(99));

    // Non-overflow variants (and non-overflow DecodeBufferError) -> None.
    assert_eq!(
        ExecuteSequencesError::ZeroOffset.output_overflow_requested(),
        None
    );
    assert_eq!(
        ExecuteSequencesError::DecodebufferError(DecodeBufferError::ZeroOffset)
            .output_overflow_requested(),
        None
    );
}

#[test]
fn block_and_sequence_display_messages_are_specific() {
    assert_eq!(
        BlockTypeError::InvalidBlocktypeNumber { num: 7 }.to_string(),
        "Invalid Blocktype number. Is: 7. Should be one of: 0, 1, 2, 3 (3 is reserved)."
    );
    assert_eq!(
        DecompressBlockError::MalformedSectionHeader {
            expected_len: 12,
            remaining_bytes: 3,
        }
        .to_string(),
        "Malformed section header. Says literals would be this long: 12 but there are only 3 bytes left"
    );
    assert_eq!(
        DecodeBlockContentError::ExpectedHeaderOfPreviousBlock.to_string(),
        "Can't decode next block body, while expecting to decode the header of the previous block. Results will be nonsense"
    );
    assert_eq!(
        DecodeSequenceError::ExtraPadding { skipped_bits: 11 }.to_string(),
        "Padding at the end of the sequence_section was more than a byte long: 11 bits. Probably caused by data corruption"
    );
}

#[test]
fn frame_decoder_display_messages_are_specific() {
    assert_eq!(
        FrameDecoderError::TargetTooSmall.to_string(),
        "Target must have at least as many bytes as the content size reported by the frame"
    );
    assert_eq!(
        FrameDecoderError::DictNotProvided { dict_id: 0xABCD }.to_string(),
        "Frame header specified dictionary id 0xABCD that wasn't provided via add_dict()/add_dict_from_bytes() (or add_dict_handle() on atomic targets) or reset_with_dict_handle()/decode_all_with_dict_handle()/decode_all_with_dict_bytes()"
    );
    assert_eq!(
        FrameDecoderError::DictIdMismatch {
            expected: 0xABCD,
            provided: 0x1234
        }
        .to_string(),
        "Frame header dictionary id 0xABCD does not match provided dictionary id 0x1234"
    );
    assert_eq!(
        FrameDecoderError::DictAlreadyRegistered { dict_id: 0xABCD }.to_string(),
        "Dictionary id 0xABCD already registered in decoder"
    );
    assert_eq!(
        FrameDecoderError::FrameContentSizeMismatch {
            declared: 100,
            produced: 87,
        }
        .to_string(),
        "Frame content size mismatch (corrupt frame): declared 100 bytes, blocks summed to 87 bytes"
    );
    // Locks the wasm-exposed checksum-mismatch contract (exact string).
    assert_eq!(
        FrameDecoderError::ChecksumMismatch {
            expected: 0xDEAD_BEEF,
            calculated: 0x0BAD_F00D,
        }
        .to_string(),
        "Content checksum mismatch (corrupt frame): frame stored 0xDEADBEEF, decoder calculated 0x0BADF00D"
    );
}

#[test]
fn decode_block_content_backend_overflow_display_names_the_step() {
    use crate::blocks::block::BlockType;
    assert_eq!(
        DecodeBlockContentError::BackendOverflow {
            step: BlockType::RLE
        }
        .to_string(),
        "RLE block's decompressed payload exceeds the caller-provided output buffer"
    );
    assert_eq!(
        DecodeBlockContentError::BackendOverflow {
            step: BlockType::Raw
        }
        .to_string(),
        "Raw block's decompressed payload exceeds the caller-provided output buffer"
    );
}

#[test]
fn literal_display_messages_are_specific() {
    assert_eq!(
        DecompressLiteralsError::MissingCompressedSize.to_string(),
        "compressed size was none even though it must be set to something for compressed literals"
    );
    assert_eq!(
        DecompressLiteralsError::MissingNumStreams.to_string(),
        "num_streams was none even though it must be set to something (1 or 4) for compressed literals"
    );
    assert_eq!(
        DecompressLiteralsError::ExtraPadding { skipped_bits: 9 }.to_string(),
        "Padding at the end of the sequence_section was more than a byte long: 9 bits. Probably caused by data corruption"
    );
}

#[test]
fn fse_and_huffman_display_messages_are_specific() {
    assert_eq!(
        FSETableError::ProbabilityCounterMismatch {
            got: 4,
            expected_sum: 3,
            symbol_probabilities: vec![1, -1],
        }
        .to_string(),
        "FSE probability sum mismatch: got 4, expected 3. Indicates corrupted data or an invalid distribution\n [1, -1]"
    );
    assert_eq!(
        HuffmanTableError::NotEnoughBytesForWeights {
            got_bytes: 2,
            expected_bytes: 5,
        }
        .to_string(),
        "Header says there should be 5 bytes for the weights but there are only 2 bytes in the stream"
    );
    assert_eq!(
        HuffmanTableError::ExtraPadding { skipped_bits: 13 }.to_string(),
        "Padding at the end of the sequence_section was more than a byte long: 13 bits. Probably caused by data corruption"
    );
    assert_eq!(
        HuffmanTableError::FSETableUsedTooManyBytes {
            used: 7,
            available_bytes: 6,
        }
        .to_string(),
        "FSE table used more bytes: 7 than were meant to be used for the whole stream of huffman weights (6)"
    );
}
