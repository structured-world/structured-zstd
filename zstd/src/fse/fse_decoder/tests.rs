use super::*;
use crate::decoding::errors::FSETableError;
use alloc::format;

/// The table-description reader as it was written before it took upstream's
/// shape: two bits at a time through every zero run, one push per symbol.
/// Kept as the reference [`read_probabilities_matches_the_reference`] holds
/// the reader to, field for field and error for error.
fn read_probabilities_reference(
    source: &[u8],
    max_log: u8,
    max_symbol: u8,
) -> Result<(u8, Vec<i32>, usize), FSETableError> {
    let mut symbol_probabilities: Vec<i32> = Vec::new();
    let total_bits = source.len() * 8;
    let mut bit_pos: usize = 0;
    fn field_at(source: &[u8], bit_pos: usize, n: usize) -> u64 {
        let byte = bit_pos >> 3;
        let remaining = &source[byte..];
        let window = if let Some(bytes) = remaining.first_chunk::<8>() {
            u64::from_le_bytes(*bytes)
        } else {
            let mut window = [0u8; 8];
            window[..remaining.len()].copy_from_slice(remaining);
            u64::from_le_bytes(window)
        };
        (window >> (bit_pos & 7)) & ((1u64 << n) - 1)
    }
    macro_rules! read_bits {
        ($n:expr) => {{
            let n: usize = $n;
            if total_bits - bit_pos < n {
                return Err(FSETableError::GetBitsError(
                    crate::bit_io::GetBitsError::NotEnoughRemainingBits {
                        requested: n,
                        remaining: total_bits - bit_pos,
                    },
                ));
            }
            let v = field_at(source, bit_pos, n);
            bit_pos += n;
            v
        }};
    }
    let accuracy_log = ACC_LOG_OFFSET + (read_bits!(4) as u8);
    if accuracy_log > ENTRY_MAX_ACCURACY_LOG {
        return Err(FSETableError::AccLogTooBig {
            got: accuracy_log,
            max: ENTRY_MAX_ACCURACY_LOG,
        });
    }
    if accuracy_log > max_log {
        return Err(FSETableError::AccLogTooBig {
            got: accuracy_log,
            max: max_log,
        });
    }
    let probability_sum = 1 << accuracy_log;
    let mut probability_counter = 0;
    while probability_counter < probability_sum {
        let max_remaining_value = probability_sum - probability_counter + 1;
        let bits_to_read = highest_bit_set(max_remaining_value);
        let unchecked_value = read_bits!(bits_to_read as usize) as u32;
        let low_threshold = ((1 << bits_to_read) - 1) - (max_remaining_value);
        let mask = (1 << (bits_to_read - 1)) - 1;
        let small_value = unchecked_value & mask;
        let value = if small_value < low_threshold {
            bit_pos -= 1;
            small_value
        } else if unchecked_value > mask {
            unchecked_value - low_threshold
        } else {
            unchecked_value
        };
        let prob = (value as i32) - 1;
        let max_symbols = max_symbol as usize + 1;
        if symbol_probabilities.len() >= max_symbols {
            return Err(FSETableError::TooManySymbols {
                got: symbol_probabilities.len() + 1,
            });
        }
        symbol_probabilities.push(prob);
        if prob != 0 {
            if prob > 0 {
                probability_counter += prob as u32;
            } else {
                probability_counter += 1;
            }
        } else {
            loop {
                let skip_amount = read_bits!(2) as usize;
                let new_len = symbol_probabilities.len() + skip_amount;
                if new_len > max_symbols {
                    return Err(FSETableError::TooManySymbols { got: new_len });
                }
                symbol_probabilities.resize(new_len, 0);
                if skip_amount != 3 {
                    break;
                }
            }
        }
    }
    if probability_counter != probability_sum {
        return Err(FSETableError::ProbabilityCounterMismatch {
            got: probability_counter,
            expected_sum: probability_sum,
            symbol_probabilities: mismatch_diagnostic(&symbol_probabilities),
        });
    }
    Ok((accuracy_log, symbol_probabilities, bit_pos.div_ceil(8)))
}

/// The table-description reader agrees with the reference on every input,
/// valid or not: the accuracy log, every probability, the bytes it reports
/// and, on a malformed description, the error and its fields. The inputs are
/// random bytes (mostly malformed, so they reach every refusal) and random
/// bytes behind a low nibble that keeps the accuracy log small, which yields
/// long zero runs and descriptions that run out of bits.
#[test]
fn read_probabilities_matches_the_reference() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut seen = [0u32; 5];
    for case in 0..200_000u32 {
        let len = (next() % 40) as usize;
        let mut source: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        if case % 2 == 0 && !source.is_empty() {
            // Accuracy log 5..=9, the sequence tables' range.
            source[0] = (source[0] & 0xF0) | (next() % 5) as u8;
        }
        if case % 3 == 0 {
            // Dense zero-run codes: many 0b11 pairs.
            for b in source.iter_mut().skip(1) {
                if next() % 2 == 0 {
                    *b = 0xFF;
                }
            }
        }
        if case % 7 == 0 {
            // Accuracy log 5 and then a stretch of all-one bits ending in
            // random bytes: once a zero probability starts a run there, its
            // "three more zeros" fields go on for many batches.
            let run = 4 + (next() % 30) as usize;
            source = alloc::vec![0x10u8];
            source.extend(core::iter::repeat_n(0xFFu8, run));
            source.extend((0..8).map(|_| next() as u8));
        }
        let max_symbol = [8u8, 31, 35, 52, 255][(next() % 5) as usize];
        let max_log = [6u8, 8, 9, 16][(next() % 4) as usize];
        let mut table = SeqFSETable::new(max_symbol);
        let ours = table.read_probabilities(&source, max_log).map(|bytes| {
            (
                table.accuracy_log,
                table.symbol_probabilities.to_vec(),
                bytes,
            )
        });
        let reference = read_probabilities_reference(&source, max_log, max_symbol);
        assert_eq!(
            format!("{ours:?}"),
            format!("{reference:?}"),
            "source {source:02x?} max_log {max_log} max_symbol {max_symbol}",
        );
        match &reference {
            Ok((_, probs, _)) => {
                seen[0] += 1;
                // A zero run longer than one batch of 16 repeat fields.
                let longest = probs
                    .split(|&p| p != 0)
                    .map(<[i32]>::len)
                    .max()
                    .unwrap_or(0);
                if longest > 48 {
                    seen[1] += 1;
                }
            }
            Err(FSETableError::TooManySymbols { got }) => {
                seen[2] += 1;
                // A run long enough to pass an alphabet of 36 or more takes
                // more than one batch of fields to get there.
                if *got > 48 {
                    seen[1] += 1;
                }
            }
            Err(FSETableError::GetBitsError(_)) => seen[3] += 1,
            Err(_) => seen[4] += 1,
        }
    }
    // Every outcome the reader has, each many times over: valid tables, a
    // valid table with a zero run past one batch, and each refusal. (A sum
    // mismatch cannot come out of the reader: each field is bounded by what
    // is left of the sum.)
    assert!(
        seen.iter().all(|&n| n >= 20),
        "outcomes [ok, long zero run, too many symbols, out of bits, \
         other] = {seen:?}",
    );
}

#[test]
fn build_from_probabilities_rejects_table_exceeding_capacity() {
    // A `SeqFSETable`'s decode array holds CAP = 512 entries (acc_log <= 9). An
    // acc_log whose `table_size` exceeds CAP must be rejected, not run the build
    // and panic-overrun the fixed 512+8 fast-spread scratch (or the `[_; 512]`
    // decode array) — `build_from_probabilities` is a public entry reachable
    // from fuzz harnesses with acc_log up to ENTRY_MAX_ACCURACY_LOG (16).
    let mut t = SeqFSETable::new(0);
    // acc_log 10 -> table_size 1024 > 512; the single symbol owns the whole
    // table, so there are no -1 entries and the fast-spread path fires.
    let res = t.build_from_probabilities(10, &[1024]);
    assert!(
        matches!(res, Err(FSETableError::AccLogTooBig { .. })),
        "table_size > CAP must be rejected, got {res:?}",
    );
}

#[test]
fn build_from_probabilities_rejects_sum_too_small() {
    // acc_log=4 → table_size=16. Valid sum (treating -1 as 1)
    // is 16. Use a distribution that sums to 8 (probs sum 8,
    // no -1s).
    let mut t = FSETable::new(8);
    let probs: [i32; 4] = [4, 2, 1, 1];
    let result = t.build_from_probabilities(4, &probs);
    assert!(
        matches!(
            result,
            Err(FSETableError::ProbabilityCounterMismatch { .. })
        ),
        "expected ProbabilityCounterMismatch for sum=8 vs expected=16, got {result:?}",
    );
}

#[test]
fn build_from_probabilities_rejects_sum_too_large() {
    // acc_log=4 → table_size=16. Probs summing to 20 (>16)
    // would over-fill the spread.
    let mut t = FSETable::new(8);
    let probs: [i32; 4] = [8, 6, 4, 2];
    let result = t.build_from_probabilities(4, &probs);
    assert!(
        matches!(
            result,
            Err(FSETableError::ProbabilityCounterMismatch { .. })
        ),
        "expected ProbabilityCounterMismatch for sum=20 vs expected=16, got {result:?}",
    );
}

#[test]
fn build_from_probabilities_accepts_negative_one_in_sum() {
    // acc_log=4 → table_size=16. Probs: 12 positive + 4 × -1
    // (counted as 1 each) sum to 16. Should succeed.
    let mut t = FSETable::new(8);
    let probs: [i32; 6] = [8, 4, -1, -1, -1, -1];
    let result = t.build_from_probabilities(4, &probs);
    assert!(
        result.is_ok(),
        "expected Ok for sum=16 with -1s, got {result:?}"
    );
}

#[test]
fn build_from_probabilities_rejects_overflow_in_sum() {
    // DoS-shaped exploit: two i32::MAX values + 0x42 cast to u32
    // and summed wrapping wraps to 0x40 = 64 = 1 << acc_log. The
    // pre-fix u32 sum check would accept the input, then
    // build_decoding_table would run `for _ in 0..prob` against
    // prob = i32::MAX, looping 2^31-1 times per such symbol.
    let mut t = FSETable::new(8);
    let probs: [i32; 3] = [i32::MAX, i32::MAX, 0x42];
    let result = t.build_from_probabilities(6, &probs);
    assert!(
        matches!(result, Err(FSETableError::InvalidProbability { .. })),
        "expected InvalidProbability for overflow-shaped exploit, got {result:?}",
    );
}

#[test]
fn build_from_probabilities_rejects_negative_below_minus_one() {
    // RFC 8878 §4.1.1: probability values are in {-1, 0, positive}.
    // p = -2 is outside the spec; clamping to 0 silently is wrong.
    let mut t = FSETable::new(8);
    let probs: [i32; 4] = [-2, 8, 4, 4];
    let result = t.build_from_probabilities(4, &probs);
    assert!(
        matches!(
            result,
            Err(FSETableError::InvalidProbability { value: -2, .. })
        ),
        "expected InvalidProbability{{value: -2, ..}}, got {result:?}",
    );
}

#[test]
fn build_from_probabilities_accepts_exact_sum() {
    // Sum 4+4+4+4 = 16 = 1 << 4. No -1s.
    let mut t = FSETable::new(8);
    let probs: [i32; 4] = [4, 4, 4, 4];
    let result = t.build_from_probabilities(4, &probs);
    assert!(result.is_ok(), "expected Ok for exact sum, got {result:?}");
}

/// More probabilities than the table's alphabet holds are refused before they
/// are stored: the store is bounded by the alphabet under `caller-workspace`,
/// so copying them first would panic instead of returning the typed error.
#[test]
fn build_from_probabilities_rejects_more_symbols_than_the_alphabet() {
    let mut t = FSETable::new(255);
    let mut probs = [0i32; 300];
    probs[0] = 16;
    let result = t.build_from_probabilities(4, &probs);
    assert!(
        matches!(result, Err(FSETableError::TooManySymbols { got: 300 })),
        "expected TooManySymbols {{ got: 300 }}, got {result:?}",
    );
}

/// A table description whose zero-probability run outgrows the alphabet is
/// refused as soon as it does, as upstream's `FSE_readNCount` stops at
/// `charnum > maxSV1`, rather than collecting zeros until the input runs out.
/// Accuracy log 5, one zero probability, then 2-bit "three more zeros" fields
/// for the rest of the input.
#[test]
fn zero_run_past_the_alphabet_is_refused_early() {
    let mut source = alloc::vec![0x10u8, 0xFE];
    source.extend(core::iter::repeat_n(0xFF, 200));
    let mut table = SeqFSETable::new(35);
    match table.build_decoder(&source, 9) {
        Err(FSETableError::TooManySymbols { got }) => assert!(got > 36 && got <= 39),
        other => panic!("expected TooManySymbols, got {other:?}"),
    }
}

/// The decoder accepts tables with `accuracy_log` up to
/// `ENTRY_MAX_ACCURACY_LOG = 16`, but the encoder table builder indexes a
/// fixed 512-entry (`1 << 9`) scratch. A decoder table with `accuracy_log >
/// 9` reached through `to_encoder_table` (e.g. a non-sequence table carried
/// by a dictionary) must return None rather than slice past the scratch and
/// panic.
#[test]
fn to_encoder_table_rejects_accuracy_log_above_nine() {
    let mut t = FSETable::new(8);
    // A table with accuracy_log 10 (1 << 10 = 1024 entries) and a plausible
    // probability distribution summing to 1024.
    t.accuracy_log = 10;
    t.symbol_probabilities.clear();
    t.symbol_probabilities.extend_from_slice(&[512, 512]);
    assert!(
        t.to_encoder_table().is_none(),
        "accuracy_log > 9 must not be reusable as an encoder table"
    );
}

/// Regression: `is_populated` must report a built table for RLE tables, not just
/// FSE-mode ones. `build_rle` sets `decode_len = 1` but leaves `accuracy_log` at
/// 0 (a valid RLE DTable's tableLog), so an `accuracy_log != 0` check would miss
/// it — and the per-frame scratch reset that gates on `is_populated` would then
/// SKIP clearing a used RLE sequence table, letting a later Repeat-mode frame
/// read stale RLE state. The "built" signal is `decode_len != 0`, matching the
/// uninitialized check `init_state` uses.
#[test]
fn is_populated_detects_built_rle_table() {
    let mut t = SeqFSETable::new(9);
    assert!(!t.is_populated(), "a fresh table must report unpopulated");
    t.build_rle(7);
    assert!(
        t.is_populated(),
        "an RLE table (decode_len = 1, accuracy_log = 0) must report populated"
    );
}

/// A rebuild that fails while reading the table description stores the new
/// accuracy log before rejecting it. The decode length must not survive from
/// the previous table: the state init trusts `decode_len == 1 << accuracy_log`
/// outside debug builds, so an RLE table's single entry under a larger log
/// would let a Repeat-mode section read past it.
#[test]
fn a_rebuild_failing_on_its_description_leaves_the_table_unbuilt() {
    let mut t = SeqFSETable::new(52);
    t.build_rle(5);
    assert!(t.is_populated());
    // Low nibble 15: accuracy log 5 + 15 = 20, past every table's limit.
    let res = t.build_decoder(&[0x0F, 0, 0, 0], 9);
    assert!(
        matches!(res, Err(FSETableError::AccLogTooBig { .. })),
        "{res:?}"
    );
    assert!(
        !t.is_populated(),
        "the RLE table's decode length outlived a failed rebuild"
    );
}
