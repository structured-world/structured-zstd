//! Decoding a frame block by block and frame inspection: what the C
//! buffer-less and block APIs and the frame queries are built on.

use alloc::vec;
use alloc::vec::Vec;

use super::errors::FrameDecoderError;
use super::{
    ContentChecksum, DictionaryHandle, FrameDecoder, FrameMagic, decompression_margin,
    frame_header_size_in_format, frame_magic, min_decoding_buffer_size,
};
use crate::encoding::{
    CompressionContext, CompressionLevel, CompressionParameters, FrameCompressor,
};

/// Text that changes every few hundred bytes, so blocks compress and refer
/// back to each other.
fn text(len: usize) -> Vec<u8> {
    let phrase = b"the quick brown fox jumps over the lazy dog ";
    (0..len)
        .map(|i| phrase[i % phrase.len()] ^ (i / 700) as u8)
        .collect()
}

fn noise(len: usize, mut state: u32) -> Vec<u8> {
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

/// A frame of `input` in 1 KiB blocks, with a checksum.
fn blocky_frame(input: &[u8]) -> Vec<u8> {
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_max_block_size(Some(1024));
    compressor.compress_independent_frame(input)
}

/// A frame of `input` with no content size, a 1 KiB window and 1 KiB blocks:
/// its decode runs in the ring buffer, which wraps.
fn ring_frame(input: &[u8]) -> Vec<u8> {
    let params = CompressionParameters::builder(CompressionLevel::Default)
        .window_log(10)
        .build()
        .unwrap();
    let mut context = CompressionContext::new(CompressionLevel::Default);
    context.set_parameters(&params).unwrap();
    context.set_content_checksum(true).unwrap();
    let mut frame = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let n = context.write(&mut frame, rest).unwrap();
        rest = &rest[n..];
    }
    context.finish_frame(&mut frame).unwrap();
    frame
}

/// Decode `frame` one block at a time, each into the same small target.
fn decode_by_blocks(frame: &[u8]) -> Vec<u8> {
    let mut source = frame;
    let mut decoder = FrameDecoder::new();
    decoder.set_content_checksum(ContentChecksum::Verify);
    decoder.reset(&mut source).unwrap();
    let mut output = Vec::new();
    let mut target = vec![0u8; 1 << 17];
    loop {
        let (header, rest) = source.split_first_chunk::<3>().unwrap();
        let (len, last) = decoder.block_content_len(*header).unwrap();
        let n = decoder
            .decode_block(*header, &rest[..len], &mut target)
            .unwrap();
        output.extend_from_slice(&target[..n]);
        source = &rest[len..];
        if last {
            break;
        }
    }
    if decoder.awaits_checksum() {
        let (checksum, rest) = source.split_first_chunk::<4>().unwrap();
        decoder.read_content_checksum(*checksum).unwrap();
        source = rest;
    }
    assert!(source.is_empty());
    output
}

/// Block by block decodes what a whole-frame decode does, in a flat buffer
/// (the frame declares its size) and in a ring that wraps (it does not).
#[test]
fn block_by_block_decode_matches_the_input() {
    let input = text(10_000);
    let frame = blocky_frame(&input);
    assert_eq!(decode_by_blocks(&frame), input);

    let mut input = text(6_000);
    input.extend(noise(3_000, 7));
    input.extend(text(6_000));
    let frame = ring_frame(&input);
    assert_eq!(decode_by_blocks(&frame), input);
}

/// The block API refuses what it cannot take: no frame begun, a target too
/// small, content of the wrong length, a checksum that disagrees.
#[test]
fn block_decode_refusals() {
    let input = text(4_000);
    let frame = blocky_frame(&input);
    let header = [frame[6], frame[7], frame[8]];

    let mut fresh = FrameDecoder::new();
    assert!(matches!(
        fresh.decode_block(header, &[], &mut [0u8; 8]),
        Err(FrameDecoderError::NotYetInitialized)
    ));
    assert!(matches!(
        fresh.block_content_len(header),
        Err(FrameDecoderError::NotYetInitialized)
    ));
    assert!(matches!(
        fresh.read_content_checksum([0; 4]),
        Err(FrameDecoderError::NotYetInitialized)
    ));
    assert!(matches!(
        fresh.insert_block(b"x"),
        Err(FrameDecoderError::NotYetInitialized)
    ));

    let begin = |decoder: &mut FrameDecoder| {
        let mut source = frame.as_slice();
        decoder.reset(&mut source).unwrap();
        source
    };
    let mut decoder = FrameDecoder::new();
    let rest = begin(&mut decoder);
    let (header, rest) = rest.split_first_chunk::<3>().unwrap();
    let (len, _) = decoder.block_content_len(*header).unwrap();
    assert!(matches!(
        decoder.decode_block(*header, &rest[..len], &mut [0u8; 8]),
        Err(FrameDecoderError::TargetTooSmall)
    ));

    let mut decoder = FrameDecoder::new();
    let rest = begin(&mut decoder);
    let (header, rest) = rest.split_first_chunk::<3>().unwrap();
    let (len, _) = decoder.block_content_len(*header).unwrap();
    assert!(
        decoder
            .decode_block(*header, &rest[..len - 1], &mut [0u8; 2048])
            .is_err()
    );

    // A wrong checksum is caught when verifying.
    let mut corrupt = frame.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    let mut source = corrupt.as_slice();
    let mut decoder = FrameDecoder::new();
    decoder.set_content_checksum(ContentChecksum::Verify);
    decoder.reset(&mut source).unwrap();
    let mut target = vec![0u8; 2048];
    loop {
        let (header, rest) = source.split_first_chunk::<3>().unwrap();
        let (len, last) = decoder.block_content_len(*header).unwrap();
        decoder
            .decode_block(*header, &rest[..len], &mut target)
            .unwrap();
        source = &rest[len..];
        if last {
            break;
        }
    }
    assert!(decoder.awaits_checksum());
    let (checksum, _) = source.split_first_chunk::<4>().unwrap();
    assert!(matches!(
        decoder.read_content_checksum(*checksum),
        Err(FrameDecoderError::ChecksumMismatch { .. })
    ));
}

/// A block refused before it is decoded (content of the wrong length) leaves
/// the frame as it was, so the right content still decodes, as upstream's
/// size check precedes any state change; a block whose content fails to
/// decode ends the frame, and the next block is refused rather than decoded
/// against what the failure left behind.
#[test]
fn a_block_that_fails_to_decode_ends_the_frame() {
    let input = text(4_000);
    let frame = blocky_frame(&input);
    let begin = |decoder: &mut FrameDecoder| {
        let mut source = frame.as_slice();
        decoder.reset(&mut source).unwrap();
        source
    };
    let mut decoder = FrameDecoder::new();
    let rest = begin(&mut decoder);
    let (header, body) = rest.split_first_chunk::<3>().unwrap();
    let (len, _) = decoder.block_content_len(*header).unwrap();
    let mut target = vec![0u8; 2048];
    assert!(
        decoder
            .decode_block(*header, &body[..len - 1], &mut target)
            .is_err()
    );
    assert!(
        decoder
            .decode_block(*header, &body[..len], &mut target)
            .is_ok()
    );

    let mut decoder = FrameDecoder::new();
    let rest = begin(&mut decoder);
    let (header, body) = rest.split_first_chunk::<3>().unwrap();
    let (len, _) = decoder.block_content_len(*header).unwrap();
    let corrupt = vec![0xFFu8; len];
    assert!(
        decoder
            .decode_block(*header, &corrupt, &mut target)
            .is_err()
    );
    assert!(matches!(
        decoder.decode_block(*header, &body[..len], &mut target),
        Err(FrameDecoderError::NotYetInitialized)
    ));
}

/// Encode the RFC 8878 3.1.1.2 header of a compressed block of `len` bytes.
fn compressed_header(len: usize) -> [u8; 3] {
    let raw = ((len as u32) << 3 | 2 << 1).to_le_bytes();
    [raw[0], raw[1], raw[2]]
}

/// Blocks outside a frame: ones stored as they are go into the history, and
/// a later block that only compresses against both of them decodes. With a
/// dictionary, and with frames that carry no magic number.
#[test]
fn a_run_of_blocks_decodes_against_its_history() {
    for (dictionary, magicless) in [(false, false), (true, false), (false, true)] {
        let dict_bytes = text(3_000);
        let mut context = CompressionContext::new(CompressionLevel::Default);
        if dictionary {
            context.set_dictionary_from_bytes(&dict_bytes).unwrap();
        }
        context.start_frame(&mut Vec::new()).unwrap();
        // Noise neither compresses on its own nor matches the dictionary, so
        // the third block shrinks only by matching the two before it.
        let first = noise(2_000, 3);
        let stored = noise(1_500, 11);
        let mut third = stored.clone();
        third.extend_from_slice(&first);

        let mut decoder = FrameDecoder::new();
        decoder.set_magicless(magicless);
        let handle = DictionaryHandle::from_serialized_or_raw_content(&dict_bytes).unwrap();
        decoder.begin_blocks(dictionary.then_some(&handle)).unwrap();
        let mut target = vec![0u8; 4096];
        for (index, block) in [&first, &stored, &third].into_iter().enumerate() {
            let mut payload = Vec::new();
            let compressed = context.compress_block(block, &mut payload).unwrap();
            assert_eq!(compressed, index == 2, "block {index}");
            if compressed {
                let n = decoder
                    .decode_block(compressed_header(payload.len()), &payload, &mut target)
                    .unwrap();
                assert_eq!(&target[..n], &block[..]);
            } else {
                decoder.insert_block(block).unwrap();
            }
        }
    }
}

/// A stored block far larger than the window goes into the history a block
/// at a time, so the history never holds more than the window and a block,
/// whatever the caller hands over; the window's worth it keeps is the end of
/// the block.
#[test]
fn inserting_a_huge_block_keeps_only_the_window() {
    let mut decoder = FrameDecoder::new();
    decoder.set_max_window_size(1 << 10).unwrap();
    decoder.begin_blocks(None).unwrap();
    let huge = noise(1 << 20, 5);
    let tail_start = huge.len() - 500;
    decoder.insert_block(&huge).unwrap();
    assert!(
        decoder.workspace_size() < 512 << 10,
        "{} bytes held after a 1 MiB insert into a 1 KiB window",
        decoder.workspace_size()
    );
    // The last window of the block is the history the next block reads: an
    // encoder that stored only that tail compresses a block repeating it,
    // and the block decodes against the decoder's history.
    let params = CompressionParameters::builder(CompressionLevel::Default)
        .window_log(10)
        .build()
        .unwrap();
    let mut context = CompressionContext::new(CompressionLevel::Default);
    context.set_parameters(&params).unwrap();
    context.start_frame(&mut Vec::new()).unwrap();
    let tail = &huge[tail_start..];
    assert!(!context.compress_block(tail, &mut Vec::new()).unwrap());
    let mut repeat = tail.to_vec();
    repeat.extend_from_slice(tail);
    let mut payload = Vec::new();
    assert!(context.compress_block(&repeat, &mut payload).unwrap());
    let mut target = vec![0u8; 4096];
    let n = decoder
        .decode_block(compressed_header(payload.len()), &payload, &mut target)
        .unwrap();
    assert_eq!(&target[..n], &repeat[..]);
}

#[test]
fn frame_magic_names_each_kind() {
    assert_eq!(
        frame_magic(&[0x28, 0xB5, 0x2F, 0xFD, 0]),
        Some(FrameMagic::Zstd)
    );
    assert_eq!(
        frame_magic(&[0x5F, 0x2A, 0x4D, 0x18]),
        Some(FrameMagic::Skippable(15))
    );
    assert_eq!(
        frame_magic(&[0x50, 0x2A, 0x4D, 0x18]),
        Some(FrameMagic::Skippable(0))
    );
    assert_eq!(frame_magic(&[0x60, 0x2A, 0x4D, 0x18]), None);
    assert_eq!(frame_magic(&[0x28, 0xB5, 0x2F]), None);
}

/// The margin counts every frame's header, block headers and checksum, a
/// skippable frame whole, plus one largest block; a broken input is refused.
#[test]
fn decompression_margin_over_frames() {
    let first = blocky_frame(&text(5_000));
    let mut skippable = Vec::new();
    crate::skippable::write_skippable_frame(3, b"meta", &mut skippable).unwrap();
    let mut both = first.clone();
    both.extend_from_slice(&skippable);
    both.extend_from_slice(&first);
    let one = decompression_margin(&first).unwrap();
    let all = decompression_margin(&both).unwrap();
    // A single-segment frame's window is its content, so its largest block
    // is the 5000 bytes; the per-frame part is the rest.
    let largest_block = 5_000;
    let per_frame = one - largest_block;
    assert!(per_frame > 0 && per_frame < first.len());
    // The largest block is counted once, not per frame.
    assert_eq!(all, 2 * per_frame + skippable.len() + largest_block);

    assert!(decompression_margin(&first[..first.len() - 2]).is_err());
    assert!(decompression_margin(b"not a frame at all").is_err());
}

#[test]
fn min_decoding_buffer_size_cases() {
    // A small window: both block terms are the window.
    assert_eq!(
        min_decoding_buffer_size(1024, None),
        Some(1024 + 2 * 1024 + 64)
    );
    assert_eq!(
        min_decoding_buffer_size(1 << 20, None),
        Some((1 << 20) + 2 * 131_072 + 64)
    );
    assert_eq!(min_decoding_buffer_size(1 << 20, Some(500)), Some(500));
    // A window past `u64` still fits a declared content size.
    assert_eq!(min_decoding_buffer_size(u64::MAX, Some(10)), Some(10));
    assert_eq!(min_decoding_buffer_size(u64::MAX, None), None);
}

#[test]
fn frame_header_size_in_either_format() {
    let frame = blocky_frame(&text(100));
    let with_magic = frame_header_size_in_format(&frame, false).unwrap();
    assert_eq!(
        frame_header_size_in_format(&frame[4..], true).unwrap(),
        with_magic - 4
    );
    assert!(frame_header_size_in_format(&[], true).is_err());
    assert!(frame_header_size_in_format(&frame[..3], false).is_err());
}
