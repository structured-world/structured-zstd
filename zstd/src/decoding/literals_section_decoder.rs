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

        // Donor-style burst loop with SIMD fallback interleaved per
        // iteration — burst is the primary tier whenever the gate
        // holds, SIMD takes over for the iterations where the burst
        // is gated out (typically right after `advance_state_by_bits`
        // triggers a refill inside a SIMD iter and `bits_consumed`
        // rebases to `[0, 7]`).
        //
        // bits[s] register (per stream) layout for the burst, MSB → LSB:
        //   [ state (max_num_bits) | stream (≤ 64 - 2·max bits) | zeros + sentinel ]
        //
        // Our `decoder.state` is conceptually "the next max-bit
        // lookahead window starting at the current consumption
        // point"; the stream bits that constitute it sit in
        // `bit_container` at positions
        // `[(64 - bits_consumed), (63 - bits_consumed + max))` BUT
        // ONLY when `bits_consumed >= max_num_bits`. After a refill
        // `bits_consumed` resets to `[0, 7]`, where those positions
        // partially fall outside the current window — the formula
        // would then lose low stream bits. The `bits_consumed >=
        // max_num_bits` gate keeps the burst sound; the SIMD branch
        // handles the post-refill iterations until `bits_consumed`
        // grows back into burst range, at which point we re-enter
        // the burst body in the same outer loop.
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
        let decode4_unchecked = HuffmanDecoder::decode4_has_shared_table_and_kernel(&decoders);

        loop {
            // Common bound: any stream exhausted or any cursor at end
            // → exit; the single-symbol tail below handles the drain.
            if brs[0].bits_remaining() <= -max_bits
                || brs[1].bits_remaining() <= -max_bits
                || brs[2].bits_remaining() <= -max_bits
                || brs[3].bits_remaining() <= -max_bits
                || cursors[0] >= ends[0]
                || cursors[1] >= ends[1]
                || cursors[2] >= ends[2]
                || cursors[3] >= ends[3]
            {
                break;
            }

            let burst_ok = symbols_per_burst >= 1
                && brs[0].bits_remaining() > burst_bits_isize
                && brs[1].bits_remaining() > burst_bits_isize
                && brs[2].bits_remaining() > burst_bits_isize
                && brs[3].bits_remaining() > burst_bits_isize
                // Saturating form so the bound holds even when `ends[i]
                // < symbols_per_burst` near the segment tail (rather
                // than relying on `cursors[i] + symbols_per_burst` not
                // wrapping). `regen` is bounded by RFC 8878 block
                // size ⇒ overflow is unreachable in practice, but the
                // saturating shape costs the same single subq and
                // removes the addition entirely.
                && cursors[0] <= ends[0].saturating_sub(symbols_per_burst)
                && cursors[1] <= ends[1].saturating_sub(symbols_per_burst)
                && cursors[2] <= ends[2].saturating_sub(symbols_per_burst)
                && cursors[3] <= ends[3].saturating_sub(symbols_per_burst)
                && brs[0].bits_consumed >= max_num_bits
                && brs[1].bits_consumed >= max_num_bits
                && brs[2].bits_consumed >= max_num_bits
                && brs[3].bits_consumed >= max_num_bits
                // Burst body has no `ensure_bits` — confirm the burst
                // fits inside the current `bit_container` so the
                // inner shifts never read past the loaded 8-byte
                // window.
                && (brs[0].bits_consumed as usize) + burst_bits as usize <= 64
                && (brs[1].bits_consumed as usize) + burst_bits as usize <= 64
                && (brs[2].bits_consumed as usize) + burst_bits as usize <= 64
                && (brs[3].bits_consumed as usize) + burst_bits as usize <= 64;

            if burst_ok {
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
                    brs[s].consume(consumed);
                    decoders[s].state = bits[s] >> table_shift;
                }
            } else {
                // SIMD 4-symbol fallback for one outer iteration.
                // `advance_state_by_bits` triggers a refill inside
                // `get_bits` when needed; after this iter
                // `bits_consumed` is back in `[0, 7]+n` and the
                // burst gate may be satisfied again on the next
                // outer-loop pass.
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

#[cfg(test)]
mod burst_gate_tests {
    //! Regression coverage for the HUF 4-stream burst-gate boundary
    //! states in `decompress_literals`:
    //!
    //!   1. `bits_consumed == max_num_bits` — lower boundary of the
    //!      burst gate, where the gate is entered with zero slack.
    //!   2. `bits_consumed + burst_bits == 64` — upper boundary, where
    //!      the burst consumes all remaining bits in the 64-bit window
    //!      without overflow.
    //!   3. SIMD-fallback → refill → burst re-entry — outer loop falls
    //!      back to the SIMD 4-symbol path, a `BitReaderReversed`
    //!      refill occurs, the next iteration re-enters the burst path
    //!      once `bits_consumed` grows back into burst range.
    //!
    //! Each named test pins an input shape chosen to drive the gate
    //! through the corresponding regime — short skewed input for the
    //! initial-entry lower-bound, long mid-cardinality streams for
    //! many upper-bound brushes, multi-segment input for repeated
    //! SIMD↔burst transitions. The sweep test covers the gate in
    //! aggregate across many `(size, alphabet)` combinations.
    //!
    //! These tests do NOT assert that a specific
    //! `(bits_consumed, burst_bits)` configuration is hit deterministically
    //! on any single iteration — that would require white-box state
    //! instrumentation that the current decoder does not expose. They
    //! assert end-to-end roundtrip correctness through the full
    //! encoder → 4-stream HUF block → `decode_literals` path; a
    //! burst-gate regression that returns the wrong symbol or
    //! desynchronises a stream produces either a
    //! `DecompressLiteralsError` from the `BitstreamReadMismatch` /
    //! `DecodedLiteralCountMismatch` guards or a mismatched decoded
    //! buffer — both fail the assertion. The `max_num_bits` range
    //! checks in the per-test helper also detect silent drift where
    //! the encoder's table-generation choice shifts the test out of
    //! the intended gate regime.
    use super::*;
    use crate::bit_io::BitWriter;
    use crate::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
    use crate::decoding::scratch::HuffmanScratch;
    use crate::huff0::huff0_encoder::{HuffmanEncoder, HuffmanTable as EncTable};
    use alloc::vec::Vec;

    /// Encode `data` as a 4-stream HUF Compressed literals block (table
    /// description + jump table + 4 padded streams) and return the
    /// matching `LiteralsSection` header plus the wire bytes.
    fn build_huf4x_block(data: &[u8]) -> (LiteralsSection, Vec<u8>) {
        assert!(data.len() >= 4, "encode4x requires at least 4 bytes");
        let table = EncTable::build_from_data(data);
        let mut source: Vec<u8> = Vec::new();
        {
            let mut writer = BitWriter::from(&mut source);
            let mut encoder = HuffmanEncoder::new(&table, &mut writer);
            encoder.encode4x(data, true);
            writer.flush();
        }
        let section = LiteralsSection {
            ls_type: LiteralsSectionType::Compressed,
            regenerated_size: data.len() as u32,
            compressed_size: Some(source.len() as u32),
            num_streams: Some(4),
        };
        (section, source)
    }

    /// Roundtrip `data` through encode4x + decode_literals and assert
    /// the decoded buffer matches byte-for-byte. Returns the HUF table's
    /// `max_num_bits` so call sites can sanity-check that they actually
    /// hit the expected burst-gate regime.
    fn roundtrip_assert(data: &[u8]) -> u8 {
        let (section, source) = build_huf4x_block(data);
        let mut scratch = HuffmanScratch::new();
        let mut target = Vec::new();
        let bytes_read = decode_literals(&section, &mut scratch, &source, &mut target)
            .expect("decode_literals must succeed on a well-formed roundtrip");
        assert_eq!(
            bytes_read as usize,
            source.len(),
            "decoder must consume every byte of the literals block"
        );
        assert_eq!(
            target, data,
            "decoded literals must match the encoder input"
        );
        scratch.table.max_num_bits
    }

    /// Roundtrip + assertion that the HUF table's `max_num_bits` falls
    /// inside the expected range — this is what selects which burst-gate
    /// regime the body runs under (`symbols_per_burst = (63 - max) / max`).
    fn roundtrip_with_max_bits_range(data: &[u8], expected: core::ops::RangeInclusive<u8>) {
        let m = roundtrip_assert(data);
        assert!(
            expected.contains(&m),
            "max_num_bits {} outside expected range {:?} for this fixture — \
             test no longer exercises the intended gate regime",
            m,
            expected
        );
    }

    /// Lower boundary: targets `bits_consumed == max_num_bits` on
    /// early burst entries.
    ///
    /// A short stream with a skewed 23-symbol alphabet keeps
    /// `max_num_bits` in the 5..=11 band and limits the number of
    /// burst iterations, so early iterations run with `bits_consumed`
    /// near the gate threshold. The decoder must not lose low stream
    /// bits when the shift formula runs close to the threshold;
    /// roundtrip correctness over short input is the regression signal.
    #[test]
    fn burst_gate_lower_boundary_short_skewed_alphabet() {
        // 36 bytes, 23 distinct symbols, skewed distribution —
        // encoder picks max_num_bits in the 5..=11 band.
        let mut data: Vec<u8> = Vec::with_capacity(36);
        data.extend_from_slice(&[
            0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
            14, 15, 16, 17, 18, 19, 20, 21, 22,
        ]);
        roundtrip_with_max_bits_range(&data, 5..=11);
    }

    /// Upper boundary: `bits_consumed + burst_bits == 64`.
    ///
    /// A long, mid-cardinality alphabet drives many full burst windows.
    /// Across thousands of iterations the burst-fits-in-64 guard
    /// (`bits_consumed + burst_bits <= 64`) is approached and met
    /// exactly. A regression that miscalculated the upper boundary
    /// would read past the loaded 8-byte window and either crash under
    /// debug bounds checks or desynchronise the stream — either way
    /// the roundtrip fails.
    #[test]
    fn burst_gate_upper_boundary_long_mid_alphabet() {
        // 4 KiB with a 97-symbol pseudo-random alphabet (kept under the
        // encoder's 128-weight raw-table limit). Broad distribution →
        // max_num_bits ≈ 7..9, thousands of burst iterations across all
        // four streams.
        let mut data: Vec<u8> = Vec::with_capacity(4096);
        for i in 0..4096u32 {
            data.push((i.wrapping_mul(0x9E37_79B1) % 97) as u8);
        }
        roundtrip_with_max_bits_range(&data, 6..=11);
    }

    /// SIMD-fallback → refill → burst re-entry transition.
    ///
    /// After a `BitReaderReversed::refill` (triggered inside
    /// `advance_state_by_bits` on the SIMD path), `bits_consumed`
    /// rebases to `[0, 7]`. Until it climbs back to `max_num_bits` the
    /// burst gate is closed and the outer loop runs the 4-symbol SIMD
    /// fallback; on the next outer-loop iteration after `bits_consumed`
    /// grows past `max_num_bits` the burst path must re-enter cleanly.
    ///
    /// Stream length of 16 KiB / 4 ≈ 4 KiB per stream encoded ⇒ each
    /// `BitReaderReversed` window crosses many refill boundaries,
    /// guaranteeing the SIMD→refill→burst transition fires repeatedly.
    #[test]
    fn burst_simd_fallback_refill_reentry_long_streams() {
        // 67-symbol modulo distribution (`i % 67`, prime modulus spreads
        // the alphabet evenly) → max_num_bits typically 7..8, which gives
        // `symbols_per_burst = (63 - max) / max ≈ 6..8`.
        let mut data: Vec<u8> = Vec::with_capacity(16 * 1024);
        for i in 0..16 * 1024u32 {
            data.push((i % 67) as u8);
        }
        roundtrip_with_max_bits_range(&data, 5..=8);
    }

    /// Parametric sweep across stream lengths and alphabet shapes.
    ///
    /// The three burst-gate states above are also hit across this matrix
    /// at varying `(bits_consumed, max_num_bits, symbols_per_burst)`
    /// configurations; any future tweak to the gate that mishandles a
    /// specific `(max_num_bits, post-refill bits_consumed)` combo trips
    /// at least one cell here.
    #[test]
    fn burst_gate_sweep_sizes_and_alphabets() {
        let sizes = [
            16usize, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 511, 512, 513, 1023,
            1024, 1025, 4096,
        ];
        for &n in &sizes {
            // Binary alphabet → max_num_bits == 1, symbols_per_burst large.
            let mut bin: Vec<u8> = Vec::with_capacity(n);
            for i in 0..n {
                bin.push((i & 1) as u8);
            }
            roundtrip_assert(&bin);

            // 16-symbol uniform alphabet → max_num_bits ≈ 4.
            let mut sm: Vec<u8> = Vec::with_capacity(n);
            for i in 0..n {
                sm.push((i % 16) as u8);
            }
            roundtrip_assert(&sm);

            // 97-symbol pseudo-random alphabet (where length permits) →
            // max_num_bits ≈ 7..9; kept under the encoder's 128-weight
            // raw-table cap so the encoder reliably succeeds.
            if n >= 128 {
                let mut wide: Vec<u8> = Vec::with_capacity(n);
                for i in 0..n {
                    wide.push((i.wrapping_mul(2_654_435_761) % 97) as u8);
                }
                roundtrip_assert(&wide);
            }
        }
    }
}
