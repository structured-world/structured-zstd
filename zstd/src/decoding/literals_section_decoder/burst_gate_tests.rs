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
    let bytes_read = decode_literals(&section, &mut scratch, None, &source, &mut target)
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
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14,
        15, 16, 17, 18, 19, 20, 21, 22,
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

/// Adversarial regression for the `burst_eligible` safety gate.
///
/// Builds a valid 4-stream HUF block, then forges a `LiteralsSection`
/// header that claims `regenerated_size = 1` while the encoded
/// streams still contain a full block worth of symbols. The shrunk
/// `regenerated_size` collapses `min_seg_len` below
/// `symbols_per_burst`, the exact precondition `burst_eligible`
/// guards against. Without that gate, the burst inner loop would
/// advance `cursors[i]` past `ends[i]` and panic on the
/// `target[cursors[i]]` write — a DoS surface on malformed input.
///
/// With the gate, the decoder either:
///   - falls through to the SIMD-fallback path which immediately
///     hits the top-of-loop `cursor_exit_olimit` exit and returns
///     a count-mismatch / bitstream-mismatch error, or
///   - returns an error before the loop ever runs.
///
/// Either way the test asserts `Err(_)` — the contract is "no
/// panic, return an error".
#[test]
fn burst_gate_malformed_small_regen_returns_error() {
    // 256 bytes is well above MIN_LITERALS_FOR_4_STREAMS so the
    // encoder will happily emit a 4-stream HUF block. The modulo
    // alphabet keeps `max_num_bits` small (≤ 8), maximising
    // `symbols_per_burst` so the small forged `regenerated_size`
    // sits well below it.
    let mut data: Vec<u8> = Vec::with_capacity(256);
    for i in 0..256u32 {
        data.push((i % 67) as u8);
    }
    let (mut section, source) = build_huf4x_block(&data);

    // Forge: claim only 1 regenerated byte. Streams in `source`
    // are still encoded for the full 256-byte input.
    section.regenerated_size = 1;

    let mut scratch = HuffmanScratch::new();
    let mut target = Vec::new();
    let result = decode_literals(&section, &mut scratch, None, &source, &mut target);

    assert!(
        result.is_err(),
        "decoder must reject the malformed header instead of panicking; \
             got Ok({})",
        result.unwrap_or(0)
    );
}
