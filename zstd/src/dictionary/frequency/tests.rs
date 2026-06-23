use super::estimate_frequency;
#[test]
fn dead_beef() {
    assert_eq!(
        estimate_frequency(&[0xde, 0xad], &[0xde, 0xad, 0xbe, 0xef, 0xde, 0xad]),
        2
    );
}

#[test]
fn smallest_body() {
    assert_eq!(estimate_frequency(&[0x00, 0xff], &[0x00, 0xff]), 1);
}

#[test]
fn no_match() {
    assert_eq!(
        estimate_frequency(&[0xff, 0xff], &[0xde, 0xad, 0xbe, 0xef, 0xde, 0xad]),
        0
    );
}
