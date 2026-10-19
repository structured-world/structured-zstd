//! Decoders for the frame formats zstd wrote before 1.0: v0.5, v0.6 and v0.7.
//!
//! libzstd 1.5.7 reads these by default (upstream's legacy level 5 in every
//! build system), so a consumer that holds an archive from 2016 gets its
//! content back from `ZSTD_decompress`; this module gives the same answer.
//! The formats differ from RFC 8878 and from each other in their headers,
//! literal and sequence coding, and dictionaries: each has its own module,
//! and what they share lives here and in [`entropy`].
//!
//! Upstream's decoders are `lib/legacy/zstd_v0{5,6,7}.c`.

mod entropy;
mod v05;
mod v06;
mod v07;

use crate::decoding::errors::LegacyFrameError as Error;
use alloc::vec::Vec;

/// One of the pre-1.0 frame formats this crate decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LegacyVersion {
    /// The v0.5 format, magic number `0xFD2FB525`.
    V05,
    /// The v0.6 format, magic number `0xFD2FB526`.
    V06,
    /// The v0.7 format, magic number `0xFD2FB527`.
    V07,
}

impl LegacyVersion {
    /// The format whose magic number starts `src`, if it is one of these.
    pub fn of(src: &[u8]) -> Option<LegacyVersion> {
        let magic = u32::from_le_bytes(src.get(..4)?.try_into().ok()?);
        match magic {
            0xFD2F_B525 => Some(LegacyVersion::V05),
            0xFD2F_B526 => Some(LegacyVersion::V06),
            0xFD2F_B527 => Some(LegacyVersion::V07),
            _ => None,
        }
    }

    /// The minor version number: 5, 6 or 7.
    pub fn minor(self) -> u8 {
        match self {
            LegacyVersion::V05 => 5,
            LegacyVersion::V06 => 6,
            LegacyVersion::V07 => 7,
        }
    }
}

/// Most bytes one block decodes to, the formats' `BLOCKSIZE`.
const BLOCK_SIZE_MAX: usize = 128 * 1024;

fn corrupt(reason: &'static str) -> Error {
    Error::Corrupted { reason }
}

/// A dictionary as the legacy decoders take it.
#[derive(Clone, Copy)]
pub(crate) enum LegacyDict<'a> {
    None,
    /// The bytes as the caller supplied them; each format reads its own
    /// layout from them.
    Raw(&'a [u8]),
    /// A dictionary already split by the RFC 8878 parser, whose layout is the
    /// v0.7 one: its ID, the bytes from its entropy tables through its repeat
    /// offsets, and its content.
    Split {
        id: u32,
        entropy: &'a [u8],
        content: &'a [u8],
    },
}

impl<'a> LegacyDict<'a> {
    /// The dictionary's bytes as one contiguous run, for a format that reads
    /// a split dictionary as plain content. Only a split dictionary needs the
    /// copy; it is rare enough (an RFC 8878 dictionary with a v0.5 or v0.6
    /// frame) to pay for one.
    fn whole(&self) -> alloc::borrow::Cow<'a, [u8]> {
        use alloc::borrow::Cow;
        match *self {
            LegacyDict::None => Cow::Borrowed(&[]),
            LegacyDict::Raw(bytes) => Cow::Borrowed(bytes),
            LegacyDict::Split {
                id,
                entropy,
                content,
            } => {
                let mut whole = Vec::with_capacity(8 + entropy.len() + content.len());
                whole.extend_from_slice(&v07::DICT_MAGIC.to_le_bytes());
                whole.extend_from_slice(&id.to_le_bytes());
                whole.extend_from_slice(entropy);
                whole.extend_from_slice(content);
                Cow::Owned(whole)
            }
        }
    }
}

/// The frame header fields the decoders and the size queries need.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameHeader {
    pub(crate) version: LegacyVersion,
    /// Header length including the magic number.
    pub(crate) size: usize,
    /// The window the frame needs, in bytes.
    pub(crate) window: u64,
    /// The declared content size; the formats write 0 for unknown.
    pub(crate) content_size: Option<u64>,
    /// The dictionary the frame names, 0 for none.
    pub(crate) dict_id: u32,
    /// Whether a v0.7 frame carries a content checksum.
    pub(crate) checksum: bool,
}

/// Read the frame header at the start of `src`.
pub(crate) fn read_header(src: &[u8]) -> Result<FrameHeader, Error> {
    match LegacyVersion::of(src).ok_or(corrupt("not a v0.5-v0.7 frame"))? {
        LegacyVersion::V05 => v05::read_header(src),
        LegacyVersion::V06 => v06::read_header(src),
        LegacyVersion::V07 => v07::read_header(src),
    }
}

/// The content size the frame at the start of `src` declares, `None` when it
/// declares none or its header does not read: libzstd reports both as an
/// unknown size (`ZSTD_getDecompressedSize_legacy`).
pub(crate) fn declared_content_size(src: &[u8]) -> Option<u64> {
    read_header(src).ok()?.content_size
}

/// A block as its three-byte header describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Compressed(usize),
    Raw(usize),
    /// A byte repeated this many times.
    Rle(usize),
    End,
}

impl Block {
    fn read(src: &[u8]) -> Result<Block, Error> {
        let header = src.get(..3).ok_or(Error::Truncated)?;
        let size =
            usize::from(header[2]) | usize::from(header[1]) << 8 | usize::from(header[0] & 7) << 16;
        Ok(match header[0] >> 6 {
            0 => Block::Compressed(size),
            1 => Block::Raw(size),
            2 => Block::Rle(size),
            _ => Block::End,
        })
    }

    /// Bytes the block's body takes after its header.
    fn body_len(self) -> usize {
        match self {
            Block::Compressed(size) | Block::Raw(size) => size,
            Block::Rle(_) => 1,
            Block::End => 0,
        }
    }
}

/// Whether `block` ends the frame. v0.5 and v0.6 end it at any block whose
/// body is empty, as their decoders and size walks do
/// (`if (cBlockSize == 0) break;`); v0.7 only at the end block.
fn ends_frame(version: LegacyVersion, block: Block) -> bool {
    match version {
        LegacyVersion::V07 => block == Block::End,
        LegacyVersion::V05 | LegacyVersion::V06 => block.body_len() == 0,
    }
}

/// Where the frame starting `src` ends, and the most it can decode to: one
/// block's maximum per block (`ZSTD_findFrameSizeInfoLegacy`).
pub(crate) fn frame_size(src: &[u8]) -> Result<(usize, u64), Error> {
    let header = read_header(src)?;
    let mut at = header.size;
    let mut blocks = 0u64;
    loop {
        let block = Block::read(&src[at.min(src.len())..])?;
        at += 3;
        let body = block.body_len();
        if body > src.len() - at.min(src.len()) {
            return Err(Error::Truncated);
        }
        if ends_frame(header.version, block) {
            break;
        }
        at += body;
        blocks += 1;
    }
    Ok((at, blocks * BLOCK_SIZE_MAX as u64))
}

/// Where decoded bytes go: an output that knows how much of the frame it has
/// produced and how much of that it still holds for matches to reach.
pub(crate) trait Output {
    /// Bytes of the current frame produced so far.
    fn produced(&self) -> usize;
    /// Bytes of the current frame still held, the most a match can reach
    /// back within the frame.
    fn held(&self) -> usize;
    /// Refuse `len` more bytes the output cannot take.
    fn reserve(&mut self, len: usize) -> Result<(), Error>;
    fn extend(&mut self, bytes: &[u8]);
    fn repeat(&mut self, byte: u8, len: usize);
    /// Append `len` bytes copied from `offset` back, `1 <= offset <= held`;
    /// an overlapping copy repeats the bytes it has just written.
    fn copy_back(&mut self, offset: usize, len: usize);
    /// The bytes produced since `from` (a value of `produced()`), for the
    /// content checksum.
    fn since(&self, from: usize) -> &[u8];
}

/// Output into a growing vector, the current frame starting at `start`.
pub(crate) struct VecOutput<'a> {
    pub(crate) buf: &'a mut Vec<u8>,
    pub(crate) start: usize,
    /// Bytes of the frame already handed on and dropped from `buf`.
    pub(crate) dropped: usize,
    /// Most bytes the frame may produce.
    pub(crate) limit: usize,
}

impl Output for VecOutput<'_> {
    fn produced(&self) -> usize {
        self.dropped + self.held()
    }

    fn held(&self) -> usize {
        self.buf.len() - self.start
    }

    fn reserve(&mut self, len: usize) -> Result<(), Error> {
        if len > self.limit - self.produced() {
            return Err(Error::OutputTooSmall);
        }
        self.buf.reserve(len);
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn repeat(&mut self, byte: u8, len: usize) {
        self.buf.resize(self.buf.len() + len, byte);
    }

    fn copy_back(&mut self, offset: usize, len: usize) {
        let from = self.buf.len() - offset;
        if offset >= len {
            self.buf.extend_from_within(from..from + len);
        } else {
            for i in 0..len {
                let byte = self.buf[from + i];
                self.buf.push(byte);
            }
        }
    }

    fn since(&self, from: usize) -> &[u8] {
        &self.buf[self.start + (from - self.dropped)..]
    }
}

/// Output into a caller's slice, the current frame starting at its start.
pub(crate) struct SliceOutput<'a> {
    pub(crate) buf: &'a mut [u8],
    pub(crate) pos: usize,
}

impl Output for SliceOutput<'_> {
    fn produced(&self) -> usize {
        self.pos
    }

    fn held(&self) -> usize {
        self.pos
    }

    fn reserve(&mut self, len: usize) -> Result<(), Error> {
        if len > self.buf.len() - self.pos {
            return Err(Error::OutputTooSmall);
        }
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
    }

    fn repeat(&mut self, byte: u8, len: usize) {
        self.buf[self.pos..self.pos + len].fill(byte);
        self.pos += len;
    }

    fn copy_back(&mut self, offset: usize, len: usize) {
        let from = self.pos - offset;
        if offset >= len {
            self.buf.copy_within(from..from + len, self.pos);
        } else {
            for i in 0..len {
                self.buf[self.pos + i] = self.buf[from + i];
            }
        }
        self.pos += len;
    }

    fn since(&self, from: usize) -> &[u8] {
        &self.buf[from..self.pos]
    }
}

/// Copy a match of `len` bytes from `offset` back: within the frame, from
/// the dictionary's content in front of it, or across both
/// (`ZSTDv07_execSequence`). The output has room for `len` more bytes.
fn copy_match<O: Output>(out: &mut O, offset: usize, len: usize, dict: &[u8]) -> Result<(), Error> {
    if offset == 0 {
        return Err(corrupt("match with offset 0"));
    }
    let held = out.held();
    if offset <= held {
        out.copy_back(offset, len);
        return Ok(());
    }
    let produced = out.produced();
    if offset <= produced {
        return Err(corrupt("match reaches before the window"));
    }
    let back = offset - produced;
    if back > dict.len() {
        return Err(corrupt("match reaches before the dictionary"));
    }
    let from = dict.len() - back;
    let first = len.min(back);
    out.extend(&dict[from..from + first]);
    let rest = len - first;
    if rest > 0 {
        // The match runs on from the frame's first byte.
        if produced != held {
            return Err(corrupt("match reaches before the window"));
        }
        let distance = out.held();
        out.copy_back(distance, rest);
    }
    Ok(())
}

/// Append one sequence: `lit_len` literals from `literals[*lit_pos..]`, then a
/// match of `match_len` bytes from `offset` back.
fn execute<O: Output>(
    out: &mut O,
    literals: &[u8],
    lit_pos: &mut usize,
    lit_len: usize,
    match_len: usize,
    offset: usize,
    dict: &[u8],
) -> Result<(), Error> {
    out.reserve(
        lit_len
            .checked_add(match_len)
            .ok_or(Error::OutputTooSmall)?,
    )?;
    let lits = literals
        .get(*lit_pos..)
        .and_then(|rest| rest.get(..lit_len))
        .ok_or(corrupt("sequence takes more literals than the block has"))?;
    out.extend(lits);
    *lit_pos += lit_len;
    copy_match(out, offset, match_len, dict)
}

/// The literal section's header, common to the three formats.
enum Literals {
    /// Huffman-coded: `size` literals from `compressed` bytes after a
    /// `header`-byte header, in one stream or four.
    Huffman {
        header: usize,
        size: usize,
        compressed: usize,
        single_stream: bool,
    },
    /// Huffman-coded with the previous table: one stream.
    Repeat {
        size: usize,
        compressed: usize,
    },
    Raw {
        header: usize,
        size: usize,
    },
    Rle {
        header: usize,
        size: usize,
    },
}

impl Literals {
    /// Read the literal header at the start of a compressed block
    /// (`ZSTDv07_decodeLiteralsBlock`).
    fn read(src: &[u8]) -> Result<Literals, Error> {
        // A compressed block holds at least a literal header byte, one
        // literal or run byte, and the sequence count.
        if src.len() < 3 {
            return Err(corrupt("compressed block too short"));
        }
        let b0 = usize::from(src[0]);
        let b1 = usize::from(src[1]);
        let b2 = usize::from(src[2]);
        let size_format = (b0 >> 4) & 3;
        Ok(match b0 >> 6 {
            0 => {
                if src.len() < 5 {
                    return Err(corrupt("compressed block too short"));
                }
                let b3 = usize::from(src[3]);
                let b4 = usize::from(src[4]);
                let (header, size, compressed, single_stream) = match size_format {
                    0 | 1 => (
                        3,
                        ((b0 & 15) << 6) + (b1 >> 2),
                        ((b1 & 3) << 8) + b2,
                        b0 & 16 != 0,
                    ),
                    2 => (
                        4,
                        ((b0 & 15) << 10) + (b1 << 2) + (b2 >> 6),
                        ((b2 & 63) << 8) + b3,
                        false,
                    ),
                    _ => (
                        5,
                        ((b0 & 15) << 14) + (b1 << 6) + (b2 >> 2),
                        ((b2 & 3) << 16) + (b3 << 8) + b4,
                        false,
                    ),
                };
                if size > BLOCK_SIZE_MAX {
                    return Err(corrupt("literal section too large"));
                }
                if compressed + header > src.len() {
                    return Err(corrupt("literals run past the block"));
                }
                Literals::Huffman {
                    header,
                    size,
                    compressed,
                    single_stream,
                }
            }
            1 => {
                // Only the small single-stream form exists.
                if size_format != 1 {
                    return Err(corrupt("repeated-table literals in an unknown form"));
                }
                let size = ((b0 & 15) << 6) + (b1 >> 2);
                let compressed = ((b1 & 3) << 8) + b2;
                if compressed + 3 > src.len() {
                    return Err(corrupt("literals run past the block"));
                }
                Literals::Repeat { size, compressed }
            }
            kind => {
                let (header, size) = match size_format {
                    0 | 1 => (1, b0 & 31),
                    2 => (2, ((b0 & 15) << 8) + b1),
                    _ => (3, ((b0 & 15) << 16) + (b1 << 8) + b2),
                };
                if kind == 2 {
                    if size + header > src.len() {
                        return Err(corrupt("literals run past the block"));
                    }
                    Literals::Raw { header, size }
                } else {
                    if header == 3 && src.len() < 4 {
                        return Err(corrupt("compressed block too short"));
                    }
                    if size > BLOCK_SIZE_MAX {
                        return Err(corrupt("literal section too large"));
                    }
                    Literals::Rle { header, size }
                }
            }
        })
    }
}

/// Where a block's literals are once decoded.
enum LiteralSource {
    /// In the block itself, at this range.
    Block(core::ops::Range<usize>),
    /// In the decoder's literal buffer.
    Buffer,
}

/// The per-format state of a frame being decoded.
enum VersionDecoder {
    V05(v05::Decoder),
    V06(v06::Decoder),
    V07(v07::Decoder),
}

/// An output that refuses to grow past `end` produced bytes: a block decodes
/// to at most [`BLOCK_SIZE_MAX`], however many sequences it holds.
struct Budget<'o, O: Output> {
    out: &'o mut O,
    end: usize,
}

impl<O: Output> Output for Budget<'_, O> {
    fn produced(&self) -> usize {
        self.out.produced()
    }
    fn held(&self) -> usize {
        self.out.held()
    }
    fn reserve(&mut self, len: usize) -> Result<(), Error> {
        if len > self.end - self.out.produced() {
            return Err(corrupt("block decodes past the largest block size"));
        }
        self.out.reserve(len)
    }
    fn extend(&mut self, bytes: &[u8]) {
        self.out.extend(bytes);
    }
    fn repeat(&mut self, byte: u8, len: usize) {
        self.out.repeat(byte, len);
    }
    fn copy_back(&mut self, offset: usize, len: usize) {
        self.out.copy_back(offset, len);
    }
    fn since(&self, from: usize) -> &[u8] {
        self.out.since(from)
    }
}

/// A frame being decoded block by block: the whole-buffer and the streaming
/// decoders both drive one.
pub(crate) struct LegacyFrame<'d> {
    header: FrameHeader,
    decoder: VersionDecoder,
    /// The bytes holding the dictionary's content, at `content`.
    dict: alloc::borrow::Cow<'d, [u8]>,
    content: core::ops::Range<usize>,
    #[cfg(feature = "hash")]
    hash: Option<twox_hash::XxHash64>,
}

impl<'d> LegacyFrame<'d> {
    /// Start decoding the frame `header` describes.
    pub(crate) fn new(header: FrameHeader, dict: LegacyDict<'d>) -> Result<LegacyFrame<'d>, Error> {
        use alloc::borrow::Cow;
        let (decoder, bytes, content) = match header.version {
            LegacyVersion::V07 => {
                let (decoder, content) = v07::Decoder::new(dict)?;
                let bytes: &'d [u8] = match dict {
                    LegacyDict::None => &[],
                    LegacyDict::Raw(raw) => raw,
                    LegacyDict::Split { content, .. } => content,
                };
                if header.dict_id != 0 && header.dict_id != decoder.dict_id {
                    return Err(Error::DictionaryMismatch {
                        expected: header.dict_id,
                        provided: decoder.dict_id,
                    });
                }
                (VersionDecoder::V07(decoder), Cow::Borrowed(bytes), content)
            }
            LegacyVersion::V06 => {
                let whole = dict.whole();
                let (decoder, content) = v06::Decoder::new(&whole)?;
                (VersionDecoder::V06(decoder), whole, content)
            }
            LegacyVersion::V05 => {
                let whole = dict.whole();
                let (decoder, content) = v05::Decoder::new(&whole)?;
                (VersionDecoder::V05(decoder), whole, content)
            }
        };
        Ok(LegacyFrame {
            header,
            decoder,
            dict: bytes,
            content,
            #[cfg(feature = "hash")]
            hash: header.checksum.then(|| twox_hash::XxHash64::with_seed(0)),
        })
    }

    pub(crate) fn header(&self) -> &FrameHeader {
        &self.header
    }

    /// Decode one block, `body` being the bytes after its header.
    fn block<O: Output>(&mut self, block: Block, body: &[u8], out: &mut O) -> Result<(), Error> {
        let before = out.produced();
        let mut out = Budget {
            end: before + BLOCK_SIZE_MAX,
            out,
        };
        let dict = &self.dict[self.content.clone()];
        match block {
            Block::Compressed(_) => match &mut self.decoder {
                VersionDecoder::V05(decoder) => decoder.block(body, &mut out, dict)?,
                VersionDecoder::V06(decoder) => decoder.block(body, &mut out, dict)?,
                VersionDecoder::V07(decoder) => decoder.block(body, &mut out, dict)?,
            },
            Block::Raw(len) => {
                out.reserve(len)?;
                out.extend(body);
            }
            Block::Rle(len) => {
                out.reserve(len)?;
                out.repeat(body[0], len);
            }
            Block::End => {}
        }
        #[cfg(feature = "hash")]
        if let Some(hash) = &mut self.hash {
            use core::hash::Hasher;
            hash.write(out.since(before));
        }
        Ok(())
    }

    /// Close the frame at its end block, whose header is `end`: the content
    /// checksum a v0.7 frame keeps there.
    ///
    /// Upstream checks it only while streaming; a frame failing it is
    /// corrupt whichever way it is read, so it is checked here for both.
    ///
    /// The declared content size is not checked against the output. It is
    /// no integrity field in these formats: the v0.6.2 command line writes
    /// the size of the first chunk it read, so a v0.6 archive past 128 KiB
    /// declares too little. Upstream's streaming decoder (`zstd -d`) reads
    /// such an archive whole; its one-shot `ZSTD_decompress` calls it
    /// corrupt, which would refuse intact data.
    fn finish(&mut self, end: [u8; 3]) -> Result<(), Error> {
        #[cfg(feature = "hash")]
        if let Some(hash) = &self.hash {
            use core::hash::Hasher;
            let expected =
                u32::from(end[2]) | u32::from(end[1]) << 8 | u32::from(end[0] & 0x3F) << 16;
            let calculated = ((hash.finish() >> 11) & 0x3F_FFFF) as u32;
            if expected != calculated {
                return Err(Error::ChecksumMismatch {
                    expected,
                    calculated,
                });
            }
        }
        #[cfg(not(feature = "hash"))]
        let _ = end;
        Ok(())
    }
}

/// Decode the frame starting `src` into `out`. Returns the bytes the frame
/// took.
pub(crate) fn decode_frame<O: Output>(
    src: &[u8],
    dict: LegacyDict<'_>,
    out: &mut O,
) -> Result<usize, Error> {
    let header = read_header(src)?;
    let mut frame = LegacyFrame::new(header, dict)?;
    let mut at = header.size;
    loop {
        let block_header = src.get(at..at + 3).ok_or(Error::Truncated)?;
        let block = Block::read(block_header)?;
        let end = [block_header[0], block_header[1], block_header[2]];
        at += 3;
        let body = src.get(at..at + block.body_len()).ok_or(Error::Truncated)?;
        if ends_frame(header.version, block) {
            frame.finish(end)?;
            return Ok(at);
        }
        frame.block(block, body, out)?;
        at += body.len();
    }
}

#[cfg(test)]
mod tests;
