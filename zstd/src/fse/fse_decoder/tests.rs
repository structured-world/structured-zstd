use super::*;
use crate::decoding::errors::FSETableError;

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
    t.symbol_probabilities = alloc::vec![512, 512];
    assert!(
        t.to_encoder_table().is_none(),
        "accuracy_log > 9 must not be reusable as an encoder table"
    );
}
