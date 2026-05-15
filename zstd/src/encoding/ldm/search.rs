//! Bucket lookup + forward / backward extension for the LDM
//! producer.
//!
//! Implements the per-split candidate selection from donor
//! `ZSTD_ldm_generateSequences_internal` (`zstd_ldm.c:405-466`)
//! v1.5.7, **prefix-only path**. The two-segment `extDict`
//! variant (donor `ZSTD_count_2segments` +
//! `ZSTD_ldm_countBackwardsMatch_2segments`) is deferred — the
//! current Rust encoder does not surface a separate `extDict`
//! buffer, so every byte the producer can reach lives in a
//! single contiguous `history` slice and the prefix-only path
//! is bit-for-bit equivalent to donor on those inputs.
//!
//! Donor parity anchors:
//! * `ZSTD_count`                        → [`super::super::match_table::helpers::common_prefix_len`]
//! * `ZSTD_ldm_countBackwardsMatch`      → [`count_backwards_match`]
//! * Per-bucket best-match selection     → [`find_best_match`]

use super::super::match_table::helpers::common_prefix_len;
use super::table::LdmHashTable;

/// Result of [`find_best_match`]: a verified LDM candidate.
///
/// Holds the *resolved* forward and backward lengths separately
/// because the caller needs both to derive the emitted raw-seq
/// `lit_length` (`split - backward - anchor`) and the wire-format
/// match length (`forward + backward`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct LdmMatch {
    /// Absolute byte position of the matching window's start in
    /// the back reference (donor `bestEntry->offset`).
    pub(crate) match_pos: u32,
    /// Bytes that matched forward from `split`.
    pub(crate) forward_len: usize,
    /// Bytes that matched backward from `split` (capped by
    /// `split - anchor` and by `match_pos > lowest_index`).
    pub(crate) backward_len: usize,
}

impl LdmMatch {
    /// Total match length emitted on the wire. Donor
    /// `mLength = forwardMatchLength + backwardMatchLength`
    /// (`zstd_ldm.c:477`).
    pub(crate) const fn total_len(&self) -> usize {
        self.forward_len + self.backward_len
    }
}

/// Donor `ZSTD_ldm_countBackwardsMatch` (`zstd_ldm.c:214-225`):
/// walk left from `(p_in, p_match)` while bytes still match and
/// both pointers stay above their respective lower bounds.
///
/// Bounds expressed as absolute byte positions into the same
/// `history` slice the caller passed in: `p_in_abs` is the
/// candidate's "split" position, `p_match_abs` is the back-ref
/// position; the walk stops when either reaches its bound or the
/// bytes diverge.
///
/// Returns the number of matched backward bytes (caps at the
/// tighter of the two `min(p_in_abs - anchor, p_match_abs -
/// match_base)` distances).
pub(crate) fn count_backwards_match(
    history: &[u8],
    p_in_abs: usize,
    anchor_abs: usize,
    p_match_abs: usize,
    match_base_abs: usize,
) -> usize {
    debug_assert!(p_in_abs <= history.len());
    debug_assert!(p_match_abs <= history.len());
    debug_assert!(anchor_abs <= p_in_abs);
    debug_assert!(match_base_abs <= p_match_abs);

    let mut p_in = p_in_abs;
    let mut p_match = p_match_abs;
    let mut len = 0usize;
    while p_in > anchor_abs && p_match > match_base_abs && history[p_in - 1] == history[p_match - 1]
    {
        p_in -= 1;
        p_match -= 1;
        len += 1;
    }
    len
}

/// Per-call inputs to [`find_best_match`]. Bundled into a struct
/// so the public function avoids the `clippy::too_many_arguments`
/// trip-wire while keeping each input clearly named (every field
/// has a distinct semantic role; merging would obscure the donor
/// citations).
pub(crate) struct FindBestMatchInputs<'a> {
    /// Full per-frame byte slice the match is verified against.
    pub(crate) history: &'a [u8],
    /// Absolute byte position of the candidate window's start.
    /// Donor `split`.
    pub(crate) split_abs: usize,
    /// Leftmost byte the producer is still allowed to emit as
    /// literal — the previous emitted match's post-match
    /// boundary or `block_start` at frame entry. Donor `anchor`.
    pub(crate) anchor_abs: usize,
    /// Donor `lowestIndex` — entries with
    /// `offset <= lowest_index_abs` are stale and rejected.
    pub(crate) lowest_index_abs: u32,
    /// Donor `params->minMatchLength` — forward matches shorter
    /// than this floor are filtered out.
    pub(crate) min_match_length: usize,
}

/// Walk every slot of the bucket associated with `hash_id`, scoring
/// each entry by `forward + backward` match length, and return the
/// best candidate strictly above the donor's
/// `forward >= min_match_length` floor. Returns `None` when no
/// bucket entry survives the filter.
///
/// Mirrors the per-split inner loop in
/// `ZSTD_ldm_generateSequences_internal` (`zstd_ldm.c:405-466`)
/// prefix-only path. The caller must pre-resolve `hash_id` via
/// [`LdmHashTable::bucket_mask`].
pub(crate) fn find_best_match(
    table: &LdmHashTable,
    hash_id: u32,
    checksum: u32,
    inputs: FindBestMatchInputs<'_>,
) -> Option<LdmMatch> {
    let FindBestMatchInputs {
        history,
        split_abs,
        anchor_abs,
        lowest_index_abs,
        min_match_length,
    } = inputs;
    debug_assert!(split_abs <= history.len());
    debug_assert!(anchor_abs <= split_abs);

    let bucket = table.bucket(hash_id);
    let mut best: Option<LdmMatch> = None;
    let iend = history.len();
    // Prefix-only path: history starts at absolute 0 → both the
    // `match_base` (donor `base`) and the low-prefix pointer
    // (donor `lowPrefixPtr`) collapse to byte 0.
    let match_base_abs = 0usize;
    let low_prefix_abs = 0usize;

    for entry in bucket {
        // Donor `zstd_ldm.c:431`: skip stale or wrong-checksum
        // entries.
        if entry.checksum != checksum || entry.offset <= lowest_index_abs {
            continue;
        }
        let match_pos = entry.offset as usize;
        // Defensive: the producer should never insert an offset
        // beyond `history.len()`, but if a caller injects a
        // synthetic table this guard avoids panicking on the
        // slice below.
        if match_pos >= iend {
            continue;
        }

        // Forward match: bytes that compare equal starting from
        // `split` vs `match_pos`. Donor `ZSTD_count(split, pMatch,
        // iend)`.
        let forward_len = common_prefix_len(&history[split_abs..], &history[match_pos..]);
        if forward_len < min_match_length {
            continue;
        }

        // Backward match: walk left as far as both `anchor`-bound
        // and `low_prefix`-bound permit. Donor `zstd_ldm.c:455-456`.
        let backward_len = count_backwards_match(
            history,
            split_abs,
            anchor_abs,
            match_pos,
            low_prefix_abs.max(match_base_abs),
        );

        let candidate = LdmMatch {
            match_pos: match_pos as u32,
            forward_len,
            backward_len,
        };
        match best {
            Some(b) if candidate.total_len() <= b.total_len() => {}
            _ => best = Some(candidate),
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::ldm::table::{LdmEntry, LdmHashTable};

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
        table.insert(
            1,
            LdmEntry {
                offset: 4,
                checksum: 0x1111_1111,
            },
        );
        let history = b"abcdefghabcdefgh";
        let m = find_best_match(
            &table,
            1,
            0xDEAD_BEEF,
            FindBestMatchInputs {
                history,
                split_abs: 8,
                anchor_abs: 0,
                lowest_index_abs: 0,
                min_match_length: 4,
            },
        );
        assert!(m.is_none(), "wrong checksum must be filtered out");
    }

    /// Bucket lookup returns `None` when the offset is at or
    /// below `lowest_index_abs` (donor staleness rejection).
    #[test]
    fn find_best_match_rejects_stale_entries() {
        let mut table = fresh_table();
        table.insert(
            1,
            LdmEntry {
                offset: 4,
                checksum: 0xCAFE,
            },
        );
        let history = b"abcdefghabcdefgh";
        // lowest_index_abs = 4 → entry offset 4 is NOT strictly
        // greater → rejected.
        let m = find_best_match(
            &table,
            1,
            0xCAFE,
            FindBestMatchInputs {
                history,
                split_abs: 8,
                anchor_abs: 0,
                lowest_index_abs: 4,
                min_match_length: 4,
            },
        );
        assert!(m.is_none(), "stale entry must be filtered out");
    }

    /// `find_best_match` returns the longest combined
    /// forward+backward match across the bucket. Engineered
    /// fixture: a 4-byte preamble (so the donor `offset > 0`
    /// staleness floor is satisfied — entry.offset == 0 is the
    /// reserved "empty slot" sentinel) followed by two
    /// repetitions of "abcdefgh". The single candidate at
    /// offset 4 should produce forward 8 + backward 0 = 8.
    #[test]
    fn find_best_match_picks_longest_combined_match() {
        let mut table = fresh_table();
        table.insert(
            1,
            LdmEntry {
                offset: 4,
                checksum: 0xCAFE,
            },
        );
        let history = b"PPPPabcdefghabcdefgh";
        // split at position 12, anchor at 12 → no backward room.
        // The forward count should match 8 bytes ("abcdefgh").
        let m = find_best_match(
            &table,
            1,
            0xCAFE,
            FindBestMatchInputs {
                history,
                split_abs: 12,
                anchor_abs: 12,
                lowest_index_abs: 0,
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
        table.insert(
            1,
            LdmEntry {
                offset: 2,
                checksum: 0xCAFE,
            },
        );
        let history = b"XYabcdefghXYabcdefgh";
        // split at 12 (start of second "abcdefgh"), anchor at 10
        // → backward up to 2 bytes ("XY" at positions 10..12 vs
        // 0..2). Forward count: 8 bytes ("abcdefgh").
        let m = find_best_match(
            &table,
            1,
            0xCAFE,
            FindBestMatchInputs {
                history,
                split_abs: 12,
                anchor_abs: 10,
                lowest_index_abs: 0,
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
    /// bytes shift both candidate offsets above the donor `offset
    /// > 0` floor.
    #[test]
    fn find_best_match_prefers_longer_total_across_slots() {
        let mut table = fresh_table();
        // Slot 0: offset 4 (short forward match — only 4 bytes).
        table.insert(
            1,
            LdmEntry {
                offset: 4,
                checksum: 0xCAFE,
            },
        );
        // Slot 1: offset 8 (8-byte match — extends further forward).
        table.insert(
            1,
            LdmEntry {
                offset: 8,
                checksum: 0xCAFE,
            },
        );
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
                history,
                split_abs: 16,
                anchor_abs: 16,
                lowest_index_abs: 0,
                min_match_length: 4,
            },
        )
        .expect("a valid candidate must be found");
        assert_eq!(m.match_pos, 8, "longer-forward winner must be picked");
        assert_eq!(m.forward_len, 8);
    }

    /// Forward match below `min_match_length` is rejected even
    /// when the checksum agrees (donor `zstd_ldm.c:444/452`).
    #[test]
    fn find_best_match_filters_short_forward_matches() {
        let mut table = fresh_table();
        table.insert(
            1,
            LdmEntry {
                offset: 4,
                checksum: 0xCAFE,
            },
        );
        let history = b"PPPPabXXXXXXab";
        // 2-byte forward match from split=12 vs match=4, but
        // min_match_length = 4 → rejected.
        let m = find_best_match(
            &table,
            1,
            0xCAFE,
            FindBestMatchInputs {
                history,
                split_abs: 12,
                anchor_abs: 12,
                lowest_index_abs: 0,
                min_match_length: 4,
            },
        );
        assert!(m.is_none());
    }
}
