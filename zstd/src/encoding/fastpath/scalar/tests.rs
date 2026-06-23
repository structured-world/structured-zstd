use super::*;
use alloc::vec::Vec;

#[test]
fn scalar_prefix_len_matches_naive_reference() {
    let a = b"abcdef_long string here for the test 1234567890";
    let mut b: Vec<u8> = a.to_vec();
    b[20] = b'!';
    let max = a.len().min(b.len());
    let len = unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
    assert_eq!(len, 20);
}

#[test]
fn scalar_prefix_len_empty_inputs() {
    let p = core::ptr::NonNull::<u8>::dangling().as_ptr();
    assert_eq!(unsafe { common_prefix_len_ptr(p, p, 0) }, 0);
}

#[test]
#[cfg(target_endian = "little")]
fn mismatch_byte_index_finds_low_bit_byte_le() {
    // diff == 0x_FF_00 -> mismatch is byte 1 (bits 8..16) on LE.
    let diff: usize = 0xff00;
    assert_eq!(mismatch_byte_index(diff), 1);
}

#[test]
#[cfg(target_endian = "big")]
fn mismatch_byte_index_finds_low_bit_byte_be() {
    // Mirror of the LE case for the BE branch: the LSB byte sits at the
    // tail of the word, so the mismatch byte index is `bytes_per_usize-1`.
    let diff: usize = 0xff;
    assert_eq!(mismatch_byte_index(diff), core::mem::size_of::<usize>() - 1);
}
