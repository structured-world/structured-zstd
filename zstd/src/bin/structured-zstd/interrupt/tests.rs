use std::path::Path;

use super::imp::is_guarded;
use super::{clear, guard};

/// The guard is a window: it opens when a temporary is being written and
/// closes when the file is in place. A guard left open past `clear` would
/// delete a finished output on the next interruption.
#[cfg(any(unix, windows))]
#[test]
fn a_guard_is_set_by_guard_and_removed_by_clear() {
    super::imp::forget_inherited();
    super::imp::take_default_action();
    clear();
    assert!(!is_guarded());
    guard(Path::new("/tmp/szstd-guard-test"));
    assert!(is_guarded(), "a path in range is guarded");
    clear();
    assert!(!is_guarded());
}

/// A path the buffer cannot hold is not guarded rather than guarded under a
/// truncated name, which would be some other file's.
#[cfg(any(unix, windows))]
#[test]
fn a_path_that_does_not_fit_is_left_unguarded() {
    super::imp::forget_inherited();
    super::imp::take_default_action();
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

/// A process started with `SIGINT` ignored (a background job of a
/// non-interactive shell, a `nohup` run) keeps ignoring it: installing the
/// handler over the inherited disposition would make such a run die on a
/// `Ctrl-C` meant for the foreground, and would hand an ignore marker to a
/// function-pointer binding on the way.
#[cfg(any(unix, windows))]
#[test]
fn an_inherited_ignore_of_sigint_is_kept() {
    super::imp::forget_inherited();
    super::imp::ignore_interrupts();
    guard(Path::new("/tmp/szstd-guard-test"));
    assert!(
        !is_guarded(),
        "with interruptions ignored there is nothing to guard against"
    );
    assert!(
        super::imp::interrupts_ignored(),
        "SIGINT must stay ignored after guard"
    );
    clear();
    assert!(
        super::imp::interrupts_ignored(),
        "and after clear, which restores what guard found"
    );
}

/// `clear` puts back the disposition the first `guard` replaced, the default
/// action for a process started normally, rather than a fixed value.
#[cfg(any(unix, windows))]
#[test]
fn clear_restores_the_action_guard_replaced() {
    super::imp::forget_inherited();
    super::imp::take_default_action();
    guard(Path::new("/tmp/szstd-guard-test"));
    assert!(
        !super::imp::default_action(),
        "the handler is in place while guarded"
    );
    clear();
    assert!(
        super::imp::default_action(),
        "the default action is back once cleared"
    );
}

#[cfg(not(any(unix, windows)))]
#[test]
fn the_no_op_guard_never_reports_a_guard() {
    guard(Path::new("anything"));
    assert!(!is_guarded());
    clear();
}
