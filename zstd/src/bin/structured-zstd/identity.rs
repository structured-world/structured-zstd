//! Which file a path or standard output is, as the filesystem identifies it,
//! so two names for one file (another spelling, a symlink, a hard link, a
//! redirected stdout) compare equal.

use std::path::Path;

/// A file as the filesystem identifies it: device and inode on Unix, volume
/// serial number and file index on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId(u64, u64);

/// The file `path` names, following symlinks; `None` when there is none, or
/// the platform reads no identity.
pub fn of_path(path: &Path) -> Option<FileId> {
    sys::of_path(path)
}

/// The file standard output writes to. A pipe or a console is one too, so it
/// compares unequal to every path that is not it.
pub fn of_stdout() -> Option<FileId> {
    sys::of_stdout()
}

#[cfg(unix)]
mod sys {
    use super::FileId;
    use std::fs::{self, File};
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    fn of(metadata: &fs::Metadata) -> FileId {
        FileId(metadata.dev(), metadata.ino())
    }

    pub fn of_path(path: &Path) -> Option<FileId> {
        fs::metadata(path).ok().as_ref().map(of)
    }

    pub fn of_stdout() -> Option<FileId> {
        let fd = std::io::stdout().as_fd().try_clone_to_owned().ok()?;
        File::from(fd).metadata().ok().as_ref().map(of)
    }
}

#[cfg(windows)]
mod sys {
    use super::FileId;
    use core::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::path::Path;

    /// `FILE_FLAG_BACKUP_SEMANTICS`: lets a directory be opened too, as
    /// `fs::metadata` answers for any path on Unix.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    /// `BY_HANDLE_FILE_INFORMATION`; each `FILETIME` is two `DWORD`s.
    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        attributes: u32,
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
        fn GetFileInformationByHandle(
            file: *mut c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    fn of_handle(handle: RawHandle) -> Option<FileId> {
        let mut information = ByHandleFileInformation::default();
        // SAFETY: `handle` is open for the duration of the call (borrowed from
        // a live `File` or the process's stdout), and `information` is a
        // writable `BY_HANDLE_FILE_INFORMATION`; zero is the failure.
        let ok = unsafe { GetFileInformationByHandle(handle.cast(), &mut information) };
        (ok != 0).then(|| {
            FileId(
                u64::from(information.volume_serial_number),
                (u64::from(information.file_index_high) << 32)
                    | u64::from(information.file_index_low),
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

    pub fn of_stdout() -> Option<FileId> {
        of_handle(std::io::stdout().as_raw_handle())
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::FileId;
    use std::path::Path;

    pub fn of_path(_: &Path) -> Option<FileId> {
        None
    }

    pub fn of_stdout() -> Option<FileId> {
        None
    }
}
