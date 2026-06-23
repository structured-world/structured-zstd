//! FSE, short for Finite State Entropy, is an encoding technique
//! that assigns shorter codes to symbols that appear more frequently in data,
//! and longer codes to less frequent symbols.
//!
//! FSE works by mutating a state and using that state to index into a table.
//!
//! Zstandard uses two different kinds of entropy encoding: FSE, and Huffman coding.
//! Huffman is used to compress literals,
//! while FSE is used for all other symbols (literal length code, match length code, offset code).
//!
//! <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#fse>
//!
//! <https://arxiv.org/pdf/1311.2540>

mod fse_decoder;

pub use fse_decoder::*;

pub mod fse_encoder;

/// Only needed for testing.
///
/// Encodes the data with a table built from that data
/// Decodes the result again by first decoding the table and then the data
/// Asserts that the decoded data equals the input
#[cfg(any(test, feature = "fuzz_exports"))]
pub fn round_trip(data: &[u8]) {
    use crate::bit_io::{BitReaderReversed, BitWriter};
    use fse_encoder::FSEEncoder;

    // The decode side here is `FSETable` = `FSETableImpl<Entry, 64>`, the
    // HUF-weight FSE table, which production caps at accuracy_log 6 (64 states;
    // `build_decoder(_, 6)`). Constrain the alphabet to <=32 symbols so the FSE
    // encoder stays inside that envelope (it needs >= one state per symbol), and
    // build with max accuracy_log 6 to match. The general high-accuracy_log FSE
    // path is exercised by the sequence-section `SeqSymbol` tables in the
    // roundtrip / cross-validation suites.
    let mapped: alloc::vec::Vec<u8> = data.iter().map(|b| b % 32).collect();
    let data = mapped.as_slice();

    if data.len() < 2 {
        return;
    }
    if data.iter().all(|x| *x == data[0]) {
        return;
    }
    if data.len() < 64 {
        return;
    }

    let mut writer = BitWriter::new();
    let mut encoder = FSEEncoder::new(
        fse_encoder::build_table_from_data(data.iter().copied(), 6, false),
        &mut writer,
    );
    let mut dec_table = FSETable::new(255);
    encoder.encode(data);
    let acc_log = encoder.acc_log();
    let enc_table = encoder.into_table();
    let encoded = writer.dump();

    let table_bytes = dec_table.build_decoder(&encoded, acc_log).unwrap();
    let encoded = &encoded[table_bytes..];
    let mut decoder = FSEDecoder::new(&dec_table);

    check_tables(&dec_table, &enc_table);

    let mut br = BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(encoded);
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
        panic!("Corrupted end marker");
    }
    decoder.init_state(&mut br).unwrap();
    let mut decoded = alloc::vec::Vec::new();

    for x in data {
        let w = decoder.decode_symbol();
        assert_eq!(w, *x);
        decoded.push(w);
        if decoded.len() < data.len() {
            decoder.update_state(&mut br);
        }
    }

    assert_eq!(&decoded, data);

    assert_eq!(br.bits_remaining(), 0);
}

#[cfg(any(test, feature = "fuzz_exports"))]
fn check_tables(dec_table: &fse_decoder::FSETable, enc_table: &fse_encoder::FSETable) {
    // Per-symbol `Vec<State>` storage was dropped in #110 — the encoder now
    // holds only `nextStateTable` (upstream zstd parity) and per-symbol
    // `symbolTT`. Verify decoder/encoder parity by enumerating every
    // valid `(symbol, input_state)` transition and asserting that:
    //   (a) `next.index` lands on a decoder slot owned by `symbol`
    //       (the transition reaches a state that decodes back to the
    //       same symbol — without this, a broken encoder routing
    //       symbol A into symbol B's slot would slip through), and
    //   (b) `baseline` / `num_bits` from the encoder match what the
    //       decoder records for that slot.
    // After enumerating, every decoder slot must have been hit at
    // least once (full coverage).
    let table_size = enc_table.table_size;
    let mut hit = alloc::vec![false; table_size];
    for symbol_u in 0..=255u16 {
        let symbol = symbol_u as u8;
        if enc_table.symbol_probability(symbol) == 0 {
            continue;
        }
        for input_state in 0..table_size {
            let next = enc_table.next_state(symbol, input_state);
            let dec_state = &dec_table.decode()[next.index];
            assert_eq!(
                dec_state.symbol, symbol,
                "encoder transition for symbol {symbol} from state {input_state} \
                 lands on decoder slot {} which decodes to symbol {} \
                 (encoder/decoder routing mismatch)",
                next.index, dec_state.symbol
            );
            assert_eq!(
                next.baseline, dec_state.new_state as usize,
                "decode/encode baseline mismatch at slot {} (symbol {symbol})",
                next.index
            );
            assert_eq!(
                next.num_bits, dec_state.num_bits,
                "decode/encode num_bits mismatch at slot {} (symbol {symbol})",
                next.index
            );
            hit[next.index] = true;
        }
    }
    for (idx, was_hit) in hit.iter().enumerate() {
        assert!(
            *was_hit,
            "decoder slot {idx} not reached by any (symbol, prev_state) encoder transition"
        );
    }
}

#[cfg(test)]
mod tests;
