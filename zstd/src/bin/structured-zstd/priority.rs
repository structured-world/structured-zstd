//! `--priority=rt`: the scheduling a benchmark asks for before it measures,
//! as the reference sets it (`programs/util.h`, `SET_REALTIME_PRIORITY`).

/// Raise this process to the highest scheduling priority the platform offers
/// a process: nice -20 on POSIX systems, the real-time class on Windows.
/// `false` when the system refused, which it does without the privilege; the
/// benchmark then runs at its ordinary priority, as the reference's does.
pub fn raise_to_realtime() -> bool {
    sys::raise()
}

#[cfg(unix)]
mod sys {
    use core::ffi::c_int;

    /// `PRIO_PROCESS`: the `which` that names a process, 0 on every POSIX
    /// system.
    const PRIO_PROCESS: c_int = 0;

    unsafe extern "C" {
        fn setpriority(which: c_int, who: u32, prio: c_int) -> c_int;
    }

    pub fn raise() -> bool {
        // SAFETY: a plain libc call; `who` 0 names the calling process.
        unsafe { setpriority(PRIO_PROCESS, 0, -20) == 0 }
    }
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;

    /// `REALTIME_PRIORITY_CLASS`.
    const REALTIME_PRIORITY_CLASS: u32 = 0x0000_0100;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn SetPriorityClass(process: *mut c_void, class: u32) -> i32;
    }

    pub fn raise() -> bool {
        // SAFETY: plain kernel32 calls on the current process's pseudo-handle,
        // which needs no closing.
        unsafe { SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS) != 0 }
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    /// No scheduling to change here, as the reference has none on such a
    /// system either.
    pub fn raise() -> bool {
        false
    }
}
