use super::prefetch;
use super::scratch::DecoderScratch;
use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::ExecuteSequencesError;

/// Take the provided decoder and execute the sequences stored within
pub fn execute_sequences(scratch: &mut DecoderScratch) -> Result<(), ExecuteSequencesError> {
    let mut literals_copy_counter = 0;
    let old_buffer_size = scratch.buffer.len();
    let mut seq_sum = 0;

    // Reserve once for the maximum possible decoded block output (128 KB per
    // the zstd spec). This avoids repeated re-allocations inside the hot
    // execute loop without an extra scan over the sequence vector, and is
    // inherently bounded against corrupted inputs.
    scratch.buffer.reserve(MAX_BLOCK_SIZE as usize);

    for idx in 0..scratch.sequences.len() {
        let seq = scratch.sequences[idx];
        prefetch_literals_n_plus_two(scratch, idx, literals_copy_counter);

        if seq.ll > 0 {
            let high = literals_copy_counter + seq.ll as usize;
            if high > scratch.literals_buffer.len() {
                return Err(ExecuteSequencesError::NotEnoughBytesForSequence {
                    wanted: high,
                    have: scratch.literals_buffer.len(),
                });
            }
            let literals = &scratch.literals_buffer[literals_copy_counter..high];
            literals_copy_counter += seq.ll as usize;

            scratch.buffer.push(literals);
        }

        let actual_offset = do_offset_history(seq.of, seq.ll, &mut scratch.offset_hist);
        if actual_offset == 0 {
            return Err(ExecuteSequencesError::ZeroOffset);
        }
        if seq.ml > 0 {
            scratch
                .buffer
                .repeat(actual_offset as usize, seq.ml as usize)?;
        }

        seq_sum += seq.ml;
        seq_sum += seq.ll;
    }
    if literals_copy_counter < scratch.literals_buffer.len() {
        let rest_literals = &scratch.literals_buffer[literals_copy_counter..];
        scratch.buffer.push(rest_literals);
        seq_sum += rest_literals.len() as u32;
    }

    let diff = scratch.buffer.len() - old_buffer_size;
    assert!(
        seq_sum as usize == diff,
        "Seq_sum: {} is different from the difference in buffersize: {}",
        seq_sum,
        diff
    );
    Ok(())
}

/// Update the most recently used offsets to reflect the provided offset value, and return the
/// "actual" offset needed because offsets are not stored in a raw way, some transformations are needed
/// before you get a functional number.
fn do_offset_history(offset_value: u32, lit_len: u32, scratch: &mut [u32; 3]) -> u32 {
    #[derive(Copy, Clone)]
    struct Rule {
        scratch_idx: usize,
        use_new_offset: bool,
        subtract_one: bool,
        update_mode: u8,
    }

    // update_mode:
    // 0 = no history update
    // 1 = [actual, old0, old2]
    // 2 = [actual, old0, old1]
    const RULES_LIT_NON_ZERO: [Rule; 4] = [
        Rule {
            scratch_idx: 0,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 0,
        },
        Rule {
            scratch_idx: 1,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 1,
        },
        Rule {
            scratch_idx: 2,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 2,
        },
        Rule {
            scratch_idx: 0,
            use_new_offset: true,
            subtract_one: false,
            update_mode: 2,
        },
    ];
    const RULES_LIT_ZERO: [Rule; 4] = [
        Rule {
            scratch_idx: 1,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 1,
        },
        Rule {
            scratch_idx: 2,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 2,
        },
        Rule {
            scratch_idx: 0,
            use_new_offset: false,
            subtract_one: true,
            update_mode: 2,
        },
        Rule {
            scratch_idx: 0,
            use_new_offset: true,
            subtract_one: false,
            update_mode: 2,
        },
    ];

    #[inline(always)]
    fn mask_from_bool(cond: bool) -> u32 {
        0u32.wrapping_sub(u32::from(cond))
    }

    #[inline(always)]
    fn select_u32(a: u32, b: u32, choose_b: bool) -> u32 {
        let mask = mask_from_bool(choose_b);
        (a & !mask) | (b & mask)
    }

    let valid_offset = offset_value != 0;
    let class = offset_value.saturating_sub(1).min(3) as usize;
    let rule = if lit_len > 0 {
        RULES_LIT_NON_ZERO[class]
    } else {
        RULES_LIT_ZERO[class]
    };

    let from_history = scratch[rule.scratch_idx];
    let from_new = offset_value.wrapping_sub(3);
    let mut actual_offset = select_u32(from_new, from_history, !rule.use_new_offset);
    actual_offset = actual_offset.wrapping_sub(u32::from(rule.subtract_one));
    actual_offset = select_u32(actual_offset, 0, !valid_offset);

    let old0 = scratch[0];
    let old1 = scratch[1];
    let old2 = scratch[2];

    let update_none = rule.update_mode == 0 || !valid_offset;
    let update_b = rule.update_mode == 2 && valid_offset;
    let update_any = !update_none;

    scratch[0] = select_u32(old0, actual_offset, update_any);
    scratch[1] = select_u32(old0, old1, update_none);
    scratch[2] = select_u32(old2, old1, update_b);

    actual_offset
}

#[inline(always)]
fn prefetch_literals_n_plus_two(scratch: &DecoderScratch, idx: usize, literals_cursor: usize) {
    if idx + 2 >= scratch.sequences.len() {
        return;
    }

    let ll_curr = scratch.sequences[idx].ll as usize;
    let ll_next = scratch.sequences[idx + 1].ll as usize;
    let ll_n2 = scratch.sequences[idx + 2].ll as usize;
    if ll_n2 < 64 {
        return;
    }

    let start = literals_cursor + ll_curr + ll_next;
    let end = start + ll_n2;
    if end <= scratch.literals_buffer.len() {
        prefetch::prefetch_slice(&scratch.literals_buffer[start..end]);
    }
}

#[cfg(test)]
mod tests {
    use super::do_offset_history;

    #[test]
    fn offset_history_lit_non_zero_rep1_keeps_history() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(1, 5, &mut hist);
        assert_eq!(actual, 10);
        assert_eq!(hist, [10, 20, 30]);
    }

    #[test]
    fn offset_history_lit_non_zero_rep2_rotates_first_two() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(2, 5, &mut hist);
        assert_eq!(actual, 20);
        assert_eq!(hist, [20, 10, 30]);
    }

    #[test]
    fn offset_history_lit_non_zero_rep3_full_rotate() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(3, 5, &mut hist);
        assert_eq!(actual, 30);
        assert_eq!(hist, [30, 10, 20]);
    }

    #[test]
    fn offset_history_lit_zero_rep1_uses_second_history() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(1, 0, &mut hist);
        assert_eq!(actual, 20);
        assert_eq!(hist, [20, 10, 30]);
    }

    #[test]
    fn offset_history_lit_zero_rep2_uses_third_history() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(2, 0, &mut hist);
        assert_eq!(actual, 30);
        assert_eq!(hist, [30, 10, 20]);
    }

    #[test]
    fn offset_history_lit_zero_rep3_minus_one() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(3, 0, &mut hist);
        assert_eq!(actual, 9);
        assert_eq!(hist, [9, 10, 20]);
    }

    #[test]
    fn offset_history_new_offset_path() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(9, 1, &mut hist);
        assert_eq!(actual, 6);
        assert_eq!(hist, [6, 10, 20]);
    }

    #[test]
    fn offset_history_zero_offset_preserves_error_path() {
        let mut hist = [10, 20, 30];
        let actual = do_offset_history(0, 1, &mut hist);
        assert_eq!(actual, 0);
        assert_eq!(hist, [10, 20, 30]);
    }
}
