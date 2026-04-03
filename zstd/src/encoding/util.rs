use alloc::vec::Vec;

/// Returns the minimum number of bytes needed to represent this value, as
/// either 1, 2, 4, or 8 bytes. A value of 0 will still return one byte.
///
/// Used for variable length fields like `Dictionary_ID`.
pub fn find_min_size(val: u64) -> usize {
    if val == 0 {
        return 1;
    }
    if val >> 8 == 0 {
        return 1;
    }
    if val >> 16 == 0 {
        return 2;
    }
    if val >> 32 == 0 {
        return 4;
    }
    8
}

/// Returns the same value, but represented using the smallest number of bytes needed.
/// Returned vector will be 1, 2, 4, or 8 bytes in length. Zero is represented as 1 byte.
///
/// Operates in **little-endian**.
pub fn minify_val(val: u64) -> Vec<u8> {
    let new_size = find_min_size(val);
    val.to_le_bytes()[0..new_size].to_vec()
}

/// Returns the minimum FCS field size for the given content size.
///
/// FCS has different sizing rules than `Dictionary_ID` due to the +256 offset
/// for 2-byte fields (spec range 256–65791). When `single_segment` is true,
/// values 0–255 can use a 1-byte field (FCS flag = 0 combined with the
/// single-segment flag). Otherwise the smallest usable field is 4 bytes.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_content_size>
pub fn find_fcs_field_size(val: u64, single_segment: bool) -> usize {
    if single_segment && val <= 255 {
        return 1;
    }
    if (256..=65791).contains(&val) {
        return 2;
    }
    if val <= u32::MAX as u64 {
        return 4;
    }
    8
}

#[cfg(test)]
mod tests {
    use super::find_fcs_field_size;
    use super::find_min_size;
    use super::minify_val;
    use alloc::vec;

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
}
