//! Structural metadata describing the layout of an emitted zstd frame.
//!
//! Surfaced via [`FrameCompressor::last_frame_emit_info`] (encode side)
//! after every successful `compress()`. Lets storage-format consumers
//! discover where each Block_Header / block body / optional content
//! checksum lands in the byte buffer without re-parsing the frame
//! themselves.
//!
//! Gated behind the `lsm` Cargo feature (default off) — the
//! `FrameCompressor` field that stores this info, the methods that
//! return it, and these public types only exist when the feature is
//! enabled. Without `lsm` the C FFI surface stays strict drop-in for
//! donor `libzstd` v1.5.7.
//!
//! [`FrameCompressor::last_frame_emit_info`]: super::FrameCompressor::last_frame_emit_info

extern crate alloc;

use alloc::vec::Vec;

pub use crate::blocks::block::BlockType;

/// Layout of a single zstd block inside an emitted frame.
///
/// Offsets are absolute byte positions in the emitted-frame buffer:
/// `offset_in_frame` points at the first byte of the 3-byte
/// `Block_Header`, and the block body lives at
/// `offset_in_frame + header_size .. offset_in_frame + header_size +
/// body_size`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBlock {
    /// Byte offset of this block's `Block_Header` within the emitted
    /// frame buffer (frame-absolute, includes the bytes consumed by
    /// the frame header / magic / FCS that precede the first block).
    pub offset_in_frame: u32,
    /// Size of the `Block_Header` in bytes. Always `3` today; carried
    /// as a field so the API stays forward-compatible with any future
    /// spec extension that widens the header.
    pub header_size: u8,
    /// Length of this block's body in bytes (does NOT include
    /// `header_size`). For Raw / Compressed blocks this is the
    /// emitted bytes after the header; for RLE blocks this is `1`
    /// (the repeated byte itself).
    pub body_size: u32,
    /// Whether the block is Raw, RLE, or Compressed per RFC 8878
    /// §3.1.1.2.1 (`Block_Type`).
    pub block_type: BlockType,
    /// `true` only on the final block of the frame (matches the
    /// `Last_Block` flag in `Block_Header`).
    pub last_block: bool,
}

/// Complete layout of an emitted zstd frame.
///
/// Captures the byte positions of the frame header, every block, and
/// the optional trailing content checksum. The ranges are `u32` byte
/// offsets into the emitted buffer (`compressed_data` sink of
/// [`FrameCompressor`]).
///
/// [`FrameCompressor`]: super::FrameCompressor
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEmitInfo {
    /// Byte range of the frame header (magic number + frame-header
    /// fields). For magicless frames the magic is omitted but the
    /// range still starts at offset 0.
    pub frame_header_range: core::ops::Range<u32>,
    /// One entry per emitted block, in stream order. The last entry
    /// has `last_block = true`.
    pub blocks: Vec<FrameBlock>,
    /// Byte range of the trailing 4-byte content checksum (XXH64
    /// truncated to low 32 bits). `None` if the frame was emitted
    /// without `content_checksum`.
    pub checksum_range: Option<core::ops::Range<u32>>,
    /// Total emitted frame size in bytes (one past the last byte of
    /// the frame).
    pub total_size: u32,
}
