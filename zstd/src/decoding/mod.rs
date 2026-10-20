//! RFC 8878 Zstandard decoder.
//!
//! The entry points:
//!
//! * [`StreamingDecoder`] — implements [`crate::io::Read`] over a compressed
//!   byte stream, transparently parsing the frame header and concatenated
//!   frames. The typical choice for application code.
//! * [`FrameDecoder`] — single-frame interface; use when the caller manages
//!   the input buffer manually (zero-copy slices, network framing, etc).
//! * `workspace::WorkspaceDecoder` (feature `caller-workspace`): streaming
//!   decoder over a byte slice the caller sizes and owns, with no allocation
//!   and limits on window, output and work. For consumers that account their
//!   memory themselves, such as browser bundles verifying a download.
//! * [`DictionaryHandle`] — pre-parsed dictionary handle. Parse the
//!   dictionary bytes once with [`DictionaryHandle::decode_dict`] and reuse
//!   the handle across every subsequent decode; saves the per-frame
//!   dictionary parse cost when the same dictionary is used many times in a
//!   row.
//!
//! Both decoders expose dictionary-aware constructors / methods,
//! though the exact naming differs:
//!
//! * [`StreamingDecoder::new_with_dictionary_handle`] /
//!   [`StreamingDecoder::new_with_dictionary_bytes`]
//! * [`FrameDecoder::decode_all_with_dict_handle`] /
//!   [`FrameDecoder::decode_all_with_dict_bytes`]
//!
//! The `_handle` variants reuse a previously parsed
//! [`DictionaryHandle`]; the `_bytes` variants parse the dictionary
//! per call (suitable for one-off decodes).
//!
//! Errors surface through [`errors::FrameDecoderError`] and the per-decoder
//! error types in the [`errors`] submodule.

pub mod errors;
mod frame_decoder;
mod streaming_decoder;
#[cfg(feature = "caller-workspace")]
#[cfg_attr(docsrs, doc(cfg(feature = "caller-workspace")))]
pub mod workspace;

pub use dictionary::{Dictionary, DictionaryHandle, MAGIC_NUM as DICTIONARY_MAGIC};
pub use frame_decoder::{BlockDecodingStrategy, ContentChecksum, FrameDecoder};
#[cfg(feature = "legacy")]
#[cfg_attr(docsrs, doc(cfg(feature = "legacy")))]
pub use legacy::LegacyVersion;

/// Largest window a frame may declare before a decoder that holds the window
/// itself refuses it, unless [`FrameDecoder::set_max_window_size`] raised the
/// ceiling.
///
/// The bound is what makes decoding untrusted input safe: a frame header can
/// ask for a window far larger than the data behind it, and honouring that
/// would let a few bytes of input demand gigabytes of memory.
pub use crate::common::MAXIMUM_ALLOWED_WINDOW_SIZE;

/// Largest window any decode accepts, the ceiling
/// [`FrameDecoder::set_max_window_size`] can be raised to.
pub use crate::common::MAX_DECODER_WINDOW_SIZE;
#[cfg(feature = "lsm")]
pub use frame_decoder::{PartialDecode, ResumeInput, ResumeState};
pub use streaming_decoder::{DictionaryRefused, FinishedFrame, StreamingDecoder};

/// Decompressed size a frame declares in its header, as read by
/// [`read_frame_content_size`] without decoding the frame body.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameContentSize {
    /// The header carried an explicit `Frame_Content_Size` field (in bytes).
    Known(u64),
    /// The header did not declare a content size; the true size is only
    /// known after decoding (or from out-of-band knowledge).
    Unknown,
}

/// Read the decompressed size a frame declares in its header, without
/// decoding the frame body.
///
/// Parses only the leading frame header of `src`. Returns
/// [`FrameContentSize::Known`] when the header carries an explicit
/// `Frame_Content_Size`, or [`FrameContentSize::Unknown`] when it does not.
/// This backs the C `ZSTD_getFrameContentSize` entry point, where the two
/// variants map to a concrete size and `ZSTD_CONTENTSIZE_UNKNOWN`.
///
/// With the `legacy` feature a v0.5-v0.7 frame reports the size its header
/// declares, and [`FrameContentSize::Unknown`] when it declares none or its
/// header does not read, as libzstd does: the field is advisory in those
/// formats.
///
/// # Errors
/// Returns [`ReadFrameHeaderError`](errors::ReadFrameHeaderError) when `src`
/// is too short to hold a header, carries a bad magic number, or begins with
/// a skippable frame.
///
/// ```rust
/// use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
/// use structured_zstd::decoding::{read_frame_content_size, FrameContentSize};
/// let frame = compress_slice_to_vec(&[42u8; 100], CompressionLevel::Default);
/// assert_eq!(read_frame_content_size(&frame).unwrap(), FrameContentSize::Known(100));
/// ```
pub fn read_frame_content_size(
    src: &[u8],
) -> Result<FrameContentSize, errors::ReadFrameHeaderError> {
    #[cfg(feature = "legacy")]
    if LegacyVersion::of(src).is_some() {
        return Ok(legacy::declared_content_size(src)
            .map_or(FrameContentSize::Unknown, FrameContentSize::Known));
    }
    let (header, _consumed) = frame::read_frame_header_with_format(src, false)?;
    Ok(if header.fcs_declared() {
        FrameContentSize::Known(header.frame_content_size())
    } else {
        FrameContentSize::Unknown
    })
}

/// Error from [`find_frame_compressed_size`].
#[derive(Debug)]
pub enum FrameSizeError {
    /// The frame header could not be parsed.
    Header(errors::ReadFrameHeaderError),
    /// The buffer ends before the frame's blocks (or trailing checksum) are
    /// complete.
    Truncated,
    /// A block declared the reserved block type, which is invalid per RFC 8878.
    ReservedBlock,
    /// A block declared a `Block_Size` larger than the frame's
    /// `Block_Maximum_Size` (`min(Window_Size, 128 KiB)`), which is invalid per
    /// RFC 8878 §3.1.1.2. Accepting it would let a corrupt frame pass a size
    /// query and make the no-`Frame_Content_Size` decompressed-bound
    /// under-count (each block can regenerate at most `Block_Maximum_Size`).
    OversizedBlock,
}

/// On-disk byte length of the FIRST frame in `src` — magic number, frame
/// header, every block, and the trailing content checksum when present —
/// computed by walking the block headers without decoding any block body.
///
/// For a skippable frame, returns its full `8 + Frame_Size` length, and with
/// the `legacy` feature a v0.5-v0.7 frame's length through its end block. This
/// backs the C `ZSTD_findFrameCompressedSize` entry point; the returned value
/// is the offset at which a following concatenated frame would begin.
///
/// # Errors
/// [`FrameSizeError`] when the header is unreadable, the buffer is truncated
/// mid-frame, or a block uses the reserved type.
///
/// ```rust
/// use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
/// use structured_zstd::decoding::find_frame_compressed_size;
/// let frame = compress_slice_to_vec(&[5u8; 256], CompressionLevel::Default);
/// assert_eq!(find_frame_compressed_size(&frame).unwrap(), frame.len());
/// ```
pub fn find_frame_compressed_size(src: &[u8]) -> Result<usize, FrameSizeError> {
    #[cfg(feature = "legacy")]
    if let Some(version) = LegacyVersion::of(src) {
        return legacy::frame_size(version, src)
            .map(|(size, _)| size)
            .ok_or(FrameSizeError::Truncated);
    }
    let (header, header_len) = match frame::read_frame_header_with_format(src, false) {
        Ok(parsed) => parsed,
        // Skippable frame: magic (4) + Frame_Size field (4) + payload.
        Err(errors::ReadFrameHeaderError::SkipFrame { length, .. }) => {
            return 8usize
                .checked_add(length as usize)
                .filter(|end| *end <= src.len())
                .ok_or(FrameSizeError::Truncated);
        }
        Err(e) => return Err(FrameSizeError::Header(e)),
    };

    let walk = walk_blocks(src, header_len as usize, frame_block_size_max(&header))?;
    if header.descriptor.content_checksum_flag() {
        walk.end
            .checked_add(4)
            .filter(|end| *end <= src.len())
            .ok_or(FrameSizeError::Truncated)
    } else {
        Ok(walk.end)
    }
}

/// Error from [`find_decompressed_size`].
#[derive(Debug)]
pub enum DecompressedSizeError {
    /// A frame header could not be parsed.
    Header(errors::ReadFrameHeaderError),
    /// A frame's blocks could not be walked to the start of the next one.
    Frame(FrameSizeError),
    /// The declared sizes add up past `u64::MAX`.
    Overflow,
}

/// Total decompressed size the frames in `src` declare, without decoding any
/// of them: [`FrameContentSize::Known`] with the sum, skippable frames counting
/// zero, or [`FrameContentSize::Unknown`] as soon as one frame does not declare
/// its size. Backs the C `ZSTD_findDecompressedSize` entry point.
///
/// # Errors
/// [`DecompressedSizeError`] when a frame is malformed or truncated, or the
/// sizes overflow `u64`.
///
/// ```rust
/// use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
/// use structured_zstd::decoding::{find_decompressed_size, FrameContentSize};
/// let mut frames = compress_slice_to_vec(&[1u8; 300], CompressionLevel::Default);
/// frames.extend(compress_slice_to_vec(&[2u8; 200], CompressionLevel::Default));
/// assert_eq!(find_decompressed_size(&frames).unwrap(), FrameContentSize::Known(500));
/// ```
pub fn find_decompressed_size(src: &[u8]) -> Result<FrameContentSize, DecompressedSizeError> {
    let mut rest = src;
    let mut total: u64 = 0;
    while !rest.is_empty() {
        // A v0.5-v0.7 frame's size is advisory, so a header too short to read
        // passes as unknown on its own; here a tail shorter than a frame's
        // first five bytes is an error, as upstream's `ZSTD_findDecompressedSize`
        // walks frames only while five remain.
        #[cfg(feature = "legacy")]
        if rest.len() < 5 && LegacyVersion::of(rest).is_some() {
            return Err(DecompressedSizeError::Frame(FrameSizeError::Truncated));
        }
        match read_frame_content_size(rest) {
            Ok(FrameContentSize::Known(size)) => {
                total = total
                    .checked_add(size)
                    .ok_or(DecompressedSizeError::Overflow)?;
            }
            Ok(FrameContentSize::Unknown) => return Ok(FrameContentSize::Unknown),
            // A skippable frame regenerates nothing.
            Err(errors::ReadFrameHeaderError::SkipFrame { .. }) => {}
            Err(err) => return Err(DecompressedSizeError::Header(err)),
        }
        let consumed = find_frame_compressed_size(rest).map_err(DecompressedSizeError::Frame)?;
        rest = &rest[consumed..];
    }
    Ok(FrameContentSize::Known(total))
}

/// Result of walking the block sequence of one frame (between the header and
/// the optional trailing checksum).
struct BlockWalk {
    /// Offset just past the last block (before any content checksum).
    end: usize,
    /// Number of blocks in the frame.
    count: u64,
}

/// `Block_Maximum_Size` for the frame: `min(Window_Size, 128 KiB)`. Per RFC
/// 8878 §3.1.1.2 every block's `Block_Size` is bounded by this, and each block
/// regenerates at most this many bytes. Single-segment frames omit the
/// `Window_Descriptor`; their window equals the declared content size.
fn frame_block_size_max(header: &frame::FrameHeader) -> usize {
    let window_size = match header.window_descriptor() {
        Some(desc) => {
            let exponent = u64::from(desc >> 3);
            let mantissa = u64::from(desc & 0x7);
            let window_base = 1u64 << (10 + exponent);
            window_base + (window_base / 8) * mantissa
        }
        None => header.frame_content_size(),
    };
    // The 128 KiB cap keeps the result within usize on every target.
    window_size.min(128 * 1024) as usize
}

/// Walk the block headers of a single frame starting at `start` (the offset of
/// the first block header), validating each fits in `src` and declares a
/// `Block_Size` no larger than `max_block_size` (the frame's
/// `Block_Maximum_Size`). Does not consume the trailing content checksum.
/// Shared by [`find_frame_compressed_size`] and [`frame_decompressed_bound`] so
/// the on-disk-size and block-count views never diverge.
fn walk_blocks(
    src: &[u8],
    start: usize,
    max_block_size: usize,
) -> Result<BlockWalk, FrameSizeError> {
    let mut offset = start;
    let mut count = 0u64;
    loop {
        // 3-byte block header (RFC 8878 §3.1.1.2): bit0 last-block flag,
        // bits1-2 block type, bits3-23 Block_Size.
        let hdr = src
            .get(offset..offset + 3)
            .ok_or(FrameSizeError::Truncated)?;
        let raw = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
        let last_block = (raw & 1) != 0;
        let block_type = (raw >> 1) & 0b11;
        let block_size = (raw >> 3) as usize;
        // On-disk bytes following the header: RLE stores a single byte
        // regardless of the run length; Raw/Compressed store Block_Size bytes;
        // the reserved type is invalid.
        let on_disk = match block_type {
            1 => 1,              // RLE
            0 | 2 => block_size, // Raw / Compressed
            _ => return Err(FrameSizeError::ReservedBlock),
        };
        // RFC 8878 §3.1.1.2: Block_Size MUST NOT exceed Block_Maximum_Size for
        // any block type (it bounds both the on-disk Raw/Compressed payload and
        // the RLE/Raw regenerated size). Reject rather than accept a corrupt
        // declaration that would otherwise pass the size query and let the
        // no-FCS bound under-count.
        if block_size > max_block_size {
            return Err(FrameSizeError::OversizedBlock);
        }
        offset = offset
            .checked_add(3 + on_disk)
            .filter(|end| *end <= src.len())
            .ok_or(FrameSizeError::Truncated)?;
        count += 1;
        if last_block {
            break;
        }
    }
    Ok(BlockWalk { end: offset, count })
}

/// Upper bound on the decompressed size of the FIRST frame in `src`, without
/// decoding the body. Backs the C `ZSTD_decompressBound` (per-frame term).
///
/// Returns the exact size when the header declares `Frame_Content_Size`;
/// otherwise a valid (loose) bound of `block_count * block_size_max`, where
/// `block_size_max = min(window_size, 128 KiB)` — every block decompresses to
/// at most that many bytes. Skippable frames contribute `0`. With the `legacy`
/// feature a v0.5-v0.7 frame is bounded by 128 KiB per block whatever its
/// header declares, as libzstd bounds it, since those formats do not hold a
/// frame to its declared size.
///
/// # Errors
/// [`FrameSizeError`] on an unreadable header, truncation, or a reserved block.
pub fn frame_decompressed_bound(src: &[u8]) -> Result<u64, FrameSizeError> {
    #[cfg(feature = "legacy")]
    if let Some(version) = LegacyVersion::of(src) {
        return legacy::frame_size(version, src)
            .map(|(_, bound)| bound)
            .ok_or(FrameSizeError::Truncated);
    }
    let (header, header_len) = match frame::read_frame_header_with_format(src, false) {
        Ok(parsed) => parsed,
        // Skippable frame contributes 0, but its full payload must be present:
        // truncation is an error per this function's contract.
        Err(errors::ReadFrameHeaderError::SkipFrame { length, .. }) => {
            return 8usize
                .checked_add(length as usize)
                .filter(|end| *end <= src.len())
                .map(|_| 0)
                .ok_or(FrameSizeError::Truncated);
        }
        Err(e) => return Err(FrameSizeError::Header(e)),
    };

    // Walk the blocks (and the optional checksum trailer) so a truncated frame
    // is rejected even when Frame_Content_Size is declared — without this the
    // declared-FCS path would return a bound for an incomplete buffer. The
    // per-frame block maximum both bounds the walk and scales the no-FCS bound.
    let block_size_max = frame_block_size_max(&header);
    let walk = walk_blocks(src, header_len as usize, block_size_max)?;
    if header.descriptor.content_checksum_flag() {
        walk.end
            .checked_add(4)
            .filter(|end| *end <= src.len())
            .ok_or(FrameSizeError::Truncated)?;
    }

    if header.fcs_declared() {
        return Ok(header.frame_content_size());
    }
    // Saturating is intentional here: this is an UPPER bound, so capping at the
    // maximum representable value is the correct ceiling for a pathologically
    // large frame, not a masked arithmetic bug. Each of `walk.count` blocks
    // regenerates at most `block_size_max` bytes (now enforced by `walk_blocks`,
    // so the bound can no longer be undercut by an oversized block header).
    Ok(walk.count.saturating_mul(block_size_max as u64))
}

/// What the first four bytes of a buffer announce, read by [`frame_magic`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrameMagic {
    /// A Zstandard frame (RFC 8878 §3.1.1).
    Zstd,
    /// A skippable frame (RFC 8878 §3.1.2) with its magic variant, `0..=15`.
    Skippable(u8),
}

/// Which frame `src` starts with, judged by its magic number alone. Backs the
/// C `ZSTD_isFrame` / `ZSTD_isSkippableFrame`. `None` when `src` is shorter
/// than four bytes or the magic is no frame's.
///
/// ```rust
/// use structured_zstd::decoding::{frame_magic, FrameMagic};
/// assert_eq!(frame_magic(&[0x28, 0xB5, 0x2F, 0xFD]), Some(FrameMagic::Zstd));
/// assert_eq!(frame_magic(&[0x53, 0x2A, 0x4D, 0x18]), Some(FrameMagic::Skippable(3)));
/// assert_eq!(frame_magic(b"zip"), None);
/// ```
pub fn frame_magic(src: &[u8]) -> Option<FrameMagic> {
    let magic = u32::from_le_bytes(src.get(..4)?.try_into().ok()?);
    if magic == crate::common::MAGIC_NUM {
        Some(FrameMagic::Zstd)
    } else if magic & 0xFFFF_FFF0 == crate::skippable::SKIPPABLE_MAGIC_START {
        Some(FrameMagic::Skippable((magic & 0xF) as u8))
    } else {
        None
    }
}

/// Whether `src` starts with a frame this build decodes or skips: a Zstandard
/// frame, a skippable frame, or with the `legacy` feature a v0.5-v0.7 frame.
/// Backs the C `ZSTD_isFrame`, which answers the same for upstream's default
/// build.
///
/// ```rust
/// use structured_zstd::decoding::is_frame;
/// assert!(is_frame(&[0x28, 0xB5, 0x2F, 0xFD]));
/// assert!(is_frame(&[0x50, 0x2A, 0x4D, 0x18]));
/// assert!(!is_frame(b"zip"));
/// ```
pub fn is_frame(src: &[u8]) -> bool {
    #[cfg(feature = "legacy")]
    if LegacyVersion::of(src).is_some() {
        return true;
    }
    frame_magic(src).is_some()
}

/// Bytes the compressed input must trail the end of the output by for every
/// frame in `src` to decode in place: the compressed data sits at the end of
/// the output buffer and the decoder writes from its start. Backs the C
/// `ZSTD_decompressionMargin`.
///
/// A frame costs its header, its checksum and three bytes per block, a
/// skippable frame its whole length, and one largest block is added on top:
/// a block may expand to its full size before its compressed bytes are
/// consumed (upstream `zstd_decompress.c:838-879`).
///
/// # Errors
/// [`FrameSizeError`] on an unreadable header, truncation, or an invalid block.
///
/// ```rust
/// use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
/// use structured_zstd::decoding::decompression_margin;
/// let frame = compress_slice_to_vec(&[9u8; 4096], CompressionLevel::Default);
/// assert!(decompression_margin(&frame).unwrap() >= frame.len().min(4096));
/// ```
pub fn decompression_margin(src: &[u8]) -> Result<usize, FrameSizeError> {
    let mut rest = src;
    let mut margin = 0usize;
    let mut max_block_size = 0usize;
    while !rest.is_empty() {
        let consumed = match frame::read_frame_header_with_format(rest, false) {
            Ok((header, header_len)) => {
                let block_size_max = frame_block_size_max(&header);
                let walk = walk_blocks(rest, header_len as usize, block_size_max)?;
                let checksum = if header.descriptor.content_checksum_flag() {
                    4
                } else {
                    0
                };
                let end = walk
                    .end
                    .checked_add(checksum)
                    .filter(|end| *end <= rest.len())
                    .ok_or(FrameSizeError::Truncated)?;
                // Header, block headers and checksum all lie inside
                // `..end <= rest.len()`, so the margin never exceeds the input.
                margin += header_len as usize + checksum + 3 * walk.count as usize;
                max_block_size = max_block_size.max(block_size_max);
                end
            }
            Err(errors::ReadFrameHeaderError::SkipFrame { .. }) => {
                let end = find_frame_compressed_size(rest)?;
                margin += end;
                end
            }
            Err(e) => return Err(FrameSizeError::Header(e)),
        };
        rest = &rest[consumed..];
    }
    Ok(margin + max_block_size)
}

/// Smallest output buffer a block-by-block decode of a frame can run in:
/// the window, plus room for a block written after it and split literals,
/// capped by the content size when the frame declares one. Backs the C
/// `ZSTD_decodingBufferSize_min`; `None` when the result does not fit `usize`.
///
/// The two 32-byte terms are the over-copy allowance the sequence executor's
/// wide copies may write past a match (upstream `WILDCOPY_OVERLENGTH`,
/// `zstd_decompress.c:1965-1986`).
///
/// ```rust
/// use structured_zstd::decoding::min_decoding_buffer_size;
/// // A 1 MiB window with no declared content size.
/// assert_eq!(min_decoding_buffer_size(1 << 20, None), Some((1 << 20) + 2 * 131_072 + 64));
/// // A 1 KiB window holds no block over 1 KiB, so both block terms shrink.
/// assert_eq!(min_decoding_buffer_size(1 << 10, None), Some((1 << 10) + 2 * 1024 + 64));
/// // A frame whose whole content is smaller than that.
/// assert_eq!(min_decoding_buffer_size(1 << 20, Some(1000)), Some(1000));
/// ```
pub fn min_decoding_buffer_size(
    window_size: u64,
    frame_content_size: Option<u64>,
) -> Option<usize> {
    const WILDCOPY_OVERLENGTH: u64 = 32;
    // Both block terms are the largest block the frame can hold, not a fixed
    // 128 KiB, since no block exceeds the window: RFC 8878 3.1.1.2.4,
    // Block_Maximum_Size = min(Window_Size, 128 KiB); upstream
    // zstd_decompress.c:1972-1980 computes window + 2 * that block + 64.
    let block_size = window_size.min(crate::common::MAX_BLOCK_SIZE as u64);
    let needed = window_size.checked_add(2 * block_size + 2 * WILDCOPY_OVERLENGTH);
    let size = match (frame_content_size, needed) {
        (Some(content), Some(needed)) => content.min(needed),
        // A window past `u64` still decodes in a buffer the content fits.
        (Some(content), None) => content,
        (None, Some(needed)) => needed,
        (None, None) => return None,
    };
    usize::try_from(size).ok()
}

/// Frame header fields decoded by [`read_frame_header_info`], mirroring the
/// values the C `ZSTD_getFrameHeader` fills into a `ZSTD_FrameHeader`.
#[derive(Copy, Clone, Debug)]
pub struct FrameHeaderInfo {
    /// Declared decompressed size, or [`FrameContentSize::Unknown`] when the
    /// header omits the `Frame_Content_Size` field.
    pub content_size: FrameContentSize,
    /// Decoder window size in bytes (the minimum buffer needed to decode the
    /// frame). For single-segment frames this equals the content size.
    pub window_size: u64,
    /// Dictionary id required to decode the frame, if the header carries one.
    pub dictionary_id: Option<u32>,
    /// Whether a 32-bit content checksum trails the frame.
    pub content_checksum: bool,
    /// Header length in bytes, measured in the parsed input format: it includes
    /// the 4-byte magic number in the default format, but excludes it when
    /// parsed as magicless (`read_frame_header_info(.., true)`), since those 4
    /// bytes are not present on the wire in that mode.
    pub header_size: usize,
}

/// Length in bytes of the frame header at the start of `src`, including the
/// 4-byte magic number (the offset at which the first block begins). Backs the
/// C `ZSTD_frameHeaderSize`.
///
/// Only the magic number and the descriptor byte are needed: the length
/// follows from the descriptor alone, so a caller holding just that five-byte
/// prefix learns how much of the header is still to read. Upstream's
/// `ZSTD_frameHeaderSize` measures it the same way
/// (`zstd_decompress.c:416-429`), without looking at the descriptor's other
/// bits, so a header this library refuses to decode is still measured. With
/// the `legacy` feature a v0.5-v0.7 header is measured by its own format's
/// size fields, as [`LegacyVersion::header_size`] does.
///
/// # Errors
/// [`ReadFrameHeaderError`](errors::ReadFrameHeaderError) when `src` is shorter
/// than five bytes, has a bad magic number, or is a skippable frame.
///
/// ```rust
/// use structured_zstd::decoding::frame_header_size;
/// // Magic number and a descriptor announcing a window byte only.
/// assert_eq!(frame_header_size(&[0x28, 0xB5, 0x2F, 0xFD, 0x00]).unwrap(), 6);
/// ```
pub fn frame_header_size(src: &[u8]) -> Result<usize, errors::ReadFrameHeaderError> {
    // A v0.5-v0.7 header is measured by its own format's size fields. Upstream
    // reads every header with RFC 8878's layout, which only v0.7 shares.
    #[cfg(feature = "legacy")]
    if let Some(version) = LegacyVersion::of(src) {
        return version
            .header_size(src)
            .ok_or_else(|| errors::ReadFrameHeaderError::FrameDescriptorReadError(frame::eof()));
    }
    frame::measure_header_from_slice(src)
}

/// [`frame_header_size`] for a frame in either format: with `magicless` the
/// header starts at its descriptor byte and its length counts no magic
/// number.
///
/// ```rust
/// use structured_zstd::decoding::frame_header_size_in_format;
/// // A descriptor announcing a window byte only.
/// assert_eq!(frame_header_size_in_format(&[0x00], true).unwrap(), 2);
/// assert_eq!(frame_header_size_in_format(&[0x28, 0xB5, 0x2F, 0xFD, 0x00], false).unwrap(), 6);
/// ```
///
/// # Errors
/// As [`frame_header_size`]; a magicless input fails only when it is empty.
pub fn frame_header_size_in_format(
    src: &[u8],
    magicless: bool,
) -> Result<usize, errors::ReadFrameHeaderError> {
    frame::measure_header_with_format(src, magicless)
}

/// How many bytes from its start the frame header beginning `src` takes to
/// read whole, judged from what `src` holds of it: the bytes that hold the
/// descriptor until they are all here (five, or one without the magic
/// number), then the header's own length, and eight for a skippable frame.
/// A caller reading a header in pieces asks this before
/// [`read_frame_header_info`]; upstream's `ZSTD_getFrameHeader_advanced`
/// returns the same counts.
///
/// ```rust
/// use structured_zstd::decoding::frame_header_input_needed;
/// assert_eq!(frame_header_input_needed(&[0x28, 0xB5], false).unwrap(), 5);
/// // Magic number and a descriptor announcing a window byte only.
/// assert_eq!(frame_header_input_needed(&[0x28, 0xB5, 0x2F, 0xFD, 0x00], false).unwrap(), 6);
/// assert!(frame_header_input_needed(&[0x00, 0x01], false).is_err());
/// ```
///
/// # Errors
/// [`ReadFrameHeaderError::BadMagicNumber`](errors::ReadFrameHeaderError::BadMagicNumber)
/// when the bytes start no known magic number.
pub fn frame_header_input_needed(
    src: &[u8],
    magicless: bool,
) -> Result<usize, errors::ReadFrameHeaderError> {
    frame::header_input_needed(src, magicless)
}

/// Decode the leading frame header fields of `src` without decoding the body.
///
/// Backs the C `ZSTD_getFrameHeader`. When `magicless` is `true` the 4-byte
/// magic prefix is assumed absent (the `ZSTD_f_zstd1_magicless` format); the
/// caller must know out-of-band that the stream is magicless. The reported
/// [`FrameHeaderInfo::window_size`] is the raw value derived from the header
/// (no maximum-window policy applied here; that bound is enforced at decode
/// time), so callers see the frame's own declared window even when it exceeds
/// a decoder limit.
///
/// # Errors
/// As [`read_frame_content_size`].
///
/// ```rust
/// use structured_zstd::encoding::{compress_slice_to_vec, CompressionLevel};
/// use structured_zstd::decoding::{read_frame_header_info, FrameContentSize};
/// let frame = compress_slice_to_vec(&[7u8; 512], CompressionLevel::Default);
/// let info = read_frame_header_info(&frame, false).unwrap();
/// assert_eq!(info.content_size, FrameContentSize::Known(512));
/// assert!(info.window_size >= 512);
/// ```
pub fn read_frame_header_info(
    src: &[u8],
    magicless: bool,
) -> Result<FrameHeaderInfo, errors::ReadFrameHeaderError> {
    let (header, consumed) = frame::read_frame_header_with_format(src, magicless)?;
    let content_size = if header.fcs_declared() {
        FrameContentSize::Known(header.frame_content_size())
    } else {
        FrameContentSize::Unknown
    };
    // Compute the window size without the decode-time maximum-window check
    // (RFC 8878 §3.1.1.1.2). `window_descriptor()` returns `None` for a
    // single-segment frame, where the window equals the content size.
    let window_size = match header.window_descriptor() {
        Some(desc) => {
            let exponent = u64::from(desc >> 3);
            let mantissa = u64::from(desc & 0x7);
            let window_base = 1u64 << (10 + exponent);
            window_base + (window_base / 8) * mantissa
        }
        None => header.frame_content_size(),
    };
    Ok(FrameHeaderInfo {
        content_size,
        window_size,
        dictionary_id: header.dictionary_id(),
        content_checksum: header.descriptor.content_checksum_flag(),
        header_size: consumed as usize,
    })
}

pub(crate) mod block_decoder;
pub(crate) mod buffer_backend;
pub(crate) mod decode_buffer;
pub(crate) mod dictionary;
pub(crate) mod exec_sequence_inline;
// FlatBuf is the compile-time-monomorphised "frame fits in window"
// backend selected via `DecodeBuffer<FlatBuf>`. `FrameDecoder`'s
// `DecoderScratchKind` picks it when the frame header has
// `Single_Segment_flag` set; the ring backend remains the default
// for multi-segment frames. See backlog item #132 for the wiring
// rationale.
pub(crate) mod flat_buf;
pub(crate) mod frame;
#[cfg(feature = "legacy")]
pub(crate) mod legacy;
pub(crate) mod literals_section_decoder;
pub(crate) mod prefetch;
mod ringbuffer;
#[allow(dead_code)]
pub(crate) mod scratch;
// Per-kernel monolithic sequence-section decoder entry points. Each
// kernel has its own self-contained function with the full pipeline
// (outer init, both arms, decode_one, execute_one) inlined inside one
// `#[target_feature]`-scoped body. The dispatcher in
// `sequence_section_decoder::decode_and_execute_sequences` selects the
// kernel ONCE per call via cached `detect_cpu_kernel`. aarch64 Neon
// and Sve still go through the K-generic
// `decode_and_execute_sequences_impl` shared body until their own
// monoliths land.
//
// The shared helpers (`decode_and_execute_sequences_impl`,
// `run_pipelined_sequence_loop`, `decode_one_sequence_inline`, the
// `execute_one_sequence_pipelined*` wrappers) live on aarch64
// (Neon/Sve dispatch arms in `decode_and_execute_sequences`) and in
// tests, but are orphan on x86_64 production builds where the
// per-kernel monoliths bypass them entirely. Each carries
// `#[allow(dead_code)]` so the `-D warnings` clippy gate stays green
// on x86_64 without losing the cross-arch reuse. The vestigial
// `_bmi2`/`_avx2`/`_vbmi2` variants are pre-R12 macro-dispatch
// helpers with no remaining callers; they should be cleaned up in
// a follow-up PR once the per-kernel monolithic shape is fully
// settled.
#[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
pub(crate) mod seq_decoder_avx2;
#[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
pub(crate) mod seq_decoder_bmi2;
pub(crate) mod seq_decoder_scalar;
#[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
pub(crate) mod seq_decoder_vbmi2;
pub(crate) mod sequence_execution;
pub(crate) mod sequence_section_decoder;
pub(crate) mod simd_copy;
/// Diagnostic-only re-export of the copy-shape histogram counters. Public
/// only when the `copy-shape-stats` feature is on (off in shipping builds).
#[cfg(feature = "copy-shape-stats")]
pub use simd_copy::shape_stats;
// `UserSliceBackend` is the compile-time-monomorphised backend that
// writes directly into the caller's `&mut [u8]` output slice, used
// by the `FrameDecoder::decode_all` direct-decode path. It
// eliminates the `FlatBuf` drain copy + anonymous-page-fault cost
// on large literal sections. Wiring happens via
// `DecodeBuffer<UserSliceBackend<'a>>`; the lifetime binds the
// backend to the caller's slice for the call duration.
pub(crate) mod user_slice_buf;

#[cfg(feature = "bench-internals")]
pub(crate) use self::simd_copy::copy_bytes_overshooting_for_bench;

#[cfg(test)]
mod block_api_tests;
#[cfg(test)]
mod frame_inspection_tests;
