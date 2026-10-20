//! The v0.5 format (upstream `lib/legacy/zstd_v05.c`): lengths coded as
//! symbols with long values in a separate "dumps" area, a single repeat
//! offset, and a minimum match of 4.

use super::entropy::{FseState, FseTable, HufTable, Slot, StreamEnd, open_bits, overflowed};
use super::v06::huffman;
use super::v07::{SeqTables, sequence_count};
use super::{
    BLOCK_SIZE_MAX, FrameHeader, History, LegacyVersion, LiteralSource, Literals, Output, corrupt,
    execute,
};
use crate::decoding::errors::LegacyFrameError as Error;
use alloc::vec::Vec;

/// Magic number of a v0.5 dictionary.
const DICT_MAGIC: u32 = 0xEC30_A435;

const LL_BITS: u32 = 6;
const ML_BITS: u32 = 7;
const OFF_BITS: u32 = 5;
const MAX_LL: usize = (1 << LL_BITS) - 1;
const MAX_ML: usize = (1 << ML_BITS) - 1;
const MAX_OFF: usize = (1 << OFF_BITS) - 1;
const LL_LOG: u32 = 10;
const ML_LOG: u32 = 10;
const OFF_LOG: u32 = 9;
const MIN_MATCH: usize = 4;

/// Offset code `n` reads `n - 1` bits on top of this; code 0 is the repeat
/// offset, and codes 27 to 31 are not used.
const OFFSET_PREFIX: [usize; MAX_OFF + 1] = [
    1, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072,
    262144, 524288, 1048576, 2097152, 4194304, 8388608, 16777216, 33554432, 1, 1, 1, 1, 1,
];

/// Read a v0.5 frame header (`ZSTDv05_getFrameParams`): five bytes, no
/// content size.
pub(super) fn read_header(src: &[u8]) -> Result<FrameHeader, Error> {
    let &descriptor = src.get(4).ok_or(Error::Truncated)?;
    let window_log = u32::from(descriptor & 0xF) + 11;
    if descriptor >> 4 != 0 || (usize::BITS == 32 && window_log > 25) {
        return Err(Error::UnsupportedParameter);
    }
    Ok(FrameHeader {
        version: LegacyVersion::V05,
        size: 5,
        window: 1 << window_log,
        content_size: None,
        dict_id: 0,
        checksum: false,
    })
}

/// The decoding state a v0.5 frame carries from block to block.
pub(super) struct Decoder {
    /// The dictionary's Huffman table, which repeat-table literals use.
    dict_huf: Slot<HufTable>,
    /// The table a block's own Huffman description is read into.
    huf: HufTable,
    tables: SeqTables,
    /// Whether a block may keep the previous tables: once a dictionary has
    /// been loaded (`flagStaticTables`).
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
        // `ZSTDv05_loadEntropy`.
        let src = &bytes[4..];
        let bad = |_| Error::DictionaryCorrupted;
        let mut at = self
            .dict_huf
            .table
            .read_into(src, StreamEnd::Exact)
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

    /// Decode one compressed block into `out` (`ZSTDv05_decompressBlock_internal`).
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
                        StreamEnd::Exact,
                    )?;
                } else {
                    // `HUFv05_decompress`: an uncompressed section is not
                    // Huffman literals here, a one-byte section is a run.
                    if size == 0 || compressed >= size {
                        return Err(corrupt("Huffman literals no smaller than their content"));
                    }
                    if compressed == 1 {
                        self.literals.resize(size, src[0]);
                    } else {
                        self.literals.resize(size, 0);
                        huffman(
                            &mut self.huf,
                            src,
                            &mut self.literals,
                            true,
                            StreamEnd::Exact,
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
        let (count, mut at) = sequence_count(src, false)?;
        let mut lit_pos = 0;
        if count > 0 {
            // `ZSTDv05_decodeSeqHeaders`: the field modes, then the dumps.
            let &modes = src.get(at).ok_or(Error::Truncated)?;
            let dumps_len;
            if modes & 2 != 0 {
                let bytes = src.get(at + 1..at + 3).ok_or(Error::Truncated)?;
                dumps_len = usize::from(bytes[1]) + (usize::from(bytes[0]) << 8);
                at += 3;
            } else {
                let &low = src.get(at + 1).ok_or(Error::Truncated)?;
                dumps_len = usize::from(low) + (usize::from(modes & 1) << 8);
                at += 2;
            }
            let dumps_start = at;
            at += dumps_len;
            if at + 3 > src.len() {
                return Err(Error::Truncated);
            }
            let dumps_end = at;
            let repeat = self.repeat_tables;
            at += field_table(
                &mut self.tables.ll,
                modes >> 6,
                src,
                at,
                Field::LITERAL_LENGTH,
                repeat,
            )?;
            at += field_table(
                &mut self.tables.of,
                (modes >> 4) & 3,
                src,
                at,
                Field::OFFSET,
                repeat,
            )?;
            at += field_table(
                &mut self.tables.ml,
                (modes >> 2) & 3,
                src,
                at,
                Field::MATCH_LENGTH,
                repeat,
            )?;
            let (ll, of, ml) = self.tables.all()?;

            let mut bits = open_bits(&src[at..]).map_err(|_| corrupt("bad sequence bitstream"))?;
            let mut ll_state = FseState::new(ll, &mut bits);
            let mut of_state = FseState::new(of, &mut bits);
            let mut ml_state = FseState::new(ml, &mut bits);
            let mut dumps = Dumps {
                src,
                at: dumps_start,
                end: dumps_end,
            };
            // The previous sequence's offset and the one before a literal-
            // free sequence, both starting at 1 in every block.
            let mut last_offset = 1usize;
            let mut prev_offset = 1usize;
            for _ in 0..count {
                if overflowed(&bits) {
                    return Err(corrupt("sequence bitstream ends early"));
                }
                // `ZSTDv05_decodeSequence`.
                let mut lit_len = usize::from(ll_state.symbol(ll));
                let repeat_offset = if lit_len != 0 {
                    last_offset
                } else {
                    prev_offset
                };
                if lit_len == MAX_LL {
                    lit_len = dumps.extend(lit_len, true);
                }
                let code = usize::from(of_state.symbol(of));
                let extra = if code == 0 { 0 } else { code as u8 - 1 };
                let mut offset = OFFSET_PREFIX[code] + bits.get_bits(extra) as usize;
                if code == 0 {
                    offset = repeat_offset;
                }
                if code != 0 || lit_len == 0 {
                    prev_offset = last_offset;
                }
                of_state.update(of, &mut bits);
                ll_state.update(ll, &mut bits);
                let mut match_len = usize::from(ml_state.symbol(ml));
                ml_state.update(ml, &mut bits);
                if match_len == MAX_ML {
                    match_len = dumps.extend(match_len, false);
                }
                match_len += MIN_MATCH;
                last_offset = offset;
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

/// The largest symbol, largest table log and raw width of one sequence field.
struct Field {
    max: usize,
    max_log: u32,
    raw_bits: u32,
    /// Whether a one-symbol table's symbol is masked to the field and needs
    /// a byte after it (offsets and match lengths), or taken as is (literal
    /// lengths).
    checked_rle: bool,
}

impl Field {
    const LITERAL_LENGTH: Field = Field {
        max: MAX_LL,
        max_log: LL_LOG,
        raw_bits: LL_BITS,
        checked_rle: false,
    };
    const OFFSET: Field = Field {
        max: MAX_OFF,
        max_log: OFF_LOG,
        raw_bits: OFF_BITS,
        checked_rle: true,
    };
    const MATCH_LENGTH: Field = Field {
        max: MAX_ML,
        max_log: ML_LOG,
        raw_bits: ML_BITS,
        checked_rle: true,
    };
}

/// Set one field's table by its mode, the description starting at
/// `src[at..]` (`ZSTDv05_decodeSeqHeaders`). Returns the bytes it took.
fn field_table(
    slot: &mut Slot<FseTable>,
    mode: u8,
    src: &[u8],
    at: usize,
    field: Field,
    repeat: bool,
) -> Result<usize, Error> {
    if mode == 2 {
        if !repeat || !slot.set {
            return Err(corrupt("sequence table repeated before one was sent"));
        }
        return Ok(0);
    }
    // Built in the previous table's storage; left unset should the
    // description be bad, so no later block repeats a half-built table.
    slot.set = false;
    let new = &mut slot.table;
    let len = match mode {
        // Every symbol read as raw bits.
        0 => {
            new.set_raw(field.raw_bits);
            0
        }
        1 => {
            if field.checked_rle && at + 2 > src.len() {
                return Err(Error::Truncated);
            }
            let &symbol = src.get(at).ok_or(Error::Truncated)?;
            // Only offsets are masked; a literal or match length symbol past
            // its field is taken as it stands.
            let symbol = if field.max == MAX_OFF {
                symbol & MAX_OFF as u8
            } else {
                symbol
            };
            new.set_rle(symbol);
            1
        }
        _ => {
            let (dist, len) = super::entropy::read_distribution(&src[at..], field.max)
                .map_err(|_| corrupt("bad sequence table"))?;
            if dist.table_log() > field.max_log {
                return Err(corrupt("sequence table too large"));
            }
            new.build(&dist)?;
            len
        }
    };
    slot.set = true;
    Ok(len)
}

/// The dumps area, read forward for lengths past a field's largest symbol.
struct Dumps<'s> {
    src: &'s [u8],
    at: usize,
    end: usize,
}

impl Dumps<'_> {
    /// The length a symbol of `base` (the field's largest) stands for: `base`
    /// plus one dumps byte, or, when that byte is 255, a value of 16 or 24
    /// bits halved. Reads as upstream does, including its clamp back to the
    /// last dumps byte; the area sits inside the sequence section, ahead of
    /// at least three more bytes, so every read stays in `src`.
    fn extend(&mut self, base: usize, literal: bool) -> usize {
        // A literal length always takes its byte; a match length only while
        // the area has one.
        let add = if literal || self.at < self.end {
            let byte = usize::from(self.src[self.at]);
            self.at += 1;
            byte
        } else {
            0
        };
        let mut value = base;
        if add < 255 {
            value += add;
        } else if self.at + 2 <= self.end {
            value = usize::from(u16::from_le_bytes([
                self.src[self.at],
                self.src[self.at + 1],
            ]));
            self.at += 2;
            if value & 1 != 0 && self.at < self.end {
                value += usize::from(self.src[self.at]) << 16;
                self.at += 1;
            }
            value >>= 1;
        }
        if self.at >= self.end {
            self.at = self.end - 1;
        }
        value
    }
}
