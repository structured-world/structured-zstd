use super::decode_buffer::DecodeBuffer;
use crate::blocks::sequence_section::Sequence;
use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::ExecuteSequencesError;

/// Field-level variant of [`execute_sequences`] used when the caller cannot
/// hand over a full `&mut DecoderScratch` (e.g. another field is immutably
/// borrowed at the call site). Takes the same subset of fields the workspace
/// version touches and runs the identical execution loop. Used by the
/// fused `decode_and_execute_sequences` for its RLE-mode fallback.
pub(crate) fn execute_sequences_fields(
    buffer: &mut DecodeBuffer,
    literals_buffer: &[u8],
    offset_hist: &mut [u32; 3],
    sequences: &[Sequence],
) -> Result<(), ExecuteSequencesError> {
    let mut literals_copy_counter: usize = 0;
    let old_buffer_size = buffer.len();
    let mut seq_sum: u32 = 0;

    buffer.reserve(MAX_BLOCK_SIZE as usize);

    let literals_buffer_len = literals_buffer.len();
    for seq in sequences {
        let seq = *seq;
        let high = literals_copy_counter + seq.ll as usize;
        if high > literals_buffer_len {
            return Err(ExecuteSequencesError::NotEnoughBytesForSequence {
                wanted: high,
                have: literals_buffer_len,
            });
        }
        // SAFETY: literals_copy_counter <= high <= literals_buffer_len.
        let literals = unsafe { literals_buffer.get_unchecked(literals_copy_counter..high) };
        literals_copy_counter = high;
        buffer.push(literals);

        let actual_offset = do_offset_history(seq.of, seq.ll, offset_hist);
        if actual_offset == 0 {
            return Err(ExecuteSequencesError::ZeroOffset);
        }
        buffer.repeat(actual_offset as usize, seq.ml as usize)?;

        seq_sum = seq_sum.wrapping_add(seq.ml).wrapping_add(seq.ll);
    }
    if literals_copy_counter < literals_buffer_len {
        let rest_literals = &literals_buffer[literals_copy_counter..];
        buffer.push(rest_literals);
        seq_sum = seq_sum.wrapping_add(rest_literals.len() as u32);
    }

    let diff = buffer.len() - old_buffer_size;
    debug_assert_eq!(
        seq_sum as usize, diff,
        "seq_sum {seq_sum} != buffer growth {diff}"
    );
    Ok(())
}

/// Update the most recently used offsets to reflect the provided offset value, and return the
/// "actual" offset needed because offsets are not stored in a raw way, some transformations are needed
/// before you get a functional number.
pub(crate) fn do_offset_history(offset_value: u32, lit_len: u32, scratch: &mut [u32; 3]) -> u32 {
    // Fast path: offset_value >= 4 means a fresh (non-repcode) offset, which
    // is the dominant case for non-trivial corpora. Donor (zstd_decompress_block.c
    // ZSTD_updateRep) special-cases this with a straight shift: rotate the
    // history down and store `offset_value - 3` at slot 0. No rule table, no
    // branchless masks. The slow path below handles repcode 1..=3 with the
    // full RULES table dispatch.
    if offset_value >= 4 {
        let actual = offset_value - 3;
        scratch[2] = scratch[1];
        scratch[1] = scratch[0];
        scratch[0] = actual;
        return actual;
    }

    do_offset_history_repcode(offset_value, lit_len, scratch)
}

#[cold]
#[inline(never)]
fn do_offset_history_repcode(offset_value: u32, lit_len: u32, scratch: &mut [u32; 3]) -> u32 {
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
    // Indexing: class * 2 + lit_is_zero
    const RULES: [Rule; 8] = [
        // class=0 (offset_value=1)
        Rule {
            // lit_len > 0
            scratch_idx: 0,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 0,
        },
        Rule {
            // lit_len == 0
            scratch_idx: 1,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 1,
        },
        // class=1 (offset_value=2)
        Rule {
            // lit_len > 0
            scratch_idx: 1,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 1,
        },
        Rule {
            // lit_len == 0
            scratch_idx: 2,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 2,
        },
        // class=2 (offset_value=3)
        Rule {
            // lit_len > 0
            scratch_idx: 2,
            use_new_offset: false,
            subtract_one: false,
            update_mode: 2,
        },
        Rule {
            // lit_len == 0
            scratch_idx: 0,
            use_new_offset: false,
            subtract_one: true,
            update_mode: 2,
        },
        // class=3 (offset_value>=4)
        Rule {
            // lit_len > 0
            scratch_idx: 0,
            use_new_offset: true,
            subtract_one: false,
            update_mode: 2,
        },
        Rule {
            // lit_len == 0
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
    let lit_is_zero = usize::from(lit_len == 0);
    let rule = RULES[class * 2 + lit_is_zero];

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
