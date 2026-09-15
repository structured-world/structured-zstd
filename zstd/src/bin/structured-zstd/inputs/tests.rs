use std::fs;
use std::path::{Path, PathBuf};

use super::{
    create_mirrored_dirs, flat_output_path, has_compressed_extension, mirrored_output_dir,
    parse_filelist, select_inputs, shared_file_names,
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
    assert!(selection.explicit, "the directory was named");
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

    // At the verbosity that reports the skipped link.
    let kept = select_inputs(vec![target.clone(), link.clone()], &[], false, false, 2).unwrap();
    assert_eq!(kept.files, vec![target.clone()], "the link is dropped");

    let followed = select_inputs(vec![target.clone(), link.clone()], &[], false, true, 0).unwrap();
    assert_eq!(followed.files, vec![target, link.clone()], "-f keeps it");

    let err = select_inputs(vec![link], &[], false, false, 0)
        .expect_err("a run whose every input was a link has nothing to do")
        .to_string();
    assert!(err.contains("symbolic link"), "the refusal says why: {err}");
}

/// A run whose named inputs were all links still has the inputs its
/// `--filelist` names: the "nothing left" refusal is judged on the merged set,
/// not on the command line alone.
#[cfg(unix)]
#[test]
fn a_filelist_keeps_the_run_alive_when_every_named_input_is_a_link() {
    let scratch = Scratch::new("linklist");
    let target = scratch.file("target.txt");
    let link = scratch.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let list = scratch.path().join("list.txt");
    fs::write(&list, format!("{}\n", target.display())).unwrap();

    let selection = select_inputs(vec![link], &[list], false, false, 0)
        .expect("the list still supplies an input");
    assert_eq!(selection.files, vec![target]);
}

/// Under `-f` a link back to an ancestor directory is entered once and then
/// recognised: the walk reports the loop and does not descend again, so each
/// file is listed once instead of once per nesting level until the path runs
/// out of room.
#[cfg(unix)]
#[test]
fn a_link_back_into_the_tree_is_not_walked_twice_under_f() {
    let scratch = Scratch::new("loop");
    let leaf = scratch.file("tree/inner/leaf.txt");
    std::os::unix::fs::symlink(
        scratch.path().join("tree"),
        scratch.path().join("tree/inner/up"),
    )
    .unwrap();

    // At the verbosity that reports the loop.
    let selection = select_inputs(vec![scratch.path().join("tree")], &[], true, true, 2).unwrap();
    assert_eq!(selection.files, vec![leaf]);
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
    // At the verbosity that reports the skipped link.
    let skipped = select_inputs(vec![dir.clone()], &[], true, false, 2).unwrap();
    assert_eq!(skipped.files, vec![real.clone()]);

    let followed = select_inputs(vec![dir.clone()], &[], true, true, 0).unwrap();
    assert_eq!(followed.files, vec![dir.join("link.txt"), real]);
}

/// A directory `-r` cannot open is reported and skipped, and the rest of the
/// tree is still selected: the reference command does the same and exits 0,
/// so a run over a tree with one private directory keeps working the way a
/// script written against it expects.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_skipped_and_the_walk_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("locked");
    let readable = scratch.file("tree/ok/a.txt");
    scratch.file("tree/locked/b.txt");
    let locked = scratch.path().join("tree/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // A process that permissions do not bind (root) opens it anyway, and then
    // there is no unreadable directory to test.
    let binding = fs::read_dir(&locked).is_err();

    // At the verbosity that reports the directory.
    let selection = select_inputs(vec![scratch.path().join("tree")], &[], true, false, 1);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if binding {
        assert_eq!(selection.expect("the walk goes on").files, vec![readable]);
    }
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
    assert!(selection.explicit, "a name and a list were both given");
}

/// A `--filelist` that lists nothing is still a source the command line gave:
/// the run has no files, and must not fall back to reading stdin, which with
/// a redirected stdin would compress data the caller never pointed it at.
#[test]
fn an_empty_filelist_is_an_explicit_empty_selection() {
    let scratch = Scratch::new("emptylist");
    let list = scratch.path().join("list.txt");
    fs::write(&list, "\n\n").unwrap();
    let selection = select_inputs(Vec::new(), &[list], false, false, 0).unwrap();
    assert!(selection.files.is_empty());
    assert!(
        selection.explicit,
        "a list was given, so stdin is not the input"
    );

    let nothing = select_inputs(Vec::new(), &[], false, false, 0).unwrap();
    assert!(!nothing.explicit, "no name and no list: stdin is the input");
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

/// The size limit holds for what is read, not only for what `stat` reported:
/// a list that grew after the check, or a pseudo-file whose reported length
/// understates its contents, is refused once it passes the limit instead of
/// being read without end. A list exactly at the limit is fine.
#[test]
fn a_filelist_past_the_limit_is_refused_while_reading() {
    let list = Path::new("list.txt");
    let at_limit = b"first\nsecond\n";
    let names = parse_filelist(&at_limit[..], at_limit.len() as u64, list)
        .expect("a list at the limit is read whole");
    assert_eq!(names, vec![PathBuf::from("first"), PathBuf::from("second")]);

    let err = parse_filelist(&at_limit[..], at_limit.len() as u64 - 1, list)
        .expect_err("one byte past the limit is refused")
        .to_string();
    assert!(
        err.contains("larger than"),
        "the refusal names the limit: {err}"
    );
}

/// A list whose size is already past the limit is refused before it is
/// opened, and says so. Sparse, so the test writes nothing of it.
#[test]
fn a_filelist_reported_past_the_limit_is_refused_up_front() {
    let scratch = Scratch::new("hugelist");
    let list = scratch.path().join("list.txt");
    fs::File::create(&list)
        .unwrap()
        .set_len(super::FILELIST_MAX_BYTES + 1)
        .unwrap();
    let err = select_inputs(Vec::new(), &[list], false, false, 0)
        .expect_err("a list past the limit is refused")
        .to_string();
    assert!(err.contains("larger than"), "{err}");
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
    assert!(selection.explicit, "the list is the source");
}

/// Links named by a `--filelist` are taken as given, without `-f`: a link to a
/// file stays an input and a link to a directory is walked by `-r`. Only the
/// command-line names are filtered, before the lists are merged, which is the
/// order the reference command applies (`zstdcli.c`), and a script that hands
/// links over through a list expects them processed. Links found INSIDE the
/// walked directory still follow the walk's own rule and are skipped.
#[cfg(unix)]
#[test]
fn filelist_links_are_kept_and_walked_without_f() {
    let scratch = Scratch::new("listlinks");
    let target = scratch.file("target.txt");
    scratch.file("real/inner.txt");
    let elsewhere = scratch.file("elsewhere/other.txt");
    std::os::unix::fs::symlink(&elsewhere, scratch.path().join("real/nested-link.txt")).unwrap();
    let file_link = scratch.path().join("linkfile");
    let dir_link = scratch.path().join("linkdir");
    std::os::unix::fs::symlink(&target, &file_link).unwrap();
    std::os::unix::fs::symlink(scratch.path().join("real"), &dir_link).unwrap();
    let list = scratch.path().join("list.txt");
    fs::write(
        &list,
        format!("{}\n{}\n", dir_link.display(), file_link.display()),
    )
    .unwrap();

    let selection = select_inputs(Vec::new(), &[list], true, false, 0).unwrap();
    assert_eq!(
        selection.files,
        vec![dir_link.join("inner.txt"), file_link],
        "the listed links are processed; the link inside the walked tree is not"
    );
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
    // A path with nothing left once the root is dropped lands in the root.
    assert_eq!(
        mirrored_output_dir(Path::new("/"), root),
        Some(PathBuf::from("out"))
    );
}

/// A source with no directory to mirror needs only the root, and a root that
/// cannot be a directory (a file is there, or under it) is an error rather
/// than a mirror written somewhere else.
#[test]
fn a_mirror_root_is_created_alone_or_refused() {
    let scratch = Scratch::new("mirrorroot");
    let root = scratch.path().join("root");
    create_mirrored_dirs(Path::new("/"), &root).expect("the root alone is created");
    assert!(root.is_dir());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        0,
        "and nothing under it"
    );

    let file = scratch.file("occupied");
    let src = Path::new("dir/leaf.txt");
    assert!(
        create_mirrored_dirs(src, &file).is_err(),
        "a file where the root goes"
    );
    assert!(
        create_mirrored_dirs(src, &file.join("below")).is_err(),
        "a root under a file"
    );
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

/// Each file name that several paths share is reported once, whatever
/// directories the paths are in.
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
    assert!(
        !has_compressed_extension(Path::new("/")),
        "no file name, no extension"
    );
    assert!(!has_compressed_extension(Path::new("a.gz/..")));
}

/// A Unix file name is bytes, and the part before the extension need not be
/// UTF-8: `\xff.zst` still ends in `.zst` and is still compressed.
#[cfg(unix)]
#[test]
fn a_compressed_extension_counts_after_a_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::OsStr::from_bytes(b"\xff.zst");
    assert!(has_compressed_extension(Path::new(name)));
    let plain = std::ffi::OsStr::from_bytes(b"\xff.txt");
    assert!(!has_compressed_extension(Path::new(plain)));
}
