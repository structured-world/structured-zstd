//! This module contains the decompress_literals function, used to take a
//! parsed literals header and a source and decompress it.

use super::super::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
use super::scratch::HuffmanScratch;
use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::DecompressLiteralsError;
use crate::huff0::HuffmanDecoder;
use alloc::vec::Vec;

/// Decode and decompress the provided literals section into `target`, returning the number of bytes read.
pub fn decode_literals(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    match section.ls_type {
        LiteralsSectionType::Raw => {
            target.extend(&source[0..section.regenerated_size as usize]);
            Ok(section.regenerated_size)
        }
        LiteralsSectionType::RLE => {
            target.resize(target.len() + section.regenerated_size as usize, source[0]);
            Ok(1)
        }
        LiteralsSectionType::Compressed | LiteralsSectionType::Treeless => {
            let bytes_read = decompress_literals(section, scratch, source, target)?;

            //return sum of used bytes
            Ok(bytes_read)
        }
    }
}

/// Decompress the provided literals section and source into the provided `target`.
/// This function is used when the literals section is `Compressed` or `Treeless`
///
/// Returns the number of bytes read.
fn decompress_literals(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    use DecompressLiteralsError as err;

    let compressed_size = section.compressed_size.ok_or(err::MissingCompressedSize)? as usize;
    let num_streams = section.num_streams.ok_or(err::MissingNumStreams)?;
    let base = target.len();
    let regen = section.regenerated_size as usize;

    target.reserve(regen);
    let source = &source[0..compressed_size];
    let mut bytes_read = 0;

    match section.ls_type {
        LiteralsSectionType::Compressed => {
            //read Huffman tree description
            bytes_read += scratch.table.build_decoder(source)?;
            vprintln!("Built huffman table using {} bytes", bytes_read);
        }
        LiteralsSectionType::Treeless if scratch.table.max_num_bits == 0 => {
            return Err(err::UninitializedHuffmanTable);
        }

        _ => { /* nothing to do, huffman tree has been provided by previous block */ }
    }

    let source = &source[bytes_read as usize..];

    if num_streams == 4 {
        //build jumptable
        if source.len() < 6 {
            return Err(err::MissingBytesForJumpHeader { got: source.len() });
        }
        let jump1 = source[0] as usize + ((source[1] as usize) << 8);
        let jump2 = jump1 + source[2] as usize + ((source[3] as usize) << 8);
        let jump3 = jump2 + source[4] as usize + ((source[5] as usize) << 8);
        bytes_read += 6;
        let source = &source[6..];

        if source.len() < jump3 {
            return Err(err::MissingBytesForLiterals {
                got: source.len(),
                needed: jump3,
            });
        }

        //decode 4 streams with interleaved operations to hide memory latency
        let streams: [&[u8]; 4] = [
            &source[..jump1],
            &source[jump1..jump2],
            &source[jump2..jump3],
            &source[jump3..],
        ];

        let mut decoders: [HuffmanDecoder<'_>; 4] = [
            HuffmanDecoder::new(&scratch.table),
            HuffmanDecoder::new(&scratch.table),
            HuffmanDecoder::new(&scratch.table),
            HuffmanDecoder::new(&scratch.table),
        ];
        let mut brs: [BitReaderReversed<'_>; 4] = [
            BitReaderReversed::new(streams[0]),
            BitReaderReversed::new(streams[1]),
            BitReaderReversed::new(streams[2]),
            BitReaderReversed::new(streams[3]),
        ];

        // Initialize all 4 streams: skip padding and set initial state
        for i in 0..4 {
            let mut skipped_bits = 0;
            loop {
                let val = brs[i].get_bits(1);
                skipped_bits += 1;
                if val == 1 || skipped_bits > 8 {
                    break;
                }
            }
            if skipped_bits > 8 {
                return Err(DecompressLiteralsError::ExtraPadding { skipped_bits });
            }
            decoders[i].init_state(&mut brs[i]);
        }

        let max_bits = scratch.table.max_num_bits as isize;

        // RFC 8878 §3.1.1.3.2: first 3 streams produce ceil(regen_size/4)
        // symbols each, 4th produces the remainder. Pre-allocate target and
        // decode directly into slices — no temporary Vec allocations.
        let seg = regen.div_ceil(4);

        target.resize(base + regen, 0);
        // Clamp every start/end into [base, base+regen] so cursors can
        // never index past the pre-allocated region, even with corrupted
        // frame headers that produce small regen (where N*seg > regen).
        let limit = base + regen;
        let starts: [usize; 4] = [
            base,
            (base + seg).min(limit),
            (base + 2 * seg).min(limit),
            (base + 3 * seg).min(limit),
        ];
        let ends: [usize; 4] = [starts[1], starts[2], starts[3], limit];
        let mut cursors = starts;

        // Fast interleaved loop: decode 4 symbols/bit-counts via decode4 helper
        // (which may use packed/SIMD gather+unpack kernels), then advance the
        // 4 stream states independently. This gives the CPU's out-of-order
        // engine more independent work to schedule, hiding decode latency.
        enum Decode4Mode {
            Unchecked,
            Checked,
        }
        let decode4_mode = if HuffmanDecoder::decode4_has_shared_table_and_kernel(&decoders) {
            Decode4Mode::Unchecked
        } else {
            Decode4Mode::Checked
        };

        // Donor-parity burst: decode `SYMBOLS_PER_BURST` symbols per stream
        // (× 4 streams = `4 * SYMBOLS_PER_BURST` symbols per outer iteration)
        // between bit-reader refill checks. Mirrors
        // `HUF_DECODEFAST_4X1_LOOP_SYM = 5` in
        // `huf_decompress.c:HUF_decompress4X1_usingDTable_internal_fast_c_loop`.
        //
        // Burst size is computed so the maximum bits consumed per stream
        // across one burst is `SYMBOLS_PER_BURST * max_num_bits <= 56`,
        // matching the `BitReaderReversed::ensure_bits(n <= 56)` contract.
        // For the dominant HUF table widths (`max_num_bits <= 11`) this
        // selects 5; for the rare 12-bit literal tables it backs off to 4.
        // `max_num_bits.max(1)` guards the degenerate zero-table case
        // (compressed literal sections with `max_num_bits == 0` decode to
        // empty output via the tail loop anyway).
        let max_num_bits = scratch.table.max_num_bits.max(1);
        let symbols_per_burst: usize = (56 / max_num_bits as usize).max(1);
        let burst_bits = (symbols_per_burst * max_num_bits as usize) as u8;
        let burst_bits_isize = burst_bits as isize;

        // Per-stream bound used to decide whether the next burst can fit
        // entirely inside the bit budget. Donor uses an explicit
        // pre-computed `olimit`; the `bits_remaining > burst_bits_isize`
        // check here serves the same role — the burst can consume at most
        // `burst_bits` per stream and we want at least `max_bits` left
        // over so the tail loop has a clean termination condition.
        while brs[0].bits_remaining() > burst_bits_isize
            && brs[1].bits_remaining() > burst_bits_isize
            && brs[2].bits_remaining() > burst_bits_isize
            && brs[3].bits_remaining() > burst_bits_isize
            && cursors[0] + symbols_per_burst <= ends[0]
            && cursors[1] + symbols_per_burst <= ends[1]
            && cursors[2] + symbols_per_burst <= ends[2]
            && cursors[3] + symbols_per_burst <= ends[3]
        {
            // Single refill check per stream per burst — drops the per-call
            // `bits_consumed + n > 64` branch that `get_bits` performs
            // inside `advance_state_by_bits`. Donor `HUF_4X1_RELOAD_STREAM`
            // (huf_decompress.c:795-804) does the equivalent at the end of
            // each 5-symbol burst.
            brs[0].ensure_bits(burst_bits);
            brs[1].ensure_bits(burst_bits);
            brs[2].ensure_bits(burst_bits);
            brs[3].ensure_bits(burst_bits);

            for _ in 0..symbols_per_burst {
                let (symbols, bits) = match decode4_mode {
                    Decode4Mode::Unchecked => {
                        // SAFETY: guarded by decode4_has_shared_table_and_kernel above.
                        unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_unchecked(&decoders) }
                    }
                    Decode4Mode::Checked => HuffmanDecoder::decode4_symbols_and_num_bits(&decoders),
                };

                target[cursors[0]] = symbols[0];
                target[cursors[1]] = symbols[1];
                target[cursors[2]] = symbols[2];
                target[cursors[3]] = symbols[3];
                cursors[0] += 1;
                cursors[1] += 1;
                cursors[2] += 1;
                cursors[3] += 1;

                // _unchecked: the burst-level ensure_bits above already
                // covers `symbols_per_burst * max_num_bits` bits per stream
                // and each `bits[i]` is `<= max_num_bits`, so cumulative
                // consumption inside the burst can never exceed the ensured
                // budget.
                decoders[0].advance_state_by_bits_unchecked(&mut brs[0], bits[0]);
                decoders[1].advance_state_by_bits_unchecked(&mut brs[1], bits[1]);
                decoders[2].advance_state_by_bits_unchecked(&mut brs[2], bits[2]);
                decoders[3].advance_state_by_bits_unchecked(&mut brs[3], bits[3]);
            }
        }

        // Spill-over: a few symbols may still fit in each stream after the
        // last burst exits its bound check (e.g. cursors close to ends, or
        // a stream's bit budget dropped just under `burst_bits` while the
        // others still had room). Walk one symbol at a time using the
        // checked path until each stream individually trips its termination
        // condition.
        while brs[0].bits_remaining() > -max_bits
            && brs[1].bits_remaining() > -max_bits
            && brs[2].bits_remaining() > -max_bits
            && brs[3].bits_remaining() > -max_bits
            && cursors[0] < ends[0]
            && cursors[1] < ends[1]
            && cursors[2] < ends[2]
            && cursors[3] < ends[3]
        {
            let (symbols, bits) = match decode4_mode {
                Decode4Mode::Unchecked => {
                    // SAFETY: guarded by decode4_has_shared_table_and_kernel above.
                    unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_unchecked(&decoders) }
                }
                Decode4Mode::Checked => HuffmanDecoder::decode4_symbols_and_num_bits(&decoders),
            };

            target[cursors[0]] = symbols[0];
            target[cursors[1]] = symbols[1];
            target[cursors[2]] = symbols[2];
            target[cursors[3]] = symbols[3];
            cursors[0] += 1;
            cursors[1] += 1;
            cursors[2] += 1;
            cursors[3] += 1;

            decoders[0].advance_state_by_bits(&mut brs[0], bits[0]);
            decoders[1].advance_state_by_bits(&mut brs[1], bits[1]);
            decoders[2].advance_state_by_bits(&mut brs[2], bits[2]);
            decoders[3].advance_state_by_bits(&mut brs[3], bits[3]);
        }

        // Drain remaining symbols from each stream, bounded by segment end
        for i in 0..4 {
            while brs[i].bits_remaining() > -max_bits && cursors[i] < ends[i] {
                target[cursors[i]] = decoders[i].decode_symbol_and_advance(&mut brs[i]);
                cursors[i] += 1;
            }
            if brs[i].bits_remaining() != -max_bits {
                target.truncate(base);
                return Err(DecompressLiteralsError::BitstreamReadMismatch {
                    read_til: brs[i].bits_remaining(),
                    expected: -max_bits,
                });
            }
        }

        // Verify total decoded count matches expected regenerated size.
        // Return error immediately rather than deferring to the downstream check.
        let decoded: usize = cursors.iter().zip(starts.iter()).map(|(c, s)| c - s).sum();
        if decoded != regen {
            // Truncate to base: segmented layout means partial decode left
            // bytes scattered across segments, so only base is a clean boundary.
            target.truncate(base);
            return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
                decoded,
                expected: regen,
            });
        }

        bytes_read += source.len() as u32;
    } else {
        //just decode the one stream
        assert!(num_streams == 1);
        let mut decoder = HuffmanDecoder::new(&scratch.table);
        let mut br = BitReaderReversed::new(source);
        let mut skipped_bits = 0;
        loop {
            let val = br.get_bits(1);
            skipped_bits += 1;
            if val == 1 || skipped_bits > 8 {
                break;
            }
        }
        if skipped_bits > 8 {
            //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
            return Err(DecompressLiteralsError::ExtraPadding { skipped_bits });
        }
        decoder.init_state(&mut br);
        while br.bits_remaining() > -(scratch.table.max_num_bits as isize) {
            target.push(decoder.decode_symbol_and_advance(&mut br));
        }
        let expected = -(scratch.table.max_num_bits as isize);
        if br.bits_remaining() != expected {
            target.truncate(base);
            return Err(DecompressLiteralsError::BitstreamReadMismatch {
                read_til: br.bits_remaining(),
                expected,
            });
        }
        bytes_read += source.len() as u32;
    }

    if target.len() != base + regen {
        let decoded = target.len() - base;
        target.truncate(base);
        return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
            decoded,
            expected: regen,
        });
    }

    Ok(bytes_read)
}
