use super::*;
use alloc::vec::Vec;

#[test]
fn neon_prefix_len_matches_scalar_on_long_run() {
    // 40-byte runs cover both the SIMD 16-byte loop and the scalar tail.
    let a = b"abcdefghijklmnopqrstuvwxyz0123456789-+=*";
    let mut b: Vec<u8> = a.to_vec();
    b[25] = b'!';
    let max = a.len();
    let neon = unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
    let scl = unsafe { scalar::common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
    assert_eq!(neon, scl);
    assert_eq!(neon, 25);
}

#[test]
fn neon_handles_short_input() {
    let a = b"abc";
    let b = b"abc";
    let max = a.len();
    assert_eq!(
        unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) },
        3
    );
}
