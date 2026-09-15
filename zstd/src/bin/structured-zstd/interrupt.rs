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
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

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
        pub fn units(path: &Path) -> Vec<PathUnit> {
            path.as_os_str().as_bytes().to_vec()
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

        /// The path as `_wunlink` has to be given it: absolute and verbatim,
        /// since past `MAX_PATH` only the `\\?\` form reaches the file, and
        /// that is the form the standard library created it through.
        pub fn units(path: &Path) -> Vec<PathUnit> {
            match std::path::absolute(path) {
                Ok(absolute) => {
                    let wide: Vec<PathUnit> = absolute.as_os_str().encode_wide().collect();
                    super::verbatim(&wide)
                }
                // With no working directory to resolve against, the name as
                // given is all there is: not verbatim, which a relative name
                // cannot be, and still enough for a path under `MAX_PATH`.
                Err(_) => path.as_os_str().encode_wide().collect(),
            }
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

    /// `absolute` in the verbatim `\\?\` form, which Windows hands to the
    /// filesystem without the `MAX_PATH` limit: `C:\x` becomes `\\?\C:\x` and
    /// `\\server\share\x` becomes `\\?\UNC\server\share\x`, while a path
    /// already verbatim, or one naming a device (`\\.\`), is kept as it is.
    #[cfg(any(windows, test))]
    pub fn verbatim(absolute: &[u16]) -> Vec<u16> {
        const SEPARATOR: u16 = b'\\' as u16;
        let wide = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
        if absolute.starts_with(&wide(r"\\?\")) || absolute.starts_with(&wide(r"\\.\")) {
            return absolute.to_vec();
        }
        let mut verbatim = wide(r"\\?\");
        match absolute.strip_prefix(&[SEPARATOR, SEPARATOR][..]) {
            Some(share) => {
                verbatim.extend(wide(r"UNC\"));
                verbatim.extend_from_slice(share);
            }
            None => verbatim.extend_from_slice(absolute),
        }
        verbatim
    }

    /// Where the guarded path is kept, NUL-terminated, for the handler: a
    /// buffer of `CAPACITY` units that grows to the longest path guarded.
    /// Only `publish`, on the main thread, writes or replaces it, and only once
    /// no handler can be reading it (see `HANDLING`).
    static BUFFER: AtomicPtr<PathUnit> = AtomicPtr::new(ptr::null_mut());
    static CAPACITY: AtomicUsize = AtomicUsize::new(0);

    /// Points at `BUFFER` while a file is guarded, null otherwise.
    static ARTEFACT: AtomicPtr<PathUnit> = AtomicPtr::new(ptr::null_mut());

    /// Raised by the handler before it reads `ARTEFACT`, and never lowered:
    /// the handler ends the process.
    ///
    /// On Windows the handler runs on a thread of its own, so it can hold the
    /// published name while the main thread moves on to the next file.
    /// `publish` clears `ARTEFACT` first and reads this second, the handler
    /// raises this first and reads `ARTEFACT` second, all sequentially
    /// consistent: either the handler finds no name, or `publish` finds it
    /// handling and leaves the buffer as it is. So no name is rewritten, or
    /// freed, under the handler.
    static HANDLING: AtomicBool = AtomicBool::new(false);

    /// What `SIGINT` was before the handler first went in, `SIG_ERR` until
    /// then: the disposition `clear` puts back.
    static INHERITED: AtomicUsize = AtomicUsize::new(SIG_ERR);

    /// Up while `create_guarded` creates a temporary and publishes its name:
    /// an interruption then is left to it, to handle once the name is there
    /// to remove, rather than ending the process with the file unnamed.
    static PUBLISHING: AtomicBool = AtomicBool::new(false);

    /// Raised by every interruption, and never lowered: the process ends.
    ///
    /// The handler raises this and then reads `PUBLISHING`; `create_guarded`
    /// lowers `PUBLISHING` and then reads this, all sequentially consistent.
    /// So at least one of the two sees the other: an interruption during
    /// publication is handled by the handler, by `create_guarded`, or by both,
    /// which remove the same name and exit with the same status.
    static DEFERRED: AtomicBool = AtomicBool::new(false);

    /// Async-signal-safe by construction: atomics, `unlink`, `write` and
    /// `_exit` only, no allocation, no locks, no formatting.
    extern "C" fn on_interrupt(_signum: c_int) {
        DEFERRED.store(true, Ordering::SeqCst);
        if PUBLISHING.load(Ordering::SeqCst) {
            return;
        }
        end_interrupted();
    }

    /// Remove the published temporary, if any, and exit with status 2.
    fn end_interrupted() -> ! {
        remove_published();
        // SAFETY: a plain exit with a constant status.
        unsafe {
            sys::_exit(2);
        }
    }

    /// What an interruption does before the process exits: remove the
    /// published temporary, if any, and end the line on stderr.
    pub fn remove_published() {
        HANDLING.store(true, Ordering::SeqCst);
        let path = ARTEFACT.load(Ordering::SeqCst);
        // SAFETY: a non-null `path` points at a buffer that holds a
        // NUL-terminated string from the moment the pointer was published,
        // and `guard` neither rewrites nor frees it once `HANDLING` is up.
        if !path.is_null() {
            unsafe {
                sys::remove(path);
            }
        }
        sys::newline();
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

    /// Create a temporary through `create`, which names it, and remove it if
    /// the process is interrupted before [`clear`] is called.
    ///
    /// No moment passes with the temporary on disk and no name for the handler
    /// to remove: the handler is in place before the file is created, and an
    /// interruption between the two steps waits for the name to be published.
    pub fn create_guarded<T, E>(
        create: impl FnOnce() -> Result<(PathBuf, T), E>,
    ) -> Result<(PathBuf, T), E> {
        if !install() {
            return create();
        }
        PUBLISHING.store(true, Ordering::SeqCst);
        let created = create();
        if let Ok((path, _)) = &created {
            publish(path);
        }
        PUBLISHING.store(false, Ordering::SeqCst);
        if DEFERRED.load(Ordering::SeqCst) {
            end_interrupted();
        }
        created
    }

    /// Guard an existing `path`, as [`create_guarded`] does the file it
    /// creates (for tests).
    #[cfg(test)]
    pub fn guard(path: &Path) {
        let _guarded =
            create_guarded(|| Ok::<_, core::convert::Infallible>((path.to_path_buf(), ())));
    }

    /// Hand `path` to the handler.
    fn publish(path: &Path) {
        ARTEFACT.store(ptr::null_mut(), Ordering::SeqCst);
        // An interruption already being handled is ending the process, and its
        // handler may still be reading the last name; that name is left alone.
        if HANDLING.load(Ordering::SeqCst) {
            return;
        }
        // An empty name names nothing. Asked of the name as given, before
        // `units` turns it into the form the handler needs: on Windows that
        // form carries a prefix even when there is nothing after it.
        if path.as_os_str().is_empty() {
            return;
        }
        let units = sys::units(path);
        // A terminator inside the name is not a name the handler can be given.
        if units.contains(&0) {
            return;
        }
        let needed = units.len() + 1;
        let mut buffer = BUFFER.load(Ordering::SeqCst);
        let capacity = CAPACITY.load(Ordering::SeqCst);
        if needed > capacity {
            // Paths are bounded by the platform far below where doubling
            // could overflow.
            let grown = needed.next_power_of_two();
            let outgrown = buffer;
            buffer = Box::into_raw(vec![0; grown].into_boxed_slice()).cast::<PathUnit>();
            BUFFER.store(buffer, Ordering::SeqCst);
            CAPACITY.store(grown, Ordering::SeqCst);
            if !outgrown.is_null() {
                // SAFETY: `outgrown` came from `Box::into_raw` on a slice of
                // `capacity` units, is no longer published, and no handler
                // holds it (`HANDLING` was down after `ARTEFACT` was cleared).
                unsafe {
                    drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                        outgrown, capacity,
                    )));
                }
            }
        }
        // SAFETY: `buffer` holds `CAPACITY >= needed` units, and the handler
        // reads it only through `ARTEFACT`, which is null for this write.
        unsafe {
            ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
            *buffer.add(units.len()) = 0;
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

    /// Mark an interruption as being handled, as the handler does on entry
    /// (for tests).
    #[cfg(test)]
    pub fn begin_handling() {
        HANDLING.store(true, Ordering::SeqCst);
    }

    /// The name held in the guard's buffer, whether or not it is published
    /// (for tests).
    #[cfg(test)]
    pub fn stored_path() -> Vec<PathUnit> {
        let buffer = BUFFER.load(Ordering::SeqCst);
        let mut units = Vec::new();
        if buffer.is_null() {
            return units;
        }
        // SAFETY: a non-null buffer holds a NUL-terminated name within its
        // capacity, and only `guard` on this thread writes it.
        unsafe {
            let mut at = 0;
            while *buffer.add(at) != 0 {
                units.push(*buffer.add(at));
                at += 1;
            }
        }
        units
    }

    /// The units the guard stores for `path`: the form the handler removes
    /// (verbatim and absolute on Windows) (for tests).
    #[cfg(test)]
    pub fn units_of(path: &Path) -> Vec<PathUnit> {
        sys::units(path)
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
    use std::path::PathBuf;

    pub fn create_guarded<T, E>(
        create: impl FnOnce() -> Result<(PathBuf, T), E>,
    ) -> Result<(PathBuf, T), E> {
        create()
    }

    #[cfg(test)]
    pub fn guard(_path: &std::path::Path) {}

    pub fn clear() {}

    #[cfg(test)]
    pub fn is_guarded() -> bool {
        false
    }
}

#[cfg(test)]
pub use imp::guard;
pub use imp::{clear, create_guarded};

#[cfg(test)]
mod tests;
