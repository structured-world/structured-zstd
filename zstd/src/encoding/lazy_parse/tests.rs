/// Minimal match shape the `lazy_decide!` macro needs (`.match_len` / `.offset`).
#[derive(Clone, Copy, Debug)]
struct M {
    match_len: usize,
    offset: usize,
}

/// Regression: at `lazy_depth >= 2`, when the `abs_pos + 2` probe wins but
/// `abs_pos + 1` has no match (cold bucket), the macro returns `Some(None)` —
/// a DEFER that carries nothing. It is NOT `None` (commit). A consumer that
/// collapsed `Some(None)` into commit would skip the depth-2 deferral and miss
/// the two-ahead match (the P1 the HC carry path hit).
#[test]
fn defers_without_carry_when_two_ahead_wins_and_one_ahead_is_cold() {
    let decision = lazy_decide!(
        best_len = 5usize,
        best_off = 50usize,
        target_len = usize::MAX,
        lazy_depth = 2u8,
        abs_pos = 10usize,
        lit_len = 0usize,
        history_end = 1000usize,
        min_match = 4usize,
        // abs_pos + 1 == 11 -> None (cold); abs_pos + 2 == 12 -> len 7 > best+1.
        search = |p, _l| {
            if p == 12 {
                Some(M {
                    match_len: 7,
                    offset: 100,
                })
            } else {
                None
            }
        },
    );
    assert!(
        matches!(decision, Some(None)),
        "two-ahead win with a cold one-ahead must DEFER without carry (Some(None)), got {decision:?}"
    );
}

/// No lookahead beats the current best -> COMMIT (`None`).
#[test]
fn commits_when_no_lookahead_improves() {
    let decision = lazy_decide!(
        best_len = 8usize,
        best_off = 50usize,
        target_len = usize::MAX,
        lazy_depth = 2u8,
        abs_pos = 10usize,
        lit_len = 0usize,
        history_end = 1000usize,
        min_match = 4usize,
        search = |_p, _l| Option::<M>::None,
    );
    assert!(
        decision.is_none(),
        "no improving lookahead must COMMIT (None), got {decision:?}"
    );
}

/// A strictly longer match one byte ahead -> DEFER carrying it (`Some(Some)`).
#[test]
fn defers_with_carry_when_one_ahead_is_longer() {
    let decision = lazy_decide!(
        best_len = 5usize,
        best_off = 50usize,
        target_len = usize::MAX,
        lazy_depth = 1u8,
        abs_pos = 10usize,
        lit_len = 0usize,
        history_end = 1000usize,
        min_match = 4usize,
        search = |p, _l| {
            if p == 11 {
                Some(M {
                    match_len: 7,
                    offset: 100,
                })
            } else {
                None
            }
        },
    );
    assert!(
        matches!(decision, Some(Some(m)) if m.match_len == 7),
        "a longer one-ahead match must DEFER carrying it, got {decision:?}"
    );
}

/// The `target_len` sufficient-length early-out commits immediately (the OPT
/// parser's shortcut; lazy passes `usize::MAX` to disable it).
#[test]
fn commits_immediately_when_best_reaches_target_len() {
    let decision = lazy_decide!(
        best_len = 64usize,
        best_off = 50usize,
        target_len = 32usize,
        lazy_depth = 2u8,
        abs_pos = 10usize,
        lit_len = 0usize,
        history_end = 1000usize,
        min_match = 4usize,
        // Would win if probed, but the target-length early-out fires first.
        search = |_p, _l| {
            Some(M {
                match_len: 999,
                offset: 1,
            })
        },
    );
    assert!(
        decision.is_none(),
        "best >= target_len must COMMIT without probing, got {decision:?}"
    );
}
