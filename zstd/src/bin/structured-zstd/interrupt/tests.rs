use std::path::Path;

use super::imp::is_guarded;
use super::{clear, guard};

/// The guard is a window: it opens when a temporary is being written and
/// closes when the file is in place. A guard left open past `clear` would
/// delete a finished output on the next interruption.
#[cfg(unix)]
#[test]
fn a_guard_is_set_by_guard_and_removed_by_clear() {
    clear();
    assert!(!is_guarded());
    guard(Path::new("/tmp/szstd-guard-test"));
    assert!(is_guarded(), "a path in range is guarded");
    clear();
    assert!(!is_guarded());
}

/// A path the buffer cannot hold is not guarded rather than guarded under a
/// truncated name, which would be some other file's.
#[cfg(unix)]
#[test]
fn a_path_that_does_not_fit_is_left_unguarded() {
    clear();
    let long = "x".repeat(super::imp::PATH_CAPACITY);
    guard(Path::new(&long));
    assert!(
        !is_guarded(),
        "a name that would not fit must not be guarded"
    );
    guard(Path::new(""));
    assert!(!is_guarded(), "nor an empty one");
    clear();
}

#[cfg(not(unix))]
#[test]
fn the_no_op_guard_never_reports_a_guard() {
    guard(Path::new("anything"));
    assert!(!is_guarded());
    clear();
}
