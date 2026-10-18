//! Sequences that come into the encoder from outside, or go out of it: a parse
//! of the input into literal runs and matches, laid out as the C
//! `ZSTD_Sequence` so the C entry points pass caller arrays straight through.
//!
//! * [`SequenceReplay`] is a [`Matcher`] that searches nothing and reports a
//!   given parse instead, so a frame is built from it by the ordinary block
//!   pipeline (C `ZSTD_compressSequences`).
//! * [`SequenceProducer`] is a parser asked for each block in place of the
//!   built-in match finder (C `ZSTD_registerSequenceProducer`).
//! * [`FrameCompressor::set_sequence_capture`](crate::encoding::FrameCompressor::set_sequence_capture)
//!   records the parse the built-in match finder chose (C
//!   `ZSTD_generateSequences`).

use alloc::vec::Vec;

use crate::encoding::block_input::BlockInput;
use crate::encoding::{CompressionLevel, HistoryBuf, Matcher, Sequence};

/// The shortest match a sequence can carry (RFC 8878 3.1.1.3.2.1.1: match
/// length codes start at 3).
pub const MIN_MATCH: u32 = 3;

/// One sequence of a parse: `lit_len` literal bytes, then `match_len` bytes
/// copied from `offset` bytes back. A sequence with `offset == 0` and
/// `match_len == 0` carries only literals and ends a block (a block
/// delimiter). Laid out as the C `ZSTD_Sequence`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExternalSequence {
    /// How far back the match copies from, in bytes (not the offset code).
    pub offset: u32,
    /// Literal bytes before the match.
    pub lit_len: u32,
    /// Bytes the match copies.
    pub match_len: u32,
    /// Which repeat offset `offset` is (1..=3, numbered as RFC 8878 3.1.1.5
    /// numbers them), or 0 when it is none. A captured parse fills it in; the
    /// encoder works it out itself and never reads it.
    pub rep: u32,
}

impl ExternalSequence {
    /// A block delimiter carrying `lit_len` trailing literals.
    pub const fn delimiter(lit_len: u32) -> Self {
        Self {
            offset: 0,
            lit_len,
            match_len: 0,
            rep: 0,
        }
    }

    /// Whether this sequence only ends a block, with its literals.
    pub const fn is_delimiter(&self) -> bool {
        self.offset == 0 && self.match_len == 0
    }
}

/// Upper bound on the sequences a parse of `src_size` bytes can hold, block
/// delimiters included (C `ZSTD_sequenceBound`): one per minimal match, and
/// one delimiter per smallest block.
///
/// ```
/// use structured_zstd::encoding::sequences::sequence_bound;
/// assert_eq!(sequence_bound(0), 2);
/// assert_eq!(sequence_bound(3000), 1004);
/// ```
pub const fn sequence_bound(src_size: usize) -> usize {
    (src_size / MIN_MATCH as usize + 1)
        + (src_size / crate::common::MIN_MAX_BLOCK_SIZE as usize + 1)
}

/// Heap bytes a compressor keeps for a [`SequenceProducer`]'s parse of blocks
/// up to `block_size` bytes: room for [`sequence_bound`] sequences, as many as
/// a parse it can use holds, kept from block to block (upstream reserves the
/// same for its external sequences, zstd_compress.c:2236-2239).
///
/// ```
/// use structured_zstd::encoding::sequences::{producer_parse_heap_bytes, sequence_bound};
/// assert_eq!(producer_parse_heap_bytes(131_072), sequence_bound(131_072) * 16);
/// ```
pub const fn producer_parse_heap_bytes(block_size: usize) -> usize {
    sequence_bound(block_size) * core::mem::size_of::<ExternalSequence>()
}

/// Remove every block delimiter from `sequences`, handing its literals to the
/// sequence after it, and return how many sequences remain at the front (C
/// `ZSTD_mergeBlockDelimiters`). A final delimiter's literals become the
/// parse's trailing literals, which no sequence carries.
///
/// ```
/// use structured_zstd::encoding::sequences::{merge_block_delimiters, ExternalSequence};
/// let mut parse = [
///     ExternalSequence { offset: 4, lit_len: 2, match_len: 5, rep: 0 },
///     ExternalSequence { offset: 0, lit_len: 3, match_len: 0, rep: 0 },
///     ExternalSequence { offset: 9, lit_len: 1, match_len: 4, rep: 0 },
/// ];
/// assert_eq!(merge_block_delimiters(&mut parse), 2);
/// assert_eq!(parse[1].lit_len, 4);
/// ```
pub fn merge_block_delimiters(sequences: &mut [ExternalSequence]) -> usize {
    let mut kept = 0;
    for index in 0..sequences.len() {
        let sequence = sequences[index];
        if sequence.is_delimiter() {
            if let Some(next) = sequences.get_mut(index + 1) {
                // Wrapping, as upstream adds them: the C entry point has no
                // error to return, a sum past `u32::MAX` describes no block,
                // and `validate_parse` refuses such a parse later.
                next.lit_len = next.lit_len.wrapping_add(sequence.lit_len);
            }
        } else {
            sequences[kept] = sequence;
            kept += 1;
        }
    }
    kept
}

/// The plain offset an offset code stands for, given the repeat-offset
/// history before it, which it advances (RFC 8878 3.1.1.5: with no literals
/// before the match the repeat codes shift by one, and the third is the
/// first repeat offset less one).
pub(crate) fn plain_offset(code: u32, lit_len: u32, history: &mut [u32; 3]) -> u32 {
    if code > 3 {
        let offset = code - 3;
        history[2] = history[1];
        history[1] = history[0];
        history[0] = offset;
        return offset;
    }
    let index = if lit_len == 0 { code } else { code - 1 };
    let offset = match index {
        0 => history[0],
        1 => history[1],
        2 => history[2],
        // Wrapping as the decoder reads it: a history of 1 makes this 0,
        // which a valid stream never asks for.
        _ => history[0].wrapping_sub(1),
    };
    if index > 0 {
        if index > 1 {
            history[2] = history[1];
        }
        history[1] = history[0];
        history[0] = offset;
    }
    offset
}

/// Why a parse cannot build a valid frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SequenceError {
    /// The parse covers more bytes than the input holds, or, with explicit
    /// block delimiters, not exactly the input.
    LengthMismatch,
    /// Sequence `index` has an offset but matches fewer than [`MIN_MATCH`]
    /// bytes; a run of literals alone is a delimiter, with offset 0.
    MatchTooShort {
        /// Index of the sequence.
        index: usize,
    },
    /// Sequence `index` copies from further back than the output and the
    /// dictionary reach, or than the window allows.
    OffsetTooFar {
        /// Index of the sequence.
        index: usize,
    },
    /// Sequence `index` has a match but no offset.
    ZeroOffset {
        /// Index of the sequence.
        index: usize,
    },
    /// A block between two delimiters is larger than `max_block`.
    BlockTooLarge,
    /// Sequence `index` copies bytes that differ from the ones the input has
    /// where the copy lands, so the frame would decode to something else.
    MatchMismatch {
        /// Index of the sequence.
        index: usize,
    },
}

impl core::fmt::Display for SequenceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LengthMismatch => f.write_str("the sequences do not cover the input"),
            Self::MatchTooShort { index } => write!(f, "sequence {index} matches too few bytes"),
            Self::OffsetTooFar { index } => write!(f, "sequence {index} copies from too far back"),
            Self::ZeroOffset { index } => write!(f, "sequence {index} matches at offset 0"),
            Self::BlockTooLarge => f.write_str("a block is larger than the block size"),
            Self::MatchMismatch { index } => {
                write!(f, "sequence {index} copies bytes the input does not hold")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SequenceError {}

/// Check that `sequences` is a parse of `src_len` bytes a decoder can follow:
/// every match at least [`MIN_MATCH`] bytes (literals with no match are a
/// delimiter, offset 0), every
/// offset within what is decoded so far plus `dict_len` bytes of dictionary
/// while that fits the window, and within `window` after. With
/// `explicit_delimiters` the parse must cover the input exactly and each
/// delimited block hold at most `max_block` bytes; without, bytes past the
/// parse are trailing literals.
///
/// ```
/// use structured_zstd::encoding::sequences::{validate_parse, ExternalSequence, SequenceError};
/// let parse = [ExternalSequence { offset: 3, lit_len: 3, match_len: 6, rep: 0 }];
/// assert_eq!(validate_parse(&parse, 12, 1 << 20, 0, false, 1 << 17), Ok(()));
/// let reaching = [ExternalSequence { offset: 9, lit_len: 3, match_len: 6, rep: 0 }];
/// assert_eq!(
///     validate_parse(&reaching, 12, 1 << 20, 0, false, 1 << 17),
///     Err(SequenceError::OffsetTooFar { index: 0 })
/// );
/// ```
pub fn validate_parse(
    sequences: &[ExternalSequence],
    src_len: usize,
    window: u64,
    dict_len: usize,
    explicit_delimiters: bool,
    max_block: usize,
) -> Result<(), SequenceError> {
    let mut position: u64 = 0;
    let mut block_start: u64 = 0;
    for (index, sequence) in sequences.iter().enumerate() {
        position += u64::from(sequence.lit_len);
        if sequence.is_delimiter() {
            if explicit_delimiters {
                if position - block_start > max_block as u64 {
                    return Err(SequenceError::BlockTooLarge);
                }
                block_start = position;
            }
            continue;
        }
        if sequence.offset == 0 {
            return Err(SequenceError::ZeroOffset { index });
        }
        // A run of literals that ends the parse is a delimiter, handled above:
        // with an offset it would be a match of no length (zstd.h,
        // `ZSTD_Sequence`: offset 0 when matchLength is 0).
        if sequence.match_len < MIN_MATCH {
            return Err(SequenceError::MatchTooShort { index });
        }
        // RFC 8878 3.1.1.1.2 / 3.1.1.5: while the output is within the window
        // a match may reach into the dictionary; past it, only the window.
        let reach = if position > window {
            window
        } else {
            position + dict_len as u64
        };
        if u64::from(sequence.offset) > reach {
            return Err(SequenceError::OffsetTooFar { index });
        }
        position += u64::from(sequence.match_len);
        if position > src_len as u64 {
            return Err(SequenceError::LengthMismatch);
        }
    }
    let covered = if explicit_delimiters {
        position == src_len as u64
    } else {
        position <= src_len as u64
    };
    if !covered {
        return Err(SequenceError::LengthMismatch);
    }
    Ok(())
}

/// Check that `sequences` decodes to `src`: every match copies the bytes the
/// input holds where it lands, out of `src` itself or, reaching before the
/// start, out of the tail of `dict`. A parse that [`validate_parse`] accepts
/// can still describe different bytes; compressing it would make a frame that
/// decodes to them, not to `src`.
///
/// The decoder has rebuilt the input up to each match, so the input stands
/// for the output it copies from, overlapping copies included.
///
/// # Errors
/// [`SequenceError::MatchMismatch`] for the first match that copies other
/// bytes, [`SequenceError::LengthMismatch`] or [`SequenceError::OffsetTooFar`]
/// for a parse [`validate_parse`] would also refuse.
///
/// ```
/// use structured_zstd::encoding::sequences::{parse_reproduces, ExternalSequence, SequenceError};
/// let parse = [ExternalSequence { offset: 3, lit_len: 3, match_len: 3, rep: 0 }];
/// assert_eq!(parse_reproduces(&parse, b"abcabc", &[]), Ok(()));
/// assert_eq!(
///     parse_reproduces(&parse, b"abcabd", &[]),
///     Err(SequenceError::MatchMismatch { index: 0 })
/// );
/// ```
pub fn parse_reproduces(
    sequences: &[ExternalSequence],
    src: &[u8],
    dict: &[u8],
) -> Result<(), SequenceError> {
    let mut position = 0usize;
    for (index, sequence) in sequences.iter().enumerate() {
        position = position
            .checked_add(sequence.lit_len as usize)
            .ok_or(SequenceError::LengthMismatch)?;
        if sequence.is_delimiter() {
            continue;
        }
        let offset = sequence.offset as usize;
        let len = sequence.match_len as usize;
        let end = position
            .checked_add(len)
            .filter(|&end| end <= src.len())
            .ok_or(SequenceError::LengthMismatch)?;
        let target = &src[position..end];
        // The bytes copied from before the input come from the dictionary's
        // tail; the rest from the input.
        let from_dict = if offset > position {
            (offset - position).min(len)
        } else {
            0
        };
        if from_dict > 0 {
            let start = dict
                .len()
                .checked_sub(offset - position)
                .ok_or(SequenceError::OffsetTooFar { index })?;
            if dict[start..start + from_dict] != target[..from_dict] {
                return Err(SequenceError::MatchMismatch { index });
            }
        }
        if len > from_dict {
            // `offset <= position + from_dict` here: a dictionary part ends
            // exactly at the start of the input.
            let from = position + from_dict - offset;
            if src[from..from + (len - from_dict)] != target[from_dict..] {
                return Err(SequenceError::MatchMismatch { index });
            }
        }
        position = end;
    }
    Ok(())
}

/// The input a parse decodes to, rebuilt from its literals as it is read, as
/// a decoder would (C `ZSTD_compressSequencesAndLiterals` takes the literals
/// rather than the input). Matches reaching before the start read from
/// `dict`; literals past the last sequence are the trailing ones. Only the
/// last `window` bytes are held for the matches to copy from (more only
/// while a match reaching further, into the dictionary, is being copied), so
/// a frame is built from it without the whole input in memory.
///
/// ```
/// use structured_zstd::encoding::sequences::{ExternalSequence, RebuiltSource};
/// use std::io::Read;
/// let parse = [
///     ExternalSequence { offset: 3, lit_len: 3, match_len: 6, rep: 0 },
///     ExternalSequence { offset: 0, lit_len: 1, match_len: 0, rep: 0 },
/// ];
/// let mut source = RebuiltSource::new(&parse, b"abc!", &[], 1 << 10).unwrap();
/// assert_eq!(source.len(), 10);
/// let mut out = Vec::new();
/// source.read_to_end(&mut out).unwrap();
/// assert_eq!(out, b"abcabcabc!");
/// ```
pub struct RebuiltSource<'a> {
    sequences: core::slice::Iter<'a, ExternalSequence>,
    literals: &'a [u8],
    dict: &'a [u8],
    window: usize,
    /// Literals handed out so far.
    lit_at: usize,
    /// Literals of the sequence in hand still to come.
    lit_left: usize,
    /// Bytes of the match in hand still to come, and its offset.
    match_left: usize,
    offset: usize,
    /// Bytes rebuilt so far.
    produced: usize,
    /// The bytes most recently rebuilt: at least the last `window` of them,
    /// and as far back as the match in hand reaches.
    history: Vec<u8>,
    len: usize,
}

impl<'a> RebuiltSource<'a> {
    /// Check that `sequences` and `literals` rebuild to a whole input with
    /// every match within the output so far plus `dict` while the output is
    /// within `window`, and within `window` after (as [`validate_parse`]
    /// checks), and set up to read it.
    ///
    /// # Errors
    /// [`SequenceError::LengthMismatch`] when the sequences need more literals
    /// than there are, [`SequenceError::OffsetTooFar`] when a match reaches
    /// further back than that, [`SequenceError::ZeroOffset`] for a match at
    /// offset 0.
    pub fn new(
        sequences: &'a [ExternalSequence],
        literals: &'a [u8],
        dict: &'a [u8],
        window: usize,
    ) -> Result<Self, SequenceError> {
        let mut position = 0usize;
        let mut lits = 0usize;
        for (index, sequence) in sequences.iter().enumerate() {
            let lit_len = sequence.lit_len as usize;
            lits = lits
                .checked_add(lit_len)
                .filter(|lits| *lits <= literals.len())
                .ok_or(SequenceError::LengthMismatch)?;
            // Within the literals, so within `usize`.
            position += lit_len;
            if sequence.match_len == 0 {
                continue;
            }
            let offset = sequence.offset as usize;
            if offset == 0 {
                return Err(SequenceError::ZeroOffset { index });
            }
            // RFC 8878 3.1.1.1.2 / 3.1.1.5, as `validate_parse` reads them:
            // while the output is within the window a match may reach into
            // the dictionary; past it, only the window. A reach past `usize`
            // holds any offset.
            let past_reach = if position > window {
                offset > window
            } else {
                position
                    .checked_add(dict.len())
                    .is_some_and(|reach| offset > reach)
            };
            if past_reach {
                return Err(SequenceError::OffsetTooFar { index });
            }
            position = position
                .checked_add(sequence.match_len as usize)
                .ok_or(SequenceError::LengthMismatch)?;
        }
        let len = position
            .checked_add(literals.len() - lits)
            .ok_or(SequenceError::LengthMismatch)?;
        Ok(Self {
            sequences: sequences.iter(),
            literals,
            dict,
            window,
            lit_at: 0,
            lit_left: 0,
            match_left: 0,
            offset: 0,
            produced: 0,
            history: Vec::new(),
            len,
        })
    }

    /// Bytes the source rebuilds in all.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the source rebuilds nothing.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Rebuild up to `want` bytes of the match in hand onto `history`.
    fn copy_match(&mut self, want: usize) {
        let mut left = want;
        while left > 0 {
            let take = if self.offset > self.produced {
                // The match starts in the dictionary and runs on into the
                // output; `new` checked the dictionary reaches that far.
                let back = self.offset - self.produced;
                let from = self.dict.len() - back;
                let take = left.min(back);
                self.history
                    .extend_from_slice(&self.dict[from..from + take]);
                take
            } else {
                // `history` holds the last `window` bytes, and as far back as
                // this match reaches (a match past the window starts while the
                // output is within it, when nothing has been let go yet). An
                // overlapping match repeats its first `offset` bytes.
                let from = self.history.len() - self.offset;
                let take = left.min(self.offset);
                self.history.extend_from_within(from..from + take);
                take
            };
            self.produced += take;
            left -= take;
        }
        self.match_left -= want;
    }
}

impl crate::io::Read for RebuiltSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, crate::io::Error> {
        let mut filled = 0;
        while filled < buf.len() {
            let room = buf.len() - filled;
            let start = self.history.len();
            if self.lit_left > 0 {
                let n = self.lit_left.min(room);
                self.history
                    .extend_from_slice(&self.literals[self.lit_at..self.lit_at + n]);
                self.lit_at += n;
                self.lit_left -= n;
                self.produced += n;
            } else if self.match_left > 0 {
                self.copy_match(self.match_left.min(room));
            } else if let Some(sequence) = self.sequences.next() {
                self.lit_left = sequence.lit_len as usize;
                self.match_left = sequence.match_len as usize;
                self.offset = sequence.offset as usize;
                continue;
            } else if self.lit_at < self.literals.len() {
                self.lit_left = self.literals.len() - self.lit_at;
                continue;
            } else {
                break;
            }
            let n = self.history.len() - start;
            buf[filled..filled + n].copy_from_slice(&self.history[start..]);
            filled += n;
        }
        // Keep what the matches still to come can reach: the window, and the
        // match in hand's reach while it lasts (later matches stay within the
        // window, as `new` checked). The rest goes a `keep`'s worth at a time,
        // so the drain is paid once per that many bytes; a `keep` past half
        // of `usize` never fills twice over.
        let keep = if self.match_left > 0 {
            self.window.max(self.offset)
        } else {
            self.window
        };
        if keep
            .checked_mul(2)
            .is_some_and(|limit| self.history.len() > limit)
        {
            let drop = self.history.len() - keep;
            self.history.drain(..drop);
        }
        Ok(filled)
    }
}

/// A [`Matcher`] that reports a given parse instead of searching, so the
/// ordinary block pipeline builds a frame from it (C
/// `ZSTD_compressSequences`). The parse is consumed as blocks are cut:
/// wherever the encoder ends a block, a sequence running across the cut is
/// split there, and a match piece shorter than [`MIN_MATCH`] goes out as
/// literals. Block delimiters, if any, end blocks: each delimited region is
/// a block of its own, cut further only where it exceeds the block size.
///
/// The parse should pass [`validate_parse`]: offsets are taken as given.
///
/// ```
/// use structured_zstd::encoding::sequences::{ExternalSequence, SequenceReplay};
/// use structured_zstd::encoding::{CompressionLevel, FrameCompressor};
///
/// let input = b"abcabcabcabc";
/// let parse = [ExternalSequence { offset: 3, lit_len: 3, match_len: 9, rep: 0 }];
/// let replay = SequenceReplay::new(&parse, 1 << 10);
/// let mut frame = Vec::new();
/// let mut compressor = FrameCompressor::new_with_matcher(replay, CompressionLevel::Default);
/// compressor.set_source(&input[..]);
/// compressor.set_drain(&mut frame);
/// compressor.compress();
/// # let mut decoded = Vec::with_capacity(input.len());
/// # structured_zstd::decoding::FrameDecoder::new().decode_all_to_vec(&frame, &mut decoded).unwrap();
/// # assert_eq!(decoded, input);
/// ```
pub struct SequenceReplay<'s> {
    sequences: &'s [ExternalSequence],
    /// The sequence the next block continues.
    next: usize,
    /// Literal bytes of `sequences[next]` earlier blocks took.
    lit_taken: u32,
    /// Match bytes of `sequences[next]` earlier blocks took.
    match_taken: u32,
    input: BlockInput,
    window_size: u64,
    dictionary_priming: bool,
    /// The parse holds block delimiters, which then lay the blocks out.
    delimited: bool,
}

impl<'s> SequenceReplay<'s> {
    /// Replay `sequences` into frames whose window is `window_size` bytes,
    /// which every offset must respect.
    pub fn new(sequences: &'s [ExternalSequence], window_size: u64) -> Self {
        Self {
            sequences,
            next: 0,
            lit_taken: 0,
            match_taken: 0,
            input: BlockInput::default(),
            window_size,
            dictionary_priming: false,
            delimited: sequences.iter().any(ExternalSequence::is_delimiter),
        }
    }

    /// Let the frame use the compressor's dictionary: its ID goes in the
    /// header and its entropy and repeat offsets seed the first block, so
    /// offsets may reach into its content.
    pub fn with_dictionary(mut self) -> Self {
        self.dictionary_priming = true;
        self
    }

    /// Step past `len` bytes of the parse without reporting them, for a block
    /// the encoder writes out unsearched.
    fn skip(&mut self, len: usize) {
        self.walk(len, &mut |_| {});
    }

    /// Report the parse for the next `len` bytes through `emit`.
    fn walk(&mut self, len: usize, emit: &mut dyn FnMut(Sequence)) {
        let mut remaining = len as u64;
        let mut literals = 0usize;
        while remaining > 0 {
            let Some(sequence) = self.sequences.get(self.next) else {
                break;
            };
            let lits_left = sequence.lit_len - self.lit_taken;
            let take = u64::from(lits_left).min(remaining) as u32;
            self.lit_taken += take;
            literals += take as usize;
            remaining -= u64::from(take);
            if self.lit_taken < sequence.lit_len {
                break;
            }
            let match_left = sequence.match_len - self.match_taken;
            let take = u64::from(match_left).min(remaining) as u32;
            if take > 0 {
                // A piece the cut left shorter than a match can be goes out
                // as literals: they are the same bytes either way, and the
                // rest of the match continues in the next block.
                if take >= MIN_MATCH {
                    emit(Sequence::Triple {
                        literal_len: literals,
                        offset: sequence.offset as usize,
                        match_len: take as usize,
                    });
                    literals = 0;
                } else {
                    literals += take as usize;
                }
                self.match_taken += take;
                remaining -= u64::from(take);
            }
            if self.match_taken == sequence.match_len {
                self.next += 1;
                self.lit_taken = 0;
                self.match_taken = 0;
            }
        }
        // Whatever the parse does not cover is literals.
        literals += remaining as usize;
        if literals > 0 {
            emit(Sequence::Literals { len: literals });
        }
    }
}

impl Matcher for SequenceReplay<'_> {
    fn get_last_space(&mut self) -> &[u8] {
        self.input.last_block()
    }

    fn fill_in_place(
        &mut self,
        capacity: usize,
        fill: &mut dyn FnMut(&mut HistoryBuf) -> (usize, bool),
    ) -> (usize, bool) {
        self.input.fill(capacity, fill)
    }

    fn uncommitted_input(&self) -> &[u8] {
        self.input.uncommitted()
    }

    fn commit_filled(&mut self, len: usize) {
        self.input.commit(len);
    }

    fn skip_matching(&mut self) {
        let len = self.input.last_block().len();
        self.skip(len);
    }

    fn start_matching(&mut self, mut handle_sequence: impl FnMut(Sequence)) {
        let len = self.input.last_block().len();
        self.walk(len, &mut handle_sequence);
    }

    fn reset(&mut self, _level: CompressionLevel) {
        self.input.clear();
        self.next = 0;
        self.lit_taken = 0;
        self.match_taken = 0;
    }

    fn supports_dictionary_priming(&self) -> bool {
        self.dictionary_priming
    }

    fn window_size(&self) -> u64 {
        self.window_size
    }

    /// The rest of the region up to the next block delimiter, skipping
    /// regions with nothing in them.
    fn fixed_block_len(&self) -> Option<usize> {
        if !self.delimited {
            return None;
        }
        let mut len = 0u64;
        let (mut lit_taken, mut match_taken) = (self.lit_taken, self.match_taken);
        for sequence in &self.sequences[self.next..] {
            len += u64::from(sequence.lit_len - lit_taken)
                + u64::from(sequence.match_len - match_taken);
            (lit_taken, match_taken) = (0, 0);
            if sequence.is_delimiter() && len > 0 {
                return usize::try_from(len).ok();
            }
        }
        None
    }
}

/// Why a [`SequenceProducer`] gave no parse for a block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SequenceProducerError;

/// A parser asked for each block in place of the built-in match finder (C
/// `ZSTD_registerSequenceProducer`). Its parse covers the block alone, with
/// no history before it.
pub trait SequenceProducer {
    /// Append a parse of `block` to `out`: matches only within `block`, at
    /// most `window_size` back, ending with a sequence of no match, or one
    /// whose match reaches the end.
    ///
    /// # Errors
    /// [`SequenceProducerError`] when there is no parse; the encoder then
    /// falls back to its own match finder or fails the frame, as configured.
    fn produce(
        &mut self,
        block: &[u8],
        window_size: u64,
        out: &mut Vec<ExternalSequence>,
    ) -> Result<(), SequenceProducerError>;
}

#[cfg(test)]
mod tests;
