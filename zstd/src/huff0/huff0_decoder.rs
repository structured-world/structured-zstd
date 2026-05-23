//! Utilities for decoding Huff0 encoded huffman data.

use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::HuffmanTableError;
use crate::fse::{FSEDecoder, FSETable};
use alloc::vec::Vec;
#[cfg(target_arch = "x86")]
use core::arch::x86::{_bzhi_u32, _mm_cvtsi128_si32, _mm_maskz_compress_epi8, _mm_set_epi32};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{_bzhi_u64, _mm_cvtsi128_si32, _mm_maskz_compress_epi8, _mm_set_epi32};
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
        self.table.decode[self.state as usize].symbol
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
    pub fn init_state(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        let num_bits = self.table.max_num_bits;
        let new_bits = br.get_bits(num_bits);
        self.state = new_bits;
        num_bits
    }

    /// Advance the internal cursor to the next symbol. After this, you can call `decode_symbol`
    /// to read from the new position.
    #[cfg(feature = "fuzz_exports")]
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
    pub(crate) fn decode_symbol_and_num_bits(&self) -> (u8, u8) {
        let entry = self.table.decode[self.state as usize];
        (entry.symbol, entry.num_bits)
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
    #[inline]
    unsafe fn decode4_symbols_and_num_bits_vbmi2(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        let table = decoders[0].table;
        // Pack four u16 entries into a single u64. `compress_epi8`
        // below operates on bytes regardless of element width, so a
        // 64-bit register holding 4 × {symbol, nbBits} byte pairs is
        // enough. No SIMD gather — scalar u16 loads through OOO load
        // ports beat any small-batch gather on consumer x86.
        let lane0 = u64::from(table.packed_decode[decoders[0].state as usize]);
        let lane1 = u64::from(table.packed_decode[decoders[1].state as usize]);
        let lane2 = u64::from(table.packed_decode[decoders[2].state as usize]);
        let lane3 = u64::from(table.packed_decode[decoders[3].state as usize]);
        let packed_u64 = lane0 | (lane1 << 16) | (lane2 << 32) | (lane3 << 48);
        // Unpack the four u16 entries into four 32-bit lanes of an
        // xmm reg. Each lane holds `{symbol, nbBits}` in its low two
        // bytes — the upper two bytes of each lane are zero. This
        // preserves the per-lane byte layout the `_mm_maskz_compress_epi8`
        // masks below depend on (one symbol byte and one nbBits byte
        // per 32-bit lane); a packed u64-in-low-64-bits shape would
        // collapse the lanes and break the mask patterns.
        let packed = _mm_set_epi32(
            (packed_u64 >> 48) as i32 & 0xFFFF,
            ((packed_u64 >> 32) & 0xFFFF) as i32,
            ((packed_u64 >> 16) & 0xFFFF) as i32,
            (packed_u64 & 0xFFFF) as i32,
        );

        // Keep byte0 and byte1 from each u32 lane, then compress them to the low bytes.
        let symbols_bytes = _mm_maskz_compress_epi8(0b0001_0001_0001_0001, packed);
        let bits_bytes = _mm_maskz_compress_epi8(0b0010_0010_0010_0010, packed);

        let symbols_word = _mm_cvtsi128_si32(symbols_bytes) as u32;
        let bits_word = _mm_cvtsi128_si32(bits_bytes) as u32;
        let symbols = symbols_word.to_le_bytes();
        let bits = bits_word.to_le_bytes();
        (
            [symbols[0], symbols[1], symbols[2], symbols[3]],
            [bits[0], bits[1], bits[2], bits[3]],
        )
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "avx2")]
    #[inline]
    unsafe fn decode4_symbols_and_num_bits_avx2(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        // Donor parity: scalar u16 lookups, no SIMD gather. Upstream
        // `huf_decompress.c` decodes via `dtable[index]` scalar loads
        // in `HUF_4X1_DECODE_SYMBOL`, never a gather instruction.
        // Modern CPUs execute 4 scalar u16 loads in parallel through
        // multiple load ports (3-5 cycles via OOO) — strictly faster
        // than any 4-element SIMD gather. The pre-fix AVX2 path
        // additionally chose a 4-byte gather stride against the
        // `Vec<u32>` table layout; the table is now `Vec<u16>` and
        // there is no advantage in introducing a 2-byte gather.
        let table = decoders[0].table;
        let entry0 = table.packed_decode[decoders[0].state as usize];
        let entry1 = table.packed_decode[decoders[1].state as usize];
        let entry2 = table.packed_decode[decoders[2].state as usize];
        let entry3 = table.packed_decode[decoders[3].state as usize];
        (
            [
                (entry0 & 0xFF) as u8,
                (entry1 & 0xFF) as u8,
                (entry2 & 0xFF) as u8,
                (entry3 & 0xFF) as u8,
            ],
            [
                ((entry0 >> 8) & 0xFF) as u8,
                ((entry1 >> 8) & 0xFF) as u8,
                ((entry2 >> 8) & 0xFF) as u8,
                ((entry3 >> 8) & 0xFF) as u8,
            ],
        )
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    #[inline]
    unsafe fn decode4_symbols_and_num_bits_neon(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        // Donor parity: scalar u16 loads + byte split. The previous
        // NEON shape loaded four `u32` entries with `vld1q_u32`, then
        // applied `vandq_u32` / `vshrq_n_u32` to extract the symbol /
        // num_bits byte from each lane. With `packed_decode: Vec<u16>`
        // the source no longer holds u32 lanes — and even if it did,
        // four scalar loads through M1's 4-way load pipeline beat the
        // SIMD round-trip on a 4-element batch.
        let table = decoders[0].table;
        let entry0 = table.packed_decode[decoders[0].state as usize];
        let entry1 = table.packed_decode[decoders[1].state as usize];
        let entry2 = table.packed_decode[decoders[2].state as usize];
        let entry3 = table.packed_decode[decoders[3].state as usize];
        (
            [
                (entry0 & 0xFF) as u8,
                (entry1 & 0xFF) as u8,
                (entry2 & 0xFF) as u8,
                (entry3 & 0xFF) as u8,
            ],
            [
                ((entry0 >> 8) & 0xFF) as u8,
                ((entry1 >> 8) & 0xFF) as u8,
                ((entry2 >> 8) & 0xFF) as u8,
                ((entry3 >> 8) & 0xFF) as u8,
            ],
        )
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "sve")]
    #[inline]
    unsafe fn decode4_symbols_and_num_bits_sve(
        decoders: &[HuffmanDecoder<'_>; 4],
    ) -> ([u8; 4], [u8; 4]) {
        // Donor parity: scalar u16 loads. See `decode4_symbols_and_num_bits_avx2`
        // for the rationale — the SVE inline-asm 4-lane unpack here
        // round-trips through a u32 buffer and gains nothing over
        // four 16-bit scalar loads. The `packed_decode` table is now
        // `Vec<u16>`, so the prior `ld1w` (32-bit lane load) wouldn't
        // even be type-correct.
        let table = decoders[0].table;
        let entry0 = table.packed_decode[decoders[0].state as usize];
        let entry1 = table.packed_decode[decoders[1].state as usize];
        let entry2 = table.packed_decode[decoders[2].state as usize];
        let entry3 = table.packed_decode[decoders[3].state as usize];
        (
            [
                (entry0 & 0xFF) as u8,
                (entry1 & 0xFF) as u8,
                (entry2 & 0xFF) as u8,
                (entry3 & 0xFF) as u8,
            ],
            [
                ((entry0 >> 8) & 0xFF) as u8,
                ((entry1 >> 8) & 0xFF) as u8,
                ((entry2 >> 8) & 0xFF) as u8,
                ((entry3 >> 8) & 0xFF) as u8,
            ],
        )
    }

    #[inline(always)]
    fn decode_symbol_and_advance_scalar(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        let entry = self.table.decode[self.state as usize];
        let new_bits = br.get_bits(entry.num_bits);
        self.state = ((self.state << entry.num_bits) & self.table.state_mask) | new_bits;
        entry.symbol
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

/// Compile-time-monomorphised kernel dispatch for the HUF 4-stream
/// SIMD-fallback decode tier in `literals_section_decoder`.
///
/// The fallback tier fires every post-refill iteration of the 4-stream
/// burst loop. Each iteration needs one 4-symbol decode (kernel-specific
/// 4-symbol gather: scalar / AVX2 / VBMI2 / NEON / SVE) and four state
/// advances (BMI2 `_bzhi_u64` on x86, scalar on aarch64). Before this
/// trait, each of those five calls did its own runtime `match self.kernel`
/// on the same process-wide constant choice. The `HufKernel` ZST + single
/// dispatch at `decompress_literals` entry lets the inner loop bake in
/// one kernel choice at compile time and skip the per-call branch
/// (5 runtime branches eliminated per fallback iteration).
///
/// Implementors are zero-sized marker types whose associated functions
/// directly call the kernel-specific helpers on `HuffmanDecoder`. The
/// runtime kernel selection happens once via
/// [`detect_huffman_decode_kernel`] and dispatches into the
/// monomorphised inner loop via `match kernel { ... }`.
///
/// # Safety
///
/// Each implementor's associated functions are only safe to call after
/// the running CPU has been verified to support the kernel's required
/// instruction set. `decompress_literals` performs this check exactly
/// once and dispatches accordingly; callers within the monomorphised
/// loop can assume the precondition.
pub(crate) trait HufKernel {
    /// Decode 4 symbols + their bit-widths, one per stream, sharing a
    /// single table read through `decoders[0].table`.
    ///
    /// # Safety
    /// All four decoders must reference the same table (verified at
    /// `decompress_literals` entry, holds by construction inside the
    /// monomorphised loop). The running CPU must support this
    /// kernel's feature set.
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]);

    /// Pull `num_bits` from `br` and fold into `decoder.state`.
    ///
    /// # Safety
    /// The running CPU must support this kernel's feature set
    /// (matters for `Bmi2Kernel` / `Avx2Kernel` / `Vbmi2Kernel`,
    /// which use `_bzhi_u64`).
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    );
}

/// Universal scalar fallback — no SIMD intrinsics, always safe to
/// call on any target.
pub(crate) struct ScalarKernel;

impl HufKernel for ScalarKernel {
    #[inline(always)]
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]) {
        HuffmanDecoder::decode4_symbols_and_num_bits_scalar(decoders)
    }

    #[inline(always)]
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    ) {
        let new_bits = br.get_bits(num_bits);
        decoder.state = ((decoder.state << num_bits) & decoder.table.state_mask) | new_bits;
    }
}

/// x86 BMI2 kernel — `_bzhi_u64`-based state advance, scalar decode4
/// (no AVX2 vector decode at this tier).
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) struct Bmi2Kernel;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl HufKernel for Bmi2Kernel {
    #[inline(always)]
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]) {
        HuffmanDecoder::decode4_symbols_and_num_bits_scalar(decoders)
    }

    #[inline(always)]
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    ) {
        let new_bits = br.get_bits(num_bits);
        // SAFETY: caller guarantees BMI2 availability (kernel dispatch
        // at `decompress_literals` entry).
        decoder.state = unsafe { decoder.advance_state_x86_bmi2(num_bits, new_bits) };
    }
}

/// x86 AVX2+BMI2 kernel — AVX2 4-symbol gather + BMI2 state advance.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) struct Avx2Kernel;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl HufKernel for Avx2Kernel {
    #[inline(always)]
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]) {
        // SAFETY: caller guarantees AVX2+BMI2 availability.
        unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_avx2(decoders) }
    }

    #[inline(always)]
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    ) {
        let new_bits = br.get_bits(num_bits);
        // SAFETY: AVX2 path implies BMI2 (kernel selector requires both).
        decoder.state = unsafe { decoder.advance_state_x86_bmi2(num_bits, new_bits) };
    }
}

/// x86 AVX-512 VBMI2+BMI2 kernel — VBMI2 4-symbol gather + BMI2 state.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) struct Vbmi2Kernel;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl HufKernel for Vbmi2Kernel {
    #[inline(always)]
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]) {
        // SAFETY: caller guarantees VBMI2 availability.
        unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_vbmi2(decoders) }
    }

    #[inline(always)]
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    ) {
        let new_bits = br.get_bits(num_bits);
        // SAFETY: VBMI2 path implies BMI2 (kernel selector requires both).
        decoder.state = unsafe { decoder.advance_state_x86_bmi2(num_bits, new_bits) };
    }
}

/// aarch64 NEON kernel — NEON 4-symbol gather + scalar state advance
/// (the NEON `decode_symbol_and_advance` variant was dropped in an
/// earlier commit; it was a verbatim clone of scalar).
#[cfg(target_arch = "aarch64")]
pub(crate) struct NeonKernel;

#[cfg(target_arch = "aarch64")]
impl HufKernel for NeonKernel {
    #[inline(always)]
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]) {
        // SAFETY: caller guarantees NEON availability.
        unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_neon(decoders) }
    }

    #[inline(always)]
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    ) {
        let new_bits = br.get_bits(num_bits);
        decoder.state = ((decoder.state << num_bits) & decoder.table.state_mask) | new_bits;
    }
}

/// aarch64 SVE kernel — SVE 4-symbol gather + scalar state advance.
#[cfg(target_arch = "aarch64")]
pub(crate) struct SveKernel;

#[cfg(target_arch = "aarch64")]
impl HufKernel for SveKernel {
    #[inline(always)]
    unsafe fn decode4_unchecked(decoders: &[HuffmanDecoder<'_>; 4]) -> ([u8; 4], [u8; 4]) {
        // SAFETY: caller guarantees SVE availability.
        unsafe { HuffmanDecoder::decode4_symbols_and_num_bits_sve(decoders) }
    }

    #[inline(always)]
    unsafe fn advance_state(
        decoder: &mut HuffmanDecoder<'_>,
        br: &mut BitReaderReversed<'_>,
        num_bits: u8,
    ) {
        let new_bits = br.get_bits(num_bits);
        decoder.state = ((decoder.state << num_bits) & decoder.table.state_mask) | new_bits;
    }
}

/// A Huffman decoding table contains a list of Huffman prefix codes and their associated values
pub struct HuffmanTable {
    decode: Vec<Entry>,
    /// Packed `symbol | (num_bits << 8)` per state index, exposed
    /// `pub(crate)` because the HUF 4-stream burst hot path in
    /// `literals_section_decoder::decode_literals` indexes it
    /// directly (`packed_decode[idx]`) for a single-load table lookup
    /// matching donor `huf_decompress.c:dtable[index]` — bypassing the
    /// kernel-dispatch path used by SIMD fallback.
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
                // Donor `HUF_DEltX1` packing: low byte = symbol, high
                // byte = nbBits. `num_bits ≤ 11` always fits in the
                // upper byte alongside an 8-bit symbol.
                let packed = u16::from(entry.symbol) | (u16::from(entry.num_bits) << 8);
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
            .map(|e| u16::from(e.symbol) | (u16::from(e.num_bits) << 8))
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
    fn scalar_kernel_advance_state_matches_formula() {
        let table = test_table();
        let initial_state = 2_u64;
        let num_bits = 2_u8;
        let mut manual_br = BitReaderReversed::new(&[0b00110110, 0b11110000]);
        let expected_new_bits = manual_br.get_bits(num_bits);
        let expected_state = ((initial_state << num_bits) & table.state_mask) | expected_new_bits;

        let mut decoder = HuffmanDecoder {
            table: &table,
            kernel: HuffmanDecodeKernel::Scalar,
            state: initial_state,
        };
        let mut br = BitReaderReversed::new(&[0b00110110, 0b11110000]);
        // SAFETY: ScalarKernel has no SIMD prereqs.
        unsafe { ScalarKernel::advance_state(&mut decoder, &mut br, num_bits) };

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

        // SAFETY: ScalarKernel has no SIMD prereqs.
        let (symbols, bits) = unsafe { ScalarKernel::decode4_unchecked(&decoders) };
        assert_eq!(symbols, [b'A', b'B', b'C', b'D']);
        assert_eq!(bits, [1, 2, 1, 2]);
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn bmi2_advance_matches_scalar_formula_when_available() {
        if !std::arch::is_x86_feature_detected!("bmi2") {
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

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn decode4_avx2_matches_scalar_when_available() {
        if !std::arch::is_x86_feature_detected!("avx2") {
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

        let expected = unsafe { ScalarKernel::decode4_unchecked(&scalar) };
        let actual = unsafe { Avx2Kernel::decode4_unchecked(&avx2) };
        assert_eq!(actual, expected);
    }

    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn decode4_vbmi2_matches_scalar_when_available() {
        if !(std::arch::is_x86_feature_detected!("avx512vbmi2")
            && std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512vl")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("bmi2"))
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

        let expected = unsafe { ScalarKernel::decode4_unchecked(&scalar) };
        let actual = unsafe { Vbmi2Kernel::decode4_unchecked(&vbmi2) };
        assert_eq!(actual, expected);
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

    // Mixed-table and mixed-kernel fallback tests removed: those exercised the
    // old dispatcher's defensive fallback when callers passed decoders with
    // different tables or kernels. The `HufKernel` trait API requires shared
    // table+kernel by precondition, established by construction at the
    // single dispatch site in `literals_section_decoder::decompress_literals`
    // (all four decoders are built from the same `&scratch.table`, and the
    // outer `match detect_huffman_decode_kernel()` picks one kernel per
    // call). The trait dispatch itself is compile-time monomorphisation,
    // but the precondition guarantee is structural, not statically checked.
    // The mixed-input shape is now a caller-side invariant violation, not a
    // tested-fallback behaviour.

    #[cfg(all(feature = "std", target_arch = "aarch64"))]
    #[test]
    fn decode4_neon_matches_scalar_when_available() {
        if !std::arch::is_aarch64_feature_detected!("neon") {
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

        // SAFETY: Scalar has no SIMD prereqs; NEON checked by feature-detect above.
        let expected = unsafe { ScalarKernel::decode4_unchecked(&scalar) };
        let actual = unsafe { NeonKernel::decode4_unchecked(&neon) };
        assert_eq!(actual, expected);
    }

    #[cfg(all(feature = "std", target_arch = "aarch64"))]
    #[test]
    fn decode4_sve_matches_scalar_when_available() {
        if !std::arch::is_aarch64_feature_detected!("sve") {
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

        // SAFETY: Scalar has no SIMD prereqs; SVE checked by feature-detect above.
        let expected = unsafe { ScalarKernel::decode4_unchecked(&scalar) };
        let actual = unsafe { SveKernel::decode4_unchecked(&sve) };
        assert_eq!(actual, expected);
    }
}
