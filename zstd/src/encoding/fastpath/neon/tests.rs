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

/// Every length up to three 32-byte steps and every mismatch position in it,
/// or none: each half of a step, the lone 16-byte step after the loop and the
/// scalar tail all have to find the first unequal byte the scalar kernel finds.
#[test]
fn neon_prefix_len_matches_scalar_at_every_mismatch_position() {
    let a: Vec<u8> = (0..100u8).map(|i| i.wrapping_mul(37)).collect();
    for max in 0..=a.len() {
        for mismatch in (0..max).map(Some).chain([None]) {
            let mut b = a.clone();
            if let Some(at) = mismatch {
                b[at] ^= 0x5A;
            }
            let neon = unsafe { common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
            let scl = unsafe { scalar::common_prefix_len_ptr(a.as_ptr(), b.as_ptr(), max) };
            assert_eq!(neon, scl, "max {max}, mismatch at {mismatch:?}");
            assert_eq!(neon, mismatch.unwrap_or(max));
        }
    }
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
