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

/// Appends the value represented using the smallest number of bytes needed
/// (1, 2, 4, or 8; zero takes 1 byte) directly onto `output`.
///
/// Operates in **little-endian**.
pub fn write_minified_val(val: u64, output: &mut Vec<u8>) {
    let new_size = find_min_size(val);
    output.extend_from_slice(&val.to_le_bytes()[0..new_size]);
}

/// Returns the minimum FCS field size for the given content size.
///
/// FCS has different sizing rules than `Dictionary_ID` due to the +256 offset
/// for 2-byte fields (spec range 256–65791). When `single_segment` is true,
/// values 0–255 can use a 1-byte field (FCS flag = 0 combined with the
/// single-segment flag). Otherwise, only the 1-byte encoding is unavailable:
/// values 256–65791 still use a 2-byte field, while smaller values fall back
/// to the 4-byte encoding.
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

/// The top `width` bits (`1..=32`) of a 64-bit multiplicative hash product,
/// the index a hash table of `width` bits takes.
#[cfg(not(target_pointer_width = "32"))]
#[inline(always)]
pub(crate) fn hash_top_bits(product: u64, width: u32) -> u32 {
    debug_assert!(
        (1..=32).contains(&width),
        "hash width {width} out of 1..=32"
    );
    (product >> (64 - width)) as u32
}

/// The top `width` bits (`1..=32`) of a 64-bit multiplicative hash product,
/// taken from its high word: they all lie there, and a 32-bit target then
/// shifts one register instead of a register pair by a variable count
/// (`shrd`, `shr`, a test of bit 5 and a select).
#[cfg(target_pointer_width = "32")]
#[inline(always)]
pub(crate) fn hash_top_bits(product: u64, width: u32) -> u32 {
    debug_assert!(
        (1..=32).contains(&width),
        "hash width {width} out of 1..=32"
    );
    ((product >> 32) as u32) >> (32 - width)
}

#[cfg(test)]
mod tests;
