//! Which file a path or standard output is, as the filesystem identifies it,
//! so two names for one file (another spelling, a symlink, a hard link, a
//! redirected stdout) compare equal.

use std::path::Path;

/// A file as the filesystem identifies it: device and inode on Unix, volume
/// serial number and the 128-bit file ID on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u64, u128);

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

    fn of(metadata: &fs::Metadata) -> FileId {
        FileId(metadata.dev(), u128::from(metadata.ino()))
    }

    pub fn of_path(path: &Path) -> Option<FileId> {
        fs::metadata(path).ok().as_ref().map(of)
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
    use super::FileId;
    use core::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::path::Path;

    /// `FILE_FLAG_BACKUP_SEMANTICS`: lets a directory be opened too, as
    /// `fs::metadata` answers for any path on Unix.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    /// `FileIdInfo` of `FILE_INFO_BY_HANDLE_CLASS`.
    const FILE_ID_INFO_CLASS: i32 = 18;

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
    use super::FileId;
    use std::path::Path;

    pub fn of_path(_: &Path) -> Option<FileId> {
        None
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
