//! Entropy coding shared by the v0.5, v0.6 and v0.7 formats: the backward
//! bitstream, FSE tables and their compact description, and Huffman literals.
//!
//! The three formats agree on all of it (upstream `lib/legacy/zstd_v0{5,6,7}.c`
//! carry the same code), and it differs from RFC 8878 in details that matter
//! to a decoder: a Huffman description may be a run of weight 1, the largest
//! FSE table has 4096 states, and a table may be a fixed-width read.

use super::corrupt;
use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::LegacyFrameError as Error;
use alloc::vec;
use alloc::vec::Vec;

/// A backward bitstream, read from its last byte toward its first.
pub(super) type Bits<'s> = BitReaderReversed<'s>;

/// Largest FSE table the formats build: 4096 states (`FSEv07_MAX_TABLELOG`).
const FSE_MAX_TABLE_LOG: u32 = 12;
/// Smallest table log a description encodes (`FSEv07_MIN_TABLELOG`).
const FSE_MIN_TABLE_LOG: i32 = 5;
/// Largest table log a description may name (`FSEv07_TABLELOG_ABSOLUTE_MAX`).
const FSE_DESCRIPTION_MAX_LOG: i32 = 15;
/// Largest Huffman code length a table may use (`HUFv07_TABLELOG_MAX`).
const HUF_MAX_TABLE_LOG: u32 = 12;
/// Weights at or past this value are refused (`HUFv07_TABLELOG_ABSOLUTEMAX`).
const HUF_WEIGHT_LIMIT: u8 = 16;

/// Open a backward bitstream over `src`, past the padding and the end mark
/// in its last byte.
pub(super) fn open_bits(src: &[u8]) -> Result<Bits<'_>, Error> {
    let &last = src.last().ok_or(corrupt("empty bitstream"))?;
    if last == 0 {
        return Err(corrupt("bitstream without an end mark"));
    }
    let mut bits = Bits::new(src);
    bits.get_bits(last.leading_zeros() as u8 + 1);
    Ok(bits)
}

/// Whether `bits` has been read past its first byte.
#[inline]
pub(super) fn overflowed(bits: &Bits<'_>) -> bool {
    bits.bits_remaining() < 0
}

/// A normalised symbol distribution, as an FSE table description encodes it:
/// `-1` marks a symbol below one state.
pub(super) struct Distribution {
    counts: [i16; 256],
    /// Highest symbol the description lists.
    max_symbol: usize,
    table_log: u32,
}

/// Read an FSE table description (`FSEv07_readNCount`), listing at most
/// `max_symbol + 1` symbols. Returns the distribution and the bytes read.
pub(super) fn read_distribution(
    src: &[u8],
    max_symbol: usize,
) -> Result<(Distribution, usize), Error> {
    let end = src.len();
    if end < 4 {
        return Err(Error::Truncated);
    }
    let read32 = |at: usize| u32::from_le_bytes([src[at], src[at + 1], src[at + 2], src[at + 3]]);
    let mut counts = [0i16; 256];
    let mut ip = 0usize;
    let mut stream = read32(0);
    let mut nb_bits = (stream & 0xF) as i32 + FSE_MIN_TABLE_LOG;
    if nb_bits > FSE_DESCRIPTION_MAX_LOG {
        return Err(corrupt("FSE table log too large"));
    }
    let table_log = nb_bits as u32;
    stream >>= 4;
    let mut bit_count: i32 = 4;
    let mut remaining: i32 = (1 << nb_bits) + 1;
    let mut threshold: i32 = 1 << nb_bits;
    nb_bits += 1;
    let mut symbol = 0usize;
    let mut previous_zero = false;

    while remaining > 1 && symbol <= max_symbol {
        if previous_zero {
            let mut run_end = symbol;
            while stream & 0xFFFF == 0xFFFF {
                run_end += 24;
                if ip + 5 < end {
                    ip += 2;
                    stream = read32(ip).wrapping_shr(bit_count as u32);
                } else {
                    stream >>= 16;
                    bit_count += 16;
                }
            }
            while stream & 3 == 3 {
                run_end += 3;
                stream >>= 2;
                bit_count += 2;
            }
            run_end += (stream & 3) as usize;
            bit_count += 2;
            if run_end > max_symbol {
                return Err(corrupt("FSE description lists too many symbols"));
            }
            // `counts` was zeroed; the run only moves past its symbols.
            symbol = run_end;
            if ip + 7 <= end || ip + (bit_count >> 3) as usize + 4 <= end {
                ip += (bit_count >> 3) as usize;
                bit_count &= 7;
                stream = read32(ip).wrapping_shr(bit_count as u32);
            } else {
                stream >>= 2;
            }
        }
        let max = (2 * threshold - 1) - remaining;
        let mut count;
        if ((stream & (threshold as u32 - 1)) as i32) < max {
            count = (stream & (threshold as u32 - 1)) as i32;
            bit_count += nb_bits - 1;
        } else {
            count = (stream & (2 * threshold as u32 - 1)) as i32;
            if count >= threshold {
                count -= max;
            }
            bit_count += nb_bits;
        }
        count -= 1;
        remaining -= count.abs();
        // `count` is at most `remaining` before the subtraction, so at least
        // one state is always left.
        debug_assert!(remaining >= 1);
        counts[symbol] = count as i16;
        symbol += 1;
        previous_zero = count == 0;
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
        if ip + 7 <= end || ip + (bit_count >> 3) as usize + 4 <= end {
            ip += (bit_count >> 3) as usize;
            bit_count &= 7;
        } else {
            bit_count -= 8 * (end - 4 - ip) as i32;
            ip = end - 4;
        }
        stream = read32(ip).wrapping_shr((bit_count & 31) as u32);
    }
    if remaining != 1 {
        return Err(corrupt("FSE description does not fill its table"));
    }
    let consumed = ip + ((bit_count + 7) >> 3) as usize;
    if consumed > end {
        return Err(Error::Truncated);
    }
    Ok((
        Distribution {
            counts,
            max_symbol: symbol - 1,
            table_log,
        },
        consumed,
    ))
}

impl Distribution {
    /// A predefined distribution, as the formats' default tables list them.
    pub(super) fn predefined(counts: &[i16], table_log: u32) -> Distribution {
        let mut all = [0i16; 256];
        all[..counts.len()].copy_from_slice(counts);
        Distribution {
            counts: all,
            max_symbol: counts.len() - 1,
            table_log,
        }
    }

    /// The distribution an RFC 8878 table description parsed to, listing at
    /// most `max_symbol + 1` symbols.
    pub(super) fn from_probabilities(
        probabilities: &[i32],
        table_log: u32,
        max_symbol: usize,
    ) -> Result<Distribution, Error> {
        if probabilities.is_empty() || probabilities.len() > max_symbol + 1 {
            return Err(corrupt("FSE description lists too many symbols"));
        }
        let mut counts = [0i16; 256];
        for (count, &probability) in counts.iter_mut().zip(probabilities) {
            *count = i16::try_from(probability).map_err(|_| corrupt("FSE count out of range"))?;
        }
        Ok(Distribution {
            counts,
            max_symbol: probabilities.len() - 1,
            table_log,
        })
    }

    pub(super) fn table_log(&self) -> u32 {
        self.table_log
    }
}

#[derive(Clone, Copy, Default)]
struct FseCell {
    new_state: u16,
    symbol: u8,
    nb_bits: u8,
}

/// An FSE decoding table.
#[derive(Clone)]
pub(super) struct FseTable {
    log: u32,
    cells: Vec<FseCell>,
    /// Whether every state reads at least one bit: no symbol holds half the
    /// table or more (`fastMode`).
    fast: bool,
}

/// How an FSE-coded byte string marks its end.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamEnd {
    /// The decoder reads until a read passes the stream's first bit, then
    /// takes one more symbol from the other state (v0.6, v0.7).
    PastStart,
    /// The stream ends exactly at its first bit with both states at 0
    /// (v0.5, `FSEv05_decompress_usingDTable_generic`).
    Exact,
}

impl FseTable {
    /// The table a distribution spreads (`FSEv07_buildDTable`).
    pub(super) fn new(dist: &Distribution) -> Result<FseTable, Error> {
        let log = dist.table_log;
        if log > FSE_MAX_TABLE_LOG {
            return Err(corrupt("FSE table log too large"));
        }
        let size = 1usize << log;
        let counts = &dist.counts[..=dist.max_symbol];
        let mut cells = vec![FseCell::default(); size];
        let mut next = [0u16; 256];

        // Symbols below one state take the top cells, one each.
        let mut high = size;
        for (symbol, &count) in counts.iter().enumerate() {
            if count == -1 {
                high = high
                    .checked_sub(1)
                    .ok_or(corrupt("FSE distribution overfills its table"))?;
                cells[high].symbol = symbol as u8;
                next[symbol] = 1;
            } else {
                next[symbol] = count.max(0) as u16;
            }
        }

        let mask = size - 1;
        let step = (size >> 1) + (size >> 3) + 3;
        let mut position = 0usize;
        for (symbol, &count) in counts.iter().enumerate() {
            for _ in 0..count.max(0) {
                if high == 0 {
                    return Err(corrupt("FSE distribution overfills its table"));
                }
                cells[position].symbol = symbol as u8;
                position = (position + step) & mask;
                while position >= high {
                    position = (position + step) & mask;
                }
            }
        }
        // A distribution that sums to the table size reaches every cell once.
        if position != 0 {
            return Err(corrupt("FSE distribution does not fill its table"));
        }

        for cell in &mut cells {
            let state = next[usize::from(cell.symbol)];
            next[usize::from(cell.symbol)] += 1;
            let nb_bits = log - (31 - u32::from(state).leading_zeros());
            cell.nb_bits = nb_bits as u8;
            cell.new_state = ((u32::from(state) << nb_bits) - size as u32) as u16;
        }
        let large = 1i16 << (log - 1);
        let fast = counts.iter().all(|&count| count < large);
        Ok(FseTable { log, cells, fast })
    }

    /// A table reading every symbol as `nb_bits` raw bits (`FSEv07_buildDTable_raw`).
    pub(super) fn raw(nb_bits: u32) -> FseTable {
        let cells = (0..1usize << nb_bits)
            .map(|symbol| FseCell {
                new_state: 0,
                symbol: symbol as u8,
                nb_bits: nb_bits as u8,
            })
            .collect();
        FseTable {
            log: nb_bits,
            cells,
            fast: true,
        }
    }

    /// A table producing `symbol` without reading (`FSEv07_buildDTable_rle`).
    pub(super) fn rle(symbol: u8) -> FseTable {
        FseTable {
            log: 0,
            cells: vec![FseCell {
                new_state: 0,
                symbol,
                nb_bits: 0,
            }],
            fast: false,
        }
    }
}

/// A position in an [`FseTable`].
pub(super) struct FseState {
    state: usize,
}

impl FseState {
    pub(super) fn new(table: &FseTable, bits: &mut Bits<'_>) -> FseState {
        FseState {
            state: bits.get_bits(table.log as u8) as usize,
        }
    }

    /// The symbol at the current state.
    #[inline]
    pub(super) fn symbol(&self, table: &FseTable) -> u8 {
        table.cells[self.state].symbol
    }

    /// Move to the next state, reading the bits the current one calls for.
    #[inline]
    pub(super) fn update(&mut self, table: &FseTable, bits: &mut Bits<'_>) {
        let cell = table.cells[self.state];
        // A cell's `new_state` plus any value of its `nb_bits` stays inside
        // the table by construction.
        self.state = usize::from(cell.new_state) + bits.get_bits(cell.nb_bits) as usize;
    }
}

/// Decode an FSE-compressed byte string of at most `limit` symbols
/// (`FSEv07_decompress`): a description, then two interleaved states.
fn fse_decompress(
    src: &[u8],
    out: &mut [u8; 256],
    limit: usize,
    end: StreamEnd,
) -> Result<usize, Error> {
    if src.len() < 2 {
        return Err(Error::Truncated);
    }
    let (dist, description_len) = read_distribution(src, 255)?;
    if description_len >= src.len() {
        return Err(Error::Truncated);
    }
    let table = FseTable::new(&dist)?;
    let mut bits = open_bits(&src[description_len..])?;
    let mut first = FseState::new(&table, &mut bits);
    let mut second = FseState::new(&table, &mut bits);
    if end == StreamEnd::Exact {
        return decode_to_exact_end(&table, &mut bits, [first, second], out, limit);
    }
    let mut n = 0usize;
    loop {
        if n + 2 > limit {
            return Err(corrupt("FSE stream decodes past its output"));
        }
        out[n] = first.symbol(&table);
        n += 1;
        first.update(&table, &mut bits);
        if overflowed(&bits) {
            out[n] = second.symbol(&table);
            return Ok(n + 1);
        }
        if n + 2 > limit {
            return Err(corrupt("FSE stream decodes past its output"));
        }
        out[n] = second.symbol(&table);
        n += 1;
        second.update(&table, &mut bits);
        if overflowed(&bits) {
            out[n] = first.symbol(&table);
            return Ok(n + 1);
        }
    }
}

/// The v0.5 ending: alternate the two states until the stream is read to its
/// first bit and the state about to decode is at 0 (in a table where every
/// state reads a bit, as soon as the stream is read); the string is whole
/// only if the stream ended exactly with both states at 0.
fn decode_to_exact_end(
    table: &FseTable,
    bits: &mut Bits<'_>,
    mut states: [FseState; 2],
    out: &mut [u8; 256],
    limit: usize,
) -> Result<usize, Error> {
    let mut n = 0usize;
    let mut turn = 0;
    loop {
        let state = &mut states[turn];
        let exhausted = bits.bits_remaining() == 0;
        if overflowed(bits) || n == limit || (exhausted && (table.fast || state.state == 0)) {
            break;
        }
        out[n] = state.symbol(table);
        n += 1;
        state.update(table, bits);
        turn ^= 1;
    }
    if bits.bits_remaining() == 0 && states[0].state == 0 && states[1].state == 0 {
        Ok(n)
    } else {
        Err(corrupt("FSE stream does not end with its symbols"))
    }
}

/// A Huffman decoding table: one entry per value of its `log` leading bits.
#[derive(Clone)]
pub(super) struct HufTable {
    log: u32,
    /// `(symbol, code length)` per entry.
    cells: Vec<(u8, u8)>,
}

/// Run lengths a description starting with byte `242 + i` stands for: that
/// many symbols, all of weight 1 (`HUFv07_readStats`).
const WEIGHT_RUNS: [usize; 14] = [1, 2, 3, 4, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128];

impl HufTable {
    /// Read a Huffman table description (`HUFv07_readStats` and
    /// `HUFv07_readDTableX2`), its weights FSE-coded with the ending `end`.
    /// Returns the table and the bytes read.
    pub(super) fn read(src: &[u8], end: StreamEnd) -> Result<(HufTable, usize), Error> {
        let &header = src.first().ok_or(Error::Truncated)?;
        let mut weights = [0u8; 256];
        let (explicit, consumed) = match header {
            242..=255 => {
                weights.fill(1);
                (WEIGHT_RUNS[usize::from(header - 242)], 1)
            }
            128..=241 => {
                let explicit = usize::from(header) - 127;
                let packed = explicit.div_ceil(2);
                let bytes = src.get(1..1 + packed).ok_or(Error::Truncated)?;
                for (n, &byte) in bytes.iter().enumerate() {
                    weights[2 * n] = byte >> 4;
                    weights[2 * n + 1] = byte & 0xF;
                }
                (explicit, packed + 1)
            }
            _ => {
                let len = usize::from(header);
                let body = src.get(1..1 + len).ok_or(Error::Truncated)?;
                (fse_decompress(body, &mut weights, 255, end)?, len + 1)
            }
        };
        Ok((HufTable::from_weights(&mut weights, explicit)?, consumed))
    }

    /// The table whose weights are an RFC 8878 description's code lengths,
    /// the last one included, under a longest code of `max_bits`.
    pub(super) fn from_code_lengths(lengths: &[u8], max_bits: u8) -> Result<HufTable, Error> {
        let explicit = lengths
            .len()
            .checked_sub(1)
            .filter(|&explicit| explicit < 256)
            .ok_or(corrupt("Huffman description without symbols"))?;
        let mut weights = [0u8; 256];
        for (weight, &len) in weights.iter_mut().zip(&lengths[..explicit]) {
            if len > max_bits {
                return Err(corrupt("Huffman code longer than its table"));
            }
            *weight = if len == 0 { 0 } else { max_bits + 1 - len };
        }
        // The last weight is inferred again, from the others.
        HufTable::from_weights(&mut weights, explicit)
    }

    /// Build the table from the first `explicit` weights, inferring the
    /// weight of the symbol after them.
    fn from_weights(weights: &mut [u8; 256], explicit: usize) -> Result<HufTable, Error> {
        let mut ranks = [0u32; HUF_WEIGHT_LIMIT as usize + 1];
        let mut total = 0u32;
        for &weight in &weights[..explicit] {
            if weight >= HUF_WEIGHT_LIMIT {
                return Err(corrupt("Huffman weight too large"));
            }
            ranks[usize::from(weight)] += 1;
            total += (1 << weight) >> 1;
        }
        if total == 0 {
            return Err(corrupt("Huffman description without symbols"));
        }
        // The last symbol's weight is implied: it brings the total to a
        // power of two.
        let log = 32 - total.leading_zeros();
        if log > u32::from(HUF_WEIGHT_LIMIT) {
            return Err(corrupt("Huffman table too large"));
        }
        let rest = (1u32 << log) - total;
        if !rest.is_power_of_two() {
            return Err(corrupt("Huffman weights do not sum to a power of two"));
        }
        let last = rest.trailing_zeros() + 1;
        weights[explicit] = last as u8;
        ranks[last as usize] += 1;
        // A prefix code has an even number, at least two, of longest codes.
        if ranks[1] < 2 || ranks[1] % 2 != 0 {
            return Err(corrupt("Huffman weights do not form a prefix code"));
        }
        if log > HUF_MAX_TABLE_LOG {
            return Err(corrupt("Huffman table too large"));
        }

        let mut start = [0u32; HUF_WEIGHT_LIMIT as usize + 1];
        let mut next = 0u32;
        for weight in 1..=log as usize {
            start[weight] = next;
            next += ranks[weight] << (weight - 1);
        }
        let mut cells = vec![(0u8, 0u8); 1 << log];
        for (symbol, &weight) in weights[..=explicit].iter().enumerate() {
            if weight == 0 {
                continue;
            }
            let len = 1usize << (weight - 1);
            let first = start[usize::from(weight)] as usize;
            cells[first..first + len].fill((symbol as u8, (log + 1 - u32::from(weight)) as u8));
            start[usize::from(weight)] += len as u32;
        }
        Ok(HufTable { log, cells })
    }

    #[inline]
    fn decode(&self, bits: &mut Bits<'_>) -> u8 {
        let log = self.log as u8;
        bits.ensure_bits(log);
        let (symbol, len) = self.cells[bits.peek_bits(log) as usize];
        bits.consume(len);
        symbol
    }

    /// Fill `out` from one stream, which must end exactly with it
    /// (`HUFv07_decompress1X2_usingDTable`).
    pub(super) fn decode_one_stream(&self, src: &[u8], out: &mut [u8]) -> Result<(), Error> {
        let mut bits = open_bits(src)?;
        for byte in out.iter_mut() {
            *byte = self.decode(&mut bits);
        }
        if bits.bits_remaining() != 0 {
            return Err(corrupt("Huffman stream does not end with its literals"));
        }
        Ok(())
    }

    /// Fill `out` from four streams behind a six-byte jump table, each
    /// filling a quarter (`HUFv07_decompress4X2_usingDTable`).
    pub(super) fn decode_four_streams(&self, src: &[u8], out: &mut [u8]) -> Result<(), Error> {
        if src.len() < 10 {
            return Err(corrupt("four-stream Huffman literals too short"));
        }
        let len1 = usize::from(u16::from_le_bytes([src[0], src[1]]));
        let len2 = usize::from(u16::from_le_bytes([src[2], src[3]]));
        let len3 = usize::from(u16::from_le_bytes([src[4], src[5]]));
        let streams_end = 6 + len1 + len2 + len3;
        if streams_end > src.len() {
            return Err(corrupt("Huffman jump table points past its streams"));
        }
        let streams = [
            &src[6..6 + len1],
            &src[6 + len1..6 + len1 + len2],
            &src[6 + len1 + len2..streams_end],
            &src[streams_end..],
        ];
        let segment = out.len().div_ceil(4);
        // Below six literals the first three quarters reach past the output;
        // upstream decodes them into the literal buffer's slack and keeps what
        // falls inside. A scratch of that size gives the same output.
        let mut small = [0u8; 8];
        let work: &mut [u8] = if 3 * segment > out.len() {
            &mut small[..3 * segment]
        } else {
            &mut *out
        };
        let total = work.len();
        for (index, stream) in streams.iter().enumerate() {
            let start = (index * segment).min(total);
            let end = if index == 3 {
                total
            } else {
                ((index + 1) * segment).min(total)
            };
            self.decode_one_stream(stream, &mut work[start..end])?;
        }
        if 3 * segment > out.len() {
            let len = out.len();
            out.copy_from_slice(&small[..len]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
