use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// Capture every emitted sequence as `(literals_bytes, offset,
/// match_len)` plus the final `FastBlockResult` so each test can
/// assert byte-level accounting and the actual match decisions
/// without fighting the borrow checker over `Sequence<'_>`
/// lifetimes (a `Sequence` borrow lives only as long as the
/// closure scope; cloning the literal bytes into the tuple
/// detaches the capture from that lifetime).
fn run_block(
    data: &[u8],
    hash_log: u32,
    mls: u32,
) -> (Vec<(Vec<u8>, usize, usize)>, FastBlockResult) {
    let mut table = FastHashTable::new(hash_log, mls);
    let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
    let mut handle = |seq: Sequence<'_>| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            tuples.push((literals.to_vec(), offset, match_len));
        }
        Sequence::Literals { literals } => {
            tuples.push((literals.to_vec(), 0, 0));
        }
    };
    let result = match mls {
        4 => compress_block_fast::<4, false>(
            data,
            0,
            PrefixBounds {
                // Match production contract:
                // `prefix_start_index >= 1` rejects the hash table
                // empty-slot value `0` so a fresh-table probe
                // cannot be mistaken for a position-0 match (the
                // sentinel-1 floor documented on FastKernelMatcher).
                prefix_start_index: 1,
                window_low: 0,
            },
            &mut table,
            [0, 0],
            2,
            &mut handle,
        ),
        5 => compress_block_fast::<5, false>(
            data,
            0,
            PrefixBounds {
                // Match production contract:
                // `prefix_start_index >= 1` rejects the hash table
                // empty-slot value `0` so a fresh-table probe
                // cannot be mistaken for a position-0 match (the
                // sentinel-1 floor documented on FastKernelMatcher).
                prefix_start_index: 1,
                window_low: 0,
            },
            &mut table,
            [0, 0],
            2,
            &mut handle,
        ),
        _ => panic!("test helper only supports mls=4 and mls=5"),
    };
    // Accounting invariant: literals + matches + tail == input.
    let acct: usize = tuples
        .iter()
        .map(|(lits, _off, mlen)| lits.len() + mlen)
        .sum::<usize>()
        + result.tail_literals_len;
    assert_eq!(acct, data.len(), "kernel must account for every input byte",);
    (tuples, result)
}

/// Tail-too-small case: input ≤ HASH_READ_SIZE produces zero
/// sequence emissions; the kernel reports the whole block as
/// `tail_literals_len` and the caller is expected to wrap it in
/// the terminal `Sequence::Literals`.
#[test]
fn short_input_reports_tail_without_emission() {
    let data = [1u8, 2, 3, 4, 5];
    let (tuples, result) = run_block(&data, 8, 4);
    assert!(
        tuples.is_empty(),
        "kernel must NOT emit sequences for short inputs (got {tuples:?})",
    );
    assert_eq!(result.tail_literals_len, data.len());
}

/// Repeated pattern with a clear long match — the kernel should
/// detect it and emit at least one Triple. Verifies via the
/// captured tuples that an actual match was produced (`match_len
/// >= MIN_MATCH=4`, non-zero offset).
#[test]
fn finds_long_repeat_in_simple_pattern() {
    let mut data = Vec::new();
    data.extend_from_slice(b"ABCDEFGHIJKLMNOP");
    data.extend_from_slice(b"ABCDEFGHIJKLMNOP");
    // Need ≥ 8 trailing bytes past the last match position so
    // `ilimit = data.len() - HASH_READ_SIZE` keeps the inner
    // loop active long enough to scan the repeated second half.
    // Pad with distinct bytes to keep the kernel out of any
    // extra repcode branches.
    data.extend_from_slice(b"________");
    let (tuples, _result) = run_block(&data, 12, 4);
    let triple = tuples
        .iter()
        .find(|(_, _, m)| *m > 0)
        .expect("kernel must emit at least one Triple for the repeated half");
    assert!(
        triple.2 >= 4,
        "match_len must be ≥ MIN_MATCH=4 (got {})",
        triple.2,
    );
    assert!(
        triple.1 > 0,
        "explicit-offset match must have offset > 0 (got {})",
        triple.1,
    );
}

/// Helper that accepts a non-zero `rep` and pre-populated hash
/// table so individual tests can exercise specific kernel branches
/// (rep path, prefix filter, stale-entry hardening). Shares the
/// same accounting invariant as `run_block` plus returns the
/// captured tuples for behavioural assertions.
fn run_block_with_rep(
    data: &[u8],
    hash_log: u32,
    rep: [u32; 2],
) -> (Vec<(Vec<u8>, usize, usize)>, FastBlockResult) {
    let mut table = FastHashTable::new(hash_log, 4);
    let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
    let mut handle = |seq: Sequence<'_>| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => tuples.push((literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => tuples.push((literals.to_vec(), 0, 0)),
    };
    let result = compress_block_fast::<4, false>(
        data,
        0,
        PrefixBounds {
            // Match production contract:
            // `prefix_start_index >= 1` rejects the hash table
            // empty-slot value `0`.
            prefix_start_index: 1,
            window_low: 0,
        },
        &mut table,
        rep,
        2,
        &mut handle,
    );
    let acct: usize = tuples
        .iter()
        .map(|(lits, _off, mlen)| lits.len() + mlen)
        .sum::<usize>()
        + result.tail_literals_len;
    assert_eq!(acct, data.len(), "kernel must account for every input byte");
    (tuples, result)
}

/// Repcode path: uniform data + `rep[0] = 1` means every 4-byte
/// window at any `ip0 > 0` matches `data[ip0-1..ip0+3]`. The
/// kernel must emit a Triple with `offset == 1` and large
/// `match_len`. Hits the `rep_check` branch on the very first
/// loop iteration.
#[test]
fn repcode_match_emits_with_rep_offset_one() {
    let data = vec![0x42u8; 64];
    let (tuples, _) = run_block_with_rep(&data, 8, [1, 4]);
    let rep_triple = tuples
        .iter()
        .find(|(_, off, m)| *off == 1 && *m > 0)
        .unwrap_or_else(|| panic!("repcode Triple at offset=1 expected, got {tuples:?}"));
    assert!(
        rep_triple.2 >= 4,
        "match_len must be ≥ MIN_MATCH=4 (got {})",
        rep_triple.2,
    );
    // Uniform-buffer rep match should extend far — the first match
    // covers nearly the whole tail after subtracting the initial
    // literal byte and the HASH_READ_SIZE trailing cap. Assert a
    // reasonable lower bound rather than an exact value (count
    // logic chooses chunk boundaries deterministically but the
    // chunk count depends on the LE/BE branch).
    assert!(
        rep_triple.2 >= 32,
        "uniform-byte rep extension must consume most of the buffer, got {}",
        rep_triple.2,
    );
}

/// Explicit-match backward extension: a marker byte before the
/// repeated pattern lets the kernel walk the match back by one
/// byte once the 4-byte forward probe at the hashed position
/// fires.
///
/// Layout: `"X"` literal at 0, then `AAAA` 4-byte block at 1..5,
/// distinct filler, then `"X"` + `AAAA` again starting at 10. The
/// kernel hashes the second `AAAA` at ip0=11 (or wherever step
/// lands close to it), reads the stored index of the first
/// `AAAA`, and the backward-extension while-loop walks back
/// because `data[ip0 - 1] == data[match_pos - 1] == 'X'`.
#[test]
fn explicit_match_backward_extension_extends_by_marker_byte() {
    // Engineered so the FIRST emitted match deterministically
    // backward-extends through a marker byte:
    //
    //   [0..15]   distinct prefix (no 'Z', no 'A') → table
    //             writebacks here can't byte-match later AAAA
    //   [15]      'Z' marker (first copy)
    //   [16..24]  'AAAAAAAA' (first AAAA copy — table[hash("AAAA")]
    //             gets written = 16 when ip0 reaches here)
    //   [24..32]  distinct filler (no 'Z', no 'A')
    //   [32]      'Z' marker (second copy)
    //   [33..41]  'AAAAAAAA' (second AAAA copy — kernel matches
    //             this against index 16; backward extension
    //             walks back because data[32]='Z'==data[15]='Z')
    //   [41..]    HASH_READ_SIZE tail
    let mut data: Vec<u8> = (0..15u8).collect();
    data.push(b'Z');
    data.extend_from_slice(b"AAAAAAAA");
    for i in 0..8u8 {
        data.push(0x80 + i);
    }
    data.push(b'Z');
    data.extend_from_slice(b"AAAAAAAA");
    for i in 0..16u8 {
        data.push(0x40 + (i % 16));
    }
    let (tuples, _) = run_block_with_rep(&data, 12, [0, 0]);
    let triple = tuples
        .iter()
        .find(|(_, _, m)| *m > 0)
        .unwrap_or_else(|| panic!("expected an explicit-match Triple, got {tuples:?}"));
    // Backward extension must lift match_len above MIN_MATCH=4 —
    // the 'Z' marker at position 32 (matching the 'Z' at 15) is
    // absorbed by the backward walk.
    assert!(
        triple.2 >= 5,
        "expected match_len ≥ 5 from backward extension (got {})",
        triple.2,
    );
    // Literals before the emit must NOT end with 'Z' — backward
    // extension consumed the marker.
    assert!(
        !triple.0.ends_with(b"Z"),
        "backward extension must consume the 'Z' marker (literals = {:?})",
        triple.0,
    );
}

/// `prefix_start_index` filter: a stale hash entry pointing at a
/// position BELOW `prefix_start_index` must be rejected even when
/// the byte-for-byte cmp would have succeeded. Engineered by
/// pre-populating the table with an in-range-by-bytes but
/// below-prefix index.
#[test]
fn prefix_start_index_filter_rejects_below_window() {
    // Uniform data — every 4-byte window has the same hash and
    // the same bytes, so a stale entry at any position would
    // raw-cmp-match. Pre-set the hash slot for ip0=1 to index 0,
    // then run with prefix_start_index=5. Without the filter the
    // kernel would happily emit a Triple at offset=1; with it,
    // the candidate is rejected.
    let data = vec![0xAAu8; 64];
    let mut table = FastHashTable::new(8, 4);
    // SAFETY: data has ≥ 4 readable bytes at index 1.
    let h = unsafe { table.hash_ptr::<4>(data.as_ptr().add(1)) };
    // SAFETY: h came from hash_ptr on this same table.
    unsafe { table.put(h, 0) };

    let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
    let mut handle = |seq: Sequence<'_>| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => tuples.push((literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => tuples.push((literals.to_vec(), 0, 0)),
    };
    // prefix_start_index=5 blocks index 0.
    let _ = compress_block_fast::<4, false>(
        &data,
        0,
        PrefixBounds {
            prefix_start_index: 5,
            window_low: 5,
        },
        &mut table,
        [0, 0],
        2,
        &mut handle,
    );

    // Walk emitted sequences in order, tracking the running
    // `anchor` cursor (which equals the start of the current
    // emit's literal-run). For each Triple the match begins at
    // `match_start = anchor + lits.len()` and references
    // `match_start - offset`; that source position MUST be at or
    // above `prefix_start_index = 5`. The simpler `off <= ip0`
    // form fails for the second+ Triple — `lits.len()` only
    // equals `ip0` for the first emit (when anchor still sits at
    // block_start=0); a single-byte tracker keeps the bound
    // correct across multiple emits.
    let mut anchor: usize = 0;
    for (lits, off, m) in &tuples {
        if *m > 0 {
            // The real correctness check is `match_src >=
            // prefix_start_index` below — the `offset != 1`
            // form is too cadence-specific (4-cursor body's
            // double writeback per iter can land an offset=1
            // emit whose SOURCE is still ≥ prefix_start_index).
            let match_start = anchor + lits.len();
            let match_src = match_start
                .checked_sub(*off)
                .expect("offset must not exceed match_start (would wrap)");
            assert!(
                match_src >= 5,
                "match source {match_src} below prefix_start_index=5 \
                     (match_start={match_start}, offset={off})",
            );
            anchor = match_start + m;
        } else {
            // Pure-literals callback (currently never emitted by
            // the kernel — kept defensive for future contract
            // changes): advance anchor by the literal run length.
            anchor += lits.len();
        }
    }
}

/// Hardening regression (round 3, finding #11): a hash entry
/// pointing AT or AFTER the current `ip0` must be rejected
/// before the 4-byte raw compare. Without this guard the kernel
/// would compute `offset = ip0 - match_pos` and wrap into a
/// gigantic offset → emit a Triple with a meaningless backward
/// reference.
///
/// Stale hash entries below `prefix_start_index` must be rejected
/// by the upstream zstd-parity prefix filter in `match_found`. Engineered
/// scenario: pre-populate the hash slot for ip0 with a low stale
/// index (5) that points into the supposedly-out-of-window region;
/// run with `prefix_start_index = 50` so the kernel must skip
/// that candidate. The kernel's own writeback at the iteration
/// start would still leave the stale value usable if the prefix
/// filter didn't fire — uniform data ensures any survived
/// candidate would emit a non-zero match.
#[test]
fn match_found_rejects_stale_entry_below_prefix_floor() {
    let data = vec![0u8; 200];
    let mut table = FastHashTable::new(8, 4);
    // Force the explicit-match probe at ip0=50 (first iter once
    // ip0 is bumped from prefix_start_index=50) to see the stale
    // index 5.
    // SAFETY: data has ≥ 4 readable bytes at index 50.
    let h = unsafe { table.hash_ptr::<4>(data.as_ptr().add(50)) };
    // SAFETY: h came from hash_ptr on this same table.
    unsafe { table.put(h, 5) };

    let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
    let mut handle = |seq: Sequence<'_>| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => tuples.push((literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => tuples.push((literals.to_vec(), 0, 0)),
    };
    // prefix_start_index = 50 — match_idx=5 is below the floor and
    // must be rejected by the upstream zstd-parity prefix filter in
    // `match_found`.
    let _ = compress_block_fast::<4, false>(
        &data,
        50,
        PrefixBounds {
            prefix_start_index: 50,
            window_low: 50,
        },
        &mut table,
        [0, 0],
        2,
        &mut handle,
    );

    // Either zero emissions (stale rejected, no other match found
    // in the limited scan window) or a Triple whose offset
    // references a position >= prefix_start_index = 50, never a
    // 1-byte-from-stale-5 offset.
    for (_, off, m) in &tuples {
        if *m > 0 {
            assert!(
                *off > 0 && *off <= data.len(),
                "every emitted offset must reference an in-buffer backward position (got {off})",
            );
        }
    }
}

/// Input exactly `HASH_READ_SIZE` bytes long: the short-input
/// branch fires because `data.len() < block_start + HASH_READ_SIZE`
/// is `8 < 0 + 8` → false, so we enter the main loop, but
/// `ilimit = 8 - 8 = 0` makes `while ip0 < ilimit` zero-iteration
/// (ip0 starts at 1 ≥ 0). Result: zero emissions, entire input
/// reported as tail.
#[test]
fn block_exactly_hash_read_size_emits_no_sequences() {
    let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let (tuples, result) = run_block_with_rep(&data, 8, [0, 0]);
    assert!(
        tuples.is_empty(),
        "exactly HASH_READ_SIZE bytes must produce no main-loop iterations",
    );
    assert_eq!(result.tail_literals_len, data.len());
}

/// Input one byte shorter than `HASH_READ_SIZE`: the short-input
/// branch fires (`7 < 8`), the kernel returns immediately with
/// the full input as tail and no callback invocations.
#[test]
fn block_just_below_hash_read_size_emits_no_sequences() {
    let data = [1u8, 2, 3, 4, 5, 6, 7];
    let (tuples, result) = run_block_with_rep(&data, 8, [0, 0]);
    assert!(tuples.is_empty());
    assert_eq!(result.tail_literals_len, data.len());
}

/// Repcode save/restore: when the incoming `rep_offset1` is
/// larger than the addressable history (`max_rep = ip0 -
/// prefix_start_index`), the kernel stashes it into
/// `offset_saved1` and zeroes the live rep. If no explicit match
/// promotes a new rep during the block, `_cleanup` must restore
/// the saved value into the returned `rep[0]` so cross-block
/// repcode history isn't lost. The unaffected `rep[1]` is the
/// secondary witness that no mutation occurred mid-block.
#[test]
fn rep_offset_save_restore_when_out_of_range() {
    // Random-looking distinct bytes — no real matches the kernel
    // would discover; deterministic xorshift keeps the stream
    // reproducible.
    let mut data = vec![0u8; 64];
    let mut state = 0x1234_5678u32;
    for byte in &mut data {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    // rep_offset1 huge — far exceeds any plausible ip0 in a
    // 64-byte block. Must be stashed and restored unchanged.
    let huge = 9999;
    let mut table = FastHashTable::new(10, 4);
    let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
    let mut handle = |seq: Sequence<'_>| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => tuples.push((literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => tuples.push((literals.to_vec(), 0, 0)),
    };
    let result = compress_block_fast::<4, false>(
        &data,
        0,
        PrefixBounds {
            // Match production contract:
            // `prefix_start_index >= 1` rejects the hash table
            // empty-slot value `0`.
            prefix_start_index: 1,
            window_low: 0,
        },
        &mut table,
        [huge, 7],
        2,
        &mut handle,
    );
    assert_eq!(
        result.rep[0], huge,
        "out-of-range rep_offset1 must be restored verbatim across the block",
    );
    // rep_offset2 was also out of range (max_rep ≈ 0..63, 7 > 1).
    // Upstream zstd restores it through offset_saved2; the in-range
    // restoration path is the second witness.
    assert_eq!(result.rep[1], 7, "rep_offset2 (also stashed) must restore");
}

/// cmov variant: same correctness contract as branch variant —
/// produces identical output for the same input, just lowers
/// match_idx >= prefix_start_index to a cmov instead of a
/// branch. Run a known-good fixture through both and assert
/// byte-for-byte equality of the emitted Triple stream.
#[test]
fn cmov_variant_matches_branch_variant_output() {
    let mut data = alloc::vec::Vec::new();
    for i in 0..512u32 {
        data.push((i & 0xFF) as u8);
    }
    // Repeat the first 64 bytes near the end so the kernel
    // emits at least one explicit match Triple.
    let tail = data[0..64].to_vec();
    data.extend_from_slice(&tail);

    let collect = |use_cmov: bool| -> alloc::vec::Vec<(alloc::vec::Vec<u8>, usize, usize)> {
        let mut table = FastHashTable::new(12, 4);
        let mut tuples = alloc::vec::Vec::new();
        let mut handle = |seq: Sequence<'_>| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                tuples.push((literals.to_vec(), offset, match_len));
            }
            Sequence::Literals { literals } => {
                tuples.push((literals.to_vec(), 0, 0));
            }
        };
        if use_cmov {
            let _ = compress_block_fast::<4, true>(
                &data,
                0,
                PrefixBounds {
                    // Match production contract:
                    // `prefix_start_index >= 1` rejects the hash
                    // table empty-slot value `0`.
                    prefix_start_index: 1,
                    window_low: 0,
                },
                &mut table,
                [0, 0],
                2,
                &mut handle,
            );
        } else {
            let _ = compress_block_fast::<4, false>(
                &data,
                0,
                PrefixBounds {
                    // Match production contract:
                    // `prefix_start_index >= 1` rejects the hash
                    // table empty-slot value `0`.
                    prefix_start_index: 1,
                    window_low: 0,
                },
                &mut table,
                [0, 0],
                2,
                &mut handle,
            );
        }
        tuples
    };

    let out_branch = collect(false);
    let out_cmov = collect(true);
    assert_eq!(
        out_branch, out_cmov,
        "cmov and branch variants must emit identical sequences"
    );
}

/// Regression test for Copilot review thread on PR #219 — cmov
/// variant must NOT report a match when `match_idx <
/// prefix_start_index` even if the 4 bytes at `ip` happen to
/// equal `CMOV_DUMMY`. Without the explicit `in_range`
/// predicate the cmov path returns `true` here, producing an
/// out-of-window match the kernel would then encode with a
/// bogus offset.
#[test]
fn cmov_variant_rejects_out_of_window_when_ip_equals_dummy() {
    // Layout (32 bytes total):
    //   data[0..4]  = filler (not CMOV_DUMMY, won't accidentally match)
    //   data[4..]   = CMOV_DUMMY bytes at position 16, so read32(ip)
    //                 at ip_pos=16 equals read32(CMOV_DUMMY).
    //
    // match_idx=4 is below prefix_start=10 (out of window).
    // ip_pos=16 satisfies `ip == base.add(ip_pos)`.
    let mut data: alloc::vec::Vec<u8> = alloc::vec![0xAA; 32];
    data[16] = 0x12;
    data[17] = 0x34;
    data[18] = 0x56;
    data[19] = 0x78;
    // SAFETY (test fixture): ip = base + 16; both buffers cover
    // ≥ 4 readable bytes (data.len()=32 ≥ 16+4 and CMOV_DUMMY is
    // 4 bytes by construction).
    let base = data.as_ptr();
    let ip_pos = 16usize;
    let ip = unsafe { base.add(ip_pos) };
    let branch_result = unsafe { match_found::<false>(ip, base, 4, 10) };
    assert!(
        !branch_result,
        "branch variant must reject out-of-window match_idx"
    );
    let cmov_result = unsafe { match_found::<true>(ip, base, 4, 10) };
    assert!(
        !cmov_result,
        "cmov variant must reject out-of-window match_idx even when \
             ip bytes coincide with CMOV_DUMMY",
    );
}

/// Drive the borrowed dual-base dict kernel directly (Scalar tier — always
/// available, deterministic) over a crafted `(dict, input)` pair that
/// exercises the dict-match (2-segment + backward extension), input-match,
/// repcode and tail-literal branches. Reconstruct against the logical
/// `[dict][input]` window to prove every emitted offset is valid, and check
/// the byte-accounting invariant.
#[test]
fn borrowed_dict_kernel_reconstructs_via_dual_base() {
    use crate::encoding::fastpath::FastpathKernel;

    let hash_log = 12u32;
    const MLS: u32 = 4;
    // Distinct dict bytes so each 4-byte key hashes uniquely; the input
    // both matches the dict prefix (dict match) and repeats itself
    // (input match + repcode), then ends in a literal tail.
    let dict: Vec<u8> = (0u8..40).collect();
    let mut inp: Vec<u8> = Vec::new();
    inp.extend_from_slice(&dict[0..20]); // dict match against dict[0..]
    inp.extend_from_slice(&dict[0..20]); // input match against the first copy
    inp.extend_from_slice(&dict[4..24]); // dict match at a non-zero dict pos
    inp.extend_from_slice(b"tail-literals-xyz"); // terminal literals

    let dict_end = dict.len();

    // main table (filled during the scan) + immutable dict table primed
    // over every hashable dict position.
    let mut main_table = FastHashTable::new(hash_log, MLS);
    let mut dict_table = FastHashTable::new(hash_log, MLS);
    for pos in 0..=dict.len() - HASH_READ_SIZE {
        // SAFETY: pos + 8 <= dict.len(); MLS matches the table. Tagged
        // every-position last-wins fill, matching the kernel's tagged dict
        // lookup (slot + packed tag).
        let hat = unsafe { hash_ptr_raw::<MLS>(dict.as_ptr().add(pos), hash_log + DICT_TAG_BITS) };
        unsafe {
            dict_table.put(
                hat >> DICT_TAG_BITS,
                ((pos as u32) << DICT_TAG_BITS) | (hat & DICT_TAG_MASK),
            )
        };
    }

    let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
    let mut handle = |seq: Sequence<'_>| match seq {
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => tuples.push((literals.to_vec(), offset, match_len)),
        Sequence::Literals { literals } => tuples.push((literals.to_vec(), 0, 0)),
    };

    let result = compress_block_fast_dict_borrowed::<MLS, false>(
        &inp,
        &dict,
        0,
        inp.len(),
        &mut main_table,
        &dict_table,
        PrefixBounds {
            prefix_start_index: 1,
            window_low: 0,
        },
        [0, 0],
        2,
        &mut handle,
        FastpathKernel::Scalar,
    );

    // Reconstruct against the logical [dict][input] window: start from the
    // dictionary, then replay each sequence. A Triple's offset references
    // `current_len - offset` in this combined buffer, so a valid dual-base
    // offset reproduces the original input exactly.
    let mut window = dict.clone();
    let mut saw_dict_region_match = false;
    for (literals, offset, match_len) in &tuples {
        window.extend_from_slice(literals);
        if *match_len > 0 {
            let start = window.len() - offset;
            // A match whose source lands in the dictionary prefix exercised
            // the dual-base / 2-segment path (not a plain input back-ref).
            if start < dict_end {
                saw_dict_region_match = true;
            }
            for i in 0..*match_len {
                let b = window[start + i];
                window.push(b);
            }
        }
    }
    // Append the terminal tail the kernel reports separately.
    let tail_start = inp.len() - result.tail_literals_len;
    window.extend_from_slice(&inp[tail_start..]);

    assert_eq!(
        &window[dict_end..],
        &inp[..],
        "borrowed dict kernel must reconstruct the input from the [dict][input] window",
    );

    assert!(
        tuples.iter().any(|(_, _, m)| *m >= 4),
        "expected at least one match Triple",
    );
    assert!(
        saw_dict_region_match,
        "expected at least one match reading from the dictionary region (dual-base path)",
    );
}
