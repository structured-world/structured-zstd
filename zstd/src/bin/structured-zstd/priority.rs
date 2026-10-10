//! `--priority=rt`: the scheduling a benchmark asks for while it measures,
//! as the reference sets it (`programs/util.h`, `SET_REALTIME_PRIORITY`).

/// The process at raised priority; dropping it puts back the priority it had
/// before, so reading the next input or allocating the next buffers does not
/// run ahead of everything else on the machine.
pub struct Raised(sys::Previous);

impl Drop for Raised {
    fn drop(&mut self) {
        sys::restore(self.0);
    }
}

/// Raise this process to the highest scheduling priority the platform offers
/// a process: nice -20 on POSIX systems, the real-time class on Windows.
/// `None` when the system refused, which it does without the privilege, or
/// when the current priority cannot be read to be put back afterwards; the
/// benchmark then runs at its ordinary priority.
pub fn raise_to_realtime() -> Option<Raised> {
    sys::raise().map(Raised)
}

#[cfg(unix)]
mod sys {
    use core::ffi::c_int;

    /// The nice value to put back.
    pub type Previous = c_int;

    /// `PRIO_PROCESS`: the `which` that names a process, 0 on every POSIX
    /// system.
    const PRIO_PROCESS: c_int = 0;

    unsafe extern "C" {
        fn getpriority(which: c_int, who: u32) -> c_int;
        fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
        #[cfg(any(target_os = "linux", target_os = "fuchsia", target_os = "emscripten"))]
        fn __errno_location() -> *mut c_int;
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "visionos",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        fn __error() -> *mut c_int;
        #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
        fn __errno() -> *mut c_int;
    }

    /// This thread's `errno`, where the platform's C library names it.
    #[cfg(any(target_os = "linux", target_os = "fuchsia", target_os = "emscripten"))]
    fn errno() -> Option<*mut c_int> {
        // SAFETY: returns the calling thread's errno slot.
        Some(unsafe { __errno_location() })
    }

    /// This thread's `errno`, where the platform's C library names it.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    fn errno() -> Option<*mut c_int> {
        // SAFETY: returns the calling thread's errno slot.
        Some(unsafe { __error() })
    }

    /// This thread's `errno`, where the platform's C library names it.
    #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
    fn errno() -> Option<*mut c_int> {
        // SAFETY: returns the calling thread's errno slot.
        Some(unsafe { __errno() })
    }

    /// No known name for `errno` here, so a priority read cannot be checked
    /// and nothing is raised.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "fuchsia",
        target_os = "emscripten",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    fn errno() -> Option<*mut c_int> {
        None
    }

    /// The process's current nice value. `getpriority` returns -1 both for
    /// a nice value of -1 and on failure, so `errno` is cleared first and read
    /// after to tell them apart; without a way to read it, the value is not
    /// trusted.
    pub fn current() -> Option<Previous> {
        let errno = errno()?;
        // SAFETY: the slot is this thread's own errno; plain libc calls.
        unsafe {
            *errno = 0;
            let nice = getpriority(PRIO_PROCESS, 0);
            (nice != -1 || *errno == 0).then_some(nice)
        }
    }

    pub fn raise() -> Option<Previous> {
        let previous = current()?;
        // SAFETY: a plain libc call; `who` 0 names the calling process.
        (unsafe { setpriority(PRIO_PROCESS, 0, -20) } == 0).then_some(previous)
    }

    pub fn restore(previous: Previous) {
        // Lowering one's own priority back is always permitted, and there is
        // nothing to do if it were not.
        // SAFETY: a plain libc call; `who` 0 names the calling process.
        unsafe {
            setpriority(PRIO_PROCESS, 0, previous);
        }
    }
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;

    /// The priority class to put back.
    pub type Previous = u32;

    /// `REALTIME_PRIORITY_CLASS`.
    const REALTIME_PRIORITY_CLASS: u32 = 0x0000_0100;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetPriorityClass(process: *mut c_void) -> u32;
        fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
    }

    pub fn current() -> Option<Previous> {
        // SAFETY: a kernel32 call on the current process's pseudo-handle,
        // which needs no closing; zero is its failure.
        let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
        (class != 0).then_some(class)
    }

    pub fn raise() -> Option<Previous> {
        let previous = current()?;
        // SAFETY: as in `current`.
        (unsafe { SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS) } != 0)
            .then_some(previous)
    }

    pub fn restore(previous: Previous) {
        // SAFETY: as in `current`; lowering the class back is always allowed.
        unsafe {
            SetPriorityClass(GetCurrentProcess(), previous);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    /// Nothing is ever raised here, so nothing is put back.
    pub type Previous = ();

    pub fn current() -> Option<Previous> {
        None
    }

    /// No scheduling to change here, as the reference has none on such a
    /// system either.
    pub fn raise() -> Option<Previous> {
        None
    }

    pub fn restore(_previous: Previous) {}
}

#[cfg(test)]
mod tests;
