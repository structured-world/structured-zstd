//! Which file a path or standard output is, as the filesystem identifies it,
//! so two names for one file (another spelling, a symlink, a hard link, a
//! redirected stdout) compare equal.

use std::fs::{self, File};
use std::io;
use std::path::Path;

/// A file as the filesystem identifies it: device and inode on Unix, volume
/// serial number and the 128-bit file ID on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u64, u128);

/// What [`remove_if`] did with the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The path named the expected file, and that entry is gone.
    Removed,
    /// The path names another file, or none: nothing was removed.
    Changed,
    /// The platform cannot bind the check to the removal: nothing was removed.
    Unconfirmed,
}

/// The file a source was read from, as removing it afterwards needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    /// The file that was opened and read.
    pub id: FileId,
    /// The source's path was a symbolic link to it (followed under `-f`).
    pub through_link: bool,
}

impl Source {
    /// The source `path` names, open as `file`; `None` where the platform
    /// reads no identity.
    pub fn opened(path: &Path, file: &File, metadata: &fs::Metadata) -> Option<Self> {
        Some(Self {
            id: of_open(file, metadata)?,
            through_link: fs::symlink_metadata(path).is_ok_and(|link| link.is_symlink()),
        })
    }
}

/// Whether [`remove_if`] can bind its check to the removal here; where it
/// cannot, it removes nothing and answers [`Removal::Unconfirmed`].
#[cfg(test)]
pub const BINDS_REMOVAL: bool = sys::BINDS_REMOVAL;

/// Remove the entry `path` names if it is the file `expected`, checking and
/// removing through one handle so no rename between the two can redirect the
/// removal elsewhere.
///
/// `through_link` says the path was itself a symbolic link to `expected` when
/// it was processed: the link is then checked by what it points at and the
/// link is removed, as removing a path removes the name. Otherwise a link
/// found at the path is not the file processed and is kept.
pub fn remove_if(path: &Path, expected: FileId, through_link: bool) -> io::Result<Removal> {
    sys::remove_if(path, expected, through_link, || {})
}

/// [`remove_if`] with `between` run after the check and before the removal,
/// where a concurrent rename would land.
#[cfg(test)]
pub fn remove_if_racing(
    path: &Path,
    expected: FileId,
    between: impl FnOnce(),
) -> io::Result<Removal> {
    sys::remove_if(path, expected, false, between)
}

/// The file `path` names, following symlinks; `None` when there is none, or
/// the platform reads no identity.
pub fn of_path(path: &Path) -> Option<FileId> {
    sys::of_path(path)
}

/// The file `file` is open on, whatever its path names by now. `metadata` is
/// what the open file reported; Unix reads the identity from it.
pub fn of_open(file: &File, metadata: &fs::Metadata) -> Option<FileId> {
    sys::of_open(file, metadata)
}

/// The file standard output writes to. A pipe or a console is one too, so it
/// compares unequal to every path that is not it.
pub fn of_stdout() -> Option<FileId> {
    sys::of_stdout()
}

/// The file standard input reads, as [`of_stdout`] tells standard output.
pub fn of_stdin() -> Option<FileId> {
    sys::of_stdin()
}

/// The file standard error writes to, as [`of_stdout`] tells standard output.
pub fn of_stderr() -> Option<FileId> {
    sys::of_stderr()
}

#[cfg(unix)]
mod sys {
    use super::FileId;
    use std::fs::{self, File};
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    #[cfg(test)]
    pub use unlink::BINDS_REMOVAL;
    pub use unlink::remove_if;

    fn of(metadata: &fs::Metadata) -> FileId {
        FileId(metadata.dev(), u128::from(metadata.ino()))
    }

    /// Another Unix: the open flag values are not spelled out here, so the
    /// check cannot be bound to the removal and nothing is removed.
    #[cfg(not(any(
        all(
            any(target_os = "linux", target_os = "android"),
            any(
                target_arch = "x86",
                target_arch = "x86_64",
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "powerpc",
                target_arch = "powerpc64",
                target_arch = "s390x",
                target_arch = "loongarch64"
            )
        ),
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )))]
    mod unlink {
        use super::super::{FileId, Removal};
        use std::io;
        use std::path::Path;

        #[cfg(test)]
        pub const BINDS_REMOVAL: bool = false;

        pub fn remove_if(_: &Path, _: FileId, _: bool, _: impl FnOnce()) -> io::Result<Removal> {
            Ok(Removal::Unconfirmed)
        }
    }

    #[cfg(any(
        all(
            any(target_os = "linux", target_os = "android"),
            any(
                target_arch = "x86",
                target_arch = "x86_64",
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "riscv64",
                target_arch = "powerpc",
                target_arch = "powerpc64",
                target_arch = "s390x",
                target_arch = "loongarch64"
            )
        ),
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    mod unlink {
        use super::super::{FileId, Removal};
        use super::of;
        use core::ffi::{c_char, c_int};
        use std::ffi::CString;
        use std::fs::File;
        use std::io;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;

        #[cfg(test)]
        pub const BINDS_REMOVAL: bool = true;

        unsafe extern "C" {
            fn open(path: *const c_char, flags: c_int, ...) -> c_int;
            fn openat(dirfd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
            fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int;
        }

        /// How the parent directory is opened: for search only (`O_PATH` on
        /// Linux, `O_SEARCH` elsewhere), since unlinking an entry needs write
        /// and search permission on the directory, not read. OpenBSD and
        /// DragonFly have no such flag and open for reading. Every kernel Rust
        /// supports knows its flag (`O_PATH` since Linux 2.6.39, `O_SEARCH`
        /// since FreeBSD 8 and NetBSD 7); macOS before 13 ignores the `O_EXEC`
        /// bit of its `O_SEARCH` and opens for reading.
        ///
        /// Outside Linux `O_DIRECTORY` is part of it, so a FIFO put where the
        /// directory was is refused rather than opened, which would wait for
        /// a writer. `O_PATH` opens nothing, so Linux needs no such guard.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const DIR_FLAGS: c_int = 0o10000000;
        #[cfg(target_vendor = "apple")]
        const DIR_FLAGS: c_int = 0x4000_0000 | 0x0010_0000;
        #[cfg(target_os = "freebsd")]
        const DIR_FLAGS: c_int = 0x0004_0000 | 0x0002_0000;
        #[cfg(target_os = "netbsd")]
        const DIR_FLAGS: c_int = 0x0080_0000 | 0x0020_0000;
        #[cfg(target_os = "openbsd")]
        const DIR_FLAGS: c_int = 0x0002_0000;
        #[cfg(target_os = "dragonfly")]
        const DIR_FLAGS: c_int = 0x0800_0000;

        /// The parent directory as a descriptor for `openat` and `unlinkat`.
        fn open_dir(parent: &Path) -> io::Result<OwnedFd> {
            let parent = CString::new(parent.as_os_str().as_bytes()).map_err(io::Error::other)?;
            // SAFETY: `parent` is NUL-terminated; no `O_CREAT`, so no mode
            // argument is read.
            let fd = unsafe { open(parent.as_ptr(), DIR_FLAGS) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just returned by `open` and nothing else owns it.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        /// `O_RDONLY | O_NONBLOCK | O_NOCTTY`: the entry is opened only to read
        /// its identity, so a FIFO must not block and a terminal must not
        /// become the controlling one. BSD and Apple systems never take a
        /// controlling terminal on open, so `O_NOCTTY` is 0 there.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const PROBE_FLAGS: c_int = 0o4000 | 0o400;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        const PROBE_FLAGS: c_int = 0x4;

        /// `O_NOFOLLOW`, whose value differs between Linux architectures.
        #[cfg(all(
            any(target_os = "linux", target_os = "android"),
            any(
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "powerpc",
                target_arch = "powerpc64"
            )
        ))]
        const NOFOLLOW: c_int = 0o100000;
        #[cfg(all(
            any(target_os = "linux", target_os = "android"),
            not(any(
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "powerpc",
                target_arch = "powerpc64"
            ))
        ))]
        const NOFOLLOW: c_int = 0o400000;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        const NOFOLLOW: c_int = 0x100;

        /// What `open` with `O_NOFOLLOW` answers for a symbolic link: `ELOOP`
        /// on Linux, Apple and OpenBSD, `EMLINK` on FreeBSD and DragonFly,
        /// `EFTYPE` on NetBSD.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const LINK_ERRNO: i32 = 40;
        #[cfg(any(target_vendor = "apple", target_os = "openbsd"))]
        const LINK_ERRNO: i32 = 62;
        #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
        const LINK_ERRNO: i32 = 31;
        #[cfg(target_os = "netbsd")]
        const LINK_ERRNO: i32 = 79;

        /// The parent directory is opened once and both the check and the
        /// unlink go through it, so renaming any ancestor afterwards changes
        /// nothing. The entry itself can still be swapped inside that
        /// directory between the two, but only by someone who may write the
        /// directory and could remove the entry anyway; unlinking a name
        /// never reaches a file through another directory.
        pub fn remove_if(
            path: &Path,
            expected: FileId,
            through_link: bool,
            between: impl FnOnce(),
        ) -> io::Result<Removal> {
            let Some(name) = path.file_name() else {
                return Ok(Removal::Changed);
            };
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            let parent = parent.unwrap_or(Path::new("."));
            let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
            let dir = match open_dir(parent) {
                Ok(dir) => dir,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Removal::Changed),
                Err(err) => return Err(err),
            };
            let flags = if through_link {
                PROBE_FLAGS
            } else {
                PROBE_FLAGS | NOFOLLOW
            };
            // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated;
            // no `O_CREAT`, so no mode argument is read.
            let fd = unsafe { openat(dir.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(LINK_ERRNO) {
                    return Ok(Removal::Changed);
                }
                return Err(err);
            }
            // SAFETY: `fd` was just returned by `openat` and nothing else owns it.
            let entry = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            if of(&entry.metadata()?) != expected {
                return Ok(Removal::Changed);
            }
            drop(entry);
            between();
            // SAFETY: as for `openat`.
            if unsafe { unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Removal::Removed)
        }
    }

    pub fn of_path(path: &Path) -> Option<FileId> {
        fs::metadata(path).ok().as_ref().map(of)
    }

    pub fn of_open(_: &File, metadata: &fs::Metadata) -> Option<FileId> {
        Some(of(metadata))
    }

    fn of_stream(stream: impl AsFd) -> Option<FileId> {
        let fd = stream.as_fd().try_clone_to_owned().ok()?;
        File::from(fd).metadata().ok().as_ref().map(of)
    }

    pub fn of_stdout() -> Option<FileId> {
        of_stream(std::io::stdout())
    }

    pub fn of_stdin() -> Option<FileId> {
        of_stream(std::io::stdin())
    }

    pub fn of_stderr() -> Option<FileId> {
        of_stream(std::io::stderr())
    }
}

#[cfg(windows)]
mod sys {
    use super::{FileId, Removal};
    use core::ffi::c_void;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::path::Path;

    #[cfg(test)]
    pub const BINDS_REMOVAL: bool = true;

    /// `FILE_FLAG_BACKUP_SEMANTICS`: lets a directory be opened too, as
    /// `fs::metadata` answers for any path on Unix.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;

    /// `FileIdInfo` of `FILE_INFO_BY_HANDLE_CLASS`.
    const FILE_ID_INFO_CLASS: i32 = 18;
    /// `FileDispositionInfo` of `FILE_INFO_BY_HANDLE_CLASS`.
    const FILE_DISPOSITION_INFO_CLASS: i32 = 4;
    /// `FileDispositionInfoEx` of `FILE_INFO_BY_HANDLE_CLASS`.
    const FILE_DISPOSITION_INFO_EX_CLASS: i32 = 21;
    /// `FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS |
    /// FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE`: the name goes at
    /// once, and a read-only file goes too, as `fs::remove_file` removes it.
    const DISPOSITION_FLAGS: u32 = 0x1 | 0x2 | 0x10;

    /// `FILE_ID_INFO`: the volume and the 128-bit file ID. The 64-bit index
    /// of `BY_HANDLE_FILE_INFORMATION` is not unique on ReFS, which is why
    /// this one is read.
    #[repr(C)]
    #[derive(Default)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }

    /// `BY_HANDLE_FILE_INFORMATION`: the 32-bit volume serial and the 64-bit
    /// file index, which every Windows file system reports.
    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            class: i32,
            information: *mut c_void,
            size: u32,
        ) -> i32;

        fn GetFileInformationByHandle(
            file: *mut c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;

        fn SetFileInformationByHandle(
            file: *mut c_void,
            class: i32,
            information: *const c_void,
            size: u32,
        ) -> i32;
    }

    fn of_handle(handle: RawHandle) -> Option<FileId> {
        let mut information = FileIdInfo::default();
        // SAFETY: `handle` is open for the duration of the call (borrowed from
        // a live `File` or one of the process's standard streams), and
        // `information` is a writable `FILE_ID_INFO` of the size passed; zero
        // is the failure.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.cast(),
                FILE_ID_INFO_CLASS,
                (&raw mut information).cast(),
                core::mem::size_of::<FileIdInfo>() as u32,
            )
        };
        if ok != 0 {
            return Some(FileId(
                information.volume_serial_number,
                u128::from_le_bytes(information.file_id),
            ));
        }
        // FAT32, exFAT and some network shares have no `FileIdInfo`; the
        // 64-bit index identifies the file there, as `std::filesystem`'s
        // `equivalent` falls back to it in the MSVC STL.
        let mut index = ByHandleFileInformation::default();
        // SAFETY: as above; `index` is a writable `BY_HANDLE_FILE_INFORMATION`.
        let ok = unsafe { GetFileInformationByHandle(handle.cast(), &raw mut index) };
        (ok != 0).then(|| {
            FileId(
                u64::from(index.volume_serial_number),
                (u128::from(index.file_index_high) << 32) | u128::from(index.file_index_low),
            )
        })
    }

    pub fn of_path(path: &Path) -> Option<FileId> {
        // No access asked for: reading the identity needs none, so a file
        // that cannot be read, or one another process holds open, still
        // answers.
        let file = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .ok()?;
        of_handle(file.as_raw_handle())
    }

    pub fn of_open(file: &File, _: &fs::Metadata) -> Option<FileId> {
        of_handle(file.as_raw_handle())
    }

    /// The entry is opened itself, a link as the link
    /// (`FILE_FLAG_OPEN_REPARSE_POINT`), and the same handle is marked for
    /// deletion. Its identity is read from that handle; for an input that
    /// was a link, from what the link points at, since removing the link
    /// removes only a name.
    pub fn remove_if(
        path: &Path,
        expected: FileId,
        through_link: bool,
        between: impl FnOnce(),
    ) -> io::Result<Removal> {
        let file = match OpenOptions::new()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Removal::Changed),
            Err(err) => return Err(err),
        };
        let found = if through_link {
            of_path(path)
        } else {
            of_handle(file.as_raw_handle())
        };
        match found {
            None => return Ok(Removal::Unconfirmed),
            Some(found) if found != expected => return Ok(Removal::Changed),
            Some(_) => {}
        }
        between();
        let extended = FileDispositionInfoEx {
            flags: DISPOSITION_FLAGS,
        };
        // SAFETY: `file` is open with `DELETE` access and `extended` is a
        // readable `FILE_DISPOSITION_INFO_EX` of the size passed.
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FILE_DISPOSITION_INFO_EX_CLASS,
                (&raw const extended).cast(),
                core::mem::size_of::<FileDispositionInfoEx>() as u32,
            )
        };
        if ok != 0 {
            return Ok(Removal::Removed);
        }
        // File systems before NTFS on Windows 10 1709 refuse the extended
        // class; the plain one marks the same handle, which goes when this,
        // its last handle, closes. `FILE_DISPOSITION_INFO` is one `BOOLEAN`.
        let delete: u8 = 1;
        // SAFETY: as above; `delete` is a readable `FILE_DISPOSITION_INFO`.
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FILE_DISPOSITION_INFO_CLASS,
                (&raw const delete).cast(),
                1,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Removal::Removed)
    }

    /// `FILE_DISPOSITION_INFO_EX`.
    #[repr(C)]
    struct FileDispositionInfoEx {
        flags: u32,
    }

    pub fn of_stdout() -> Option<FileId> {
        of_handle(std::io::stdout().as_raw_handle())
    }

    pub fn of_stdin() -> Option<FileId> {
        of_handle(std::io::stdin().as_raw_handle())
    }

    pub fn of_stderr() -> Option<FileId> {
        of_handle(std::io::stderr().as_raw_handle())
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::{FileId, Removal};
    use std::fs::{self, File};
    use std::io;
    use std::path::Path;

    #[cfg(test)]
    pub const BINDS_REMOVAL: bool = false;

    pub fn of_path(_: &Path) -> Option<FileId> {
        None
    }

    pub fn of_open(_: &File, _: &fs::Metadata) -> Option<FileId> {
        None
    }

    pub fn remove_if(_: &Path, _: FileId, _: bool, _: impl FnOnce()) -> io::Result<Removal> {
        Ok(Removal::Unconfirmed)
    }

    pub fn of_stdout() -> Option<FileId> {
        None
    }

    pub fn of_stdin() -> Option<FileId> {
        None
    }

    pub fn of_stderr() -> Option<FileId> {
        None
    }
}
