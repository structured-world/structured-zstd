//! Donor-shape Fast strategy block compressor — port of
//! `ZSTD_compressBlock_fast_noDict_generic` from
//! `lib/compress/zstd_fast.c`. Includes the 4-cursor
//! (`ip0/ip1/ip2/ip3`) lookahead pipeline with `kSearchStrength`
//! step-doubling, repcode-at-ip2 probe, two explicit-match probes
//! per do-while iter, immediate-rep2 inner loop after match emit,
//! and both donor variants of the match-found check
//! (`ZSTD_match4Found_branch` + `ZSTD_match4Found_cmov`) selected
//! per-call via the `USE_CMOV` const generic.

use super::count::count_forward;
use super::hash_table::FastHashTable;
use crate::encoding::Sequence;

/// Donor `kSearchStrength` — the step-skip accelerator advances the
/// per-iteration step every `1 << (kSearchStrength - 1) = 32` bytes
/// when no matches are found, so incompressible regions skip ahead
/// faster than the linear 1-byte advance.
const SEARCH_STRENGTH: usize = 6;

/// Donor `kStepIncr = 1 << (kSearchStrength - 1)` — every this-many
/// bytes of no-match scanning, the per-iteration `step` is bumped
/// by 1 (donor's `step++` at `zstd_fast.c:343`). Drives the
/// incompressible-region step acceleration.
const K_STEP_INCR: usize = 1 << (SEARCH_STRENGTH - 1);

/// Donor `HASH_READ_SIZE`. The forward-progress invariant is that the
/// hash read at `ip0` MUST stay inside `[base, iend)`, so the
/// `ilimit = iend - HASH_READ_SIZE` cap is applied to the loop
/// boundary check.
const HASH_READ_SIZE: usize = 8;

/// Donor's `MEM_read32(ptr)` — unaligned native-endian 4-byte load,
/// used by the raw match probe on the hot path. The result is only
/// ever compared for equality against another `read32` of the same
/// width on the same host, so the byte ordering does not matter — both
/// sides experience the same endianness, and `a == b` holds iff the
/// underlying byte sequences match. No `.to_le()` conversion is needed
/// (donor's C `MEM_read32` is also implemented as a native-endian
/// `memcpy` for the same reason).
///
/// # Safety
///
/// `ptr` MUST point to at least 4 readable bytes.
#[inline(always)]
unsafe fn read32(ptr: *const u8) -> u32 {
    // SAFETY: caller contract.
    unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) }
}

/// Donor `ZSTD_match4Found_cmov`'s dummy buffer — 4 random-ish bytes
/// to compare against when `match_idx < prefix_start_index`. Chosen
/// so the read32 result is very unlikely to coincide with any real
/// 4-byte window from the input. Used by the cmov branchless variant
/// to avoid the unpredictable `match_idx >= prefix_start_index`
/// branch.
const CMOV_DUMMY: [u8; 4] = [0x12, 0x34, 0x56, 0x78];

/// Donor `ZSTD_match4Found_branch` / `ZSTD_match4Found_cmov`
/// branchless dispatch via const generic.
///
/// # Safety
///
/// - `ip` MUST point to ≥ 4 readable bytes (the kernel only calls
///   this when ip0..ip3 stay within `iend - HASH_READ_SIZE`).
/// - `base` MUST be the start of the same buffer that `data_len`
///   describes, i.e. `base[0..data_len]` is a valid byte range.
/// - `ip_pos` MUST equal the byte offset of `ip` within the buffer
///   `base` points at: `ip == base.add(ip_pos)`.
#[inline(always)]
unsafe fn match_found<const USE_CMOV: bool>(
    ip: *const u8,
    base: *const u8,
    match_idx: u32,
    prefix_start_index: u32,
    ip_pos: usize,
    data_len: usize,
) -> bool {
    // Bounds filters (well-predicted "not taken" on the hot path)
    // stay as explicit branches — these protect against stale entries
    // pointing past the buffer end and forward-pointing matches.
    let match_pos = match_idx as usize;
    // `match_pos + 4` would wrap on 32-bit targets when `match_idx`
    // approaches `u32::MAX`, allowing an OOB `read32` through.
    // `checked_add` keeps the unsafe read fully guarded: `None` means
    // overflow → reject; `Some(end)` mirrors the original predicate
    // (`end > data_len` rejects). On 64-bit the optimizer collapses
    // this to the same codegen as the previous form.
    if match_pos.checked_add(4).is_none_or(|end| end > data_len) {
        return false;
    }
    if match_pos >= ip_pos {
        return false;
    }

    if USE_CMOV {
        // Donor cmov variant (`ZSTD_match4Found_cmov`): pick either
        // `base + match_pos` or `CMOV_DUMMY` based on the prefix
        // filter, then AND with an explicit `in_range` predicate.
        // The compiler typically lowers the if-expression to a
        // `cmov` on x86_64 (the "cmov" name reflects that target —
        // we don't enforce the lowering, since LLVM is free to use
        // a branch where it predicts well). The dummy compare alone
        // is NOT enough — if `read32(ip)` happens to equal
        // `CMOV_DUMMY` (rare but reachable), the out-of-window
        // match would otherwise slip through.
        //
        // Donor (`ZSTD_match4Found_cmov` lines 119-124) inserts an
        // `__asm__("")` compiler barrier between the two checks to
        // pin codegen order. We don't replicate that — Rust offers
        // `core::sync::atomic::compiler_fence` if needed, but
        // empirically LLVM's lowering here already orders the
        // bytes_match comparison before the in_range AND without a
        // barrier. Revisit only if profiling shows reordering hurt.
        // SAFETY: both candidate addresses have ≥ 4 readable bytes
        // (CMOV_DUMMY is exactly 4 bytes; base+match_pos has ≥ 4
        // by the bounds check above).
        let in_range = match_idx >= prefix_start_index;
        let mval_addr = if in_range {
            unsafe { base.add(match_pos) }
        } else {
            CMOV_DUMMY.as_ptr()
        };
        let bytes_match = unsafe { read32(ip) == read32(mval_addr) };
        // Bitwise AND (not `&&`) is INTENTIONAL — short-circuit
        // would re-introduce a branch on `bytes_match`, defeating
        // the cmov-branchless path. Donor enforces the same
        // ordering with `__asm__("")` between its two checks
        // (`ZSTD_match4Found_cmov` lines 119-124).
        #[allow(clippy::needless_bitwise_bool)]
        let r = bytes_match & in_range;
        r
    } else {
        // Donor branch variant (`ZSTD_match4Found_branch`): explicit
        // branch on the prefix filter. Faster when the branch is
        // strongly predictable — that's the typical Fast strategy
        // case where almost all hash table entries are within the
        // current window.
        if match_idx < prefix_start_index {
            return false;
        }
        unsafe { read32(ip) == read32(base.add(match_pos)) }
    }
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
/// every input that doesn't trigger one of the entry-time
/// `assert!`s (see the **Panics** section below for that list). The
/// contract below is about algorithmic correctness (correct output
/// sequences, donor-parity match coverage), not Rust memory safety.
/// Passing a smaller `data` is well-defined but the kernel falls
/// into the short-input early-return branch and emits no sequences,
/// which may not be what the caller wanted.
///
/// # Panics
///
/// Entry-time `assert!`s reject misuse loudly in every build (debug
/// AND release) rather than silently miscompressing:
/// - `block_start > data.len()` — invalidates the block range and
///   breaks the arithmetic used by both code paths: in the
///   short-input branch `tail_literals_len = data.len() -
///   block_start` underflows; in the main loop
///   `block_start + HASH_READ_SIZE` can wrap and skip the
///   short-input early-return entirely, then `base.add(ip0)`
///   reads out of bounds. Either side is a clean panic instead
///   of UB / garbage output.
/// - `data.len() > u32::MAX as usize` — the kernel stores
///   absolute positions into a u32 hash table and computes offsets
///   as u32, so larger inputs would silently truncate match indices.
/// - `MLS` outside `4..=8` — the donor's Fast strategy supports
///   only mls 4..=8; out-of-range MLS would route to a
///   non-existent hash formula.
/// - `MLS` != `hash_table.mls()` — a mismatched table layout would
///   cause the kernel to hash with the wrong formula and probe
///   entries indexed by a different formula, leading to garbage
///   match candidates.
///
/// The remaining-block length `data.len() - block_start` SHOULD be
/// at least `HASH_READ_SIZE` (8) bytes — `data` itself may be much
/// longer because it holds the prefix history before `block_start`,
/// so the slice's total size is not the relevant gate. The kernel's
/// short-input early-return (line below) compares precisely
/// `data.len() < block_start + HASH_READ_SIZE`, matching this
/// remaining-block phrasing.
///
/// The `ilimit = data.len() - HASH_READ_SIZE` cap constrains where
/// the main loop hashes and probes — i.e. it stops emitting new
/// matches once `ip0 >= ilimit`. It does NOT mean the trailing 7
/// bytes are ALWAYS literals: an in-progress forward match found at
/// `ip0 < ilimit` extends through `count_forward` and can reach all
/// the way to `iend`, leaving `tail_literals_len = 0`. The kernel
/// reports the actual number of trailing literal bytes (zero or
/// more) in `FastBlockResult.tail_literals_len`, and the caller
/// emits a terminal `Sequence::Literals` only when that value is
/// non-zero.
///
/// # Sequence emission contract
///
/// The kernel emits ONLY `Sequence::Triple` callbacks — one per
/// emitted match (repcode or explicit). Each `Triple` carries the
/// literal-run that precedes the match in its `literals` field, so
/// the kernel never needs a separate `Sequence::Literals` mid-block
/// call. The trailing bytes from the last anchor to the end of
/// `data` are NOT emitted via the closure; they are accounted for
/// by `FastBlockResult.tail_literals_len`, and emitting them as
/// the terminal `Sequence::Literals` (or absorbing them however
/// the caller wants) is the caller's responsibility. This rule
/// applies UNIFORMLY across every exit branch, including the
/// short-input early-return; without that uniformity a caller
/// wrapping the kernel's output would have to special-case "did
/// the kernel already emit the tail" per branch, which is exactly
/// the inconsistency this contract removes.
#[inline(always)]
pub(crate) fn compress_block_fast<const MLS: u32, const USE_CMOV: bool>(
    data: &[u8],
    block_start: usize,
    prefix_start_index: u32,
    hash_table: &mut FastHashTable,
    rep: [u32; 2],
    step_size: usize,
    mut handle_sequence: impl for<'a> FnMut(Sequence<'a>),
) -> FastBlockResult {
    // Donor's `stepSize = targetLength + !(targetLength) + 1`
    // (min 2). Callers must pass >= 2; values larger than 2 drive
    // the kernel's acceleration gradient on negative levels.
    // Validated in release builds too: `compress_block_fast` is a
    // safe `pub(crate)` boundary, so any direct caller bypassing
    // `FastKernelMatcher::with_params` / `reset` must not silently
    // mis-iterate the loop cadence on a mis-typed step. The
    // once-per-block branch is negligible relative to the per-block
    // hash/probe work that follows.
    assert!(
        step_size >= 2,
        "Fast kernel requires step_size >= 2 (got {step_size}); \
         the donor formula clamps to a min of 2",
    );
    // Real runtime check (not debug_assert) — MLS is a const-generic
    // so the wrong value would compile, and a mismatched table at the
    // call site would silently hash/probe with the wrong layout in
    // release: `compress_block_fast::<5, false>(..., &mut
    // FastHashTable::new(_, 4), ...)` would route to the mls=5 hash
    // formula but read entries indexed by the mls=4 hash → garbage
    // match candidates, mis-compression instead of a clean failure.
    // The `(4..=8).contains(&MLS)` check is logically redundant given
    // the `_ => debug_assert!(false)` arm in `FastHashTable::hash_ptr`,
    // but stating it here surfaces the contract at the call site of
    // the entry point and produces a clearer panic message than the
    // hash-table-internal one.
    assert!(
        (4..=8).contains(&MLS),
        "Fast kernel only supports MLS in 4..=8 (got {MLS})",
    );
    assert_eq!(
        MLS,
        hash_table.mls(),
        "compress_block_fast<{MLS}> called with hash_table whose mls = {}; \
         the table's hash formula must match the kernel's monomorphised mls",
        hash_table.mls(),
    );
    // Real runtime checks (not debug_assert) — both run in every
    // build because they catch distinct failure modes:
    //
    // `block_start > data.len()` is a memory-safety risk: it would
    // wrap `block_start + HASH_READ_SIZE` in the short-input guard
    // below, skip the early return, and proceed into the main loop
    // with an out-of-bounds ip0 → OOB read via `base.add(ip0)`.
    //
    // `data.len() > u32::MAX` is an algorithmic-correctness risk:
    // the kernel stores absolute positions into a u32 hash table
    // (`ip0 as u32`) and computes offsets as u32 (`offset as u32`).
    // For inputs above 4 GiB the silent truncation would corrupt
    // match indices and repcode offsets — every downstream pointer
    // read still stays in-bounds (we re-bound by `data.len()`
    // before any dereference), so it's not memory-unsafe, but the
    // emitted sequences would reference wrong positions and the
    // decoder would produce wrong output. Surfacing the bound at
    // entry turns this into a loud assertion instead of silent
    // miscompression.
    assert!(
        block_start <= data.len(),
        "block_start ({block_start}) must not exceed data.len() ({})",
        data.len(),
    );
    assert!(
        data.len() <= u32::MAX as usize,
        "FastKernel does not support data.len() ({}) > u32::MAX ({}); \
         the kernel stores absolute positions in a u32 hash table and \
         u32 offset codes, so larger inputs would silently truncate",
        data.len(),
        u32::MAX,
    );

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

    // Step-skip state: donor's `step = stepSize` (`targetLength + 1`
    // = 2 for Fast strategy with `targetLength == 0`). The 4-cursor
    // loop walks ip0/ip1 = ip0 + 1 adjacent + ip2/ip3 at `step` gap.
    // `next_step` is the absolute position where step doubles next;
    // donor increments by `kStepIncr = 1 << (kSearchStrength - 1)`.
    let mut step: usize = step_size;
    let mut next_step: usize = ip0.saturating_add(K_STEP_INCR);

    // 4-cursor donor port. Outer `'restart` loop matches donor's
    // `_start:` reentry: every emitted match `goto _start`s back
    // here for a fresh setup. The inner `do-while` walks ip0..ip3
    // with hash precomputation, repcode-at-ip2 probe, two explicit-
    // match probes (at ip0 then at the shifted ip0), and a step-
    // doubling cadence.
    'restart: while ip0 < ilimit {
        // _start: setup. ip0 already positioned; derive ip1/ip2/ip3
        // from current step. If even ip3 is past ilimit, the loop
        // can't make forward progress on this iteration — drain to
        // the cleanup path below. `checked_add` here defends against
        // a wild `step_size` (or a runaway `step` from the doubling
        // cadence) wrapping past `ilimit` and turning the
        // out-of-range guard below into a false-pass; on overflow we
        // take the same break path as a normal ip3-past-ilimit miss.
        let mut ip1 = ip0 + 1;
        let Some(mut ip2) = ip0.checked_add(step) else {
            break;
        };
        let Some(mut ip3) = ip2.checked_add(1) else {
            break;
        };
        if ip3 > ilimit {
            break;
        }

        // Hash precomputation for ip0 + ip1 (donor lines 261-262).
        // SAFETY: ip0, ip1 < ilimit = iend - 8, so ≥ 8 readable
        // bytes at each `base + ip*`. MLS ≤ 8 matches hash_ptr's
        // contract.
        let mut hash0 = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip0)) };
        let mut hash1 = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip1)) };
        let mut match_idx = unsafe { hash_table.get(hash0) };

        // Inner do-while body. On any match, break out with the
        // `MatchFound` enum carrying the match coordinates; the
        // post-loop block handles backward/forward extension + emit.
        enum MatchFound {
            Rep {
                new_ip: usize,
                match0: usize,
                m_len: usize,
                // Donor's `current0` — position of the LAST hash
                // writeback before the rep was found. For the
                // rep-at-ip2 path that's the iter-start ip0 (the
                // writeback at the top of the do-while body).
                // Used post-emit to insert hash at `current0 + 2`
                // (donor zstd_fast.c:407). Captured BEFORE the
                // backward-extension decrement so it doesn't
                // collapse onto new_ip for rep matches.
                current0: usize,
            },
            Explicit {
                new_ip: usize,
                match_idx: u32,
                // Same role as `current0` on the Rep variant: the
                // position of the LAST writeback before the match.
                // Path 1 (probe at iter-start ip0) → current0 == ip0;
                // path 2 (probe at shifted ip0) → current0 == shifted
                // ip0. Identical to new_ip for explicit since
                // explicit emits don't decrement new_ip pre-emit.
                current0: usize,
            },
        }
        let found: Option<MatchFound> = loop {
            // Repcode probe at ip2 (donor line 268). Load rval
            // first so it stays consistent even if writeback below
            // races. SAFETY: ip2 < ilimit ⇒ ≥ 4 readable bytes at
            // ip2; ip2 ≥ rep_offset1 when rep_offset1 is non-zero
            // because the save/restore prologue zeroed any
            // rep_offset that exceeded the prefix-distance.
            let rval = if rep_offset1 > 0 {
                unsafe { read32(base.add(ip2 - rep_offset1 as usize)) }
            } else {
                // Sentinel — the equality check below can't pass
                // when rep_offset1 == 0 anyway because the
                // `rep_offset1 > 0` guard short-circuits.
                0
            };

            // Writeback hash for ip0 (donor line 272). Donor writes
            // BEFORE the rep probe so the hash table reflects ip0
            // even if the iteration's match comes from rep at ip2.
            // SAFETY: hash0 from hash_ptr ⇒ in-bounds; ip0 ≤ u32::MAX
            // by the entry-point cap.
            unsafe { hash_table.put(hash0, ip0 as u32) };

            // Repcode-at-ip2 check.
            if rep_offset1 > 0 && unsafe { read32(base.add(ip2)) } == rval {
                // Repcode match. ip0 fast-forwards to ip2; backward-
                // extend by 1 if the byte before ip2 also matches.
                // Donor's `mLength = ip0[-1] == match0[-1]` is a
                // single-byte extension with implicit `new_ip >
                // anchor` AND `match > prefix` checks via the
                // prologue's save/restore on rep_offset1.
                let mut new_ip = ip2;
                let mut match0 = new_ip - rep_offset1 as usize;
                let mut m_len: usize = 4;
                if new_ip > anchor
                    && match0 > prefix_start_index as usize
                    && data[new_ip - 1] == data[match0 - 1]
                {
                    new_ip -= 1;
                    match0 -= 1;
                    m_len += 1;
                }
                // Safe writeback for hash1 — ip1 is BEFORE ip2 (the
                // match site), so its position won't conflict with
                // the match's forward extension. Donor lines 286-287.
                unsafe { hash_table.put(hash1, ip1 as u32) };
                break Some(MatchFound::Rep {
                    new_ip,
                    match0,
                    m_len,
                    // Iter-start ip0 (the writeback at line 426
                    // above) — donor's `current0` for this path.
                    current0: ip0,
                });
            }

            // First explicit-match probe at ip0 (donor line 292).
            if unsafe {
                match_found::<USE_CMOV>(
                    base.add(ip0),
                    base,
                    match_idx,
                    prefix_start_index,
                    ip0,
                    iend_addr,
                )
            } {
                // Safe writeback for hash1 (ip1 = ip0 + 1, before
                // search resumption). Donor line 296.
                unsafe { hash_table.put(hash1, ip1 as u32) };
                break Some(MatchFound::Explicit {
                    new_ip: ip0,
                    match_idx,
                    current0: ip0,
                });
            }

            // Shift: ip0 ← ip1, ip1 ← ip2, ip2 ← ip3. hash0 ← hash1
            // (precomputed last iteration). hash1 is recomputed from
            // the CURRENT ip2 (before the cursor shift below), which
            // becomes the new ip1 — so post-shift `hash1` matches
            // the new `ip1`, NOT the new `ip2`.
            match_idx = unsafe { hash_table.get(hash1) };
            hash0 = hash1;
            hash1 = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip2)) };
            ip0 = ip1;
            ip1 = ip2;
            ip2 = ip3;

            // Writeback for new ip0. Donor lines 314-315.
            unsafe { hash_table.put(hash0, ip0 as u32) };

            // Second explicit-match probe at the shifted ip0
            // (donor line 317).
            if unsafe {
                match_found::<USE_CMOV>(
                    base.add(ip0),
                    base,
                    match_idx,
                    prefix_start_index,
                    ip0,
                    iend_addr,
                )
            } {
                // Conditional writeback: only safe if `step <= 4`
                // (donor lines 319-324) — otherwise ip1 might fall
                // past the match start when we resume scanning.
                if step <= 4 {
                    unsafe { hash_table.put(hash1, ip1 as u32) };
                }
                break Some(MatchFound::Explicit {
                    new_ip: ip0,
                    match_idx,
                    // After the shift + writeback at line 488,
                    // current0 == shifted ip0.
                    current0: ip0,
                });
            }

            // Shift again with the larger `step` gap. The second
            // shift is the one that advances ip2/ip3 by `step`
            // rather than by 1; this is where the step-skip kicks
            // in. Donor lines 329-339.
            match_idx = unsafe { hash_table.get(hash1) };
            hash0 = hash1;
            hash1 = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip2)) };
            ip0 = ip1;
            ip1 = ip2;
            // Same overflow defence as the loop-head setup: a wild
            // `step` (e.g. after enough step-doubling cycles) could
            // otherwise wrap `ip0 + step` past `usize::MAX` and bypass
            // the `ip3 > ilimit` guard. On overflow we drain to the
            // post-loop cleanup, identical to the normal "ran out of
            // room" exit.
            let Some(new_ip2) = ip0.checked_add(step) else {
                break None;
            };
            let Some(new_ip3) = ip1.checked_add(step) else {
                break None;
            };
            ip2 = new_ip2;
            ip3 = new_ip3;

            // Step-doubling: donor lines 342-347. Drives the
            // kSearchStrength-based acceleration on incompressible
            // regions.
            if ip2 >= next_step {
                step += 1;
                next_step = next_step.saturating_add(K_STEP_INCR);
            }

            // do-while termination: if ip3 walks past ilimit, drain.
            if ip3 > ilimit {
                break None;
            }
        };

        // _cleanup path: drain to the post-loop save/restore.
        let Some(found) = found else {
            break 'restart;
        };

        // _offset / _match — backward + forward extension + emit.
        // Donor's `current0` = position of the LAST hash writeback
        // before the match was found. Captured at break-time on
        // each MatchFound variant so the Rep path's backward
        // extension doesn't collapse `current0` onto the post-
        // backward `new_ip` (donor zstd_fast.c:407 uses the
        // pre-backward iter-start position).
        let current0 = match found {
            MatchFound::Rep { current0, .. } => current0,
            MatchFound::Explicit { current0, .. } => current0,
        };
        let (mut match_ip, mut match_pos, mut m_len, offset, is_rep) = match found {
            MatchFound::Rep {
                new_ip,
                match0,
                m_len,
                current0: _,
            } => (new_ip, match0, m_len, rep_offset1 as usize, true),
            MatchFound::Explicit {
                new_ip,
                match_idx,
                current0: _,
            } => {
                let match_pos = match_idx as usize;
                let offset = new_ip - match_pos;
                // Rotate the rep stack ahead of backward extension
                // — donor stores the offset BEFORE the backward
                // walk (lines 381-383). This way the explicit-match
                // path's backward extension is bounded by the
                // anchor + prefix_start_index pair; subsequent
                // iterations get a tighter rep_offset1.
                rep_offset2 = rep_offset1;
                rep_offset1 = offset as u32;
                (new_ip, match_pos, 4usize, offset, false)
            }
        };

        // Backward extension — only for explicit matches; rep path
        // already handled the 1-byte backward step above.
        if !is_rep {
            while match_ip > anchor
                && match_pos > prefix_start_index as usize
                && data[match_ip - 1] == data[match_pos - 1]
            {
                match_ip -= 1;
                match_pos -= 1;
                m_len += 1;
            }
        }

        // Forward extension via ZSTD_count.
        // SAFETY: both pointers stay within `data`; iend pointer
        // arithmetic stays in bounds.
        let forward = unsafe {
            count_forward(
                base.add(match_ip + m_len),
                base.add(match_pos + m_len),
                base.add(iend_addr),
            )
        };
        m_len += forward;

        // Emit.
        let literals = &data[anchor..match_ip];
        handle_sequence(Sequence::Triple {
            literals,
            offset,
            match_len: m_len,
        });

        ip0 = match_ip + m_len;
        anchor = ip0;

        // Immediate-rep2 inner loop (donor lines 404-420). After a
        // match emit, donor (a) refills the hash for `ip0 - 2` to
        // give the next scan a head start, (b) probes for repcode-2
        // matches at the new ip0 — if found, emit them as lit_len=0
        // rep1 sequences (with rep stack swap) until exhausted.
        if ip0 <= ilimit {
            // Donor inserts TWO hashes after a match emit
            // (zstd_fast.c:407-408): one at `current0 + 2` (2 bytes
            // past the trigger-ip position, filling a slot inside
            // the now-consumed match), one at `ip0 - 2` (2 bytes
            // before the next scan start). Both are vital — missing
            // either makes the hash table sparser than donor's and
            // costs subsequent matches.
            //
            // SAFETY: each `base.add(N - 2)` covers ≥ 8 readable
            // bytes when N - 2 ≤ ilimit (ilimit = iend - 8).
            // Non-overflowing bounds check: on 32-bit targets even
            // the raw `current0 + 2` can wrap when `current0`
            // approaches `usize::MAX`, producing a small value that
            // would then pass the `+ HASH_READ_SIZE` check and call
            // `hash_ptr` at the wrong position — out-of-bounds read
            // under the surrounding `unsafe`. Chain the additions
            // through `checked_add` so the slot is skipped if either
            // step overflows.
            let in_range_fwd = current0
                .checked_add(2)
                .and_then(|c| c.checked_add(HASH_READ_SIZE))
                .is_some_and(|end| end <= iend_addr);
            if in_range_fwd {
                // Safe to compute the index here — `in_range_fwd`
                // guarantees `current0 + 2 + HASH_READ_SIZE` fits in
                // `usize`, so the `+ 2` cannot wrap.
                let current0_plus_2 = current0 + 2;
                let h_fwd = unsafe { hash_table.hash_ptr::<MLS>(base.add(current0_plus_2)) };
                unsafe { hash_table.put(h_fwd, current0_plus_2 as u32) };
            }
            if ip0 >= 2 {
                let h_back = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip0 - 2)) };
                unsafe { hash_table.put(h_back, (ip0 - 2) as u32) };
            }

            // Repcode-2 inner loop. Donor swaps rep1 / rep2 on each
            // hit so the just-found offset becomes the new rep1.
            while rep_offset2 > 0
                && ip0 <= ilimit
                && ip0 >= rep_offset2 as usize
                && unsafe { read32(base.add(ip0)) == read32(base.add(ip0 - rep_offset2 as usize)) }
            {
                // 4-byte match guaranteed by the equality probe.
                // Extend forward via count_forward starting at
                // ip0 + 4.
                let r_off = rep_offset2 as usize;
                let r_extra = unsafe {
                    count_forward(
                        base.add(ip0 + 4),
                        base.add(ip0 + 4 - r_off),
                        base.add(iend_addr),
                    )
                };
                let r_len = 4 + r_extra;

                // Swap rep1 / rep2 (donor line 414).
                core::mem::swap(&mut rep_offset1, &mut rep_offset2);

                // Hash refill at ip0 (donor line 415).
                let h_at = unsafe { hash_table.hash_ptr::<MLS>(base.add(ip0)) };
                unsafe { hash_table.put(h_at, ip0 as u32) };

                // Emit lit_len=0 rep1 sequence.
                handle_sequence(Sequence::Triple {
                    literals: &data[anchor..ip0],
                    offset: r_off,
                    match_len: r_len,
                });

                ip0 += r_len;
                anchor = ip0;
            }
        }

        // `goto _start` — restart the outer loop with fresh step
        // (reset to the level-resolved initial step_size). The
        // `saturating_add` here is symmetric with the inner loop's
        // step-doubling site: ip0 is already < ilimit < data.len()
        // <= u32::MAX, so the wrap would require an unrepresentable
        // `K_STEP_INCR`, but staying defensive keeps the cursor math
        // wrap-free on every cold reset of the outer state.
        step = step_size;
        next_step = ip0.saturating_add(K_STEP_INCR);
        continue 'restart;
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
            4 => compress_block_fast::<4, false>(data, 0, 0, &mut table, [0, 0], 2, &mut handle),
            5 => compress_block_fast::<5, false>(data, 0, 0, &mut table, [0, 0], 2, &mut handle),
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
        let result = compress_block_fast::<4, false>(data, 0, 0, &mut table, rep, 2, &mut handle);
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
        let _ = compress_block_fast::<4, false>(&data, 0, 5, &mut table, [0, 0], 2, &mut handle);

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
        let _ = compress_block_fast::<4, false>(&data, 0, 0, &mut table, [0, 0], 2, &mut handle);

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
        let result =
            compress_block_fast::<4, false>(&data, 0, 0, &mut table, [huge, 7], 2, &mut handle);
        assert_eq!(
            result.rep[0], huge,
            "out-of-range rep_offset1 must be restored verbatim across the block",
        );
        // rep_offset2 was also out of range (max_rep ≈ 0..63, 7 > 1).
        // Donor restores it through offset_saved2; the in-range
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
                let _ =
                    compress_block_fast::<4, true>(&data, 0, 0, &mut table, [0, 0], 2, &mut handle);
            } else {
                let _ = compress_block_fast::<4, false>(
                    &data,
                    0,
                    0,
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
        let branch_result = unsafe { match_found::<false>(ip, base, 4, 10, ip_pos, data.len()) };
        assert!(
            !branch_result,
            "branch variant must reject out-of-window match_idx"
        );
        let cmov_result = unsafe { match_found::<true>(ip, base, 4, 10, ip_pos, data.len()) };
        assert!(
            !cmov_result,
            "cmov variant must reject out-of-window match_idx even when \
             ip bytes coincide with CMOV_DUMMY",
        );
    }
}
