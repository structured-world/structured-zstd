//! Removes the output being written when the run is interrupted.
//!
//! A `Ctrl-C` in the middle of a file would otherwise leave the partial
//! temporary beside the source. The reference command installs a `SIGINT`
//! handler that unlinks the artefact and exits with status 2; this does the
//! same, through the C library the standard library already links, so the
//! tool takes on no dependency for it. Platforms without POSIX signals get
//! the no-op version and keep the temporary on interruption.

#[cfg(unix)]
mod imp {
    use core::ffi::{c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;
    use std::sync::atomic::{AtomicPtr, Ordering};

    type Handler = extern "C" fn(c_int);

    unsafe extern "C" {
        fn signal(signum: c_int, handler: Option<Handler>) -> Option<Handler>;
        fn unlink(path: *const c_char) -> c_int;
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn _exit(status: c_int) -> !;
    }

    /// `SIGINT` has this number on every POSIX system.
    const SIGINT: c_int = 2;

    /// Longest path the guard covers, NUL included. A longer temporary is not
    /// guarded rather than truncated to a name that is not the file's.
    pub const PATH_CAPACITY: usize = 4096;

    /// The guarded path as a C string, or all zeros. Written only while
    /// `ARTEFACT` is null, so the handler never reads a half-written name.
    static mut PATH: [u8; PATH_CAPACITY] = [0; PATH_CAPACITY];

    /// Points into `PATH` while a file is guarded, null otherwise.
    static ARTEFACT: AtomicPtr<c_char> = AtomicPtr::new(ptr::null_mut());

    /// Async-signal-safe by construction: `unlink`, `write` and `_exit`
    /// only, no allocation, no locks, no formatting.
    extern "C" fn on_interrupt(_signum: c_int) {
        let path = ARTEFACT.load(Ordering::SeqCst);
        // SAFETY: a non-null `path` points at `PATH`, which holds a
        // NUL-terminated string from the moment the pointer was published
        // and is not rewritten until the pointer has been cleared.
        if !path.is_null() {
            unsafe {
                unlink(path);
            }
        }
        // SAFETY: plain libc calls on a valid buffer and a constant status.
        unsafe {
            write(2, b"\n".as_ptr().cast(), 1);
            _exit(2);
        }
    }

    /// Remove `path` if the process is interrupted before [`clear`] is called.
    pub fn guard(path: &Path) {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.len() >= PATH_CAPACITY || bytes.contains(&0) {
            return;
        }
        ARTEFACT.store(ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: the handler reads `PATH` only through `ARTEFACT`, which is
        // null for the length of this write; raw pointer access keeps no
        // reference to the static alive.
        unsafe {
            let buffer = (&raw mut PATH).cast::<u8>();
            ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
            *buffer.add(bytes.len()) = 0;
            ARTEFACT.store(buffer.cast::<c_char>(), Ordering::SeqCst);
            signal(SIGINT, Some(on_interrupt));
        }
    }

    /// Stop guarding: an interruption from here on keeps the file and takes
    /// the default action.
    pub fn clear() {
        ARTEFACT.store(ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: restoring the default disposition is always valid.
        unsafe {
            signal(SIGINT, None);
        }
    }

    /// Whether a file is currently guarded (for tests).
    #[cfg(test)]
    pub fn is_guarded() -> bool {
        !ARTEFACT.load(Ordering::SeqCst).is_null()
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    pub fn guard(_path: &Path) {}

    pub fn clear() {}

    #[cfg(test)]
    pub fn is_guarded() -> bool {
        false
    }
}

pub use imp::{clear, guard};

#[cfg(test)]
mod tests;
