use super::*;

#[test]
fn new_uses_level_1_defaults() {
    let m = FastKernelMatcher::new();
    assert_eq!(m.window_log, FAST_LEVEL_1_WINDOW_LOG);
    assert_eq!(m.hash_table.hash_log(), FAST_LEVEL_1_HASH_LOG);
    assert_eq!(m.hash_table.mls(), FAST_LEVEL_1_MLS);
    assert_eq!(m.rep, FAST_INITIAL_REP);
    assert_eq!(m.offset_hist, FAST_INITIAL_OFFSET_HIST);
    assert_eq!(m.max_window_size, 1usize << FAST_LEVEL_1_WINDOW_LOG);
    // M8: history starts empty (HISTORY_DRAIN_BASE = 0).
    // First input byte will live at position 0; sentinel-0
    // protection comes from the prefix filter, not from a
    // dummy region.
    assert_eq!(m.history.len(), HISTORY_DRAIN_BASE);
    // prefix_start_index = 1 makes position 0 unmatchable so
    // the hash table's empty-slot value 0 can't be confused
    // with a real match.
    assert_eq!(m.prefix_start_index, INITIAL_PREFIX_START_INDEX);
    assert!(m.pending.is_none());
}

#[test]
fn borrowed_window_reads_match_owned_then_restores() {
    let mut m = FastKernelMatcher::new();
    // Owned path: history_bytes mirrors the owned buffer.
    m.history = b"owned-history-bytes".to_vec();
    assert_eq!(m.history_bytes(), b"owned-history-bytes");

    // Borrowed path: history_bytes views the caller's buffer, not
    // the owned one, and the owned buffer is left untouched.
    let external = b"a-different-borrowed-window".to_vec();
    // SAFETY: `external` outlives every history_bytes() call below
    // and is cleared before it drops at end of scope.
    unsafe { m.set_borrowed_window(&external) };
    assert_eq!(m.history_bytes(), &external[..]);
    assert_eq!(m.history, b"owned-history-bytes");

    // Clearing returns to the owned path with the original bytes.
    m.clear_borrowed_window();
    assert_eq!(m.history_bytes(), b"owned-history-bytes");
}

#[test]
fn reset_clears_borrowed_window() {
    let mut m = FastKernelMatcher::new();
    let external = b"borrowed".to_vec();
    // SAFETY: `external` outlives the reset call below.
    unsafe { m.set_borrowed_window(&external) };
    m.reset(
        FAST_LEVEL_1_WINDOW_LOG,
        FAST_LEVEL_1_HASH_LOG,
        FAST_LEVEL_1_MLS,
        2,
        false,
        false,
    );
    // After reset the borrowed window is dropped — back to the
    // (now empty) owned buffer, never the dangling external range.
    assert!(m.borrowed.is_none());
    assert_eq!(m.history_bytes().len(), HISTORY_DRAIN_BASE);
}

/// The borrowed one-shot scan must emit the EXACT same sequence
/// stream (and end with the same rep / offset_hist state) as the
/// owned block-by-block path. This is the load-bearing correctness
/// check for the borrowed window: the one-shot frame path relies on
/// it producing identical output without the per-block history copy.
#[test]
fn borrowed_window_matches_owned_sequence_stream() {
    #[derive(PartialEq, Debug)]
    enum Seq {
        Triple(alloc::vec::Vec<u8>, usize, usize),
        Lits(alloc::vec::Vec<u8>),
    }

    // Repeating pattern so the matcher emits real matches, split into
    // two blocks. Window (1 << 15 = 32 KiB) far exceeds the input, so
    // the owned path never evicts — its accumulated `history` is
    // byte-identical to the borrowed buffer, the precondition for
    // stream equality.
    let mut whole = alloc::vec::Vec::with_capacity(320);
    for i in 0..320usize {
        whole.push((i % 64) as u8);
    }
    let split = 192usize;

    // Owned path: commit each block, then scan.
    let mut owned = FastKernelMatcher::with_params(15, 12, 5, 2);
    let mut owned_seqs: alloc::vec::Vec<Seq> = alloc::vec::Vec::new();
    owned.accept_data(whole[..split].to_vec());
    owned.start_matching(|seq| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => owned_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => owned_seqs.push(Seq::Lits(literals.to_vec())),
    });
    owned.accept_data(whole[split..].to_vec());
    owned.start_matching(|seq| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => owned_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => owned_seqs.push(Seq::Lits(literals.to_vec())),
    });

    // Borrowed path: same bytes, scanned in place by block range.
    let mut borrowed = FastKernelMatcher::with_params(15, 12, 5, 2);
    let mut borrowed_seqs: alloc::vec::Vec<Seq> = alloc::vec::Vec::new();
    // SAFETY: `whole` outlives both scans below; `borrowed` is dropped
    // at end of scope before `whole`, so the window never dangles.
    unsafe { borrowed.set_borrowed_window(&whole) };
    borrowed.start_matching_borrowed(0, split, |seq| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => borrowed_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => borrowed_seqs.push(Seq::Lits(literals.to_vec())),
    });
    borrowed.start_matching_borrowed(split, whole.len(), |seq| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => borrowed_seqs.push(Seq::Triple(literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => borrowed_seqs.push(Seq::Lits(literals.to_vec())),
    });

    assert_eq!(
        owned_seqs, borrowed_seqs,
        "borrowed window must emit the identical sequence stream as the owned path",
    );
    assert_eq!(
        owned.rep, borrowed.rep,
        "rep state must match after both scans"
    );
    assert_eq!(
        owned.offset_hist, borrowed.offset_hist,
        "offset_hist must match after both scans",
    );
    // The borrowed path must have produced at least one match (else
    // the test would trivially pass on an all-literals stream).
    assert!(
        borrowed_seqs.iter().any(|s| matches!(s, Seq::Triple(..))),
        "pattern must yield at least one match to make the check meaningful",
    );
}

#[test]
fn with_params_threads_through_each_field() {
    // Pick a non-default triple to prove no silent override by
    // upstream zstd-default constants.
    let m = FastKernelMatcher::with_params(16, 12, 5, 2);
    assert_eq!(m.window_log, 16);
    assert_eq!(m.hash_table.hash_log(), 12);
    assert_eq!(m.hash_table.mls(), 5);
    assert_eq!(m.max_window_size, 1usize << 16);
}

#[test]
fn window_size_reports_one_shifted_window_log() {
    // window_log = 16 → 64 KiB reported window.
    let m = FastKernelMatcher::with_params(16, 12, 5, 2);
    assert_eq!(m.window_size(), 1u64 << 16);
    // Larger window_log → larger reported window. window_log = 22
    // (4 MiB) confirms the shift width (`u64` head room).
    let m = FastKernelMatcher::with_params(22, 14, 7, 2);
    assert_eq!(m.window_size(), 1u64 << 22);
}

#[test]
fn last_committed_space_empty_before_commit() {
    let m = FastKernelMatcher::new();
    assert!(m.last_committed_space().is_empty());
}

#[test]
fn reset_clears_history_and_state() {
    let mut m = FastKernelMatcher::new();
    // Simulate prior-frame state — non-empty history, advanced
    // prefix, non-default rep/offset stacks, a leftover pending
    // block. Reset must restore the matcher to a from-scratch
    // appearance regardless of which fields were dirtied.
    m.history.extend_from_slice(&[1, 2, 3, 4]);
    m.prefix_start_index = 7;
    m.rep = [42, 99];
    m.offset_hist = [10, 20, 30];
    m.pending = Some(alloc::vec![5, 6, 7]);

    m.reset(
        FAST_LEVEL_1_WINDOW_LOG,
        FAST_LEVEL_1_HASH_LOG,
        FAST_LEVEL_1_MLS,
        2,
        false,
        false,
    );

    // Post-reset: history empty (HISTORY_DRAIN_BASE=0; no
    // dummy only; prefix_start_index pinned to that baseline.
    assert_eq!(m.history.len(), HISTORY_DRAIN_BASE);
    assert_eq!(m.prefix_start_index, INITIAL_PREFIX_START_INDEX);
    assert_eq!(m.rep, FAST_INITIAL_REP);
    assert_eq!(m.offset_hist, FAST_INITIAL_OFFSET_HIST);
    assert!(m.pending.is_none());
    // Hash-table identity preserved (same shape) — `clear()` path,
    // not a fresh `new()`. Equality test is over the params, not
    // the buffer pointer, because the `Vec`-internal allocation
    // identity is an internal detail the test should not lock in.
    assert_eq!(m.hash_table.hash_log(), FAST_LEVEL_1_HASH_LOG);
    assert_eq!(m.hash_table.mls(), FAST_LEVEL_1_MLS);
}

#[test]
fn reset_with_changed_params_rebuilds_hash_table() {
    let mut m = FastKernelMatcher::new();
    // Force a parameter change — every Vec we hand the new
    // FastHashTable will be a fresh allocation.
    m.reset(16, 10, 4, 2, false, false);
    assert_eq!(m.hash_table.hash_log(), 10);
    assert_eq!(m.hash_table.mls(), 4);
    assert_eq!(m.window_log, 16);
    assert_eq!(m.max_window_size, 1usize << 16);
}

// The copy-mode restore fast path: a same-shape reset with
// `table_overwritten_by_restore` must leave the table contents
// untouched (the snapshot restore replaces them right after), while
// the plain same-shape reset must memset them. Guards against a
// refactor silently reintroducing the wasted per-frame clear — or,
// worse, dropping the clear on the plain path.
#[test]
fn reset_keeps_table_when_overwritten_by_restore() {
    let mut m = FastKernelMatcher::new();
    m.reset(16, 10, 4, 2, false, false);
    let probe_hash = 7u32;
    // SAFETY: hash 7 < (1 << hash_log = 1024) table entries.
    unsafe { m.hash_table.put(probe_hash, 0xCAFE) };

    // Same shape + restore-pending: contents survive the reset.
    m.reset(16, 10, 4, 2, false, true);
    // SAFETY: same bounds as the put above.
    assert_eq!(
        unsafe { m.hash_table.get(probe_hash) },
        0xCAFE,
        "restore-pending reset must not clear the table"
    );

    // Plain same-shape reset: contents are memset back to empty.
    m.reset(16, 10, 4, 2, false, false);
    // SAFETY: same bounds as the put above.
    assert_eq!(
        unsafe { m.hash_table.get(probe_hash) },
        0,
        "plain reset must clear the table"
    );

    // Shape change overrides the flag: the table is rebuilt at the
    // new geometry even when a restore is claimed to be pending.
    unsafe { m.hash_table.put(probe_hash, 0xCAFE) };
    m.reset(16, 11, 4, 2, false, true);
    assert_eq!(m.hash_table.hash_log(), 11);
    // SAFETY: hash 7 < (1 << 11) table entries.
    assert_eq!(
        unsafe { m.hash_table.get(probe_hash) },
        0,
        "shape change must rebuild the table regardless of the flag"
    );
}

/// Drive the matcher through a single block whose tail contains
/// a repeated 4-byte run — the kernel must emit at least one
/// `Sequence::Triple` with `match_len >= 4` and the bookkeeping
/// invariant `Σ(literals + match_len) + tail_literals_len ==
/// input.len()` must hold.
#[test]
fn accept_then_start_matching_emits_match_for_repeated_run() {
    // 64 bytes: 32 bytes of pseudo-random preamble + 32-byte
    // verbatim copy of bytes [0..32]. The kernel scanning the
    // tail should find the 32-byte repeat with offset = 32.
    let mut data = alloc::vec::Vec::with_capacity(64);
    for i in 0..32u8 {
        // Spread the byte values so 4-byte windows are all
        // distinct (avoids accidental rep hits that would skew
        // the assertion).
        data.push(i.wrapping_mul(7).wrapping_add(13));
    }
    data.extend_from_within(0..32);
    // Use a small mls=4 table so the test exercises the simpler
    // hash arm; level-1 defaults (mls=7) would also work but the
    // hash collisions on a 64-byte synthetic input are noisier
    // for mls>=5.
    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    m.accept_data(data.clone());

    let mut emitted_match_lens: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    let mut emitted_literal_byte_count: usize = 0;
    let mut tail_byte_count: usize = 0;
    m.start_matching(|seq| match seq {
        Sequence::Triple {
            literals,
            offset: _,
            match_len,
        } => {
            emitted_literal_byte_count += literals.len();
            emitted_match_lens.push(match_len);
        }
        Sequence::Literals { literals } => {
            tail_byte_count += literals.len();
        }
    });

    let total_matched: usize = emitted_match_lens.iter().sum();
    assert_eq!(
        emitted_literal_byte_count + total_matched + tail_byte_count,
        data.len(),
        "all input bytes must be accounted for as literals/matches/tail",
    );
    assert!(
        emitted_match_lens.iter().any(|&m| m >= 4),
        "kernel must emit at least one Triple with match_len >= MIN_MATCH (got {emitted_match_lens:?})",
    );
    // Pending buffer was consumed.
    assert!(m.pending.is_none());
    // History grew by exactly the block size (plus the
    // no dummy carried since construction — M8).
    assert_eq!(m.history.len(), data.len() + HISTORY_DRAIN_BASE);
    // `last_committed_space` post-processing reads from
    // history[last_block_start..] (upstream zstd / legacy MatchGenerator
    // parity for the frame compressor's raw-block emission
    // path) — for a single-block-then-process flow it equals
    // the input data verbatim.
    assert_eq!(m.last_committed_space(), data.as_slice());
}

/// Skip path: `skip_matching` must move the pending buffer into
/// history WITHOUT emitting any sequences and WITHOUT touching
/// the rep / offset_hist state.
#[test]
fn skip_matching_extends_history_without_emissions() {
    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    let pre_rep = m.rep;
    let pre_offset_hist = m.offset_hist;

    let payload: alloc::vec::Vec<u8> = (0..40u8).collect();
    m.accept_data(payload.clone());
    // Take a count of state pre-skip.
    assert_eq!(m.last_committed_space().len(), payload.len());

    m.skip_matching_with_hint(None);

    assert_eq!(
        m.history.len(),
        payload.len() + HISTORY_DRAIN_BASE,
        "skip_matching must append the pending buffer to history",
    );
    assert_eq!(m.rep, pre_rep, "skip must not touch rep state");
    assert_eq!(
        m.offset_hist, pre_offset_hist,
        "skip must not touch offset_hist",
    );
    assert!(m.pending.is_none());
}

/// Two-block run with literal block then matchable block — the
/// SECOND `start_matching` must find a cross-block match against
/// the first block's bytes (cross-block matches are the headline
/// reason for keeping the hash table persistent across blocks).
///
/// Sizing rationale: the kernel's main loop only scans up to
/// `ilimit = data.len() - HASH_READ_SIZE` (upstream zstd parity). Block
/// 2 must therefore carry enough trailing bytes past the
/// crossblock-match start for `ip0` to actually reach the copy.
/// We use a 128-byte block 1 and a 64-byte block 2 with the
/// 32-byte copy of block 1's prefix landing at block-2 offset
/// 16, leaving plenty of headroom under `ilimit`.
#[test]
fn cross_block_match_finds_first_block_payload() {
    // Block 1: 128-byte pattern, distinct 4-byte windows.
    let mut block1 = alloc::vec::Vec::with_capacity(128);
    for i in 0..128u8 {
        block1.push(i.wrapping_mul(11).wrapping_add(5));
    }
    // Block 2: 16 fresh bytes followed by a 32-byte verbatim copy
    // of block 1's [0..32]. The matcher must reach back into
    // block 1's bytes (offset 128+16-0 = 144 ≈ length of block 1
    // plus the leading fresh bytes of block 2). Tail (16 bytes
    // past the copy) gives `ip0` enough room to reach the copy
    // before hitting `ilimit`.
    let mut block2 = alloc::vec::Vec::with_capacity(64);
    block2.extend(0..16u8); // 16 fresh bytes (different from block1)
    block2.extend_from_slice(&block1[0..32]); // 32-byte cross-block copy
    block2.extend(200..216u8); // 16-byte tail buffer

    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);

    // Block 1 — drain emissions, ignore.
    m.accept_data(block1.clone());
    m.start_matching(|_seq| {});

    // Block 2 — capture emissions.
    m.accept_data(block2.clone());
    let mut max_match: usize = 0;
    let mut saw_cross_block = false;
    m.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
        {
            max_match = max_match.max(match_len);
            // Cross-block match: offset must reach back into
            // block 1, i.e. offset > position-within-block-2.
            // Block 2's payload starts at history position
            // `block1.len()`; the source is in block 1 when
            // offset >= block2.len() (offset measured from ip0
            // backwards, so a block-1 source means offset
            // exceeds any block-2-internal distance).
            if offset >= block2.len() {
                saw_cross_block = true;
            }
        }
    });

    assert!(
        saw_cross_block,
        "block 2's matcher must find at least one cross-block match \
             (max_len={max_match})",
    );
    assert_eq!(
        m.history.len(),
        block1.len() + block2.len() + HISTORY_DRAIN_BASE,
        "history must hold both blocks after two start_matching calls",
    );
}

/// Dictionary-priming skip: `skip_matching(Some(false))` MUST
/// pre-populate the hash table for the just-appended range so a
/// subsequent `start_matching` can find matches against the
/// dict-primed bytes. Without that pre-population, a future
/// block that copies the dict prefix verbatim would emit only
/// literals.
#[test]
fn skip_matching_with_false_hint_populates_hashes_for_dict_priming() {
    // Stage: 32 bytes "dict" via skip_matching(Some(false)),
    // then a second block whose tail copies the dict prefix.
    // Without the hash pre-population the kernel can't reach
    // the dict bytes in block 2.
    let mut dict_block = alloc::vec::Vec::with_capacity(32);
    for i in 0..32u8 {
        dict_block.push(i.wrapping_mul(13).wrapping_add(7));
    }

    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    m.accept_data(dict_block.clone());
    m.skip_matching_with_hint(Some(false)); // dictionary-priming skip

    // Sanity: history grew, prefix_start_index unchanged.
    assert_eq!(m.history.len(), dict_block.len() + HISTORY_DRAIN_BASE);
    assert_eq!(m.prefix_start_index, INITIAL_PREFIX_START_INDEX);

    // Block 2: 16 fresh bytes + 16-byte copy of dict_block[0..16]
    // + 16-byte tail buffer so the kernel can reach the copy.
    let mut block2 = alloc::vec::Vec::with_capacity(48);
    block2.extend(100..116u8);
    block2.extend_from_slice(&dict_block[0..16]);
    block2.extend(120..136u8);
    m.accept_data(block2.clone());

    let mut saw_cross_block = false;
    m.start_matching(|seq| {
        if let Sequence::Triple { offset, .. } = seq
            && offset >= block2.len()
        {
            saw_cross_block = true;
        }
    });

    assert!(
        saw_cross_block,
        "skip_matching(Some(false)) must populate hashes so block 2 \
             can match against the primed bytes",
    );
}

/// Control case for the prime-path test: same setup but with
/// `skip_matching(None)` — the bytes are NOT hashed, so block 2
/// must NOT find the cross-block match.
#[test]
fn skip_matching_with_none_hint_skips_hash_population() {
    let mut dict_block = alloc::vec::Vec::with_capacity(32);
    for i in 0..32u8 {
        dict_block.push(i.wrapping_mul(13).wrapping_add(7));
    }

    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    m.accept_data(dict_block.clone());
    m.skip_matching_with_hint(None); // plain skip — no hash pre-population

    let mut block2 = alloc::vec::Vec::with_capacity(48);
    block2.extend(100..116u8);
    block2.extend_from_slice(&dict_block[0..16]);
    block2.extend(120..136u8);
    m.accept_data(block2.clone());

    let mut saw_cross_block = false;
    m.start_matching(|seq| {
        if let Sequence::Triple { offset, .. } = seq
            && offset >= block2.len()
        {
            saw_cross_block = true;
        }
    });

    assert!(
        !saw_cross_block,
        "skip_matching(None) must NOT populate hashes — the legacy \
             skip cost-savings only hold when future blocks are willing \
             to miss matches in the skipped region",
    );
}

/// Window eviction: when total history would exceed `2 ×
/// max_window_size`, the matcher must drain the oldest prefix
/// down to a `max_window_size` tail BEFORE appending the new
/// block, bump `prefix_start_index`, and clear the hash table.
///
/// Post-append history can still hold up to
/// `max_window_size + block_size` bytes (the kernel needs the
/// just-arrived block for matching plus the retained prefix for
/// cross-block lookups). The hard upper bound is therefore the
/// eviction threshold itself: `2 × max_window_size`.
#[test]
fn extend_history_drains_old_prefix_past_two_window_sizes() {
    // window_log = 8 → max_window_size = 256, eviction threshold
    // = 512. Stage three 200-byte blocks: after the third commit,
    // total would be 600 > 512 → eviction fires.
    let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
    for round in 0..3 {
        // Distinct payload per round so a hash entry from round
        // 0 referencing position 0 is identifiable as stale
        // after eviction.
        let block: alloc::vec::Vec<u8> = (0..200u8)
            .map(|i| i.wrapping_add(round as u8 * 17))
            .collect();
        m.accept_data(block);
        m.skip_matching_with_hint(None);
    }
    // Hard bound: post-append history can hold up to
    // `max_window_size + block_size` (retained prefix + the
    // just-appended block). The eviction policy keeps total
    // strictly below `2 × max_window_size` for the next
    // accept_data call, so the invariant we assert here is the
    // post-append upper bound.
    assert!(
        m.history.len() <= m.max_window_size * 2 + HISTORY_DRAIN_BASE,
        "after eviction, REAL history must be bounded by 2× \
             max_window_size (got history.len()={}, max_window_size={})",
        m.history.len(),
        m.max_window_size,
    );
    assert!(
        m.history.len() <= m.max_window_size + 200 + HISTORY_DRAIN_BASE,
        "post-append history = retained prefix (≤ max_window_size) \
             + last block (200 bytes); got {}",
        m.history.len(),
    );
    // Post-fix: drain RESETS prefix_start_index back to 1 (the
    // initial sentinel-0 baseline) rather than accumulating
    // saturating_add — see the `drain_rebases_prefix_start_index`
    // regression test for the full rationale. Eviction is
    // proven by post-history shrinking, not by an
    // index-advancement signal.
    assert_eq!(
        m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
        "drain must rebase prefix_start_index to the baseline (1)",
    );
}

/// Boundary: exactly `HASH_READ_SIZE` (8) real bytes appended
/// via `skip_matching(Some(false))` — must hash one position
/// (`range_start == HISTORY_DRAIN_BASE`, `last_hashable ==
/// HISTORY_DRAIN_BASE`) without overrun.
#[test]
fn skip_matching_dict_prime_handles_exactly_hash_read_size_bytes() {
    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    // 8-byte real payload appended to history → history.len() =
    // 8 + HISTORY_DRAIN_BASE (= 8 under M8), last_hashable =
    // HISTORY_DRAIN_BASE (= 0), hashed range = [0..=0] (one
    // position).
    let payload: alloc::vec::Vec<u8> = (0..8u8).collect();
    m.accept_data(payload);
    m.skip_matching_with_hint(Some(false));
    assert_eq!(m.history.len(), 8 + HISTORY_DRAIN_BASE);
    // No assertion on hash entries — the bug we're guarding
    // against is a panic / overrun, not a behavioural one.
    // Reaching this line without unwinding is the test.
}

/// Boundary: pending block too short to hash anything (less than
/// `HASH_READ_SIZE` bytes). The dict-prime path must early-return
/// without panicking on the `last_hashable` subtract.
#[test]
fn skip_matching_dict_prime_handles_below_hash_read_size_bytes() {
    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    let payload: alloc::vec::Vec<u8> = (0..4u8).collect();
    m.accept_data(payload);
    // history will be 4 bytes after append < HASH_READ_SIZE (8).
    // prime_hash_table_for_range must short-circuit on the
    // `history_len < HASH_READ_SIZE` guard.
    m.skip_matching_with_hint(Some(false));
    assert_eq!(m.history.len(), 4 + HISTORY_DRAIN_BASE);
}

/// Regression: priming a dictionary in two chunks must hash the
/// positions straddling the chunk seam. The second chunk's prime
/// starts at `range_start = end-of-first-chunk`, but a hash read at
/// any of the preceding `HASH_READ_SIZE - 1` positions spans the
/// seam into the second chunk — those positions belong to neither
/// chunk's `[range_start ..= last_hashable]` window unless the second
/// prime backfills them. Without the backfill a dictionary fed in
/// `slice_size` chunks silently drops every seam-spanning match.
#[test]
fn dict_prime_indexes_positions_across_chunk_seam() {
    // hash_log=20 (1 Mi slots) makes a hash collision among ~32
    // primed positions negligible, so `get(hash(p)) == p` is a
    // deterministic assertion on this fixed input.
    let mut m = FastKernelMatcher::with_params(20, 20, 4, 2);
    let chunk1: alloc::vec::Vec<u8> = (0..16u8)
        .map(|i| i.wrapping_mul(37).wrapping_add(13))
        .collect();
    m.accept_data(chunk1);
    m.skip_matching_for_dict_prime();
    let seam = m.history.len(); // end of first chunk
    let chunk2: alloc::vec::Vec<u8> = (16..32u8)
        .map(|i| i.wrapping_mul(37).wrapping_add(13))
        .collect();
    m.accept_data(chunk2);
    m.skip_matching_for_dict_prime();

    // A position in the (HASH_READ_SIZE - 1)-byte gap just below the
    // seam: its 8-byte hash read straddles the chunk boundary.
    let p = seam - 4;
    let dt = m.dict.table().expect("dict table primed");
    // Read the dict table exactly as the kernel does: hash the position and
    // return the stored dict position for that slot (0 if empty).
    let found = unsafe {
        crate::encoding::simple::fast_kernel::kernel::dict_lookup::<4>(
            dt,
            m.history.as_ptr().add(p),
            dt.hash_log(),
        )
    };
    assert_eq!(
        found, p as u32,
        "seam-spanning position {p} must be indexed in the dict table",
    );
}

#[test]
fn block_samples_match_dict_fires_only_on_extendable_dict_match() {
    // Distinct dict bytes so the 4-byte hashes are collision-free at
    // hash_log=20 and a verbatim run is the only way to hit the table.
    let dict: alloc::vec::Vec<u8> = (0..200u8)
        .map(|i| i.wrapping_mul(37).wrapping_add(13))
        .collect();
    let mut m = FastKernelMatcher::with_params(20, 20, 4, 2);
    m.accept_data(dict.clone());
    m.skip_matching_for_dict_prime();
    assert!(m.dict_is_attached(), "dict must be attached after prime");

    // A 32-byte verbatim dict run extends well past the 16-byte useful-match
    // floor → the probe fires.
    let hit = dict[8..40].to_vec();
    assert!(
        m.block_samples_match_dict(&hit),
        "a long verbatim dict run must trip the dict probe",
    );

    // An 8-byte dict prefix followed by non-dict bytes stays BELOW the
    // 16-byte floor → the probe must NOT fire (a short coincidental match
    // does not signal a dict that compresses the block).
    let mut short = dict[8..16].to_vec();
    short.extend((0..56u8).map(|i| i.wrapping_mul(53).wrapping_add(201)));
    assert!(
        !m.block_samples_match_dict(&short),
        "a sub-16-byte dict match must not trip the probe",
    );

    // A high-entropy block sharing no extendable run with the dict → no hit.
    let miss: alloc::vec::Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(91).wrapping_add(7) ^ 0xA5)
        .collect();
    assert!(
        !m.block_samples_match_dict(&miss),
        "a non-dict block must not trip the probe",
    );

    // A block too short to hash (< HASH_READ_SIZE) → false, no panic.
    assert!(
        !m.block_samples_match_dict(&dict[..4]),
        "a sub-8-byte block cannot be probed",
    );
}

#[test]
fn block_samples_match_dict_is_false_without_a_dictionary() {
    // No dict primed → the probe short-circuits to false (the no-dict path
    // never reaches it, but the guard must hold).
    let m = FastKernelMatcher::with_params(20, 20, 4, 2);
    let block: alloc::vec::Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(37).wrapping_add(13))
        .collect();
    assert!(!m.dict_is_attached());
    assert!(!m.block_samples_match_dict(&block));
}

/// Regression: same seam-gap defect for the MAIN hash table, primed
/// per slice via `skip_matching_with_hint(Some(false))` (the dict
/// COPY path and multi-slice history priming). Cross-slice matches
/// at the seam are dropped unless the second prime backfills the
/// trailing `HASH_READ_SIZE - 1` positions.
#[test]
fn main_table_prime_indexes_positions_across_slice_seam() {
    let mut m = FastKernelMatcher::with_params(20, 20, 4, 2);
    let chunk1: alloc::vec::Vec<u8> = (0..16u8)
        .map(|i| i.wrapping_mul(53).wrapping_add(7))
        .collect();
    m.accept_data(chunk1);
    m.skip_matching_with_hint(Some(false));
    let seam = m.history.len();
    let chunk2: alloc::vec::Vec<u8> = (16..32u8)
        .map(|i| i.wrapping_mul(53).wrapping_add(7))
        .collect();
    m.accept_data(chunk2);
    m.skip_matching_with_hint(Some(false));

    let p = seam - 4;
    let h = unsafe { m.hash_table.hash_ptr::<4>(m.history.as_ptr().add(p)) };
    assert_eq!(
        unsafe { m.hash_table.get(h) },
        p as u32,
        "seam-spanning position {p} must be indexed in the main table",
    );
}

/// After a single block emits matches, the matcher's `rep[0]`
/// (kernel's `rep_offset1` post-block) must reflect the last emitted
/// explicit offset — that is the state the next block's kernel probes
/// against. The matcher's `offset_hist` is NOT updated by matching:
/// the Fast backend drives repcode probes off `rep`, and the wire
/// offset coding is done downstream against the encode pipeline's own
/// offset history, so per-match `encode_offset_with_history` on the
/// matcher would be redundant work. `offset_hist` therefore stays at
/// whatever `reset` / `prime_offset_history` set it to.
#[test]
fn rep_tracks_last_explicit_offset_and_offset_hist_is_not_matched() {
    // Engineer a single block that produces a deterministic
    // explicit match. 96 bytes: 48-byte distinct-window
    // preamble + 48-byte verbatim copy of bytes [0..48].
    let mut data = alloc::vec::Vec::with_capacity(96);
    for i in 0..48u8 {
        data.push(i.wrapping_mul(11).wrapping_add(3));
    }
    data.extend_from_within(0..48);

    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    m.accept_data(data.clone());

    let mut emitted_offsets: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    m.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
            && match_len >= 4
        {
            emitted_offsets.push(offset);
        }
    });

    assert!(
        !emitted_offsets.is_empty(),
        "test setup must produce at least one explicit match",
    );
    let last_explicit = emitted_offsets[emitted_offsets.len() - 1];
    // rep[0] is the functional state: the kernel updates it from its
    // own result each block and the NEXT block probes against it.
    assert_eq!(
        m.rep[0] as usize, last_explicit,
        "kernel's rep[0] must reflect the last emitted explicit offset",
    );
    // offset_hist is NOT touched by matching on the Fast backend — it
    // stays at the construction default (FAST_INITIAL_OFFSET_HIST).
    assert_eq!(
        m.offset_hist, FAST_INITIAL_OFFSET_HIST,
        "Fast matching must not mutate offset_hist (it is seeded only \
             by reset / prime_offset_history; the wire offset coding runs \
             downstream against the encode pipeline's own history)",
    );
}

/// Eviction during a dict-priming sequence: when consecutive
/// `skip_matching(Some(false))` calls accumulate past the
/// eviction threshold, the second skip must drop the older
/// prime'd hash entries (via `hash_table.clear()`) AND bump
/// `prefix_start_index` past the dropped bytes. Otherwise the
/// matcher would carry stale absolute positions referencing
/// evicted history.
#[test]
fn eviction_during_dict_priming_drops_stale_prime_entries() {
    // window_log=8 → max_window_size=256, threshold=512.
    // Two 300-byte blocks both via dict-prime skip — second
    // one triggers eviction.
    let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
    let block1: alloc::vec::Vec<u8> = (0..200u8).collect();
    m.accept_data(block1);
    m.skip_matching_with_hint(Some(false));
    let block2: alloc::vec::Vec<u8> = (0..200u8).map(|i| i.wrapping_add(50)).collect();
    m.accept_data(block2);
    // Second skip would push total to 400, still under 512 — no
    // eviction yet. Make sure two more rounds trigger it.
    m.skip_matching_with_hint(Some(false));
    let block3: alloc::vec::Vec<u8> = (0..200u8).map(|i| i.wrapping_add(100)).collect();
    m.accept_data(block3);
    // Now 400+200=600 > 512 → eviction fires inside extend.
    m.skip_matching_with_hint(Some(false));

    // Post-fix: drain rebases prefix_start_index to 1 (rather
    // than cumulative saturating_add); eviction is proven by
    // bounded history below.
    assert_eq!(
        m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
        "drain must rebase prefix_start_index to the baseline (1)",
    );
    // History within the 2× window-size hard cap.
    assert!(m.history.len() <= m.max_window_size * 2);
}

/// Regression for #216 review #1: `accept_data` MUST perform
/// window eviction immediately so the driver's `commit_space`
/// can observe the byte delta via a pre/post `history.len()`
/// comparison. Without commit-time eviction visibility, the
/// driver's `retire_dictionary_budget` never runs for this
/// backend → `max_window_size` stays inflated post-dict-prime
/// → matcher can emit offsets exceeding the frame header's
/// reported window (format-correctness risk).
#[test]
fn accept_data_evicts_eagerly_so_commit_observes_byte_delta() {
    // window_log = 8 → max_window_size = 256, eviction threshold
    // = 512. Stage three 200-byte blocks via accept_data + a
    // start_matching cycle each so history accumulates without
    // eviction (200, 400 bytes). The THIRD accept_data crosses
    // the 512-byte threshold; its eviction MUST be visible at
    // accept_data return-time via the history.len() drop.
    let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);

    m.accept_data((0..200u8).collect());
    m.skip_matching_with_hint(None);
    assert_eq!(m.history.len(), 200 + HISTORY_DRAIN_BASE);
    assert_eq!(
        m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
        "no eviction yet"
    );

    m.accept_data((0..200u8).map(|i| i.wrapping_add(50)).collect());
    m.skip_matching_with_hint(None);
    assert_eq!(m.history.len(), 400 + HISTORY_DRAIN_BASE);
    assert_eq!(
        m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
        "still no eviction (400 < 512)",
    );

    // Third commit: real history (400) + new space (200) = 600 > 512.
    // Eviction MUST fire inside accept_data, dropping history
    // back to max_window_size (256) BEFORE the kernel runs.
    // `history_len_for_eviction_accounting` returns the real-data
    // length (history.len() minus HISTORY_DRAIN_BASE, which is 0
    // under M8), so pre/post compare cleanly in real-byte units.
    let pre = m.history_len_for_eviction_accounting();
    m.accept_data((0..200u8).map(|i| i.wrapping_add(100)).collect());
    let post = m.history_len_for_eviction_accounting();
    assert!(
        pre > post,
        "accept_data must shrink history at the eviction threshold \
             (pre={pre}, post={post}) — driver's commit_space relies on \
             this delta for retire_dictionary_budget accounting",
    );
    assert_eq!(
        post, 256,
        "post-eviction retained must equal max_window_size",
    );
    assert_eq!(
        m.prefix_start_index, INITIAL_PREFIX_START_INDEX,
        "drain rebases prefix_start_index to INITIAL_PREFIX_START_INDEX \
             — eviction is proven by the history.len() shrink above",
    );
}

/// Regression for #216 review #2: `trim_to_window` must update
/// `last_block_start` to track the drain. Without the update,
/// the OLD position references pre-drain coordinates and
/// `last_committed_space()` would either panic with OOB or
/// return wrong bytes when `last_block_start > history.len()`
/// post-drain.
#[test]
fn trim_to_window_keeps_last_committed_space_consistent() {
    // window_log = 8 → max_window_size = 256. Process a 200-byte
    // block (now in history at positions [0..200], last_block_start
    // = 0). Then bump the matcher's max_window_size DOWN to 128
    // (simulating a dictionary-budget retire shrinking the
    // window) and call trim_to_window — drain_n = 200 - 128 = 72.
    // Post-drain history is bytes [72..200] = 128 bytes. The
    // last_block_start (was 0) MUST now be 0 (since 72 > 0 →
    // saturating_sub gives 0) so last_committed_space() returns
    // a valid in-bounds slice.
    let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
    let payload: alloc::vec::Vec<u8> = (0..200u8).collect();
    m.accept_data(payload);
    m.skip_matching_with_hint(None);
    // First block lands at history[RESERVED..RESERVED+200] after
    // the seed dummy at [0..RESERVED). last_block_start tracks
    // that absolute index.
    assert_eq!(m.last_block_start, HISTORY_DRAIN_BASE);
    assert_eq!(m.history.len(), 200 + HISTORY_DRAIN_BASE);

    // Shrink the window and trim. Without the fix, last_block_start
    // stays mid-history past the post-drain end — to make the
    // bug surface, use a SECOND block so last_block_start is
    // somewhere AFTER the dummy + first block.
    let payload2: alloc::vec::Vec<u8> = (50..150u8).collect();
    m.accept_data(payload2);
    m.skip_matching_with_hint(None);
    // history = [dummy] + [0..200] + [50..150] = 1 + 200 + 100 = 301.
    // last_block_start = RESERVED + 200 = start of second block.
    assert_eq!(m.last_block_start, HISTORY_DRAIN_BASE + 200);
    assert_eq!(m.history.len(), 300 + HISTORY_DRAIN_BASE);

    // Now force trim_to_window to drain into the middle of the
    // second block: shrink max_window_size below the second
    // block's start. trim_to_window operates on REAL data so
    // the drain target is 300 REAL bytes → 64.
    m.max_window_size = 64;
    let drained = m.trim_to_window();
    assert_eq!(
        drained,
        300 - 64,
        "trim must drain REAL history down to max_window_size = 64",
    );
    // history.len() = RESERVED (dummy preserved) + 64.
    assert_eq!(m.history.len(), 64 + HISTORY_DRAIN_BASE);

    // The slice MUST be in bounds — the bug would panic here OR
    // return a stale slice. After the fix, last_block_start is
    // saturating_sub'd by drained (236) AND clamped to >= RESERVED
    // — since drained > old last_block_start - RESERVED, new
    // last_block_start = RESERVED, pointing at the current first
    // real byte of history (post-drain start of what remains of
    // block 2).
    let last = m.last_committed_space();
    assert!(
        last.len() <= 64,
        "last_committed_space must be in-bounds after trim \
             (got len {})",
        last.len(),
    );
}

/// Regression for #216 Copilot review #15: after
/// `prime_offset_history` the kernel's `rep[0..2]` must mirror
/// the wire-encoder's `offset_hist[0..2]` — without this the
/// kernel makes repcode decisions against stale FAST_INITIAL_REP
/// while the wire encoder uses the primed history → wrong
/// repcode wire encoding (correctness bug, not perf).
#[test]
fn prime_offset_history_keeps_rep_and_offset_hist_in_lockstep() {
    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    // Pre-prime: matcher carries the upstream zstd's initial state.
    assert_eq!(m.rep, FAST_INITIAL_REP);
    assert_eq!(m.offset_hist, FAST_INITIAL_OFFSET_HIST);

    // Prime with non-default history (upstream zstd's dictionary load
    // restores explicit rep1/rep2/rep3 values).
    let primed = [9u32, 4, 8];
    m.prime_offset_history(primed);

    // BOTH must reflect the primed values; rep[0..2] = the first
    // two entries of offset_hist.
    assert_eq!(
        m.offset_hist, primed,
        "offset_hist must be updated by prime_offset_history",
    );
    assert_eq!(
        m.rep,
        [primed[0], primed[1]],
        "rep[0..2] must mirror offset_hist[0..2] post-prime \
             (kernel's repcode decisions must match the wire \
             encoder's seeded history)",
    );
}

/// Regression for #216 CodeRabbit #19: when a committed block
/// is larger than `max_window_size`, the pre-fix eviction math
/// always retained a full `max_window_size` of historical real
/// data and then appended the (oversized) block, blowing past
/// the documented `2 × max_window_size` post-append bound.
/// Fix retains a SMALLER prefix (or none) so the bound holds.
#[test]
fn accept_data_evicts_more_aggressively_when_block_larger_than_window() {
    let mut m = FastKernelMatcher::with_params(8, 6, 4, 2);
    // max_window_size = 256 (1 << 8). Threshold = 512.

    // Pre-fill history to full max_window_size of real data via
    // skip_matching_with_hint (no kernel-side trimming). u8 range
    // tops at 255, so the 256-byte preamble cycles via modulo.
    let preamble: alloc::vec::Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    m.accept_data(preamble);
    m.skip_matching_with_hint(None);
    assert_eq!(
        m.history_len_for_eviction_accounting(),
        256,
        "history pre-filled to one full window of real bytes",
    );

    // Commit a block larger than max_window_size but under 2×
    // (so its append should still be allowed without rejecting
    // outright). 400 > 256 but < 512.
    let oversize: alloc::vec::Vec<u8> = (0..400u32)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(11))
        .collect();
    m.accept_data(oversize);

    // Post-append real_len + space.len() MUST stay within 2×
    // max_window_size (the documented invariant). Pre-fix it
    // jumped to 256 + 400 = 656 = 2.56× — bound violated.
    let real_len_after = m.history_len_for_eviction_accounting();
    // accept_data stashes pending without appending, so the
    // 400-byte block is in pending. real_len reflects post-
    // drain retained real bytes. To verify the bound, run
    // start_matching to commit the append.
    m.start_matching(|_| {});
    let real_total = m.history_len_for_eviction_accounting();
    assert!(
        real_total <= m.max_window_size * 2,
        "post-append history MUST stay within 2 × max_window_size \
             (got real_total={real_total}, cap={})",
        m.max_window_size * 2,
    );
    // Sanity: pre-append eviction did drain something (we had
    // 256 real bytes, can't accept 400 more while staying under
    // 512 unless we drop at least 144).
    assert!(
        real_len_after < 256,
        "pre-append drain must have shed historical bytes \
             (got real_len_after_drain={real_len_after}, was 256 \
             before accept)",
    );
}

/// Regression for PR #219 round 6 (CR Critical): `start_matching`
/// computes an effective sliding prefix floor of
/// `history.len() - max_window_size` so emitted offsets never
/// exceed the advertised `max_window_size`. Without this floor,
/// after history grows past one window (commit's
/// eager-eviction keeps up to 2× max_window_size before
/// draining), the kernel could match against positions older
/// than the frame window — invalid for decoder buffer
/// reservation.
#[test]
fn start_matching_enforces_max_window_size_offset_bound() {
    // Tight window: window_log=7 → max_window_size=128.
    // Block lengths 200 + 200 = 400 bytes total, comfortably
    // past 128 so without the sliding floor the kernel would
    // emit matches at offsets > max_window.
    let mut m = FastKernelMatcher::with_params(7, 8, 4, 2);
    let max_window = m.max_window_size;
    assert_eq!(max_window, 128, "test assumes window_log=7");

    // Block 1: 200 bytes of a distinct ASCII pattern that
    // populates the hash table at positions 0..200.
    let block1: alloc::vec::Vec<u8> = (0..200u8).map(|i| 0x30 + (i % 64)).collect();
    m.accept_data(block1.clone());
    m.start_matching(|_| {});

    // Block 2: 200 bytes that RE-USE block1's content from
    // position 0 onwards. Without the sliding floor, the
    // kernel would happily emit Triples with offset
    // ≈ history_len (~200..400) — well past max_window=128.
    let block2 = block1.clone();
    m.accept_data(block2);

    let mut max_emitted_offset = 0usize;
    let mut emitted_match_count = 0usize;
    m.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
            && match_len > 0
        {
            emitted_match_count += 1;
            if offset > max_emitted_offset {
                max_emitted_offset = offset;
            }
        }
    });

    // GUARANTEE 1: the kernel actually scanned and matched
    // something — otherwise the offset bound below passes
    // trivially with max_emitted_offset = 0.
    assert!(
        emitted_match_count > 0,
        "fixture must produce at least one Triple match — \
             history.len()=~400, max_window=128, block2 is identical to block1",
    );

    // GUARANTEE 2: every emitted offset stays within the
    // advertised max_window_size. Without the sliding floor,
    // matches against early block1 positions (e.g. position
    // 0..72) would yield offsets > 128.
    assert!(
        max_emitted_offset <= max_window,
        "sliding floor MUST cap emitted offsets at max_window_size; \
             got max emitted offset {} vs max_window_size {}",
        max_emitted_offset,
        max_window,
    );
}

/// Regression for PR #219 round 9 (Copilot Critical): the
/// sliding prefix floor in `start_matching` MUST use the
/// advertised frame window (`1 << window_log`), NOT the
/// dynamically inflated `max_window_size` (which the driver
/// adds dictionary-budget bytes to during
/// `prime_with_dictionary`). With the inflated value,
/// offsets could exceed the advertised window during
/// dictionary-primed compression — format-invalid sequences.
#[test]
fn start_matching_caps_offsets_at_window_log_not_inflated_max() {
    // Advertised frame window = 1 << 7 = 128 bytes.
    let mut m = FastKernelMatcher::with_params(7, 8, 4, 2);
    let advertised_window: usize = 1 << m.window_log;
    assert_eq!(advertised_window, 128, "test assumes window_log=7");

    // Simulate dictionary priming: driver inflates
    // max_window_size by retained_dict_budget. We add 200
    // bytes of "dictionary" content to history first, then
    // bump max_window_size to reflect the dict-retention
    // budget (mirrors MatchGeneratorDriver::prime_with_dictionary
    // for the Simple backend).
    //
    // Pattern period 4 (`(i % 4)`) — dense enough that the upstream zstd-
    // parity `kSearchStrength = 8` (K_STEP_INCR = 128) step
    // doubling, which skips positions under step=3 in dict scan,
    // still leaves matches hittable from block2: every position
    // divisible by 4 inside the [in-window, hashed] subset writes
    // the same hash slot, so the slot at block2's first probe
    // contains a recent in-window dict position. Period 64 (the
    // original fixture) only had matches at positions {0, 64,
    // 128, 192} — positions 0, 64, 128 are below the sliding
    // floor and 192 falls in the step-skip gap, leaving the test
    // with zero emittable matches.
    let dict: alloc::vec::Vec<u8> = (0..200u8).map(|i| 0x40 + (i % 4)).collect();
    m.accept_data(dict.clone());
    m.start_matching(|_| {}); // populate hash table from dict
    m.max_window_size = m.max_window_size.saturating_add(200);

    // Add a block whose first 100 bytes match dict[0..100].
    // Without the fix, the kernel would emit offsets up to
    // ~history_len (200..300), since the inflated max_window
    // (328) keeps even the dict's earliest bytes inside the
    // sliding floor.
    let block: alloc::vec::Vec<u8> = (0..100u8).map(|i| 0x40 + (i % 4)).collect();
    m.accept_data(block);

    let mut max_emitted_offset = 0usize;
    let mut emitted_match_count = 0usize;
    m.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
            && match_len > 0
        {
            emitted_match_count += 1;
            if offset > max_emitted_offset {
                max_emitted_offset = offset;
            }
        }
    });

    assert!(
        emitted_match_count > 0,
        "fixture must produce at least one match — block content \
             repeats dict, history.len() ≈ 300, scan should find at \
             least one Triple",
    );
    assert!(
        max_emitted_offset <= advertised_window,
        "sliding floor MUST cap emitted offsets at the ADVERTISED \
             frame window (1 << window_log = {}), NOT the inflated \
             max_window_size; got max emitted offset {}",
        advertised_window,
        max_emitted_offset,
    );
}

/// Regression: at block 0 the kernel's prologue must NOT zero
/// `rep_offset1 = 1` (upstream zstd's default initial rep state). Upstream zstd
/// computes `max_rep = ip0 - windowLow` where `windowLow = 0` at
/// block 0, giving `max_rep = 1` at ip0=1 → `rep_offset1 = 1`
/// survives (`1 > 1` is false).
///
/// Buggy prologue uses `max_rep = ip0 - prefix_start_index` with
/// `prefix_start_index = 1` (sentinel-0 floor for hash-table
/// filtering), giving `max_rep = 0` at ip0=1 → `rep_offset1 = 1 >
/// 0` → stashed → rep-at-ip2 probe disabled for the ENTIRE first
/// block.
///
/// Symptom assertion: on a `[0x01, 0x42 × 199]` fixture, upstream zstd's
/// rep-at-ip2 fires at iter 1 (ip2=3, both `read32` reads see
/// `[42,42,42,42]`). The upstream zstd emit sequence is:
/// `new_ip = ip2 = 3`, `match0 = ip2 - rep_offset1 = 2`, then the
/// one-byte backward extension absorbs `data[2] == data[1]`
/// (both `0x42`), giving `new_ip = 2, match_len ≈ 198`. Literal
/// prefix is `data[0..new_ip] = [0x01, 0x42]` → length 2.
///
/// The buggy path skips rep, walks the cursor via explicit-match
/// shifts until matchIdx coincides further into the run, and
/// emits a different literal prefix length. Asserting both
/// `offset == 1` AND `literals.len() == 2` pins down the
/// rep-at-ip2 path exactly — the explicit-match catch-up on
/// uniform-byte data also finds offset=1 via slot collision, so
/// an offset-only check passes both fixed and buggy paths.
#[test]
fn block_zero_prologue_preserves_default_rep_offset_one() {
    let mut data = alloc::vec::Vec::with_capacity(200);
    data.push(0x01);
    data.resize(200, 0x42);

    let mut m = FastKernelMatcher::with_params(12, 8, 4, 2);
    m.accept_data(data.clone());

    let mut first_literals_len: Option<usize> = None;
    let mut first_offset: Option<usize> = None;
    m.start_matching(|seq| {
        if first_literals_len.is_some() {
            return;
        }
        if let Sequence::Triple {
            literals, offset, ..
        } = seq
        {
            first_literals_len = Some(literals.len());
            first_offset = Some(offset);
        }
    });

    // Both `offset` AND `literals.len()` are asserted — together
    // they pin down EXACTLY which inner-loop path emitted the
    // first match. With the prologue bug (rep-at-ip2 disabled
    // because `max_rep` was computed against `prefix_start_index`
    // instead of `window_low`), the explicit-match path catches
    // up via hash-slot collision at ip0=3 and STILL emits
    // offset=1 — so an offset-only check would pass both fixed
    // and buggy paths on this uniform-byte fixture. The literal
    // prefix length is the actual discriminator: the rep-at-ip2
    // path consumes only the leading `0x01` literal (1 byte),
    // while the explicit-match catch-up walks past two more
    // `0x42` bytes before firing (3 bytes total). Asserting both
    // values keeps the regression locked to the exact path the
    // fix was meant to preserve.
    assert_eq!(
        first_offset,
        Some(1),
        "first emit must reference offset=1 — upstream zstd's default \
             rep_offset1=1 fires on rep-at-ip2 at iter 1, and the \
             prologue MUST NOT zero it (max_rep computed against \
             window_low=0 at block 0, NOT against the sentinel \
             prefix=1)",
    );
    assert_eq!(
        first_literals_len,
        Some(2),
        "first emit must have a 2-byte literal prefix \
             ([0x01, 0x42]) — the rep-at-ip2 probe lands at ip2=3, \
             then the one-byte backward extension drops new_ip to 2, \
             so literals = data[0..2]. A different prefix length \
             would indicate the explicit-match catch-up fired instead",
    );
}
