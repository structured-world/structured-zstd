use std::fs;
use std::path::{Path, PathBuf};

use super::{
    create_mirrored_dirs, flat_output_path, has_compressed_extension, mirrored_output_dir,
    select_inputs, shared_file_names,
};

/// A scratch directory unique to the test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("szstd-inputs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, relative.as_bytes()).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `-r` replaces a directory by the files beneath it, all the way down, in a
/// fixed order: a script that compresses a tree gets the same sequence of
/// outputs and messages every time it runs.
#[test]
fn recursion_walks_directories_depth_first_in_name_order() {
    let scratch = Scratch::new("walk");
    let b = scratch.file("b.txt");
    let a = scratch.file("a.txt");
    let nested = scratch.file("sub/deeper/c.txt");
    let sibling = scratch.file("sub/d.txt");

    let selection = select_inputs(vec![scratch.path().to_path_buf()], &[], true, false, 0)
        .expect("a readable tree expands");
    // Name order at each level: `d.txt` sorts before the `deeper` directory
    // (`.` before `e`), so the sibling file comes before the nested one.
    assert_eq!(selection.files, vec![a, b, sibling, nested]);
    assert_eq!(selection.named, 1, "one input was named before expansion");
}

/// Without `-r` a directory stays a directory: the caller reports it as one
/// rather than silently walking into it, which is what the reference command
/// does ("is a directory -- ignored").
#[test]
fn without_recursion_a_directory_is_kept_for_the_caller_to_refuse() {
    let scratch = Scratch::new("nowalk");
    scratch.file("inside.txt");
    let selection = select_inputs(vec![scratch.path().to_path_buf()], &[], false, false, 0)
        .expect("selection itself does not fail");
    assert_eq!(selection.files, vec![scratch.path().to_path_buf()]);
}

/// A symbolic link on the command line is skipped unless `-f` follows links:
/// compressing through a link writes the archive beside the link and, with
/// `--rm`, deletes the link rather than the file. When every input was a link
/// the run has nothing left and says so instead of falling back to stdin.
#[cfg(unix)]
#[test]
fn named_symlinks_are_skipped_unless_links_are_followed() {
    let scratch = Scratch::new("links");
    let target = scratch.file("target.txt");
    let link = scratch.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let kept = select_inputs(vec![target.clone(), link.clone()], &[], false, false, 0).unwrap();
    assert_eq!(kept.files, vec![target.clone()], "the link is dropped");

    let followed = select_inputs(vec![target.clone(), link.clone()], &[], false, true, 0).unwrap();
    assert_eq!(followed.files, vec![target, link.clone()], "-f keeps it");

    let err = select_inputs(vec![link], &[], false, false, 0)
        .expect_err("a run whose every input was a link has nothing to do")
        .to_string();
    assert!(err.contains("symbolic link"), "the refusal says why: {err}");
}

/// The same rule inside a walked tree: a link found by `-r` is skipped without
/// `-f`, so a tree with a link back into itself does not loop, and a link to a
/// directory is not descended into.
#[cfg(unix)]
#[test]
fn symlinks_inside_a_walked_tree_are_skipped_unless_followed() {
    let scratch = Scratch::new("treelinks");
    let real = scratch.file("dir/real.txt");
    let other = scratch.file("elsewhere/other.txt");
    std::os::unix::fs::symlink(&other, scratch.path().join("dir/link.txt")).unwrap();

    let dir = scratch.path().join("dir");
    let skipped = select_inputs(vec![dir.clone()], &[], true, false, 0).unwrap();
    assert_eq!(skipped.files, vec![real.clone()]);

    let followed = select_inputs(vec![dir.clone()], &[], true, true, 0).unwrap();
    assert_eq!(followed.files, vec![dir.join("link.txt"), real]);
}

/// `--filelist` names inputs one per line, in the shape `ls` prints them.
/// Blank lines carry no name and are skipped; a Windows line ending is
/// stripped like a Unix one, since the list may have been written elsewhere.
#[test]
fn a_filelist_adds_one_input_per_line() {
    let scratch = Scratch::new("filelist");
    let list = scratch.path().join("list.txt");
    fs::write(&list, "first.bin\n\nsecond.bin\r\nthird.bin").unwrap();

    let selection =
        select_inputs(vec![PathBuf::from("argv.bin")], &[list], false, false, 0).unwrap();
    assert_eq!(
        selection.files,
        vec![
            PathBuf::from("argv.bin"),
            PathBuf::from("first.bin"),
            PathBuf::from("second.bin"),
            PathBuf::from("third.bin"),
        ],
        "command-line inputs come first, then the list, blank lines dropped"
    );
    assert_eq!(selection.named, 4, "list entries count as named inputs");
}

/// A list that is not there is an error, not an empty list: a mistyped
/// `--filelist` would otherwise process nothing and report success.
#[test]
fn a_missing_or_irregular_filelist_is_an_error() {
    let scratch = Scratch::new("badlist");
    let missing = scratch.path().join("nope.txt");
    let err = select_inputs(Vec::new(), &[missing], false, false, 0)
        .expect_err("a missing list cannot be read")
        .to_string();
    assert!(err.contains("error reading"), "{err}");

    let err = select_inputs(
        Vec::new(),
        std::slice::from_ref(&scratch.path().to_path_buf()),
        false,
        false,
        0,
    )
    .expect_err("a directory is not a list")
    .to_string();
    assert!(err.contains("not a regular file"), "{err}");
}

/// Names from a list are expanded by `-r` like names from the command line:
/// the list is just another way of typing them.
#[test]
fn filelist_entries_are_expanded_recursively_too() {
    let scratch = Scratch::new("listwalk");
    let inside = scratch.file("tree/leaf.txt");
    let list = scratch.path().join("list.txt");
    fs::write(
        &list,
        format!("{}\n", scratch.path().join("tree").display()),
    )
    .unwrap();

    let selection = select_inputs(Vec::new(), &[list], true, false, 0).unwrap();
    assert_eq!(selection.files, vec![inside]);
    assert_eq!(selection.named, 1);
}

/// `--output-dir-flat` drops the source's directory and keeps its name.
#[test]
fn flat_output_keeps_only_the_file_name() {
    assert_eq!(
        flat_output_path(Path::new("a/b/c.txt"), Path::new("out")),
        PathBuf::from("out/c.txt")
    );
    assert_eq!(
        flat_output_path(Path::new("c.txt"), Path::new("out/")),
        PathBuf::from("out/c.txt")
    );
}

/// `--output-dir-mirror` replays the source's directory under the root, with
/// a leading `./` or `/` removed so an absolute input lands inside the root
/// rather than replacing it. A path that climbs with `..` is refused, since
/// mirroring it could write outside the root.
#[test]
fn mirrored_output_replays_the_source_directory_under_the_root() {
    let root = Path::new("out");
    assert_eq!(
        mirrored_output_dir(Path::new("a/b/c.txt"), root),
        Some(PathBuf::from("out/a/b"))
    );
    assert_eq!(
        mirrored_output_dir(Path::new("./x.txt"), root),
        Some(PathBuf::from("out"))
    );
    assert_eq!(
        mirrored_output_dir(Path::new("x.txt"), root),
        Some(PathBuf::from("out"))
    );
    assert_eq!(
        mirrored_output_dir(Path::new("/var/tmp/abc"), root),
        Some(PathBuf::from("out/var/tmp"))
    );
    assert_eq!(mirrored_output_dir(Path::new("../x.txt"), root), None);
    assert_eq!(mirrored_output_dir(Path::new("a/../b/x.txt"), root), None);
}

/// The mirrored directories are created before the output is written, each
/// with the permissions of the source directory it stands for, so a private
/// source tree does not become a world-readable mirror.
#[test]
fn mirrored_directories_are_created_with_the_source_permissions() {
    let scratch = Scratch::new("mirror");
    let src = scratch.file("tree/inner/leaf.txt");
    let root = scratch.path().join("mirror-root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            scratch.path().join("tree/inner"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    create_mirrored_dirs(&src, &root).expect("the chain is created");
    let expected = mirrored_output_dir(&src, &root).unwrap();
    assert!(expected.is_dir(), "{} must exist", expected.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&expected).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "the mirrored directory takes the source's mode"
        );
    }
    // Doing it again is harmless: the directories are already there.
    create_mirrored_dirs(&src, &root).expect("an existing chain is fine");
}

/// A source that cannot be mirrored creates nothing: the caller reports it.
#[test]
fn an_unmirrorable_source_creates_no_directories() {
    let scratch = Scratch::new("nomirror");
    let root = scratch.path().join("root");
    create_mirrored_dirs(Path::new("../escape.txt"), &root).expect("nothing to do is not an error");
    assert!(!root.exists(), "no root is created for a refused source");
}

/// Two inputs with one file name land on one output under
/// `--output-dir-flat`; the run warns about each such name once.
#[test]
fn shared_names_are_reported_once_each() {
    let files = vec![
        PathBuf::from("a/data.txt"),
        PathBuf::from("b/data.txt"),
        PathBuf::from("c/data.txt"),
        PathBuf::from("a/other.txt"),
        PathBuf::from("b/other.txt"),
        PathBuf::from("unique.txt"),
    ];
    assert_eq!(
        shared_file_names(&files),
        vec![
            std::ffi::OsString::from("data.txt"),
            std::ffi::OsString::from("other.txt")
        ]
    );
    assert!(shared_file_names(&[PathBuf::from("one")]).is_empty());
}

/// `--exclude-compressed` judges by the last extension, exactly as spelled: a
/// `.tar.zst` is compressed, a dotfile has no extension, and case matters as
/// it does in the reference list.
#[test]
fn compressed_extensions_are_matched_on_the_last_dot() {
    assert!(has_compressed_extension(Path::new("a.gz")));
    assert!(has_compressed_extension(Path::new("dir.d/archive.tar.zst")));
    assert!(has_compressed_extension(Path::new("movie.mp4")));
    assert!(!has_compressed_extension(Path::new("a.txt")));
    assert!(!has_compressed_extension(Path::new(".gz")));
    assert!(!has_compressed_extension(Path::new("A.GZ")));
    assert!(!has_compressed_extension(Path::new("noext")));
    assert!(!has_compressed_extension(Path::new("dir.gz/plain")));
}
