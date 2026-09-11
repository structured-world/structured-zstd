//! Which files a run works on, and where their outputs land.
//!
//! The command line names inputs; `--filelist` adds more from a file; `-r`
//! turns the directories among them into the files underneath. The output of
//! each is then placed next to it, or under `--output-dir-flat` /
//! `--output-dir-mirror`. The reference command does this in `zstdcli.c` and
//! `util.c`, and the order of the steps is kept: symbolic links are dropped
//! from the NAMED inputs before the file lists are merged, and directories are
//! expanded after. One departure: whether anything is left to do is judged
//! once the file lists are in. The reference command judges the named inputs
//! alone, which fails a run whose list still names usable inputs.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use super::{Result, WrapErr};

/// Largest `--filelist` the reference command reads (50 MiB).
pub const FILELIST_MAX_BYTES: u64 = 50 << 20;

/// The inputs a run ended up with.
#[derive(Debug)]
pub struct Selection {
    /// Every file to process, in command-line order, directories expanded.
    pub files: Vec<PathBuf>,
    /// How many inputs were named before directories were expanded. A run
    /// that named some and ended up with none was pointed at empty
    /// directories, which is not a request to read stdin.
    pub named: usize,
}

/// Resolve the command line's inputs to the files a run processes.
///
/// `follow_links` is `-f`: without it a symbolic link is skipped with a
/// warning, both on the command line and inside a directory walked by `-r`.
/// A FIFO reached through a link is kept, since the link is how a named pipe
/// is usually handed over.
pub fn select_inputs(
    named: Vec<PathBuf>,
    filelists: &[PathBuf],
    recursive: bool,
    follow_links: bool,
    verbosity: i32,
) -> Result<Selection> {
    let mut files = Vec::with_capacity(named.len());
    let named_count = named.len();
    for input in named {
        if !follow_links && input != Path::new("-") && is_symlink(&input) && !is_fifo(&input) {
            display!(
                verbosity,
                2,
                "Warning : {} is a symbolic link, ignoring",
                input.display()
            );
            continue;
        }
        files.push(input);
    }
    for list in filelists {
        files.extend(read_filelist(list)?);
    }
    if files.is_empty() && named_count > 0 {
        bail!("every named input is a symbolic link; pass -f to follow them");
    }
    let named = files.len();
    if recursive {
        let mut expanded = Vec::with_capacity(files.len());
        for input in files {
            match fs::metadata(&input) {
                Ok(metadata) if metadata.is_dir() => {
                    let mut ancestors = Vec::new();
                    descend(
                        &input,
                        &metadata,
                        follow_links,
                        verbosity,
                        &mut expanded,
                        &mut ancestors,
                    );
                }
                _ => expanded.push(input),
            }
        }
        files = expanded;
    }
    Ok(Selection { files, named })
}

/// What identifies a directory whatever name reaches it, so a walk notices
/// when a link has led it back to a directory it is already inside.
#[cfg(unix)]
type DirId = (u64, u64);
#[cfg(not(unix))]
type DirId = PathBuf;

/// The identity of the directory at `path`, whose `metadata` (links followed)
/// is already in hand; the device and inode where the file system has them,
/// the canonical path elsewhere.
#[cfg(unix)]
fn dir_id(path: &Path, metadata: &fs::Metadata) -> Option<DirId> {
    use std::os::unix::fs::MetadataExt;
    let _ = path;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn dir_id(path: &Path, metadata: &fs::Metadata) -> Option<DirId> {
    let _ = metadata;
    fs::canonicalize(path).ok()
}

/// Walk `dir` unless the walk is already inside it. A link followed under
/// `-f`, or a bind mount, can lead back to an ancestor; entering it again
/// would list the tree once more per nesting level until the path ran out of
/// room, so the loop is reported and not descended. `ancestors` holds the
/// directories on the way down to `dir`.
fn descend(
    dir: &Path,
    metadata: &fs::Metadata,
    follow_links: bool,
    verbosity: i32,
    out: &mut Vec<PathBuf>,
    ancestors: &mut Vec<DirId>,
) {
    let Some(id) = dir_id(dir, metadata) else {
        walk_directory(dir, follow_links, verbosity, out, ancestors);
        return;
    };
    if ancestors.contains(&id) {
        display!(
            verbosity,
            2,
            "Warning : {} leads back into a directory being walked, ignoring",
            dir.display()
        );
        return;
    }
    ancestors.push(id);
    walk_directory(dir, follow_links, verbosity, out, ancestors);
    ancestors.pop();
}

/// Whether `path` itself is a symbolic link, whatever it points at.
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether `path`, links followed, is a named pipe.
fn is_fifo(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Read a `--filelist`: one file name per line, blank lines skipped.
///
/// The list has to be a regular file of bounded size, as the reference command
/// requires; a name that is not there or a directory is an error rather than
/// an empty list, since a script that mistyped the list name would otherwise
/// silently process nothing.
fn read_filelist(list: &Path) -> Result<Vec<PathBuf>> {
    let metadata =
        fs::metadata(list).wrap_err_with(|| format!("error reading {}", list.display()))?;
    if !metadata.is_file() {
        bail!("error reading {}: not a regular file", list.display());
    }
    if metadata.len() > FILELIST_MAX_BYTES {
        bail!(
            "error reading {}: file list is larger than {} bytes",
            list.display(),
            FILELIST_MAX_BYTES
        );
    }
    let file =
        fs::File::open(list).wrap_err_with(|| format!("error reading {}", list.display()))?;
    let mut names = Vec::new();
    // Lines are bytes, like the names in them: a filename need not be UTF-8,
    // and reading the list as text would reject or rename such an entry.
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .wrap_err_with(|| format!("error reading {}", list.display()))?;
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        names.push(bytes_to_path(&line));
    }
    Ok(names)
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Append every file under `dir` to `out`, depth first.
///
/// Entries are taken in name order so two runs over one tree process it the
/// same way; the reference command takes them in directory order, which the
/// filesystem does not promise to keep. A directory that cannot be read is
/// reported and contributes nothing, as there too. Subdirectories go through
/// [`descend`], which keeps a link from leading the walk round in a circle.
fn walk_directory(
    dir: &Path,
    follow_links: bool,
    verbosity: i32,
    out: &mut Vec<PathBuf>,
    ancestors: &mut Vec<DirId>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            display!(
                verbosity,
                1,
                "Cannot open directory '{}': {err}",
                dir.display()
            );
            return;
        }
    };
    let mut names: Vec<OsString> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(err) => {
                display!(verbosity, 1, "readdir({}) error: {err}", dir.display());
                return;
            }
        }
    }
    names.sort();
    for name in names {
        let path = dir.join(name);
        if !follow_links && is_symlink(&path) {
            display!(
                verbosity,
                2,
                "Warning : {} is a symbolic link, ignoring",
                path.display()
            );
            continue;
        }
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                descend(&path, &metadata, follow_links, verbosity, out, ancestors);
            }
            _ => out.push(path),
        }
    }
}

/// Where `--output-dir-flat DIR` puts the output of `src`: under `DIR`, by
/// the source's own file name.
pub fn flat_output_path(src: &Path, dir: &Path) -> PathBuf {
    dir.join(src.file_name().unwrap_or(src.as_os_str()))
}

/// The source path with the parts `--output-dir-mirror` drops: a leading
/// `./`, the root, and a drive prefix. `None` when a `..` component is in it,
/// since mirroring that would climb out of the output tree.
fn mirrored_relative(src: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in src.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(part) => relative.push(part),
        }
    }
    Some(relative)
}

/// The directory `--output-dir-mirror ROOT` puts the output of `src` in: the
/// source's own directory, replayed under `ROOT`. `None` for a source the
/// reference command refuses to mirror (one with `..` in its path).
pub fn mirrored_output_dir(src: &Path, root: &Path) -> Option<PathBuf> {
    let relative = mirrored_relative(src)?;
    Some(match relative.parent() {
        Some(parent) => root.join(parent),
        None => root.to_path_buf(),
    })
}

/// Create the directory chain under `root` that mirrors the directory of
/// `src`, each level with the permissions of the source directory it mirrors.
///
/// `root` itself is created with the default permissions when it is missing.
/// A source whose path cannot be mirrored (see [`mirrored_output_dir`]) is
/// left for the caller to report.
pub fn create_mirrored_dirs(src: &Path, root: &Path) -> Result<()> {
    let Some(relative) = mirrored_relative(src) else {
        return Ok(());
    };
    create_dir_if_missing(root, None)
        .wrap_err_with(|| format!("failed to create DIR {}", root.display()))?;
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    // The source directory that each mirrored level stands for, rebuilt from
    // the source path as given so its permissions can be read: for
    // `/var/tmp/abc` the levels are `/var` and `/var/tmp`.
    let stripped: usize = src
        .components()
        .count()
        .saturating_sub(relative.components().count());
    let source_root: PathBuf = src.components().take(stripped).collect();
    let mut source_level = source_root;
    let mut destination = root.to_path_buf();
    for level in parent.components() {
        source_level.push(level);
        destination.push(level);
        let mode = fs::metadata(&source_level)
            .ok()
            .map(|metadata| metadata.permissions());
        create_dir_if_missing(&destination, mode)
            .wrap_err_with(|| format!("failed to create DIR {}", destination.display()))?;
    }
    Ok(())
}

/// Create `dir` unless it is already there, with `permissions` when given.
fn create_dir_if_missing(dir: &Path, permissions: Option<fs::Permissions>) -> std::io::Result<()> {
    match fs::metadata(dir) {
        Ok(existing) if existing.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "exists and is not a directory",
            ));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        if let Some(permissions) = &permissions {
            builder.mode(permissions.mode() & 0o7777);
        }
        builder
    };
    #[cfg(not(unix))]
    let builder = {
        let _ = permissions;
        fs::DirBuilder::new()
    };
    builder.create(dir)
}

/// The file names that more than one of `files` share.
///
/// Under `--output-dir-flat` two such inputs land on one output, the second
/// replacing the first; the reference command warns about it after the run.
pub fn shared_file_names(files: &[PathBuf]) -> Vec<OsString> {
    let mut names: Vec<&std::ffi::OsStr> =
        files.iter().filter_map(|file| file.file_name()).collect();
    names.sort_unstable();
    let mut shared = Vec::new();
    for pair in names.windows(2) {
        if pair[0] == pair[1] && shared.last() != Some(&pair[0].to_os_string()) {
            shared.push(pair[0].to_os_string());
        }
    }
    shared
}

/// Extensions the reference command treats as already compressed under
/// `--exclude-compressed` (`fileio.c`, `compressedFileExtensions`).
const COMPRESSED_EXTENSIONS: &[&str] = &[
    ".zst", ".tzst", ".gz", ".tgz", ".lzma", ".xz", ".txz", ".lz4", ".tlz4", ".7z", ".aa3", ".aac",
    ".aar", ".ace", ".alac", ".ape", ".apk", ".apng", ".arc", ".archive", ".arj", ".ark", ".asf",
    ".avi", ".avif", ".ba", ".br", ".bz2", ".cab", ".cdx", ".chm", ".cr2", ".divx", ".dmg", ".dng",
    ".docm", ".docx", ".dotm", ".dotx", ".dsft", ".ear", ".eftx", ".emz", ".eot", ".epub", ".f4v",
    ".flac", ".flv", ".gho", ".gif", ".gifv", ".gnp", ".iso", ".jar", ".jpeg", ".jpg", ".jxl",
    ".lz", ".lzh", ".m4a", ".m4v", ".mkv", ".mov", ".mp2", ".mp3", ".mp4", ".mpa", ".mpc", ".mpe",
    ".mpeg", ".mpg", ".mpl", ".mpv", ".msi", ".odp", ".ods", ".odt", ".ogg", ".ogv", ".otp",
    ".ots", ".ott", ".pea", ".png", ".pptx", ".qt", ".rar", ".s7z", ".sfx", ".sit", ".sitx",
    ".sqx", ".svgz", ".swf", ".tbz2", ".tib", ".tlz", ".vob", ".war", ".webm", ".webp", ".wma",
    ".wmv", ".woff", ".woff2", ".wvl", ".xlsx", ".xpi", ".xps", ".zip", ".zipx", ".zoo", ".zpaq",
];

/// Whether `path` carries an extension of an already-compressed format.
///
/// The extension is the file name's last dot onward, compared exactly: a
/// dotfile has none, and `.GZ` is not `.gz`, as in the reference command.
pub fn has_compressed_extension(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    match name.rfind('.') {
        Some(0) | None => false,
        Some(at) => COMPRESSED_EXTENSIONS.contains(&&name[at..]),
    }
}

#[cfg(test)]
mod tests;
