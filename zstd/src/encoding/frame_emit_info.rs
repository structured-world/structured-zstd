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
//! upstream zstd `libzstd` v1.5.7.
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
/// body_size`. The arithmetic
/// `offset_in_frame + header_size as u32 + body_size`
/// is the byte offset of the next block (or, on the last block, of
/// the trailing checksum / end of frame).
///
/// For RLE blocks the `body_size` is `1` (the single repeated byte
/// on the wire); the spec's `Block_Size` field carries the logical
/// repeat count instead and is surfaced separately as
/// [`block_size_field`](Self::block_size_field).
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
    /// Physical length of this block's body in bytes on the wire (does
    /// NOT include `header_size`). For Raw / Compressed blocks this is
    /// the number of bytes after the header; for RLE blocks this is
    /// always `1` (the repeated byte itself, while the spec's
    /// `Block_Size` field encodes the logical repeat count — see
    /// [`block_size_field`](Self::block_size_field)). The arithmetic
    /// `offset_in_frame + header_size as u32 + body_size` always
    /// lands on the next block boundary.
    pub body_size: u32,
    /// Raw `Block_Size` value from the 3-byte `Block_Header`. For Raw
    /// and Compressed blocks this equals `body_size`; for RLE blocks
    /// it's the logical repeat count (how many bytes the single
    /// physical body byte expands to during decode) and will differ
    /// from `body_size` (which is `1`).
    pub block_size_field: u32,
    /// Whether the block is Raw, RLE, or Compressed per RFC 8878
    /// §3.1.1.2.1 (`Block_Type`).
    pub block_type: BlockType,
    /// `true` only on the final block of the frame (matches the
    /// `Last_Block` flag in `Block_Header`).
    pub last_block: bool,
    /// Decompressed (regenerated) size of this block's output in bytes.
    ///
    /// For Raw and RLE blocks this is recoverable from the wire
    /// (`block_size_field`), but a Compressed block's regenerated size is
    /// NOT in its `Block_Header` (the header's `Block_Size` is the
    /// *compressed* length), so the encoder captures it from the input
    /// chunk that produced the block. Consumers map a decompressed byte
    /// offset to a block index via the prefix sum of this field; see
    /// [`FrameEmitInfo::decompressed_byte_range`].
    ///
    /// On the decode error path ([`FailedToReadBlockBodyAt`]), where the
    /// regenerated size of a failed Compressed block is unknown, this is
    /// `0` for Compressed blocks (Raw/RLE still carry their wire size).
    ///
    /// [`FailedToReadBlockBodyAt`]: crate::decoding::errors::FrameDecoderError::FailedToReadBlockBodyAt
    pub decompressed_size: u32,
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

impl FrameEmitInfo {
    /// Half-open decompressed byte range `[start, end)` of `blocks[block_index]`
    /// within the frame's full decompressed output, computed as the prefix
    /// sum of every preceding block's [`FrameBlock::decompressed_size`].
    ///
    /// This is the mapping a range-query consumer uses to turn a
    /// decompressed byte offset into the inner-block index needed by
    /// [`FrameDecoder::decode_blocks_partial`]: find the first block whose
    /// range contains the offset.
    ///
    /// Returns `None` if `block_index` is out of bounds.
    ///
    /// [`FrameDecoder::decode_blocks_partial`]: crate::decoding::FrameDecoder::decode_blocks_partial
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "lsm")] {
    /// use structured_zstd::encoding::frame_emit_info::{FrameBlock, FrameEmitInfo, BlockType};
    /// let info = FrameEmitInfo {
    ///     frame_header_range: 0..6,
    ///     blocks: vec![
    ///         FrameBlock { offset_in_frame: 6, header_size: 3, body_size: 10,
    ///             block_size_field: 10, block_type: BlockType::Compressed,
    ///             last_block: false, decompressed_size: 100 },
    ///         FrameBlock { offset_in_frame: 19, header_size: 3, body_size: 20,
    ///             block_size_field: 20, block_type: BlockType::Compressed,
    ///             last_block: true, decompressed_size: 40 },
    ///     ],
    ///     checksum_range: None,
    ///     total_size: 42,
    /// };
    /// assert_eq!(info.decompressed_byte_range(0), Some(0..100));
    /// assert_eq!(info.decompressed_byte_range(1), Some(100..140));
    /// assert_eq!(info.decompressed_byte_range(2), None);
    /// # }
    /// ```
    pub fn decompressed_byte_range(&self, block_index: usize) -> Option<core::ops::Range<u64>> {
        let target = self.blocks.get(block_index)?;
        // Prefix sum over preceding blocks. Block count is bounded by the
        // frame's block count (each block is >= 3 wire bytes), so the
        // accumulator stays well within u64.
        let start: u64 = self.blocks[..block_index]
            .iter()
            .map(|b| u64::from(b.decompressed_size))
            .sum();
        Some(start..start + u64::from(target.decompressed_size))
    }
}
