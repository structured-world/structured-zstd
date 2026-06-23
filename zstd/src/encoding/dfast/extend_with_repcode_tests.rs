//! Targeted regression coverage for `extend_with_repcode_after_match`.
//!
//! These tests intentionally bypass the higher-level
//! `compress_to_vec` roundtrip path used by `cross_validation` so
//! that a failure pinpoints the post-match rep helper rather than
//! firing somewhere downstream (block writer / huff0 / FSE / decode).
//! The capture closure records the exact sequence stream the matcher
//! emits, which is what the assertions check.
use alloc::vec;
use alloc::vec::Vec;

use super::*;

/// Capture every sequence the matcher emits into an owned record,
/// so the assertions can match on `lit_len` / `offset` / `match_len`
/// shape directly. `Sequence::Triple` carries borrowed literals; we
/// take their length and discard the bytes (the test only cares
/// about the structural shape, not the literal content).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CapturedSeq {
    Triple {
        lit_len: usize,
        offset: usize,
        match_len: usize,
    },
    Literals {
        lit_len: usize,
    },
}

fn record_seq<'a>(out: &'a mut Vec<CapturedSeq>) -> impl FnMut(Sequence<'_>) + 'a {
    move |seq| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => out.push(CapturedSeq::Triple {
            lit_len: literals.len(),
            offset,
            match_len,
        }),
        Sequence::Literals { literals } => out.push(CapturedSeq::Literals {
            lit_len: literals.len(),
        }),
    }
}

fn build_dfast_with(data: &[u8]) -> DfastMatchGenerator {
    // Window sized to the block so the matcher does not start
    // trimming history mid-test.
    let mut dfast = DfastMatchGenerator::new(data.len().next_power_of_two().max(64));
    dfast.ensure_hash_tables();
    dfast.add_data(data.to_vec(), |_| {});
    dfast
}

/// Direct call into [`DfastMatchGenerator::extend_with_repcode_after_match`]
/// with a hand-built post-primary-match state. Going through
/// `start_matching` is unreliable for this assertion because the
/// primary `best_match` greedily consumes a constant run in a
/// single `Triple` (offset 1, match_len = block - 1), leaving the
/// helper nothing to extend. Instead we set up the state the
/// helper expects after a primary emit and verify it chains
/// rep-0 sequences for as many bytes as the rep predicate
/// matches.
#[test]
fn dfast_repcode_extension_emits_zero_literal_rep_on_constant_run() {
    let data: Vec<u8> = vec![b'A'; 64];
    let mut dfast = build_dfast_with(&data);

    // Post-primary-match state: pretend a previous sequence emitted
    // with offset = 4 (`offset_hist[0]`). Under the upstream zstd swap the
    // post-match rep probe consults `offset_hist[1]`, here set to
    // 1 so every subsequent byte (constant 'A') matches its
    // predecessor.
    dfast.offset_hist = [4, 1, 8];
    let current_abs_start = dfast.history_abs_start + dfast.window_size - data.len();
    let current_len = data.len();
    // Start the helper mid-block; the leading bytes are the
    // "literals + match" the (simulated) primary would have
    // covered. `literals_start == pos` is the post-emit invariant
    // — `lit_len` for the next sequence is zero.
    let pos = 10usize;
    let mut literals_start = pos;

    let mut seqs = Vec::new();
    let new_pos = {
        let mut rec = record_seq(&mut seqs);
        dfast.extend_with_repcode_after_match(
            current_abs_start,
            current_len,
            pos,
            &mut literals_start,
            &mut rec,
        )
    };

    assert!(
        new_pos > pos,
        "helper must advance pos past at least one rep match \
             (pos={pos}, new_pos={new_pos})"
    );
    assert_eq!(
        literals_start, new_pos,
        "helper must keep literals_start == new_pos so the caller's main \
             loop sees zero pending literals after the rep chain"
    );
    assert!(!seqs.is_empty(), "helper must emit at least one Triple");
    for seq in &seqs {
        match seq {
            CapturedSeq::Triple {
                lit_len,
                offset,
                match_len: _,
            } => {
                assert_eq!(
                    *lit_len, 0,
                    "rep emission must be zero-literal (got {seq:?})"
                );
                assert_eq!(
                    *offset, 1,
                    "rep emission must use the swapped-in offset_hist[1] = 1 \
                         (got {seq:?})"
                );
            }
            CapturedSeq::Literals { .. } => {
                panic!("rep extension must not emit a Literals tail: {seq:?}");
            }
        }
    }
}

/// Cross-block / retained-history case: probe with `offset > pos`
/// (where `pos` is block-local) so the candidate lives in retained
/// history from a previously committed block. The
/// CodeRabbit-flagged `rep > pos` guard would have rejected
/// exactly this path — the current implementation only gates on
/// `cur_idx.checked_sub(rep)` so the helper accepts the cross-
/// block offset and emits the rep sequence.
#[test]
fn dfast_repcode_extension_walks_into_retained_history() {
    let block_a: Vec<u8> = vec![b'C'; 64];
    let block_b: Vec<u8> = vec![b'C'; 32];
    let mut dfast = DfastMatchGenerator::new(256);
    dfast.ensure_hash_tables();
    dfast.add_data(block_a, |_| {});
    dfast.add_data(block_b.clone(), |_| {});

    // Post-primary-match state targeting cross-block rep: probe
    // offset = 40 (a candidate inside block A bytes), block-local
    // cursor = 5 (so `rep > pos` under the rejected guard).
    dfast.offset_hist = [4, 40, 8];
    let current_len = block_b.len();
    let current_abs_start = dfast.history_abs_start + dfast.window_size - current_len;
    let pos = 5usize;
    let mut literals_start = pos;

    let mut seqs = Vec::new();
    let new_pos = {
        let mut rec = record_seq(&mut seqs);
        dfast.extend_with_repcode_after_match(
            current_abs_start,
            current_len,
            pos,
            &mut literals_start,
            &mut rec,
        )
    };

    assert!(
        new_pos > pos,
        "rep with offset > block-local pos must still emit a match when the \
             candidate lives in retained history (pos={pos}, new_pos={new_pos})"
    );
    assert_eq!(seqs.len(), 1, "expected one rep emit, got {seqs:?}");
    match &seqs[0] {
        CapturedSeq::Triple {
            lit_len,
            offset,
            match_len: _,
        } => {
            assert_eq!(*lit_len, 0, "rep emit must be zero-literal");
            assert_eq!(*offset, 40, "rep emit must use the cross-block offset 40");
        }
        other => panic!("expected Triple, got {other:?}"),
    }
}

/// The helper accepts 4-byte rep extensions (upstream zstd `MINMATCH = 4`),
/// not the main-loop `DFAST_MIN_MATCH_LEN = 5` floor. A regression
/// back above 4 would still pass the constant-run / cross-block tests
/// above (their rep matches extend much further), so this fixture
/// is built so the rep matches EXACTLY 4 bytes before terminating:
/// the byte at `pos + 4` differs from the byte at `pos + 4 - rep`.
///
/// Fixture (32 bytes, indices `0..=31`):
///   `"ABCD????ABCD!??????????ABCDX????"`
///    01234567890123456789012345678901     (ones digit)
///              1111111111222222222233     (tens digit, aligned)
///
/// Probe state:
///   * `offset_hist[1] = 8` → rep probe reads `concat[pos - 8..]`.
///   * `pos = 8` → `concat[8..12] = "ABCD"`, `concat[0..4] = "ABCD"`
///     → 4 bytes match.
///   * `concat[12] = '!'` vs `concat[4] = '?'` → 5th byte mismatch,
///     so the rep extension stops at exactly 4 bytes.
#[test]
fn dfast_repcode_extension_accepts_exactly_four_byte_rep() {
    // Block: "ABCD????" (8) + "ABCD!" (5) + "??????????" (10) +
    //        "ABCDX" (5) + "????" (4) = 32 bytes total. The
    //        important invariants are `concat[0..4] == "ABCD"`,
    //        `concat[8..12] == "ABCD"`, and `concat[12] = '!'`
    //        (so byte 12 ≠ byte 4 = '?', stopping the rep at
    //        length 4). The trailing bytes are irrelevant — we
    //        only iterate the helper at `pos = 8` and the rep
    //        chain terminates after one 4-byte emit because the
    //        next rep probe (post-swap) would need bytes at
    //        `pos + 4` to match a different offset.
    let data: Vec<u8> = b"ABCD????ABCD!??????????ABCDX????".to_vec();
    assert_eq!(data.len(), 32, "fixture invariant: 32 bytes");
    let mut dfast = DfastMatchGenerator::new(64);
    dfast.ensure_hash_tables();
    dfast.add_data(data.clone(), |_| {});

    dfast.offset_hist = [12, 8, 4];
    let current_abs_start = dfast.history_abs_start + dfast.window_size - data.len();
    let current_len = data.len();
    let pos = 8usize;
    let mut literals_start = pos;

    let mut seqs = Vec::new();
    let new_pos = {
        let mut rec = record_seq(&mut seqs);
        dfast.extend_with_repcode_after_match(
            current_abs_start,
            current_len,
            pos,
            &mut literals_start,
            &mut rec,
        )
    };

    // Helper must emit a single 4-byte rep, then stop because
    // the 5th byte mismatches.
    assert_eq!(seqs.len(), 1, "expected one 4-byte rep emit, got {seqs:?}");
    match &seqs[0] {
        CapturedSeq::Triple {
            lit_len,
            offset,
            match_len,
        } => {
            assert_eq!(*lit_len, 0, "rep emit must be zero-literal");
            assert_eq!(*offset, 8, "rep emit must use offset 8 (offset_hist[1])");
            assert_eq!(
                *match_len, 4,
                "rep emit must be exactly 4 bytes (upstream zstd MINMATCH floor). \
                     A regression back to DFAST_MIN_MATCH_LEN > 4 would skip \
                     this emission entirely and the test would fail with 0 seqs."
            );
        }
        other => panic!("expected Triple, got {other:?}"),
    }
    assert_eq!(new_pos, pos + 4, "pos must advance by exactly 4");
    assert_eq!(literals_start, new_pos, "literals_start must follow pos");
}

/// Integration coverage for the **fast-loop** call sites of
/// `extend_with_repcode_after_match` inside
/// `start_matching_fast_loop`. The direct-call tests above pin
/// down the helper's contract; this test drives the full fast
/// loop end-to-end through the production `compress_to_vec`
/// pipeline on a fixture engineered to exercise the post-match
/// rep chain on the fast-loop path.
///
/// `CompressionLevel::Default` is the production config that
/// enables `use_fast_loop = true` (see `Matcher::reset` in
/// `match_generator.rs`). The fixture alternates 60-byte runs of
/// `'A'` with single `'B'` break bytes and a short `'A'` tail
/// per cycle — the breaks terminate the fast loop's primary match
/// early, so subsequent iterations have runway for the helper to
/// chain additional reps. A regression that broke either fast-
/// loop helper call site surfaces as a roundtrip failure (decoded
/// != input) or a ratio explosion. Constructing
/// `DfastMatchGenerator` directly and asserting on captured
/// sequences was attempted but the fixture engineering is
/// brittle: the fast loop's primary match on simple constant
/// fixtures consumes the entire remaining block in a single
/// Triple, leaving no bytes for the helper to extend. The
/// high-level roundtrip sidesteps that fragility while still
/// routing through the same call site via the production driver.
///
/// Gated on `feature = "std"`: the `Read::read_to_end` method
/// used to drain `StreamingDecoder` resolves to `std::io::Read`
/// only when std is enabled. Under no-std `StreamingDecoder`
/// implements the crate's `io_nostd::Read` alias instead, and
/// the call site has to be rewritten through that trait. The
/// fast-loop helper itself is exercised under both
/// configurations by the direct-call tests above plus the
/// `cross_validation` Default-level roundtrip — gating this one
/// integration test on std loses no coverage, only saves the
/// dual-trait rewrite.
#[cfg(feature = "std")]
#[test]
fn dfast_default_level_roundtrip_with_repetitive_breaks_exercises_fast_loop() {
    // ~4 KiB of input: 64 cycles of [60 'A's, 1 'B', 3 'A's].
    let mut data: Vec<u8> = Vec::with_capacity(64 * 64);
    for _ in 0..64 {
        data.extend_from_slice(&[b'A'; 60]);
        data.push(b'B');
        data.extend_from_slice(&[b'A'; 3]);
    }
    assert!(
        data.len() > 4000,
        "fixture invariant: long enough for fast loop"
    );

    let compressed = crate::encoding::compress_to_vec(
        data.as_slice(),
        crate::encoding::CompressionLevel::Default,
    );

    // Decompress and assert byte-for-byte parity. A regression
    // that broke the fast-loop helper call would either produce
    // invalid frames (decode error) or wrong bytes (mismatch).
    let mut decoder = crate::decoding::StreamingDecoder::new(compressed.as_slice())
        .expect("default-level frame must decode");
    let mut decoded = Vec::with_capacity(data.len());
    // Under `feature = "std"` (the gate above) `StreamingDecoder`
    // implements `std::io::Read`, so `Read::read_to_end` resolves
    // through the standard library's blanket implementation.
    std::io::Read::read_to_end(&mut decoder, &mut decoded)
        .expect("fast-loop output must round-trip cleanly");
    assert_eq!(
        decoded, data,
        "Default-level (use_fast_loop = true) roundtrip must be \
             byte-for-byte exact on the repetitive-breaks fixture"
    );

    // Ratio sanity: the post-match rep helper is what makes
    // repetitive runs compress aggressively on the fast-loop
    // path. A regression to a no-op helper would still produce
    // some compression via the primary match, but the ratio
    // would degrade. A 2:1 floor is conservative enough not to
    // flake on small fixture changes while still catching
    // structural failures of the fast loop.
    assert!(
        compressed.len() * 2 < data.len(),
        "fast loop must compress repetitive runs to at least 2:1, \
             got {} → {} bytes",
        data.len(),
        compressed.len()
    );
}

/// The borrowed one-shot scan (no per-block `commit_space` copy) must
/// emit the byte-identical sequence stream as the owned `history`-copying
/// path for an in-window input: the window far exceeds the input so the
/// owned path never evicts, making its accumulated `history` identical to
/// the borrowed buffer, and the borrowed scan's absolute positions /
/// seam re-seed reproduce the same match decisions. This is the
/// correctness contract behind routing dfast through `borrowed_eligible`.
#[test]
fn dfast_borrowed_window_matches_owned_sequence_stream() {
    // Repeating pattern (period 64) so the matcher emits real matches,
    // split across two blocks.
    let mut whole: Vec<u8> = Vec::with_capacity(512);
    for i in 0..512usize {
        whole.push((i % 64) as u8);
    }
    let split = 300usize;
    let window = 1usize << 16; // 64 KiB >> 512-byte input: no eviction.

    // Owned: commit each block, then scan.
    let mut owned = DfastMatchGenerator::new(window);
    owned.ensure_hash_tables();
    let mut owned_seqs: Vec<CapturedSeq> = Vec::new();
    owned.add_data(whole[..split].to_vec(), |_| {});
    {
        let mut rec = record_seq(&mut owned_seqs);
        owned.start_matching(&mut rec);
    }
    owned.add_data(whole[split..].to_vec(), |_| {});
    {
        let mut rec = record_seq(&mut owned_seqs);
        owned.start_matching(&mut rec);
    }

    // Borrowed: same bytes, scanned in place by absolute block range.
    let mut borrowed = DfastMatchGenerator::new(window);
    borrowed.ensure_hash_tables();
    let mut borrowed_seqs: Vec<CapturedSeq> = Vec::new();
    // SAFETY: `whole` outlives both scans; `borrowed` is dropped at end
    // of scope before `whole`, so the window never dangles.
    unsafe { borrowed.set_borrowed_window(&whole) };
    {
        let mut rec = record_seq(&mut borrowed_seqs);
        borrowed.start_matching_borrowed(0, split, &mut rec);
    }
    {
        let mut rec = record_seq(&mut borrowed_seqs);
        borrowed.start_matching_borrowed(split, whole.len(), &mut rec);
    }

    assert_eq!(
        owned_seqs, borrowed_seqs,
        "dfast borrowed window must emit the identical sequence stream as the owned path",
    );
    assert_eq!(
        owned.offset_hist, borrowed.offset_hist,
        "rep state must match after both scans",
    );
}
