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

use crate::decoding::Dictionary;
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
    /// A dictionary the RFC 8878 parser has read. Its layout is the v0.7 one,
    /// so a v0.7 frame takes its tables, repeat offsets and ID; v0.5 and v0.6
    /// read any dictionary of the v0.7 layout as plain content, and are given
    /// its content.
    Parsed(&'a Dictionary),
}

impl<'a> LegacyDict<'a> {
    /// The bytes a v0.5 or v0.6 frame reads as its dictionary.
    ///
    /// Those formats take a dictionary of the v0.7 layout whole, header and
    /// tables too, as content. A parsed one keeps only its content, which is
    /// the end of those bytes: a match reaching past it into the header
    /// fails as reaching before the dictionary, rather than decoding to
    /// other bytes.
    pub(crate) fn content_bytes(&self) -> &'a [u8] {
        match *self {
            LegacyDict::None => &[],
            LegacyDict::Parsed(dict) => &dict.dict_content,
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
    /// Whether a v0.7 frame carries a content checksum. Read only where the
    /// checksum can be computed, under `hash`; the header parses it always.
    #[cfg_attr(not(feature = "hash"), allow(dead_code))]
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

/// The length of the frame header at the start of `src` from its size fields
/// alone, its other fields unchecked, as upstream's size walks read it
/// (`ZSTDv0{6,7}_frameHeaderSize`; v0.5 headers are always 5 bytes).
pub(crate) fn header_len(version: LegacyVersion, src: &[u8]) -> Result<usize, Error> {
    let descriptor = *src.get(4).ok_or(Error::Truncated)?;
    Ok(match version {
        LegacyVersion::V05 => 5,
        LegacyVersion::V06 => 5 + [0, 1, 2, 8][usize::from(descriptor >> 6)],
        LegacyVersion::V07 => {
            let single_segment = descriptor & 0x20 != 0;
            let fcs = [0, 2, 4, 8][usize::from(descriptor >> 6)];
            let window = usize::from(!single_segment);
            // A single-segment frame always carries its size, one byte at least.
            let short_size = usize::from(single_segment && fcs == 0);
            5 + window + [0, 1, 2, 4][usize::from(descriptor & 3)] + fcs + short_size
        }
    })
}

/// Where the `version` frame starting `src` ends, and the most it can decode
/// to: one block's maximum per block (`ZSTD_findFrameSizeInfoLegacy`). `None`
/// when `src` ends inside the frame, the only way the walk fails.
pub(crate) fn frame_size(version: LegacyVersion, src: &[u8]) -> Option<(usize, u64)> {
    let mut at = header_len(version, src).ok()?;
    let mut blocks = 0u64;
    loop {
        let block = Block::read(src.get(at..)?).ok()?;
        at += 3;
        let body = block.body_len();
        if body > src.len().checked_sub(at)? {
            return None;
        }
        if ends_frame(version, block) {
            break;
        }
        at += body;
        blocks += 1;
    }
    Some((at, blocks * BLOCK_SIZE_MAX as u64))
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
    #[cfg(feature = "hash")]
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

    #[cfg(feature = "hash")]
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

    #[cfg(feature = "hash")]
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
    #[cfg(feature = "hash")]
    fn since(&self, from: usize) -> &[u8] {
        self.out.since(from)
    }
}

/// A frame being decoded block by block: the whole-buffer and the streaming
/// decoders both drive one. The dictionary it was started with is passed to
/// each block again, as [`LegacyDict::content_bytes`], so the frame holds no
/// borrow and a stream can keep it beside the dictionary's handle.
pub(crate) struct LegacyFrame {
    version: LegacyVersion,
    decoder: VersionDecoder,
    /// Where the dictionary's content lies within its bytes.
    content: core::ops::Range<usize>,
    #[cfg(feature = "hash")]
    hash: Option<twox_hash::XxHash64>,
}

impl LegacyFrame {
    /// Start decoding the frame `header` describes.
    pub(crate) fn new(header: FrameHeader, dict: LegacyDict<'_>) -> Result<LegacyFrame, Error> {
        let bytes = dict.content_bytes();
        let (decoder, content) = match header.version {
            LegacyVersion::V07 => {
                let (decoder, content) = v07::Decoder::new(dict)?;
                if header.dict_id != 0 && header.dict_id != decoder.dict_id {
                    return Err(Error::DictionaryMismatch {
                        expected: header.dict_id,
                        provided: decoder.dict_id,
                    });
                }
                (VersionDecoder::V07(decoder), content)
            }
            LegacyVersion::V06 => {
                let (decoder, content) = v06::Decoder::new(bytes)?;
                (VersionDecoder::V06(decoder), content)
            }
            LegacyVersion::V05 => {
                let (decoder, content) = v05::Decoder::new(bytes)?;
                (VersionDecoder::V05(decoder), content)
            }
        };
        Ok(LegacyFrame {
            version: header.version,
            decoder,
            content,
            #[cfg(feature = "hash")]
            hash: header.checksum.then(|| twox_hash::XxHash64::with_seed(0)),
        })
    }

    /// Decode one block, `body` being the bytes after its header, with the
    /// bytes of the dictionary the frame was started with.
    fn block<O: Output>(
        &mut self,
        block: Block,
        body: &[u8],
        out: &mut O,
        dict_bytes: &[u8],
    ) -> Result<(), Error> {
        let before = out.produced();
        let mut out = Budget {
            end: before + BLOCK_SIZE_MAX,
            out,
        };
        let dict = &dict_bytes[self.content.clone()];
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
    let dict_bytes = dict.content_bytes();
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
        frame.block(block, body, out, dict_bytes)?;
        at += body.len();
    }
}

/// A v0.5-v0.7 frame decoded from input that arrives in pieces, a block at a
/// time, its output handed out as each block completes. It holds the window
/// the formats reach back into plus one block, as upstream's buffered
/// decoders (`ZBUFFv0{5,6,7}`) size theirs, and drops older output once it
/// has been handed out.
pub(crate) struct LegacyStream {
    /// The frame in progress; `None` when no frame is.
    frame: Option<LegacyFrame>,
    /// The dictionary the frame was started with.
    dict: Option<crate::decoding::DictionaryHandle>,
    /// Output still within reach of a match, and output not handed out yet.
    buf: Vec<u8>,
    /// Bytes of the frame dropped from the front of `buf`.
    dropped: usize,
    /// How much of `buf` has been handed out.
    delivered: usize,
    /// History kept for matches: the window and one block.
    keep: usize,
    /// Whether the frame's end block has been read.
    finished: bool,
    /// Bytes of the frame read so far, its header included.
    consumed: u64,
    /// A block read from a `Read` source, held while it decodes.
    block: Vec<u8>,
}

impl LegacyStream {
    pub(crate) const fn new() -> LegacyStream {
        LegacyStream {
            frame: None,
            dict: None,
            buf: Vec::new(),
            dropped: 0,
            delivered: 0,
            keep: 0,
            finished: false,
            consumed: 0,
            block: Vec::new(),
        }
    }

    /// Start the frame `header` describes, with `dict`. A frame already in
    /// progress is dropped.
    pub(crate) fn start(
        &mut self,
        header: FrameHeader,
        dict: Option<crate::decoding::DictionaryHandle>,
    ) -> Result<(), Error> {
        let legacy_dict = dict
            .as_ref()
            .map_or(LegacyDict::None, |dict| LegacyDict::Parsed(dict.as_dict()));
        self.frame = Some(LegacyFrame::new(header, legacy_dict)?);
        self.dict = dict;
        self.buf.clear();
        self.dropped = 0;
        self.delivered = 0;
        // Every header reader holds the window to its format's largest, at
        // most 128 MiB, which every target addresses.
        debug_assert!(header.window <= 1 << 27);
        self.keep = header.window as usize + BLOCK_SIZE_MAX;
        self.finished = false;
        self.consumed = header.size as u64;
        Ok(())
    }

    /// Leave the frame in progress, if any, keeping the buffer's capacity.
    pub(crate) fn end(&mut self) {
        self.frame = None;
        self.dict = None;
        self.buf.clear();
        self.delivered = 0;
        self.finished = false;
    }

    /// Whether a frame is in progress, or finished with output still to hand
    /// out.
    pub(crate) fn active(&self) -> bool {
        self.frame.is_some()
    }

    /// Whether the frame's end block has been read.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Decoded bytes not handed out yet.
    pub(crate) fn undelivered(&self) -> usize {
        self.buf.len() - self.delivered
    }

    /// Bytes the frame has decoded to so far.
    pub(crate) fn produced(&self) -> u64 {
        (self.dropped + self.buf.len()) as u64
    }

    /// Bytes of the frame read so far, its header included.
    pub(crate) fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Heap bytes the stream holds.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.buf.capacity() + self.block.capacity()
    }

    /// How many bytes from the start of `pending` the next step needs: a
    /// block header, then the whole block; 0 once the frame is finished.
    pub(crate) fn input_needed(&self, pending: &[u8]) -> usize {
        if self.finished {
            return 0;
        }
        match Block::read(pending) {
            Ok(block) => 3 + block.body_len(),
            Err(_) => 3,
        }
    }

    /// Decode the whole steps `src` holds into `target`, a block at a time,
    /// decoding no further while `target` cannot take what is decoded.
    /// Returns the bytes read and written.
    pub(crate) fn decode(
        &mut self,
        src: &[u8],
        target: &mut [u8],
    ) -> Result<(usize, usize), Error> {
        let mut read = 0;
        let mut written = 0;
        loop {
            written += self.hand_out(&mut target[written..]);
            if self.undelivered() > 0 || self.finished {
                break;
            }
            let rest = &src[read..];
            let need = self.input_needed(rest);
            if need > rest.len() {
                break;
            }
            self.step(&rest[..need])?;
            read += need;
        }
        Ok((read, written))
    }

    /// Hand out what `target` takes of the undelivered output.
    pub(crate) fn hand_out(&mut self, target: &mut [u8]) -> usize {
        let n = self.undelivered().min(target.len());
        target[..n].copy_from_slice(&self.buf[self.delivered..self.delivered + n]);
        self.delivered += n;
        n
    }

    /// The undelivered output, which [`Self::mark_delivered`] then hands out.
    pub(crate) fn undelivered_bytes(&self) -> &[u8] {
        &self.buf[self.delivered..]
    }

    /// Count the first `n` undelivered bytes as handed out.
    pub(crate) fn mark_delivered(&mut self, n: usize) {
        debug_assert!(n <= self.undelivered());
        self.delivered += n;
    }

    /// Read the next block from `source` and decode it.
    pub(crate) fn read_step<R: crate::io::Read>(
        &mut self,
        source: &mut R,
    ) -> Result<(), crate::decoding::errors::FrameDecoderError> {
        use crate::blocks::block::BlockType;
        use crate::decoding::errors::{
            BlockHeaderReadError, DecodeBlockContentError, FrameDecoderError,
        };
        // Lent out while the block decodes, which needs `self`.
        let mut block = core::mem::take(&mut self.block);
        block.clear();
        block.resize(3, 0);
        let result = source
            .read_exact(&mut block)
            .map_err(|source| {
                FrameDecoderError::FailedToReadBlockHeader(BlockHeaderReadError::ReadError(source))
            })
            .and_then(|()| {
                let need = self.input_needed(&block);
                block.resize(need, 0);
                source.read_exact(&mut block[3..]).map_err(|source| {
                    FrameDecoderError::FailedToReadBlockBody(DecodeBlockContentError::ReadError {
                        step: BlockType::Compressed,
                        source,
                    })
                })
            })
            .and_then(|()| Ok(self.step(&block)?));
        self.block = block;
        result
    }

    /// Decode one block, `step` holding its header and body.
    fn step(&mut self, step: &[u8]) -> Result<(), Error> {
        let LegacyStream {
            frame,
            dict,
            buf,
            dropped,
            delivered,
            keep,
            finished,
            consumed,
            block: _,
        } = self;
        let frame = frame.as_mut().ok_or(corrupt("no frame in progress"))?;
        let block = Block::read(step)?;
        *consumed += step.len() as u64;
        if ends_frame(frame.version, block) {
            frame.finish([step[0], step[1], step[2]])?;
            *finished = true;
            return Ok(());
        }
        // Drop history out of reach once it has been handed out, a block's
        // worth at a time so the move is paid once per block at most.
        if buf.len() > *keep + BLOCK_SIZE_MAX {
            let drop = (buf.len() - *keep).min(*delivered);
            buf.drain(..drop);
            *delivered -= drop;
            *dropped += drop;
        }
        let dict_bytes = dict
            .as_ref()
            .map_or(LegacyDict::None, |dict| LegacyDict::Parsed(dict.as_dict()))
            .content_bytes();
        let mut out = VecOutput {
            buf,
            start: 0,
            dropped: *dropped,
            limit: usize::MAX,
        };
        frame.block(block, &step[3..], &mut out, dict_bytes)
    }
}

#[cfg(test)]
mod tests;
