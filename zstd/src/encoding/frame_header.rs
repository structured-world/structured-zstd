//! Utilities and representations for a frame header.
use crate::common::MAGIC_NUM;
use crate::encoding::util::{find_fcs_field_size, find_min_size};
use alloc::vec::Vec;

/// A header for a single Zstandard frame.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header>
///
/// The containing module is `pub(crate)`, so this struct is not
/// externally constructible. The `Default` derive is kept as a
/// convenience for tests and for internal callers that only need
/// to override specific fields.
#[derive(Debug, Default)]
pub struct FrameHeader {
    /// Optionally, the original (uncompressed) size of the data within the frame in bytes.
    /// If not present, `window_size` must be set.
    pub frame_content_size: Option<u64>,
    /// If set to true, data must be regenerated within a single
    /// continuous memory segment.
    pub single_segment: bool,
    /// If set to true, a 32 bit content checksum will be present
    /// at the end of the frame.
    pub content_checksum: bool,
    /// If a dictionary ID is provided, the ID of that dictionary.
    pub dictionary_id: Option<u64>,
    /// The minimum memory buffer required to compress a frame. If not present,
    /// `single_segment` will be set to true. If present, this value must be greater than 1KB
    /// and less than 3.75TB. Encoders should not generate a frame that requires a window size larger than
    /// 8mb.
    pub window_size: Option<u64>,
    /// If true, the 4-byte magic number prefix is omitted from the
    /// serialized output. The caller MUST know out-of-band that the
    /// stream is magicless and use a magicless-aware decoder.
    /// Upstream zstd parity: `ZSTD_f_zstd1_magicless` (see `ZSTD_d_format`).
    pub magicless: bool,
}

impl FrameHeader {
    /// Writes the serialized frame header into the provided buffer.
    ///
    /// The returned header *does include* a frame header descriptor.
    pub fn serialize(self, output: &mut Vec<u8>) {
        vprintln!("Serializing frame with header: {self:?}");
        // https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header
        // The fields are laid out in a local buffer with fixed-width stores,
        // each followed by the field's real width, and land in `output` in one
        // copy: pushed one at a time, every field was a capacity check and the
        // variable-width ones a `memcpy` call.
        let mut header = [0u8; MAX_FRAME_HEADER_LEN];
        let mut len = 0usize;
        // Magic Number (omitted in magicless mode — `ZSTD_f_zstd1_magicless`):
        if !self.magicless {
            header[..4].copy_from_slice(&MAGIC_NUM.to_le_bytes());
            len = 4;
        }

        // `Frame_Header_Descriptor`:
        header[len] = self.descriptor();
        len += 1;

        // `Window_Descriptor
        // TODO: https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#window_descriptor
        if !self.single_segment
            && let Some(window_size) = self.window_size
        {
            let log = window_size.next_power_of_two().ilog2();
            let exponent = if log > 10 { log - 10 } else { 1 } as u8;
            header[len] = exponent << 3;
            len += 1;
        }

        if let Some(id) = self.dictionary_id {
            // At most four bytes: `descriptor` refuses a wider id above. Four
            // are stored and the id's width kept.
            header[len..len + 4].copy_from_slice(&(id as u32).to_le_bytes());
            len += find_min_size(id);
        }

        if let Some(frame_content_size) = self.frame_content_size {
            let field_size = find_fcs_field_size(frame_content_size, self.single_segment);
            // RFC 8878 3.1.1.1.4: a two-byte field stores the size less 256.
            let stored = if field_size == 2 {
                frame_content_size - 256
            } else {
                frame_content_size
            };
            header[len..len + 8].copy_from_slice(&stored.to_le_bytes());
            len += field_size;
        }
        let start = output.len();
        if output.capacity() - start >= MAX_FRAME_HEADER_LEN {
            // A copy of constant width, a pair of loads and stores, into the
            // room the frame's caller reserved; only `len` bytes are kept.
            // SAFETY: the buffer has `MAX_FRAME_HEADER_LEN` bytes of spare
            // capacity past `start`, and `len <= MAX_FRAME_HEADER_LEN` bytes of
            // them are initialized by the copy before the length covers them.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    header.as_ptr(),
                    output.as_mut_ptr().add(start),
                    MAX_FRAME_HEADER_LEN,
                );
                output.set_len(start + len);
            }
        } else {
            output.extend_from_slice(&header[..len]);
        }
    }

    /// Generate a serialized frame header descriptor for the frame header.
    ///
    /// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header_descriptor>
    fn descriptor(&self) -> u8 {
        // A frame header starts with a frame header descriptor.
        // It describes what other fields are present
        // https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header_descriptor
        // Built with plain bit arithmetic (LSB-first field order below); a
        // `BitWriter` here allocated a fresh `Vec` for the single byte on
        // every frame header.

        // Bits 0-1, `Dictionary_ID_flag`: size class of the
        // `Dictionary_ID` field (0 = absent).
        let dict_id_flag: u8 = match self.dictionary_id.map(find_min_size) {
            None => 0,
            Some(1) => 1,
            Some(2) => 2,
            Some(4) => 3,
            _ => panic!(),
        };

        // Bit 2, `Content_Checksum_flag`.
        let checksum_flag = u8::from(self.content_checksum);

        // Bits 3-4: `Reserved_bit` + `Unused_bit`, both zero.

        // Bit 5, `Single_Segment_flag`:
        // If this flag is set, data must be regenerated within a single continuous memory segment,
        // and the `Frame_Content_Size` field must be present in the header.
        // If this flag is not set, the `Window_Descriptor` field must be present in the frame header.
        if self.single_segment {
            assert!(
                self.frame_content_size.is_some(),
                "if the `single_segment` flag is set to true, then a frame content size must be provided"
            );
        } else {
            assert!(
                self.window_size.is_some(),
                "if the `single_segment` flag is set to false, then a window size must be provided"
            );
        }
        let single_segment_flag = u8::from(self.single_segment);

        // Bits 6-7, `Frame_Content_Size_flag`: size class of the FCS field.
        // If the `Single_Segment_flag` is set and this value is zero,
        // the size of the FCS field is 1 byte; otherwise the FCS field is
        // omitted when the flag is zero.
        // | Value | Size of field (Bytes)
        // | 0     | 0 or 1
        // | 1     | 2
        // | 2     | 4
        // | 3     | 8
        let fcs_flag: u8 = match self.frame_content_size {
            Some(frame_content_size) => {
                match find_fcs_field_size(frame_content_size, self.single_segment) {
                    1 => 0,
                    2 => 1,
                    4 => 2,
                    8 => 3,
                    _ => unreachable!(),
                }
            }
            None => 0,
        };

        dict_id_flag | (checksum_flag << 2) | (single_segment_flag << 5) | (fcs_flag << 6)
    }
}

/// Whether a frame whose header writes `content_size` (`None` when it leaves
/// the size out) under a window of `window_size` bytes is single-segment:
/// whenever the size is written and the window covers it, with a dictionary or
/// without, as upstream decides it (`ZSTD_writeFrameHeader`,
/// `zstd_compress.c:4728`). The decoder's window is then the content size
/// (RFC 8878 3.1.1.1.2) rather than the power of two above it, and a
/// dictionary stays referenceable before the content.
pub(crate) fn single_segment(content_size: Option<u64>, window_size: u64) -> bool {
    content_size.is_some_and(|size| size <= window_size)
}

/// The longest frame header: magic (4), descriptor (1), window descriptor (1),
/// dictionary id (4) and content size (8) (RFC 8878 3.1.1). A dictionary id is
/// stored in at most four bytes here, and a frame with a window descriptor
/// carries no one-byte content size, so the fixed-width stores of
/// [`FrameHeader::serialize`] stay inside it.
const MAX_FRAME_HEADER_LEN: usize = 18;

#[cfg(test)]
mod tests;
