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

    target.reserve(section.regenerated_size as usize);
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

        // Each stream decodes into its own buffer, concatenated at the end.
        // Estimate ~1/4 of regenerated_size per stream for pre-allocation.
        let est = section.regenerated_size as usize / 4 + 1;
        let mut bufs: [Vec<u8>; 4] = [
            Vec::with_capacity(est),
            Vec::with_capacity(est),
            Vec::with_capacity(est),
            Vec::with_capacity(est),
        ];

        let max_bits = scratch.table.max_num_bits as isize;

        // Fast interleaved loop: while all 4 streams have bits remaining,
        // issue 4 independent table lookups then 4 independent state advances
        // per iteration. This gives the CPU's out-of-order engine more
        // independent work to schedule, hiding table-lookup latency.
        while brs[0].bits_remaining() > -max_bits
            && brs[1].bits_remaining() > -max_bits
            && brs[2].bits_remaining() > -max_bits
            && brs[3].bits_remaining() > -max_bits
        {
            // Decode phase: 4 independent table lookups
            let s0 = decoders[0].decode_symbol();
            let s1 = decoders[1].decode_symbol();
            let s2 = decoders[2].decode_symbol();
            let s3 = decoders[3].decode_symbol();

            bufs[0].push(s0);
            bufs[1].push(s1);
            bufs[2].push(s2);
            bufs[3].push(s3);

            // State advance phase: 4 independent bit reads + state updates
            decoders[0].next_state(&mut brs[0]);
            decoders[1].next_state(&mut brs[1]);
            decoders[2].next_state(&mut brs[2]);
            decoders[3].next_state(&mut brs[3]);
        }

        // Drain remaining symbols from each stream individually
        for i in 0..4 {
            while brs[i].bits_remaining() > -max_bits {
                bufs[i].push(decoders[i].decode_symbol());
                decoders[i].next_state(&mut brs[i]);
            }
            if brs[i].bits_remaining() != -max_bits {
                return Err(DecompressLiteralsError::BitstreamReadMismatch {
                    read_til: brs[i].bits_remaining(),
                    expected: -max_bits,
                });
            }
        }

        // Concatenate all 4 stream outputs
        for buf in &bufs {
            target.extend_from_slice(buf);
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
            target.push(decoder.decode_symbol());
            decoder.next_state(&mut br);
        }
        bytes_read += source.len() as u32;
    }

    if target.len() != section.regenerated_size as usize {
        return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
            decoded: target.len(),
            expected: section.regenerated_size as usize,
        });
    }

    Ok(bytes_read)
}
