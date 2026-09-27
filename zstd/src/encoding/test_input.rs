//! Input bookkeeping for the test matchers, which search nothing: the bytes
//! read in, the block last committed, and what is still uncommitted.

use super::HistoryBuf;

#[derive(Default)]
pub(crate) struct TestInput {
    history: HistoryBuf,
    /// End of the last committed block, which starts the history.
    committed: usize,
}

impl TestInput {
    /// [`Matcher::fill_in_place`](super::Matcher::fill_in_place).
    pub(crate) fn fill(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        self.history.reserve(capacity);
        fill(&mut self.history)
    }

    /// [`Matcher::uncommitted_input`](super::Matcher::uncommitted_input).
    pub(crate) fn uncommitted(&self) -> &[u8] {
        &self.history[self.committed..]
    }

    /// [`Matcher::commit_filled`](super::Matcher::commit_filled): the block
    /// replaces the one before it, since nothing matches against history.
    pub(crate) fn commit(&mut self, len: usize) {
        self.history.drain_front(self.committed);
        assert!(len <= self.history.len(), "committing past the input read");
        self.committed = len;
    }

    /// [`Matcher::get_last_space`](super::Matcher::get_last_space).
    pub(crate) fn last_block(&self) -> &[u8] {
        &self.history[..self.committed]
    }

    /// Drops everything, for a matcher's reset.
    pub(crate) fn clear(&mut self) {
        self.history.clear();
        self.committed = 0;
    }
}
