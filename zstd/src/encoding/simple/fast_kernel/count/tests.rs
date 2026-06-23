use super::*;

fn count(a: &[u8], b: &[u8]) -> usize {
    let min_len = a.len().min(b.len());
    // SAFETY: both slices have at least `min_len` readable bytes,
    // `iend = a.as_ptr() + min_len` stays in range.
    unsafe { count_forward(a.as_ptr(), b.as_ptr(), a.as_ptr().add(min_len)) }
}

#[test]
fn empty_inputs_return_zero() {
    // Empty range → loop body never executes.
    let a: [u8; 0] = [];
    let b: [u8; 0] = [];
    // SAFETY: iend == ip, the function never dereferences.
    let n = unsafe { count_forward(a.as_ptr(), b.as_ptr(), a.as_ptr()) };
    assert_eq!(n, 0);
}

#[test]
fn full_match_inside_8_byte_chunk() {
    let a = [1, 2, 3, 4, 5, 6, 7, 8];
    let b = [1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(count(&a, &b), 8);
}

#[test]
fn diff_at_byte_3_in_first_chunk() {
    let a = [1, 2, 3, 9, 5, 6, 7, 8];
    let b = [1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(count(&a, &b), 3);
}

#[test]
fn match_spanning_two_chunks() {
    let mut a = [0u8; 16];
    let mut b = [0u8; 16];
    for i in 0..16 {
        a[i] = i as u8;
        b[i] = i as u8;
    }
    a[13] = 99;
    assert_eq!(count(&a, &b), 13);
}

#[test]
fn match_terminates_at_iend_within_tail() {
    // 11 bytes: 1×8-chunk + 3 tail bytes (u16 + u8 fall-through).
    let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    assert_eq!(count(&a, &b), 11);
}

#[test]
fn diff_in_u32_tail() {
    // 12 bytes: 1×8-chunk match, then 4-byte tail diverges at
    // BYTE INDEX 9 (`99` vs `10`). After the 8-chunk advances
    // ip/m by 8, the upstream zstd's u32 tail check compares
    // a[8..12]=[9,99,11,12] vs b[8..12]=[9,10,11,12] → unequal,
    // so the u32 advance is skipped. Same for u16
    // (a[8..10]=[9,99] vs b[8..10]=[9,10] → unequal). The single
    // byte cmp THEN sees a[8]=9 == b[8]=9 and advances ip by 1.
    // Final match length: 8 + 1 = 9.
    let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 99, 11, 12];
    let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    assert_eq!(count(&a, &b), 9);
}

#[test]
fn diff_in_u16_tail_after_u32_match() {
    // 14 bytes total, first 12 match, then u16 differs at byte 12.
    let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 99, 14];
    let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
    // 8 chunk + 4 u32 = 12 matched. u16 cmp on (99,14) vs (13,14)
    // says unequal → 0 more. Single byte cmp on 99 vs 13 → 0 more.
    assert_eq!(count(&a, &b), 12);
}

#[test]
fn diff_in_single_byte_tail() {
    // 13 bytes, first 12 match, then single byte differs.
    let a = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 99];
    let b = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
    // 8 chunk + 4 u32 = 12; u16 cmp not entered (only 1 byte left).
    // Single byte cmp 99 != 13 → 0 more.
    assert_eq!(count(&a, &b), 12);
}

#[test]
fn long_match_thousand_bytes() {
    let a = [0x5Au8; 1024];
    let b = [0x5Au8; 1024];
    assert_eq!(count(&a, &b), 1024);
}

#[test]
fn no_match_first_byte() {
    let a = [0u8, 1, 2, 3, 4, 5, 6, 7];
    let b = [9u8, 1, 2, 3, 4, 5, 6, 7];
    assert_eq!(count(&a, &b), 0);
}

#[test]
fn dict_2segment_within_dict_only() {
    // Candidate fully inside the dict; current input matches the dict tail.
    let dict = [10u8, 20, 30, 40];
    let inp = [30u8, 40, 99];
    // cand=2: dict[2]=30 vs inp[0]=30 ✓; dict[3]=40 vs inp[1]=40 ✓;
    // cand_idx=4 == dict.len() → inp[0]=30 vs inp[2]=99 ✗ → len 2.
    assert_eq!(count_forward_dict_2segment(&dict, 2, &inp, 0), 2);
}

#[test]
fn dict_2segment_crosses_boundary_into_input() {
    // Match starts in the dict and continues past the boundary into the
    // input (the logical [dict][input] window), then stops at a mismatch.
    let dict = [1u8, 2, 3];
    let inp = [1u8, 2, 3, 1, 2, 3, 9]; // cur=3 → [1,2,3,9...]
    // cand=0: dict[0..3] match inp[3..6]; cand_idx=3 → inp[0]=1 vs inp[6]=9 ✗ → 3.
    assert_eq!(count_forward_dict_2segment(&dict, 0, &inp, 3), 3);
}

#[test]
fn dict_2segment_continues_word_at_a_time_past_boundary() {
    // Candidate exhausts the dict mid-match and keeps matching into the
    // input segment — exercises the segment-2 count_forward path.
    let dict = [1u8, 2, 3];
    let inp = [1u8, 2, 3, 1, 2, 3, 1, 2]; // cur=3 → [1,2,3,1,2]
    // seg1: dict[0..3]=[1,2,3] vs inp[3..6]=[1,2,3] → m1=3 (dict exhausted).
    // seg2: inp[0..]=[1,2,...] vs inp[6..]=[1,2] → m2=2. total 5.
    assert_eq!(count_forward_dict_2segment(&dict, 0, &inp, 3), 5);
}

#[test]
fn dict_2segment_stops_at_input_end() {
    let dict = [7u8, 7];
    let inp = [7u8, 7, 7, 7]; // cur=2 → only 2 bytes left
    assert_eq!(count_forward_dict_2segment(&dict, 0, &inp, 2), 2);
}
