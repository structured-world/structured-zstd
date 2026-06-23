use super::{STREAM_ABS_HEADROOM, check_stream_abs_headroom};

#[test]
fn accepts_exactly_at_the_boundary() {
    // `history_abs_start + window_size + data_len + STREAM_ABS_HEADROOM == usize::MAX`.
    let history_abs_start = usize::MAX - STREAM_ABS_HEADROOM - 2;
    check_stream_abs_headroom(history_abs_start, 1, 1);
}

#[test]
fn accepts_well_below_the_boundary() {
    check_stream_abs_headroom(0, 1 << 20, 1 << 20);
}

#[test]
#[should_panic(expected = "STREAM_ABS_HEADROOM")]
fn rejects_one_byte_past_the_boundary() {
    // One byte over: sum = usize::MAX + 1 → checked_add returns None.
    let history_abs_start = usize::MAX - STREAM_ABS_HEADROOM - 1;
    check_stream_abs_headroom(history_abs_start, 1, 1);
}

#[test]
#[should_panic(expected = "STREAM_ABS_HEADROOM")]
fn rejects_history_abs_start_already_too_high() {
    check_stream_abs_headroom(usize::MAX - 10, 0, 0);
}
