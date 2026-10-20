use super::*;
use crate::decoding::FrameDecoder;
use crate::decoding::read_frame_header_info;
use crate::encoding::{CompressionLevel, compress_slice_to_vec};
use alloc::vec;
use alloc::vec::Vec;

const UNLIMITED: u64 = u64::MAX;

/// The per-block charge must cover the largest tables a block can make the
/// decoder build: a Huffman table at the longest code the decoder accepts and
/// the three sequence tables at their accuracy-log limits.
#[test]
fn compressed_block_charge_covers_the_largest_table_builds() {
    use crate::decoding::sequence_section_decoder::{LL_MAX_LOG, ML_MAX_LOG, OF_MAX_LOG};
    let largest_builds = (1u64 << crate::huff0::MAX_MAX_NUM_BITS)
        + (1u64 << LL_MAX_LOG)
        + (1u64 << ML_MAX_LOG)
        + (1u64 << OF_MAX_LOG);
    assert!(
        COMPRESSED_BLOCK_UNITS >= largest_builds,
        "charge {COMPRESSED_BLOCK_UNITS} < {largest_builds} table entries"
    );
}

fn text(len: usize) -> Vec<u8> {
    let words = [
        "alpha ",
        "beta ",
        "gamma ",
        "delta ",
        "manifest ",
        "blob ",
        "chunk ",
    ];
    let mut out = Vec::with_capacity(len);
    let mut i = 0usize;
    while out.len() < len {
        out.extend_from_slice(words[(i * 7 + i / 13) % words.len()].as_bytes());
        out.extend_from_slice(&(i as u32).to_le_bytes()[..(i % 3)]);
        i += 1;
    }
    out.truncate(len);
    out
}

fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Feed `compressed` `in_chunk` bytes at a time with `out_chunk` bytes of
/// output room per step, asserting every step makes progress.
fn run(
    decoder: &mut WorkspaceDecoder<'_>,
    compressed: &[u8],
    in_chunk: usize,
    out_chunk: usize,
    work: &mut WorkBudget,
) -> Result<Vec<u8>, WorkspaceDecodeError> {
    let mut decoded = Vec::new();
    let mut out = vec![0u8; out_chunk];
    let mut pos = 0;
    loop {
        let end = (pos + in_chunk).min(compressed.len());
        let last = end == compressed.len();
        let progress = decoder.step(&compressed[pos..end], last, &mut out, work)?;
        decoded.extend_from_slice(&out[..progress.produced]);
        match progress.state {
            StepState::Done => return Ok(decoded),
            StepState::NeedInput => assert_eq!(progress.consumed, end - pos),
            StepState::NeedOutput => assert_eq!(progress.produced, out_chunk),
        }
        assert!(
            progress.consumed > 0 || progress.produced > 0,
            "a step that is not done must make progress"
        );
        pos += progress.consumed;
    }
}

fn decode_chunked(
    compressed: &[u8],
    limits: DecodeLimits,
    in_chunk: usize,
    out_chunk: usize,
    work: &mut WorkBudget,
) -> Result<Vec<u8>, WorkspaceDecodeError> {
    let mut workspace = vec![0u8; workspace_bytes(limits.max_window_size).unwrap()];
    let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
    run(&mut decoder, compressed, in_chunk, out_chunk, work)
}

fn decode_whole(compressed: &[u8], limits: DecodeLimits) -> Result<Vec<u8>, WorkspaceDecodeError> {
    decode_chunked(
        compressed,
        limits,
        compressed.len().max(1),
        1 << 20,
        &mut WorkBudget::new(UNLIMITED),
    )
}

fn reference_decode(compressed: &[u8], capacity: usize) -> Option<Vec<u8>> {
    let mut decoder = FrameDecoder::new();
    decoder.set_content_checksum(crate::decoding::ContentChecksum::Verify);
    let mut target = vec![0u8; capacity];
    let n = decoder.decode_all(compressed, &mut target).ok()?;
    target.truncate(n);
    Some(target)
}

/// A frame without content size or dictionary, with window `1 << (10 + exp)`
/// and the given `(block_type, payload, regenerated_size)` blocks.
fn handmade_frame(exp: u8, checksum: Option<u32>, blocks: &[(u32, &[u8], u32)]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&MAGIC_NUM.to_le_bytes());
    frame.push(if checksum.is_some() { 1 << 2 } else { 0 });
    frame.push(exp << 3);
    for (i, &(block_type, payload, size)) in blocks.iter().enumerate() {
        let last = u32::from(i + 1 == blocks.len());
        let header = last | (block_type << 1) | (size << 3);
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(payload);
    }
    if let Some(sum) = checksum {
        frame.extend_from_slice(&sum.to_le_bytes());
    }
    frame
}

fn skippable_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Every chunking of input and output reproduces the data and agrees with the
/// one-shot decoder, on compressible, incompressible and run-length input.
#[test]
fn chunked_decode_matches_frame_decoder() {
    let mut mixed = text(150_000);
    mixed.extend(core::iter::repeat_n(0xA5, 140_000));
    mixed.extend(noise(150_000));
    let fixtures = [text(200_000), noise(150_000), vec![0u8; 300_000], mixed];
    let levels = [
        CompressionLevel::Uncompressed,
        CompressionLevel::Fastest,
        CompressionLevel::Default,
        CompressionLevel::Best,
    ];
    let limits = DecodeLimits::new(1 << 20);
    for data in &fixtures {
        for level in levels {
            let compressed = compress_slice_to_vec(data, level);
            let reference = reference_decode(&compressed, data.len()).unwrap();
            assert_eq!(&reference, data);
            for (in_chunk, out_chunk) in [(7, 13), (100, 4096), (compressed.len(), data.len())] {
                let decoded = decode_chunked(
                    &compressed,
                    limits,
                    in_chunk,
                    out_chunk,
                    &mut WorkBudget::new(UNLIMITED),
                )
                .unwrap();
                assert!(
                    decoded == *data,
                    "level {level:?} chunks {in_chunk}/{out_chunk}"
                );
            }
        }
    }
}

/// Compress with the window capped at `1 << window_log`, so content larger
/// than that becomes a multi-segment frame decoded through the ring.
fn compress_windowed(data: &[u8], level: i32, window_log: u32) -> Vec<u8> {
    use crate::encoding::{CompressionParameters, compress_with_parameters};
    let parameters = CompressionParameters::builder(CompressionLevel::Level(level))
        .window_log(window_log)
        .build()
        .unwrap();
    compress_with_parameters(data, &parameters)
}

/// Multi-segment frames, whose ring wraps many times over a small window, and
/// single-segment frames, which decode linearly, both reproduce the data at
/// every chunking.
#[test]
fn ring_and_linear_windows_decode() {
    let mut data = text(120_000);
    data.extend(noise(30_000));
    data.extend(text(50_000));
    for (window_log, level) in [(10, 3), (12, 7), (17, 19)] {
        let compressed = compress_windowed(&data, level, window_log);
        let info = read_frame_header_info(&compressed, false).unwrap();
        assert!(info.window_size < data.len() as u64, "multi-segment frame");
        for (in_chunk, out_chunk) in [(1, 7), (999, 1_000), (compressed.len(), 1 << 16)] {
            let decoded = decode_chunked(
                &compressed,
                DecodeLimits::new(info.window_size.max(1024)),
                in_chunk,
                out_chunk,
                &mut WorkBudget::new(UNLIMITED),
            )
            .unwrap();
            assert!(
                decoded == data,
                "window_log {window_log} chunks {in_chunk}/{out_chunk}"
            );
        }
    }
    let single = compress_slice_to_vec(&data[..60_000], CompressionLevel::Default);
    let info = read_frame_header_info(&single, false).unwrap();
    assert_eq!(info.window_size, 60_000, "single-segment frame");
    let decoded = decode_chunked(
        &single,
        DecodeLimits::new(60_000),
        333,
        77,
        &mut WorkBudget::new(UNLIMITED),
    )
    .unwrap();
    assert_eq!(decoded, data[..60_000]);
}

/// Byte-at-a-time input and output, the most a header or block can be split.
#[test]
fn single_byte_steps_decode() {
    let data = text(20_000);
    let compressed = compress_slice_to_vec(&data, CompressionLevel::Default);
    let decoded = decode_chunked(
        &compressed,
        DecodeLimits::new(1 << 16),
        1,
        1,
        &mut WorkBudget::new(UNLIMITED),
    )
    .unwrap();
    assert_eq!(decoded, data);
}

/// Concatenated frames decode in order, skippable frames add no output, and a
/// skippable frame split across steps is still skipped whole.
#[test]
fn concatenated_and_skippable_frames() {
    let a = text(10_000);
    let b = noise(5_000);
    let mut stream = skippable_frame(b"leading metadata");
    stream.extend(compress_slice_to_vec(&a, CompressionLevel::Fastest));
    stream.extend(skippable_frame(&[]));
    stream.extend(skippable_frame(&noise(300)));
    stream.extend(compress_slice_to_vec(&b, CompressionLevel::Default));
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    for chunk in [1, 5, stream.len()] {
        let decoded = decode_chunked(
            &stream,
            DecodeLimits::new(1 << 16),
            chunk,
            977,
            &mut WorkBudget::new(UNLIMITED),
        )
        .unwrap();
        assert_eq!(decoded, expected);
    }
}

/// No input at all is an empty stream; an empty frame decodes to nothing.
#[test]
fn empty_stream_and_empty_frame() {
    let limits = DecodeLimits::new(1 << 16);
    assert_eq!(decode_whole(&[], limits).unwrap(), Vec::<u8>::new());
    let empty = compress_slice_to_vec(&[], CompressionLevel::Default);
    assert_eq!(decode_whole(&empty, limits).unwrap(), Vec::<u8>::new());
}

/// Input that stops inside a frame is `Truncated` when final and a request
/// for more input when not, at every cut point.
#[test]
fn every_cut_is_truncation_when_final() {
    let data = text(3_000);
    let compressed = compress_slice_to_vec(&data, CompressionLevel::Default);
    let limits = DecodeLimits::new(1 << 16);
    let mut workspace = vec![0u8; workspace_bytes(limits.max_window_size).unwrap()];
    let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
    let mut out = vec![0u8; data.len()];
    for cut in 1..compressed.len() {
        decoder.reset();
        let mut work = WorkBudget::new(UNLIMITED);
        let progress = decoder
            .step(&compressed[..cut], false, &mut out, &mut work)
            .unwrap();
        assert_eq!(progress.state, StepState::NeedInput, "cut {cut}");
        assert_eq!(progress.consumed, cut);
        assert!(matches!(
            decoder.step(&[], true, &mut out, &mut work),
            Err(WorkspaceDecodeError::Truncated)
        ));
    }
}

/// The exact workspace size works at every address; one byte less is refused
/// before anything is decoded, as is a window limit out of range.
#[test]
fn workspace_size_boundaries() {
    let limit = 64 * 1024;
    let needed = workspace_bytes(limit).unwrap();
    let data = text(50_000);
    let compressed = compress_slice_to_vec(&data, CompressionLevel::Default);
    let mut backing = vec![0u8; needed + TABLE_ALIGN];
    for offset in 0..TABLE_ALIGN {
        let workspace = &mut backing[offset..offset + needed];
        let mut decoder = WorkspaceDecoder::new(workspace, DecodeLimits::new(limit)).unwrap();
        let decoded = run(
            &mut decoder,
            &compressed,
            333,
            4096,
            &mut WorkBudget::new(UNLIMITED),
        )
        .unwrap();
        assert_eq!(decoded, data, "offset {offset}");
    }
    assert!(matches!(
        WorkspaceDecoder::new(&mut backing[..needed - 1], DecodeLimits::new(limit)),
        Err(WorkspaceConfigError::WorkspaceTooSmall { needed: n, provided })
            if n == needed && provided == needed - 1
    ));
    for bad in [
        0,
        MIN_WINDOW_SIZE - 1,
        MAX_DECODER_WINDOW_SIZE + 1,
        u64::MAX,
    ] {
        assert_eq!(
            workspace_bytes(bad),
            Err(WorkspaceConfigError::WindowLimitOutOfRange { requested: bad })
        );
    }
}

/// A frame's window is admitted up to the limit and refused one byte past it.
#[test]
fn window_limit_at_and_over() {
    let payload = text(1_000);
    let frame = handmade_frame(6, None, &[(0, &payload, 1_000)]);
    let window = 1u64 << 16;
    assert_eq!(
        decode_whole(&frame, DecodeLimits::new(window)).unwrap(),
        payload
    );
    assert!(matches!(
        decode_whole(&frame, DecodeLimits::new(window - 1)),
        Err(WorkspaceDecodeError::Limit(LimitExceeded::Window { requested, limit }))
            if requested == window && limit == window - 1
    ));
}

/// Output is admitted up to the limit and refused past it, for blocks that
/// state their size (before they run) and for a frame that declares one (at
/// its header).
#[test]
fn output_limit_at_and_over() {
    let payload = text(2_000);
    let frame = handmade_frame(6, None, &[(0, &payload, 2_000), (1, &[7], 1_000)]);
    let mut expected = payload.clone();
    expected.extend(core::iter::repeat_n(7, 1_000));
    let limits = DecodeLimits::new(1 << 16);
    assert_eq!(
        decode_whole(&frame, limits.with_max_output(3_000)).unwrap(),
        expected
    );
    assert!(matches!(
        decode_whole(&frame, limits.with_max_output(2_999)),
        Err(WorkspaceDecodeError::Limit(LimitExceeded::Output {
            limit: 2_999
        }))
    ));

    let data = text(40_000);
    let compressed = compress_slice_to_vec(&data, CompressionLevel::Default);
    assert_eq!(
        decode_whole(&compressed, limits.with_max_output(40_000)).unwrap(),
        data
    );
    assert!(matches!(
        decode_whole(&compressed, limits.with_max_output(39_999)),
        Err(WorkspaceDecodeError::Limit(LimitExceeded::Output { .. }))
    ));
}

/// The exact work a stream costs succeeds and one unit less is refused; and
/// at any budget, no byte is produced that was not paid for first.
#[test]
fn work_budget_at_and_over() {
    let mut data = text(200_000);
    data.extend(noise(60_000));
    let compressed = compress_slice_to_vec(&data, CompressionLevel::Default);
    let limits = DecodeLimits::new(1 << 20);
    let mut work = WorkBudget::new(UNLIMITED);
    decode_chunked(&compressed, limits, 4096, 4096, &mut work).unwrap();
    let cost = UNLIMITED - work.remaining();
    assert!(cost >= compressed.len() as u64 + data.len() as u64);

    let mut exact = WorkBudget::new(cost);
    assert_eq!(
        decode_chunked(&compressed, limits, 4096, 4096, &mut exact).unwrap(),
        data
    );
    assert_eq!(exact.remaining(), 0);
    assert!(matches!(
        decode_chunked(
            &compressed,
            limits,
            4096,
            4096,
            &mut WorkBudget::new(cost - 1)
        ),
        Err(WorkspaceDecodeError::Limit(LimitExceeded::Work { .. }))
    ));

    let mut workspace = vec![0u8; workspace_bytes(limits.max_window_size).unwrap()];
    let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
    for budget in (0..cost).step_by(9_973) {
        decoder.reset();
        let result = run(
            &mut decoder,
            &compressed,
            4096,
            4096,
            &mut WorkBudget::new(budget),
        );
        assert!(matches!(
            result,
            Err(WorkspaceDecodeError::Limit(LimitExceeded::Work { .. }))
        ));
        assert!(decoder.total_output() <= budget, "budget {budget}");
    }
}

/// A block larger than its frame's block maximum is malformed, at the edge.
#[test]
fn block_size_at_and_over_the_maximum() {
    let fits = noise(1_024);
    let frame = handmade_frame(0, None, &[(0, &fits, 1_024)]);
    let limits = DecodeLimits::new(1 << 16);
    assert_eq!(decode_whole(&frame, limits).unwrap(), fits);
    let over = noise(1_025);
    let frame = handmade_frame(0, None, &[(0, &over, 1_025)]);
    assert!(matches!(
        decode_whole(&frame, limits),
        Err(WorkspaceDecodeError::Malformed(
            MalformedInput::BlockTooLarge {
                size: 1_025,
                maximum: 1_024
            }
        ))
    ));
}

/// The content checksum is verified, and a wrong one is reported as such.
#[cfg(feature = "hash")]
#[test]
fn checksum_verified() {
    use core::hash::Hasher;
    let payload = text(5_000);
    let mut hasher = twox_hash::XxHash64::with_seed(0);
    hasher.write(&payload);
    let sum = hasher.finish() as u32;
    let limits = DecodeLimits::new(1 << 16);
    let good = handmade_frame(6, Some(sum), &[(0, &payload, 5_000)]);
    for chunk in [1, 3, good.len()] {
        let decoded = decode_chunked(&good, limits, chunk, 64, &mut WorkBudget::new(UNLIMITED));
        assert_eq!(decoded.unwrap(), payload);
    }
    let bad = handmade_frame(6, Some(sum ^ 1), &[(0, &payload, 5_000)]);
    assert!(matches!(
        decode_whole(&bad, limits),
        Err(WorkspaceDecodeError::Malformed(MalformedInput::Checksum { expected, calculated }))
            if expected == sum ^ 1 && calculated == sum
    ));
}

/// A frame naming a dictionary is refused as unsupported, not as malformed.
#[test]
fn dictionary_frame_is_unsupported() {
    let frame = include_bytes!("../../../dict_tests/files/ModemManager.service.zst");
    let id = read_frame_header_info(frame, false)
        .unwrap()
        .dictionary_id
        .unwrap();
    assert!(matches!(
        decode_whole(frame, DecodeLimits::new(1 << 20)),
        Err(WorkspaceDecodeError::Unsupported(Unsupported::Dictionary { id: got })) if got == id
    ));
}

/// A v0.5-v0.7 frame, which the owned decoders read, is refused as
/// unsupported rather than as malformed: decoding it would allocate.
#[cfg(feature = "legacy")]
#[test]
fn v05_to_v07_frames_are_unsupported() {
    // v0.7 with a one-byte dictionary ID of 0 and no blocks before its end.
    let frame = [
        0x27, 0xB5, 0x2F, 0xFD, 0x51, 0x00, 0x00, 0xFA, 0x00, 0xFF, 0x00, 0x00,
    ];
    let mut out = [0u8; 16];
    assert_eq!(
        crate::decoding::FrameDecoder::new()
            .decode_all(&frame, &mut out)
            .unwrap(),
        0
    );
    assert!(matches!(
        decode_whole(&frame, DecodeLimits::new(1 << 20)),
        Err(WorkspaceDecodeError::Unsupported(
            Unsupported::LegacyFrame {
                version: crate::decoding::LegacyVersion::V07
            }
        ))
    ));
    let message = alloc::string::ToString::to_string(
        &decode_whole(&frame, DecodeLimits::new(1 << 20)).unwrap_err(),
    );
    assert!(message.contains("v0.7"), "{message}");
}

/// Bytes after the last frame that are not a frame are malformed; the error
/// holds until a reset, after which the decoder works again.
#[test]
fn failure_is_terminal_until_reset() {
    let data = text(4_000);
    let mut stream = compress_slice_to_vec(&data, CompressionLevel::Default);
    stream.extend_from_slice(b"junk");
    let limits = DecodeLimits::new(1 << 16);
    let mut workspace = vec![0u8; workspace_bytes(limits.max_window_size).unwrap()];
    let mut decoder = WorkspaceDecoder::new(&mut workspace, limits).unwrap();
    let mut work = WorkBudget::new(UNLIMITED);
    assert!(matches!(
        run(&mut decoder, &stream, 64, 64, &mut work),
        Err(WorkspaceDecodeError::Malformed(
            MalformedInput::FrameHeader(ReadFrameHeaderError::BadMagicNumber(_))
        ))
    ));
    let mut out = [0u8; 16];
    assert!(matches!(
        decoder.step(&stream, true, &mut out, &mut work),
        Err(WorkspaceDecodeError::Failed)
    ));
    decoder.reset();
    let valid = &stream[..stream.len() - 4];
    assert_eq!(run(&mut decoder, valid, 64, 64, &mut work).unwrap(), data);
}

/// The decoder can move to another thread with its workspace, as the
/// `&mut [u8]` it borrows can; the raw window pointer must not take that away.
#[test]
fn decoder_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<WorkspaceDecoder<'static>>();
}

/// Corrupting any byte of a frame is accepted or refused exactly as the
/// one-shot decoder decides, with the same output when accepted, and never
/// panics or stalls.
#[test]
fn corrupted_frames_agree_with_frame_decoder() {
    let data = text(6_000);
    let limits = DecodeLimits::new(1 << 20);
    // A single-segment frame (linear window) and a multi-segment one whose
    // 1 KiB window makes the ring wrap.
    let frames = [
        compress_slice_to_vec(&data, CompressionLevel::Default),
        compress_windowed(&data, 3, 10),
    ];
    for compressed in &frames {
        for index in 0..compressed.len() {
            for flip in [0x01u8, 0x40, 0xFF] {
                let mut corrupt = compressed.clone();
                corrupt[index] ^= flip;
                let ours =
                    decode_chunked(&corrupt, limits, 17, 251, &mut WorkBudget::new(UNLIMITED));
                if matches!(
                    ours,
                    Err(WorkspaceDecodeError::Limit(_) | WorkspaceDecodeError::Unsupported(_))
                ) {
                    // A limit or a dictionary id the corruption introduced: the
                    // one-shot decoder has neither, so there is nothing to compare.
                    continue;
                }
                let theirs = reference_decode(&corrupt, 1 << 20);
                assert_eq!(
                    ours.as_ref().ok(),
                    theirs.as_ref(),
                    "byte {index} ^ {flip:#04x}: ours {:?}",
                    ours.as_ref().err()
                );
            }
        }
    }
}
