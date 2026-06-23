use super::{
    FrameContentSize, FrameSizeError, find_frame_compressed_size, frame_decompressed_bound,
    frame_header_size, read_frame_content_size, read_frame_header_info,
};
use crate::encoding::{CompressionLevel, compress_slice_to_vec};
use alloc::vec;
use alloc::vec::Vec;

fn frame(content: &[u8]) -> Vec<u8> {
    compress_slice_to_vec(content, CompressionLevel::Default)
}

/// A hand-built single raw-block frame that omits `Frame_Content_Size`
/// (descriptor `0x00`: FCS_Flag=0, Single_Segment=0) so it carries a
/// Window_Descriptor instead — the only way to exercise the window-size
/// fallback in [`frame_decompressed_bound`] / [`read_frame_header_info`],
/// which the encoder (always declaring FCS) never produces.
fn no_fcs_frame() -> Vec<u8> {
    vec![
        0x28, 0xB5, 0x2F, 0xFD, // magic
        0x00, // frame header descriptor: no FCS, multi-segment, no checksum
        0x00, // window descriptor -> windowLog 10 -> 1024 bytes
        0x19, 0x00, 0x00, // block header: last, raw, size 3
        0xAA, 0xBB, 0xCC, // raw payload
    ]
}

/// As [`no_fcs_frame`] but with the Content_Checksum_flag (descriptor bit 2)
/// set and a 4-byte trailer appended, to cover the checksum-trailer branch.
fn no_fcs_checksum_frame() -> Vec<u8> {
    vec![
        0x28, 0xB5, 0x2F, 0xFD, 0x04, // descriptor: checksum flag set
        0x00, 0x19, 0x00, 0x00, 0xAA, 0xBB, 0xCC, // window + block + payload
        0xDE, 0xAD, 0xBE, 0xEF, // content checksum trailer
    ]
}

/// A skippable frame: magic `0x184D2A50`, 4-byte length, then `length` bytes.
fn skippable_frame(payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x50, 0x2A, 0x4D, 0x18];
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

#[test]
fn read_frame_content_size_reports_declared_size() {
    let f = frame(&[42u8; 100]);
    assert_eq!(
        read_frame_content_size(&f).unwrap(),
        FrameContentSize::Known(100)
    );
}

#[test]
fn read_frame_content_size_reports_unknown_without_fcs() {
    assert_eq!(
        read_frame_content_size(&no_fcs_frame()).unwrap(),
        FrameContentSize::Unknown
    );
}

#[test]
fn read_frame_content_size_errors_on_garbage() {
    assert!(read_frame_content_size(&[0xAB; 16]).is_err());
}

#[test]
fn find_frame_compressed_size_spans_one_frame_then_the_next() {
    let first = frame(&[5u8; 256]);
    assert_eq!(find_frame_compressed_size(&first).unwrap(), first.len());

    let mut two = first.clone();
    two.extend_from_slice(&frame(&[9u8; 50]));
    // Still reports only the first frame so a caller can step forward.
    assert_eq!(find_frame_compressed_size(&two).unwrap(), first.len());
}

#[test]
fn find_frame_compressed_size_measures_skippable_frame() {
    let skip = skippable_frame(&[1, 2, 3, 4]);
    assert_eq!(find_frame_compressed_size(&skip).unwrap(), skip.len());
}

#[test]
fn find_frame_compressed_size_rejects_truncation() {
    let f = frame(&[7u8; 512]);
    // Drop the trailing block bytes mid-frame.
    let err = find_frame_compressed_size(&f[..f.len() - 4]).unwrap_err();
    assert!(matches!(err, FrameSizeError::Truncated));
}

#[test]
fn frame_header_size_matches_first_block_offset() {
    let f = frame(&[3u8; 2048]);
    let hdr = frame_header_size(&f).unwrap();
    assert!((5..=18).contains(&hdr));
    assert!(frame_header_size(&[0u8; 2]).is_err());
}

#[test]
fn read_frame_header_info_fills_declared_fields() {
    let f = frame(&[7u8; 512]);
    let info = read_frame_header_info(&f, false).unwrap();
    assert_eq!(info.content_size, FrameContentSize::Known(512));
    assert!(info.window_size >= 512);
    assert_eq!(info.dictionary_id, None);
}

#[test]
fn read_frame_header_info_derives_window_without_fcs() {
    let info = read_frame_header_info(&no_fcs_frame(), false).unwrap();
    assert_eq!(info.content_size, FrameContentSize::Unknown);
    assert_eq!(info.window_size, 1024);
}

#[test]
fn frame_decompressed_bound_returns_declared_size() {
    let f = frame(&[4u8; 4096]);
    assert_eq!(frame_decompressed_bound(&f).unwrap(), 4096);
}

#[test]
fn frame_decompressed_bound_uses_block_bound_without_fcs() {
    // No declared FCS -> block_count(1) * block_size_max(min(1024,128K)).
    assert_eq!(frame_decompressed_bound(&no_fcs_frame()).unwrap(), 1024);
}

#[test]
fn frame_decompressed_bound_accepts_present_checksum_trailer() {
    assert_eq!(
        frame_decompressed_bound(&no_fcs_checksum_frame()).unwrap(),
        1024
    );
}

#[test]
fn frame_decompressed_bound_rejects_missing_checksum_trailer() {
    let mut f = no_fcs_checksum_frame();
    f.truncate(f.len() - 4); // drop the 4-byte trailer the descriptor promises
    assert!(matches!(
        frame_decompressed_bound(&f).unwrap_err(),
        FrameSizeError::Truncated
    ));
}

/// A block header may declare a `Block_Size` larger than the frame's
/// `Block_Maximum_Size` (`min(window, 128 KiB)`). RFC 8878 forbids this;
/// accepting it lets a corrupt frame pass the size query and makes the
/// no-FCS decompressed bound under-count (the raw block regenerates more
/// bytes than `block_count * block_size_max`). Both helpers must reject it.
#[test]
fn size_helpers_reject_oversized_block_header() {
    // Window 1024 (WD 0x00) -> Block_Maximum_Size = 1024. Declare a raw
    // block of Block_Size 2000 with all 2000 payload bytes present, so the
    // failure is the oversized declaration, not truncation.
    let block_size = 2000usize;
    let raw = ((block_size as u32) << 3) | 1; // last_block flag, Raw type (00)
    let mut f = vec![
        0x28,
        0xB5,
        0x2F,
        0xFD, // magic
        0x00, // descriptor: no FCS, multi-segment, no checksum
        0x00, // window descriptor -> 1024 bytes
        (raw & 0xFF) as u8,
        ((raw >> 8) & 0xFF) as u8,
        ((raw >> 16) & 0xFF) as u8,
    ];
    f.resize(f.len() + block_size, 0xAB);

    assert!(matches!(
        find_frame_compressed_size(&f).unwrap_err(),
        FrameSizeError::OversizedBlock
    ));
    assert!(matches!(
        frame_decompressed_bound(&f).unwrap_err(),
        FrameSizeError::OversizedBlock
    ));
}

#[test]
fn frame_decompressed_bound_handles_skippable_frame() {
    assert_eq!(
        frame_decompressed_bound(&skippable_frame(&[0u8; 8])).unwrap(),
        0
    );
    // A skippable frame whose advertised payload is absent is truncation.
    let mut short = skippable_frame(&[0u8; 8]);
    short.truncate(short.len() - 2);
    assert!(matches!(
        frame_decompressed_bound(&short).unwrap_err(),
        FrameSizeError::Truncated
    ));
}

#[test]
fn frame_decompressed_bound_errors_on_garbage_header() {
    assert!(frame_decompressed_bound(&[0xAB; 16]).is_err());
}
