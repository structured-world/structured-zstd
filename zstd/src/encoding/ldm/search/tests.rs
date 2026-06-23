use super::*;
use crate::encoding::ldm::table::LdmHashTable;

fn fresh_table() -> LdmHashTable {
    // 4-bucket × 4-slot table — small enough that we can
    // hand-place candidates in known slots.
    LdmHashTable::new(4, 2)
}

/// `count_backwards_match` honours both lower bounds and
/// stops on the first mismatch. Fixture: "XXXabc" matches
/// "YYYabc" with 3 backward bytes from offset 3 in each.
#[test]
fn count_backwards_match_walks_until_mismatch_or_bound() {
    // history = "abc__abc"  positions: 0..3 = "abc",
    //                                  3..5 = "__",
    //                                  5..8 = "abc".
    // Backwards walk from p_in=8 (after "abc") and p_match=3
    // (after the first "abc") should match the 3 bytes
    // "abc".
    let history = b"abc__abc";
    let len = count_backwards_match(history, 8, 0, 3, 0);
    assert_eq!(len, 3);

    // Mismatch on the 4th byte back: '_' (history[4]) vs
    // nothing in the first window — the walk hits the
    // match_base_abs bound (0) earlier on the match side.
    let len2 = count_backwards_match(history, 5, 0, 0, 0);
    // p_match starts at 0 → match_base bound reached
    // immediately → 0 bytes matched.
    assert_eq!(len2, 0);
}

/// `anchor_abs` caps the backward walk on the in-stream side.
#[test]
fn count_backwards_match_respects_anchor_bound() {
    let history = b"aaaaaaaa";
    // anchor at position 5 → only 1 byte of leftward room.
    let len = count_backwards_match(history, 6, 5, 4, 0);
    assert_eq!(len, 1);
}

/// Bucket lookup returns `None` when every slot mismatches
/// the checksum.
#[test]
fn find_best_match_returns_none_on_checksum_mismatch() {
    let mut table = fresh_table();
    table.insert_absolute(1, 4, 0x1111_1111);
    let history = b"abcdefghabcdefgh";
    let m = find_best_match(
        &table,
        1,
        0xDEAD_BEEF,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 8,
            anchor_abs: 0,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    );
    assert!(m.is_none(), "wrong checksum must be filtered out");
}

/// Bucket lookup returns `None` when the offset is strictly
/// below `lowest_index_abs` (inclusive lower bound — entries
/// at exactly `lowest_index_abs` survive, entries below are
/// stale).
#[test]
fn find_best_match_rejects_stale_entries() {
    let mut table = fresh_table();
    table.insert_absolute(1, 4, 0xCAFE);
    let history = b"abcdefghabcdefgh";
    // lowest_index_abs = 5 → entry offset 4 is strictly below
    // → rejected. (At lowest_index_abs = 4 the entry would
    // exactly meet the floor and survive — that's the edge
    // case the inclusive bound is designed to preserve.)
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 8,
            anchor_abs: 0,
            lowest_index_abs: 5,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    );
    assert!(m.is_none(), "stale entry must be filtered out");
}

/// `find_best_match` returns the longest combined
/// forward+backward match across the bucket. Engineered
/// fixture: a 4-byte preamble (so the upstream zstd `offset > 0`
/// staleness floor is satisfied — entry.offset == 0 is the
/// reserved "empty slot" sentinel) followed by two
/// repetitions of "abcdefgh". The single candidate at
/// offset 4 should produce forward 8 + backward 0 = 8.
#[test]
fn find_best_match_picks_longest_combined_match() {
    let mut table = fresh_table();
    table.insert_absolute(1, 4, 0xCAFE);
    let history = b"PPPPabcdefghabcdefgh";
    // split at position 12, anchor at 12 → no backward room.
    // The forward count should match 8 bytes ("abcdefgh").
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 12,
            anchor_abs: 12,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    )
    .expect("a valid candidate must be found");
    assert_eq!(m.match_pos, 4);
    assert_eq!(m.forward_len, 8);
    assert_eq!(m.backward_len, 0);
    assert_eq!(m.total_len(), 8);
}

/// Backward extension picks up the bytes BEFORE `split` when
/// `anchor` allows. Fixture: "XYabcdefghXYabcdefgh" — split
/// at position 12 ('a'), anchor at 10 ('X') gives 2 bytes of
/// backward room ("XY"). Forward 8 + backward 2 = total 10.
#[test]
fn find_best_match_extends_backwards_into_pre_split_bytes() {
    let mut table = fresh_table();
    table.insert_absolute(1, 2, 0xCAFE);
    let history = b"XYabcdefghXYabcdefgh";
    // split at 12 (start of second "abcdefgh"), anchor at 10
    // → backward up to 2 bytes ("XY" at positions 10..12 vs
    // 0..2). Forward count: 8 bytes ("abcdefgh").
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 12,
            anchor_abs: 10,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    )
    .expect("a valid candidate must be found");
    assert_eq!(m.match_pos, 2);
    assert_eq!(m.forward_len, 8);
    assert_eq!(m.backward_len, 2);
    assert_eq!(m.total_len(), 10);
}

/// When the bucket holds multiple valid candidates the longer
/// combined match wins, regardless of slot order. Preamble
/// bytes shift both candidate offsets above the upstream zstd `offset
/// > 0` floor.
#[test]
fn find_best_match_prefers_longer_total_across_slots() {
    let mut table = fresh_table();
    // Slot 0: offset 4 (short forward match — only 4 bytes).
    table.insert_absolute(1, 4, 0xCAFE);
    // Slot 1: offset 8 (8-byte match — extends further forward).
    table.insert_absolute(1, 8, 0xCAFE);
    let history = b"PPPPabcdabcdefghabcdefgh";
    // split at position 16 ('a' of trailing block). Match at
    // offset 8 ("abcdefgh") gives 8 bytes forward; match at
    // offset 4 ("abcdabcd...") gives only 4 bytes forward
    // because the 5th byte ('a' vs 'e') mismatches.
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 16,
            anchor_abs: 16,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    )
    .expect("a valid candidate must be found");
    assert_eq!(m.match_pos, 8, "longer-forward winner must be picked");
    assert_eq!(m.forward_len, 8);
}

/// `find_best_match` must reject entries at or past
/// `split_abs` so the producer's `offset = split_abs −
/// match_pos` never underflows / emits a zero back-reference.
/// Regression for PR #139 round-9 review.
#[test]
fn find_best_match_rejects_entries_at_or_past_split() {
    let mut table = fresh_table();
    // Inject a stray entry at exactly `split_abs` — would be
    // structurally impossible from `LdmProducer::generate_into`
    // (inserts happen after the lookup) but possible from a
    // direct caller / test fixture / future extDict variant.
    table.insert_absolute(1, 12, 0xCAFE);
    let history = b"PPPPabcdefghabcdefgh";
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 12,
            anchor_abs: 12,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    );
    assert!(m.is_none(), "entry at split_abs must be rejected");

    // Same fixture but entry at strictly past split — also
    // must be rejected.
    let mut table_after = fresh_table();
    table_after.insert_absolute(1, 16, 0xCAFE);
    let m_after = find_best_match(
        &table_after,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 12,
            anchor_abs: 12,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    );
    assert!(m_after.is_none(), "entry past split_abs must be rejected");
}

/// Forward count must respect `iend_abs` — even when matching
/// bytes continue past the block end inside `live_history`,
/// `forward_len` is capped at `iend_abs - split_abs`.
/// Regression for PR #139 round-7 review.
#[test]
fn find_best_match_forward_count_is_bounded_by_iend_abs() {
    let mut table = fresh_table();
    table.insert_absolute(1, 4, 0xCAFE);
    // 4 preamble bytes + two 8-byte "abcdefgh" runs = 20 bytes.
    // Without iend_abs cap the match at split=12 vs match=4
    // would return forward_len = 8 ("abcdefgh"). With
    // iend_abs = 16 (4 bytes past the split) the match must
    // cap at 4.
    let history = b"PPPPabcdefghabcdefgh";
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 12,
            anchor_abs: 12,
            lowest_index_abs: 0,
            iend_abs: 16, // cap: only 4 forward bytes allowed
            min_match_length: 4,
        },
    )
    .expect("a 4-byte forward match still passes the min_match floor");
    assert_eq!(
        m.forward_len, 4,
        "forward count must cap at iend_abs - split_abs"
    );
}

/// Forward match below `min_match_length` is rejected even
/// when the checksum agrees (upstream zstd `zstd_ldm.c:444/452`).
#[test]
fn find_best_match_filters_short_forward_matches() {
    let mut table = fresh_table();
    table.insert_absolute(1, 4, 0xCAFE);
    let history = b"PPPPabXXXXXXab";
    // 2-byte forward match from split=12 vs match=4, but
    // min_match_length = 4 → rejected.
    let m = find_best_match(
        &table,
        1,
        0xCAFE,
        FindBestMatchInputs {
            live_history: history,
            history_abs_start: 0,
            split_abs: 12,
            anchor_abs: 12,
            lowest_index_abs: 0,
            iend_abs: history.len(),
            min_match_length: 4,
        },
    );
    assert!(m.is_none());
}
