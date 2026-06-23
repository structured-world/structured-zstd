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
