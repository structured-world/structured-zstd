//! A match finder's history kept as a ring with an older segment, the way
//! upstream keeps its input buffer and extDict (`ZSTD_window_t`,
//! `ZSTD_window_update`): once the history is full, the next fill starts
//! over at the buffer's front instead of moving the window there, and the
//! committed window left behind stays readable where it lies.

use super::fastpath::EncodeTier;
use super::opt::types::MatchCandidate;
use super::workspace::HistoryBuf;
use crate::common::MAX_BLOCK_SIZE;

/// Bytes a key read may run past the ext segment's end: the widest key a
/// match finder hashes, which the room holds right after the segment as a
/// copy of the prefix's first bytes (see [`Ring::after_fill`]).
pub(crate) const KEY_SLACK: usize = 8;

/// The ring bookkeeping of a history: where its older (ext) segment lies.
///
/// The history's own `history_start` / `history_abs_start` keep their
/// meaning for the prefix and the window: `history_abs_start` is the
/// window's floor, which lies in the ext segment while there is one, and the
/// prefix then starts at buffer offset 0 and position `history_abs_start +
/// len`.
#[derive(Clone, Debug)]
pub(crate) struct Ring {
    /// Buffer offset of the ext segment's first live byte.
    pub(crate) off: usize,
    /// Live bytes of the ext segment; 0 while the window is one segment.
    pub(crate) len: usize,
    /// How many of the prefix's first bytes, up to [`KEY_SLACK`], the room
    /// holds right after the ext segment.
    mirrored: usize,
    /// Whether a full history wraps; off while a reader takes the window as
    /// one slice.
    pub(crate) on: bool,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub(crate) const fn new() -> Self {
        Self {
            off: 0,
            len: 0,
            mirrored: 0,
            on: true,
        }
    }

    /// History bytes the window may occupy as a ring: the window, the block
    /// being matched, the input read ahead of it, and the slack that keeps a
    /// fill after a wrap below the ext segment's live bytes until eviction
    /// has passed them. After a wrap the prefix ends at the bytes committed
    /// since plus those read ahead, while eviction has moved the ext
    /// segment's live start by at least the bytes committed once the window
    /// is full; a window not yet full when it wraps grows into the dead room
    /// ahead of it first, which the slack covers as well.
    pub(crate) fn capacity(max_window: usize) -> usize {
        max_window + 4 * MAX_BLOCK_SIZE as usize
    }

    /// Room a ring needs: its capacity and the mirror after the ext segment,
    /// which can lie at the capacity's edge.
    pub(crate) fn room(max_window: usize) -> usize {
        Self::capacity(max_window) + KEY_SLACK
    }

    /// Makes room for a fill of `fill` bytes: wraps when the prefix would run
    /// past the ring, else checks the fill stays below the ext segment's live
    /// bytes, else (one segment) reserves as a growing history does.
    pub(crate) fn before_fill(
        &mut self,
        history: &mut HistoryBuf,
        history_start: &mut usize,
        uncommitted: usize,
        fill: usize,
        max_window: usize,
    ) {
        let cap = Self::capacity(max_window);
        if self.on
            && self.len == 0
            && history.capacity() >= cap + KEY_SLACK
            && history.len() + fill > cap
        {
            self.wrap(history, history_start, uncommitted);
        }
        if self.len != 0 {
            // A write over the ext segment's live bytes would corrupt the
            // window; the capacity's slack rules it out.
            assert!(
                history.len() + fill <= self.off,
                "ring prefix would overwrite the live ext segment",
            );
        } else {
            history.reserve(fill);
        }
    }

    /// Starts the prefix over at the buffer's front: the committed window
    /// stays where it lies as the ext segment and only the input read but
    /// not yet claimed moves (upstream `ZSTD_window_update` on a
    /// non-contiguous segment). The window is far shorter than the prefix
    /// then, so no older ext segment is live.
    fn wrap(&mut self, history: &mut HistoryBuf, history_start: &mut usize, uncommitted: usize) {
        debug_assert_eq!(self.len, 0, "a live ext segment at the next wrap");
        let committed_end = history.len() - uncommitted;
        self.off = *history_start;
        self.len = committed_end - *history_start;
        history.wrap_tail(committed_end);
        *history_start = 0;
        // The moved tail's bytes still lie right after the ext segment.
        self.mirrored = uncommitted.min(KEY_SLACK);
    }

    /// Keeps the prefix's first bytes, up to [`KEY_SLACK`], right after the
    /// ext segment, so a key read at one of its last positions runs on into
    /// the bytes that follow them in the window. The prefix never reaches
    /// that far while the ext segment lives.
    pub(crate) fn after_fill(&mut self, history: &mut HistoryBuf) {
        if self.len == 0 || self.mirrored == KEY_SLACK {
            return;
        }
        let upto = history.len().min(KEY_SLACK);
        if upto <= self.mirrored {
            return;
        }
        let mut head = [0u8; KEY_SLACK];
        let n = upto - self.mirrored;
        head[..n].copy_from_slice(&history[self.mirrored..upto]);
        history.write_room(self.off + self.len + self.mirrored, &head[..n]);
        self.mirrored = upto;
    }

    /// Drops the window's oldest `len` bytes: the ext segment's first, then
    /// the prefix's (advancing `history_start`).
    #[inline]
    pub(crate) fn evict(&mut self, len: usize, history_start: &mut usize) {
        let from_ext = len.min(self.len);
        self.off += from_ext;
        self.len -= from_ext;
        *history_start += len - from_ext;
    }

    /// Puts the ext segment back in front of the prefix, for a path that
    /// hands the window out as one slice.
    pub(crate) fn linearize(&mut self, history: &mut HistoryBuf, history_start: usize) {
        debug_assert_eq!(
            history_start, 0,
            "the prefix starts at the front while an ext segment lives"
        );
        history.prepend_from_room(self.off, self.len);
        self.len = 0;
    }

    /// The ext segment's live bytes.
    pub(crate) fn ext_bytes<'a>(&self, history: &'a HistoryBuf) -> &'a [u8] {
        &history.room()[self.off..self.off + self.len]
    }

    /// The window as its two segments; `history_start` and
    /// `history_abs_start` as the owning history keeps them, `uncommitted`
    /// its bytes read ahead.
    pub(crate) fn segments(
        &self,
        history: &HistoryBuf,
        history_start: usize,
        history_abs_start: usize,
        uncommitted: usize,
    ) -> WindowSegments {
        // Both pointers from the whole written room: an ext key read runs past
        // the segment into the mirror after it.
        let room = history.room().as_ptr();
        let committed = history.len() - uncommitted;
        let prefix_abs = history_abs_start + self.len;
        WindowSegments {
            ext: room.wrapping_add(self.off),
            ext_abs: history_abs_start,
            prefix: room.wrapping_add(history_start),
            prefix_abs,
            end_abs: prefix_abs + (committed - history_start),
        }
    }
}

/// The owned window as the paths that read a candidate by its position see
/// it: the ext segment (empty while the window is one segment), then the
/// prefix, which holds the block (upstream `dictBase` / `base`).
///
/// Raw pointers from the history's whole room, not slices of each segment:
/// a key read at one of the ext segment's last positions runs past the
/// segment's end into the room after it.
#[derive(Clone, Copy)]
pub(crate) struct WindowSegments {
    /// The ext segment's first byte. Up to [`KEY_SLACK`] bytes after its last
    /// are readable and equal the prefix's first bytes.
    pub(crate) ext: *const u8,
    /// Position of the ext segment's first byte, the window's floor.
    pub(crate) ext_abs: usize,
    /// The prefix's first committed byte.
    pub(crate) prefix: *const u8,
    /// Position of the prefix's first byte, right after the ext segment's last.
    pub(crate) prefix_abs: usize,
    /// One past the prefix's last committed position.
    pub(crate) end_abs: usize,
}

impl WindowSegments {
    /// Pointer to the byte at `abs`, a position of the window or its end. At
    /// an ext position, up to [`KEY_SLACK`] bytes past the segment's end are
    /// readable through it.
    #[inline(always)]
    pub(crate) fn ptr(&self, abs: usize) -> *const u8 {
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
    pub(crate) fn byte(&self, abs: usize) -> u8 {
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
    pub(crate) unsafe fn count_forward<K: EncodeTier>(
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

    /// Walk the match of `cand` against `cur` back over at most `lit_len`
    /// bytes, never before the window's floor; `start` of the result is the
    /// cursor's position.
    pub(crate) fn extend_back(
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
