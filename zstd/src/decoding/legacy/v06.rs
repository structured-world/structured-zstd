//! The v0.6 format (upstream `lib/legacy/zstd_v06.c`): v0.7's coding without
//! a dictionary ID or checksum, repeat offsets that restart every block, and
//! offsets coded two higher.

use super::entropy::{FseState, HufTable, Slot, StreamEnd, open_bits, overflowed};
use super::v07::{
    LL_BASE, LL_BITS, LL_DEFAULT, LL_DEFAULT_LOG, ML_BASE, ML_BITS, ML_DEFAULT, ML_DEFAULT_LOG,
    OF_DEFAULT, OF_DEFAULT_LOG, SeqTables, seq_table, sequence_count,
};
use super::{
    BLOCK_SIZE_MAX, FrameHeader, History, LegacyVersion, LiteralSource, Literals, Output, corrupt,
    execute,
};
use crate::decoding::errors::LegacyFrameError as Error;
use alloc::vec::Vec;

/// Magic number of a v0.6 dictionary.
pub(super) const DICT_MAGIC: u32 = 0xEC30_A436;

const MAX_LL: usize = 35;
const MAX_ML: usize = 52;
const MAX_OFF: usize = 28;
const LL_LOG: u32 = 9;
const ML_LOG: u32 = 9;
const OFF_LOG: u32 = 8;

/// Offset bases; codes 27 and 28 are not used and read as 1.
const OF_BASE: [u32; MAX_OFF + 1] = [
    0, 1, 3, 7, 0xF, 0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF,
    0xFFFF, 0x1FFFF, 0x3FFFF, 0x7FFFF, 0xFFFFF, 0x1FFFFF, 0x3FFFFF, 0x7FFFFF, 0xFFFFFF, 0x1FFFFFF,
    0x3FFFFFF, 1, 1,
];

/// Read a v0.6 frame header (`ZSTDv06_getFrameParams`).
pub(super) fn read_header(src: &[u8]) -> Result<FrameHeader, Error> {
    let &descriptor = src.get(4).ok_or(Error::Truncated)?;
    let fcs_len = [0usize, 1, 2, 8][usize::from(descriptor >> 6)];
    let header = src.get(..5 + fcs_len).ok_or(Error::Truncated)?;
    // Descriptor bits (upstream zstd_v06.c:2826-2832): 0-3 window log less
    // 12, 4 a minimum-match flag, 5 reserved, 6-7 the content size field.
    // The window field is four bits, 27 at most. Bit 4 is read by no
    // decoder and written by no encoder, so a frame setting it decodes, as
    // in upstream (zstd_v06.c:2937-2938); only bit 5 is refused, with the
    // stream's code; decoded whole it is corruption (`read_header_whole`).
    let window_log = u32::from(descriptor & 0xF) + 12;
    if descriptor & 0x20 != 0 || (usize::BITS == 32 && window_log > 25) {
        return Err(Error::UnsupportedParameter);
    }
    let fcs = &header[5..];
    let content_size = match descriptor >> 6 {
        0 => 0,
        1 => u64::from(fcs[0]),
        2 => u64::from(u16::from_le_bytes([fcs[0], fcs[1]])) + 256,
        _ => u64::from_le_bytes(fcs.try_into().expect("eight bytes")),
    };
    Ok(FrameHeader {
        version: LegacyVersion::V06,
        size: header.len(),
        window: 1 << window_log,
        content_size: (content_size != 0).then_some(content_size),
        dict_id: 0,
        checksum: false,
    })
}

/// The decoding state a v0.6 frame carries from block to block.
pub(super) struct Decoder {
    /// The dictionary's Huffman table, which repeat-table literals use.
    dict_huf: Slot<HufTable>,
    /// The table a block's own Huffman description is read into.
    huf: HufTable,
    tables: SeqTables,
    /// Whether the next block may use the dictionary's tables: from loading a
    /// dictionary until the first sequence section is read
    /// (`flagRepeatTable`).
    repeat_tables: bool,
    literals: Vec<u8>,
}

impl Decoder {
    pub(super) const fn empty() -> Decoder {
        Decoder {
            dict_huf: Slot::<HufTable>::empty(),
            huf: HufTable::empty(),
            tables: SeqTables::empty(),
            repeat_tables: false,
            literals: Vec::new(),
        }
    }

    /// Ready the decoder for a frame primed with the dictionary `bytes`, in
    /// the storage the previous frame left, and say where its content lies in
    /// them.
    pub(super) fn reset(&mut self, bytes: &[u8]) -> Result<core::ops::Range<usize>, Error> {
        self.dict_huf.set = false;
        self.tables.unset();
        self.repeat_tables = false;
        self.literals.clear();
        if bytes.len() < 4
            || u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != DICT_MAGIC
        {
            return Ok(0..bytes.len());
        }
        // `ZSTDv06_loadEntropy`.
        let src = &bytes[4..];
        let bad = |_| Error::DictionaryCorrupted;
        let mut at = self
            .dict_huf
            .table
            .read_into(src, StreamEnd::PastStart)
            .map_err(bad)?;
        for (slot, max, max_log) in [
            (&mut self.tables.of, MAX_OFF, OFF_LOG),
            (&mut self.tables.ml, MAX_ML, ML_LOG),
            (&mut self.tables.ll, MAX_LL, LL_LOG),
        ] {
            let (dist, len) = super::entropy::read_distribution(
                src.get(at..).ok_or(Error::DictionaryCorrupted)?,
                max,
            )
            .map_err(bad)?;
            if dist.table_log() > max_log {
                return Err(Error::DictionaryCorrupted);
            }
            slot.table.build(&dist).map_err(bad)?;
            slot.set = true;
            at += len;
        }
        self.dict_huf.set = true;
        self.repeat_tables = true;
        Ok(4 + at..bytes.len())
    }

    /// Heap bytes the decoder holds.
    pub(super) fn heap_bytes(&self) -> usize {
        self.dict_huf.table.heap_bytes()
            + self.huf.heap_bytes()
            + self.tables.heap_bytes()
            + self.literals.capacity()
    }

    /// Decode one compressed block into `out` (`ZSTDv06_decompressBlock_internal`).
    pub(super) fn block<O: Output>(
        &mut self,
        block: &[u8],
        out: &mut O,
        dict: History<'_>,
    ) -> Result<(), Error> {
        if block.len() >= BLOCK_SIZE_MAX {
            return Err(corrupt("compressed block too large"));
        }
        let (source, consumed) = self.literals(block)?;
        // The buffer is lent out for the sequence pass, which also needs
        // `self` for its tables, and handed back after it.
        let buffer = core::mem::take(&mut self.literals);
        let literals = match &source {
            LiteralSource::Block(range) => &block[range.clone()],
            LiteralSource::Buffer => &buffer[..],
        };
        let result = self.sequences(&block[consumed..], literals, out, dict);
        self.literals = buffer;
        result
    }

    fn literals(&mut self, block: &[u8]) -> Result<(LiteralSource, usize), Error> {
        Ok(match Literals::read(block)? {
            Literals::Huffman {
                header,
                size,
                compressed,
                single_stream,
            } => {
                let src = &block[header..header + compressed];
                self.literals.clear();
                if single_stream {
                    self.literals.resize(size, 0);
                    huffman(
                        &mut self.huf,
                        src,
                        &mut self.literals,
                        false,
                        StreamEnd::PastStart,
                    )?;
                } else {
                    // `HUFv06_decompress`: a section as long as its content
                    // is the content, a one-byte section is a run of it.
                    if size == 0 || compressed > size {
                        return Err(corrupt("Huffman literals larger than their content"));
                    }
                    if compressed == size {
                        self.literals.extend_from_slice(src);
                    } else if compressed == 1 {
                        self.literals.resize(size, src[0]);
                    } else {
                        self.literals.resize(size, 0);
                        huffman(
                            &mut self.huf,
                            src,
                            &mut self.literals,
                            true,
                            StreamEnd::PastStart,
                        )?;
                    }
                }
                (LiteralSource::Buffer, header + compressed)
            }
            Literals::Repeat { size, compressed } => {
                if !self.repeat_tables {
                    return Err(Error::DictionaryCorrupted);
                }
                let table = self.dict_huf.get().ok_or(Error::DictionaryCorrupted)?;
                self.literals.clear();
                self.literals.resize(size, 0);
                table
                    .decode_one_stream(&block[3..3 + compressed], &mut self.literals)
                    .map_err(|_| corrupt("bad Huffman literals"))?;
                (LiteralSource::Buffer, 3 + compressed)
            }
            Literals::Raw { header, size } => {
                (LiteralSource::Block(header..header + size), header + size)
            }
            Literals::Rle { header, size } => {
                self.literals.clear();
                self.literals.resize(size, block[header]);
                (LiteralSource::Buffer, header + 1)
            }
        })
    }

    fn sequences<O: Output>(
        &mut self,
        src: &[u8],
        literals: &[u8],
        out: &mut O,
        dict: History<'_>,
    ) -> Result<(), Error> {
        let (count, mut at) = sequence_count(src, true)?;
        let mut lit_pos = 0;
        if count > 0 {
            if at + 4 > src.len() {
                return Err(Error::Truncated);
            }
            let modes = src[at];
            at += 1;
            let repeat = self.repeat_tables;
            at += seq_table(
                &mut self.tables.ll,
                modes >> 6,
                &src[at..],
                (MAX_LL, LL_LOG, &LL_DEFAULT, LL_DEFAULT_LOG),
                repeat,
            )?;
            at += seq_table(
                &mut self.tables.of,
                (modes >> 4) & 3,
                &src[at..],
                (MAX_OFF, OFF_LOG, &OF_DEFAULT, OF_DEFAULT_LOG),
                repeat,
            )?;
            at += seq_table(
                &mut self.tables.ml,
                (modes >> 2) & 3,
                &src[at..],
                (MAX_ML, ML_LOG, &ML_DEFAULT, ML_DEFAULT_LOG),
                repeat,
            )?;
        }
        // Read once, the dictionary's tables are no longer on offer, even to
        // a block that sent none.
        self.repeat_tables = false;
        if count > 0 {
            let (ll, of, ml) = self.tables.all()?;
            let mut reps = [1usize; 3];
            let mut bits = open_bits(&src[at..]).map_err(|_| corrupt("bad sequence bitstream"))?;
            let mut ll_state = FseState::new(ll, &mut bits);
            let mut of_state = FseState::new(of, &mut bits);
            let mut ml_state = FseState::new(ml, &mut bits);
            for _ in 0..count {
                if overflowed(&bits) {
                    return Err(corrupt("sequence bitstream ends early"));
                }
                let ll_code = usize::from(ll_state.symbol(ll));
                let ml_code = usize::from(ml_state.symbol(ml));
                let of_code = of_state.symbol(of);
                let mut offset = if of_code == 0 {
                    0
                } else {
                    OF_BASE[usize::from(of_code)] as usize + bits.get_bits(of_code) as usize
                };
                if offset < 3 {
                    if ll_code == 0 && offset <= 1 {
                        offset = 1 - offset;
                    }
                    if offset != 0 {
                        let picked = reps[offset];
                        if offset != 1 {
                            reps[2] = reps[1];
                        }
                        reps[1] = reps[0];
                        reps[0] = picked;
                        offset = picked;
                    } else {
                        offset = reps[0];
                    }
                } else {
                    offset -= 2;
                    reps[2] = reps[1];
                    reps[1] = reps[0];
                    reps[0] = offset;
                }
                let match_len =
                    ML_BASE[ml_code] as usize + bits.get_bits(ML_BITS[ml_code]) as usize;
                let lit_len = LL_BASE[ll_code] as usize + bits.get_bits(LL_BITS[ll_code]) as usize;
                ll_state.update(ll, &mut bits);
                ml_state.update(ml, &mut bits);
                of_state.update(of, &mut bits);
                execute(
                    out,
                    literals,
                    &mut lit_pos,
                    lit_len,
                    match_len,
                    offset,
                    dict,
                )?;
            }
        }
        let rest = &literals[lit_pos..];
        out.reserve(rest.len())?;
        out.extend(rest);
        Ok(())
    }
}

/// Decode Huffman literals with the table their section starts with, read
/// into `table`, into `out` (`HUFv06_decompress1X2` / `HUFv06_decompress4X2`);
/// `end` is how the format ends the table's FSE-coded weights.
pub(super) fn huffman(
    table: &mut HufTable,
    src: &[u8],
    out: &mut [u8],
    four_streams: bool,
    end: StreamEnd,
) -> Result<(), Error> {
    let table_len = table
        .read_into(src, end)
        .map_err(|_| corrupt("bad Huffman table"))?;
    if table_len >= src.len() {
        return Err(corrupt("Huffman table fills the literal section"));
    }
    let streams = &src[table_len..];
    if four_streams {
        table.decode_four_streams(streams, out)
    } else {
        table.decode_one_stream(streams, out)
    }
    .map_err(|_| corrupt("bad Huffman literals"))
}
