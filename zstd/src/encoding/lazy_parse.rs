//! Shared, C-faithful lazy-parse decision logic.
//!
//! Upstream zstd has a SINGLE lazy parser — `ZSTD_compressBlock_lazy_generic`
//! (`lib/compress/zstd_lazy.c`) — parameterized by the search method
//! (hash-chain / binary-tree / row-hash) and the lookahead `depth`. The match
//! finders differ per strategy; the PARSE DECISION (how a candidate at the
//! current position is weighed against one a byte or two ahead) is identical
//! across all of them.
//!
//! This module hosts that one decision so the per-strategy matchers
//! (HC / Row / dfast) share it instead of each carrying a divergent copy. The
//! register-hungry part is the search kernel, which stays specialized per
//! strategy and is injected here as a closure — the decision itself is a
//! handful of comparisons per position, so sharing it adds no spill pressure.

/// Upstream "match gain" heuristic (`gain = match_len*4 - offset_bits`) used by
/// the lazy lookahead to compare a candidate against one ahead
/// (`ZSTD_compressBlock_lazy_generic`, zstd_lazy.c). `offset` is the raw match
/// distance and must be `> 0` (zstd offsets are 1-indexed).
#[inline]
pub(crate) fn lazy_match_gain(match_len: usize, offset: usize) -> i32 {
    debug_assert!(
        offset > 0,
        "zstd offsets are 1-indexed, offset=0 is invalid"
    );
    let offset_bits = 32 - (offset as u32).leading_zeros() as i32;
    (match_len as i32) * 4 - offset_bits
}

/// C-faithful lazy commit/defer decision shared by every strategy's parse loop.
///
/// Returns `true` to COMMIT the current best match, `false` to DEFER (a
/// lookahead position wins and the caller should advance one byte and re-pick).
///
/// Mirrors upstream `ZSTD_compressBlock_lazy_generic` (zstd_lazy.c:1629-1700):
/// the depth-1 lookahead defers when `gain(next) > gain(best) + 4`; the depth-2
/// lookahead (only when `lazy_depth >= 2`) defers when
/// `gain(next2) > gain(best) + 7` — i.e. a `+3` increment over depth-1, NOT
/// `+4`. The only early-out is the out-of-bounds guard plus the
/// `target_len` sufficient-length shortcut.
///
/// `search(abs_pos, lit_len) -> Option<(match_len, offset)>` runs the
/// strategy's own match finder at a position (injected so the kernel stays
/// specialized). `history_end` is the absolute end of searchable input and
/// `min_match` the finder's minimum match length (its forward-bounds margin).
#[inline]
#[expect(
    clippy::too_many_arguments,
    reason = "one shared parse driver with genuinely distinct inputs (best, \
              target, depth, position, bounds, finder); bundling them would only \
              obscure the C-faithful decision it mirrors"
)]
pub(crate) fn lazy_should_commit(
    best_len: usize,
    best_off: usize,
    target_len: usize,
    lazy_depth: u8,
    abs_pos: usize,
    lit_len: usize,
    history_end: usize,
    min_match: usize,
    mut search: impl FnMut(usize, usize) -> Option<(usize, usize)>,
) -> bool {
    if best_len >= target_len || abs_pos + 1 + min_match > history_end {
        return true;
    }

    let current_gain = lazy_match_gain(best_len, best_off) + 4;

    if let Some((next_len, next_off)) = search(abs_pos + 1, lit_len + 1)
        && lazy_match_gain(next_len, next_off) > current_gain
    {
        return false;
    }

    if lazy_depth >= 2 && abs_pos + 2 + min_match <= history_end {
        // +3 over depth-1's +4 = upstream's +7 base bias at depth 2.
        if let Some((next_len, next_off)) = search(abs_pos + 2, lit_len + 2)
            && lazy_match_gain(next_len, next_off) > current_gain + 3
        {
            return false;
        }
    }

    true
}
