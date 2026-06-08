//! Utilities for decoding Huff0 encoded huffman data.

use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::HuffmanTableError;
use crate::fse::{FSEDecoder, FSETable};
use alloc::vec::Vec;
#[cfg(target_arch = "x86")]
use core::arch::x86::_bzhi_u32;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::_bzhi_u64;
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

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
const fn select_x86_huffman_decode_kernel(
    has_avx512vbmi2: bool,
    has_avx512f: bool,
    has_avx512vl: bool,
    has_avx512bw: bool,
    has_bmi2: bool,
    has_avx2: bool,
) -> HuffmanDecodeKernel {
    if has_avx512vbmi2 && has_avx512f && has_avx512vl && has_avx512bw && has_bmi2 {
        return HuffmanDecodeKernel::X86Vbmi2;
    }
    if has_avx2 && has_bmi2 {
        return HuffmanDecodeKernel::X86Avx2;
    }
    if has_bmi2 {
        return HuffmanDecodeKernel::X86Bmi2;
    }
    HuffmanDecodeKernel::Scalar
}

#[cfg(feature = "std")]
#[inline(always)]
pub(crate) fn detect_huffman_decode_kernel() -> HuffmanDecodeKernel {
    static KERNEL: OnceLock<HuffmanDecodeKernel> = OnceLock::new();
    *KERNEL.get_or_init(|| {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            let kernel = select_x86_huffman_decode_kernel(
                is_x86_feature_detected!("avx512vbmi2"),
                is_x86_feature_detected!("avx512f"),
                is_x86_feature_detected!("avx512vl"),
                is_x86_feature_detected!("avx512bw"),
                is_x86_feature_detected!("bmi2"),
                is_x86_feature_detected!("avx2"),
            );
            if kernel != HuffmanDecodeKernel::Scalar {
                return kernel;
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
        let kernel = select_x86_huffman_decode_kernel(
            cfg!(target_feature = "avx512vbmi2"),
            cfg!(target_feature = "avx512f"),
            cfg!(target_feature = "avx512vl"),
            cfg!(target_feature = "avx512bw"),
            cfg!(target_feature = "bmi2"),
            cfg!(target_feature = "avx2"),
        );
        if kernel != HuffmanDecodeKernel::Scalar {
            return kernel;
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
    /// Read by `decode_symbol_and_advance` on x86 to pick between the
    /// scalar and BMI2 single-symbol decode bodies (single-stream tail
    /// loop after the 4-stream burst). On aarch64 and portable targets
    /// the BMI2 arm doesn't exist and the field is unread — the
    /// 4-stream SIMD-fallback path that previously consumed this
    /// field now dispatches via the [`HufKernel`] trait at
    /// `decompress_literals` entry instead.
    #[cfg_attr(
        not(any(target_arch = "x86", target_arch = "x86_64")),
        allow(dead_code)
    )]
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
    #[cfg(feature = "fuzz_exports")]
    #[inline(always)]
    fn decode_symbol(&mut self) -> u8 {
        self.table.packed_decode[self.state as usize] as u8
    }

    /// Fuzz-only shim for reading the symbol at the current state.
    #[cfg(feature = "fuzz_exports")]
    #[inline(always)]
    pub fn fuzz_decode_symbol(&mut self) -> u8 {
        self.decode_symbol()
    }

    /// Initialize internal state and prepare to decode data. Then
    /// `decode_symbol_and_advance` can be used for full decode steps.
    /// The 4-stream batched fallback path used by
    /// `literals_section_decoder` lives in the [`HufKernel`] trait
    /// impls (`decode4_unchecked` + `advance_state`) and is selected
    /// once via `match detect_huffman_decode_kernel() { ... }`.
    #[inline(always)]
    pub fn init_state<K: crate::cpu_kernel::CpuKernel>(
        &mut self,
        br: &mut BitReaderReversed<'_, K>,
    ) -> u8 {
        let num_bits = self.table.max_num_bits;
        let new_bits = br.get_bits(num_bits);
        self.state = new_bits;
        num_bits
    }

    /// Advance the internal cursor to the next symbol. After this, you can call `decode_symbol`
    /// to read from the new position.
    #[cfg(feature = "fuzz_exports")]
    #[inline(always)]
    fn next_state<K: crate::cpu_kernel::CpuKernel>(
        &mut self,
        br: &mut BitReaderReversed<'_, K>,
    ) -> u8 {
        // self.state stores a small section, or a window of the bit stream. The table can be indexed via this state,
        // telling you how many bits identify the current symbol.
        let num_bits = (self.table.packed_decode[self.state as usize] >> 8) as u8;
        // New bits are read from the stream
        let new_bits = br.get_bits(num_bits);
        // Shift and mask out the bits that identify the current symbol
        self.state = ((self.state << num_bits) & self.table.state_mask) | new_bits;
        num_bits
    }

    /// Fuzz-only shim for advancing to the next decoding state.
    #[cfg(feature = "fuzz_exports")]
    #[inline(always)]
    pub fn fuzz_next_state<K: crate::cpu_kernel::CpuKernel>(
        &mut self,
        br: &mut BitReaderReversed<'_, K>,
    ) -> u8 {
        self.next_state(br)
    }

    /// Decode symbol and advance state in one table lookup.
    #[inline(always)]
    pub fn decode_symbol_and_advance<K: crate::cpu_kernel::CpuKernel>(
        &mut self,
        br: &mut BitReaderReversed<'_, K>,
    ) -> u8 {
        // On x86 the BMI2 kernel uses `_bzhi_u64` and is a real
        // perf win over the scalar `((state << n) & mask) | new_bits`
        // sequence, so the runtime match is load-bearing. On aarch64
        // both NEON and SVE arms previously aliased the scalar body
        // verbatim — the match was paying a 3-arm dispatch cost for
        // zero benefit. Collapsed to a direct scalar call there.
        // The enum's Aarch64Neon / Aarch64Sve variants are themselves
        // cfg-gated to target_arch = "aarch64", so under the outer
        // x86 cfg below they don't exist — the match here is
        // exhaustive on Scalar + X86Bmi2/Avx2/Vbmi2 alone, and an
        // inner `cfg(target_arch = "aarch64")` arm would be dead
        // (outer x86 cfg already false on aarch64).
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            match self.kernel {
                HuffmanDecodeKernel::Scalar => self.decode_symbol_and_advance_scalar(br),
                HuffmanDecodeKernel::X86Bmi2
                | HuffmanDecodeKernel::X86Avx2
                | HuffmanDecodeKernel::X86Vbmi2 => {
                    // SAFETY: This path is selected only after runtime/static feature checks.
                    unsafe { self.decode_symbol_and_advance_x86_bmi2(br) }
                }
            }
        }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        {
            // aarch64 and portable targets: the X86* arms compile out
            // entirely, so the match would collapse to a single arm.
            // Bypass the match and call scalar directly — both
            // Aarch64Neon and Aarch64Sve specialisations were
            // verbatim clones of the scalar body (they were dropped
            // in an earlier commit), and no NEON/SVE intrinsics
            // exist for the single-symbol decode shape.
            self.decode_symbol_and_advance_scalar(br)
        }
    }

    #[inline(always)]
    fn decode_symbol_and_advance_scalar<K: crate::cpu_kernel::CpuKernel>(
        &mut self,
        br: &mut BitReaderReversed<'_, K>,
    ) -> u8 {
        let packed = self.table.packed_decode[self.state as usize];
        let num_bits = (packed >> 8) as u8;
        let new_bits = br.get_bits(num_bits);
        self.state = ((self.state << num_bits) & self.table.state_mask) | new_bits;
        packed as u8
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "bmi2")]
    unsafe fn decode_symbol_and_advance_x86_bmi2<K: crate::cpu_kernel::CpuKernel>(
        &mut self,
        br: &mut BitReaderReversed<'_, K>,
    ) -> u8 {
        let packed = self.table.packed_decode[self.state as usize];
        let num_bits = (packed >> 8) as u8;
        let new_bits = br.get_bits(num_bits);
        self.state = unsafe { self.advance_state_x86_bmi2(num_bits, new_bits) };
        packed as u8
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
            u64::from(_bzhi_u32(shifted, u32::from(self.table.max_num_bits))) | new_bits
        }
    }

    // aarch64 NEON / SVE kernels for `decode_symbol_and_advance` were
    // identical clones of the scalar body — no NEON/SVE intrinsics
    // were ever in use here (the SIMD kernels live in `decode4_*`
    // below, where they actually batch four streams). The aarch64
    // arm of `decode_symbol_and_advance` now calls
    // `decode_symbol_and_advance_scalar` directly; keeping the
    // duplicate functions around just so the match could enumerate
    // them was dead code.
}

/// A Huffman decoding table contains a list of Huffman prefix codes and their associated values
pub struct HuffmanTable {
    /// Packed `symbol | (num_bits << 8)` per state index, exposed
    /// `pub(crate)` because the HUF 4-stream burst hot path in
    /// `literals_section_decoder::decode_literals` indexes it directly
    /// (`packed_decode[idx]`) for a single-load table lookup matching
    /// donor `huf_decompress.c:dtable[index]`. This is the primary
    /// (and only) 4-stream decode lookup table since the previous
    /// SIMD-fallback dispatch was removed in favour of donor's
    /// always-firing burst.
    ///
    /// **`u16` (matches donor `HUF_DEltX1` layout exactly).** Donor's
    /// `dtable[index]` returns a 2-byte entry — low byte is `symbol`,
    /// high byte is `nbBits`. We mirror that representation so the
    /// table size is `2 × (1 << max_num_bits)` bytes instead of `4 ×`.
    /// At `max_num_bits = 11` (zstd spec ceiling) that's 4 KiB vs the
    /// older 8 KiB representation — halves L1d footprint on the hot
    /// HUF decode path. Worth the change because the L-7 Fast workload
    /// (and any small-alphabet poorly-compressed input) decodes most
    /// output bytes through this table; literal-buffer pressure on
    /// L1d makes the cache hit rate sensitive to table footprint.
    pub(crate) packed_decode: Vec<u16>,
    /// The weight of a symbol is the number of occurrences in a table.
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
        // Copy ONLY the decode-time state. `weights` / `bits` /
        // `rank_indexes` and the weight-decoding `fse_table` are build-time
        // scratch (repopulated when a block carries a new HUF table); the
        // literal-decode hot path reads only `packed_decode` + `max_num_bits`
        // + `state_mask`. Skipping the rest mirrors the donor copying just the
        // HUF decode table per frame, not the full build workspace.
        self.packed_decode.extend_from_slice(&other.packed_decode);
        self.max_num_bits = other.max_num_bits;
        self.state_mask = other.state_mask;
    }

    /// Completely empty the table of all data.
    pub fn reset(&mut self) {
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
        self.packed_decode.clear();

        let bytes_used = self.read_weights(source)?;
        self.build_table_from_weights()?;
        Ok(bytes_used)
    }

    /// Parse the weight header into `bits` + `max_num_bits` WITHOUT building
    /// the decode lookup table. Returns the same byte count as
    /// [`Self::build_decoder`] (the weight-header length), so a caller
    /// stepping a cursor over packed tables advances identically. Used by
    /// the encoder dictionary load: [`Self::to_encoder_table`] reads only
    /// `bits` + `max_num_bits`, so filling `packed_decode` is pure waste.
    ///
    /// Crate-internal: the table it produces is intentionally non-decodable
    /// (empty `packed_decode`); only `decode_dict_for_encoding` calls it, and
    /// its result is wrapped in an `EncoderDictionary` that has no decode path.
    pub(crate) fn build_weights_only(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        self.packed_decode.clear();

        let bytes_used = self.read_weights(source)?;
        self.compute_huffman_bits()?;
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
                let mut br =
                    BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(compressed_weights);

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

                // The weights FSE table is built with a max accuracy_log of 6
                // (the `build_decoder(fse_stream, 6)` call above), so each state
                // update reads at most 6 bits. Refilling once per loop step
                // (two interleaved updates = up to 12 bits) lets both updates
                // use the unchecked fast advance instead of a per-symbol refill
                // branch, mirroring the donor's single reload per decode step.
                // `bits_remaining()` still tracks end-of-stream via `extra_bits`
                // (maintained by `refill_slow`), so the termination checks below
                // fire identically.
                const WEIGHTS_REFILL_BUDGET: u8 = 12;

                // The two decoders take turns decoding a single symbol and updating their state.
                loop {
                    br.ensure_bits(WEIGHTS_REFILL_BUDGET);

                    let w = dec1.decode_symbol();
                    self.weights.push(w);
                    dec1.update_state_fast(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec2.decode_symbol());
                        break;
                    }

                    let w = dec2.decode_symbol();
                    self.weights.push(w);
                    dec2.update_state_fast(&mut br);

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

    /// Compute per-symbol code lengths (`bits`) + `max_num_bits` from the
    /// parsed `weights`, returning `max_bits`. This is the slice of the
    /// table build the ENCODER side needs: [`Self::to_encoder_table`] reads
    /// only `bits` + `max_num_bits`. The decode lookup-table fill
    /// (`bit_ranks` / `packed_decode` / `rank_indexes`) is decoder-only and
    /// lives in [`Self::build_table_from_weights`].
    fn compute_huffman_bits(&mut self) -> Result<u8, HuffmanTableError> {
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

        Ok(max_bits)
    }

    fn build_table_from_weights(&mut self) -> Result<(), HuffmanTableError> {
        let max_bits = self.compute_huffman_bits()?;

        self.bit_ranks.clear();
        self.bit_ranks.resize((max_bits + 1) as usize, 0);
        for num_bits in &self.bits {
            self.bit_ranks[(*num_bits) as usize] += 1;
        }

        //fill with dummy symbols
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
            self.rank_indexes[0] == self.packed_decode.len(),
            "rank_idx[0]: {} should be: {}",
            self.rank_indexes[0],
            self.packed_decode.len()
        );

        for symbol in 0..self.bits.len() {
            let bits_for_symbol = self.bits[symbol];
            if bits_for_symbol != 0 {
                // allocate code for the symbol and set in the table
                // a code ignores all max_bits - bits[symbol] bits, so it gets
                // a range that spans all of those in the decoding table.
                // Donor `HUF_DEltX1` packing: low byte = symbol, high
                // byte = nbBits. `num_bits ≤ 11` always fits in the
                // upper byte alongside an 8-bit symbol.
                let base_idx = self.rank_indexes[bits_for_symbol as usize];
                let len = 1 << (max_bits - bits_for_symbol);
                self.rank_indexes[bits_for_symbol as usize] += len;
                let packed = u16::from(symbol as u8) | (u16::from(bits_for_symbol) << 8);
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
        // Packed `symbol | (num_bits << 8)` per state index (donor `HUF_DEltX1`).
        let packed_decode = vec![
            u16::from(b'A') | (1u16 << 8),
            u16::from(b'B') | (2u16 << 8),
            u16::from(b'C') | (1u16 << 8),
            u16::from(b'D') | (2u16 << 8),
        ];

        HuffmanTable {
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
        let packed = table.packed_decode[initial_state as usize];
        let entry_num_bits = (packed >> 8) as u8;
        let entry_symbol = packed as u8;
        let mut manual_br =
            BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&[0b10101010, 0b01010101]);
        let expected_new_bits = manual_br.get_bits(entry_num_bits);
        let expected_state =
            ((initial_state << entry_num_bits) & table.state_mask) | expected_new_bits;

        let mut decoder = HuffmanDecoder {
            table: &table,
            kernel: HuffmanDecodeKernel::Scalar,
            state: initial_state,
        };
        let mut br =
            BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&[0b10101010, 0b01010101]);
        let symbol = decoder.decode_symbol_and_advance(&mut br);

        assert_eq!(symbol, entry_symbol);
        assert_eq!(decoder.state, expected_state);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn select_x86_kernel_ordering_is_stable() {
        assert_eq!(
            select_x86_huffman_decode_kernel(true, true, true, true, true, true),
            HuffmanDecodeKernel::X86Vbmi2
        );
        assert_eq!(
            select_x86_huffman_decode_kernel(false, false, false, false, true, true),
            HuffmanDecodeKernel::X86Avx2
        );
        assert_eq!(
            select_x86_huffman_decode_kernel(false, false, false, false, true, false),
            HuffmanDecodeKernel::X86Bmi2
        );
        assert_eq!(
            select_x86_huffman_decode_kernel(false, false, false, false, false, true),
            HuffmanDecodeKernel::Scalar
        );
    }
}
