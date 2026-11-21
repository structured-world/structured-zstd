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
    /// The source's path was a symbolic link to it (followed under `-f`):
    /// the link's own identity.
    pub link: Option<FileId>,
}

impl Source {
    /// The source opened as the file `id` (from [`of_open`]), whose path
    /// named `entry` (from [`of_entry`]) just before it was opened; `None`
    /// where the platform reads no identity.
    ///
    /// An entry other than the file opened is a link the open followed. It is
    /// read before the open rather than after, so a link put at the path once
    /// the file is open is not taken for the one the source was named
    /// through. An entry swapped in between can only be taken for a link to
    /// the file opened, and removing such a link removes no file.
    pub fn opened(entry: Option<FileId>, id: Option<FileId>) -> Option<Self> {
        let (entry, id) = (entry?, id?);
        Some(Self {
            id,
            link: (entry != id).then_some(entry),
        })
    }
}

/// Whether [`remove_if`] can bind its check to the removal here; where it
/// cannot, it removes nothing and answers [`Removal::Unconfirmed`].
#[cfg(test)]
pub const BINDS_REMOVAL: bool = sys::BINDS_REMOVAL;

/// Whether [`remove_if`] can also bind the check of a source named through a
/// symbolic link; where it cannot, such a source is kept as
/// [`Removal::Unconfirmed`].
#[cfg(test)]
pub const BINDS_LINK_REMOVAL: bool = sys::BINDS_LINK_REMOVAL;

/// Remove the entry `path` names if it is the file `expected`, checking and
/// removing through one handle (the parent directory on Unix, the entry on
/// Windows), so renaming an ancestor between the two cannot redirect the
/// removal elsewhere. On Unix the entry itself can still be replaced inside
/// that directory between the check and the unlink, as upstream zstd's
/// `FIO_removeFile` (fileio.c) states for its own: only by someone who may
/// write the directory, and so could remove the entry anyway.
///
/// `link` is the identity of the symbolic link the path was when it was
/// processed: the entry must then still be that link and lead to
/// `expected`, and the link is removed, as removing a path removes the name.
/// Where the link cannot be checked in the handle it is removed through
/// (Windows), it is kept as [`Removal::Unconfirmed`]. Without `link`, a link
/// found at the path is not the file processed and is kept.
pub fn remove_if(path: &Path, expected: FileId, link: Option<FileId>) -> io::Result<Removal> {
    sys::remove_if(path, expected, link, || {}, || {})
}

/// [`remove_if`] with `between` run after the check and before the removal,
/// where a concurrent rename would land.
#[cfg(test)]
pub fn remove_if_racing(
    path: &Path,
    expected: FileId,
    between: impl FnOnce(),
) -> io::Result<Removal> {
    sys::remove_if(path, expected, None, || {}, between)
}

/// [`remove_if`] with `opened` run once the directory (Unix) or the entry
/// (Windows) is held and before anything is checked.
#[cfg(test)]
pub fn remove_if_after_open(
    path: &Path,
    expected: FileId,
    link: Option<FileId>,
    opened: impl FnOnce(),
) -> io::Result<Removal> {
    sys::remove_if(path, expected, link, opened, || {})
}

/// The file `path` names, following symlinks; `None` when there is none, or
/// the platform reads no identity.
pub fn of_path(path: &Path) -> Option<FileId> {
    sys::of_path(path)
}

/// The file `path` named when it was looked up and reported `metadata`
/// (`fs::metadata`, following symlinks). Unix reads the identity from
/// `metadata`, so the lookup is not repeated; Windows looks the path up
/// again, as `metadata` carries no file ID there.
pub fn of_named(path: &Path, metadata: &fs::Metadata) -> Option<FileId> {
    sys::of_named(path, metadata)
}

/// The entry `path` names itself, a symbolic link as the link; `None` when
/// there is none, or the platform reads no identity.
pub fn of_entry(path: &Path) -> Option<FileId> {
    sys::of_entry(path)
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

    pub use unlink::remove_if;
    #[cfg(test)]
    pub use unlink::{BINDS_LINK_REMOVAL, BINDS_REMOVAL};

    fn of(metadata: &fs::Metadata) -> FileId {
        FileId(metadata.dev(), u128::from(metadata.ino()))
    }

    /// Another Unix: how its parent directory is opened for search is not
    /// spelled out here, so the check cannot be bound to the removal and
    /// nothing is removed.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
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

        #[cfg(test)]
        pub const BINDS_LINK_REMOVAL: bool = false;

        pub fn remove_if(
            _: &Path,
            _: FileId,
            _: Option<FileId>,
            _: impl FnOnce(),
            _: impl FnOnce(),
        ) -> io::Result<Removal> {
            Ok(Removal::Unconfirmed)
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    mod unlink {
        use super::super::{FileId, Removal};
        use core::ffi::c_int;
        use core::mem::MaybeUninit;
        use std::ffi::{CStr, CString};
        use std::io;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;

        #[cfg(test)]
        pub const BINDS_REMOVAL: bool = true;

        #[cfg(test)]
        pub const BINDS_LINK_REMOVAL: bool = true;

        #[cfg(not(all(
            any(target_os = "linux", target_os = "android"),
            not(any(target_env = "musl", target_env = "ohos"))
        )))]
        use libc::{fstatat, stat as Stat};
        /// The 64-bit `fstatat` of 32-bit glibc and Bionic: the default one
        /// there answers `EOVERFLOW` for a file of 2 GiB or more. musl's is
        /// 64-bit already.
        #[cfg(all(
            any(target_os = "linux", target_os = "android"),
            not(any(target_env = "musl", target_env = "ohos"))
        ))]
        use libc::{fstatat64 as fstatat, stat64 as Stat};

        /// How the parent directory is opened, as upstream zstd opens it
        /// (util.c `UTIL_openParentDir`): for search only, since unlinking an
        /// entry needs write and search permission on the directory, not
        /// read. `O_PATH` on Linux; `O_SEARCH` on Apple systems, FreeBSD
        /// (whose `O_PATH` came only in 14) and NetBSD; OpenBSD and DragonFly
        /// have neither and open for reading, as macOS before 13 does too,
        /// ignoring the `O_EXEC` bit of its `O_SEARCH`.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const SEARCH: c_int = libc::O_PATH;
        #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "netbsd"))]
        const SEARCH: c_int = libc::O_SEARCH;
        #[cfg(any(target_os = "openbsd", target_os = "dragonfly"))]
        const SEARCH: c_int = libc::O_RDONLY;

        /// The parent directory as a descriptor for `fstatat` and
        /// `unlinkat`. `O_DIRECTORY` refuses a FIFO put where the directory
        /// was, which opening for reading would wait on.
        fn open_dir(parent: &Path) -> io::Result<OwnedFd> {
            let parent = CString::new(parent.as_os_str().as_bytes()).map_err(io::Error::other)?;
            // SAFETY: `parent` is NUL-terminated; no `O_CREAT`, so no mode
            // argument is read.
            let fd = unsafe {
                libc::open(
                    parent.as_ptr(),
                    SEARCH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was just returned by `open` and nothing else owns it.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        /// The entry `name` in `dir`, a symbolic link as the link unless
        /// `follow`; `None` when there is no such entry. Only metadata is
        /// read, as upstream zstd reads it (util.c `UTIL_statInDir`), so the
        /// file needs no permission of its own.
        fn stat_at(dir: &OwnedFd, name: &CStr, follow: bool) -> io::Result<Option<FileId>> {
            let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
            let mut stat = MaybeUninit::<Stat>::uninit();
            // SAFETY: `dir` is an open descriptor, `name` is NUL-terminated
            // and `stat` is writable storage of the type the call fills.
            if unsafe { fstatat(dir.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), flags) } != 0 {
                let err = io::Error::last_os_error();
                return match err.kind() {
                    io::ErrorKind::NotFound => Ok(None),
                    _ => Err(err),
                };
            }
            // SAFETY: `fstatat` succeeded, so it filled `stat`.
            let stat = unsafe { stat.assume_init() };
            Ok(Some(id_of(&stat)))
        }

        /// The identity in `stat`, widened as `std`'s `MetadataExt` widens
        /// it, so it compares equal to the one [`super::of`] reads.
        #[allow(
            clippy::unnecessary_cast,
            reason = "the field types differ between targets"
        )]
        fn id_of(stat: &Stat) -> FileId {
            FileId(stat.st_dev as u64, u128::from(stat.st_ino as u64))
        }

        /// The parent directory of `path`, opened, and the entry's name in
        /// it; `None` when `path` names no entry or its directory is gone.
        fn held(path: &Path) -> io::Result<Option<(OwnedFd, CString)>> {
            let Some(name) = path.file_name() else {
                return Ok(None);
            };
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
            match open_dir(parent.unwrap_or(Path::new("."))) {
                Ok(dir) => Ok(Some((dir, name))),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err),
            }
        }

        /// The parent directory is opened once and the checks and the unlink
        /// all go through it, so renaming any ancestor afterwards changes
        /// nothing; unlinking a name never reaches a file through another
        /// directory.
        ///
        /// For a source named through a link, the entry is read as itself and
        /// then followed, two reads rather than one. What can change between
        /// them while the entry stays that link is only where the link's path
        /// leads, and the entry unlinked is still the link the source was
        /// named through, as removing a path removes the name. Putting
        /// another link there, even one that takes the old link's inode
        /// number, needs a writer of this directory.
        pub fn remove_if(
            path: &Path,
            expected: FileId,
            link: Option<FileId>,
            opened: impl FnOnce(),
            between: impl FnOnce(),
        ) -> io::Result<Removal> {
            let Some((dir, name)) = held(path)? else {
                return Ok(Removal::Changed);
            };
            opened();
            let entry = stat_at(&dir, &name, false)?;
            let processed = match link {
                Some(link) => entry == Some(link) && stat_at(&dir, &name, true)? == Some(expected),
                None => entry == Some(expected),
            };
            if !processed {
                return Ok(Removal::Changed);
            }
            between();
            // SAFETY: `dir` is an open descriptor and `name` is NUL-terminated.
            if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Removal::Removed)
        }
    }

    pub fn of_path(path: &Path) -> Option<FileId> {
        fs::metadata(path).ok().as_ref().map(of)
    }

    pub fn of_named(_: &Path, metadata: &fs::Metadata) -> Option<FileId> {
        Some(of(metadata))
    }

    pub fn of_open(_: &File, metadata: &fs::Metadata) -> Option<FileId> {
        Some(of(metadata))
    }

    pub fn of_entry(path: &Path) -> Option<FileId> {
        fs::symlink_metadata(path).ok().as_ref().map(of)
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

    #[cfg(test)]
    pub const BINDS_LINK_REMOVAL: bool = false;

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

    pub fn of_named(path: &Path, _: &fs::Metadata) -> Option<FileId> {
        of_path(path)
    }

    pub fn of_open(file: &File, _: &fs::Metadata) -> Option<FileId> {
        of_handle(file.as_raw_handle())
    }

    pub fn of_entry(path: &Path) -> Option<FileId> {
        let file = OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        of_handle(file.as_raw_handle())
    }

    /// The entry is opened itself, a link as the link
    /// (`FILE_FLAG_OPEN_REPARSE_POINT`), its identity is read from that
    /// handle, and the same handle is marked for deletion.
    ///
    /// A source named through a link is kept: whether the link still leads
    /// to the file processed can be read only by resolving its path again,
    /// and that resolution can pass through another parent directory than
    /// the one the handle was opened in.
    pub fn remove_if(
        path: &Path,
        expected: FileId,
        link: Option<FileId>,
        opened: impl FnOnce(),
        between: impl FnOnce(),
    ) -> io::Result<Removal> {
        if link.is_some() {
            return Ok(Removal::Unconfirmed);
        }
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
        opened();
        match of_handle(file.as_raw_handle()) {
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

    #[cfg(test)]
    pub const BINDS_LINK_REMOVAL: bool = false;

    pub fn of_path(_: &Path) -> Option<FileId> {
        None
    }

    pub fn of_named(_: &Path, _: &fs::Metadata) -> Option<FileId> {
        None
    }

    pub fn of_open(_: &File, _: &fs::Metadata) -> Option<FileId> {
        None
    }

    pub fn of_entry(_: &Path) -> Option<FileId> {
        None
    }

    pub fn remove_if(
        _: &Path,
        _: FileId,
        _: Option<FileId>,
        _: impl FnOnce(),
        _: impl FnOnce(),
    ) -> io::Result<Removal> {
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
