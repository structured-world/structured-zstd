use super::{row_tag_match_mask_neon, row_tag_match_mask_scalar};

fn fill(buf: &mut [u8], mut state: u64) {
    for b in buf.iter_mut() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (state >> 56) as u8;
    }
}

/// The NEON kernel must produce byte-identical masks to the scalar
/// reference for every supported row width (16 / 32 / 64) and tag, so
/// match selection (and the compressed output) is unchanged on aarch64.
#[test]
fn neon_tag_mask_matches_scalar() {
    for &width in &[16usize, 32, 64] {
        let mut tags = alloc::vec![0u8; width];
        for seed in 0..32u64 {
            fill(&mut tags, 0x9e3779b97f4a7c15u64.wrapping_add(seed));
            for tag in [tags[seed as usize % width], 0u8, 0xFF, (seed as u8)] {
                let expected = row_tag_match_mask_scalar(&tags, tag);
                // SAFETY: NEON is baseline on aarch64.
                let got = unsafe { row_tag_match_mask_neon(&tags, tag) };
                assert_eq!(got, expected, "neon width={width} tag={tag}");
            }
        }
    }
}
