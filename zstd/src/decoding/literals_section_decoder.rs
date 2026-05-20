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

        // Donor-style burst loop.
        //
        // bits[s] register (per stream) layout, MSB → LSB:
        //   [ state (max_num_bits) | stream (≤ 64 - 2·max bits) | zeros + sentinel ]
        //
        // Our `decoder.state` is conceptually "the next max-bit
        // lookahead window starting at the current consumption point";
        // the stream bits that constitute it sit in `bit_container` at
        // positions `[(64 - bits_consumed), (63 - bits_consumed + max))`
        // BUT ONLY when `bits_consumed >= max_num_bits`. After a refill
        // `bits_consumed` resets to `[0, 7]`, where those positions
        // partially fall outside the current window — the formula then
        // would lose low stream bits. Guard the burst by requiring
        // `bits_consumed >= max_num_bits` on every stream; if any
        // stream falls below, the tail loop runs single-symbol until
        // the invariant is restored.
        let max_num_bits = scratch.table.max_num_bits;
        // symbols_per_burst * max ≤ 63 - max so the sentinel stays
        // below the state region after the worst-case T-shift.
        // For max=11: 4 symbols. For max=8: 6 symbols.
        let symbols_per_burst: usize = (63 - max_num_bits as usize) / max_num_bits as usize;
        let burst_bits = (symbols_per_burst * max_num_bits as usize) as u8;
        let burst_bits_isize = burst_bits as isize;
        let table_shift = (64 - max_num_bits) as u32;
        let state_shift = 64 - max_num_bits;
        let packed = scratch.table.packed_decode.as_slice();

        while symbols_per_burst >= 1
            && brs[0].bits_remaining() > burst_bits_isize
            && brs[1].bits_remaining() > burst_bits_isize
            && brs[2].bits_remaining() > burst_bits_isize
            && brs[3].bits_remaining() > burst_bits_isize
            && cursors[0] + symbols_per_burst <= ends[0]
            && cursors[1] + symbols_per_burst <= ends[1]
            && cursors[2] + symbols_per_burst <= ends[2]
            && cursors[3] + symbols_per_burst <= ends[3]
            && brs[0].bits_consumed >= max_num_bits
            && brs[1].bits_consumed >= max_num_bits
            && brs[2].bits_consumed >= max_num_bits
            && brs[3].bits_consumed >= max_num_bits
            // Without an `ensure_bits` call inside the loop we must
            // also confirm the burst fits inside the current
            // `bit_container` (no refill in-flight). After consume,
            // `bits_consumed + burst_bits ≤ 64` so the inner shifts
            // never read past the loaded 8-byte window.
            && (brs[0].bits_consumed as usize) + burst_bits as usize <= 64
            && (brs[1].bits_consumed as usize) + burst_bits as usize <= 64
            && (brs[2].bits_consumed as usize) + burst_bits as usize <= 64
            && (brs[3].bits_consumed as usize) + burst_bits as usize <= 64
        {
            let mut bits = [
                (decoders[0].state << state_shift)
                    | ((brs[0].bit_container << brs[0].bits_consumed) >> max_num_bits)
                    | 1,
                (decoders[1].state << state_shift)
                    | ((brs[1].bit_container << brs[1].bits_consumed) >> max_num_bits)
                    | 1,
                (decoders[2].state << state_shift)
                    | ((brs[2].bit_container << brs[2].bits_consumed) >> max_num_bits)
                    | 1,
                (decoders[3].state << state_shift)
                    | ((brs[3].bit_container << brs[3].bits_consumed) >> max_num_bits)
                    | 1,
            ];

            for _ in 0..symbols_per_burst {
                let idx0 = (bits[0] >> table_shift) as usize;
                let entry0 = packed[idx0];
                target[cursors[0]] = (entry0 & 0xFF) as u8;
                cursors[0] += 1;
                bits[0] <<= (entry0 >> 8) & 0xFF;

                let idx1 = (bits[1] >> table_shift) as usize;
                let entry1 = packed[idx1];
                target[cursors[1]] = (entry1 & 0xFF) as u8;
                cursors[1] += 1;
                bits[1] <<= (entry1 >> 8) & 0xFF;

                let idx2 = (bits[2] >> table_shift) as usize;
                let entry2 = packed[idx2];
                target[cursors[2]] = (entry2 & 0xFF) as u8;
                cursors[2] += 1;
                bits[2] <<= (entry2 >> 8) & 0xFF;

                let idx3 = (bits[3] >> table_shift) as usize;
                let entry3 = packed[idx3];
                target[cursors[3]] = (entry3 & 0xFF) as u8;
                cursors[3] += 1;
                bits[3] <<= (entry3 >> 8) & 0xFF;
            }

            for s in 0..4 {
                let consumed = bits[s].trailing_zeros() as u8;
                brs[s].bits_consumed += consumed;
                decoders[s].state = bits[s] >> table_shift;
            }
        }

        // SIMD 4-symbol main loop: runs when the donor burst gate
        // (bits_consumed >= max_num_bits) is not satisfied, e.g.
        // right after a refill rebases bits_consumed to [0, 7]. Uses
        // the existing `decode4_symbols_and_num_bits` SIMD lookup +
        // per-symbol `advance_state_by_bits` so the slow path still
        // beats the single-symbol tail.
        let decode4_unchecked = HuffmanDecoder::decode4_has_shared_table_and_kernel(&decoders);
        while brs[0].bits_remaining() > -max_bits
            && brs[1].bits_remaining() > -max_bits
            && brs[2].bits_remaining() > -max_bits
            && brs[3].bits_remaining() > -max_bits
            && cursors[0] < ends[0]
            && cursors[1] < ends[1]
            && cursors[2] < ends[2]
            && cursors[3] < ends[3]
        {
            let (symbols, nbits) = if decode4_unchecked {
                // SAFETY: same-table-and-kernel verified above.
                unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_unchecked(&decoders) }
            } else {
                HuffmanDecoder::decode4_symbols_and_num_bits(&decoders)
            };
            target[cursors[0]] = symbols[0];
            cursors[0] += 1;
            target[cursors[1]] = symbols[1];
            cursors[1] += 1;
            target[cursors[2]] = symbols[2];
            cursors[2] += 1;
            target[cursors[3]] = symbols[3];
            cursors[3] += 1;
            decoders[0].advance_state_by_bits(&mut brs[0], nbits[0]);
            decoders[1].advance_state_by_bits(&mut brs[1], nbits[1]);
            decoders[2].advance_state_by_bits(&mut brs[2], nbits[2]);
            decoders[3].advance_state_by_bits(&mut brs[3], nbits[3]);
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
