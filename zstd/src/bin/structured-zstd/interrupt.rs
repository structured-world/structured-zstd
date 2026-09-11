//! Removes the output being written when the run is interrupted.
//!
//! A `Ctrl-C` in the middle of a file would otherwise leave the partial
//! temporary beside the source. The reference command installs a `SIGINT`
//! handler that unlinks the artefact and exits with status 2; this does the
//! same, through the C library the standard library already links, so the
//! tool takes on no dependency for it. On Windows the C runtime's `signal`
//! delivers the console's `Ctrl-C` as `SIGINT` the same way. A `SIGINT` the
//! process inherited as ignored stays ignored, as a background job of a
//! non-interactive shell or a `nohup` run expects; the reference command
//! replaces it. Platforms with neither get the no-op version and keep the
//! temporary on interruption.

#[cfg(any(unix, windows))]
mod imp {
    use core::ffi::c_int;
    use std::path::Path;
    use std::ptr;
    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    use sys::PathUnit;

    /// The C `sighandler_t`: `SIG_DFL`, `SIG_IGN`, `SIG_ERR` or a handler's
    /// address. An integer rather than a function pointer, since the first
    /// three are markers, not addresses of anything callable.
    type SigHandler = usize;
    #[cfg(test)]
    const SIG_DFL: SigHandler = 0;
    const SIG_IGN: SigHandler = 1;
    const SIG_ERR: SigHandler = usize::MAX;

    /// `SIGINT` has this number on every POSIX system and in the Windows C
    /// runtime.
    const SIGINT: c_int = 2;

    #[cfg(unix)]
    mod sys {
        use core::ffi::{c_char, c_int, c_void};
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;

        /// A unit of the path as `unlink` takes it: a byte.
        pub type PathUnit = u8;

        unsafe extern "C" {
            pub fn signal(signum: c_int, handler: usize) -> usize;
            fn unlink(path: *const c_char) -> c_int;
            fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
            pub fn _exit(status: c_int) -> !;
        }

        /// The path's units in the order `unlink` reads them.
        pub fn units(path: &Path) -> impl Iterator<Item = PathUnit> + '_ {
            path.as_os_str().as_bytes().iter().copied()
        }

        /// # Safety
        /// `path` points at a NUL-terminated string.
        pub unsafe fn remove(path: *const PathUnit) {
            unsafe {
                unlink(path.cast());
            }
        }

        /// A newline on stderr, as the reference command prints one before
        /// exiting.
        pub fn newline() {
            // SAFETY: a plain libc call on a valid buffer.
            unsafe {
                write(2, b"\n".as_ptr().cast(), 1);
            }
        }
    }

    #[cfg(windows)]
    mod sys {
        use core::ffi::{c_int, c_uint, c_void};
        use std::os::windows::ffi::OsStrExt;
        use std::path::Path;

        /// A unit of the path as `_wunlink` takes it: a UTF-16 code unit.
        pub type PathUnit = u16;

        #[cfg_attr(
            all(target_env = "msvc", not(target_feature = "crt-static")),
            link(name = "msvcrt")
        )]
        #[cfg_attr(
            all(target_env = "msvc", target_feature = "crt-static"),
            link(name = "libcmt")
        )]
        unsafe extern "C" {
            pub fn signal(signum: c_int, handler: usize) -> usize;
            fn _wunlink(path: *const u16) -> c_int;
            fn _write(fd: c_int, buf: *const c_void, count: c_uint) -> c_int;
            pub fn _exit(status: c_int) -> !;
        }

        /// The path's units in the order `_wunlink` reads them.
        pub fn units(path: &Path) -> impl Iterator<Item = PathUnit> + '_ {
            path.as_os_str().encode_wide()
        }

        /// # Safety
        /// `path` points at a NUL-terminated wide string.
        pub unsafe fn remove(path: *const PathUnit) {
            unsafe {
                _wunlink(path);
            }
        }

        /// A newline on stderr, as the reference command prints one before
        /// exiting.
        pub fn newline() {
            // SAFETY: a plain C runtime call on a valid buffer.
            unsafe {
                _write(2, b"\n".as_ptr().cast(), 1);
            }
        }
    }

    /// Longest path the guard covers, terminator included. A longer temporary
    /// is not guarded rather than truncated to a name that is not the file's.
    pub const PATH_CAPACITY: usize = 4096;

    /// The guarded path, NUL-terminated, or all zeros. Written only while
    /// `ARTEFACT` is null, so the handler never reads a half-written name.
    static mut PATH: [PathUnit; PATH_CAPACITY] = [0; PATH_CAPACITY];

    /// Points into `PATH` while a file is guarded, null otherwise.
    static ARTEFACT: AtomicPtr<PathUnit> = AtomicPtr::new(ptr::null_mut());

    /// What `SIGINT` was before the handler first went in, `SIG_ERR` until
    /// then: the disposition `clear` puts back.
    static INHERITED: AtomicUsize = AtomicUsize::new(SIG_ERR);

    /// Async-signal-safe by construction: `unlink`, `write` and `_exit`
    /// only, no allocation, no locks, no formatting.
    extern "C" fn on_interrupt(_signum: c_int) {
        let path = ARTEFACT.load(Ordering::SeqCst);
        // SAFETY: a non-null `path` points at `PATH`, which holds a
        // NUL-terminated string from the moment the pointer was published
        // and is not rewritten until the pointer has been cleared.
        if !path.is_null() {
            unsafe {
                sys::remove(path);
            }
        }
        sys::newline();
        // SAFETY: a plain exit with a constant status.
        unsafe {
            sys::_exit(2);
        }
    }

    /// Put `on_interrupt` in place, and say whether it is. A `SIGINT` the
    /// process inherited as ignored is left ignored: `signal` is the only
    /// portable way to learn the current disposition and replaces it while
    /// doing so, so an ignore found on the first call is put straight back
    /// (the classic idiom) and remembered, and later calls do not touch it.
    fn install() -> bool {
        let handler = on_interrupt as *const () as SigHandler;
        match INHERITED.load(Ordering::SeqCst) {
            SIG_IGN => false,
            SIG_ERR => {
                // SAFETY: plain libc calls with a valid handler address.
                let previous = unsafe { sys::signal(SIGINT, handler) };
                if previous == SIG_IGN {
                    unsafe {
                        sys::signal(SIGINT, SIG_IGN);
                    }
                }
                INHERITED.store(previous, Ordering::SeqCst);
                previous != SIG_IGN && previous != SIG_ERR
            }
            _ => {
                // SAFETY: as above.
                let previous = unsafe { sys::signal(SIGINT, handler) };
                previous != SIG_ERR
            }
        }
    }

    /// Remove `path` if the process is interrupted before [`clear`] is called.
    pub fn guard(path: &Path) {
        ARTEFACT.store(ptr::null_mut(), Ordering::SeqCst);
        let buffer = (&raw mut PATH).cast::<PathUnit>();
        let mut len = 0;
        for unit in sys::units(path) {
            // A terminator inside the name, or a name that would not leave
            // room for one, is not a name the handler can be given.
            if unit == 0 || len >= PATH_CAPACITY - 1 {
                return;
            }
            // SAFETY: the handler reads `PATH` only through `ARTEFACT`, which
            // is null for the length of this write; raw pointer access keeps
            // no reference to the static alive, and `len` is in range.
            unsafe {
                *buffer.add(len) = unit;
            }
            len += 1;
        }
        if len == 0 {
            return;
        }
        // SAFETY: `len < PATH_CAPACITY`, and as above.
        unsafe {
            *buffer.add(len) = 0;
        }
        if !install() {
            return;
        }
        ARTEFACT.store(buffer, Ordering::SeqCst);
    }

    /// Stop guarding: an interruption from here on keeps the file and takes
    /// the action the process started with.
    pub fn clear() {
        ARTEFACT.store(ptr::null_mut(), Ordering::SeqCst);
        // An inherited ignore was never replaced, and an unknown disposition
        // (no guard yet, or a failed install) has nothing to put back.
        let inherited = INHERITED.load(Ordering::SeqCst);
        if inherited != SIG_IGN && inherited != SIG_ERR {
            // SAFETY: restoring a disposition `signal` itself returned.
            unsafe {
                sys::signal(SIGINT, inherited);
            }
        }
    }

    /// Whether a file is currently guarded (for tests).
    #[cfg(test)]
    pub fn is_guarded() -> bool {
        !ARTEFACT.load(Ordering::SeqCst).is_null()
    }

    /// Forget what an earlier guard found, as a fresh process would not know
    /// it (for tests).
    #[cfg(test)]
    pub fn forget_inherited() {
        INHERITED.store(SIG_ERR, Ordering::SeqCst);
    }

    /// Make the process ignore `SIGINT`, as a parent may have left it (for
    /// tests).
    #[cfg(test)]
    pub fn ignore_interrupts() {
        // SAFETY: a plain libc call with a marker value.
        unsafe {
            sys::signal(SIGINT, SIG_IGN);
        }
    }

    /// Give `SIGINT` its default action (for tests).
    #[cfg(test)]
    pub fn take_default_action() {
        // SAFETY: a plain libc call with a marker value.
        unsafe {
            sys::signal(SIGINT, SIG_DFL);
        }
    }

    /// The current disposition of `SIGINT`, read by replacing it and putting
    /// it back (for tests).
    #[cfg(test)]
    fn disposition() -> SigHandler {
        // SAFETY: plain libc calls; the second restores what the first found.
        unsafe {
            let current = sys::signal(SIGINT, SIG_IGN);
            sys::signal(SIGINT, current);
            current
        }
    }

    /// Whether `SIGINT` is ignored right now (for tests).
    #[cfg(test)]
    pub fn interrupts_ignored() -> bool {
        disposition() == SIG_IGN
    }

    /// Whether `SIGINT` takes its default action right now (for tests).
    #[cfg(test)]
    pub fn default_action() -> bool {
        disposition() == SIG_DFL
    }
}

#[cfg(not(any(unix, windows)))]
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
