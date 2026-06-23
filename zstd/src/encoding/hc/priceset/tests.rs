use super::*;

/// Per-tier deinterleave + improve-mask correctness vs a scalar reference.
/// Each tier's dispatch only fires on matching hardware (i9 picks AVX2 over
/// SSE4.1, M1 picks NEON), so the non-dispatched tiers never run in the
/// roundtrip suite; this exercises the deinterleave/mask helpers directly on
/// whatever ISA the test host exposes (AVX2 + SSE4.1 on x86, NEON on aarch64).
#[cfg(test)]
#[test]
fn priceset_tier_helpers_match_scalar() {
    // Reference: gen-stamped contiguous cells -> ordered prices on all-warm.
    fn scalar_deint<const W: usize>(cells: &[[u32; 2]], stamp: u32) -> Option<[u32; W]> {
        let mut out = [0u32; W];
        for k in 0..W {
            if cells[k][1] != stamp {
                return None;
            }
            out[k] = cells[k][0];
        }
        Some(out)
    }
    fn scalar_mask<const W: usize>(nc: &[u32; W], np: &[u32]) -> u8 {
        let mut m = 0u8;
        for k in 0..W {
            if nc[k] < np[k] {
                m |= 1 << k;
            }
        }
        m
    }
    const S: u32 = 0x55;
    let warm: [[u32; 2]; 4] = [[11, S], [22, S], [33, S], [44, S]];
    let mut cold = warm;
    cold[2][1] = S ^ 1; // one stale cell -> must yield None
    let nc4: [u32; 4] = [10, 99, 30, 41];
    let np4: [u32; 4] = [20, 21, 30, 99]; // lt: lane0 (10<20), lane3 (41<99)

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    unsafe {
        assert_eq!(
            priceset_cached_prices4_neon(&warm, S),
            scalar_deint::<4>(&warm, S)
        );
        assert_eq!(priceset_cached_prices4_neon(&cold, S), None);
        assert_eq!(
            priceset_improved_mask4_neon(&nc4, &np4),
            scalar_mask::<4>(&nc4, &np4)
        );
    }
    #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if std::is_x86_feature_detected!("sse4.2") {
            unsafe {
                assert_eq!(
                    priceset_cached_prices4_sse41(&warm, S),
                    scalar_deint::<4>(&warm, S)
                );
                assert_eq!(priceset_cached_prices4_sse41(&cold, S), None);
                assert_eq!(
                    priceset_improved_mask4_sse41(&nc4, &np4),
                    scalar_mask::<4>(&nc4, &np4)
                );
            }
        }
        if std::is_x86_feature_detected!("avx2") {
            let warm8: [[u32; 2]; 8] = [
                [11, S],
                [22, S],
                [33, S],
                [44, S],
                [55, S],
                [66, S],
                [77, S],
                [88, S],
            ];
            let mut cold8 = warm8;
            cold8[5][1] = S ^ 1;
            let nc8: [u32; 8] = [10, 99, 30, 41, 99, 60, 99, 80];
            let np8: [u32; 8] = [20, 21, 30, 99, 50, 99, 70, 99];
            unsafe {
                assert_eq!(
                    priceset_cached_prices8_avx2(&warm8, S),
                    scalar_deint::<8>(&warm8, S)
                );
                assert_eq!(priceset_cached_prices8_avx2(&cold8, S), None);
                assert_eq!(
                    priceset_improved_mask8_avx2(&nc8, &np8),
                    scalar_mask::<8>(&nc8, &np8)
                );
            }
        }
    }
}
