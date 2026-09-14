//! These tests set the process's `SIGINT` disposition and the guard's
//! statics, which every output-writing test of this binary also touches
//! through `guard` / `clear`. They are sound only with a process per test,
//! which is how nextest runs them (every CI job that runs them uses it); a
//! lock around these four alone would not stop the writers racing them under
//! the plain harness.

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

/// The guard holds a path of any length: a Windows path in its `\\?\` form
/// runs to 32767 units, and a guard that gave up on a long one would leave
/// that run's temporary behind on an interruption. A name that names nothing
/// is not guarded.
#[cfg(any(unix, windows))]
#[test]
fn a_long_path_is_guarded_and_an_empty_one_is_not() {
    super::imp::forget_inherited();
    super::imp::take_default_action();
    clear();
    let long = "x".repeat(5000);
    guard(Path::new(&long));
    assert!(is_guarded(), "a path past 4096 units is guarded");
    clear();
    guard(Path::new("short"));
    assert!(is_guarded(), "and a short one after it, in the same buffer");
    clear();
    guard(Path::new(""));
    assert!(!is_guarded(), "an empty name is not");
    clear();
}

/// `_wunlink` reaches a path past `MAX_PATH` only in the verbatim `\\?\` form,
/// which is how the standard library created the temporary: a drive path and
/// a UNC share each gain their prefix, and a path already verbatim or naming
/// a device is left alone.
#[cfg(any(unix, windows))]
#[test]
fn a_windows_path_is_guarded_in_its_verbatim_form() {
    let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let verbatim = |text: &str| super::imp::verbatim(&wide(text));
    assert_eq!(verbatim(r"C:\dir\a.zst.tmp"), wide(r"\\?\C:\dir\a.zst.tmp"));
    assert_eq!(
        verbatim(r"\\server\share\a.zst.tmp"),
        wide(r"\\?\UNC\server\share\a.zst.tmp")
    );
    assert_eq!(verbatim(r"\\?\C:\a.zst.tmp"), wide(r"\\?\C:\a.zst.tmp"));
    assert_eq!(verbatim(r"\\.\pipe\name"), wide(r"\\.\pipe\name"));
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
