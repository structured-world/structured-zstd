//! Unit coverage for the LDM helper cluster that batch 14 lifted
//! onto `impl BtMatcher`. The helpers walk the raw-LDM seq store
//! and translate its (literals, match) sequences into the optimal
//! parser's `MatchCandidate` shape. They are pure logic on
//! [`HcRawSeqStore`] / [`HcOptLdmState`] — no SIMD, no `MatchTable`
//! contact — so they exercise on every CI target.
use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::encoding::opt::ldm::{HcOptLdmState, HcRawSeq, HcRawSeqStore};

fn raw(lit: usize, mat: usize, off: usize) -> HcRawSeq {
    HcRawSeq {
        lit_length: lit,
        offset: off,
        match_length: mat,
    }
}

fn bt_with_seqs(seqs: Vec<HcRawSeq>) -> BtMatcher {
    let mut bt = BtMatcher::new();
    bt.ldm_sequences = seqs;
    bt
}

fn store(size: usize) -> HcRawSeqStore {
    HcRawSeqStore {
        pos: 0,
        pos_in_sequence: 0,
        size,
    }
}

/// Regression test for PR #139 Copilot review — threads #1/#2:
/// `prepare_ldm_candidates` must translate the absolute
/// `current_abs_start` (`history_abs_start + window_size -
/// current_len`) into a slice index inside the supplied
/// `history` slice. Before the fix, the code passed
/// `current_abs_start` directly as `block_start` to
/// `LdmProducer::generate_into`, which would index out of
/// bounds (or into the wrong byte range) as soon as
/// `history_abs_start != 0` after window eviction.
///
/// We construct a `BtMatcher` with an active `LdmProducer`
/// whose table starts empty, hand it a small `history` slice,
/// and call `prepare_ldm_candidates` with absolute coordinates
/// shifted by a non-zero `history_abs_start`. The call must
/// complete without panic — under the pre-fix code it would
/// panic on the `history[block_start..block_end]` slice access
/// inside `generate_into`.
#[test]
#[cfg(feature = "hash")]
fn prepare_ldm_candidates_translates_absolute_positions_to_slice_indices() {
    use crate::encoding::ldm::{LdmProducer, params::LdmParams};

    let mut bt = BtMatcher::new();
    // Activate the producer with hand-tuned small params —
    // the upstream zstd btultra2 defaults (`with_window_and_strategy(27, 9)`)
    // would allocate `1 << 23 = 8M` entries × 8 bytes = 64 MiB
    // for a table this test never actually populates beyond a
    // handful of entries. Build a 10-bit hash log instead so
    // the table is ~8 KiB — keeps the suite stable under
    // parallel nextest on 32-bit / memory-tight CI shards.
    bt.ldm_producer = Some(LdmProducer::new(LdmParams {
        window_log: 27,
        hash_log: 10,
        hash_rate_log: 4,
        min_match_length: 32,
        bucket_size_log: 4,
    }));

    // 256 bytes of arbitrary content — enough for the gear
    // hash to make progress against the upstream zstd btultra2
    // `min_match_length = 32`.
    let history: alloc::vec::Vec<u8> = (0u8..=255).collect();

    // Simulate a frame whose window has already evicted some
    // bytes: `history_abs_start = 1024`, and the current block
    // covers absolute `[1024, 1280)` which maps to slice
    // `[0, 256)` inside `history`.
    let history_abs_start = 1024usize;
    let current_abs_start = 1024usize;
    let current_len = history.len();

    // Pre-fix: would panic with `slice index out of bounds`
    // on `history[1024..1280]`. Post-fix: translates to
    // `[0..256]` and runs cleanly.
    bt.prepare_ldm_candidates(&history, history_abs_start, current_abs_start, current_len);
    // Cross-block fixture isn't engineered to emit a match —
    // the assertion is simply that the call succeeded.
}

#[test]
fn ldm_skip_raw_seq_store_bytes_walks_whole_sequences() {
    let bt = bt_with_seqs(vec![raw(2, 3, 100), raw(4, 1, 200), raw(0, 5, 50)]);
    let mut s = store(3);
    // 5 = first seq (lit+match=5) → advance to seq 1 with pos_in_sequence=0
    bt.ldm_skip_raw_seq_store_bytes(&mut s, 5);
    assert_eq!(s.pos, 1);
    assert_eq!(s.pos_in_sequence, 0);
}

#[test]
fn ldm_skip_raw_seq_store_bytes_leaves_partial_offset() {
    let bt = bt_with_seqs(vec![raw(2, 3, 100), raw(4, 1, 200)]);
    let mut s = store(2);
    // 4 < first seq's 5 bytes → stay on seq 0, pos_in_sequence=4
    bt.ldm_skip_raw_seq_store_bytes(&mut s, 4);
    assert_eq!(s.pos, 0);
    assert_eq!(s.pos_in_sequence, 4);
}

#[test]
fn ldm_skip_raw_seq_store_bytes_exhausts_store_when_distance_too_large() {
    let bt = bt_with_seqs(vec![raw(1, 1, 1), raw(1, 1, 1)]);
    let mut s = store(2);
    // 10 bytes total but store only holds 4 (2 seqs × 2 bytes each)
    bt.ldm_skip_raw_seq_store_bytes(&mut s, 10);
    assert_eq!(s.pos, 2);
    assert_eq!(s.pos_in_sequence, 0);
}

#[test]
fn ldm_get_next_match_empty_store_clears_window() {
    let bt = bt_with_seqs(vec![]);
    let mut opt_ldm = HcOptLdmState::default();
    opt_ldm.seq_store.size = 0;
    bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 10, 100);
    assert_eq!(opt_ldm.start_pos_in_block, usize::MAX);
    assert_eq!(opt_ldm.end_pos_in_block, usize::MAX);
}

#[test]
fn ldm_get_next_match_marks_window_inside_block() {
    let bt = bt_with_seqs(vec![raw(2, 5, 17)]);
    let mut opt_ldm = HcOptLdmState::default();
    opt_ldm.seq_store.size = 1;
    // 100 bytes remain in block, current pos 10 → window starts at
    // 10 + lit_length (2) = 12 and ends 12 + match_length (5) = 17.
    bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 10, 100);
    assert_eq!(opt_ldm.start_pos_in_block, 12);
    assert_eq!(opt_ldm.end_pos_in_block, 17);
    assert_eq!(opt_ldm.offset, 17);
}

#[test]
fn ldm_get_next_match_skips_when_literals_exceed_remaining_block() {
    let bt = bt_with_seqs(vec![raw(50, 5, 17)]);
    let mut opt_ldm = HcOptLdmState::default();
    opt_ldm.seq_store.size = 1;
    // Only 30 bytes left in block but seq starts with 50 literals →
    // window stays "no LDM in flight" and cursor advances past the
    // remaining bytes.
    bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 0, 30);
    assert_eq!(opt_ldm.start_pos_in_block, usize::MAX);
    assert_eq!(opt_ldm.end_pos_in_block, usize::MAX);
    assert_eq!(opt_ldm.seq_store.pos_in_sequence, 30);
}

#[test]
fn ldm_maybe_add_match_returns_none_before_window() {
    let bt = bt_with_seqs(vec![]);
    let opt_ldm = HcOptLdmState {
        seq_store: store(0),
        start_pos_in_block: 20,
        end_pos_in_block: 30,
        offset: 7,
    };
    assert!(bt.ldm_maybe_add_match(&opt_ldm, 10, 4).is_none());
}

#[test]
fn ldm_maybe_add_match_returns_none_past_window() {
    let bt = bt_with_seqs(vec![]);
    let opt_ldm = HcOptLdmState {
        seq_store: store(0),
        start_pos_in_block: 20,
        end_pos_in_block: 30,
        offset: 7,
    };
    assert!(bt.ldm_maybe_add_match(&opt_ldm, 30, 4).is_none());
    assert!(bt.ldm_maybe_add_match(&opt_ldm, 35, 4).is_none());
}

#[test]
fn ldm_maybe_add_match_returns_none_below_min_match() {
    let bt = bt_with_seqs(vec![]);
    let opt_ldm = HcOptLdmState {
        seq_store: store(0),
        start_pos_in_block: 20,
        end_pos_in_block: 23,
        offset: 7,
    };
    // window length is 3 < min_match (4) → reject
    assert!(bt.ldm_maybe_add_match(&opt_ldm, 20, 4).is_none());
}

#[test]
fn ldm_maybe_add_match_emits_candidate_inside_window() {
    let bt = bt_with_seqs(vec![]);
    let opt_ldm = HcOptLdmState {
        seq_store: store(0),
        start_pos_in_block: 20,
        end_pos_in_block: 30,
        offset: 7,
    };
    let cand = bt
        .ldm_maybe_add_match(&opt_ldm, 22, 4)
        .expect("should emit");
    assert_eq!(cand.start, 22);
    assert_eq!(cand.offset, 7);
    // candidate length = window - (pos - start) = 10 - 2 = 8
    assert_eq!(cand.match_len, 8);
}

#[test]
fn ldm_process_match_candidate_returns_none_when_store_exhausted() {
    let mut bt = bt_with_seqs(vec![raw(2, 5, 17)]);
    bt.ldm_sequences.clear();
    let mut opt_ldm = HcOptLdmState::default();
    opt_ldm.seq_store.size = 0;
    assert!(
        bt.ldm_process_match_candidate(&mut opt_ldm, 0, 100, 4)
            .is_none()
    );
}

#[test]
fn push_candidate_ladder_rejects_below_min_match() {
    let mut out: Vec<MatchCandidate> = vec![];
    let mut best = 0usize;
    let accepted = BtMatcher::push_candidate_ladder(
        &mut out,
        &mut best,
        MatchCandidate {
            start: 10,
            offset: 5,
            match_len: 2,
        },
        4,
    );
    assert!(!accepted);
    assert!(out.is_empty());
    assert_eq!(best, 0);
}

#[test]
fn push_candidate_ladder_rejects_when_not_strictly_better() {
    let mut out: Vec<MatchCandidate> = vec![];
    let mut best = 8usize;
    let accepted = BtMatcher::push_candidate_ladder(
        &mut out,
        &mut best,
        MatchCandidate {
            start: 10,
            offset: 5,
            match_len: 8,
        },
        4,
    );
    // match_len == best_len_for_skip → not strictly greater → rejected
    assert!(!accepted);
    assert!(out.is_empty());
    assert_eq!(best, 8);
}

#[test]
fn ldm_get_next_match_clamps_window_to_block_end() {
    // Seq match window extends past the block boundary — the
    // function clamps `end_pos_in_block` to `curr_block_end_pos`
    // and skips through the seq store accordingly.
    let bt = bt_with_seqs(vec![raw(0, 100, 99)]);
    let mut opt_ldm = HcOptLdmState::default();
    opt_ldm.seq_store.size = 1;
    bt.ldm_get_next_match_and_update_seq_store(&mut opt_ldm, 10, 30);
    // Window would naturally be [10, 110); clamped to [10, 40)
    // because remaining block bytes only allow that span.
    assert_eq!(opt_ldm.start_pos_in_block, 10);
    assert_eq!(opt_ldm.end_pos_in_block, 40);
    assert_eq!(opt_ldm.offset, 99);
}

#[test]
fn ldm_process_match_candidate_handles_overshoot_into_next_seq() {
    // Parser jumped past the active LDM window — `pos_overshoot`
    // forces the seq-store cursor forward before re-seeding.
    let bt = bt_with_seqs(vec![raw(0, 5, 17), raw(0, 5, 33)]);
    let mut opt_ldm = HcOptLdmState {
        seq_store: HcRawSeqStore {
            pos: 1,
            pos_in_sequence: 0,
            size: 2,
        },
        start_pos_in_block: 0,
        end_pos_in_block: 5,
        offset: 17,
    };
    // curr_pos = 12 (overshot the previous window's end by 7) → must
    // consume the overshoot from the next seq, then re-seed.
    let _ = bt.ldm_process_match_candidate(&mut opt_ldm, 12, 100, 4);
    // After overshoot consumes 7 bytes from seq 1 (which only had
    // 5), cursor advances to pos=2 (store exhausted).
    assert_eq!(opt_ldm.seq_store.pos, 2);
}

#[test]
fn ldm_process_match_candidate_reseeds_after_overshoot() {
    // Two sequences. Seq 0 was just emitted (cursor advanced past it),
    // so the active window points at seq 1's offset (33). The parser
    // arrives at position 5 — past the seq 0 window — and must
    // re-seed onto seq 1's window without overshoot logic re-reading
    // seq 0.
    let bt = bt_with_seqs(vec![raw(0, 5, 17), raw(0, 5, 33)]);
    let mut opt_ldm = HcOptLdmState {
        seq_store: HcRawSeqStore {
            pos: 1,
            pos_in_sequence: 0,
            size: 2,
        },
        start_pos_in_block: 0,
        end_pos_in_block: 5,
        offset: 17,
    };
    let cand = bt
        .ldm_process_match_candidate(&mut opt_ldm, 5, 100, 4)
        .expect("should emit reseeded candidate");
    assert_eq!(cand.start, 5);
    assert_eq!(cand.offset, 33);
    assert_eq!(cand.match_len, 5);
}
