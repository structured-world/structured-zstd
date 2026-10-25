//! Shared helpers for matcher tests.

use alloc::vec::Vec;

use super::{CompressionLevel, HistoryBuf, MatchGeneratorDriver, Matcher, Sequence};

/// The built-in driver reached only through [`Matcher`], as a caller wrapping
/// it does: the encoder sees no built-in driver, so every block runs through
/// the trait entry points under the driver's own tier.
pub(crate) struct TraitDriver(pub(crate) MatchGeneratorDriver);

impl TraitDriver {
    /// A driver whose trait entry points run under `tier`.
    pub(crate) fn new(tier: super::fastpath::FastpathKernel) -> Self {
        let mut driver = MatchGeneratorDriver::new(crate::common::MAX_BLOCK_SIZE as usize, 1);
        driver.set_tier(tier);
        Self(driver)
    }
}

impl Matcher for TraitDriver {
    fn get_last_space(&mut self) -> &[u8] {
        self.0.get_last_space()
    }

    fn fill_in_place(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        self.0.fill_in_place(capacity, fill)
    }

    fn uncommitted_input(&self) -> &[u8] {
        self.0.uncommitted_input()
    }

    fn commit_filled(&mut self, len: usize) {
        self.0.commit_filled(len);
    }

    fn skip_matching(&mut self) {
        self.0.skip_matching();
    }

    fn skip_matching_with_hint(&mut self, incompressible_hint: Option<bool>) {
        self.0.skip_matching_with_hint(incompressible_hint);
    }

    fn start_matching(&mut self, handle_sequence: impl FnMut(Sequence)) {
        self.0.start_matching(handle_sequence);
    }

    fn reset(&mut self, level: CompressionLevel) {
        self.0.reset(level);
    }

    fn set_source_size_hint(&mut self, size: u64) {
        self.0.set_source_size_hint(size);
    }

    fn set_dictionary_size_hint(&mut self, sizes: super::DictionarySizes) {
        self.0.set_dictionary_size_hint(sizes);
    }

    fn prime_with_dictionary(&mut self, dict_content: &[u8], offset_hist: [u32; 3]) {
        self.0.prime_with_dictionary(dict_content, offset_hist);
    }

    fn seed_dictionary_entropy(
        &mut self,
        huff: Option<&crate::huff0::huff0_encoder::HuffmanTable>,
        ll: Option<&crate::fse::fse_encoder::FSETable>,
        ml: Option<&crate::fse::fse_encoder::FSETable>,
        of: Option<&crate::fse::fse_encoder::FSETable>,
    ) {
        self.0.seed_dictionary_entropy(huff, ll, ml, of);
    }

    fn supports_dictionary_priming(&self) -> bool {
        self.0.supports_dictionary_priming()
    }

    fn settle_block_cut(&mut self, window: usize, cut: usize) -> usize {
        self.0.settle_block_cut(window, cut)
    }

    fn window_size(&self) -> u64 {
        self.0.window_size()
    }
}

/// Levels across every backend: the negative and positive Fast band, dfast,
/// the row lazy band, btlazy2 and the optimal parsers.
pub(crate) const TIER_LEVELS: [i32; 7] = [-5, 1, 3, 6, 13, 16, 19];

/// Input that takes each block path: full blocks of log lines, so the frame
/// saves enough for the pre-splitter, then a block that turns to noise halfway
/// (a pre-split cut), a block of one byte repeated (a block written without
/// a search) and a short tail of text.
pub(crate) fn tier_fixture() -> Vec<u8> {
    const BLOCK: usize = crate::common::MAX_BLOCK_SIZE as usize;
    let line = b"ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" table=orders\n";
    let mut data = line.repeat((2 * BLOCK + BLOCK / 2) / line.len() + 1);
    data.truncate(2 * BLOCK + BLOCK / 2);
    let mut state = 0x2545_F491_u32;
    data.extend((0..BLOCK / 2).map(|_| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as u8
    }));
    data.extend(core::iter::repeat_n(b'z', BLOCK));
    data.extend_from_slice(&line.repeat(40));
    data
}

/// A raw-content dictionary of the fixture's log lines.
pub(crate) fn tier_dictionary() -> crate::decoding::Dictionary {
    let content = b"level=INFO msg=\"rotate segment\" table=orders region=eu-west\n".repeat(64);
    crate::decoding::Dictionary::from_raw_content(0xD1C7_7135, content).unwrap()
}

/// Rebuilds a block from its sequences the way a decoder does: each literal
/// run is taken from the block at the position the sequences before it reach,
/// each match is copied from what has been rebuilt so far.
pub(crate) struct BlockReplay<'a> {
    block: &'a [u8],
    pos: usize,
}

impl<'a> BlockReplay<'a> {
    /// A replay of `block`, the bytes the matcher was given for it.
    pub(crate) fn new(block: &'a [u8]) -> Self {
        Self { block, pos: 0 }
    }

    /// Append what `seq` decodes to onto `out`, which holds everything
    /// rebuilt before it (earlier blocks included).
    pub(crate) fn apply(&mut self, out: &mut Vec<u8>, seq: Sequence) {
        match seq {
            Sequence::Literals { len } => {
                out.extend_from_slice(&self.block[self.pos..self.pos + len]);
                self.pos += len;
            }
            Sequence::Triple {
                literal_len,
                offset,
                match_len,
            } => {
                out.extend_from_slice(&self.block[self.pos..self.pos + literal_len]);
                let start = out.len() - offset;
                for i in 0..match_len {
                    let byte = out[start + i];
                    out.push(byte);
                }
                self.pos += literal_len + match_len;
            }
        }
    }

    /// Literal bytes `seq` carries, read from the block at the replay's
    /// position; advances past `seq` as [`Self::apply`] does.
    pub(crate) fn literals(&mut self, seq: Sequence) -> &'a [u8] {
        let (literal_len, match_len) = match seq {
            Sequence::Literals { len } => (len, 0),
            Sequence::Triple {
                literal_len,
                match_len,
                ..
            } => (literal_len, match_len),
        };
        let literals = &self.block[self.pos..self.pos + literal_len];
        self.pos += literal_len + match_len;
        literals
    }
}
