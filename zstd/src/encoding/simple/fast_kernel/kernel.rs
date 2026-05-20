//! Donor-shape Fast strategy block compressor — port of
//! `ZSTD_compressBlock_fast_noDict_generic` from
//! `lib/compress/zstd_fast.c`.
//!
//! Phase 1: single `ip0` cursor with raw 4-byte match probe, step-based
//! skip acceleration, repcode-at-ip check, backward extension, and
//! `count_forward` (ZSTD_count) for forward extension. The 4-cursor
//! pipelining (`ip0/ip1/ip2/ip3`) and `cmov` match-found variant from
//! donor's full noDict_generic are deferred to phase 3 — phase 1
//! exists to validate the data structures and close the bulk of the
//! 22× gap before adding the pipelining complexity.

use super::count::count_forward;
use super::hash_table::FastHashTable;
use crate::encoding::Sequence;

/// Donor `kSearchStrength` — the step-skip accelerator advances the
/// per-iteration step every `1 << (kSearchStrength - 1) = 32` bytes
/// when no matches are found, so incompressible regions skip ahead
/// faster than the linear 1-byte advance.
const SEARCH_STRENGTH: usize = 6;

/// Donor `HASH_READ_SIZE`. The forward-progress invariant is that the
/// hash read at `ip0` MUST stay inside `[base, iend)`, so the
/// `ilimit = iend - HASH_READ_SIZE` cap is applied to the loop
/// boundary check.
const HASH_READ_SIZE: usize = 8;

/// Donor's `MEM_read32(ptr)` — unaligned little-endian 4-byte load,
/// used by the raw match probe at the hot path.
///
/// # Safety
///
/// `ptr` MUST point to at least 4 readable bytes.
#[inline(always)]
unsafe fn read32(ptr: *const u8) -> u32 {
    // SAFETY: caller contract.
    unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) }
}

/// Donor's "match probe": does the 4-byte word at `ip` equal the
/// 4-byte word at `base + match_idx`, AND is `match_idx` an in-range
/// historic position that the encoder may legitimately match against?
///
/// Three independent filters reject invalid candidates BEFORE the
/// 4-byte raw compare:
///
/// 1. `match_idx >= prefix_start_index` — the position is at or
///    above the encoder's window start; below that it's outside the
///    addressable history.
/// 2. `(match_idx as usize) + 4 <= data_len` — the 4-byte probe at
///    `base + match_idx` stays inside the caller's `data` buffer.
///    A stale entry from a reused `FastHashTable` (table not cleared
///    between calls that shrink the `data` slice) could otherwise
///    point past the current buffer end and produce an
///    out-of-bounds read.
/// 3. `(match_idx as usize) < ip_pos` — the match is genuinely
///    backward in the input, not at or ahead of the current scan
///    cursor. A stale ≥-ip0 entry would later make
///    `offset = ip_pos - match_idx` underflow / produce an invalid
///    forward-pointing offset code.
///
/// The kernel's normal usage (single block, hash table populated by
/// writes during the same scan, indices monotonically below `ip0`)
/// already satisfies (2) and (3) by construction, but the explicit
/// checks make the function safe under future cross-block / shared-
/// table call shapes too. The added branches are strongly biased
/// "not taken" on the well-behaved path so the predictor amortises
/// them to zero cost on the hot loop.
///
/// # Safety
///
/// `ip` MUST have at least 4 readable bytes. `base` MUST be the start
/// of a buffer of length `data_len` (so any `base + i` with `i + 4 <=
/// data_len` is a valid read). The three filters above turn every
/// other invariant the unchecked read needs into a compile-time-
/// checkable property of the input slice.
#[inline(always)]
unsafe fn match_found(
    ip: *const u8,
    base: *const u8,
    match_idx: u32,
    prefix_start_index: u32,
    ip_pos: usize,
    data_len: usize,
) -> bool {
    if match_idx < prefix_start_index {
        return false;
    }
    let match_pos = match_idx as usize;
    if match_pos + 4 > data_len {
        return false;
    }
    if match_pos >= ip_pos {
        return false;
    }
    // SAFETY: ip has ≥ 4 readable bytes per the function contract;
    // `base + match_pos` has ≥ 4 readable bytes by the filter above.
    unsafe { read32(ip) == read32(base.add(match_pos)) }
}

/// Output of [`compress_block_fast`] — the new repcode pair to thread
/// through the next block's invocation, plus the number of literal
/// bytes left at the tail (the caller emits these as a trailing
/// `Sequence::Literals` so the encoder pipeline can flush the block).
pub(crate) struct FastBlockResult {
    pub(crate) rep: [u32; 2],
    pub(crate) tail_literals_len: usize,
}

/// Donor-parity Fast block compressor, monomorphised over `MLS` (4..=8).
/// Each call processes one full block; produced sequences are emitted
/// via `handle_sequence` in order. The caller is responsible for
/// flushing the trailing literals (returned in `tail_literals_len`)
/// after this function returns.
///
/// # Arguments
///
/// - `data`: the full prefix history followed by the current block,
///   laid out as a single flat buffer (matches donor's `base`).
/// - `block_start`: byte offset of the current block's first byte
///   within `data`. The kernel hashes/searches only positions in
///   `[block_start, data.len())`, but matches may reach back into the
///   prefix all the way to `prefix_start_index`.
/// - `prefix_start_index`: lowest position that match candidates may
///   reference. Donor computes this from `windowLog`; for a flat
///   single-block kernel this is typically `0` or `block_start -
///   window_size`, clamped to ≥ 0.
/// - `hash_table`: the encoder's `FastHashTable`. Mutated in place;
///   entries are absolute indices into `data`.
/// - `rep`: incoming `[rep_offset1, rep_offset2]` from the previous
///   block. Returned updated in `FastBlockResult.rep`.
/// - `handle_sequence`: closure that the kernel invokes once per
///   emitted `Sequence` — equivalent to donor's `ZSTD_storeSeq`.
///
/// # Preconditions / algorithm invariants
///
/// `compress_block_fast` is a SAFE function — memory-safety holds for
/// every input. The contract below is about algorithmic correctness
/// (correct output sequences, donor-parity match coverage), not Rust
/// memory safety. Passing a smaller `data` is well-defined but the
/// kernel falls into the short-input early-return branch and emits no
/// sequences, which may not be what the caller wanted.
///
/// `data.len()` SHOULD be at least `HASH_READ_SIZE` (8) bytes longer
/// than the caller wants to actually match against. The
/// `ilimit = data.len() - HASH_READ_SIZE` cap ensures every hash/probe
/// read stays in range; for the trailing 7 bytes the caller must
/// emit them as literals (this is the kernel's `tail_literals_len`
/// return value).
///
/// # Sequence emission contract
///
/// The kernel emits ONLY in-block sequences (literals + match
/// pairs and pure-literal runs from anchor advances inside the
/// main loop). It NEVER emits a terminal `Sequence::Literals`
/// covering the trailing bytes from the last anchor to the end of
/// `data` — those bytes are accounted for by
/// `FastBlockResult.tail_literals_len`, and emitting them is the
/// caller's responsibility. This rule applies UNIFORMLY across
/// every exit branch, including the early-return short-input
/// branch below; without this uniformity a caller wrapping the
/// kernel's output would have to special-case "did the kernel
/// already emit the tail" per branch, which is exactly the
/// inconsistency this contract removes.
#[inline(always)]
pub(crate) fn compress_block_fast<const MLS: u32>(
    data: &[u8],
    block_start: usize,
    prefix_start_index: u32,
    hash_table: &mut FastHashTable,
    rep: [u32; 2],
    mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
) -> FastBlockResult {
    debug_assert_eq!(MLS, hash_table.mls(), "MLS must match hash_table's mls");
    debug_assert!(block_start <= data.len(), "block_start past data end",);

    // Block too short to do any matching — report the whole block
    // as trailing literals without emitting anything. Donor mirrors
    // the same shape via the `_cleanup` path (`anchor = istart`,
    // returns `iend - anchor`). The caller emits the
    // `Sequence::Literals` wrapper per the contract above; we don't
    // double-emit here.
    if data.len() < block_start + HASH_READ_SIZE {
        return FastBlockResult {
            rep,
            tail_literals_len: data.len() - block_start,
        };
    }

    let base = data.as_ptr();
    let iend_addr = data.len();
    let ilimit = iend_addr - HASH_READ_SIZE;

    let mut anchor: usize = block_start;
    let mut ip0: usize = block_start;
    // Donor: `ip0 += (ip0 == prefixStart);`. Equivalent in flat-buffer
    // terms is to ensure ip0 isn't at the absolute zero position
    // (where the sentinel could be confused with a valid match).
    if ip0 == 0 {
        ip0 = 1;
    }

    let mut rep_offset1: u32 = rep[0];
    let mut rep_offset2: u32 = rep[1];
    // Donor stashes the repcodes when they're out of range for the
    // current block and restores them at `_cleanup`. For phase 1 we
    // mirror the same save/restore so cross-block repcode history
    // stays correct.
    let mut offset_saved1: u32 = 0;
    let mut offset_saved2: u32 = 0;
    {
        let max_rep = (ip0 as u32).saturating_sub(prefix_start_index);
        if rep_offset2 > max_rep {
            offset_saved2 = rep_offset2;
            rep_offset2 = 0;
        }
        if rep_offset1 > max_rep {
            offset_saved1 = rep_offset1;
            rep_offset1 = 0;
        }
    }

    // Step-skip state: starts at 1, increments every `kStepIncr` bytes
    // of no-match scanning. Donor uses `targetLength + !targetLength
    // + 1` so the minimum is 2; for Fast strategy `targetLength == 0`
    // so the donor's initial step is 2. Phase 1 mirrors that.
    let mut step: usize = 2;
    let mut next_step_threshold: usize = ip0 + (1usize << (SEARCH_STRENGTH - 1));

    // Main scan loop. Phase 1 keeps a single `ip0` cursor — the
    // donor's `ip1/ip2/ip3` four-cursor pipeline (phase 3) is added
    // on top of this body. The shape of the per-iteration body
    // (hash, probe, advance) matches donor's `_start` loop verbatim.
    while ip0 < ilimit {
        // SAFETY: ip0 < ilimit = iend - 8, so ≥ 8 readable bytes at
        // `base + ip0`. MLS ≤ 8 matches the hash_ptr contract.
        let hash0 = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip0)) };
        // SAFETY: hash0 came from hash_ptr on this table, so it is
        // bounded by `1 << hash_log == table.len()`.
        let match_idx = unsafe { hash_table.get(hash0) };

        // Write current position into the hash table BEFORE checking
        // the candidate (donor does the same — writeback then probe).
        // SAFETY: hash0 bounded by `1 << hash_log`.
        unsafe { hash_table.put(hash0, ip0 as u32) };

        // Repcode probe at ip0. Donor probes at ip2 because of the
        // 4-cursor pipeline; phase 1 with a single cursor probes at
        // ip0 directly. Functionally equivalent for the single-cursor
        // case.
        // SAFETY: ip0 < ilimit (≥ 4 readable bytes); the rep_offset1
        // check guarantees `ip0 - rep_offset1 >= prefix_start_index`
        // since we set rep_offset1 = 0 above when it would overflow.
        let rep_check = rep_offset1 > 0
            && ip0 >= rep_offset1 as usize
            && unsafe { read32(base.add(ip0)) == read32(base.add(ip0 - rep_offset1 as usize)) };
        if rep_check {
            // Repcode match — backward extension by 1 if the byte
            // before ip0 also matches the byte before the rep source.
            //
            // No explicit `(new_ip - 1 - rep_off) >= prefix_start`
            // window guard here — mirrors donor's noDict rep path,
            // which also omits it. Safety follows from three pieces
            // of state the kernel maintains every iteration:
            //
            // 1. Block-entry save/restore (above) zeroes `rep_offset1`
            //    whenever the incoming value exceeds
            //    `ip0_start - prefix_start_index`, so any surviving
            //    non-zero rep satisfies
            //    `ip0_start - rep_offset1 >= prefix_start` (NON-STRICT
            //    — equality is allowed: `rep_offset1` may equal
            //    exactly `ip0_start - prefix_start`).
            //
            // 2. The explicit-match path's backward extension uses a
            //    STRICT `match_pos > prefix_start_index` bound when
            //    it promotes a fresh offset into `rep_offset1`, so
            //    `new_rep = ip0_promote - match_pos < ip0_promote -
            //    prefix_start` (strict). At any iteration after a
            //    promotion `ip0' > ip0_promote` and `rep_offset1` is
            //    unchanged, so `ip0' - rep_offset1 >= prefix_start +
            //    1`, i.e. `(ip0' - 1) - rep_offset1 >= prefix_start`
            //    — the strict bound becomes available from the
            //    SECOND iteration onward (any iteration where the
            //    rep has been promoted at least once OR where ip0
            //    has advanced past block_start).
            //
            // 3. The runtime `new_ip > anchor` gate covers the
            //    REMAINING corner case: at block entry ip0 ==
            //    block_start AND anchor == block_start, so the
            //    `new_ip > anchor` check fails and the 1-byte
            //    backward extension is skipped entirely. That is
            //    exactly the iteration where (1) alone could give
            //    `ip0_start - rep_offset1 == prefix_start` and
            //    backward-extending would dereference `prefix_start
            //    - 1`. The anchor gate prevents that read; (2) takes
            //    over from the second iteration onward (anchor moves
            //    past block_start after the first emitted sequence).
            //
            // Combined, every iteration that REACHES the
            // `data[new_ip - 1] == data[new_ip - 1 - rep_off]` read
            // already satisfies `new_ip - 1 - rep_off >= prefix_start`
            // — either via the strict bound from (2) or via the
            // anchor gate (3) skipping the read on the boundary
            // iteration. The explicit prefix-window check would be
            // dead code on the hot path. If a future call shape
            // weakens any of (1), (2), or (3) — e.g. shared hash
            // table across resets without re-running save/restore,
            // or a custom anchor initialisation that doesn't equal
            // `block_start` — the explicit guard must be re-added.
            // See the explicit-match path at the `match_pos >
            // prefix_start_index` line for the structurally symmetric
            // bound that proves it.
            let mut m_len: usize = 4;
            let rep_off = rep_offset1 as usize;
            let mut new_ip = ip0;
            if new_ip > anchor && new_ip > rep_off && data[new_ip - 1] == data[new_ip - 1 - rep_off]
            {
                new_ip -= 1;
                m_len += 1;
            }
            // Forward extension via ZSTD_count.
            // SAFETY: both pointers have ≥ (iend - new_ip - m_len)
            // readable bytes; iend pointer arithmetic stays in bounds.
            let forward = unsafe {
                count_forward(
                    base.add(new_ip + m_len),
                    base.add(new_ip + m_len - rep_off),
                    base.add(iend_addr),
                )
            };
            m_len += forward;

            // Emit the sequence: literals from anchor..new_ip, then
            // the repcode match. Donor uses `REPCODE1_TO_OFFBASE`
            // which maps to offset_code = 1 (rep[0]).
            let literals = &data[anchor..new_ip];
            // Repcode offset wire encoding: our `Sequence::Triple`
            // expects the actual byte offset; the encoder downstream
            // applies the repcode collapse.
            handle_sequence(Sequence::Triple {
                literals,
                offset: rep_off,
                match_len: m_len,
            });

            ip0 = new_ip + m_len;
            anchor = ip0;

            // Donor refills the hash for positions inside the match
            // so subsequent searches see them. We skip the refill in
            // phase 1 (degrades ratio marginally on repcode-heavy
            // workloads, full refill ladder lands in phase 1b).
            step = 2;
            next_step_threshold = ip0 + (1usize << (SEARCH_STRENGTH - 1));
            continue;
        }

        // Explicit-match probe.
        if unsafe {
            match_found(
                base.add(ip0),
                base,
                match_idx,
                prefix_start_index,
                ip0,
                iend_addr,
            )
        } {
            // Found a 4-byte match. Backward-extend while the byte
            // before each side also matches (donor's `while (ip0 >
            // anchor) & (match0 > prefixStart)` loop).
            let mut match_ip = ip0;
            let mut match_pos = match_idx as usize;
            let mut m_len: usize = 4;
            while match_ip > anchor
                && match_pos > prefix_start_index as usize
                && data[match_ip - 1] == data[match_pos - 1]
            {
                match_ip -= 1;
                match_pos -= 1;
                m_len += 1;
            }
            // Forward extension.
            // SAFETY: both pointers stay within `data`, and iend
            // pointer is `data.as_ptr() + data.len()`.
            let forward = unsafe {
                count_forward(
                    base.add(match_ip + m_len),
                    base.add(match_pos + m_len),
                    base.add(iend_addr),
                )
            };
            m_len += forward;

            let offset = match_ip - match_pos;
            // Update repcode history with the new explicit offset.
            rep_offset2 = rep_offset1;
            rep_offset1 = offset as u32;

            let literals = &data[anchor..match_ip];
            handle_sequence(Sequence::Triple {
                literals,
                offset,
                match_len: m_len,
            });

            ip0 = match_ip + m_len;
            anchor = ip0;
            step = 2;
            next_step_threshold = ip0 + (1usize << (SEARCH_STRENGTH - 1));
            continue;
        }

        // No match — advance by `step` and bump step every
        // `kStepIncr` bytes (donor's `if (ip2 >= nextStep) step++`).
        ip0 += step;
        if ip0 >= next_step_threshold {
            step += 1;
            next_step_threshold += 1usize << (SEARCH_STRENGTH - 1);
        }
    }

    // Repcode save/restore: if `rep_offset1` came in invalid
    // (offset_saved1 != 0) and finished valid (rep_offset1 != 0),
    // then the donor-saved offset becomes the new rep[1]. Mirrors
    // `offsetSaved2 = ((offsetSaved1 != 0) && (rep_offset1 != 0)) ?
    // offsetSaved1 : offsetSaved2;`.
    if offset_saved1 != 0 && rep_offset1 != 0 {
        offset_saved2 = offset_saved1;
    }

    let final_rep = [
        if rep_offset1 != 0 {
            rep_offset1
        } else {
            offset_saved1
        },
        if rep_offset2 != 0 {
            rep_offset2
        } else {
            offset_saved2
        },
    ];

    FastBlockResult {
        rep: final_rep,
        tail_literals_len: data.len() - anchor,
    }
}

#[cfg(test)]
mod tests {
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
            4 => compress_block_fast::<4>(data, 0, 0, &mut table, [0, 0], &mut handle),
            5 => compress_block_fast::<5>(data, 0, 0, &mut table, [0, 0], &mut handle),
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
        let result = compress_block_fast::<4>(data, 0, 0, &mut table, rep, &mut handle);
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
        let mut data = Vec::new();
        data.extend_from_slice(b"XAAAA"); // 0..5, the seed copy
        data.extend_from_slice(b"_____"); // 5..10, distinct filler
        data.extend_from_slice(b"XAAAA"); // 10..15, the repeating copy
        data.extend_from_slice(b"________"); // 15..23, HASH_READ_SIZE pad
        let (tuples, _) = run_block_with_rep(&data, 12, [0, 0]);
        let triple = tuples
            .iter()
            .find(|(_, _, m)| *m > 0)
            .unwrap_or_else(|| panic!("expected an explicit-match Triple, got {tuples:?}"));
        // Backward extension must lift the match length above the
        // bare MIN_MATCH=4: at least 5 bytes ("XAAAA").
        assert!(
            triple.2 >= 5,
            "backward extension must lift match_len above MIN_MATCH (got {})",
            triple.2,
        );
        // The literals before this match must NOT include the 'X' at
        // position 10 — backward extension consumed it as part of the
        // match.
        assert!(
            !triple.0.ends_with(b"X"),
            "backward extension must absorb the 'X' marker byte (literals: {:?})",
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
        let _ = compress_block_fast::<4>(&data, 0, 5, &mut table, [0, 0], &mut handle);

        // Every Triple emitted must reference a position ≥ 5, i.e.
        // the offset must NOT exceed the distance from ip0 to the
        // prefix start. With uniform data and prefix_start_index=5,
        // legitimate matches first become possible around ip0=9 with
        // offset ≤ 4 — none should be at offset=1 (the rejected stale
        // candidate) or larger than ip0-5.
        for (lits, off, m) in &tuples {
            if *m > 0 {
                assert_ne!(
                    *off, 1,
                    "stale entry at index 0 must be filtered out by prefix_start_index=5",
                );
                // After a literals run the anchor advances by exactly
                // `lits.len()` bytes (literals are written from
                // `anchor..ip0`), so `ip0 == anchor + lits.len() ==
                // lits.len()` here (anchor starts at 0).
                let ip0 = lits.len();
                // The offset must keep the match referent above prefix
                // start.
                assert!(
                    *off <= ip0,
                    "match offset {off} would point below anchor {ip0}",
                );
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
    /// Engineered scenario: uniform data so the raw-cmp at any two
    /// positions always succeeds; pre-populate the hash slot that
    /// ip0=1 will probe with a forward-pointing stale index (150);
    /// without the `match_pos < ip_pos` filter the very first
    /// iteration would emit `Triple { offset = 1 - 150 = u_wrap, ... }`.
    /// Test asserts every emitted Triple has an offset ≤ data.len()
    /// — only achievable when the stale forward index is rejected.
    #[test]
    fn match_found_rejects_stale_forward_entry() {
        let data = vec![0u8; 200];
        let mut table = FastHashTable::new(8, 4);
        // SAFETY: data has ≥ 4 readable bytes at index 1.
        let h = unsafe { table.hash_ptr::<4>(data.as_ptr().add(1)) };
        // SAFETY: h came from hash_ptr on this same table.
        unsafe { table.put(h, 150) };

        let mut tuples: Vec<(Vec<u8>, usize, usize)> = Vec::new();
        let mut handle = |seq: Sequence<'_>| match seq {
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => tuples.push((literals.to_vec(), offset, match_len)),
            Sequence::Literals { literals } => tuples.push((literals.to_vec(), 0, 0)),
        };
        let _ = compress_block_fast::<4>(&data, 0, 0, &mut table, [0, 0], &mut handle);

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
        let result = compress_block_fast::<4>(&data, 0, 0, &mut table, [huge, 7], &mut handle);
        assert_eq!(
            result.rep[0], huge,
            "out-of-range rep_offset1 must be restored verbatim across the block",
        );
        // rep_offset2 was also out of range (max_rep ≈ 0..63, 7 > 1).
        // Donor restores it through offset_saved2; the in-range
        // restoration path is the second witness.
        assert_eq!(result.rep[1], 7, "rep_offset2 (also stashed) must restore");
    }
}
