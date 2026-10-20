use super::*;
use crate::decoding::legacy::v07::{LL_DEFAULT, LL_DEFAULT_LOG};
use alloc::vec;

/// A bitstream opens past the padding up to its end mark; an empty one, or
/// one whose last byte has no mark, is refused.
#[test]
fn bitstreams_open_at_their_end_mark() {
    assert!(open_bits(&[]).is_err());
    assert!(open_bits(&[0x5A, 0x00]).is_err());
    // The mark is the top set bit of the last byte: seven bits remain.
    let bits = open_bits(&[0x01, 0x01]).unwrap();
    assert_eq!(bits.bits_remaining(), 8);
    let mut bits = open_bits(&[0b1010_0101]).unwrap();
    assert_eq!(bits.bits_remaining(), 7);
    assert_eq!(bits.get_bits(7), 0b010_0101);
    assert!(!overflowed(&bits));
    bits.get_bits(1);
    assert!(overflowed(&bits));
}

/// The predefined literal length distribution spreads into a full table in
/// which every symbol it lists appears.
#[test]
fn predefined_distributions_fill_their_table() {
    let dist = Distribution::predefined(&LL_DEFAULT, LL_DEFAULT_LOG);
    let table = FseTable::new(&dist).unwrap();
    assert_eq!(table.cells.len(), 1 << LL_DEFAULT_LOG);
    for symbol in 0..LL_DEFAULT.len() as u8 {
        assert!(
            table.cells.iter().any(|cell| cell.symbol == symbol),
            "{symbol}"
        );
    }
    // Every state's successor stays inside the table.
    for cell in &table.cells {
        assert!(usize::from(cell.new_state) + (1 << cell.nb_bits) <= table.cells.len());
    }
}

/// A table built over another's storage, a larger one or one set to raw
/// bits, is the table a fresh build gives: nothing of the earlier table stays.
#[test]
fn a_table_rebuilt_in_place_is_a_fresh_one() {
    let dist = Distribution::predefined(&LL_DEFAULT, LL_DEFAULT_LOG);
    let fresh = FseTable::new(&dist).unwrap();
    let mut reused = FseTable::empty();
    reused.set_raw(9);
    reused.build(&dist).unwrap();
    assert_eq!(reused.log, fresh.log);
    assert_eq!(reused.fast, fresh.fast);
    assert_eq!(reused.cells.len(), fresh.cells.len());
    for (a, b) in reused.cells.iter().zip(&fresh.cells) {
        assert_eq!(
            (a.symbol, a.nb_bits, a.new_state),
            (b.symbol, b.nb_bits, b.new_state)
        );
    }
    // A Huffman table read over a wider one is the narrower table alone.
    let (fresh, _) = HufTable::read(&[127 + 2, 0x11], StreamEnd::PastStart).unwrap();
    let (mut reused, _) = HufTable::read(&[255], StreamEnd::PastStart).unwrap();
    reused
        .read_into(&[127 + 2, 0x11], StreamEnd::PastStart)
        .unwrap();
    assert_eq!((reused.log, &reused.cells), (fresh.log, &fresh.cells));
}

/// A distribution that does not sum to its table is refused, as one with
/// more below-one symbols than cells.
#[test]
fn a_distribution_that_does_not_fill_its_table_is_refused() {
    let mut short = LL_DEFAULT;
    short[0] -= 1;
    assert!(FseTable::new(&Distribution::predefined(&short, LL_DEFAULT_LOG)).is_err());
    let all_low = [-1i16; 40];
    assert!(FseTable::new(&Distribution::predefined(&all_low, 5)).is_err());
}

/// A description too short to hold its first word, or naming a table past
/// the largest log, is refused.
#[test]
fn bad_descriptions_are_refused() {
    assert_eq!(
        read_distribution(&[0x00, 0x00], 35).err(),
        Some(Error::Truncated)
    );
    // Log 5 + 15 = 20.
    assert!(read_distribution(&[0x0F, 0, 0, 0], 35).is_err());
}

/// A raw table reads every symbol as its width, from the stream's end
/// backward; a one-symbol table reads nothing.
#[test]
fn raw_and_rle_tables_read_as_described() {
    let mut raw = FseTable::empty();
    raw.set_raw(5);
    // Under the end mark: 11010 then 11, padded with zeros past the start.
    let mut bits = open_bits(&[0b1110_1011]).unwrap();
    let mut state = FseState::new(&raw, &mut bits);
    assert_eq!(state.symbol(&raw), 0b11010);
    state.update(&raw, &mut bits);
    assert_eq!(state.symbol(&raw), 0b11000);
    assert!(overflowed(&bits));

    // Set over the raw table's storage, as a block's next mode is.
    let mut rle = raw;
    rle.set_rle(42);
    let mut bits = open_bits(&[0x80]).unwrap();
    let mut state = FseState::new(&rle, &mut bits);
    for _ in 0..3 {
        assert_eq!(state.symbol(&rle), 42);
        state.update(&rle, &mut bits);
    }
    assert_eq!(bits.bits_remaining(), 7);
}

/// A description byte of 242 and up is a run of weight-1 symbols: 242 is
/// one, which with the implied last symbol makes a one-bit code.
#[test]
fn a_weight_run_makes_a_flat_code() {
    let (table, consumed) = HufTable::read(&[242], StreamEnd::PastStart).unwrap();
    assert_eq!(consumed, 1);
    assert_eq!(table.log, 1);
    // One stream of bits 1, 0, 1 then the end mark: symbols 1, 0, 1.
    let mut out = [0u8; 3];
    table.decode_one_stream(&[0b0000_1101], &mut out).unwrap();
    assert_eq!(out, [1, 0, 1]);
    // A stream with a bit left over is refused.
    let mut out = [0u8; 2];
    assert!(table.decode_one_stream(&[0b0000_1101], &mut out).is_err());
}

/// Weights stored as nibbles (a header of `127 + n` for `n` weights): symbols
/// 0 and 1 of weight 1 and the implied symbol 2 of weight 2 make codes of 2,
/// 2 and 1 bits.
#[test]
fn nibble_weights_build_their_code() {
    let end = StreamEnd::PastStart;
    let (table, consumed) = HufTable::read(&[127 + 2, 0x11], end).unwrap();
    assert_eq!(consumed, 2);
    assert_eq!(table.log, 2);
    assert_eq!(table.cells, [(0, 2), (1, 2), (2, 1), (2, 1)]);
    // A nibble section cut short is truncated.
    assert_eq!(
        HufTable::read(&[127 + 4, 0x11], end).err(),
        Some(Error::Truncated)
    );
    // Weights 3 and 1 sum to 5, which no last weight brings to a power of
    // two.
    assert!(HufTable::read(&[127 + 2, 0x31], end).is_err());
    // A weight of 16 is refused.
    assert!(HufTable::read(&[127 + 2, 0xF1], end).is_err());
}

/// Four streams fill the quarters of the output in order, and below six
/// literals the quarters past the output decode into nothing kept.
#[test]
fn four_streams_fill_their_quarters() {
    let (table, _) = HufTable::read(&[242], StreamEnd::PastStart).unwrap();
    // Each stream: one byte, symbols 0 and 1 under the end mark.
    let stream = 0b0000_0101u8;
    let mut src = vec![1, 0, 1, 0, 1, 0];
    src.extend_from_slice(&[stream, stream, stream, stream]);
    let mut out = [9u8; 8];
    table.decode_four_streams(&src, &mut out).unwrap();
    assert_eq!(out, [0, 1, 0, 1, 0, 1, 0, 1]);

    // Five literals: quarters of two, the last quarter empty. Its stream
    // must still end exactly, so it holds only the end mark.
    let mut src = vec![1, 0, 1, 0, 1, 0];
    src.extend_from_slice(&[stream, stream, stream, 0x01]);
    let mut out = [9u8; 5];
    table.decode_four_streams(&src, &mut out).unwrap();
    assert_eq!(out, [0, 1, 0, 1, 0]);

    // A jump table pointing past the section is refused.
    let mut src = vec![200, 0, 1, 0, 1, 0];
    src.extend_from_slice(&[stream, stream, stream, stream]);
    assert!(table.decode_four_streams(&src, &mut out).is_err());
}

/// Arbitrary bytes read as FSE or Huffman descriptions, and as Huffman
/// streams under whatever table they described, are refused or decode, but
/// never panic or read past their input: the descriptions in a damaged
/// frame are attacker-chosen.
#[test]
fn arbitrary_descriptions_never_panic() {
    // xorshift64: deterministic, so a failure reproduces.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut table = FseTable::empty();
    let mut huf = HufTable::empty();
    for _ in 0..20_000 {
        let len = (next() % 24) as usize + 1;
        let src: vec::Vec<u8> = (0..len).map(|_| next() as u8).collect();
        for max_symbol in [28, 35, 52, 255] {
            if let Ok((dist, consumed)) = read_distribution(&src, max_symbol) {
                assert!(consumed <= src.len());
                // A description may still ask for a table too large to build.
                let _built = table.build(&dist).is_ok();
            }
        }
        for end in [StreamEnd::PastStart, StreamEnd::Exact] {
            let Ok(consumed) = huf.read_into(&src, end) else {
                continue;
            };
            assert!(consumed <= src.len());
            let stream: vec::Vec<u8> = (0..(next() % 16) as usize + 1)
                .map(|_| next() as u8)
                .collect();
            let mut out = vec![0u8; (next() % 32) as usize + 1];
            let _one = huf.decode_one_stream(&stream, &mut out).is_ok();
            let _four = huf.decode_four_streams(&stream, &mut out).is_ok();
        }
    }
}

/// A run of zero counts read where fewer than five bytes are left shifts the
/// held bits instead of reloading them, as `FSEv07_readNCount` does; a
/// description that then lists no table's worth of counts is refused.
#[test]
fn a_zero_run_at_the_end_shifts_the_held_bits() {
    // Log 5; symbol 0 has count 0; sixteen run bits of 1, a run of 24 more.
    let src = [0x10, 0xFE, 0xFF, 0x01];
    assert!(read_distribution(&src, 255).is_err());
}

/// An RFC 8878 table description with more probabilities than the field has
/// symbols, or none, is refused.
#[test]
fn probabilities_past_the_field_are_refused() {
    assert!(Distribution::from_probabilities(&[1; 40], 5, 31).is_err());
    assert!(Distribution::from_probabilities(&[], 5, 31).is_err());
    assert!(Distribution::from_probabilities(&[32], 5, 31).is_ok());
}

/// A distribution whose low-probability symbols take every cell leaves none
/// for the rest: refused rather than spread.
#[test]
fn a_distribution_that_overfills_its_table_is_refused() {
    let mut counts = [-1i16; 33];
    counts[32] = 1;
    let dist = Distribution::predefined(&counts, 5);
    assert!(FseTable::new(&dist).is_err());
}

/// An RFC 8878 Huffman description is held to the v0.5-v0.7 limits: no code
/// longer than its table, no weight past 15, no table past 12 bits.
#[test]
fn code_lengths_past_the_formats_limits_are_refused() {
    let mut table = HufTable::empty();
    assert!(table.fill_from_code_lengths(&[5, 1, 1], 3).is_err());
    // A code of length 1 under 20 bits is a weight of 20.
    assert!(table.fill_from_code_lengths(&[1, 1, 0], 20).is_err());
    // Weights 13 down to 1, one each, and an implied last 1: a 13-bit table.
    let lengths: vec::Vec<u8> = (1..=13).chain([0]).collect();
    assert!(table.fill_from_code_lengths(&lengths, 13).is_err());
    // The same shape one bit shorter fits.
    let lengths: vec::Vec<u8> = (1..=12).chain([0]).collect();
    assert!(table.fill_from_code_lengths(&lengths, 12).is_ok());
}
