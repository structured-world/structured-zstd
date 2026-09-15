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
    assert!(
        super::imp::stored_path().is_empty(),
        "nothing is held before the first guard"
    );
    guard(Path::new("short"));
    assert!(is_guarded(), "a short path is guarded");
    let long = "x".repeat(5000);
    guard(Path::new(&long));
    assert!(is_guarded(), "a path past 4096 units is guarded");
    assert_eq!(
        super::imp::stored_path().len(),
        long.len(),
        "in a buffer grown past the short one's"
    );
    clear();
    guard(Path::new("short"));
    assert!(is_guarded(), "and a short one after it, in the same buffer");
    clear();
    guard(Path::new(""));
    assert!(!is_guarded(), "an empty name is not");
    clear();
}

/// A name with a NUL inside is not one `unlink` can be handed: the handler
/// would remove whatever its prefix names. It is not guarded.
#[cfg(unix)]
#[test]
fn a_name_with_a_nul_inside_is_not_guarded() {
    use std::os::unix::ffi::OsStrExt;
    super::imp::forget_inherited();
    super::imp::take_default_action();
    clear();
    guard(Path::new(std::ffi::OsStr::from_bytes(b"/tmp/szstd\0tail")));
    assert!(!is_guarded());
    clear();
}

/// What an interruption does before the process exits: the published
/// temporary is removed, and from then on a guard publishes nothing more,
/// since the process is ending.
#[cfg(any(unix, windows))]
#[test]
fn an_interruption_removes_the_published_temporary() {
    super::imp::forget_inherited();
    super::imp::take_default_action();
    clear();
    let dir = std::env::temp_dir().join(format!("szstd-remove-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let temporary = dir.join("output.zst.tmp");
    std::fs::write(&temporary, b"partial").unwrap();
    guard(&temporary);
    super::imp::remove_published();
    let left_behind = temporary.exists();
    guard(&dir.join("next.zst.tmp"));
    let guarded_after = is_guarded();
    clear();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!left_behind, "the published temporary was left behind");
    assert!(
        !guarded_after,
        "a guard after the interruption published a name"
    );
}

/// On Windows the handler runs on a thread of its own, so it can read the
/// guarded name while the main thread moves on to the next file. Once an
/// interruption is being handled the process is ending, and a guard taken
/// meanwhile must leave the name the handler may be reading exactly as it
/// was, rather than write the next file's over it and have a mix of the two
/// removed.
#[cfg(any(unix, windows))]
#[test]
fn a_guard_taken_while_an_interruption_is_handled_leaves_the_name_alone() {
    super::imp::forget_inherited();
    super::imp::take_default_action();
    clear();
    guard(Path::new("/tmp/szstd-first"));
    let first = super::imp::stored_path();
    super::imp::begin_handling();
    guard(Path::new("/tmp/szstd-second-file"));
    assert_eq!(
        super::imp::stored_path(),
        first,
        "the name the handler may hold is not rewritten"
    );
    assert!(!is_guarded(), "and nothing new is published");
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
    guard(Path::new("/tmp/szstd-guard-test-next"));
    assert!(
        !is_guarded(),
        "nor for the next file, which reads the inherited ignore it remembered"
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

/// An interruption that lands after the temporary exists and before its name
/// is published still removes it and exits with status 2. The process ends
/// either way, so the scenario runs in a child: this test starts its own
/// binary on itself with the directory to use, and the child raises `SIGINT`
/// from inside the creation.
#[cfg(unix)]
#[test]
fn an_interruption_while_the_temporary_is_created_still_removes_it() {
    const CHILD: &str = "SZSTD_INTERRUPT_WHILE_CREATING";
    unsafe extern "C" {
        fn raise(signum: core::ffi::c_int) -> core::ffi::c_int;
    }
    if let Some(dir) = std::env::var_os(CHILD) {
        super::imp::forget_inherited();
        super::imp::take_default_action();
        let path = std::path::PathBuf::from(dir).join("temporary");
        let _created = super::create_guarded(|| -> std::io::Result<_> {
            let file = std::fs::File::create(&path)?;
            // SAFETY: a plain libc call with a valid signal number.
            unsafe {
                raise(2);
            }
            Ok((path.clone(), file))
        });
        // The interruption ends the process before this.
        std::process::exit(0);
    }

    let dir = std::env::temp_dir().join(format!("szstd-interrupt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "interrupt::tests::an_interruption_while_the_temporary_is_created_still_removes_it",
        ])
        .env(CHILD, &dir)
        .status()
        .unwrap();
    let left_behind = dir.join("temporary").exists();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(status.code(), Some(2), "{status}");
    assert!(!left_behind, "the temporary was left behind");
}

#[cfg(not(any(unix, windows)))]
#[test]
fn the_no_op_guard_never_reports_a_guard() {
    guard(Path::new("anything"));
    assert!(!is_guarded());
    clear();
}
