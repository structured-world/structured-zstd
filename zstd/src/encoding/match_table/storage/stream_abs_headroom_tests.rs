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

#[test]
#[should_panic(expected = "STREAM_ABS_HEADROOM")]
fn counts_bytes_already_carried_by_the_ingest_buffer() {
    // In-place ingest tops the buffer up by `block_capacity - carried`, and at
    // EOF it re-inspects with a capacity of zero. The bytes already carried
    // still become window on the next commit, so the guard has to count them:
    // sized on the top-up alone, the check passes and the commit then walks
    // past the headroom the unchecked `abs_pos + N` lookahead relies on.
    // Room for one top-up but not for two: the second call's own `capacity`
    // fits, while `carried + capacity` does not.
    let carried = 64usize;
    let history_abs_start = usize::MAX - STREAM_ABS_HEADROOM - carried - 1;
    let mut table = super::MatchTable::new(1 << 16);
    table.history_abs_start = history_abs_start;
    table.fill_uncommitted(carried, |buf| {
        buf.resize(carried, 0);
        (carried, false)
    });
    // The pre-split kept everything, so the next read tops up by the same
    // amount again while those bytes are still pending.
    table.fill_uncommitted(carried, |buf| {
        buf.resize(2 * carried, 0);
        (carried, true)
    });
}
