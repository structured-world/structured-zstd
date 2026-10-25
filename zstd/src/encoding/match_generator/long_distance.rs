//! Long-distance matching in the [`MatchGeneratorDriver`]: the search ahead of
//! the blocks, the matches each block takes, and the block matched around them
//! below the optimal parser.
//!
//! A child module so it reaches the driver's private long-distance state and
//! storage directly.

use super::{MatchGeneratorDriver, Sequence, owned_sink};
use crate::encoding::fastpath::EncodeTier;

impl MatchGeneratorDriver {
    /// Search the input from where the last search stopped up to `to`, a
    /// stream position at most the end of the input read so far.
    ///
    /// What a match may reach is fixed against the END of the range, as
    /// upstream enforces the distance once per chunk before generating
    /// (`ZSTD_window_enforceMaxDist` on `chunkEnd`, `zstd_ldm.c:585`): one
    /// window back from it, or the whole dictionary while the window has not
    /// yet passed the dictionary's end. Every match then holds an offset valid
    /// at its last byte, wherever in the range it lies.
    fn search_long_distance_to<K: EncodeTier>(&mut self, to: usize) {
        let Some(producer) = self.ldm.as_mut() else {
            return;
        };
        let from = self.ldm_store.searched_end();
        if from >= to {
            return;
        }
        let window = self.reported_window_size;
        let reach_floor = if self.ldm_dict_end != 0 && to <= window + self.ldm_dict_end {
            0
        } else {
            to.saturating_sub(window)
        };
        let live_start = self.ldm_committed_end - self.storage.committed_window().len();
        let live = self.storage.window_with_pending();
        let floor = reach_floor.max(live_start);
        debug_assert!(floor <= from, "a search range wider than the window");
        self.ldm_found.clear();
        producer.generate_into::<K>(
            &live[floor - live_start..],
            floor,
            from,
            to,
            &mut self.ldm_found,
        );
        self.ldm_store.searched(&self.ldm_found, to);
    }

    /// Line the matches up with the next block starting at `at`: anything
    /// kept for another position (a prime, a reset, a skipped search) is gone.
    fn align_long_distance_store(&mut self, at: usize) {
        if self.ldm_store.taken_end() != at {
            self.ldm_store.reset(at);
        }
    }

    /// The matches of the last committed block, into [`Self::ldm_block`],
    /// searched first as far as the block reaches. Returns the block's length.
    pub(super) fn take_long_distance_block<K: EncodeTier>(&mut self) -> usize {
        let block_len = self.storage.last_block_len();
        let block_end = self.ldm_committed_end;
        self.align_long_distance_store(block_end - block_len);
        self.search_long_distance_to::<K>(block_end);
        let min_match = self.reset_shape.map_or(
            crate::encoding::cost_model::HC_FORMAT_MINMATCH,
            |(params, ..)| params.min_match(),
        );
        self.ldm_store
            .take(block_len, min_match, &mut self.ldm_block);
        block_len
    }

    /// See [`crate::encoding::Matcher::settle_block_cut`]: the next `window`
    /// uncommitted bytes are searched now, and the cut moved out of any long
    /// match.
    pub(super) fn settle_long_distance_cut<K: EncodeTier>(
        &mut self,
        window: usize,
        cut: usize,
    ) -> usize {
        if self.ldm.is_none() {
            return cut;
        }
        let start = self.ldm_committed_end;
        self.align_long_distance_store(start);
        self.search_long_distance_to::<K>(start + window);
        self.ldm_store.cut_outside_matches(window, cut)
    }

    /// Take the block's long-distance matches outright and match only the
    /// literals between them with the frame's backend (upstream
    /// `ZSTD_ldm_blockCompress` below btopt), or with the installed producer
    /// when `produced`. The block is handed back to the uncommitted input and
    /// committed again in pieces, so each gap is the backend's whole block and
    /// each match body is indexed without being searched. Returns `false` when
    /// the block has no long-distance match, leaving it committed for the
    /// ordinary search. With `produced`, the caller has emptied the optimal
    /// parser's candidates first, so a piece the producer fails on is never
    /// matched against an earlier block's long matches.
    pub(super) fn start_matching_long_distance<K: EncodeTier>(
        &mut self,
        handle_sequence: &mut impl FnMut(Sequence),
        produced: bool,
    ) -> bool {
        let block_len = self.take_long_distance_block::<K>();
        if self.ldm_block.is_empty() {
            return false;
        }
        let sequences = core::mem::take(&mut self.ldm_block);
        self.storage.uncommit_block(block_len);
        let mut consumed = 0usize;
        for seq in &sequences {
            // The gap's trailing literals become the long match's own.
            let mut gap = Some(0usize);
            if seq.lit_length != 0 {
                self.storage.commit_piece(seq.lit_length);
                self.match_gap::<K>(produced, &mut owned_sink(handle_sequence, &mut gap));
            }
            let gap_literals = gap.expect("the gap sink only ever writes Some");
            self.storage.commit_piece(seq.match_length);
            self.storage.skip_long_match();
            handle_sequence(Sequence::Triple {
                literal_len: gap_literals,
                offset: seq.offset,
                match_len: seq.match_length,
            });
            self.storage.note_long_match(seq.offset, gap_literals);
            consumed += seq.lit_length + seq.match_length;
        }
        let tail = block_len - consumed;
        if tail != 0 {
            self.storage.commit_piece(tail);
            let mut forward = None;
            self.match_gap::<K>(produced, &mut owned_sink(handle_sequence, &mut forward));
        }
        self.ldm_block = sequences;
        self.ldm_split_block = Some(block_len);
        true
    }

    /// Match the piece just committed between long matches: by the producer
    /// when `produced` and it gives a parse, by the backend otherwise.
    fn match_gap<K: EncodeTier>(&mut self, produced: bool, sink: &mut impl FnMut(Sequence)) {
        if produced && self.match_produced::<K>(sink) {
            return;
        }
        self.start_matching_owned::<K>(sink);
    }
}
