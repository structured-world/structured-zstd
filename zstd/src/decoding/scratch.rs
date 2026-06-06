//! Structures that wrap around various decoders to make decoding easier.

use super::buffer_backend::BufferBackend;
use super::decode_buffer::DecodeBuffer;
use super::ringbuffer::RingBuffer;
use crate::decoding::dictionary::DictionaryHandle;
use crate::fse::SeqFSETable;
use crate::huff0::HuffmanTable;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};

use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};

/// A block level decoding buffer, parameterised over the output
/// storage backend ([`BufferBackend`]). Default `RingBuffer` keeps
/// the historical API; `DecoderScratch<FlatBuf>` is instantiated by
/// [`super::frame_decoder::FrameDecoder`] (via `DecoderScratchKind`)
/// when the frame's `Single_Segment_flag` is set — see backlog item
/// #132.
pub struct DecoderScratch<B: BufferBackend = RingBuffer> {
    /// The decoder used for Huffman blocks.
    pub huf: HuffmanScratch,
    /// The decoder used for FSE blocks.
    pub fse: FSEScratch,

    pub buffer: DecodeBuffer<B>,
    pub offset_hist: [u32; 3],

    pub literals_buffer: Vec<u8>,
    pub block_content_buffer: Vec<u8>,
}

/// Borrowed view of all per-call decoder scratch fields as `&mut`
/// references. Returned by [`Workspace::split`] so the block /
/// literals / sequence decoder functions can hold simultaneous
/// independent borrows of distinct fields — the field-split is
/// what makes "borrow huf and literals_buffer at the same time"
/// type-check, both for the owned [`DecoderScratch<B>`] path and the
/// direct-decode path where these fields are borrowed by reference
/// from a [`crate::decoding::FrameDecoder`].
///
/// The lifetime `'a` is the shorter-of (a) the underlying owner's
/// lifetime and (b) the active borrow. The backend type `B` flows
/// through to [`super::decode_buffer::DecodeBuffer<B>`].
pub struct WorkspaceRef<'a, B: BufferBackend> {
    pub huf: &'a mut HuffmanScratch,
    pub fse: &'a mut FSEScratch,
    pub buffer: &'a mut DecodeBuffer<B>,
    pub offset_hist: &'a mut [u32; 3],
    pub literals_buffer: &'a mut Vec<u8>,
    pub block_content_buffer: &'a mut Vec<u8>,
}

/// Polymorphic accessor for the decoder's per-call scratch state.
/// Both the owned [`DecoderScratch<B>`] (used by the streaming and
/// one-shot `decode_all` paths) and the borrow-ref direct-decode
/// scratch (`DirectScratch`) implement this trait, so the block /
/// literals / sequence decode functions are written once against
/// `Workspace` and instantiated for both shapes via compile-time
/// monomorphisation.
///
/// The single `split` method returns all fields at once as a
/// [`WorkspaceRef`] so callers retain Rust's field-level
/// disjoint-borrow analysis. Per-field accessors would force
/// sequential borrows and break call sites that need e.g.
/// `&mut huf` and `&mut literals_buffer` simultaneously.
pub(crate) trait Workspace {
    type Backend: BufferBackend;
    fn split(&mut self) -> WorkspaceRef<'_, Self::Backend>;
}

impl<B: BufferBackend> Workspace for DecoderScratch<B> {
    type Backend = B;
    fn split(&mut self) -> WorkspaceRef<'_, B> {
        WorkspaceRef {
            huf: &mut self.huf,
            fse: &mut self.fse,
            buffer: &mut self.buffer,
            offset_hist: &mut self.offset_hist,
            literals_buffer: &mut self.literals_buffer,
            block_content_buffer: &mut self.block_content_buffer,
        }
    }
}

/// Direct-decode scratch: per-call workspace that wraps a
/// stack-local [`DecodeBuffer<UserSliceBackend<'o>>`] over the
/// caller's `&'o mut [u8]` output slice, plus `&'p mut` borrows of
/// the persistent decoder state (HUF / FSE tables, offset_hist,
/// sequence cache, scratch Vecs) owned by [`crate::decoding::FrameDecoder`].
///
/// The lifetime split:
/// - `'o` — caller's output slice (borrowed via `buffer`).
/// - `'p` — FrameDecoder's persistent fields (borrowed via the
///   `&'p mut` fields).
///
/// Implementing [`Workspace`] lets the existing
/// `block_decoder::decode_block_content` / `decompress_block`
/// generic-over-W functions consume this scratch unchanged. The
/// perf rationale: eliminating the `DecodeBuffer::read` drain copy
/// that the owned-buffer path performs, by writing decoded bytes
/// straight into the caller-provided output slice.
///
/// Constructed inside `FrameDecoder::decode_all` and dropped
/// at function exit; never persisted across calls.
pub struct DirectScratch<'o, 'p> {
    pub huf: &'p mut HuffmanScratch,
    pub fse: &'p mut FSEScratch,
    pub buffer: DecodeBuffer<super::user_slice_buf::UserSliceBackend<'o>>,
    pub offset_hist: &'p mut [u32; 3],
    pub literals_buffer: &'p mut Vec<u8>,
    pub block_content_buffer: &'p mut Vec<u8>,
}

impl<'o, 'p> Workspace for DirectScratch<'o, 'p> {
    type Backend = super::user_slice_buf::UserSliceBackend<'o>;
    fn split(&mut self) -> WorkspaceRef<'_, Self::Backend> {
        // Reborrow the `&'p mut` fields to `&'_ mut` so the returned
        // WorkspaceRef's lifetime is tied to the `&mut self` of this
        // call, not to `'p`. This is what lets nested decode
        // functions hold a WorkspaceRef without freezing the whole
        // `'p`-bound DirectScratch for their entire scope.
        WorkspaceRef {
            huf: &mut *self.huf,
            fse: &mut *self.fse,
            buffer: &mut self.buffer,
            offset_hist: &mut *self.offset_hist,
            literals_buffer: &mut *self.literals_buffer,
            block_content_buffer: &mut *self.block_content_buffer,
        }
    }
}

impl<B: BufferBackend> DecoderScratch<B> {
    pub fn new(window_size: usize) -> DecoderScratch<B> {
        DecoderScratch {
            huf: HuffmanScratch {
                table: HuffmanTable::new(),
            },
            fse: FSEScratch {
                offsets: AlignedFSETable::new(MAX_OFFSET_CODE),
                literal_lengths: AlignedFSETable::new(MAX_LITERAL_LENGTH_CODE),
                match_lengths: AlignedFSETable::new(MAX_MATCH_LENGTH_CODE),
                offsets_long_share: 0,
                ddict_is_cold: false,
            },
            buffer: DecodeBuffer::new(window_size),
            offset_hist: [1, 4, 8],

            block_content_buffer: Vec::new(),
            literals_buffer: Vec::new(),
        }
    }

    pub fn reset(&mut self, window_size: usize) {
        self.offset_hist = [1, 4, 8];
        self.literals_buffer.clear();
        self.block_content_buffer.clear();

        // Pre-allocate the per-block scratch Vecs to `min(window_size,
        // MAX_BLOCK_SIZE)` so the first block's
        // `extend_from_slice` / `resize` does not pay anonymous-page
        // first-touch faults inside the decode hot path. `clear()`
        // keeps `capacity()`, so subsequent frames with the same
        // (or smaller) window also avoid realloc. Matches donor's
        // upfront sizing strategy where `dctx->litExtraBuffer` and
        // the dst layout are sized to `blockSizeMax` at frame init.
        // Measured at ~18% of decode-time page-fault cost on
        // level_-7_fast/decodecorpus-z000033.
        let block_cap = (window_size.min(crate::common::MAX_BLOCK_SIZE as usize)).max(8);
        // Pre-TOUCH (not just reserve) so the kernel maps the
        // anonymous pages here instead of inside the decode hot
        // path. `Vec::reserve` only allocates address space; the
        // first byte-write to each 4 KiB page still triggers a
        // page fault.
        //
        // ONLY when the Vec's capacity is below the target — once
        // a frame has touched the pages once, `clear()` keeps both
        // `capacity()` AND the kernel's anonymous-page mapping, so
        // subsequent frames hit warm memory without re-zeroing.
        // The previous shape (`resize` + `clear` unconditionally)
        // paid an O(block_cap) memset every frame reset, ~37 µs
        // per 128 KiB at AVX2 store rates. Now it's only paid on
        // the very first reset (or after a grow to larger
        // window_size).
        //
        // This matches donor's `dctx->litExtraBuffer` /
        // `dctx->workspace` lifecycle — touched once at decoder
        // construction, warm across all subsequent frames.
        if self.literals_buffer.capacity() < block_cap {
            self.literals_buffer.resize(block_cap, 0);
            self.literals_buffer.clear();
        }
        if self.block_content_buffer.capacity() < block_cap {
            self.block_content_buffer.resize(block_cap, 0);
            self.block_content_buffer.clear();
        }

        self.buffer.reset(window_size);

        self.fse.literal_lengths.reset();
        self.fse.match_lengths.reset();
        self.fse.offsets.reset();
        // Reset the cached pipeline-gate signal alongside the FSE
        // table reset — otherwise scratch reuse across frames could
        // engage the long pipeline on a new frame's Repeat-mode
        // header based on the previous frame's offset distribution
        // (or vice versa: skip the pipeline when the new frame
        // actually has long offsets).
        self.fse.offsets_long_share = 0;
        // Pair the one-shot cold-dict flag with `reset`: a scratch
        // reused from a dictionary-attached frame whose blocks never
        // entered sequence decoding (raw-/RLE-only blocks, zero-seq
        // compressed blocks) would otherwise carry the flag into the
        // next frame and mis-apply the cold-dict gate there. Cleared
        // alongside `offsets_long_share` so the no-dict path keeps
        // the documented "no behaviour change" property.
        self.fse.ddict_is_cold = false;

        self.huf.table.reset();
    }

    pub fn init_from_dict(&mut self, dict: &DictionaryHandle) {
        let d = dict.as_dict();
        self.fse.reinit_from(&d.fse);
        self.huf.table.reinit_from(&d.huf.table);
        self.offset_hist = d.offset_hist;
        // Share the dictionary content by handle (Arc/Rc clone = refcount
        // bump) instead of copying it into a per-frame buffer; the decoder
        // reads match bytes straight out of the shared content.
        self.buffer.set_dict(dict.clone());
        // Donor parity: `ZSTD_decompressBegin_usingDDict` sets
        // `dctx->ddictIsCold = 1` so the first block of the frame
        // engages the prefetch decoder regardless of long-offset
        // share. We do the same here; the first
        // `decode_and_execute_sequences` call consumes the flag and
        // resets it to `false`.
        self.fse.ddict_is_cold = true;
    }
}

pub struct HuffmanScratch {
    pub table: HuffmanTable,
}

impl HuffmanScratch {
    pub fn new() -> HuffmanScratch {
        HuffmanScratch {
            table: HuffmanTable::new(),
        }
    }
}

impl Default for HuffmanScratch {
    fn default() -> Self {
        Self::new()
    }
}

pub struct FSEScratch {
    pub offsets: AlignedFSETable,
    pub literal_lengths: AlignedFSETable,
    pub match_lengths: AlignedFSETable,
    /// Cached "share of offset codes strictly > LONG_OFFSET_CODE_THRESHOLD
    /// (i.e. codes ≥ 23 when the threshold is 22)" scaled to donor's
    /// `OffFSELog = 8` (256-entry reference).
    /// Updated by [`crate::decoding::sequence_section_decoder`] when
    /// the offsets FSE table is rebuilt (FSE / Predefined modes);
    /// stale-but-correct on Repeat-mode blocks where the table was
    /// not touched — the share is identical to the previous block's.
    /// The sequence-section pipeline gate reads this directly instead
    /// of re-walking `offsets.decode` per block.
    pub offsets_long_share: u32,
    /// Mirrors donor `ZSTD_DCtx::ddictIsCold`. Set to `true` when a
    /// dictionary is freshly attached (its FSE / HUF tables are not
    /// yet in cache); the first sequence-section decode of the
    /// resulting frame engages the pipelined prefetch decoder
    /// regardless of long-offset share, then clears the flag so
    /// subsequent blocks fall back to the `offsets_long_share`
    /// heuristic. The `num_sequences >= ADVANCE * 2` guard still
    /// applies: blocks too small to fill the lookahead pipeline take
    /// the short-block fallback in both the cold-dict and warm cases.
    /// Without a dictionary the flag stays `false` (cache state of the
    /// predefined and repeat tables is not considered "cold" in the
    /// donor model).
    pub ddict_is_cold: bool,
}

impl FSEScratch {
    pub fn new() -> FSEScratch {
        FSEScratch {
            offsets: AlignedFSETable::new(MAX_OFFSET_CODE),
            literal_lengths: AlignedFSETable::new(MAX_LITERAL_LENGTH_CODE),
            match_lengths: AlignedFSETable::new(MAX_MATCH_LENGTH_CODE),
            offsets_long_share: 0,
            ddict_is_cold: false,
        }
    }

    pub fn reinit_from(&mut self, other: &Self) {
        self.offsets.reinit_from(&other.offsets);
        self.literal_lengths.reinit_from(&other.literal_lengths);
        self.match_lengths.reinit_from(&other.match_lengths);
        // Recompute the share from the just-copied offsets table
        // rather than trusting `other.offsets_long_share`. Two source
        // shapes produce a populated `offsets` table but a still-zero
        // cached share: (a) `Dictionary::decode_dict` rebuilds the
        // offsets FSE table from the dictionary's entropy section
        // without ever calling the sequence-decoder path that updates
        // the cache, and (b) any future caller that mutates the table
        // directly. Recomputing here keeps the pipeline gate aligned
        // with the actual table shape regardless of how the table got
        // there.
        self.offsets_long_share =
            super::sequence_section_decoder::compute_offsets_long_share(&self.offsets);
    }
}

impl Default for FSEScratch {
    fn default() -> Self {
        Self::new()
    }
}

// Keep LL/ML/OF table *objects* cache-line aligned to avoid cross-table placement
// effects in DecoderScratch when they are accessed in the same decode hot loop.
// Note: this aligns the table containers, not the `Vec<SeqSymbol>` backing allocations.
#[cfg_attr(target_arch = "aarch64", repr(align(128)))]
#[cfg_attr(not(target_arch = "aarch64"), repr(align(64)))]
pub struct AlignedFSETable(SeqFSETable);

impl AlignedFSETable {
    fn new(max_symbol: u8) -> Self {
        Self(SeqFSETable::new(max_symbol))
    }
}

impl Deref for AlignedFSETable {
    type Target = SeqFSETable;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for AlignedFSETable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoding::dictionary::Dictionary;

    #[test]
    fn init_from_dict_marks_fse_ddict_is_cold() {
        // Donor parity: `ZSTD_decompressBegin_usingDDict` sets
        // `dctx->ddictIsCold = 1`. Mirror: `init_from_dict` must
        // leave `fse.ddict_is_cold = true` so the first
        // sequence-section decode of the frame engages the prefetch
        // pipeline regardless of `offsets_long_share`.
        extern crate std;
        let dict_raw =
            std::fs::read("./dict_tests/dictionary").expect("dictionary fixture should load");
        let dict = DictionaryHandle::from_dictionary(
            Dictionary::decode_dict(&dict_raw).expect("dictionary should parse"),
        );
        let mut scratch: DecoderScratch = DecoderScratch::new(1024);
        assert!(
            !scratch.fse.ddict_is_cold,
            "fresh DecoderScratch must not advertise a cold dict"
        );
        scratch.init_from_dict(&dict);
        assert!(
            scratch.fse.ddict_is_cold,
            "init_from_dict must set ddict_is_cold = true"
        );
    }
}
