use super::*;

/// Whatever the raise does (refused without the privilege, granted with it),
/// the priority the process had is back once the guard is gone, so the work
/// after a measurement does not keep it.
#[test]
fn the_priority_is_put_back_after_a_raise() {
    let before = sys::current();
    drop(raise_to_realtime());
    assert_eq!(sys::current(), before);
}
