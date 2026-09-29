use super::find_fcs_field_size;
use super::find_min_size;
use super::hash_top_bits;
use super::write_minified_val;
use alloc::vec;
use alloc::vec::Vec;

/// A hash index is the product's top `width` bits for every width a table
/// takes (1..=32), on a 32-bit target (read from the high word) as on a 64-bit
/// one (one 64-bit shift).
#[test]
fn hash_top_bits_are_the_products_top_bits_at_every_width() {
    let products = [
        0u64,
        u64::MAX,
        0x8000_0000_0000_0001,
        0x0000_0000_ffff_ffff,
        0xcf1b_bcdc_b7a5_6463,
        0x1234_5678_9abc_def0,
    ];
    for product in products {
        for width in 1..=32 {
            assert_eq!(
                hash_top_bits(product, width),
                (product >> (64 - width)) as u32,
                "product {product:#x}, width {width}"
            );
        }
    }
}

fn minify_val(val: u64) -> Vec<u8> {
    let mut out = Vec::new();
    write_minified_val(val, &mut out);
    out
}

#[test]
fn min_size_detection() {
    assert_eq!(find_min_size(0), 1);
    assert_eq!(find_min_size(0xff), 1);
    assert_eq!(find_min_size(0xff_ff), 2);
    assert_eq!(find_min_size(0x00_ff_ff_ff), 4);
    assert_eq!(find_min_size(0xff_ff_ff_ff), 4);
    assert_eq!(find_min_size(0x00ff_ffff_ffff_ffff), 8);
    assert_eq!(find_min_size(0xffff_ffff_ffff_ffff), 8);
}

#[test]
fn fcs_field_size_single_segment() {
    // 1-byte range: 0–255 when single_segment is true
    assert_eq!(find_fcs_field_size(0, true), 1);
    assert_eq!(find_fcs_field_size(255, true), 1);
    // 2-byte range: 256–65791
    assert_eq!(find_fcs_field_size(256, true), 2);
    assert_eq!(find_fcs_field_size(65791, true), 2);
    // 4-byte range
    assert_eq!(find_fcs_field_size(65792, true), 4);
    assert_eq!(find_fcs_field_size(u32::MAX as u64, true), 4);
    // 8-byte range
    assert_eq!(find_fcs_field_size(u32::MAX as u64 + 1, true), 8);
}

#[test]
fn fcs_field_size_no_single_segment() {
    // Without single_segment, 0–255 cannot use 1-byte → falls to 4 bytes
    assert_eq!(find_fcs_field_size(0, false), 4);
    assert_eq!(find_fcs_field_size(255, false), 4);
    // 256–65791 still fits in 2 bytes
    assert_eq!(find_fcs_field_size(256, false), 2);
    assert_eq!(find_fcs_field_size(65791, false), 2);
    // Values that find_min_size would map to 4 but FCS can still fit in 2
    assert_eq!(find_fcs_field_size(65536, false), 2);
}

#[test]
fn bytes_minified() {
    assert_eq!(minify_val(0), vec![0]);
    assert_eq!(minify_val(0xff), vec![0xff]);
    assert_eq!(minify_val(0xff_ff), vec![0xff, 0xff]);
    assert_eq!(minify_val(0xff_ff_ff_ff), vec![0xff, 0xff, 0xff, 0xff]);
    assert_eq!(
        minify_val(0xffff_ffff_ffff_ffff),
        vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    );
}
