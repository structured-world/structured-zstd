//! The v0.7 format (upstream `lib/legacy/zstd_v07.c`): the last before 1.0,
//! with a content checksum, a dictionary ID, and repeat offsets that carry
//! from block to block.

use super::entropy::{
    Distribution, FseState, FseTable, HufTable, Slot, StreamEnd, open_bits, overflowed,
};
use super::{
    BLOCK_SIZE_MAX, FrameHeader, History, LegacyDict, LegacyVersion, LiteralSource, Literals,
    Output, corrupt, execute,
};
use crate::decoding::errors::LegacyFrameError as Error;
use alloc::vec::Vec;

/// Largest window a frame may ask for (`ZSTDv07_WINDOWLOG_MAX`).
const WINDOW_LOG_MAX: u32 = if usize::BITS == 32 { 25 } else { 27 };

const MAX_LL: usize = 35;
const MAX_ML: usize = 52;
const MAX_OFF: usize = 28;
const LL_LOG: u32 = 9;
const ML_LOG: u32 = 9;
const OFF_LOG: u32 = 8;

pub(super) const LL_BITS: [u8; MAX_LL + 1] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
pub(super) const LL_BASE: [u32; MAX_LL + 1] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    0x80, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000, 0x4000, 0x8000, 0x10000,
];
pub(super) const ML_BITS: [u8; MAX_ML + 1] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
/// Match length bases, the minimum match of 3 included.
pub(super) const ML_BASE: [u32; MAX_ML + 1] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 0x83, 0x103, 0x203,
    0x403, 0x803, 0x1003, 0x2003, 0x4003, 0x8003, 0x10003,
];
const OF_BASE: [u32; MAX_OFF + 1] = [
    0, 1, 1, 5, 0xD, 0x1D, 0x3D, 0x7D, 0xFD, 0x1FD, 0x3FD, 0x7FD, 0xFFD, 0x1FFD, 0x3FFD, 0x7FFD,
    0xFFFD, 0x1FFFD, 0x3FFFD, 0x7FFFD, 0xFFFFD, 0x1FFFFD, 0x3FFFFD, 0x7FFFFD, 0xFFFFFD, 0x1FFFFFD,
    0x3FFFFFD, 0x7FFFFFD, 0xFFFFFFD,
];

/// The predefined distributions, shared with v0.6.
pub(super) const LL_DEFAULT: [i16; MAX_LL + 1] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(super) const ML_DEFAULT: [i16; MAX_ML + 1] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
pub(super) const OF_DEFAULT: [i16; MAX_OFF + 1] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
pub(super) const LL_DEFAULT_LOG: u32 = 6;
pub(super) const ML_DEFAULT_LOG: u32 = 6;
pub(super) const OF_DEFAULT_LOG: u32 = 5;

/// Read a v0.7 frame header (`ZSTDv07_getFrameParams`).
pub(super) fn read_header(src: &[u8]) -> Result<FrameHeader, Error> {
    let &descriptor = src.get(4).ok_or(Error::Truncated)?;
    let dict_id_code = descriptor & 3;
    let checksum = descriptor & 4 != 0;
    let single_segment = descriptor & 0x20 != 0;
    let fcs_code = descriptor >> 6;
    let fcs_len = [0usize, 2, 4, 8][usize::from(fcs_code)];
    let size = 5
        + usize::from(!single_segment)
        + [0usize, 1, 2, 4][usize::from(dict_id_code)]
        + fcs_len
        + usize::from(single_segment && fcs_len == 0);
    let header = src.get(..size).ok_or(Error::Truncated)?;
    // The field's own code, as the stream reports it; a frame decoded whole
    // reports it as corruption (`read_header_whole`), as libzstd's one-shot
    // decoder does for every header error.
    if descriptor & 0x08 != 0 {
        return Err(Error::UnsupportedParameter);
    }
    let mut pos = 5;
    let mut window: u32 = 0;
    if !single_segment {
        let byte = header[pos];
        pos += 1;
        let log = u32::from(byte >> 3) + 10;
        if log > WINDOW_LOG_MAX {
            return Err(Error::UnsupportedParameter);
        }
        window = 1 << log;
        window += (window >> 3) * u32::from(byte & 7);
    }
    let le = |at: usize, len: usize| {
        header[at..at + len]
            .iter()
            .rev()
            .fold(0u64, |acc, &byte| acc << 8 | u64::from(byte))
    };
    let dict_id = match dict_id_code {
        0 => 0,
        1 => le(pos, 1) as u32,
        2 => le(pos, 2) as u32,
        _ => le(pos, 4) as u32,
    };
    pos += [0usize, 1, 2, 4][usize::from(dict_id_code)];
    let content_size = match fcs_code {
        0 if single_segment => le(pos, 1),
        0 => 0,
        1 => le(pos, 2) + 256,
        2 => le(pos, 4),
        _ => le(pos, 8),
    };
    if window == 0 {
        // A single-segment frame's window is its content, cut to 32 bits as
        // upstream's `windowSize` field holds it.
        window = content_size as u32;
    }
    if window > 1 << WINDOW_LOG_MAX {
        return Err(Error::UnsupportedParameter);
    }
    Ok(FrameHeader {
        version: LegacyVersion::V07,
        size,
        window: u64::from(window),
        content_size: (content_size != 0).then_some(content_size),
        dict_id,
        checksum,
    })
}

/// The literal length, offset and match length tables last in use, which a
/// block may keep instead of sending new ones.
pub(super) struct SeqTables {
    pub(super) ll: Slot<FseTable>,
    pub(super) of: Slot<FseTable>,
    pub(super) ml: Slot<FseTable>,
}

impl SeqTables {
    pub(super) const fn empty() -> SeqTables {
        SeqTables {
            ll: Slot::<FseTable>::empty(),
            of: Slot::<FseTable>::empty(),
            ml: Slot::<FseTable>::empty(),
        }
    }

    /// Set none of the tables, keeping their storage.
    pub(super) fn unset(&mut self) {
        self.ll.set = false;
        self.of.set = false;
        self.ml.set = false;
    }

    pub(super) fn heap_bytes(&self) -> usize {
        self.ll.table.heap_bytes() + self.of.table.heap_bytes() + self.ml.table.heap_bytes()
    }
}

/// The decoding state a v0.7 frame carries from block to block.
pub(super) struct Decoder {
    /// The last Huffman table, which repeat-table literals reuse.
    huf: Slot<HufTable>,
    tables: SeqTables,
    /// Whether a block may keep the previous tables: once a dictionary or a
    /// block with sequences has set them (`fseEntropy`).
    repeat_tables: bool,
    reps: [usize; 3],
    literals: Vec<u8>,
    /// ID of the dictionary in use, 0 for none.
    pub(super) dict_id: u32,
}

impl Decoder {
    pub(super) const fn empty() -> Decoder {
        Decoder {
            huf: Slot::<HufTable>::empty(),
            tables: SeqTables::empty(),
            repeat_tables: false,
            reps: [1, 4, 8],
            literals: Vec::new(),
            dict_id: 0,
        }
    }

    /// Ready the decoder for a frame primed with `dict`, in the storage the
    /// previous frame left, and say where the dictionary's content lies
    /// within its parsed content.
    pub(super) fn reset(&mut self, dict: LegacyDict<'_>) -> Result<core::ops::Range<usize>, Error> {
        self.huf.set = false;
        self.tables.unset();
        self.repeat_tables = false;
        self.reps = [1, 4, 8];
        self.literals.clear();
        self.dict_id = 0;
        match dict {
            LegacyDict::None => Ok(0..0),
            LegacyDict::Parsed { dict, .. } => {
                // A raw-content dictionary has no tables to load.
                if dict.huf.table.max_num_bits != 0 {
                    self.load_parsed(dict)?;
                }
                self.dict_id = dict.id;
                Ok(0..dict.dict_content.len())
            }
        }
    }

    /// Heap bytes the decoder holds.
    pub(super) fn heap_bytes(&self) -> usize {
        self.huf.table.heap_bytes() + self.tables.heap_bytes() + self.literals.capacity()
    }

    /// Load the tables and offsets a dictionary of the v0.7 layout primes,
    /// from what the RFC 8878 parser read of it: the two layouts are the same
    /// bytes, so the parsed distributions and code lengths are the ones
    /// `ZSTDv07_loadEntropy` would read, held to the v0.7 symbol limits.
    ///
    /// The repeat offsets are not held to the dictionary's size here, as the
    /// parser kept no count of its table bytes; one reaching past the content
    /// fails at the match that uses it.
    fn load_parsed(&mut self, dict: &crate::decoding::Dictionary) -> Result<(), Error> {
        let bad = |_| Error::DictionaryCorrupted;
        let huf_table = &dict.huf.table;
        self.huf
            .table
            .fill_from_code_lengths(huf_table.code_lengths(), huf_table.max_num_bits)
            .map_err(bad)?;
        self.huf.set = true;
        let load = |slot: &mut Slot<FseTable>,
                    fse: &crate::decoding::scratch::AlignedFSETable,
                    max: usize,
                    max_log: u32|
         -> Result<(), Error> {
            let log = u32::from(fse.accuracy_log);
            if log > max_log {
                return Err(Error::DictionaryCorrupted);
            }
            let dist = Distribution::from_probabilities(&fse.symbol_probabilities, log, max)
                .map_err(bad)?;
            slot.table.build(&dist).map_err(bad)?;
            slot.set = true;
            Ok(())
        };
        load(&mut self.tables.of, &dict.fse.offsets, MAX_OFF, OFF_LOG)?;
        load(&mut self.tables.ml, &dict.fse.match_lengths, MAX_ML, ML_LOG)?;
        load(
            &mut self.tables.ll,
            &dict.fse.literal_lengths,
            MAX_LL,
            LL_LOG,
        )?;
        for (rep, &offset) in self.reps.iter_mut().zip(&dict.offset_hist) {
            if offset == 0 {
                return Err(Error::DictionaryCorrupted);
            }
            *rep = offset as usize;
        }
        self.repeat_tables = true;
        Ok(())
    }

    /// Decode one compressed block into `out` (`ZSTDv07_decompressBlock_internal`).
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
                if !single_stream && (compressed >= size || compressed <= 1) {
                    return Err(corrupt("Huffman literals no smaller than their content"));
                }
                // Built in the previous table's storage; left unset should the
                // description be bad, so no later block repeats a half-built
                // table.
                self.huf.set = false;
                let table = &mut self.huf.table;
                let table_len = table
                    .read_into(src, StreamEnd::PastStart)
                    .map_err(|_| corrupt("bad Huffman table"))?;
                if table_len >= src.len() {
                    return Err(corrupt("Huffman table fills the literal section"));
                }
                self.literals.clear();
                self.literals.resize(size, 0);
                let streams = &src[table_len..];
                if single_stream {
                    table.decode_one_stream(streams, &mut self.literals)
                } else {
                    table.decode_four_streams(streams, &mut self.literals)
                }
                .map_err(|_| corrupt("bad Huffman literals"))?;
                self.huf.set = true;
                (LiteralSource::Buffer, header + compressed)
            }
            Literals::Repeat { size, compressed } => {
                let table = self.huf.get().ok_or(Error::DictionaryCorrupted)?;
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
            self.repeat_tables = true;
            let (ll, of, ml) = self.tables.all()?;

            let mut reps = self.reps;
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
                if of_code <= 1 {
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
            self.reps = reps;
        }
        let rest = &literals[lit_pos..];
        out.reserve(rest.len())?;
        out.extend(rest);
        Ok(())
    }
}

/// The sequence count at the start of a sequence section and the bytes it
/// takes (`ZSTDv07_decodeSeqHeaders`); `long_form` allows the three-byte
/// count v0.6 and v0.7 have.
pub(super) fn sequence_count(src: &[u8], long_form: bool) -> Result<(usize, usize), Error> {
    let &first = src.first().ok_or(Error::Truncated)?;
    let first = usize::from(first);
    Ok(if first <= 0x7F {
        (first, 1)
    } else if long_form && first == 0xFF {
        let bytes = src.get(1..3).ok_or(Error::Truncated)?;
        (
            usize::from(u16::from_le_bytes([bytes[0], bytes[1]])) + 0x7F00,
            3,
        )
    } else {
        let &second = src.get(1).ok_or(Error::Truncated)?;
        (((first - 0x80) << 8) + usize::from(second), 2)
    })
}

impl SeqTables {
    /// The three tables, all set once a block has used them.
    pub(super) fn all(&self) -> Result<(&FseTable, &FseTable, &FseTable), Error> {
        match (self.ll.get(), self.of.get(), self.ml.get()) {
            (Some(ll), Some(of), Some(ml)) => Ok((ll, of, ml)),
            _ => Err(corrupt("sequence table repeated before one was sent")),
        }
    }
}

/// Set the table one sequence field is coded with, by its mode
/// (`ZSTDv07_buildSeqTable`): `field` gives the largest symbol, the largest
/// table log, and the predefined distribution; the repeat mode keeps `slot`
/// as it is, where `repeat` allows it. Returns the bytes the mode's
/// description took.
pub(super) fn seq_table(
    slot: &mut Slot<FseTable>,
    mode: u8,
    src: &[u8],
    field: (usize, u32, &[i16], u32),
    repeat: bool,
) -> Result<usize, Error> {
    let (max, max_log, default, default_log) = field;
    // The previous table, in place.
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
        // Predefined.
        0 => {
            new.build(&Distribution::predefined(default, default_log))?;
            0
        }
        // One symbol throughout.
        1 => {
            let &symbol = src.first().ok_or(Error::Truncated)?;
            if usize::from(symbol) > max {
                return Err(corrupt("sequence symbol out of range"));
            }
            new.set_rle(symbol);
            1
        }
        _ => {
            let (dist, len) = super::entropy::read_distribution(src, max)
                .map_err(|_| corrupt("bad sequence table"))?;
            if dist.table_log() > max_log {
                return Err(corrupt("sequence table too large"));
            }
            new.build(&dist)?;
            len
        }
    };
    slot.set = true;
    Ok(len)
}
