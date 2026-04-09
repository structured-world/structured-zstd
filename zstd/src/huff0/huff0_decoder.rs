//! Utilities for decoding Huff0 encoded huffman data.

use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::HuffmanTableError;
use crate::fse::{FSEDecoder, FSETable};
use alloc::vec::Vec;
#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::{vandq_u32, vdupq_n_u32, vld1q_u32, vshrq_n_u32, vst1q_u32};
#[cfg(target_arch = "aarch64")]
use core::arch::asm;
#[cfg(target_arch = "x86")]
use core::arch::x86::{
    _bzhi_u32, _mm_i32gather_epi32, _mm_maskz_compress_epi8, _mm_set_epi32, _mm_storeu_si128,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    _bzhi_u64, _mm_i32gather_epi32, _mm_maskz_compress_epi8, _mm_set_epi32, _mm_storeu_si128,
};
#[cfg(all(feature = "std", target_arch = "aarch64"))]
use std::arch::is_aarch64_feature_detected;
#[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
use std::arch::is_x86_feature_detected;
#[cfg(feature = "std")]
use std::sync::OnceLock;

/// The Zstandard specification limits the maximum length of a code to 11 bits.
pub(crate) const MAX_MAX_NUM_BITS: u8 = 11;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum HuffmanDecodeKernel {
    Scalar,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    X86Bmi2,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    X86Avx2,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    X86Vbmi2,
    #[cfg(target_arch = "aarch64")]
    Aarch64Neon,
    #[cfg(target_arch = "aarch64")]
    Aarch64Sve,
}

#[cfg(feature = "std")]
#[inline(always)]
pub(crate) fn detect_huffman_decode_kernel() -> HuffmanDecodeKernel {
    static KERNEL: OnceLock<HuffmanDecodeKernel> = OnceLock::new();
    *KERNEL.get_or_init(|| {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if is_x86_feature_detected!("avx512vbmi2")
                && is_x86_feature_detected!("avx512f")
                && is_x86_feature_detected!("avx512vl")
                && is_x86_feature_detected!("avx512bw")
                && is_x86_feature_detected!("bmi2")
            {
                return HuffmanDecodeKernel::X86Vbmi2;
            }
            if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("bmi2") {
                return HuffmanDecodeKernel::X86Avx2;
            }
            if is_x86_feature_detected!("bmi2") {
                return HuffmanDecodeKernel::X86Bmi2;
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            if is_aarch64_feature_detected!("sve") {
                return HuffmanDecodeKernel::Aarch64Sve;
            }
            if is_aarch64_feature_detected!("neon") {
                return HuffmanDecodeKernel::Aarch64Neon;
            }
        }
        HuffmanDecodeKernel::Scalar
    })
}

#[cfg(not(feature = "std"))]
#[inline(always)]
pub(crate) fn detect_huffman_decode_kernel() -> HuffmanDecodeKernel {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if cfg!(all(
            target_feature = "avx512vbmi2",
            target_feature = "avx512f",
            target_feature = "avx512vl",
            target_feature = "avx512bw",
            target_feature = "bmi2"
        )) {
            return HuffmanDecodeKernel::X86Vbmi2;
        }
        if cfg!(all(target_feature = "avx2", target_feature = "bmi2")) {
            return HuffmanDecodeKernel::X86Avx2;
        }
        if cfg!(target_feature = "bmi2") {
            return HuffmanDecodeKernel::X86Bmi2;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if cfg!(target_feature = "sve") {
            return HuffmanDecodeKernel::Aarch64Sve;
        }
        if cfg!(target_feature = "neon") {
            return HuffmanDecodeKernel::Aarch64Neon;
        }
    }
    HuffmanDecodeKernel::Scalar
}

pub struct HuffmanDecoder<'table> {
    table: &'table HuffmanTable,
    kernel: HuffmanDecodeKernel,
    /// State is used to index into the table.
    pub state: u64,
}

impl<'t> HuffmanDecoder<'t> {
    /// Create a new decoder with the provided table
    pub fn new(table: &'t HuffmanTable) -> HuffmanDecoder<'t> {
        HuffmanDecoder {
            table,
            kernel: detect_huffman_decode_kernel(),
            state: 0,
        }
    }

    /// Decode the symbol the internal state (cursor) is pointed at and return the
    /// decoded literal.
    #[inline(always)]
    fn decode_symbol(&mut self) -> u8 {
        self.table.decode[self.state as usize].symbol
    }

    /// Fuzz-only shim for reading the symbol at the current state.
    #[cfg(feature = "fuzz_exports")]
    #[inline(always)]
    pub fn fuzz_decode_symbol(&mut self) -> u8 {
        self.decode_symbol()
    }

    /// Initialize internal state and prepare to decode data. Then, `decode_symbol` can be called
    /// to read the byte the internal cursor is pointing at, and `next_state` can be called to advance
    /// the cursor until the max number of bits has been read.
    #[inline(always)]
    pub fn init_state(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        let num_bits = self.table.max_num_bits;
        let new_bits = br.get_bits(num_bits);
        self.state = new_bits;
        num_bits
    }

    /// Advance the internal cursor to the next symbol. After this, you can call `decode_symbol`
    /// to read from the new position.
    #[inline(always)]
    fn next_state(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        // self.state stores a small section, or a window of the bit stream. The table can be indexed via this state,
        // telling you how many bits identify the current symbol.
        let num_bits = self.table.decode[self.state as usize].num_bits;
        // New bits are read from the stream
        let new_bits = br.get_bits(num_bits);
        // Shift and mask out the bits that identify the current symbol
        self.state = ((self.state << num_bits) & self.table.state_mask) | new_bits;
        num_bits
    }

    /// Fuzz-only shim for advancing to the next decoding state.
    #[cfg(feature = "fuzz_exports")]
    #[inline(always)]
    pub fn fuzz_next_state(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        self.next_state(br)
    }

    /// Decode symbol and advance state in one table lookup.
    #[inline(always)]
    pub fn decode_symbol_and_advance(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        match self.kernel {
            HuffmanDecodeKernel::Scalar => self.decode_symbol_and_advance_scalar(br),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            HuffmanDecodeKernel::X86Bmi2
            | HuffmanDecodeKernel::X86Avx2
            | HuffmanDecodeKernel::X86Vbmi2 => {
                // SAFETY: This path is selected only after runtime/static feature checks.
                unsafe { self.decode_symbol_and_advance_x86_bmi2(br) }
            }
            #[cfg(target_arch = "aarch64")]
            HuffmanDecodeKernel::Aarch64Neon => {
                // SAFETY: This path is selected only after runtime/static feature checks.
                unsafe { self.decode_symbol_and_advance_aarch64_neon(br) }
            }
            #[cfg(target_arch = "aarch64")]
            HuffmanDecodeKernel::Aarch64Sve => {
                // SAFETY: This path is selected only after runtime/static feature checks.
                unsafe { self.decode_symbol_and_advance_aarch64_sve(br) }
            }
        }
    }

    #[inline(always)]
    pub(crate) fn decode_symbol_and_num_bits(&self) -> (u8, u8) {
        let entry = self.table.decode[self.state as usize];
        (entry.symbol, entry.num_bits)
    }

    #[inline(always)]
    pub(crate) fn advance_state_by_bits(&mut self, br: &mut BitReaderReversed<'_>, num_bits: u8) {
        let new_bits = br.get_bits(num_bits);
        match self.kernel {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            HuffmanDecodeKernel::X86Bmi2
            | HuffmanDecodeKernel::X86Avx2
            | HuffmanDecodeKernel::X86Vbmi2 => {
                // SAFETY: Kernel dispatch guarantees BMI2 on this path.
                unsafe {
                    self.state = self.advance_state_x86_bmi2(num_bits, new_bits);
                }
            }
            _ => {
                self.state = ((self.state << num_bits) & self.table.state_mask) | new_bits;
            }
        }
    }

    #[inline(always)]
    pub(crate) fn decode4_symbols_and_num_bits(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let kernel = decoders[0].kernel;
        let same_kernel = decoders.iter().all(|d| d.kernel == kernel);
        let same_table = decoders
            .iter()
            .all(|d| core::ptr::eq(d.table, decoders[0].table));
        debug_assert!(same_kernel);
        debug_assert!(same_table);
        // Keep this invariant in release builds too: SIMD variants read packed
        // entries through `decoders[0].table` for all decoder states.
        if !(same_kernel && same_table) {
            return Self::decode4_symbols_and_num_bits_scalar(decoders);
        }
        match kernel {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            HuffmanDecodeKernel::X86Vbmi2 => {
                // SAFETY: VBMI2 kernel is selected only after runtime/static feature checks.
                unsafe { Self::decode4_symbols_and_num_bits_vbmi2(decoders) }
            }
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            HuffmanDecodeKernel::X86Avx2 => {
                // SAFETY: AVX2 kernel is selected only after runtime/static feature checks.
                unsafe { Self::decode4_symbols_and_num_bits_avx2(decoders) }
            }
            #[cfg(target_arch = "aarch64")]
            HuffmanDecodeKernel::Aarch64Neon => {
                // SAFETY: NEON kernel is selected only after runtime/static feature checks.
                unsafe { Self::decode4_symbols_and_num_bits_neon(decoders) }
            }
            #[cfg(target_arch = "aarch64")]
            HuffmanDecodeKernel::Aarch64Sve => {
                // SAFETY: SVE kernel is selected only after runtime/static feature checks.
                unsafe { Self::decode4_symbols_and_num_bits_sve(decoders) }
            }
            _ => Self::decode4_symbols_and_num_bits_scalar(decoders),
        }
    }

    #[inline(always)]
    fn decode4_symbols_and_num_bits_scalar(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let mut symbols = [0_u8; 4];
        let mut num_bits = [0_u8; 4];
        let mut i = 0;
        while i < 4 {
            let (sym, bits) = decoders[i].decode_symbol_and_num_bits();
            symbols[i] = sym;
            num_bits[i] = bits;
            i += 1;
        }
        (symbols, num_bits)
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx512vbmi2,avx512f,avx512vl,avx512bw")]
    unsafe fn decode4_symbols_and_num_bits_vbmi2(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let table = decoders[0].table;
        let packed = _mm_set_epi32(
            table.packed_decode[decoders[3].state as usize] as i32,
            table.packed_decode[decoders[2].state as usize] as i32,
            table.packed_decode[decoders[1].state as usize] as i32,
            table.packed_decode[decoders[0].state as usize] as i32,
        );

        // Keep byte0 and byte1 from each u32 lane, then compress them to the low bytes.
        let symbols_bytes = _mm_maskz_compress_epi8(0b0001_0001_0001_0001, packed);
        let bits_bytes = _mm_maskz_compress_epi8(0b0010_0010_0010_0010, packed);

        let mut symbols_tmp = [0_u8; 16];
        let mut bits_tmp = [0_u8; 16];
        unsafe {
            _mm_storeu_si128(symbols_tmp.as_mut_ptr().cast(), symbols_bytes);
            _mm_storeu_si128(bits_tmp.as_mut_ptr().cast(), bits_bytes);
        }
        (
            [
                symbols_tmp[0],
                symbols_tmp[1],
                symbols_tmp[2],
                symbols_tmp[3],
            ],
            [bits_tmp[0], bits_tmp[1], bits_tmp[2], bits_tmp[3]],
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2")]
    unsafe fn decode4_symbols_and_num_bits_avx2(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let table = decoders[0].table;
        let states = _mm_set_epi32(
            decoders[3].state as i32,
            decoders[2].state as i32,
            decoders[1].state as i32,
            decoders[0].state as i32,
        );
        let gathered =
            unsafe { _mm_i32gather_epi32(table.packed_decode.as_ptr().cast::<i32>(), states, 4) };

        let mut packed = [0_i32; 4];
        unsafe {
            _mm_storeu_si128(packed.as_mut_ptr().cast(), gathered);
        }

        let mut symbols = [0_u8; 4];
        let mut num_bits = [0_u8; 4];
        let mut i = 0;
        while i < 4 {
            let v = packed[i] as u32;
            symbols[i] = (v & 0xFF) as u8;
            num_bits[i] = ((v >> 8) & 0xFF) as u8;
            i += 1;
        }
        (symbols, num_bits)
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn decode4_symbols_and_num_bits_neon(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let table = decoders[0].table;
        let packed_scalar = [
            table.packed_decode[decoders[0].state as usize],
            table.packed_decode[decoders[1].state as usize],
            table.packed_decode[decoders[2].state as usize],
            table.packed_decode[decoders[3].state as usize],
        ];

        let packed = unsafe { vld1q_u32(packed_scalar.as_ptr()) };
        let mask = vdupq_n_u32(0xFF);
        let symbols_v = vandq_u32(packed, mask);
        let bits_v = vandq_u32(vshrq_n_u32::<8>(packed), mask);

        let mut symbols_u32 = [0_u32; 4];
        let mut bits_u32 = [0_u32; 4];
        unsafe {
            vst1q_u32(symbols_u32.as_mut_ptr(), symbols_v);
            vst1q_u32(bits_u32.as_mut_ptr(), bits_v);
        }

        let mut symbols = [0_u8; 4];
        let mut bits = [0_u8; 4];
        let mut i = 0;
        while i < 4 {
            symbols[i] = symbols_u32[i] as u8;
            bits[i] = bits_u32[i] as u8;
            i += 1;
        }
        (symbols, bits)
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "sve")]
    unsafe fn decode4_symbols_and_num_bits_sve(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let table = decoders[0].table;
        let packed_scalar = [
            table.packed_decode[decoders[0].state as usize],
            table.packed_decode[decoders[1].state as usize],
            table.packed_decode[decoders[2].state as usize],
            table.packed_decode[decoders[3].state as usize],
        ];

        let mut symbols_u32 = [0_u32; 4];
        let mut bits_u32 = [0_u32; 4];
        let lanes = 4_usize;

        // Stable Rust does not yet expose SVE intrinsics in core::arch.
        // Use SVE inline asm for 4-lane packed-entry unpack:
        // symbol = packed & 0xff; bits = (packed >> 8) & 0xff.
        unsafe {
            asm!(
                "whilelt p0.s, xzr, {lanes}",
                "ld1w z0.s, p0/z, [{inptr}]",
                "mov z1.d, z0.d",
                "lsr z2.s, z0.s, #8",
                "and z1.s, z1.s, #0xff",
                "and z2.s, z2.s, #0xff",
                "st1w z1.s, p0, [{symptr}]",
                "st1w z2.s, p0, [{bitptr}]",
                inptr = in(reg) packed_scalar.as_ptr(),
                symptr = in(reg) symbols_u32.as_mut_ptr(),
                bitptr = in(reg) bits_u32.as_mut_ptr(),
                lanes = in(reg) lanes,
                lateout("z0") _,
                lateout("z1") _,
                lateout("z2") _,
                lateout("p0") _,
                options(nostack),
            );
        }

        let mut symbols = [0_u8; 4];
        let mut bits = [0_u8; 4];
        let mut i = 0;
        while i < 4 {
            symbols[i] = symbols_u32[i] as u8;
            bits[i] = bits_u32[i] as u8;
            i += 1;
        }
        (symbols, bits)
    }

    #[inline(always)]
    fn decode_symbol_and_advance_scalar(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        let symbol = self.decode_symbol();
        self.next_state(br);
        symbol
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "bmi2")]
    unsafe fn decode_symbol_and_advance_x86_bmi2(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        let entry = self.table.decode[self.state as usize];
        let new_bits = br.get_bits(entry.num_bits);
        self.state = unsafe { self.advance_state_x86_bmi2(entry.num_bits, new_bits) };
        entry.symbol
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "bmi2")]
    unsafe fn advance_state_x86_bmi2(&self, num_bits: u8, new_bits: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            _bzhi_u64(self.state << num_bits, u32::from(self.table.max_num_bits)) | new_bits
        }
        #[cfg(target_arch = "x86")]
        {
            let shifted = ((self.state << num_bits) & u64::from(u32::MAX)) as u32;
            unsafe { u64::from(_bzhi_u32(shifted, u32::from(self.table.max_num_bits))) | new_bits }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn decode_symbol_and_advance_aarch64_neon(
        &mut self,
        br: &mut BitReaderReversed<'_>,
    ) -> u8 {
        let entry = self.table.decode[self.state as usize];
        let new_bits = br.get_bits(entry.num_bits);
        self.state = ((self.state << entry.num_bits) & self.table.state_mask) | new_bits;
        entry.symbol
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "sve")]
    unsafe fn decode_symbol_and_advance_aarch64_sve(
        &mut self,
        br: &mut BitReaderReversed<'_>,
    ) -> u8 {
        let entry = self.table.decode[self.state as usize];
        let new_bits = br.get_bits(entry.num_bits);
        self.state = ((self.state << entry.num_bits) & self.table.state_mask) | new_bits;
        entry.symbol
    }
}

/// A Huffman decoding table contains a list of Huffman prefix codes and their associated values
pub struct HuffmanTable {
    decode: Vec<Entry>,
    packed_decode: Vec<u32>,
    /// The weight of a symbol is the number of occurences in a table.
    /// This value is used in constructing a binary tree referred to as
    /// a Huffman tree. Once this tree is constructed, it can be used to build the
    /// lookup table
    weights: Vec<u8>,
    /// The maximum size in bits a prefix code in the encoded data can be.
    /// This value is used so that the decoder knows how many bits
    /// to read from the bitstream before checking the table. This
    /// value must be 11 or lower.
    pub max_num_bits: u8,
    state_mask: u64,
    bits: Vec<u8>,
    bit_ranks: Vec<u32>,
    rank_indexes: Vec<usize>,
    /// In some cases, the list of weights is compressed using FSE compression.
    fse_table: FSETable,
}

impl HuffmanTable {
    /// Create a new, empty table.
    pub fn new() -> HuffmanTable {
        HuffmanTable {
            decode: Vec::new(),
            packed_decode: Vec::new(),

            weights: Vec::with_capacity(256),
            max_num_bits: 0,
            state_mask: 0,
            bits: Vec::with_capacity(256),
            bit_ranks: Vec::with_capacity(11),
            rank_indexes: Vec::with_capacity(11),
            fse_table: FSETable::new(255),
        }
    }

    /// Completely empty the table then repopulate as a replica
    /// of `other`.
    pub fn reinit_from(&mut self, other: &Self) {
        self.reset();
        self.decode.extend_from_slice(&other.decode);
        self.packed_decode.extend_from_slice(&other.packed_decode);
        self.weights.extend_from_slice(&other.weights);
        self.max_num_bits = other.max_num_bits;
        self.state_mask = other.state_mask;
        self.bits.extend_from_slice(&other.bits);
        self.rank_indexes.extend_from_slice(&other.rank_indexes);
        self.fse_table.reinit_from(&other.fse_table);
    }

    /// Completely empty the table of all data.
    pub fn reset(&mut self) {
        self.decode.clear();
        self.packed_decode.clear();
        self.weights.clear();
        self.max_num_bits = 0;
        self.state_mask = 0;
        self.bits.clear();
        self.bit_ranks.clear();
        self.rank_indexes.clear();
        self.fse_table.reset();
    }

    /// Build the equivalent encoder-side Huffman table from parsed weights.
    pub(crate) fn to_encoder_table(&self) -> Option<crate::huff0::huff0_encoder::HuffmanTable> {
        if self.bits.is_empty() || self.max_num_bits == 0 {
            return None;
        }

        let max_bits = usize::from(self.max_num_bits);
        let weights = self
            .bits
            .iter()
            .copied()
            .map(|num_bits| {
                if num_bits == 0 {
                    0
                } else {
                    max_bits - usize::from(num_bits) + 1
                }
            })
            .collect::<Vec<_>>();
        Some(crate::huff0::huff0_encoder::HuffmanTable::build_from_weights(&weights))
    }

    /// Read from `source` and decode the input, populating the huffman decoding table.
    ///
    /// Returns the number of bytes read.
    pub fn build_decoder(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        self.decode.clear();

        let bytes_used = self.read_weights(source)?;
        self.build_table_from_weights()?;
        Ok(bytes_used)
    }

    /// Read weights from the provided source.
    ///
    /// The huffman table is represented in the input data as a list of weights.
    /// After the header, weights are read, then a Huffman decoding table
    /// can be constructed using that list of weights.
    ///
    /// Returns the number of bytes read.
    fn read_weights(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        use HuffmanTableError as err;

        if source.is_empty() {
            return Err(err::SourceIsEmpty);
        }
        let header = source[0];
        let mut bits_read = 8;

        match header {
            // If the header byte is less than 128, the series of weights
            // is compressed using two interleaved FSE streams that share
            // a distribution table.
            0..=127 => {
                let fse_stream = &source[1..];
                if header as usize > fse_stream.len() {
                    return Err(err::NotEnoughBytesForWeights {
                        got_bytes: fse_stream.len(),
                        expected_bytes: header,
                    });
                }
                //fse decompress weights
                let bytes_used_by_fse_header = self.fse_table.build_decoder(fse_stream, 6)?;

                if bytes_used_by_fse_header > header as usize {
                    return Err(err::FSETableUsedTooManyBytes {
                        used: bytes_used_by_fse_header,
                        available_bytes: header,
                    });
                }

                vprintln!(
                    "Building fse table for huffman weights used: {}",
                    bytes_used_by_fse_header
                );
                // Huffman headers are compressed using two interleaved
                // FSE bitstreams, where the first state (decoder) handles
                // even symbols, and the second handles odd symbols.
                let mut dec1 = FSEDecoder::new(&self.fse_table);
                let mut dec2 = FSEDecoder::new(&self.fse_table);

                let compressed_start = bytes_used_by_fse_header;
                let compressed_length = header as usize - bytes_used_by_fse_header;

                let compressed_weights = &fse_stream[compressed_start..];
                if compressed_weights.len() < compressed_length {
                    return Err(err::NotEnoughBytesToDecompressWeights {
                        have: compressed_weights.len(),
                        need: compressed_length,
                    });
                }
                let compressed_weights = &compressed_weights[..compressed_length];
                let mut br = BitReaderReversed::new(compressed_weights);

                bits_read += (bytes_used_by_fse_header + compressed_length) * 8;

                //skip the 0 padding at the end of the last byte of the bit stream and throw away the first 1 found
                let mut skipped_bits = 0;
                loop {
                    let val = br.get_bits(1);
                    skipped_bits += 1;
                    if val == 1 || skipped_bits > 8 {
                        break;
                    }
                }
                if skipped_bits > 8 {
                    //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
                    return Err(err::ExtraPadding { skipped_bits });
                }

                dec1.init_state(&mut br)?;
                dec2.init_state(&mut br)?;

                self.weights.clear();

                // The two decoders take turns decoding a single symbol and updating their state.
                loop {
                    let w = dec1.decode_symbol();
                    self.weights.push(w);
                    dec1.update_state(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec2.decode_symbol());
                        break;
                    }

                    let w = dec2.decode_symbol();
                    self.weights.push(w);
                    dec2.update_state(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec1.decode_symbol());
                        break;
                    }
                    //maximum number of weights is 255 because we use u8 symbols and the last weight is inferred from the sum of all others
                    if self.weights.len() > 255 {
                        return Err(err::TooManyWeights {
                            got: self.weights.len(),
                        });
                    }
                }
            }
            // If the header byte is greater than or equal to 128,
            // weights are directly represented, where each weight is
            // encoded directly as a 4 bit field. The weights will
            // always be encoded with full bytes, meaning if there's
            // an odd number of weights, the last weight will still
            // occupy a full byte.
            _ => {
                // weights are directly encoded
                let weights_raw = &source[1..];
                let num_weights = header - 127;
                self.weights.resize(num_weights as usize, 0);

                let bytes_needed = if num_weights.is_multiple_of(2) {
                    num_weights as usize / 2
                } else {
                    (num_weights as usize / 2) + 1
                };

                if weights_raw.len() < bytes_needed {
                    return Err(err::NotEnoughBytesInSource {
                        got: weights_raw.len(),
                        need: bytes_needed,
                    });
                }

                for idx in 0..num_weights {
                    if idx % 2 == 0 {
                        self.weights[idx as usize] = weights_raw[idx as usize / 2] >> 4;
                    } else {
                        self.weights[idx as usize] = weights_raw[idx as usize / 2] & 0xF;
                    }
                    bits_read += 4;
                }
            }
        }

        let bytes_read = if bits_read % 8 == 0 {
            bits_read / 8
        } else {
            (bits_read / 8) + 1
        };
        Ok(bytes_read as u32)
    }

    /// Once the weights have been read from the data, you can decode the weights
    /// into a table, and use that table to decode the actual compressed data.
    ///
    /// This function populates the rest of the table from the series of weights.
    fn build_table_from_weights(&mut self) -> Result<(), HuffmanTableError> {
        use HuffmanTableError as err;

        self.bits.clear();
        self.bits.resize(self.weights.len() + 1, 0);

        let mut weight_sum: u32 = 0;
        for w in &self.weights {
            if *w > MAX_MAX_NUM_BITS {
                return Err(err::WeightBiggerThanMaxNumBits { got: *w });
            }
            weight_sum += if *w > 0 { 1_u32 << (*w - 1) } else { 0 };
        }

        if weight_sum == 0 {
            return Err(err::MissingWeights);
        }

        let max_bits = highest_bit_set(weight_sum) as u8;
        let left_over = (1 << max_bits) - weight_sum;

        //left_over must be power of two
        if !left_over.is_power_of_two() {
            return Err(err::LeftoverIsNotAPowerOf2 { got: left_over });
        }

        let last_weight = highest_bit_set(left_over) as u8;

        for symbol in 0..self.weights.len() {
            let bits = if self.weights[symbol] > 0 {
                max_bits + 1 - self.weights[symbol]
            } else {
                0
            };
            self.bits[symbol] = bits;
        }

        self.bits[self.weights.len()] = max_bits + 1 - last_weight;
        self.max_num_bits = max_bits;
        self.state_mask = (1_u64 << max_bits) - 1;

        if max_bits > MAX_MAX_NUM_BITS {
            return Err(err::MaxBitsTooHigh { got: max_bits });
        }

        self.bit_ranks.clear();
        self.bit_ranks.resize((max_bits + 1) as usize, 0);
        for num_bits in &self.bits {
            self.bit_ranks[(*num_bits) as usize] += 1;
        }

        //fill with dummy symbols
        self.decode.resize(
            1 << self.max_num_bits,
            Entry {
                symbol: 0,
                num_bits: 0,
            },
        );
        self.packed_decode.resize(1 << self.max_num_bits, 0);

        //starting codes for each rank
        self.rank_indexes.clear();
        self.rank_indexes.resize((max_bits + 1) as usize, 0);

        self.rank_indexes[max_bits as usize] = 0;
        for bits in (1..self.rank_indexes.len() as u8).rev() {
            self.rank_indexes[bits as usize - 1] = self.rank_indexes[bits as usize]
                + self.bit_ranks[bits as usize] as usize * (1 << (max_bits - bits));
        }

        assert!(
            self.rank_indexes[0] == self.decode.len(),
            "rank_idx[0]: {} should be: {}",
            self.rank_indexes[0],
            self.decode.len()
        );

        for symbol in 0..self.bits.len() {
            let bits_for_symbol = self.bits[symbol];
            if bits_for_symbol != 0 {
                // allocate code for the symbol and set in the table
                // a code ignores all max_bits - bits[symbol] bits, so it gets
                // a range that spans all of those in the decoding table
                let base_idx = self.rank_indexes[bits_for_symbol as usize];
                let len = 1 << (max_bits - bits_for_symbol);
                self.rank_indexes[bits_for_symbol as usize] += len;
                let entry = Entry {
                    symbol: symbol as u8,
                    num_bits: bits_for_symbol,
                };
                self.decode[base_idx..base_idx + len].fill(entry);
                let packed = u32::from(entry.symbol) | (u32::from(entry.num_bits) << 8);
                self.packed_decode[base_idx..base_idx + len].fill(packed);
            }
        }

        Ok(())
    }
}

impl Default for HuffmanTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A single entry in the table contains the decoded symbol/literal and the
/// size of the prefix code.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    /// The byte that the prefix code replaces during encoding.
    symbol: u8,
    /// The number of bits the prefix code occupies.
    num_bits: u8,
}

/// Assert that the provided value is greater than zero, and returns the
/// 32 - the number of leading zeros
fn highest_bit_set(x: u32) -> u32 {
    assert!(x > 0);
    u32::BITS - x.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn test_table() -> HuffmanTable {
        let decode = vec![
            Entry {
                symbol: b'A',
                num_bits: 1,
            },
            Entry {
                symbol: b'B',
                num_bits: 2,
            },
            Entry {
                symbol: b'C',
                num_bits: 1,
            },
            Entry {
                symbol: b'D',
                num_bits: 2,
            },
        ];
        let packed_decode = decode
            .iter()
            .map(|e| u32::from(e.symbol) | (u32::from(e.num_bits) << 8))
            .collect::<Vec<_>>();

        HuffmanTable {
            decode,
            packed_decode,
            weights: Vec::new(),
            max_num_bits: 2,
            state_mask: 0b11,
            bits: Vec::new(),
            bit_ranks: Vec::new(),
            rank_indexes: Vec::new(),
            fse_table: FSETable::new(255),
        }
    }

    #[test]
    fn decode_symbol_and_advance_scalar_matches_manual_transition() {
        let table = test_table();
        let initial_state = 1_u64;
        let entry = table.decode[initial_state as usize];
        let mut manual_br = BitReaderReversed::new(&[0b10101010, 0b01010101]);
        let expected_new_bits = manual_br.get_bits(entry.num_bits);
        let expected_state =
            ((initial_state << entry.num_bits) & table.state_mask) | expected_new_bits;

        let mut decoder = HuffmanDecoder {
            table: &table,
            kernel: HuffmanDecodeKernel::Scalar,
            state: initial_state,
        };
        let mut br = BitReaderReversed::new(&[0b10101010, 0b01010101]);
        let symbol = decoder.decode_symbol_and_advance(&mut br);

        assert_eq!(symbol, entry.symbol);
        assert_eq!(decoder.state, expected_state);
    }

    #[test]
    fn decode4_scalar_reads_symbols_and_num_bits_from_each_state() {
        let table = test_table();
        let decoders = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 3,
            },
        ];

        let (symbols, bits) = HuffmanDecoder::decode4_symbols_and_num_bits(&decoders);
        assert_eq!(symbols, [b'A', b'B', b'C', b'D']);
        assert_eq!(bits, [1, 2, 1, 2]);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn bmi2_advance_matches_scalar_formula_when_available() {
        if !is_x86_feature_detected!("bmi2") {
            return;
        }

        let table = test_table();
        let decoder = HuffmanDecoder {
            table: &table,
            kernel: HuffmanDecodeKernel::X86Bmi2,
            state: 3,
        };

        let num_bits = 2_u8;
        let new_bits = 1_u64;
        let expected = ((decoder.state << num_bits) & table.state_mask) | new_bits;
        let actual = unsafe { decoder.advance_state_x86_bmi2(num_bits, new_bits) };
        assert_eq!(actual, expected);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn decode4_avx2_matches_scalar_when_available() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }

        let table = test_table();
        let scalar = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 3,
            },
        ];
        let avx2 = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 3,
            },
        ];

        let expected = HuffmanDecoder::decode4_symbols_and_num_bits(&scalar);
        let actual = HuffmanDecoder::decode4_symbols_and_num_bits(&avx2);
        assert_eq!(actual, expected);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn decode4_vbmi2_matches_scalar_when_available() {
        if !is_x86_feature_detected!("avx512vbmi2")
            || !is_x86_feature_detected!("avx512f")
            || !is_x86_feature_detected!("avx512vl")
            || !is_x86_feature_detected!("avx512bw")
        {
            return;
        }

        let table = test_table();
        let scalar = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 3,
            },
        ];
        let vbmi2 = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Vbmi2,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Vbmi2,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Vbmi2,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Vbmi2,
                state: 3,
            },
        ];

        let expected = HuffmanDecoder::decode4_symbols_and_num_bits(&scalar);
        let actual = HuffmanDecoder::decode4_symbols_and_num_bits(&vbmi2);
        assert_eq!(actual, expected);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn decode4_mixed_tables_falls_back_in_release() {
        let table_a = test_table();
        let mut table_b = test_table();
        table_b.decode[0] = Entry {
            symbol: b'Z',
            num_bits: 2,
        };
        table_b.packed_decode[0] = u32::from(b'Z') | (u32::from(2_u8) << 8);

        let mixed = [
            HuffmanDecoder {
                table: &table_a,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 0,
            },
            HuffmanDecoder {
                table: &table_b,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 0,
            },
            HuffmanDecoder {
                table: &table_a,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 1,
            },
            HuffmanDecoder {
                table: &table_b,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 1,
            },
        ];

        #[cfg(debug_assertions)]
        {
            let panicked = std::panic::catch_unwind(|| {
                let _ = HuffmanDecoder::decode4_symbols_and_num_bits(&mixed);
            });
            assert!(panicked.is_err());
        }

        #[cfg(not(debug_assertions))]
        {
            let (symbols, bits) = HuffmanDecoder::decode4_symbols_and_num_bits(&mixed);
            assert_eq!(symbols, [b'A', b'Z', b'B', b'B']);
            assert_eq!(bits, [1, 2, 2, 2]);
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn decode4_mixed_kernels_falls_back_in_release() {
        let table = test_table();
        let mixed = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::X86Avx2,
                state: 3,
            },
        ];

        #[cfg(debug_assertions)]
        {
            let panicked = std::panic::catch_unwind(|| {
                let _ = HuffmanDecoder::decode4_symbols_and_num_bits(&mixed);
            });
            assert!(panicked.is_err());
        }

        #[cfg(not(debug_assertions))]
        {
            let (symbols, bits) = HuffmanDecoder::decode4_symbols_and_num_bits(&mixed);
            assert_eq!(symbols, [b'A', b'B', b'C', b'D']);
            assert_eq!(bits, [1, 2, 1, 2]);
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn decode4_neon_matches_scalar_when_available() {
        if !is_aarch64_feature_detected!("neon") {
            return;
        }

        let table = test_table();
        let scalar = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 3,
            },
        ];
        let neon = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Neon,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Neon,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Neon,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Neon,
                state: 3,
            },
        ];

        let expected = HuffmanDecoder::decode4_symbols_and_num_bits(&scalar);
        let actual = HuffmanDecoder::decode4_symbols_and_num_bits(&neon);
        assert_eq!(actual, expected);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn decode4_sve_matches_scalar_when_available() {
        if !is_aarch64_feature_detected!("sve") {
            return;
        }

        let table = test_table();
        let scalar = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Scalar,
                state: 3,
            },
        ];
        let sve = [
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Sve,
                state: 0,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Sve,
                state: 1,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Sve,
                state: 2,
            },
            HuffmanDecoder {
                table: &table,
                kernel: HuffmanDecodeKernel::Aarch64Sve,
                state: 3,
            },
        ];

        let expected = HuffmanDecoder::decode4_symbols_and_num_bits(&scalar);
        let actual = HuffmanDecoder::decode4_symbols_and_num_bits(&sve);
        assert_eq!(actual, expected);
    }
}
