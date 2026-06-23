use super::{PARALLEL_COUNT_THRESHOLD, count_bytes, count_bytes_scalar, merge_lane_counts};

fn make_data(len: usize, seed: u64) -> alloc::vec::Vec<u8> {
    let mut state = seed;
    let mut out = alloc::vec![0u8; len];
    for byte in out.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        *byte = (state >> 32) as u8;
    }
    out
}

#[test]
fn count_bytes_matches_scalar_for_large_input() {
    let data = make_data(8192, 0xDEADBEEF);
    let mut fast = [0usize; 256];
    let mut scalar = [0usize; 256];

    let fast_meta = count_bytes(&data, &mut fast);
    let scalar_meta = count_bytes_scalar(&data, &mut scalar);

    assert_eq!(fast, scalar);
    assert_eq!(fast_meta, scalar_meta);
}

#[test]
fn count_bytes_handles_empty_input() {
    let mut counts = [123usize; 256];
    let meta = count_bytes(&[], &mut counts);

    assert_eq!(meta, (0, 0));
    assert!(counts.iter().all(|value| *value == 0));
}

#[test]
fn count_bytes_parallel_handles_tail() {
    let data = make_data(PARALLEL_COUNT_THRESHOLD + 7, 42);
    let mut fast = [0usize; 256];
    let mut scalar = [0usize; 256];

    let fast_meta = count_bytes(&data, &mut fast);
    let scalar_meta = count_bytes_scalar(&data, &mut scalar);

    assert_eq!(fast, scalar);
    assert_eq!(fast_meta, scalar_meta);
}

#[test]
fn merge_lane_counts_widens_before_sum() {
    let lane = u32::MAX / 4;
    let sum = merge_lane_counts(lane, lane, lane, lane);
    let expected = 4u64 * (lane as u64);
    assert_eq!(sum as u64, expected);
}
