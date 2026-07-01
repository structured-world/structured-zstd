//! Unit coverage for `HcMatcher` paths the encode-level suite
//! doesn't naturally hit: short-suffix early returns on probe
//! helpers, chain-walk self-loop branch, and the lazy-pick
//! "next match is better" decline paths.
use super::*;
use crate::encoding::match_table::storage::MatchTable;

fn table_with_history(buf: &[u8]) -> MatchTable {
    let mut t = MatchTable::new(buf.len().max(8));
    t.history = buf.to_vec();
    t.history_start = 0;
    t.history_abs_start = 0;
    t.window_size = buf.len();
    t.position_base = 0;
    t.hash_log = 8;
    t.chain_log = 8;
    t.hash3_log = 0;
    t.ensure_tables();
    // `history` is set directly above; record one live chunk.
    t.chunk_lens.push_back(buf.len());
    t
}

#[test]
fn repcode_candidate_returns_none_when_suffix_too_short() {
    let mut t = table_with_history(b"abc");
    t.offset_hist = [1, 2, 3];
    // current_idx + HC_MIN_MATCH_LEN > concat.len() → early no-match.
    assert!(!HcMatcher::repcode_candidate(&t, 0, 1).is_match());
}

#[test]
fn repcode_candidate_skips_rep_at_history_boundary() {
    // rep=5 but abs_pos=4, so candidate_pos would underflow into
    // pre-history bytes; the `rep > abs_pos` guard must skip it.
    let mut t = table_with_history(b"abcdefgh");
    t.offset_hist = [5, 6, 7];
    // No match possible at abs_pos=4 because every rep aims past
    // history start.
    let result = HcMatcher::repcode_candidate(&t, 4, 1);
    assert!(!result.is_match(), "no rep can land in-range");
}

#[test]
fn find_best_match_returns_none_for_short_suffix() {
    let hc = HcMatcher::new(2, 4, 32);
    let t = table_with_history(b"abc");
    assert!(
        !hc.find_best_match::<false>(t.live_history(), false, &t, 0, 1)
            .is_match()
    );
}

/// Forward-length selection (upstream `ZSTD_HcFindBestMatch`): the chain
/// walk keeps the longest FORWARD match (`currentMl > ml`) and applies the
/// single backward extension to THAT winner — a shorter-forward candidate
/// is NOT promoted just because it has more backward (`lit_len`) room.
/// Backward "catch up" happens once, on the forward winner, after the walk;
/// it never changes which candidate wins.
///
/// Fixture (40 bytes): `"AAAabcdefZMQabcdefIJBAAAabcdefIJKKKKKKKK"`.
/// Probing `abs_pos = 24`: the 4-byte hash at `idx 24` ("abcd") collides
/// with `idx 3` and `idx 12`, so the walk visits `[12, 3]` (LIFO).
///   - `idx 12`: forward 8 (`"abcdefIJ"`), `concat[11] = 'Q'` !=
///     `concat[23] = 'A'` so no backward room. Total 8, offset 12.
///   - `idx 3`: forward only 6 (`"abcdef"`), but `concat[0..3] = "AAA"` ==
///     `concat[21..24]` so it could backward-extend 3 to a TOTAL of 9 — yet
///     its forward length (6) loses to candidate 12's (8), so it is never
///     selected. The forward winner (12) wins at both `lit_len`s.
#[test]
fn hash_chain_candidate_picks_longest_forward_over_shorter_with_backward_room() {
    let mut t = MatchTable::new(64);
    t.history = b"AAAabcdefZMQabcdefIJBAAAabcdefIJKKKKKKKK".to_vec();
    t.history_start = 0;
    t.history_abs_start = 0;
    t.window_size = t.history.len();
    t.position_base = 0;
    t.hash_log = 8;
    t.chain_log = 8;
    t.hash3_log = 0;
    t.ensure_tables();
    t.chunk_lens.push_back(t.history.len());
    t.insert_positions(0, 24);

    let hc = HcMatcher::new(2, 16, 64);

    // The forward winner (idx 12, forward 8) has no backward room.
    let c0 = hc.hash_chain_candidate::<false>(t.live_history(), false, &t, 24);
    assert!(c0.is_match(), "forward match must be found");
    assert_eq!(c0.match_len, 8, "longest forward match is 8 (idx 12)");
    assert_eq!(
        c0.offset, 12,
        "winner is the forward-8 candidate at offset 12"
    );
}

/// Forward-length ties keep the FIRST-visited candidate (upstream
/// `ZSTD_HcFindBestMatch` uses `currentMl > ml`, strictly-longer, so an
/// equal-length later candidate never displaces the earlier one). The walk
/// is newest-first, so in organic chains "first visited" is the closest
/// (smallest-offset) position anyway; this test hand-wires the chain into a
/// non-monotonic order to pin the tie-break rule itself.
///
/// Fixture: four 8-byte `"abcdefgh"` chunks at `0 / 9 / 18 / 27`, each
/// followed by a unique terminator (`'A'/'B'/'C'/'D'`) capping cross-chunk
/// forward matches at exactly 8. Probing `abs_pos = 27` with the chain
/// hand-wired to visit pos 9 (offset 18) THEN pos 18 (offset 9): both have
/// forward length 8, so the first-visited (pos 9, offset 18) wins. The
/// self-tightening gate at `ml = 8` also rejects pos 18 (its tail byte is a
/// different chunk terminator), so the equal-length later candidate is
/// skipped before the count — consistent with ties-keep-first.
#[test]
fn hash_chain_candidate_forward_ties_keep_first_visited() {
    let mut t = MatchTable::new(64);
    t.history = b"abcdefghAabcdefghBabcdefghCabcdefghDZZZZ".to_vec();
    assert_eq!(t.history.len(), 40);
    t.history_start = 0;
    t.history_abs_start = 0;
    t.window_size = t.history.len();
    t.position_base = 0;
    t.hash_log = 8;
    t.chain_log = 8;
    t.hash3_log = 0;
    t.ensure_tables();
    t.chunk_lens.push_back(t.history.len());

    let abs_pos = 27usize;
    let concat = t.live_history();
    let probe_hash = t.hash_position(&concat[abs_pos..]);
    // Hand-wire the chain head + link so the walk surfaces pos 9 first
    // (offset 18) then pos 18 (offset 9). `stored = pos + 1`.
    t.hash_table[probe_hash] = 9 + 1;
    let chain_mask = (1usize << t.chain_log) - 1;
    t.chain_table[9 & chain_mask] = 18 + 1;
    t.chain_table[18 & chain_mask] = HC_EMPTY;

    let hc = HcMatcher::new(2, 16, 64);
    let cand = hc.hash_chain_candidate::<false>(t.live_history(), false, &t, abs_pos);
    assert!(cand.is_match(), "walk must still produce a match");
    assert_eq!(
        cand.match_len, 8,
        "both candidates have an 8-byte forward prefix"
    );
    assert_eq!(
        cand.offset, 18,
        "forward-length ties keep the first-visited candidate (pos 9, \
             offset 18); a value of 9 would mean the equal-length later \
             candidate displaced it (non-upstream gain-based tie-break)"
    );
}
