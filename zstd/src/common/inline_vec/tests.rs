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
    v.resize(8, 4);
    assert_eq!(v.len(), 8);
    v.clear();
    assert!(v.is_empty());
}

/// A clone holds the same elements.
#[test]
fn clone_copies_the_elements() {
    let mut v: InlineVec<u16, 4> = InlineVec::new();
    v.extend_from_slice(&[5, 6, 7]);
    assert_eq!(&*v.clone(), &[5, 6, 7]);
}

/// Appending past the capacity is a broken invariant and must stop at the
/// buffer's edge rather than write beyond it.
#[test]
#[should_panic]
fn push_past_capacity_panics() {
    let mut v: InlineVec<u8, 2> = InlineVec::new();
    v.extend_from_slice(&[1, 2]);
    v.push(3);
}

#[test]
#[should_panic]
fn resize_past_capacity_panics() {
    let mut v: InlineVec<i32, 3> = InlineVec::new();
    v.resize(4, 0);
}

#[test]
#[should_panic(expected = "past its capacity")]
fn set_len_past_capacity_panics() {
    let mut v: InlineVec<u8, 2> = InlineVec::new();
    // SAFETY: the call is expected to panic before exposing anything.
    unsafe { v.set_len(3) };
}
