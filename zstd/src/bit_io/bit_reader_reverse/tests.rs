// `try_extract_triple_with_pext` (x86_64 + BMI2 only) reaches the production
// `triple_extract_dispatch` / `extract_triple_pext` through this glob; on other
// targets that helper compiles out, leaving the import unused.
#[allow(unused_imports)]
use super::*;

/// Return the lowest `n` bits of `value` (zero the rest).
///
/// On x86-64 with BMI2 this compiles to a single `bzhi` instruction.
/// Everywhere else it computes the mask via `u64::MAX >> (64 - n)`
/// (replaced the previous `BIT_MASK[n]` table load — one shift + one
/// predicted cmov vs one 3–5 cycle L1 load on the hot FSE path).
///
/// This function supports `n <= 64`; zstd callers normally guarantee
/// `n <= 56` (the maximum single-symbol width in zstd). The
/// `debug_assert!(n <= 64)` on the FIRST line of the function body
/// (not just on `get_bits` callers) is the input-validation gate that
/// the fuzz suite relies on — invalid `n > 64` (e.g. from a malformed
/// FSE table or `accuracy_log`) trips it instead of silently returning
/// 0 from the release path. On the BMI2 path `_bzhi_u64` would
/// silently truncate without it; on the fallback path `checked_shr`
/// returns `None` for the wrapping-underflow shift and the
/// `unwrap_or(0)` would otherwise hide the upstream bug.
// Used only by the in-file mask_lower_bits unit tests after the
// hot-path migration to `K::mask_lower_bits` via the `CpuKernel`
// trait. Gating with `#[cfg(test)]` keeps the helper available for
// the regression tests below without triggering a `dead_code`
// warning under `-D warnings` in normal builds.
#[cfg(test)]
#[inline(always)]
fn mask_lower_bits(value: u64, n: u8) -> u64 {
    // Input-validation gate documented in the rustdoc above — keep this
    // as the first statement; removing it lets malformed inputs (`n >
    // 64`) silently decode to 0 in release builds instead of being
    // caught by the fuzz suite.
    debug_assert!(n <= 64, "mask_lower_bits: n must be <= 64, got {}", n);
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        // SAFETY: `_bzhi_u64` is always safe to call when the target supports BMI2.
        unsafe { core::arch::x86_64::_bzhi_u64(value, n as u32) }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        // Compute the mask via `u64::MAX >> (64 - n)` instead of a
        // `BIT_MASK[n]` table load. One shift + one (predicted) cmov
        // vs one L1 load (3-5 cycle latency). For the hot FSE bitstream
        // decode path this fires 3x per sequence; saving the load
        // latency per call compounds over thousands of sequences.
        //
        // `checked_shr` returns `None` when the shift count is ≥ 64,
        // which happens exactly when `n == 0` (`64 - 0 = 64`) or when
        // the debug_assert above would have fired (`n > 64`, underflow
        // wraps to a huge value). Mapping both to `0` gives the
        // mathematically-correct empty mask for n=0 and a safe-ish
        // fallback for the invalid range.
        let mask = u64::MAX
            .checked_shr(64u32.wrapping_sub(n as u32))
            .unwrap_or(0);
        value & mask
    }
}
// Used only by the in-file extract_triple correctness tests after
// `peek_bits_triple` switched to the per-reader `use_pext_triple`
// cached flag (commit 8805122f) — production now calls
// `extract_triple_pext` directly via that path. Gating with
// `#[cfg(test)]` keeps the helper available for the tests while
// avoiding a `dead_code` warning under `-D warnings`.
#[cfg(all(test, feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[inline(always)]
fn try_extract_triple_with_pext(all_three: u64, n1: u8, n2: u8, n3: u8) -> Option<(u64, u64, u64)> {
    if !triple_extract_dispatch().use_pext {
        return None;
    }

    Some(unsafe { extract_triple_pext(all_three, n1, n2, n3) })
}
#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
use std::arch::is_x86_feature_detected;

#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[inline]
fn scalar_extract_triple(all_three: u64, n1: u8, n2: u8, n3: u8) -> (u64, u64, u64) {
    let val3 = all_three & super::BIT_MASK[n3 as usize];
    let val2 = all_three.wrapping_shr(u32::from(n3)) & super::BIT_MASK[n2 as usize];
    let val1 = all_three.wrapping_shr(u32::from(n2) + u32::from(n3)) & super::BIT_MASK[n1 as usize];
    (val1, val2, val3)
}

#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[inline]
fn next_test_value(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[test]
fn it_works() {
    let data = [0b10101010, 0b01010101];
    let mut br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
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
    let mut ref_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
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
    let mut fast_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);

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
    assert_eq!(mask_lower_bits(u64::MAX, 0), 0);
    assert_eq!(mask_lower_bits(u64::MAX, 1), 1);
    assert_eq!(
        mask_lower_bits(0xABCD_1234_5678_9ABC, 64),
        0xABCD_1234_5678_9ABC
    );
    assert_eq!(mask_lower_bits(0xABCD_1234_5678_9ABC, 8), 0xBC);
    assert_eq!(mask_lower_bits(0xABCD_1234_5678_9ABC, 16), 0x9ABC);
}

/// peek_bits(0) must return 0 in all states, including when
/// bits_consumed is 0 (post-exhaustion refill).
#[test]
fn peek_bits_zero_is_always_zero() {
    let data = [0xFF; 8];
    let mut br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);

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
        0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x13, 0x37, 0xCA, 0xFE, 0x01, 0x99, 0x88, 0x77, 0x66, 0x55,
        0x44,
    ];

    // Reference: individual reads
    let mut ref_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
    let r1 = ref_br.get_bits(8);
    let r2 = ref_br.get_bits(9);
    let r3 = ref_br.get_bits(9);

    // Triple read
    let mut triple_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
    let (t1, t2, t3) = triple_br.get_bits_triple(8, 9, 9);

    assert_eq!((r1, r2, r3), (t1, t2, t3));
    assert_eq!(ref_br.bits_remaining(), triple_br.bits_remaining());

    // No-refill fast path: 8 bits already consumed, so the next 26 bits
    // still fit in the current container and `ensure_bits(26)` should
    // skip `refill()`.
    let mut ref_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
    let mut triple_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
    let _ = ref_br.get_bits(8);
    let _ = triple_br.get_bits(8);

    let r1 = ref_br.get_bits(8);
    let r2 = ref_br.get_bits(9);
    let r3 = ref_br.get_bits(9);
    let (t1, t2, t3) = triple_br.get_bits_triple(8, 9, 9);

    assert_eq!((r1, r2, r3), (t1, t2, t3));
    assert_eq!(ref_br.bits_remaining(), triple_br.bits_remaining());

    // Mixed zero-widths: individual sequence extra-bit fields can be zero.
    let mut ref_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
    let mut triple_br = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);

    let r1 = ref_br.get_bits(5);
    let r2 = ref_br.get_bits(0);
    let r3 = ref_br.get_bits(4);
    let (t1, t2, t3) = triple_br.get_bits_triple(5, 0, 4);

    assert_eq!((r1, r2, r3), (t1, t2, t3));
    assert_eq!(ref_br.bits_remaining(), triple_br.bits_remaining());
}

/// `peek_bits_bmi2` MUST produce the same value as scalar `peek_bits`
/// on every BMI2-capable CPU. Without parity the bmi2 fast-path
/// chain (when wired) would silently corrupt FSE state.
#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[test]
fn peek_bits_bmi2_matches_scalar() {
    if !is_x86_feature_detected!("bmi2") {
        return;
    }
    let data: [u8; 16] = [
        0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x13, 0x37, 0xCA, 0xFE, 0x01, 0x99, 0x88, 0x77, 0x66, 0x55,
        0x44,
    ];

    for n in [0u8, 1, 5, 8, 13, 24, 32, 48, 56] {
        let mut scalar = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
        let mut bmi2 = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
        scalar.ensure_bits(n);
        bmi2.ensure_bits(n);
        let s = scalar.peek_bits(n);
        // SAFETY: gated on `is_x86_feature_detected!("bmi2")` above.
        let b = unsafe { bmi2.peek_bits_bmi2(n) };
        assert_eq!(s, b, "mismatch at n={}", n);
    }
}

/// `peek_bits_triple_bmi2` MUST produce the same triple as the
/// scalar variant for every width combination the FSE/HUF decoders
/// can reach.
#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[test]
fn peek_bits_triple_bmi2_matches_scalar() {
    if !is_x86_feature_detected!("bmi2") {
        return;
    }
    let data: [u8; 16] = [
        0xDE, 0xAD, 0xBE, 0xEF, 0x42, 0x13, 0x37, 0xCA, 0xFE, 0x01, 0x99, 0x88, 0x77, 0x66, 0x55,
        0x44,
    ];

    let widths = [
        (0, 0, 0),
        (1, 1, 1),
        (3, 5, 7),
        (8, 8, 8),
        (15, 16, 17),
        (5, 0, 4),
    ];
    for &(n1, n2, n3) in &widths {
        let sum = n1 + n2 + n3;
        let mut scalar = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
        let mut bmi2 = super::BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&data);
        scalar.ensure_bits(sum);
        bmi2.ensure_bits(sum);
        let s = scalar.peek_bits_triple(sum, n1, n2, n3);
        // SAFETY: gated on `is_x86_feature_detected!("bmi2")` above.
        let b = unsafe { bmi2.peek_bits_triple_bmi2(sum, n1, n2, n3) };
        assert_eq!(s, b, "mismatch at widths=({},{},{})", n1, n2, n3);
    }
}

#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[test]
fn should_use_pext_policy_table() {
    let cases = [
        (*b"AuthenticAMD", 0x17, false),
        (*b"AuthenticAMD", 0x19, true),
        (*b"GenuineIntel", 0x06, true),
    ];

    for (vendor, family, expected) in cases {
        assert_eq!(super::should_use_pext(vendor, family), expected);
    }
}

#[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
#[test]
fn bmi2_triple_extract_matches_scalar_reference() {
    if !is_x86_feature_detected!("bmi2") {
        return;
    }

    let widths = [
        (0, 0, 0),
        (1, 1, 1),
        (3, 5, 7),
        (8, 8, 8),
        (15, 16, 17),
        (21, 21, 21),
        (0, 13, 27),
        (31, 0, 1),
        (1, 31, 0),
        (20, 20, 24),
    ];
    let fixed_values = [
        0,
        1,
        u64::MAX,
        0x0123_4567_89AB_CDEF,
        0xFEDC_BA98_7654_3210,
        0xAAAA_AAAA_AAAA_AAAA,
        0x5555_5555_5555_5555,
        1u64 << 63,
        (1u64 << 32) - 1,
    ];

    for &(n1, n2, n3) in &widths {
        for &all_three in &fixed_values {
            let expected = scalar_extract_triple(all_three, n1, n2, n3);
            let pext = unsafe { super::extract_triple_pext(all_three, n1, n2, n3) };
            assert_eq!(pext, expected);

            if let Some(dispatched) = try_extract_triple_with_pext(all_three, n1, n2, n3) {
                assert_eq!(dispatched, expected);
            }
        }
    }

    let mut state = 0xD6E8_FD9D_5A2C_19B7u64;
    for &(n1, n2, n3) in &widths {
        for _ in 0..64 {
            let all_three = next_test_value(&mut state);
            let expected = scalar_extract_triple(all_three, n1, n2, n3);
            let pext = unsafe { super::extract_triple_pext(all_three, n1, n2, n3) };
            assert_eq!(pext, expected);

            if let Some(dispatched) = try_extract_triple_with_pext(all_three, n1, n2, n3) {
                assert_eq!(dispatched, expected);
            }
        }
    }
}
