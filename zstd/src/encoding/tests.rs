use super::dictionary_describes_frame;

/// The size question answers for any dictionary a caller can name, including
/// one whose six times does not fit the type.
///
/// The comparison is `src < dict * 6`, and it is public, so nothing stops a
/// caller from passing a `usize` whose sixfold overflows: the multiplication
/// then panics in a debug build and wraps in a release one, and a wrapped
/// product reads as a SMALLER bound than the real one, so a source that is
/// plainly about its dictionary is answered as if it were not. Asked as a
/// division it cannot overflow at all.
#[test]
fn a_dictionary_too_large_to_multiply_still_answers() {
    // Six times this is past `u64::MAX`, so the multiplying form cannot hold
    // the bound it needs to compare against.
    let huge = usize::MAX;
    assert!(
        dictionary_describes_frame(huge, Some(1 << 20)),
        "a megabyte against a dictionary of {huge} bytes is about the dictionary"
    );
    // And the answer stays right on both sides of the real bound, at sizes the
    // multiplying form has no trouble with.
    assert!(dictionary_describes_frame(4096, Some(8192)));
    assert!(!dictionary_describes_frame(4096, Some(1 << 20)));
    // The cutoff itself: a source under six times the dictionary is about it,
    // one at six times exactly is not.
    assert!(dictionary_describes_frame(1 << 20, Some((6 << 20) - 1)));
    assert!(!dictionary_describes_frame(1 << 20, Some(6 << 20)));
    // An empty dictionary describes nothing, and an unknown size leaves the
    // dictionary's own shape as the only thing to go on.
    assert!(!dictionary_describes_frame(0, Some(8192)));
    assert!(dictionary_describes_frame(4096, None));
    assert!(dictionary_describes_frame(4096, Some(u64::MAX)));
}
