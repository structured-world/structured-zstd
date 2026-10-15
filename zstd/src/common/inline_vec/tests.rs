use super::InlineVec;

/// Appends within capacity read back exactly, like a `Vec`.
#[test]
fn push_resize_extend_within_capacity_read_back() {
    let mut v: InlineVec<u8, 8> = InlineVec::new();
    v.push(1);
    v.extend_from_slice(&[2, 3]);
    v.resize(5, 9);
    assert_eq!(v.as_slice(), &[1, 2, 3, 9, 9]);
    v.resize(2, 0);
    assert_eq!(v.as_slice(), &[1, 2]);
    v.clear();
    assert!(v.is_empty());
}

/// Writes past the capacity are dropped but counted, so a builder's own
/// length check sees the overflow; only the stored prefix is readable.
#[test]
fn overflow_is_counted_not_stored() {
    let mut v: InlineVec<u8, 4> = InlineVec::new();
    for b in 0..6 {
        v.push(b);
    }
    assert_eq!(v.len(), 6);
    assert_eq!(&*v, &[0, 1, 2, 3]);

    let mut w: InlineVec<i32, 3> = InlineVec::new();
    w.resize(10, 0);
    assert_eq!(w.len(), 10);
    assert_eq!(&*w, &[0, 0, 0]);
    w.extend_from_slice(&[7, 7]);
    assert_eq!(w.len(), 12);

    // Shrinking back below the capacity restores a fully readable buffer.
    w.resize(1, 0);
    assert_eq!(&*w, &[0]);
}

/// A clone keeps both the stored prefix and the counted length.
#[test]
fn clone_preserves_counted_length() {
    let mut v: InlineVec<u16, 2> = InlineVec::new();
    v.extend_from_slice(&[5, 6, 7]);
    let c = v.clone();
    assert_eq!(c.len(), 3);
    assert_eq!(&*c, &[5, 6]);
}

/// `set_len` past the capacity would expose memory that does not exist.
#[test]
#[should_panic(expected = "past its capacity")]
fn set_len_past_capacity_panics() {
    let mut v: InlineVec<u8, 2> = InlineVec::new();
    // SAFETY: the call is expected to panic before exposing anything.
    unsafe { v.set_len(3) };
}
