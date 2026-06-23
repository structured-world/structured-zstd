use super::{CompressionLevel, compress_bound, compress_slice_to_vec};

#[test]
fn matches_upstream_formula_below_threshold() {
    // src_size + (src_size >> 8) + ((128 KiB - src_size) >> 11).
    assert_eq!(compress_bound(0), 64);
    assert_eq!(compress_bound(4096), 4096 + 16 + 62);
}

#[test]
fn drops_margin_at_and_above_threshold() {
    let lower = 128 * 1024;
    assert_eq!(compress_bound(lower), lower + (lower >> 8));
    assert_eq!(compress_bound(lower + 1), (lower + 1) + ((lower + 1) >> 8));
}

#[test]
fn saturates_instead_of_wrapping() {
    // No allocation this large can exist; the ceiling is the right sentinel.
    assert_eq!(compress_bound(usize::MAX), usize::MAX);
}

#[test]
fn always_fits_real_compressed_output() {
    for len in [0usize, 1, 100, 4096, 200_000] {
        let data = alloc::vec![7u8; len];
        let out = compress_slice_to_vec(&data, CompressionLevel::Default);
        assert!(out.len() <= compress_bound(len), "len={len}");
    }
}
