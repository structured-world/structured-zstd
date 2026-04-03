use core::convert::TryInto;

/// Pre-computed mask table: `BIT_MASK[n]` equals the lower `n` bits set,
/// i.e. `(1u64 << n) - 1` for `n` in `0..=64`.
///
/// Using a lookup table instead of computing the mask on every call
/// eliminates a shift + subtract on the hot decode path.
/// On BMI2-capable x86-64 CPUs the table is bypassed entirely in favour
/// of the single-cycle `bzhi` instruction (see [`mask_lower_bits`]).
const BIT_MASK: [u64; 65] = {
    let mut table = [0u64; 65];
    let mut i: u32 = 1;
    while i < 64 {
        table[i as usize] = (1u64 << i) - 1;
        i += 1;
    }
    table[64] = u64::MAX;
    table
};

/// Return the lowest `n` bits of `value` (zero the rest).
///
/// On x86-64 with BMI2 this compiles to a single `bzhi` instruction.
/// Everywhere else it falls back to the pre-computed [`BIT_MASK`] table.
/// This function supports `n <= 64`; zstd callers normally guarantee
/// `n <= 56` (the maximum single-symbol width in zstd).
/// We intentionally do NOT clamp: an out-of-range `n` indicates a
/// corrupted bitstream or caller bug, and should panic consistently
/// rather than silently returning garbage.
#[inline(always)]
fn mask_lower_bits(value: u64, n: u8) -> u64 {
    assert!(n <= 64, "mask_lower_bits: n must be <= 64, got {}", n);
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        // SAFETY: `_bzhi_u64` is always safe to call when the target supports BMI2.
        unsafe { core::arch::x86_64::_bzhi_u64(value, n as u32) }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        value & BIT_MASK[n as usize]
    }
}

/// Zstandard encodes some types of data in a way that the data must be read
/// back to front to decode it properly. `BitReaderReversed` provides a
/// convenient interface to do that.
pub struct BitReaderReversed<'s> {
    /// The index of the last read byte in the source.
    index: usize,

    /// How many bits have been consumed from `bit_container`.
    bits_consumed: u8,

    /// How many bits have been consumed past the end of the input. Will be zero until all the input
    /// has been read.
    extra_bits: usize,

    /// The source data to read from.
    source: &'s [u8],

    /// The reader doesn't read directly from the source, it reads bits from here, and the container
    /// is "refilled" as it's emptied.
    bit_container: u64,
}

impl<'s> BitReaderReversed<'s> {
    /// How many bits are left to read by the reader.
    pub fn bits_remaining(&self) -> isize {
        self.index as isize * 8 + (64 - self.bits_consumed as isize) - self.extra_bits as isize
    }

    pub fn new(source: &'s [u8]) -> BitReaderReversed<'s> {
        BitReaderReversed {
            index: source.len(),
            bits_consumed: 64,
            source,
            bit_container: 0,
            extra_bits: 0,
        }
    }

    /// We refill the container in full bytes, shifting the still unread portion to the left, and filling the lower bits with new data
    #[cold]
    fn refill(&mut self) {
        let bytes_consumed = self.bits_consumed as usize / 8;
        if bytes_consumed == 0 {
            return;
        }

        if self.index >= bytes_consumed {
            // We can safely move the window contained in `bit_container` down by `bytes_consumed`
            // If the reader wasn't byte aligned, the byte that was partially read is now in the highest order bits in the `bit_container`
            self.index -= bytes_consumed;
            // Some bits of the `bits_container` might have been consumed already because we read the window byte aligned
            self.bits_consumed &= 7;
            self.bit_container =
                u64::from_le_bytes((&self.source[self.index..][..8]).try_into().unwrap());
        } else if self.index > 0 {
            // Read the last portion of source into the `bit_container`
            if self.source.len() >= 8 {
                self.bit_container = u64::from_le_bytes((&self.source[..8]).try_into().unwrap());
            } else {
                let mut value = [0; 8];
                value[..self.source.len()].copy_from_slice(self.source);
                self.bit_container = u64::from_le_bytes(value);
            }

            self.bits_consumed -= 8 * self.index as u8;
            self.index = 0;

            self.bit_container <<= self.bits_consumed;
            self.extra_bits += self.bits_consumed as usize;
            self.bits_consumed = 0;
        } else if self.bits_consumed < 64 {
            // Shift out already used bits and fill up with zeroes
            self.bit_container <<= self.bits_consumed;
            self.extra_bits += self.bits_consumed as usize;
            self.bits_consumed = 0;
        } else {
            // All useful bits have already been read and more than 64 bits have been consumed, all we now do is return zeroes
            self.extra_bits += self.bits_consumed as usize;
            self.bits_consumed = 0;
            self.bit_container = 0;
        }

        // Assert that at least `56 = 64 - 8` bits are available to read.
        debug_assert!(self.bits_consumed < 8);
    }

    /// Read `n` number of bits from the source. Will read at most 56 bits.
    /// If there are no more bits to be read from the source zero bits will be returned instead.
    #[inline(always)]
    pub fn get_bits(&mut self, n: u8) -> u64 {
        if self.bits_consumed + n > 64 {
            self.refill();
        }

        let value = self.peek_bits(n);
        self.consume(n);
        value
    }

    /// Ensure at least `n` bits are available for subsequent unchecked reads.
    /// After calling this, it is safe to call [`get_bits_unchecked`](Self::get_bits_unchecked)
    /// for a combined total of up to `n` bits without individual refill checks.
    ///
    /// `n` must be at most 56.
    #[inline(always)]
    pub fn ensure_bits(&mut self, n: u8) {
        debug_assert!(n <= 56);
        if self.bits_consumed + n > 64 {
            self.refill();
        }
    }

    /// Read `n` bits from the source **without** checking whether a refill is
    /// needed. The caller **must** guarantee enough bits are available (e.g. via
    /// a prior [`ensure_bits`](Self::ensure_bits) call).
    #[inline(always)]
    pub fn get_bits_unchecked(&mut self, n: u8) -> u64 {
        debug_assert!(n <= 56);
        debug_assert!(
            self.bits_consumed + n <= 64,
            "get_bits_unchecked: not enough bits (consumed={}, requested={})",
            self.bits_consumed,
            n
        );
        let value = self.peek_bits(n);
        self.consume(n);
        value
    }

    /// Get the next `n` bits from the source without consuming them.
    /// Caller is responsible for making sure that `n` many bits have been refilled.
    ///
    /// Branchless: when `n == 0` the mask is zero so the result is zero
    /// without a dedicated check. `wrapping_shr` avoids a debug-mode
    /// panic when the computed shift equals 64 (which happens legitimately
    /// when `bits_consumed == 0` and `n == 0`).
    #[inline(always)]
    pub fn peek_bits(&mut self, n: u8) -> u64 {
        // n == 0 is valid (branchless no-op); otherwise the caller must
        // guarantee bits_consumed + n <= 64 via ensure_bits / get_bits.
        debug_assert!(
            n == 0 || self.bits_consumed + n <= 64,
            "peek_bits: not enough bits (consumed={}, requested={})",
            self.bits_consumed,
            n
        );
        let shift_by = (64u8 - self.bits_consumed).wrapping_sub(n);
        mask_lower_bits(self.bit_container.wrapping_shr(shift_by as u32), n)
    }

    /// Get the next `n1` `n2` and `n3` bits from the source without consuming them.
    /// Caller is responsible for making sure that `sum` many bits have been refilled.
    ///
    /// Branchless: when all widths are zero the masks are zero, producing (0, 0, 0).
    #[inline(always)]
    pub fn peek_bits_triple(&mut self, sum: u8, n1: u8, n2: u8, n3: u8) -> (u64, u64, u64) {
        debug_assert_eq!(
            sum,
            n1 + n2 + n3,
            "peek_bits_triple: sum ({}) must equal n1+n2+n3 ({}+{}+{})",
            sum,
            n1,
            n2,
            n3
        );
        debug_assert!(
            sum == 0 || self.bits_consumed + sum <= 64,
            "peek_bits_triple: not enough bits (consumed={}, requested={})",
            self.bits_consumed,
            sum
        );
        // all_three contains bits like this: |XXXX..XXX111122223333|
        // Where XXX are already consumed bytes, 1/2/3 are bits of the respective value
        // Lower bits are to the right
        let shift_by = (64u8 - self.bits_consumed).wrapping_sub(sum);
        let all_three = self.bit_container.wrapping_shr(shift_by as u32);

        let val1 = mask_lower_bits(all_three >> (n3 + n2), n1);
        let val2 = mask_lower_bits(all_three >> n3, n2);
        let val3 = mask_lower_bits(all_three, n3);

        (val1, val2, val3)
    }

    /// Consume `n` bits from the source.
    #[inline(always)]
    pub fn consume(&mut self, n: u8) {
        self.bits_consumed += n;
        debug_assert!(self.bits_consumed <= 64);
    }

    /// Same as calling get_bits three times but slightly more performant.
    ///
    /// Uses a single conditional refill (via [`ensure_bits`](Self::ensure_bits))
    /// instead of unconditionally refilling, avoiding redundant work when the
    /// bit container already holds enough bits.
    #[inline(always)]
    pub fn get_bits_triple(&mut self, n1: u8, n2: u8, n3: u8) -> (u64, u64, u64) {
        let sum = n1 + n2 + n3;
        if sum <= 56 {
            self.ensure_bits(sum);

            let triple = self.peek_bits_triple(sum, n1, n2, n3);
            self.consume(sum);
            return triple;
        }

        (self.get_bits(n1), self.get_bits(n2), self.get_bits(n3))
    }
}

#[cfg(test)]
mod test {

    #[test]
    fn it_works() {
        let data = [0b10101010, 0b01010101];
        let mut br = super::BitReaderReversed::new(&data);
        assert_eq!(br.get_bits(1), 0);
        assert_eq!(br.get_bits(1), 1);
        assert_eq!(br.get_bits(1), 0);
        assert_eq!(br.get_bits(4), 0b1010);
        assert_eq!(br.get_bits(4), 0b1101);
        assert_eq!(br.get_bits(4), 0b0101);
        // Last 0 from source, three zeroes filled in
        assert_eq!(br.get_bits(4), 0b0000);
        // All zeroes filled in
        assert_eq!(br.get_bits(4), 0b0000);
        assert_eq!(br.bits_remaining(), -7);
    }

    /// Verify that `ensure_bits(n)` + `get_bits_unchecked(..)` returns the same
    /// values as plain `get_bits(..)`, including across refill boundaries and
    /// for edge cases like n=0.
    #[test]
    fn ensure_and_unchecked_match_get_bits() {
        // 10 bytes = 80 bits — enough to force multiple refills
        let data: [u8; 10] = [0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x13, 0x37, 0xCA, 0xFE, 0x01];

        // Reference: read with get_bits
        let mut ref_br = super::BitReaderReversed::new(&data);
        let r1 = ref_br.get_bits(0);
        let r2 = ref_br.get_bits(7);
        let r3 = ref_br.get_bits(13);
        let r4 = ref_br.get_bits(9);
        let r5 = ref_br.get_bits(8);
        let r5b = ref_br.get_bits(2);
        // After 39 bits consumed, ensure_bits(26) triggers a real refill
        // because 39 + 26 = 65 > 64.
        let r6 = ref_br.get_bits(9);
        let r7 = ref_br.get_bits(9);
        let r8 = ref_br.get_bits(8);

        // Unchecked path: same reads via ensure_bits + get_bits_unchecked
        let mut fast_br = super::BitReaderReversed::new(&data);

        // n=0 edge case
        fast_br.ensure_bits(0);
        assert_eq!(fast_br.get_bits_unchecked(0), r1);

        // Single reads
        fast_br.ensure_bits(7);
        assert_eq!(fast_br.get_bits_unchecked(7), r2);

        fast_br.ensure_bits(13);
        assert_eq!(fast_br.get_bits_unchecked(13), r3);

        fast_br.ensure_bits(9);
        assert_eq!(fast_br.get_bits_unchecked(9), r4);

        fast_br.ensure_bits(8);
        assert_eq!(fast_br.get_bits_unchecked(8), r5);

        fast_br.ensure_bits(2);
        assert_eq!(fast_br.get_bits_unchecked(2), r5b);

        // Batched: one ensure covering 9+9+8 = 26 bits.
        // At 39 bits consumed, this forces a real refill (39+26=65 > 64).
        fast_br.ensure_bits(26);
        assert_eq!(fast_br.get_bits_unchecked(9), r6);
        assert_eq!(fast_br.get_bits_unchecked(9), r7);
        assert_eq!(fast_br.get_bits_unchecked(8), r8);

        assert_eq!(ref_br.bits_remaining(), fast_br.bits_remaining());
    }

    /// Verify that the pre-computed BIT_MASK table produces correct values.
    #[test]
    fn mask_table_correctness() {
        assert_eq!(super::BIT_MASK[0], 0);
        assert_eq!(super::BIT_MASK[1], 1);
        assert_eq!(super::BIT_MASK[8], 0xFF);
        assert_eq!(super::BIT_MASK[16], 0xFFFF);
        assert_eq!(super::BIT_MASK[32], 0xFFFF_FFFF);
        assert_eq!(super::BIT_MASK[63], (1u64 << 63) - 1);
        assert_eq!(super::BIT_MASK[64], u64::MAX);
        for n in 0..64u32 {
            assert_eq!(
                super::BIT_MASK[n as usize],
                (1u64 << n) - 1,
                "BIT_MASK[{n}] mismatch"
            );
        }
    }

    /// Verify mask_lower_bits matches manual computation for edge values.
    #[test]
    fn mask_lower_bits_edge_cases() {
        assert_eq!(super::mask_lower_bits(u64::MAX, 0), 0);
        assert_eq!(super::mask_lower_bits(u64::MAX, 1), 1);
        assert_eq!(super::mask_lower_bits(0xABCD_1234_5678_9ABC, 8), 0xBC);
        assert_eq!(super::mask_lower_bits(0xABCD_1234_5678_9ABC, 16), 0x9ABC);
    }

    /// peek_bits(0) must return 0 in all states, including when
    /// bits_consumed is 0 (post-exhaustion refill).
    #[test]
    fn peek_bits_zero_is_always_zero() {
        let data = [0xFF; 8];
        let mut br = super::BitReaderReversed::new(&data);

        // Initial state: bits_consumed = 64
        assert_eq!(br.peek_bits(0), 0);

        // After reading some bits: bits_consumed < 64
        br.get_bits(7);
        assert_eq!(br.peek_bits(0), 0);

        // Force bits_consumed == 0 to exercise the shift-by-64 edge case
        // in peek_bits. This state occurs naturally during refill() when the
        // source is exhausted. We set it directly because get_bits always
        // calls consume(n) after refill, making bits_consumed > 0 by the
        // time it returns.
        br.bits_consumed = 0;
        assert_eq!(br.peek_bits(0), 0);
    }

    /// get_bits_triple must produce the same values as three individual
    /// get_bits calls, both with and without a refill in between.
    #[test]
    fn get_bits_triple_matches_individual() {
        let data: [u8; 16] = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x13, 0x37, 0xCA, 0xFE, 0x01, 0x99, 0x88, 0x77, 0x66,
            0x55, 0x44,
        ];

        // Reference: individual reads
        let mut ref_br = super::BitReaderReversed::new(&data);
        let r1 = ref_br.get_bits(8);
        let r2 = ref_br.get_bits(9);
        let r3 = ref_br.get_bits(9);

        // Triple read
        let mut triple_br = super::BitReaderReversed::new(&data);
        let (t1, t2, t3) = triple_br.get_bits_triple(8, 9, 9);

        assert_eq!((r1, r2, r3), (t1, t2, t3));
        assert_eq!(ref_br.bits_remaining(), triple_br.bits_remaining());

        // No-refill fast path: 8 bits already consumed, so the next 26 bits
        // still fit in the current container and `ensure_bits(26)` should
        // skip `refill()`.
        let mut ref_br = super::BitReaderReversed::new(&data);
        let mut triple_br = super::BitReaderReversed::new(&data);
        let _ = ref_br.get_bits(8);
        let _ = triple_br.get_bits(8);

        let r1 = ref_br.get_bits(8);
        let r2 = ref_br.get_bits(9);
        let r3 = ref_br.get_bits(9);
        let (t1, t2, t3) = triple_br.get_bits_triple(8, 9, 9);

        assert_eq!((r1, r2, r3), (t1, t2, t3));
        assert_eq!(ref_br.bits_remaining(), triple_br.bits_remaining());
    }
}
