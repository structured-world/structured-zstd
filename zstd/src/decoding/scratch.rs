//! Structures that wrap around various decoders to make decoding easier.

use super::super::blocks::sequence_section::Sequence;
use super::decode_buffer::DecodeBuffer;
use crate::decoding::dictionary::Dictionary;
use crate::fse::FSETable;
use crate::huff0::HuffmanTable;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};

use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};

/// A block level decoding buffer.
pub struct DecoderScratch {
    /// The decoder used for Huffman blocks.
    pub huf: HuffmanScratch,
    /// The decoder used for FSE blocks.
    pub fse: FSEScratch,

    pub buffer: DecodeBuffer,
    pub offset_hist: [u32; 3],

    pub literals_buffer: Vec<u8>,
    pub sequences: Vec<Sequence>,
    pub block_content_buffer: Vec<u8>,
}

impl DecoderScratch {
    pub fn new(window_size: usize) -> DecoderScratch {
        DecoderScratch {
            huf: HuffmanScratch {
                table: HuffmanTable::new(),
            },
            fse: FSEScratch {
                offsets: AlignedFSETable::new(MAX_OFFSET_CODE),
                of_rle: None,
                literal_lengths: AlignedFSETable::new(MAX_LITERAL_LENGTH_CODE),
                ll_rle: None,
                match_lengths: AlignedFSETable::new(MAX_MATCH_LENGTH_CODE),
                ml_rle: None,
            },
            buffer: DecodeBuffer::new(window_size),
            offset_hist: [1, 4, 8],

            block_content_buffer: Vec::new(),
            literals_buffer: Vec::new(),
            sequences: Vec::new(),
        }
    }

    pub fn reset(&mut self, window_size: usize) {
        self.offset_hist = [1, 4, 8];
        self.literals_buffer.clear();
        self.sequences.clear();
        self.block_content_buffer.clear();

        self.buffer.reset(window_size);

        self.fse.literal_lengths.reset();
        self.fse.match_lengths.reset();
        self.fse.offsets.reset();
        self.fse.ll_rle = None;
        self.fse.ml_rle = None;
        self.fse.of_rle = None;

        self.huf.table.reset();
    }

    pub fn init_from_dict(&mut self, dict: &Dictionary) {
        self.fse.reinit_from(&dict.fse);
        self.huf.table.reinit_from(&dict.huf.table);
        self.offset_hist = dict.offset_hist;
        self.buffer.dict_content.clear();
        self.buffer
            .dict_content
            .extend_from_slice(&dict.dict_content);
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
    pub of_rle: Option<u8>,
    pub literal_lengths: AlignedFSETable,
    pub ll_rle: Option<u8>,
    pub match_lengths: AlignedFSETable,
    pub ml_rle: Option<u8>,
}

impl FSEScratch {
    pub fn new() -> FSEScratch {
        FSEScratch {
            offsets: AlignedFSETable::new(MAX_OFFSET_CODE),
            of_rle: None,
            literal_lengths: AlignedFSETable::new(MAX_LITERAL_LENGTH_CODE),
            ll_rle: None,
            match_lengths: AlignedFSETable::new(MAX_MATCH_LENGTH_CODE),
            ml_rle: None,
        }
    }

    pub fn reinit_from(&mut self, other: &Self) {
        self.offsets.reinit_from(&other.offsets);
        self.literal_lengths.reinit_from(&other.literal_lengths);
        self.match_lengths.reinit_from(&other.match_lengths);
        self.of_rle = other.of_rle;
        self.ll_rle = other.ll_rle;
        self.ml_rle = other.ml_rle;
    }
}

impl Default for FSEScratch {
    fn default() -> Self {
        Self::new()
    }
}

// Keep LL/ML/OF table objects cache-line aligned to avoid cross-table placement
// effects when they are accessed in the same decode hot loop.
#[cfg_attr(target_arch = "aarch64", repr(align(128)))]
#[cfg_attr(not(target_arch = "aarch64"), repr(align(64)))]
pub struct AlignedFSETable(FSETable);

impl AlignedFSETable {
    fn new(max_symbol: u8) -> Self {
        Self(FSETable::new(max_symbol))
    }
}

impl Deref for AlignedFSETable {
    type Target = FSETable;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for AlignedFSETable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
