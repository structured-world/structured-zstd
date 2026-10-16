//! Long-distance matches searched ahead of the blocks that consume them.
//!
//! The search runs over whatever input is known, which can reach past the
//! block about to be compressed: the block splitter asks where long matches
//! lie before it cuts. Each block then takes the matches covering it, the one
//! crossing its end cut there and carried into the next block, as upstream
//! carries a raw sequence store across blocks (`maybeSplitSequence` and
//! `ZSTD_ldm_skipSequences`, `zstd_ldm.c:607-666`).

use alloc::vec::Vec;

use crate::encoding::opt::ldm::HcRawSeq;

/// One match found by the search, in stream positions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct LongMatch {
    start: usize,
    end: usize,
    offset: usize,
}

#[derive(Default)]
pub(crate) struct LdmSequenceStore {
    /// Found and not yet taken past, in stream order.
    matches: Vec<LongMatch>,
    /// Index of the first match a block has not taken to its end.
    pos: usize,
    /// Stream position the next block starts at.
    taken_end: usize,
    /// Stream position the input is searched up to.
    searched_end: usize,
}

impl LdmSequenceStore {
    /// Forget everything, with input from `at` on unsearched and untaken.
    pub(crate) fn reset(&mut self, at: usize) {
        self.matches.clear();
        self.pos = 0;
        self.taken_end = at;
        self.searched_end = at;
    }

    /// Stream position the input is searched up to.
    pub(crate) fn searched_end(&self) -> usize {
        self.searched_end
    }

    /// Stream position the next block starts at.
    pub(crate) fn taken_end(&self) -> usize {
        self.taken_end
    }

    /// Heap bytes the matches hold.
    pub(crate) fn heap_size(&self) -> usize {
        self.matches.capacity() * core::mem::size_of::<LongMatch>()
    }

    /// Record the sequences a search of `[searched_end, to)` found, literal
    /// lengths counted from the start of that range.
    pub(crate) fn searched(&mut self, found: &[HcRawSeq], to: usize) {
        let mut at = self.searched_end;
        for seq in found {
            let start = at + seq.lit_length;
            let end = start + seq.match_length;
            self.matches.push(LongMatch {
                start,
                end,
                offset: seq.offset,
            });
            at = end;
        }
        debug_assert!(at <= to, "a sequence reaches past the searched range");
        self.searched_end = to;
    }

    /// `cut` bytes past the next block's start, moved out of any long match it
    /// falls inside: to the match end, or to `window` when the match runs past
    /// it. Cutting inside one buys a block whose first sequence repeats the
    /// offset the previous one ended on.
    pub(crate) fn cut_outside_matches(&self, window: usize, cut: usize) -> usize {
        let cut_at = self.taken_end + cut;
        for found in &self.matches[self.pos..] {
            if found.start >= cut_at {
                break;
            }
            if cut_at < found.end {
                return (found.end - self.taken_end).min(window);
            }
        }
        cut
    }

    /// Take the matches of the next `len` bytes into `out` as sequences,
    /// literal lengths counted from the block start. A match crossing the
    /// block end is cut there, the rest carried into the next block; a piece
    /// shorter than `min_match`, on either side of the cut, is left as
    /// literals.
    pub(crate) fn take(&mut self, len: usize, min_match: usize, out: &mut Vec<HcRawSeq>) {
        out.clear();
        let block_end = self.taken_end + len;
        let mut at = self.taken_end;
        while let Some(&found) = self.matches.get(self.pos) {
            if found.start >= block_end {
                break;
            }
            let start = found.start.max(self.taken_end);
            let end = found.end.min(block_end);
            if end - start >= min_match {
                out.push(HcRawSeq {
                    lit_length: start - at,
                    offset: found.offset,
                    match_length: end - start,
                });
                at = end;
            }
            if found.end > block_end {
                break;
            }
            self.pos += 1;
        }
        self.taken_end = block_end;
        // Taken matches go once they are the bulk of the vector, so a search
        // far ahead of a long stream holds only what is still to come.
        if self.pos > self.matches.len() / 2 {
            self.matches.drain(..self.pos);
            self.pos = 0;
        }
    }
}

#[cfg(test)]
mod tests;
