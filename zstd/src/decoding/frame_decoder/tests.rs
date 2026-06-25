extern crate std;

use super::{DictionaryHandle, FrameDecoder};
use crate::encoding::{CompressionLevel, FrameCompressor};
use alloc::vec::Vec;

/// `FrameDecoder` must stay `Send + Sync` (multi-instance contract: one
/// dictionary shared across decoder instances on other threads). The decode
/// state owns the dictionary as a thread-safe handle (`active_dict:
/// Option<DictionaryHandle>`) and every `Dict`-sourced table read borrows it as
/// a call-scoped argument, so `Send`/`Sync` auto-derive with zero `unsafe impl`.
/// This is a COMPILE-TIME guard — if a future change adds a non-thread-safe
/// field, this fails to compile.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FrameDecoder>();
};

#[test]
fn force_dict_installs_active_dictionary_handle() {
    // Regression: `force_dict` arms `Dict`-sourced scratch tables for a
    // dictless-header frame, so it MUST also install the OWNING dictionary
    // handle (`active_dict`). Without it the block loop derives a `None`
    // dictionary borrow while the scratch sources say `Dict`, so the first
    // dict-table read hits the unwrap and panics (and dict-content matches
    // resolve against an empty/stale dictionary instead of the forced one).
    let payload: Vec<u8> = (0..256u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let raw = include_bytes!("../../../dict_tests/dictionary");
    let dict = crate::decoding::dictionary::Dictionary::decode_dict(raw).expect("parse dict");
    let dict_id = dict.id;

    let mut dec = FrameDecoder::new();
    dec.add_dict(dict).expect("register owned dict");
    dec.reset(compressed.as_slice())
        .expect("reset on a dictless-header frame");
    assert!(
        !dec.active_dict_installed(),
        "a dictless-header reset must not install any dictionary"
    );
    dec.force_dict(dict_id)
        .expect("force_dict applies the dict");
    assert!(
        dec.active_dict_installed(),
        "force_dict must install the owning dictionary handle"
    );
}

#[test]
fn decode_all_tight_and_slack_outputs_match_on_single_segment_frame() {
    // Roundtrip a small payload through the encoder, then decode
    // it via `decode_all` on two output shapes that select
    // different internal sequence-exec paths within the direct
    // decode:
    //   1. Tight output (exactly `frame_content_size`, no
    //      WILDCOPY_OVERLENGTH slack) → direct path whose trailing
    //      sequence(s) take the bounded (non-overshooting) copy in
    //      `UserSliceBackend::exec_sequence_bounded`.
    //   2. Output with WILDCOPY slack → direct path whose
    //      sequences all take the SIMD wildcopy fast path.
    // Both must produce identical output bytes — the bounded tail
    // copy must reconstruct the same data as the overshooting fast
    // path. This is the regression gate for the relaxed
    // direct-decode gate (`cap >= content_size`).
    let payload: Vec<u8> = (0..4096u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    // Baseline: tight output → legacy drain path.
    let mut dec_a = FrameDecoder::new();
    let mut out_a = alloc::vec![0u8; payload.len()];
    let n_a = dec_a
        .decode_all(compressed.as_slice(), &mut out_a)
        .expect("decode_all (legacy drain) should succeed");
    assert_eq!(n_a, payload.len());
    assert_eq!(&out_a[..n_a], payload.as_slice());

    // Direct: output with WILDCOPY slack → direct path.
    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec_b = FrameDecoder::new();
    let mut out_b = alloc::vec![0u8; payload.len() + slack];
    let n_b = dec_b
        .decode_all(compressed.as_slice(), &mut out_b)
        .expect("decode_all (direct path) should succeed");
    assert_eq!(
        n_b,
        payload.len(),
        "direct decode produced wrong byte count"
    );
    assert_eq!(&out_b[..n_b], payload.as_slice());
}

#[test]
fn decode_all_tight_output_overlapping_tail_match_roundtrips() {
    // The bounded tail copy must handle an OVERLAPPING match
    // (offset < match_length) as the trailing sequence when the
    // output slice is sized to exactly `frame_content_size`. A long
    // run of a single byte at the end of the payload encodes as an
    // offset-1 match whose length far exceeds the offset, so the
    // bounded copy's overlapping (forward byte-by-byte) branch is
    // exercised at the buffer tail where the SIMD overshoot would
    // otherwise run past `cap`. Decoding into a tight buffer and
    // matching the original payload byte-for-byte is the regression
    // gate for the overlap branch of `exec_sequence_bounded`.
    let mut payload: Vec<u8> = (0..256u32).map(|i| (i & 0xFF) as u8).collect();
    payload.extend(core::iter::repeat_n(0xABu8, 8192));
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    // Anti-vacuous precondition: the 8 KiB trailing run of a single
    // byte must compress to a Compressed block dominated by ONE long
    // offset-1 (overlapping, offset < match_length) match — not a Raw
    // block. If the encoder ever stopped emitting that overlapping
    // tail match the test would pass without exercising
    // `exec_sequence_bounded`'s overlapping forward-copy branch, so
    // gate on the output being a tiny fraction of the input (a raw
    // block would be ~`payload.len()`; an offset-1 run match is tens
    // of bytes).
    assert!(
        compressed.len() < payload.len() / 8,
        "expected an overlapping-tail match to dominate the frame \
             (compressed={} payload={}); the bounded overlap branch would \
             not be exercised otherwise",
        compressed.len(),
        payload.len(),
    );

    // Tight output: exactly content_size, no WILDCOPY slack.
    let mut dec = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload.len()];
    let n = dec
        .decode_all(compressed.as_slice(), &mut out)
        .expect("tight-output decode with overlapping tail match should succeed");
    assert_eq!(n, payload.len());
    assert_eq!(out, payload, "bounded overlap tail copy corrupted output");
}

#[test]
fn decode_all_multi_segment_frame_decodes_correctly() {
    // Multi-segment frame: payload large enough that the
    // encoder's default frame layout has `single_segment_flag =
    // false` and `window_size < frame_content_size`. The direct
    // path must cap the visible buffer at window_size after each
    // block (drop_to_window_size) so match-offset validation
    // matches the spec rule `offset <= window_size`, and still
    // produce the same bytes as decode_all on the
    // FlatBuf/Ring-backed path.
    //
    // Make the payload structured so multi-segment behavior
    // actually kicks in: 2 MiB of repeating + random-ish bytes
    // forces window_size lower than content_size at the encoder.
    let mut payload: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024);
    for i in 0..payload.capacity() {
        payload.push((i.wrapping_mul(2_654_435_761) & 0xFF) as u8);
    }
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    // Baseline: decode_all through the FlatBuf+drain path.
    let mut dec_a = FrameDecoder::new();
    let mut out_a = alloc::vec![0u8; payload.len()];
    let n_a = dec_a
        .decode_all(compressed.as_slice(), &mut out_a)
        .expect("decode_all should succeed");
    assert_eq!(n_a, payload.len());
    assert_eq!(&out_a[..n_a], payload.as_slice());

    // Direct path: must give identical bytes via UserSliceBackend
    // + per-block drop_to_window_size.
    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec_b = FrameDecoder::new();
    let mut out_b = alloc::vec![0u8; payload.len() + slack];
    let n_b = dec_b
        .decode_all(compressed.as_slice(), &mut out_b)
        .expect("decode_all should succeed on multi-segment frame");
    assert_eq!(n_b, payload.len(), "wrong byte count on direct path");
    assert_eq!(&out_b[..n_b], payload.as_slice());

    // Sanity-check: confirm the encoded frame really IS
    // multi-segment. If a future encoder default changes,
    // catching the assumption here is better than silently
    // testing single_segment on this name.
    let mut sanity = FrameDecoder::new();
    sanity.init(&mut compressed.as_slice()).unwrap();
    assert!(
        !sanity
            .state
            .as_ref()
            .unwrap()
            .frame_header
            .descriptor
            .single_segment_flag(),
        "test precondition violated: frame is single-segment, rename or resize"
    );
}

#[cfg(feature = "hash")]
#[test]
fn decode_all_propagates_checksum_into_persistent_scratch() {
    // Direct path on a checksum-flagged frame: the FrameCompressor
    // under `feature = "hash"` sets content_checksum_flag, so the
    // decoded frame has a recorded checksum. After
    // decode_all we must be able to verify it matches via
    // the public get_calculated_checksum() accessor — the digest
    // is computed by walking output at end of decode and stored
    // into the persistent scratch's hasher.
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload.len() + slack];
    let n = dec
        .decode_all(compressed.as_slice(), &mut out)
        .expect("decode_all with checksum must succeed");
    assert_eq!(n, payload.len());
    assert_eq!(&out[..n], payload.as_slice());

    // Both sides must report the same checksum: the frame header
    // carries the stored u32, and get_calculated_checksum reads
    // the running digest the direct path just propagated.
    let stored = dec.get_checksum_from_data();
    let calculated = dec.get_calculated_checksum();
    assert!(stored.is_some(), "frame must carry stored checksum");
    assert!(
        calculated.is_some(),
        "direct path must propagate calculated checksum"
    );
    assert_eq!(
        stored, calculated,
        "stored vs calculated checksum mismatch on direct path"
    );
}

#[cfg(feature = "hash")]
#[test]
fn verify_mode_accepts_a_valid_frame() {
    use crate::decoding::ContentChecksum;
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    dec.set_content_checksum(ContentChecksum::Verify);
    let mut out = alloc::vec![0u8; payload.len() + slack];
    let n = dec
        .decode_all(compressed.as_slice(), &mut out)
        .expect("Verify mode must accept a frame with a correct checksum");
    assert_eq!(&out[..n], payload.as_slice());
}

#[cfg(feature = "hash")]
#[test]
fn verify_mode_rejects_a_corrupted_checksum() {
    use crate::decoding::ContentChecksum;
    use crate::decoding::errors::FrameDecoderError;
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    // Flip a bit in the trailing 4-byte content checksum: the frame body
    // still decodes to the correct bytes, but the stored digest no longer
    // matches the one the decoder computes.
    let last = compressed.len() - 1;
    compressed[last] ^= 0xFF;

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    dec.set_content_checksum(ContentChecksum::Verify);
    let mut out = alloc::vec![0u8; payload.len() + slack];
    let err = dec
        .decode_all(compressed.as_slice(), &mut out)
        .expect_err("Verify mode must reject a corrupted checksum");
    assert!(
        matches!(err, FrameDecoderError::ChecksumMismatch { .. }),
        "expected ChecksumMismatch, got {err:?}"
    );
}

#[cfg(feature = "hash")]
#[test]
fn decode_from_to_verify_rejects_corrupted_checksum() {
    // decode_from_to has its own block loop (not decode_blocks); it must
    // still honour Verify and reject a corrupted trailer rather than
    // silently accept it.
    use crate::decoding::ContentChecksum;
    use crate::decoding::errors::FrameDecoderError;
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();
    let last = compressed.len() - 1;
    compressed[last] ^= 0xFF;

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    dec.set_content_checksum(ContentChecksum::Verify);
    let mut out = alloc::vec![0u8; payload.len() + slack];

    // Split the trailing 4-byte checksum into a SEPARATE call so the
    // verification must happen on the checksum-only early-return path (not
    // the post-drain path) — the incremental case CodeRabbit flagged.
    let split = compressed.len() - 4;
    let (_r1, w1) = dec
        .decode_from_to(&compressed[..split], &mut out)
        .expect("blocks decode without the trailer");
    let err = dec
        .decode_from_to(&compressed[split..], &mut out[w1..])
        .expect_err("decode_from_to in Verify mode must reject a corrupted checksum");
    assert!(
        matches!(err, FrameDecoderError::ChecksumMismatch { .. }),
        "expected ChecksumMismatch, got {err:?}"
    );
}

#[cfg(feature = "hash")]
#[test]
fn decode_from_to_small_target_split_trailer_flushes_tail() {
    // Regression: when a prior call decoded the last block but a small
    // `target` left output buffered, the trailer-only call must still flush
    // the buffered tail (it used to early-return Ok((4,0)) and lose it).
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let split = compressed.len() - 4;
    let mut dec = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload.len()];
    // Call 1: all blocks, but a SMALL (64-byte) target leaves the rest
    // buffered on the decoder side.
    let (_r1, w1) = dec
        .decode_from_to(&compressed[..split], &mut out[..64])
        .expect("blocks decode with a small target");
    assert!(w1 <= 64);
    // Call 2: the 4-byte trailer alone must flush the buffered tail through
    // the shared read path, not return early and drop it.
    let (_r2, w2) = dec
        .decode_from_to(&compressed[split..], &mut out[w1..])
        .expect("trailer call must flush the buffered tail");
    assert_eq!(w1 + w2, payload.len(), "buffered tail was dropped");
    assert_eq!(&out[..w1 + w2], payload.as_slice());
}

#[cfg(feature = "hash")]
#[test]
fn none_mode_skips_the_checksum_pass() {
    use crate::decoding::ContentChecksum;
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    dec.set_content_checksum(ContentChecksum::None);
    let mut out = alloc::vec![0u8; payload.len() + slack];
    let n = dec
        .decode_all(compressed.as_slice(), &mut out)
        .expect("None mode must still decode correctly");
    assert_eq!(&out[..n], payload.as_slice());
    // No digest is computed in None mode, even though the frame carries one.
    assert!(dec.get_checksum_from_data().is_some());
    assert!(dec.get_calculated_checksum().is_none());
}

#[cfg(feature = "hash")]
#[test]
fn encoder_without_checksum_emits_no_trailing_digest() {
    let payload: Vec<u8> = (0..8192u32).map(|i| (i & 0xFF) as u8).collect();

    let mut with = Vec::new();
    let mut c_with = FrameCompressor::new(CompressionLevel::Default);
    c_with.set_content_checksum(true);
    c_with.set_source(payload.as_slice());
    c_with.set_drain(&mut with);
    c_with.compress();

    let mut without = Vec::new();
    let mut c_without = FrameCompressor::new(CompressionLevel::Default);
    c_without.set_content_checksum(false);
    c_without.set_source(payload.as_slice());
    c_without.set_drain(&mut without);
    c_without.compress();

    // The checksum-off frame is exactly the 4-byte trailing digest shorter.
    assert_eq!(with.len(), without.len() + 4);

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload.len() + slack];
    let n = dec
        .decode_all(without.as_slice(), &mut out)
        .expect("a frame without a content checksum must decode");
    assert_eq!(&out[..n], payload.as_slice());
    assert!(
        dec.get_checksum_from_data().is_none(),
        "no trailing checksum should be reported"
    );
}

#[test]
fn decode_all_fcs_overflow_via_corrupt_frame_returns_structured_error() {
    // Hand-build a corrupt frame that declares
    // frame_content_size = 4 but the (last) block carries a
    // larger Raw payload. The pre-flight FCS check inside the
    // direct path's block loop catches this and returns the
    // structured FrameContentSizeMismatch variant — not a
    // panic, not a generic TargetTooSmall.
    //
    // Frame layout (single_segment, FCS=4):
    //   magic            4 bytes  0xFD2FB528
    //   FHD              1 byte   single_segment=1, no checksum,
    //                              FCS field size = 0 (-> 1-byte FCS)
    //   FCS              1 byte   0x04
    //   block_header     3 bytes  last=1, type=Raw, block_size=10
    //   block_payload    10 bytes 0xAA repeated
    let mut frame = alloc::vec::Vec::new();
    // magic
    frame.extend_from_slice(&0xFD2FB528u32.to_le_bytes());
    // FHD: single_segment=1, fcs_flag=0 (1-byte FCS), no checksum,
    // no dict. Bit layout: FCS(7-6)=0, single_segment(5)=1,
    // reserved/uncs(4)=0, content_checksum(2)=0, dict(0-1)=00.
    frame.push(0b0010_0000);
    // FCS: 1 byte
    frame.push(4);
    // Block header: cBlockSize=10, type=Raw (0), last=1
    // 3-byte LE: bit0=last, bits1-2=type(2 bits), bits3-23=size
    let cblock_size: u32 = 10;
    let bh: u32 = 1 | (cblock_size << 3); // last=1, type=Raw=0
    frame.push((bh & 0xFF) as u8);
    frame.push((bh >> 8) as u8);
    frame.push((bh >> 16) as u8);
    // Payload — 10 bytes that, if decoded, would exceed FCS=4.
    frame.extend(core::iter::repeat_n(0xAAu8, 10));

    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut dec = FrameDecoder::new();
    let mut out = alloc::vec![0u8; 4 + slack];
    let err = dec
        .decode_all(&frame, &mut out)
        .expect_err("FCS-overflow frame must fail decode");
    assert!(
        matches!(
            err,
            super::FrameDecoderError::FrameContentSizeMismatch { .. }
        ),
        "expected FrameContentSizeMismatch, got {:?}",
        err
    );
}

#[test]
fn decode_all_compressed_block_fcs_overflow_returns_structured_error() {
    // Acceptance test for #246: a malformed frame whose *Compressed*
    // block expands past the declared `frame_content_size` must
    // surface `FrameContentSizeMismatch` from the direct-decode path
    // (UserSliceBackend sequence executor), NOT panic and NOT a
    // generic FailedToReadBlockBody. The Raw-block sibling above
    // covers the `BackendOverflow` arm; this covers the Compressed
    // sequence-executor overflow arm (`ExecuteSequencesError::
    // OutputBufferOverflow` folded into FrameContentSizeMismatch in
    // `run_direct_decode`).
    //
    // Construction: compress a compressible payload to get a genuine
    // Compressed block + a header-declared FCS, then surgically patch
    // the FCS field down to a tiny value. The block body still
    // decodes (literals/sequences are independent of FCS) and the
    // sequence executor overflows the small output slice.
    // Highly compressible payload (repeated phrase) → Compressed
    // block whose sequence executor produces ~4 KiB of output.
    let unit = b"The quick brown fox jumps over the lazy dog. ";
    let mut payload = Vec::with_capacity(4 * 1024);
    while payload.len() < 4 * 1024 {
        payload.extend_from_slice(unit);
    }
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut frame = Vec::new();
    compressor.set_drain(&mut frame);
    compressor.compress();
    // Sanity: the encoder actually compressed (=> a Compressed block,
    // not a raw-stored fallback) so we exercise the sequence path.
    assert!(frame.len() < payload.len());

    // Locate the FCS field: it is the last `fcs_len` bytes of the
    // frame header, whose total size `header_size` includes the magic.
    // A ~4 KiB single-segment frame declares FCS = 4096, which lands in
    // the 2-byte field range [256, 65791] (RFC 8878 §3.1.1.1.4) — assert
    // that so the patch logic below stays a single deterministic branch.
    let (header, header_size) =
        super::super::frame::read_frame_header(frame.as_slice()).expect("valid header");
    let fcs_len = header
        .descriptor
        .frame_content_size_bytes()
        .expect("fcs present") as usize;
    assert_eq!(
        fcs_len, 2,
        "4 KiB single-segment frame must use a 2-byte FCS"
    );
    let fcs_off = header_size as usize - fcs_len;

    // Patch the 2-byte FCS to its floor: stored bytes 0 decode to 256
    // (the field's `+256` bias), far below the 4 KiB the block actually
    // produces, so the sequence executor overflows the output slice.
    let patched_declared: u64 = 256;
    frame[fcs_off] = 0;
    frame[fcs_off + 1] = 0;

    // Size the output to declared + WILDCOPY slack so the direct path
    // is eligible (output.len() >= content_size + slack) — the
    // overflow then comes from the frame, not an undersized buffer.
    let slack = super::super::buffer_backend::WILDCOPY_OVERLENGTH;
    let mut out = alloc::vec![0u8; patched_declared as usize + slack];
    let mut dec = FrameDecoder::new();
    let err = dec
        .decode_all(frame.as_slice(), &mut out)
        .expect_err("Compressed block exceeding FCS must fail decode");
    match err {
        super::FrameDecoderError::FrameContentSizeMismatch { declared, produced } => {
            assert_eq!(declared, patched_declared, "declared echoes patched FCS");
            assert!(produced > declared, "produced must exceed declared");
        }
        other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
    }
}

/// Block-precise error positions (#174): a failing block header / body
/// reports its 0-based index and frame-absolute offset, consistent with
/// the encoder's `FrameEmitInfo.blocks[index].offset_in_frame`.
#[cfg(feature = "lsm")]
#[test]
fn block_precise_errors_carry_index_and_offset() {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    // ~1.3 MiB of incompressible (xorshift) bytes → many 128 KiB raw
    // blocks, so blocks 3 and 7 both exist and are not the last block.
    let mut data = alloc::vec::Vec::with_capacity(1_300_000);
    let mut s: u64 = 0x2545_F491_4F6C_DD1D;
    while data.len() < 1_300_000 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        data.push((s >> 33) as u8);
    }

    let mut frame = alloc::vec::Vec::new();
    let blocks = {
        let mut fc = FrameCompressor::new(CompressionLevel::Level(1));
        fc.set_source(data.as_slice());
        fc.set_drain(&mut frame);
        fc.compress();
        fc.last_frame_emit_info()
            .expect("emit info present under lsm")
            .blocks
            .clone()
    };
    assert!(blocks.len() > 7, "need >7 blocks, got {}", blocks.len());

    let mut out = alloc::vec![0u8; data.len() + 4096];

    // (1) Corrupt block 7's header: force its Block_Type to Reserved (3)
    // by setting both type bits — fails the header read at block 7.
    let off7 = blocks[7].offset_in_frame as usize;
    let mut corrupt = frame.clone();
    corrupt[off7] |= 0b0000_0110;
    let mut dec = FrameDecoder::new();
    let err = dec
        .decode_all(&corrupt, &mut out)
        .expect_err("reserved block-7 header must fail");
    match err {
        super::FrameDecoderError::FailedToReadBlockHeaderAt {
            block_index,
            frame_offset,
            ..
        } => {
            assert_eq!(block_index, 7);
            assert_eq!(frame_offset, blocks[7].offset_in_frame);
        }
        other => panic!("expected FailedToReadBlockHeaderAt, got {other:?}"),
    }

    // (2) Truncate at block 3's body start: header intact, body missing
    // → the body decode fails at block 3 with its FrameBlock metadata.
    let body3 = blocks[3].offset_in_frame as usize + blocks[3].header_size as usize;
    let mut dec = FrameDecoder::new();
    let err = dec
        .decode_all(&frame[..body3], &mut out)
        .expect_err("truncated block-3 body must fail");
    match err {
        super::FrameDecoderError::FailedToReadBlockBodyAt {
            block_index,
            frame_offset,
            block,
            ..
        } => {
            assert_eq!(block_index, 3);
            assert_eq!(frame_offset, blocks[3].offset_in_frame);
            assert_eq!(block.offset_in_frame, blocks[3].offset_in_frame);
        }
        other => panic!("expected FailedToReadBlockBodyAt, got {other:?}"),
    }
}

#[test]
fn decode_all_exact_fit_output_decodes_correctly() {
    // Output sized exactly to frame_content_size (no
    // WILDCOPY_OVERLENGTH slack) is now eligible for the direct
    // path: every output-write site is exact-fit-safe (sequence
    // exec falls back to the bounded, non-overshooting copy on the
    // trailing sequence(s), Raw/RLE blocks copy exactly). This must
    // produce the same bytes as a slack-padded buffer. Exercised on
    // x86 through the per-kernel AVX2/SSE2 inline-exec macros, which
    // carry the same tight-tail branch.
    let payload: Vec<u8> = (0..2048u32)
        .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
        .collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut dec = FrameDecoder::new();
    // Exactly payload.len(), no slack.
    let mut out = alloc::vec![0u8; payload.len()];
    let n = dec
        .decode_all(compressed.as_slice(), &mut out)
        .expect("exact-fit decode_all should succeed");
    assert_eq!(n, payload.len());
    assert_eq!(&out[..n], payload.as_slice());
}

#[test]
fn decode_all_fallback_validates_fcs_against_total_output() {
    // Synthetic single-segment frame: FCS = 20 bytes, but the
    // last-block flag fires after only 4 bytes of raw payload.
    // On the direct path this would trip the post-block
    // `produced > content_size` check; the fallback path
    // (eligible=false because output is sized exactly to FCS,
    // no WILDCOPY slack) used to silently return Ok(4). With
    // the fix it now surfaces `FrameContentSizeMismatch`
    // matching the direct path.
    //
    // Frame layout: 4 B magic | 1 B FHD (single_segment=1,
    // FCS_flag=3 → 8-byte FCS) | 8 B FCS=20 | block header
    // (Raw, last, size=4) | 4 raw bytes.
    let mut wire = Vec::new();
    wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes()); // magic
    // FHD: FCS_flag=3 (8-byte FCS) <<6 | single_segment=1 <<5.
    wire.push(0b1110_0000);
    wire.extend_from_slice(&20u64.to_le_bytes()); // declared FCS
    // Block header: (size << 3) | (block_type << 1) | last_block.
    // Raw block (block_type=0), last_block=1, size=4 → 0b00100001 = 0x21.
    wire.push(0x21);
    wire.push(0x00);
    wire.push(0x00);
    wire.extend_from_slice(&[1u8, 2, 3, 4]);

    let mut dec = FrameDecoder::new();
    // Size output SMALLER than the declared FCS so direct-decode is
    // gated out (`output.len() >= content_size` is false) and the
    // frame takes the legacy fallback drain loop — the path this test
    // guards. The corrupt frame only produces 4 bytes, so 19 is ample
    // room; the point is `19 != declared FCS (20)`.
    const DECLARED_FCS: usize = 20;
    let mut out = alloc::vec![0u8; DECLARED_FCS - 1];
    assert_ne!(
        out.len(),
        DECLARED_FCS,
        "output must be smaller than FCS to exercise the fallback path",
    );
    let err = dec
        .decode_all(wire.as_slice(), &mut out)
        .expect_err("fallback must reject corrupt FCS underflow");
    match err {
        crate::decoding::errors::FrameDecoderError::FrameContentSizeMismatch {
            declared,
            produced,
        } => {
            assert_eq!(declared, 20);
            assert_eq!(produced, 4);
        }
        other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
    }
}

#[test]
fn decode_all_fallback_treats_explicit_fcs_zero_as_declared() {
    // Synthetic multi-segment frame with FCS_flag=2 (4-byte
    // FCS) explicitly set to 0. The header DECLARES zero
    // content, but the body carries a 5-byte raw last-block.
    // `fcs_declared()` must return true (the field is on the
    // wire) so the fallback's post-decode size check sees the
    // mismatch — even though `frame_content_size == 0`. This
    // is exactly the FCS=0 edge case where the previous
    // `content_size > 0` proxy would have silently accepted
    // the corrupt frame.
    //
    // Frame layout:
    //   4 B magic            — 28 B5 2F FD
    //   1 B FHD              — FCS_flag=2 (bits 7-6), no
    //                          single_segment, content_checksum=0,
    //                          dict_id_flag=0 → 0b1000_0000
    //   1 B window_descriptor — exp=10, mantissa=0 → window=1 MiB
    //   4 B FCS              — 0 LE
    //   3 B block header     — raw, last, size=5 → 0x29 0x00 0x00
    //   5 B raw payload      — anything non-empty
    let mut wire = Vec::new();
    wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    wire.push(0b1000_0000); // FHD: FCS_flag=2, others 0.
    wire.push(0x50); // window_descriptor: exp=10, mantissa=0.
    wire.extend_from_slice(&0u32.to_le_bytes()); // FCS = 0.
    // Block header (24-bit LE): (size << 3) | (block_type << 1) | last_block
    // = (5 << 3) | (0 << 1) | 1 = 0x29.
    wire.push(0x29);
    wire.push(0x00);
    wire.push(0x00);
    wire.extend_from_slice(&[1u8, 2, 3, 4, 5]);

    let mut dec = FrameDecoder::new();
    // FCS=0 declared, so eligibility (`content_size > 0`)
    // false — falls through to the drain loop. Output buffer
    // size doesn't matter for the eligibility check here;
    // give it some room so `read()` can drain the block.
    let mut out = alloc::vec![0u8; 16];
    let err = dec
        .decode_all(wire.as_slice(), &mut out)
        .expect_err("corrupt FCS=0 + 5-byte block must error");
    match err {
        crate::decoding::errors::FrameDecoderError::FrameContentSizeMismatch {
            declared,
            produced,
        } => {
            assert_eq!(declared, 0);
            assert_eq!(produced, 5);
        }
        other => panic!("expected FrameContentSizeMismatch, got {other:?}"),
    }
}

#[test]
fn decode_all_fallback_accepts_honest_explicit_fcs_zero() {
    // Companion to the corrupt-FCS=0 test above: an HONEST
    // empty frame with FCS_flag=2 (4-byte FCS) explicitly set
    // to 0 AND a 0-byte raw last-block. `fcs_declared()`
    // returns true and `content_size == 0 == total_written`,
    // so the fallback validation accepts the frame instead of
    // misreporting a mismatch.
    //
    // (Single-segment FCS=0 would test a similar invariant
    // but trips header-stage validation: `window_size =
    // frame_content_size = 0 < MIN_WINDOW_SIZE` fails the
    // window-size sanity check before decode runs. Use the
    // multi-segment shape where `window_size` comes from
    // `window_descriptor` independently of FCS.)
    //
    // Frame layout:
    //   4 B magic
    //   1 B FHD              — FCS_flag=2, others 0 → 0x80
    //   1 B window_descriptor — exp=10 → 1 MiB window
    //   4 B FCS              — 0 LE
    //   3 B block header     — raw, last, size=0 → 0x01 0x00 0x00
    let mut wire = Vec::new();
    wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    wire.push(0b1000_0000);
    wire.push(0x50);
    wire.extend_from_slice(&0u32.to_le_bytes());
    // Block header: (0 << 3) | (0 << 1) | 1 = 0x01.
    wire.push(0x01);
    wire.push(0x00);
    wire.push(0x00);

    let mut dec = FrameDecoder::new();
    let mut out = alloc::vec![0u8; 16];
    let n = dec
        .decode_all(wire.as_slice(), &mut out)
        .expect("honest FCS=0 + empty block must succeed");
    assert_eq!(n, 0);
}

#[test]
fn reset_with_dict_handle_applies_dict_when_no_dict_id() {
    let payload = b"reset-without-dict-id";
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let handle = DictionaryHandle::decode_dict(dict_raw).expect("dictionary should parse");

    let mut decoder = FrameDecoder::new();
    decoder
        .reset_with_dict_handle(compressed.as_slice(), &handle)
        .expect("reset should succeed");
    let state = decoder.state.as_ref().expect("state should be initialized");
    assert!(state.frame_header.dictionary_id().is_none());
    assert_eq!(state.using_dict, Some(handle.id()));
}

#[test]
fn reserve_buffer_reserves_the_shortfall_not_the_full_window_again() {
    // `Vec::reserve_exact` takes ADDITIONAL capacity. The decode_all
    // fallback loop re-enters decode_blocks once per strategy chunk,
    // and each entry pre-reserves the window: re-requesting the FULL
    // window on a buffer already holding ~window bytes of history
    // would grow it toward 2x window, defeating the peak-memory cap
    // the exact-growth policy exists for.
    use super::DecoderScratchKind;
    let window = 1usize << 20;
    let mut scratch = DecoderScratchKind::new_flat(window);
    scratch.reserve_buffer(window);
    let data = alloc::vec![0u8; window];
    match &mut scratch {
        super::DecoderScratchKind::Flat(s) => s.buffer.push(&data),
        super::DecoderScratchKind::Ring(_) => unreachable!("new_flat builds Flat"),
    }
    scratch.reserve_buffer(window);
    let workspace = scratch.workspace_bytes();
    assert!(
        workspace < window * 3 / 2,
        "second reserve_buffer grew a full window past the buffered \
             history: workspace {workspace} bytes vs window {window}"
    );
}

#[test]
fn dict_frame_decodes_through_direct_path() {
    // A dictionary frame decoded via `decode_all_with_dict_handle`
    // into a buffer sized exactly to FCS takes the direct path
    // (UserSliceBackend); matches reaching into the dictionary
    // content must resolve through `repeat_from_dict`. The payload
    // embeds dictionary content verbatim so the encoder emits
    // dict-region matches from the first bytes of the frame.
    let dict_raw = include_bytes!("../../../dict_tests/dictionary");
    let handle = DictionaryHandle::decode_dict(dict_raw).expect("dictionary should parse");
    let dict_tail: alloc::vec::Vec<u8> = handle
        .as_dict()
        .dict_content
        .iter()
        .rev()
        .take(2048)
        .rev()
        .copied()
        .collect();
    // No in-frame duplicate of the dictionary bytes: with a second
    // copy in the payload the encoder may emit the later copy as an
    // in-frame match, and the test would stay green even if the
    // direct path stopped forwarding the dictionary handle. A
    // single copy forces every dict-region match through
    // `repeat_from_dict`.
    let mut payload = dict_tail;
    payload.extend_from_slice(b"unique suffix after dictionary material 0123456789");

    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor
        .set_dictionary_from_bytes(dict_raw)
        .expect("dict load");
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    // Fixture sanity: the frame must actually depend on the
    // dictionary, otherwise the decode below never exercises
    // dict-region match resolution.
    let mut plain = Vec::new();
    let mut no_dict = FrameCompressor::new(CompressionLevel::Default);
    no_dict.set_source(payload.as_slice());
    no_dict.set_drain(&mut plain);
    no_dict.compress();
    assert!(
        compressed.len() < plain.len(),
        "fixture must depend on the dictionary: dict {} bytes vs plain {} bytes",
        compressed.len(),
        plain.len()
    );

    let mut decoder = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload.len()];
    let n = decoder
        .decode_all_with_dict_handle(compressed.as_slice(), &mut out, &handle)
        .expect("dict frame must decode on the direct path");
    assert_eq!(n, payload.len());
    assert_eq!(out, payload, "direct-path dict decode must be byte-exact");
    // Both paths are byte-identical, so pin the dispatch itself: a
    // re-introduced dict exclusion in the direct gate would silently
    // fall back to the buffered path and leave the asserts above green.
    assert_eq!(
        decoder.direct_frames, 1,
        "dict frame must take the direct path, not the buffered fallback"
    );
}

#[test]
fn implausible_content_size_skips_eager_alloc_direct_path() {
    // Adversarial frame: a 1 KiB window (small ring) but a declared
    // content size of 4 MiB, followed by a truncated raw block. The
    // direct path would `resize` the caller's Vec to the pledged 4 MiB
    // (allocating + zeroing it) BEFORE the truncated body is validated.
    // The gate must reject the implausible size (4 MiB cannot come from
    // 3 compressed bytes) and fall through to the window-bounded ring
    // drain, which errors without ever allocating the pledged size.
    //
    // Hand-built so the declared size is fully decoupled from the real
    // (tiny) input — the encoder always writes a truthful FCS.
    let frame: &[u8] = &[
        0x28, 0xB5, 0x2F, 0xFD, // magic
        0x80, // FHD: multi-segment, 4-byte FCS field, no dict
        0x00, // window descriptor -> 1 KiB window
        0x00, 0x00, 0x40, 0x00, // FCS = 4 MiB
        0x21, 0x03, 0x00, // raw block header: last, size 100, no body
    ];

    let mut dec = FrameDecoder::new();
    let mut src = frame;
    dec.init(&mut src).expect("header must parse");
    // `src` now points past the header at the truncated 3-byte block.
    let mut out = Vec::new();
    let err = dec.decode_current_frame_to_vec(src, &mut out, None);
    assert!(
        err.is_err(),
        "truncated body must fail regardless of decode path"
    );
    assert_eq!(
        dec.direct_frames, 0,
        "implausible FCS must NOT take the eager-alloc direct path"
    );
}

#[test]
fn implausible_single_segment_fcs_rejected_before_window_reservation() {
    // Single-segment adversarial frame: the window equals the declared
    // content size (4 MiB) by definition, so the fallback ring drain would
    // pre-reserve that whole window via `useful_window_size()` before the
    // truncated body errors — the multi-segment gate test does not cover
    // this. The implausible size (4 MiB cannot come from 3 compressed
    // bytes) must be rejected up front with a content-size error, NOT a
    // block-body error after the reservation.
    let frame: &[u8] = &[
        0x28, 0xB5, 0x2F, 0xFD, // magic
        0xA0, // FHD: single-segment, 4-byte FCS field
        0x00, 0x00, 0x40, 0x00, // FCS = 4 MiB (== window for single-segment)
        0x21, 0x03, 0x00, // raw block header: last, size 100, no body
    ];

    let mut dec = FrameDecoder::new();
    let mut src = frame;
    dec.init(&mut src).expect("header must parse");
    let mut out = Vec::new();
    let err = dec
        .decode_current_frame_to_vec(src, &mut out, None)
        .expect_err("implausible single-segment FCS must be rejected");
    match err {
        super::FrameDecoderError::FrameContentSizeMismatch { declared, .. } => {
            assert_eq!(declared, 4 * 1024 * 1024);
        }
        other => {
            panic!("expected early FrameContentSizeMismatch (no window reservation), got {other:?}")
        }
    }
    assert_eq!(
        dec.direct_frames, 0,
        "implausible FCS must not take the eager-alloc direct path"
    );
}

#[cfg(feature = "lsm")]
mod expect_validation {
    use super::*;
    use crate::decoding::errors::FrameDecoderError;

    fn compress(payload: &[u8]) -> Vec<u8> {
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor.set_source(payload);
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();
        compressed
    }

    fn compress_with_dict(payload: &[u8], dict_raw: &[u8]) -> Vec<u8> {
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        compressor
            .set_dictionary_from_bytes(dict_raw)
            .expect("dict load");
        compressor.set_source(payload);
        let mut compressed = Vec::new();
        compressor.set_drain(&mut compressed);
        compressor.compress();
        compressed
    }

    #[test]
    fn expect_dict_id_none_default_allows_anything() {
        let compressed = compress(b"hello-no-expect");
        let mut decoder = FrameDecoder::new();
        decoder
            .reset(compressed.as_slice())
            .expect("default None passes");
    }

    #[test]
    fn expect_dict_id_zero_matches_frame_without_dict_id() {
        // Default-encoded frame has no dict_id; pinning Some(0)
        // ("no dictionary expected") must accept it.
        let compressed = compress(b"payload");
        let mut decoder = FrameDecoder::new();
        decoder.expect_dict_id(Some(0));
        decoder
            .reset(compressed.as_slice())
            .expect("Some(0) ~ None");
    }

    #[test]
    fn expect_dict_id_matching_value_passes() {
        let dict_raw = include_bytes!("../../../dict_tests/dictionary");
        let handle = DictionaryHandle::decode_dict(dict_raw).expect("dict parse");
        let actual_id = handle.id();

        let compressed = compress_with_dict(b"payload-with-dict", dict_raw);

        let mut decoder = FrameDecoder::new();
        decoder.expect_dict_id(Some(actual_id));
        // Decode requires the dict to be registered; using
        // reset_with_dict_handle for that.
        decoder
            .reset_with_dict_handle(compressed.as_slice(), &handle)
            .expect("matching dict_id passes");
    }

    #[test]
    fn expect_dict_id_mismatching_value_fails_before_decode() {
        let dict_raw = include_bytes!("../../../dict_tests/dictionary");
        let handle = DictionaryHandle::decode_dict(dict_raw).expect("dict parse");
        let actual_id = handle.id();
        let wrong_id = actual_id.wrapping_add(1);

        let compressed = compress_with_dict(b"payload-with-dict", dict_raw);

        let mut decoder = FrameDecoder::new();
        decoder.expect_dict_id(Some(wrong_id));
        let err = decoder
            .reset_with_dict_handle(compressed.as_slice(), &handle)
            .expect_err("mismatch must fail");
        match err {
            FrameDecoderError::UnexpectedDictId { expected, found } => {
                assert_eq!(expected, Some(wrong_id));
                assert_eq!(found, Some(actual_id));
            }
            other => panic!("expected UnexpectedDictId, got {other:?}"),
        }
    }

    #[test]
    fn expect_dict_id_nonzero_fails_on_frame_without_dict_id() {
        // Frame has no dict_id; expecting Some(42) (non-zero)
        // must fail with found = None.
        let compressed = compress(b"no-dict-frame");
        let mut decoder = FrameDecoder::new();
        decoder.expect_dict_id(Some(42));
        let err = decoder
            .reset(compressed.as_slice())
            .expect_err("nonzero expectation on dictless frame must fail");
        match err {
            FrameDecoderError::UnexpectedDictId { expected, found } => {
                assert_eq!(expected, Some(42));
                assert_eq!(found, None);
            }
            other => panic!("expected UnexpectedDictId, got {other:?}"),
        }
    }

    #[test]
    fn expect_window_descriptor_none_default_allows_anything() {
        let compressed = compress(b"hello-no-wd-expect");
        let mut decoder = FrameDecoder::new();
        decoder
            .reset(compressed.as_slice())
            .expect("default None passes");
    }

    #[test]
    fn expect_window_descriptor_mismatch_fails_before_decode() {
        // Compress a payload large enough to force a
        // multi-segment frame (window_descriptor on wire).
        // Default compression at >256 KiB produces multi-
        // segment frames with a real window_descriptor byte.
        let payload = alloc::vec![0xABu8; 512 * 1024];
        let compressed = compress(&payload);

        // Read the actual window_descriptor by decoding once
        // without expectations, then pin a wrong value.
        let mut probe_decoder = FrameDecoder::new();
        probe_decoder.reset(compressed.as_slice()).unwrap();
        let probe_state = probe_decoder.state.as_ref().unwrap();
        let actual_wd = probe_state
            .frame_header
            .window_descriptor()
            .expect("multi-segment frame should expose window_descriptor");
        let wrong_wd = actual_wd.wrapping_add(0x10); // bump exponent

        let mut decoder = FrameDecoder::new();
        decoder.expect_window_descriptor(Some(wrong_wd));
        let err = decoder
            .reset(compressed.as_slice())
            .expect_err("wrong window_descriptor must fail");
        match err {
            FrameDecoderError::UnexpectedWindowDescriptor { expected, found } => {
                assert_eq!(expected, wrong_wd);
                assert_eq!(found, Some(actual_wd));
            }
            other => panic!("expected UnexpectedWindowDescriptor, got {other:?}"),
        }
    }

    /// Build a minimal synthetic single-segment zstd frame
    /// carrying a 4-byte raw payload. RFC 8878 §3.1.1.1
    /// layout, hand-rolled because our default
    /// `FrameCompressor` settings don't emit
    /// `single_segment_flag` for tiny inputs.
    ///
    /// Wire bytes (13 total for 4-byte payload):
    /// ```text
    /// 28 B5 2F FD       magic
    /// 20                FHD: single_segment=1, FCS_flag=0
    /// 04                FCS (single byte, value = payload.len())
    /// 21 00 00          block header: raw, last, size=4
    /// .. .. .. ..       payload bytes
    /// ```
    fn synth_single_segment_frame(payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 255, "1-byte FCS field caps at 255");
        assert!(payload.len() < (1usize << 21), "block size 21-bit max");
        let mut out = Vec::new();
        // Magic 0xFD2FB528 LE.
        out.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
        // FHD: single_segment_flag (bit 5) set, everything
        // else zero. With single_segment + FCS_flag=0 the FCS
        // field is 1 byte. No window_descriptor on wire.
        out.push(0b0010_0000);
        // 1-byte FCS = payload length.
        out.push(payload.len() as u8);
        // Block header (3 bytes LE):
        // last_block=1, block_type=0 (Raw), block_size=payload.len().
        // Encoded: (size << 3) | (block_type << 1) | last_block.
        // Block header: last_block flag in bit 0, block_type
        // (0 = Raw) in bits 1-2, block size in bits 3+.
        let bh: u32 = ((payload.len() as u32) << 3) | 1;
        out.push((bh & 0xFF) as u8);
        out.push(((bh >> 8) & 0xFF) as u8);
        out.push(((bh >> 16) & 0xFF) as u8);
        // Raw payload.
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn expect_window_descriptor_on_single_segment_frame_fails_with_found_none() {
        // Single-segment frames omit the window_descriptor
        // byte from the wire entirely. Setting an expectation
        // here must surface `found: None` so callers
        // distinguish "wrong descriptor" from "no descriptor
        // on the wire" — never silently pass.
        let compressed = synth_single_segment_frame(b"tiny");

        // First sanity-check: the synthetic frame decodes
        // cleanly without any expectation.
        {
            let mut probe = FrameDecoder::new();
            probe
                .reset(compressed.as_slice())
                .expect("synth frame parses");
            let probe_state = probe.state.as_ref().unwrap();
            assert!(
                probe_state.frame_header.window_descriptor().is_none(),
                "synth frame must be single-segment"
            );
        }

        let mut decoder = FrameDecoder::new();
        decoder.expect_window_descriptor(Some(0x40));
        let err = decoder
            .reset(compressed.as_slice())
            .expect_err("single-segment + expectation must fail");
        match err {
            FrameDecoderError::UnexpectedWindowDescriptor { expected, found } => {
                assert_eq!(expected, 0x40);
                assert_eq!(found, None);
            }
            other => panic!("expected UnexpectedWindowDescriptor, got {other:?}"),
        }
    }

    #[test]
    fn validation_failure_leaves_decoder_re_resettable() {
        // After UnexpectedDictId on a wrong-expectation reset,
        // clearing the expectation and re-calling reset must
        // succeed on the same source — no lingering failed
        // state.
        let compressed = compress(b"re-resettable");

        let mut decoder = FrameDecoder::new();
        decoder.expect_dict_id(Some(42));
        let err = decoder
            .reset(compressed.as_slice())
            .expect_err("first reset fails");
        assert!(matches!(err, FrameDecoderError::UnexpectedDictId { .. }));

        // Clear expectation and retry on a fresh source.
        decoder.expect_dict_id(None);
        decoder
            .reset(compressed.as_slice())
            .expect("retry after clearing expectation should succeed");
    }
}

/// Build a skippable frame on the wire: 4-byte LE magic + 4-byte LE
/// length + payload bytes. RFC 8878 §3.1.2 restricts the magic
/// variant to `0..=15`; assert here so accidental misuse of the
/// helper can't smuggle a non-skippable magic past the tests.
#[cfg(feature = "lsm")]
fn build_skippable_frame(variant: u8, payload: &[u8]) -> Vec<u8> {
    assert!(
        variant <= 15,
        "skippable-frame variant {variant} outside RFC 8878 0..=15 range",
    );
    let mut out = Vec::with_capacity(8 + payload.len());
    let magic: u32 = 0x184D2A50 + u32::from(variant);
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(feature = "lsm")]
#[test]
fn decode_all_with_skippable_visitor_sees_payloads_in_order() {
    // Build a stream: skippable(v0, "alpha") + zstd_frame +
    // skippable(v3, "beta") + zstd_frame + skippable(v15, "")
    // and verify the visitor is invoked exactly three times with
    // the correct (variant, payload) pairs in stream order while
    // the zstd frames decode normally.
    let payload_a: Vec<u8> = (0..256u16).map(|i| i as u8).collect();
    let payload_b: Vec<u8> = (0..256u16).map(|i| (i ^ 0xAA) as u8).collect();

    let mut comp_a = Vec::new();
    let mut c = FrameCompressor::new(CompressionLevel::Default);
    c.set_source(payload_a.as_slice());
    c.set_drain(&mut comp_a);
    c.compress();

    let mut comp_b = Vec::new();
    let mut c = FrameCompressor::new(CompressionLevel::Default);
    c.set_source(payload_b.as_slice());
    c.set_drain(&mut comp_b);
    c.compress();

    let skip0 = build_skippable_frame(0, b"alpha");
    let skip3 = build_skippable_frame(3, b"beta");
    let skip15 = build_skippable_frame(15, &[]);

    let mut stream = Vec::new();
    stream.extend_from_slice(&skip0);
    stream.extend_from_slice(&comp_a);
    stream.extend_from_slice(&skip3);
    stream.extend_from_slice(&comp_b);
    stream.extend_from_slice(&skip15);

    let mut decoder = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload_a.len() + payload_b.len()];
    let mut collected: Vec<(u8, Vec<u8>)> = Vec::new();
    let n = decoder
        .decode_all_with_skippable_visitor(stream.as_slice(), &mut out, |variant, payload| {
            collected.push((variant, payload.to_vec()));
        })
        .expect("decode_all_with_skippable_visitor should succeed");

    // All three skippables visited in stream order.
    assert_eq!(collected.len(), 3);
    assert_eq!(collected[0], (0u8, b"alpha".to_vec()));
    assert_eq!(collected[1], (3u8, b"beta".to_vec()));
    assert_eq!(collected[2], (15u8, Vec::<u8>::new()));

    // Both zstd frames decoded into `out` back-to-back.
    assert_eq!(n, payload_a.len() + payload_b.len());
    assert_eq!(&out[..payload_a.len()], payload_a.as_slice());
    assert_eq!(&out[payload_a.len()..n], payload_b.as_slice());
}

#[cfg(feature = "lsm")]
#[test]
fn decode_all_silently_skips_when_no_visitor() {
    // Regression gate: plain decode_all must still silently skip
    // skippable frames (RFC 8878 mandated behavior) with no
    // behavioral change after the visitor refactor.
    let payload: Vec<u8> = (0..512u16).map(|i| i as u8).collect();
    let mut comp = Vec::new();
    let mut c = FrameCompressor::new(CompressionLevel::Default);
    c.set_source(payload.as_slice());
    c.set_drain(&mut comp);
    c.compress();

    let skip = build_skippable_frame(7, b"ignored sidecar");
    let mut stream = Vec::new();
    stream.extend_from_slice(&skip);
    stream.extend_from_slice(&comp);

    let mut decoder = FrameDecoder::new();
    let mut out = alloc::vec![0u8; payload.len()];
    let n = decoder
        .decode_all(stream.as_slice(), &mut out)
        .expect("decode_all should succeed on skippable + zstd stream");
    assert_eq!(n, payload.len());
    assert_eq!(&out[..n], payload.as_slice());
}

#[cfg(feature = "lsm")]
#[test]
fn frame_emit_info_describes_emitted_block_layout() {
    // Encode a payload large enough to force >1 block, fetch
    // FrameEmitInfo, walk blocks[] and verify each block's
    // (offset_in_frame, header_size, body_size) matches the bytes
    // actually emitted into the drain buffer.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    // Content checksum is opt-in (library default mirrors libzstd's
    // checksum-off); request it so the checksum_range assertion below
    // exercises the hash-gated trailer accounting.
    compressor.set_content_checksum(true);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let info = compressor
        .last_frame_emit_info()
        .expect("last_frame_emit_info populated after compress")
        .clone();
    drop(compressor);

    // Frame header range starts at 0 and is non-empty.
    assert_eq!(info.frame_header_range.start, 0);
    assert!(info.frame_header_range.end > 0);
    // Total size matches what was written to the drain.
    assert_eq!(info.total_size as usize, compressed.len());
    // At least one block, and the last entry has last_block=true.
    assert!(!info.blocks.is_empty());
    assert!(info.blocks.last().unwrap().last_block);
    // All non-final blocks have last_block=false.
    for b in &info.blocks[..info.blocks.len() - 1] {
        assert!(!b.last_block);
    }
    // Walk and verify each block's header bytes match the
    // recorded type / size by re-decoding the 3-byte header.
    // Walking arithmetic: offset_in_frame + header_size + body_size
    // must land exactly on the next block's offset_in_frame (or,
    // for the last block, on the checksum / end of frame).
    for (i, b) in info.blocks.iter().enumerate() {
        let off = b.offset_in_frame as usize;
        assert_eq!(b.header_size, 3);
        let mut hdr = [0u8; 4];
        hdr[..3].copy_from_slice(&compressed[off..off + 3]);
        let raw = u32::from_le_bytes(hdr);
        let last = (raw & 1) != 0;
        let ty = (raw >> 1) & 0b11;
        let sz = raw >> 3;
        assert_eq!(last, b.last_block);
        assert_eq!(sz, b.block_size_field);
        // body_size is the PHYSICAL length on the wire: spec's
        // Block_Size for Raw/Compressed, always 1 for RLE.
        let expected_physical = match b.block_type {
            crate::encoding::frame_emit_info::BlockType::RLE => 1,
            _ => sz,
        };
        assert_eq!(b.body_size, expected_physical);
        let expected_ty = match b.block_type {
            crate::encoding::frame_emit_info::BlockType::Raw => 0,
            crate::encoding::frame_emit_info::BlockType::RLE => 1,
            crate::encoding::frame_emit_info::BlockType::Compressed => 2,
            crate::encoding::frame_emit_info::BlockType::Reserved => 3,
        };
        assert_eq!(ty, expected_ty);
        // Walking-arithmetic invariant.
        let next_off = b.offset_in_frame + b.header_size as u32 + b.body_size;
        if let Some(next) = info.blocks.get(i + 1) {
            assert_eq!(
                next_off, next.offset_in_frame,
                "block {i} body_size doesn't reach next block's offset_in_frame",
            );
        } else if let Some(cs) = info.checksum_range.as_ref() {
            assert_eq!(
                next_off, cs.start,
                "last block body_size doesn't reach checksum_range.start",
            );
        } else {
            assert_eq!(
                next_off, info.total_size,
                "last block body_size doesn't reach total_size",
            );
        }
    }
    // Checksum range present iff `feature = "hash"` is enabled.
    assert_eq!(info.checksum_range.is_some(), cfg!(feature = "hash"));
}

#[cfg(all(feature = "lsm", feature = "hash"))]
#[test]
fn per_block_checksum_round_trip() {
    // Encode with per-block checksums enabled. Decode with
    // per-block verification. Both sides emit exactly 1
    // checksum per physical block written to / read from the
    // wire (encoder hashes per emission site, including each
    // post-split partition; decoder hashes each decoded block).
    // Cardinality and element-wise contents must match
    // round-trip.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i & 0xFF) as u8).collect();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    compressor.enable_per_block_checksums();
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let encoder_checksums = compressor
        .last_frame_block_checksums()
        .expect("checksums populated after enable + compress")
        .to_vec();
    drop(compressor);
    assert!(!encoder_checksums.is_empty());

    // Decode side: enable verification, decode, compare.
    let mut decoder = FrameDecoder::new();
    decoder.enable_per_block_checksums();
    let mut output = alloc::vec![0u8; payload.len()];
    let n = decoder
        .decode_all(compressed.as_slice(), &mut output)
        .expect("decode_all should succeed");
    assert_eq!(n, payload.len());
    assert_eq!(&output[..n], payload.as_slice());

    let decoder_checksums = decoder.computed_block_checksums();
    assert_eq!(decoder_checksums, encoder_checksums.as_slice());
}

// ── decode_blocks_partial (block-subset partial decode, lsm) ──

/// Build a multi-block compressible frame and return
/// `(compressed, full_decode, emit_info)`. The emit info's
/// `decompressed_byte_range` maps decompressed offsets to block indices.
#[cfg(feature = "lsm")]
fn multi_block_fixture() -> (
    Vec<u8>,
    Vec<u8>,
    crate::encoding::frame_emit_info::FrameEmitInfo,
) {
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

    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut compressed);
    compressor.compress();
    let info = compressor
        .last_frame_emit_info()
        .expect("emit info populated")
        .clone();
    drop(compressor);

    let mut dec = FrameDecoder::new();
    let mut full = alloc::vec![0u8; data.len()];
    let n = dec
        .decode_all(compressed.as_slice(), &mut full)
        .expect("full decode");
    full.truncate(n);
    assert_eq!(full, data, "fixture must round-trip");
    (compressed, full, info)
}

#[cfg(feature = "lsm")]
#[test]
fn decode_blocks_partial_subset_matches_full_decode() {
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    assert!(
        nblocks >= 4,
        "fixture must have several blocks, got {nblocks}"
    );
    let half = nblocks / 2;
    // Boundaries: 1 block, 2 blocks, half, all, and a non-zero start.
    // `(0, u32::MAX)` exercises the "decode to end of frame" sentinel,
    // a distinct public contract from an explicit upper bound.
    for &(s, e) in &[
        (0u32, u32::MAX),
        (0, 1),
        (0, 2),
        (0, half),
        (0, nblocks),
        (1, 2),
        (half, nblocks),
    ] {
        // The sentinel decodes through the last block; map it to nblocks
        // for the expected-slice / block-count arithmetic below.
        let effective_end = if e == u32::MAX { nblocks } else { e };
        let mut source = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut source).unwrap();
        let pd = dec
            .decode_blocks_partial(&mut source, s, e, None, false)
            .unwrap_or_else(|err| panic!("range [{s},{e}) errored: {err:?}"));

        let start = info.decompressed_byte_range(s as usize).unwrap().start as usize;
        let end = info
            .decompressed_byte_range((effective_end - 1) as usize)
            .unwrap()
            .end as usize;
        assert_eq!(
            pd.data.as_slice(),
            &full[start..end],
            "subset bytes must equal the full-decode slice for [{s},{e})"
        );
        assert_eq!(pd.start_block, s);
        assert_eq!(pd.blocks_decoded, effective_end - s);
        assert!(pd.stopped_at.is_none(), "clean range [{s},{e})");
    }
}

#[cfg(feature = "lsm")]
#[test]
fn decode_blocks_partial_recovers_clean_prefix_on_truncated_block() {
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len();
    let k = nblocks / 2;
    assert!(k >= 1, "need a clean prefix before the failing block");

    // Truncate the source right after block k's 3-byte header, so its body
    // read fails regardless of block type (0 body bytes available).
    let cut = info.blocks[k].offset_in_frame as usize + info.blocks[k].header_size as usize;
    let truncated = &compressed[..cut];

    let mut source = truncated;
    let mut dec = FrameDecoder::new();
    dec.reset(&mut source).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut source, 0, u32::MAX, None, false)
        .unwrap();

    let (idx, _err) = pd.stopped_at.expect("must stop on the truncated block");
    assert_eq!(idx, k as u32, "stopped at the truncated block index");
    assert_eq!(pd.blocks_decoded, k as u32, "blocks 0..k decoded cleanly");
    assert!(!pd.frame_finished);
    let clean_end = info.decompressed_byte_range(k).unwrap().start as usize;
    assert_eq!(
        pd.data.as_slice(),
        &full[..clean_end],
        "clean prefix preserved through the failure"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn decode_blocks_partial_invalid_range_errors() {
    let (compressed, _full, _info) = multi_block_fixture();
    let mut source = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut source).unwrap();
    let err = dec
        .decode_blocks_partial(&mut source, 5, 2, None, false)
        .expect_err("start > end must error");
    assert!(matches!(
        err,
        crate::decoding::errors::FrameDecoderError::InvalidBlockRange {
            start_block: 5,
            end_block: 2,
        }
    ));
}

#[cfg(feature = "lsm")]
#[test]
fn decode_blocks_partial_skips_trailing_blocks() {
    let (compressed, full, info) = multi_block_fixture();
    assert!(info.blocks.len() >= 3);
    let mut source = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut source).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut source, 0, 1, None, false)
        .unwrap();

    assert_eq!(pd.blocks_decoded, 1);
    assert!(pd.stopped_at.is_none());
    assert!(!pd.frame_finished, "block 0 is not the last block");
    let end = info.decompressed_byte_range(0).unwrap().end as usize;
    assert_eq!(pd.data.as_slice(), &full[..end]);
    // The trailing blocks + checksum were never consumed from the source.
    assert!(
        dec.bytes_read_from_source() < u64::from(info.total_size),
        "only block 0's region should be consumed, read {} of {}",
        dec.bytes_read_from_source(),
        info.total_size
    );
}

#[cfg(feature = "lsm")]
#[test]
fn lsm_style_range_query_partial_recovery() {
    // Simulates lsm-tree's range-query path: a key range resolves to a
    // decompressed byte window, which maps to inner zstd block indices via
    // `decompressed_byte_range`; decode only the covering blocks and check
    // the wanted window is recovered exactly (no key outside, all inside).
    let (compressed, full, info) = multi_block_fixture();
    let total = full.len() as u64;
    let want_start = total / 3;
    let want_end = (total * 2) / 3;

    // Map [want_start, want_end) to covering block indices.
    let nblocks = info.blocks.len();
    let mut start_block = 0u32;
    let mut end_block = nblocks as u32;
    for i in 0..nblocks {
        let r = info.decompressed_byte_range(i).unwrap();
        if r.start <= want_start && want_start < r.end {
            start_block = i as u32;
        }
        if r.start < want_end && want_end <= r.end {
            end_block = i as u32 + 1;
            break;
        }
    }

    let mut source = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut source).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut source, start_block, end_block, None, false)
        .unwrap();
    assert!(pd.stopped_at.is_none());

    let covered_start = info
        .decompressed_byte_range(start_block as usize)
        .unwrap()
        .start;
    let covered_end = info
        .decompressed_byte_range((end_block - 1) as usize)
        .unwrap()
        .end;
    assert!(
        covered_start <= want_start && want_end <= covered_end,
        "covering blocks must contain the wanted window"
    );
    assert_eq!(
        pd.data.as_slice(),
        &full[covered_start as usize..covered_end as usize],
        "covered subset must equal the full-decode slice"
    );
    // Slice the exact key range out of the covered subset.
    let off = (want_start - covered_start) as usize;
    let len = (want_end - want_start) as usize;
    assert_eq!(
        &pd.data[off..off + len],
        &full[want_start as usize..want_end as usize],
        "exact key range recovered from the partial decode"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn decode_blocks_partial_leaves_no_residual_when_no_in_range_block() {
    // Regression: when the requested range reaches no in-range block (here
    // start_block is past EOF, so every block is decoded only as window
    // context), `PartialDecode::data` is empty — but the context bytes must
    // NOT linger in the decoder buffer, or a later collect()/read() on the
    // same decoder returns out-of-range data.
    let (compressed, _full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let mut source = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut source).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut source, nblocks + 5, u32::MAX, None, false)
        .unwrap();
    assert!(pd.data.is_empty(), "no in-range block → empty data");
    assert_eq!(pd.blocks_decoded, 0);
    assert!(
        pd.frame_finished,
        "frame's last block was reached as context"
    );
    assert_eq!(
        dec.can_collect(),
        0,
        "context bytes must not leak via collect()/read() when data is empty"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn decode_blocks_partial_empty_range_leaves_no_residual() {
    // Companion to the start-past-EOF case: an in-frame empty range `[k, k)`
    // (k < EOF) takes the same `prefix_window_len == None` path but with
    // `frame_finished == false` and up to `window_size` context bytes still
    // physically present. Assert the buffer is fully cleared directly (a
    // `can_collect()` check alone would pass even with <= window_size bytes
    // retained, because it holds the window back).
    let (compressed, _full, info) = multi_block_fixture();
    let k = ((info.blocks.len() as u32) / 2).max(1);
    let mut source = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut source).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut source, k, k, None, false)
        .unwrap();

    assert!(pd.data.is_empty(), "empty range must yield empty data");
    assert_eq!(pd.blocks_decoded, 0);
    assert!(
        !pd.frame_finished,
        "frame should still have trailing blocks"
    );
    assert_eq!(
        dec.state.as_ref().unwrap().decoder_scratch.buffer_len(),
        0,
        "empty-range partial decode must not retain context bytes"
    );
}

#[cfg(all(feature = "lsm", feature = "hash"))]
#[test]
fn decode_blocks_partial_captures_per_block_checksums() {
    // Regression: with per-block checksums enabled, decode_blocks_partial
    // must populate computed_block_checksums just like decode_blocks /
    // decode_all — otherwise callers verifying per-block digests silently
    // lose them on the partial path.
    let (compressed, full, _info) = multi_block_fixture();

    // Reference digests via decode_blocks (the path that captures them).
    let mut ref_dec = FrameDecoder::new();
    ref_dec.enable_per_block_checksums();
    let mut rsrc = compressed.as_slice();
    ref_dec.reset(&mut rsrc).unwrap();
    while !ref_dec.is_finished() {
        ref_dec
            .decode_blocks(&mut rsrc, crate::decoding::BlockDecodingStrategy::All)
            .unwrap();
    }
    let expected = ref_dec.computed_block_checksums().to_vec();
    assert!(!expected.is_empty(), "fixture must have multiple blocks");
    let _ = full;

    // Partial decode of the whole frame must capture the same digests.
    let mut source = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.enable_per_block_checksums();
    dec.reset(&mut source).unwrap();
    let _ = dec
        .decode_blocks_partial(&mut source, 0, u32::MAX, None, false)
        .unwrap();
    assert_eq!(
        dec.computed_block_checksums(),
        expected.as_slice(),
        "partial decode must capture the same per-block checksums as full decode"
    );
}

// ── resume (window-priming + entropy cold resume, lsm) ───────────

/// Window size of `compressed`'s frame, read from a freshly-reset decoder.
#[cfg(feature = "lsm")]
fn frame_window_size(compressed: &[u8]) -> usize {
    let mut src = compressed;
    let mut dec = FrameDecoder::new();
    dec.reset(&mut src).unwrap();
    dec.state
        .as_ref()
        .unwrap()
        .frame_header
        .window_size()
        .unwrap_or(0) as usize
}

/// Build a large compressible MULTI-SEGMENT frame (window_size < content,
/// so mid-frame blocks reach back only into a bounded window) and return
/// `(compressed, full_decode, emit_info)`.
#[cfg(feature = "lsm")]
fn multi_segment_block_fixture() -> (
    Vec<u8>,
    Vec<u8>,
    crate::encoding::frame_emit_info::FrameEmitInfo,
) {
    // ~3 MiB of compressible (runs + repeated phrase) data — large enough
    // that the encoder picks window_size < content_size (multi-segment).
    let mut data: Vec<u8> = Vec::with_capacity(3 * 1024 * 1024);
    let mut x = 0x9E37_79B9u32;
    while data.len() < 3 * 1024 * 1024 {
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

    let mut compressed = Vec::new();
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(data.as_slice());
    compressor.set_drain(&mut compressed);
    compressor.compress();
    let info = compressor
        .last_frame_emit_info()
        .expect("emit info populated")
        .clone();
    drop(compressor);

    // Confirm the precondition: the frame must be multi-segment.
    let mut sanity = FrameDecoder::new();
    sanity.init(&mut compressed.as_slice()).unwrap();
    assert!(
        !sanity
            .state
            .as_ref()
            .unwrap()
            .frame_header
            .descriptor
            .single_segment_flag(),
        "fixture precondition: frame must be multi-segment (resize if encoder default changed)"
    );

    let mut dec = FrameDecoder::new();
    let mut full = alloc::vec![0u8; data.len()];
    let n = dec
        .decode_all(compressed.as_slice(), &mut full)
        .expect("full decode");
    full.truncate(n);
    assert_eq!(full, data, "fixture must round-trip");
    (compressed, full, info)
}

/// Emit a [`ResumeState`] for resuming at block `n` by decoding `[0, n)` on
/// a throwaway decoder with `emit_resume = true`.
#[cfg(feature = "lsm")]
fn emit_resume_state_at(compressed: &[u8], n: u32) -> super::ResumeState {
    let mut src = compressed;
    let mut dec = FrameDecoder::new();
    dec.reset(&mut src).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut src, 0, n, None, true)
        .expect("prefix decode for resume-state emission");
    pd.resume_state
        .expect("emit_resume should populate resume_state")
}

#[cfg(feature = "lsm")]
#[test]
fn resume_matches_full_decode_at_first_mid_last() {
    // Acceptance criterion: after resuming at block N (cold decoder, primed
    // window + restored entropy), decode_blocks_partial yields bytes
    // byte-identical to a full decode's [ends[N-1]..ends[end-1]) slice, for
    // N in {1, mid, last}. Repeat_Mode entropy blocks are covered because
    // the emitted ResumeState carries the carry-over tables.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    assert!(nblocks >= 4, "need several blocks, got {nblocks}");

    for &n in &[1u32, nblocks / 2, nblocks - 1] {
        // Producer: emit resume state for block n (separate decoder).
        let st = emit_resume_state_at(&compressed, n);
        assert_eq!(st.block_index(), n);
        let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;
        assert_eq!(st.output_offset(), output_offset);

        // Consumer: a FRESH (cold) decoder resumes at n. Pass the WHOLE
        // decompressed prefix as window_prime; it is capped to one window
        // internally, exercising the cap path.
        let window_prime = &full[..output_offset as usize];
        let mut header_src = compressed.as_slice();
        let mut dec = FrameDecoder::new();
        dec.reset(&mut header_src).unwrap();
        // Caller positions the source at block n's compressed frame offset.
        let off = info.blocks[n as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];
        let pd = dec
            .decode_blocks_partial(
                &mut block_src,
                n,
                u32::MAX,
                Some(super::ResumeInput {
                    window_prime,
                    state: &st,
                }),
                false,
            )
            .unwrap_or_else(|e| panic!("resume decode at N={n} errored: {e:?}"));

        let start = output_offset as usize;
        let end = info
            .decompressed_byte_range((nblocks - 1) as usize)
            .unwrap()
            .end as usize;
        assert_eq!(
            pd.data.as_slice(),
            &full[start..end],
            "resumed bytes must equal the full-decode slice for N={n}"
        );
        assert_eq!(pd.start_block, n);
        assert_eq!(pd.blocks_decoded, nblocks - n);
        assert!(pd.stopped_at.is_none(), "clean resume at N={n}");
        assert!(pd.frame_finished, "decoded through the last block");
    }
}

#[cfg(feature = "lsm")]
#[test]
fn resume_with_exact_window_tail_matches_full_decode() {
    // Realistic cold-resume shape on a MULTI-SEGMENT frame: caller supplies
    // only the last `window_size` decompressed bytes (not the whole prefix),
    // which is all that can ever back a match.
    let (compressed, full, info) = multi_segment_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let window_size = frame_window_size(&compressed);
    // First block whose preceding output exceeds one window, so the tail
    // genuinely truncates the prefix.
    let n = (1..nblocks)
        .find(|&i| info.decompressed_byte_range(i as usize).unwrap().start as usize > window_size)
        .expect("multi-segment frame must have a block past one window");
    let st = emit_resume_state_at(&compressed, n);
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;
    assert!(output_offset as usize > window_size);
    let tail_start = output_offset as usize - window_size;
    let window_prime = &full[tail_start..output_offset as usize];

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut header_src).unwrap();
    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    let pd = dec
        .decode_blocks_partial(
            &mut block_src,
            n,
            u32::MAX,
            Some(super::ResumeInput {
                window_prime,
                state: &st,
            }),
            false,
        )
        .unwrap();

    let end = info
        .decompressed_byte_range((nblocks - 1) as usize)
        .unwrap()
        .end as usize;
    assert_eq!(pd.data.as_slice(), &full[output_offset as usize..end]);
    assert_eq!(pd.blocks_decoded, nblocks - n);
}

#[cfg(feature = "lsm")]
#[test]
fn resume_rejects_short_window_prime() {
    // Acceptance criterion: a window_prime shorter than the required window
    // is rejected with a typed error, not a silent mis-decode.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let window_size = frame_window_size(&compressed);
    let n = nblocks / 2;
    let st = emit_resume_state_at(&compressed, n);
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;
    let required = core::cmp::min(window_size as u64, output_offset) as usize;
    assert!(required > 0, "mid block must require a non-empty window");

    // One byte short of the required window.
    let prime = &full[output_offset as usize - (required - 1)..output_offset as usize];

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut header_src).unwrap();
    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    let err = dec
        .decode_blocks_partial(
            &mut block_src,
            n,
            u32::MAX,
            Some(super::ResumeInput {
                window_prime: prime,
                state: &st,
            }),
            false,
        )
        .expect_err("short window_prime must be rejected");
    match err {
        crate::decoding::errors::FrameDecoderError::ResumeWindowTooShort { got, need } => {
            assert_eq!(got, required - 1);
            assert_eq!(need, required);
        }
        other => panic!("expected ResumeWindowTooShort, got {other:?}"),
    }
}

#[cfg(feature = "lsm")]
#[test]
fn resume_range_validates_against_effective_start_not_start_block() {
    // In resume mode `start_block` is ignored and decoding begins at
    // `state.block_index()`. The range guard must therefore validate the
    // EFFECTIVE start against `end_block`: `end_block` below the resume
    // block is an inverted range and must error, not silently return an
    // empty decode. Caller passes the conventional ignored `start_block = 0`.
    let (compressed, _full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let n = (nblocks / 2).max(2);
    let st = emit_resume_state_at(&compressed, n);
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut header_src).unwrap();
    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    // end_block = n - 1 is below the resume block n → inverted range.
    let err = dec
        .decode_blocks_partial(
            &mut block_src,
            0,
            n - 1,
            Some(super::ResumeInput {
                window_prime: &_full[..output_offset as usize],
                state: &st,
            }),
            false,
        )
        .expect_err("end_block below the resume block must be an inverted range");
    match err {
        crate::decoding::errors::FrameDecoderError::InvalidBlockRange {
            start_block,
            end_block,
        } => {
            assert_eq!(start_block, n, "error must report the effective start");
            assert_eq!(end_block, n - 1);
        }
        other => panic!("expected InvalidBlockRange, got {other:?}"),
    }
}

#[cfg(feature = "lsm")]
#[test]
fn resume_rejects_state_from_a_different_frame() {
    // A ResumeState captured from one frame must not be applied to a frame
    // with a different decode shape (window size / single-segment / dict):
    // restoring foreign entropy tables would yield byte-wrong output. The
    // frame-identity guard must reject it up front with a typed error.
    let (frame_a, _full_a, info_a) = multi_block_fixture();
    let (frame_b, full_b, _info_b) = multi_segment_block_fixture();
    // Sanity: the two fixtures must differ in decode shape for the guard to
    // be exercised (single-segment vs multi-segment here).
    let st = emit_resume_state_at(&frame_a, (info_a.blocks.len() as u32 / 2).max(1));

    let mut header_src = frame_b.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut header_src).unwrap();
    // The frame-key check runs before the window-length check, so even a
    // valid-length window_prime for frame B is rejected on identity.
    let err = dec
        .decode_blocks_partial(
            &mut frame_b.as_slice(),
            st.block_index(),
            u32::MAX,
            Some(super::ResumeInput {
                window_prime: &full_b,
                state: &st,
            }),
            false,
        )
        .expect_err("resume state from a different frame must be rejected");
    assert!(
        matches!(
            err,
            crate::decoding::errors::FrameDecoderError::ResumeFrameMismatch
        ),
        "expected ResumeFrameMismatch, got {err:?}"
    );
}

#[cfg(all(feature = "lsm", feature = "hash"))]
#[test]
fn resume_rejects_wrong_window_prime_content() {
    // Same frame (FrameKey matches) but the caller supplies a window_prime
    // with one byte flipped. The shape key cannot catch this; the
    // content-exact XXH64 of the window must, rejecting before any restore
    // rather than mis-resolving matches against corrupted history.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let n = (nblocks / 2).max(1);
    let st = emit_resume_state_at(&compressed, n);
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start as usize;
    assert!(output_offset > 0);

    // Correct prefix with the last byte corrupted (this byte is inside the
    // window the resume block reaches back into).
    let mut corrupted = full[..output_offset].to_vec();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xFF;

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut header_src).unwrap();
    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    let err = dec
        .decode_blocks_partial(
            &mut block_src,
            n,
            u32::MAX,
            Some(super::ResumeInput {
                window_prime: &corrupted,
                state: &st,
            }),
            false,
        )
        .expect_err("corrupted window_prime must be rejected by content hash");
    assert!(
        matches!(
            err,
            crate::decoding::errors::FrameDecoderError::ResumeFrameMismatch
        ),
        "expected ResumeFrameMismatch, got {err:?}"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn resume_rejects_state_with_different_active_dictionary() {
    // A dictless-header frame can be decoded with an explicit dictionary
    // applied at runtime (force_dict / reset_with_dict_handle). Two such
    // decodes differ in entropy/repcode/dict context even though the header
    // dictionary_id is identically absent, so the resume guard must key on
    // the ACTIVE dictionary, not just the header field. Here the snapshot is
    // captured with no active dictionary; resuming with one applied must be
    // rejected before any state is restored.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let n = (nblocks / 2).max(1);
    let st = emit_resume_state_at(&compressed, n); // active_dictionary_id = None
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start as usize;

    let raw = include_bytes!("../../../dict_tests/dictionary");
    let dict = crate::decoding::dictionary::Dictionary::decode_dict(raw).expect("parse dict");
    let dict_id = dict.id;

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.add_dict(dict).unwrap();
    dec.reset(&mut header_src).unwrap();
    dec.force_dict(dict_id).unwrap(); // active_dictionary_id = Some(dict_id)
    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    let err = dec
        .decode_blocks_partial(
            &mut block_src,
            n,
            u32::MAX,
            Some(super::ResumeInput {
                window_prime: &full[..output_offset],
                state: &st,
            }),
            false,
        )
        .expect_err("resume with a different active dictionary must be rejected");
    assert!(
        matches!(
            err,
            crate::decoding::errors::FrameDecoderError::ResumeFrameMismatch
        ),
        "expected ResumeFrameMismatch, got {err:?}"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn resume_invalid_range_does_not_mutate_decoder_state() {
    // An inverted effective range must be rejected WITHOUT priming the
    // decoder: no entropy restore, no window prime, no cursor advance. As
    // written before the fix, those mutations ran before the range check,
    // leaving the decoder in a synthetic resumed state on the error path.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let n = (nblocks / 2).max(2);
    let st = emit_resume_state_at(&compressed, n);
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start as usize;

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut header_src).unwrap();
    // Freshly reset: cursor at block 0.
    assert_eq!(dec.state.as_ref().unwrap().block_counter, 0);

    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    let err = dec
        .decode_blocks_partial(
            &mut block_src,
            0,
            n - 1, // below the resume block → inverted range
            Some(super::ResumeInput {
                window_prime: &full[..output_offset],
                state: &st,
            }),
            false,
        )
        .expect_err("inverted range must error");
    assert!(matches!(
        err,
        crate::decoding::errors::FrameDecoderError::InvalidBlockRange { .. }
    ));
    assert_eq!(
        dec.state.as_ref().unwrap().block_counter,
        0,
        "error path must not advance the cursor (validate before priming)"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn emit_resume_state_absent_on_terminal_block() {
    // When a decode reaches the frame's last block there is no "next block"
    // to resume at: the snapshot's block_index would be one past EOF and the
    // caller has no offset_in_frame for it. emit_resume must therefore yield
    // None on the terminal block, not a dangling snapshot.
    let (compressed, _full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let mut src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut src).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut src, 0, nblocks, None, true)
        .unwrap();
    assert!(pd.frame_finished, "decode must reach the last block");
    assert!(
        pd.resume_state.is_none(),
        "no resume state past the frame's last block"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn emit_resume_state_absent_when_not_requested() {
    // Default partial decode (emit_resume = false) must NOT pay the entropy
    // clone: resume_state stays None.
    let (compressed, _full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let mut src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.reset(&mut src).unwrap();
    let pd = dec
        .decode_blocks_partial(&mut src, 0, nblocks, None, false)
        .unwrap();
    assert!(
        pd.resume_state.is_none(),
        "resume_state must be None unless emit_resume is set"
    );
}

#[cfg(feature = "lsm")]
#[test]
fn resume_grow_loop_reconstructs_full() {
    // The motivating scenario: a symmetric one-call grow-loop. Each call
    // takes the previous ResumeState and emits the next, decoding only the
    // new extent — concatenated, the extents reconstruct the full output
    // with no prefix ever re-decompressed.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    assert!(nblocks >= 4);

    // Walk the frame in extents of `step` blocks each.
    let step = (nblocks / 3).max(1);
    let mut combined: Vec<u8> = Vec::new();
    let mut next: u32 = 0;
    let mut carry: Option<super::ResumeState> = None;

    while next < nblocks {
        let end = (next + step).min(nblocks);
        let mut dec = FrameDecoder::new();
        let mut header_src = compressed.as_slice();
        dec.reset(&mut header_src).unwrap();

        let off = info.blocks[next as usize].offset_in_frame as usize;
        let mut block_src = &compressed[off..];

        let output_offset = info.decompressed_byte_range(next as usize).unwrap().start;
        let pd = if let Some(st) = carry.as_ref() {
            // Resume from the prior extent's state (cold: fresh decoder).
            let window_prime = &full[..output_offset as usize];
            dec.decode_blocks_partial(
                &mut block_src,
                next,
                end,
                Some(super::ResumeInput {
                    window_prime,
                    state: st,
                }),
                true,
            )
            .unwrap()
        } else {
            // First extent: no resume input, just emit for the next.
            dec.decode_blocks_partial(&mut block_src, next, end, None, true)
                .unwrap()
        };

        combined.extend_from_slice(&pd.data);
        carry = pd.resume_state;
        next = end;
    }

    assert_eq!(
        combined, full,
        "grow-loop extents must reconstruct the full output"
    );
}

#[cfg(all(feature = "lsm", feature = "hash"))]
#[test]
fn resume_does_not_redecode_prefix_blocks() {
    // Instrumented confirmation that blocks < N are not re-decoded on
    // resume. With per-block checksums enabled on the resuming decoder, the
    // resumed decode must record exactly one digest per in-range block
    // (end - N), never one per frame block.
    let (compressed, full, info) = multi_block_fixture();
    let nblocks = info.blocks.len() as u32;
    let n = nblocks / 2;
    let st = emit_resume_state_at(&compressed, n);
    let output_offset = info.decompressed_byte_range(n as usize).unwrap().start;

    let mut header_src = compressed.as_slice();
    let mut dec = FrameDecoder::new();
    dec.enable_per_block_checksums();
    dec.reset(&mut header_src).unwrap();
    let off = info.blocks[n as usize].offset_in_frame as usize;
    let mut block_src = &compressed[off..];
    let _ = dec
        .decode_blocks_partial(
            &mut block_src,
            n,
            u32::MAX,
            Some(super::ResumeInput {
                window_prime: &full[..output_offset as usize],
                state: &st,
            }),
            false,
        )
        .unwrap();

    assert_eq!(
        dec.computed_block_checksums().len() as u32,
        nblocks - n,
        "resume must decode only in-range blocks, not re-decode the prefix"
    );
}
