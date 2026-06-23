use super::{row_tag_match_mask_avx2, row_tag_match_mask_scalar, row_tag_match_mask_sse2};

/// Deterministic LCG fill so the test exercises a realistic spread of
/// matching / non-matching tag bytes without a RNG dependency.
fn fill(buf: &mut [u8], mut state: u64) {
    for b in buf.iter_mut() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (state >> 56) as u8;
    }
}

/// The SIMD kernels must produce byte-identical masks to the scalar
/// reference for every supported row width (16 / 32 / 64) and tag, or
/// the match selection diverges and the compressed output changes.
#[test]
fn simd_tag_mask_matches_scalar() {
    for &width in &[16usize, 32, 64] {
        let mut tags = alloc::vec![0u8; width];
        for seed in 0..32u64 {
            fill(&mut tags, 0x9e3779b97f4a7c15u64.wrapping_add(seed));
            // Cover both a tag that occurs in the row and arbitrary tags.
            for tag in [tags[seed as usize % width], 0u8, 0xFF, (seed as u8)] {
                let expected = row_tag_match_mask_scalar(&tags, tag);
                if std::arch::is_x86_feature_detected!("sse2") {
                    let got = unsafe { row_tag_match_mask_sse2(&tags, tag) };
                    assert_eq!(got, expected, "sse2 width={width} tag={tag}");
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    let got = unsafe { row_tag_match_mask_avx2(&tags, tag) };
                    assert_eq!(got, expected, "avx2 width={width} tag={tag}");
                }
            }
        }
    }
}
