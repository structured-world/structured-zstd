use super::*;
use alloc::vec::Vec;

#[test]
fn simd128_prefix_len_matches_scalar_on_long_run() {
    let a = b"abcdefghijklmnopqrstuvwxyz0123456789-+=*";
    let mut b: Vec<u8> = a.to_vec();
    b[25] = b'!';
    let max = a.len();
    let got = unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
    let scl = unsafe { scalar::common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
    assert_eq!(got, scl);
    assert_eq!(got, 25);
}

#[test]
fn simd128_handles_short_input() {
    let a = b"abc";
    let b = b"abc";
    assert_eq!(
        unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), a.len()) },
        3
    );
}
