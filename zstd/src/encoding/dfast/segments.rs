//! The owned window read by position across its two segments.

use crate::encoding::fastpath::EncodeTier;
use crate::encoding::opt::types::MatchCandidate;

/// The owned window as the paths that read a candidate by its position see
/// it: the ext segment, the window's older part once the history has wrapped
/// (empty while the window is one segment), then the prefix, which holds the
/// block (upstream `dictBase` / `base` split, `ZSTD_window_t`).
///
/// Raw pointers taken from the history's whole room, not slices of each
/// segment: a key read at one of the ext segment's last positions runs past
/// the segment's end into the room after it.
#[derive(Clone, Copy)]
pub(super) struct WindowSegments {
    /// The ext segment's first byte. Up to a key's width of bytes after its
    /// last are readable and equal the prefix's first bytes
    /// (`DfastMatchGenerator::mirror_prefix_head`).
    pub(super) ext: *const u8,
    /// Position of the ext segment's first byte, the window's floor.
    pub(super) ext_abs: usize,
    /// The prefix's first committed byte.
    pub(super) prefix: *const u8,
    /// Position of the prefix's first byte, right after the ext segment's last.
    pub(super) prefix_abs: usize,
    /// One past the prefix's last committed position.
    pub(super) end_abs: usize,
}

impl WindowSegments {
    /// Pointer to the byte at `abs`, a position of the window. At an ext
    /// position, up to a key's width of bytes past the segment's end are
    /// readable through it (see [`Self::ext`]).
    #[inline(always)]
    pub(super) fn ptr(&self, abs: usize) -> *const u8 {
        // The end itself is a valid pointer for a count of no bytes.
        debug_assert!(abs >= self.ext_abs && abs <= self.end_abs);
        if abs < self.prefix_abs {
            self.ext.wrapping_add(abs - self.ext_abs)
        } else {
            self.prefix.wrapping_add(abs - self.prefix_abs)
        }
    }

    /// The byte at `abs`, a position of the window.
    #[inline(always)]
    pub(super) fn byte(&self, abs: usize) -> u8 {
        // SAFETY: a window position names a committed byte of its segment.
        unsafe { *self.ptr(abs) }
    }

    /// Length of the match of the window bytes at `cand` against those at
    /// `cur` (a prefix position after it), up to `max`, counted across from
    /// the ext segment's end into the prefix as the window runs (upstream
    /// `ZSTD_count_2segments`).
    ///
    /// # Safety
    ///
    /// `cand < cur`, both window positions, `cur >= prefix_abs` and
    /// `cur + max <= end_abs`.
    #[inline(always)]
    pub(super) unsafe fn count_forward<K: EncodeTier>(
        &self,
        cand: usize,
        cur: usize,
        max: usize,
    ) -> usize {
        let rhs = self.ptr(cur);
        // Ext bytes left from `cand`; a prefix candidate has none, which is
        // what the clamp at zero says.
        let room = self.prefix_abs.saturating_sub(cand);
        if room == 0 || room >= max {
            // SAFETY: `max` bytes from `cand` inside one segment, and from
            // `cur` inside the prefix, by the caller's bounds.
            return unsafe { K::common_prefix_len_ptr(self.ptr(cand), rhs, max) };
        }
        // SAFETY: `room` bytes of the ext segment from `cand`, then the prefix
        // from its start; `rhs` runs `max` bytes inside the prefix.
        unsafe {
            let n = K::common_prefix_len_ptr(self.ptr(cand), rhs, room);
            if n < room {
                n
            } else {
                n + K::common_prefix_len_ptr(self.prefix, rhs.add(n), max - n)
            }
        }
    }

    /// `extend_backwards_shared` across the two segments: walk the match of
    /// `cand` against `cur` back over at most `lit_len` bytes, never before
    /// the window's floor.
    pub(super) fn extend_back(
        &self,
        mut cand: usize,
        mut cur: usize,
        mut match_len: usize,
        lit_len: usize,
    ) -> MatchCandidate {
        let min_cur = cur - lit_len;
        while cur > min_cur && cand > self.ext_abs && self.byte(cand - 1) == self.byte(cur - 1) {
            cand -= 1;
            cur -= 1;
            match_len += 1;
        }
        MatchCandidate {
            start: cur,
            offset: cur - cand,
            match_len,
        }
    }
}
