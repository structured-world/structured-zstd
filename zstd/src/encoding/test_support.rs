//! Shared helpers for matcher tests.

use alloc::vec::Vec;

use super::Sequence;

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
