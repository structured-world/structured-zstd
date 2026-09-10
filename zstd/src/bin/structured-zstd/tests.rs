use super::*;

/// What a run with no `-D` carries: the codecs take their dictionary already
/// parsed, and "no dictionary" is a prepared set holding neither side.
fn no_dict() -> Dictionaries {
    Dictionaries::default()
}

/// What a benchmark of `input_len` bytes across `levels` actually holds: the
/// input, the frame it compresses to, the decoded copy, the match finder the
/// passes build, and the fixed decoder allowance every run pays. The boundary
/// these tests probe, spelled the way the run spells it so the two cannot
/// drift apart.
fn benchmark_budget(input_len: u64, levels: std::ops::RangeInclusive<i32>) -> u64 {
    let encoder = levels
        .map(|level| {
            structured_zstd::encoding::estimated_compression_workspace_bytes_for_source(
                structured_zstd::encoding::CompressionLevel::Level(level),
                Some(input_len),
            ) as u64
        })
        .max()
        .unwrap_or(0);
    structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE
        + (1 << 20)
        + 2 * input_len
        + structured_zstd::encoding::compress_bound(input_len as usize) as u64
        + encoder
}

/// The same blob a `-D` run would hand the codecs, parsed for both directions
/// so one helper serves a compressing test and a decoding one alike.
fn prepared_dict(raw: &[u8]) -> Dictionaries {
    Dictionaries::prepare(Some(raw), true, true).expect("the fixture dictionary must parse")
}

/// What a plain `zstd` invocation presets, before any flag.
fn plain() -> ProgramPreset {
    program_preset("zstd")
}

/// Parse `args` as the plain `zstd` command with the built-in default level.
fn parse(args: &[&str]) -> Result<Options> {
    parse_as(&plain(), CompressionLevel::DEFAULT_LEVEL, args)
}

/// Parse `args` under `preset` (an `argv[0]` dispatch) with `default_level`
/// standing in for the built-in default (`ZSTD_CLEVEL`).
fn parse_as(preset: &ProgramPreset, default_level: i32, args: &[&str]) -> Result<Options> {
    let owned: Vec<OsString> = args.iter().map(OsString::from).collect();
    match parse_args(&owned, preset, default_level).map_err(|failure| failure.error)? {
        Parsed::Run(opts) => Ok(*opts),
        Parsed::Handled => bail!("parse handled (help/version) unexpectedly"),
    }
}

/// A scratch directory unique to the test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("szstd-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, relative: &str, content: &[u8]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A frame compressed the way the tool compresses by default.
fn frame_of(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    compress_stream(
        payload,
        &mut frame,
        &FrameSettings {
            level: 3,
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing the fixture must succeed");
    frame
}

/// Decode `stream` with the default decode settings.
fn decoded(stream: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decompress_stream(stream, &mut out, &no_dict(), &DecodeSettings::default())?;
    Ok(out)
}

/// A filename is bytes, and on Unix those bytes need not be UTF-8. Reading the
/// command line as text rejects such a name before any of the byte-preserving
/// path handling can run — and does it by panicking, which is not an answer.
#[cfg(unix)]
#[test]
fn a_non_utf8_argument_survives_parsing() {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;

    let name = OsStr::from_bytes(b"weird\xffname.zst");
    let args: Vec<OsString> = vec![OsString::from("-d"), name.to_os_string()];
    let opts = match parse_args(&args, &plain(), CompressionLevel::DEFAULT_LEVEL)
        .map_err(|failure| failure.error)
        .expect("parsing must not fail")
    {
        Parsed::Run(opts) => *opts,
        Parsed::Handled => panic!("unexpected help/version"),
    };
    assert_eq!(
        opts.inputs,
        vec![PathBuf::from(name)],
        "the argument's bytes must reach the input list unchanged"
    );
}

/// A path attached to its option — `-Dname`, `-oname`, `--use-dict=name` — is
/// the same bytes as one given separately, and has to survive the same way.
/// Deriving it from the lossy view of the whole argument aims the command at a
/// replacement-character filename that is not the one asked for.
#[cfg(unix)]
#[test]
fn attached_path_options_keep_their_bytes() {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;

    let name = OsStr::from_bytes(b"weird\xffname");
    let expected = PathBuf::from(name);

    let attached = |flag: &[u8]| -> Options {
        let mut arg = flag.to_vec();
        arg.extend_from_slice(name.as_bytes());
        let args = vec![OsString::from(OsStr::from_bytes(&arg)), OsString::from("f")];
        match parse_args(&args, &plain(), CompressionLevel::DEFAULT_LEVEL)
            .map_err(|failure| failure.error)
            .expect("parsing must not fail")
        {
            Parsed::Run(opts) => *opts,
            Parsed::Handled => panic!("unexpected help/version"),
        }
    };

    assert_eq!(attached(b"-D").dict.as_deref(), Some(expected.as_path()));
    assert_eq!(attached(b"-o").output.as_deref(), Some(expected.as_path()));
    assert_eq!(
        attached(b"--use-dict=").dict.as_deref(),
        Some(expected.as_path())
    );
    // The options that take a directory or a list name are paths too.
    assert_eq!(
        attached(b"--output-dir-flat=").output_dir.as_deref(),
        Some(expected.as_path())
    );
    assert_eq!(
        attached(b"--output-dir-mirror=")
            .output_dir_mirror
            .as_deref(),
        Some(expected.as_path())
    );
    assert_eq!(
        attached(b"--filelist=").filelists,
        vec![expected.clone()],
        "a list name keeps its bytes as well"
    );
}

#[test]
fn extension_added() {
    assert_eq!(
        add_extension(Path::new("README.md"), ".zst"),
        PathBuf::from("README.md.zst")
    );
}

#[test]
fn list_file_walks_multi_frame_archive_by_seeking() {
    use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};
    // Two concatenated frames: the seek-based frame walk must land exactly on
    // the second frame's start, or parsing it would fail. A wrong frame
    // length (the old fs::read path computed it differently) would surface as
    // an error here.
    let mut archive = compress_slice_to_vec(&[7u8; 4096], CompressionLevel::Default);
    archive.extend_from_slice(&compress_slice_to_vec(
        b"second frame payload, distinct content",
        CompressionLevel::Default,
    ));

    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-list-test-{}.zst", std::process::id()));
    fs::write(&path, &archive).unwrap();
    let result = list_file(&path, false, 0);
    let _ = fs::remove_file(&path);
    let summary = result.expect("list_file must walk both frames without error");
    assert_eq!(summary.frames, 2);
    assert_eq!(summary.decompressed, Some(4096 + 38));
    assert!(
        !summary.check && summary.checksum.is_none(),
        "library-default frames carry no checksum, so the archive reports none"
    );

    // The tool's own frames do, and the stored value of the last frame that
    // has one is kept for `-lv` to print.
    let mut checked = frame_of(&[7u8; 4096]);
    let trailer: [u8; 4] = checked[checked.len() - 4..].try_into().unwrap();
    checked.extend_from_slice(&compress_slice_to_vec(
        b"a second frame without a checksum",
        CompressionLevel::Default,
    ));
    fs::write(&path, &checked).unwrap();
    let result = list_file(&path, false, 0);
    let _ = fs::remove_file(&path);
    let summary = result.expect("a mixed archive lists");
    assert!(
        summary.check,
        "one checksummed frame makes the archive checked"
    );
    assert_eq!(
        summary.checksum,
        Some(trailer),
        "the checksummed frame's trailer is kept"
    );
}

/// The `DictID` column holds one id, and a concatenated archive can need more
/// than one. Printing the first frame's then sends the reader after a
/// dictionary that decodes only the start of the file — and when the first
/// frame needs none, it says no dictionary is needed at all. The reference tool
/// answers the same way: it warns and shows 0 for such a file.
#[test]
fn an_archive_built_from_two_dictionaries_names_neither() {
    use structured_zstd::encoding::{CompressionLevel, EncoderDictionary, StreamingEncoder};

    /// One frame primed with a dictionary carrying `id`.
    fn frame_with_dictionary(id: u32, payload: &[u8]) -> Vec<u8> {
        use structured_zstd::dictionary::{
            FastCoverOptions, FinalizeOptions, create_fastcover_dict_from_slice,
        };
        // Trained rather than assembled: a serialized dictionary carries
        // entropy tables, and the id the frame header records lives in its
        // header alongside them.
        let corpus: Vec<u8> = (0..40_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let mut blob = Vec::new();
        create_fastcover_dict_from_slice(
            corpus.as_slice(),
            &mut blob,
            8 * 1024,
            &FastCoverOptions::default(),
            FinalizeOptions { dict_id: Some(id) },
        )
        .expect("training the fixture dictionary must succeed");
        let mut out = Vec::new();
        let mut encoder = StreamingEncoder::new(&mut out, CompressionLevel::Default);
        encoder
            .set_encoder_dictionary(
                EncoderDictionary::from_serialized_or_raw_content(&blob)
                    .expect("the fixture dictionary must parse"),
            )
            .expect("attaching before the first write must work");
        std::io::Write::write_all(&mut encoder, payload).unwrap();
        encoder.finish().unwrap();
        out
    }

    let mut archive = frame_with_dictionary(111, b"first frame payload");
    archive.extend_from_slice(&frame_with_dictionary(222, b"second frame payload"));

    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-mixdict-{}.zst", std::process::id()));
    fs::write(&path, &archive).unwrap();
    let summary = summarize_archive(&path);
    let _ = fs::remove_file(&path);

    let summary = summary.expect("both frames must be walked");
    assert!(
        !summary.dict_ids_agree,
        "the frames name different dictionaries"
    );
    assert_eq!(
        summary.dict_id, None,
        "so no single id can be reported for the archive"
    );
}

/// `--target-compressed-block-size` is what a caller reaches for when they need
/// bounded latency or block-level processing: smaller blocks flush sooner.
/// Validating the number and then compressing with the default geometry gives
/// them the blocks they were trying to avoid.
#[test]
fn target_block_size_reaches_the_encoder() {
    /// Count the blocks in a single-frame stream by walking their headers.
    fn count_blocks(frame: &[u8]) -> usize {
        use structured_zstd::decoding::read_frame_header_info;
        let info = read_frame_header_info(frame, false).expect("header must parse");
        let mut at = info.header_size as usize;
        let mut blocks = 0;
        loop {
            let raw = u32::from(frame[at])
                | (u32::from(frame[at + 1]) << 8)
                | (u32::from(frame[at + 2]) << 16);
            let last = (raw & 1) != 0;
            let on_disk = if (raw >> 1) & 0b11 == 1 {
                1
            } else {
                (raw >> 3) as usize
            };
            at += 3 + on_disk;
            blocks += 1;
            if last {
                break;
            }
        }
        blocks
    }

    let opts = parse(&["--target-compressed-block-size=4096", "in.txt"]).unwrap();
    assert_eq!(opts.target_block_size, Some(4096));

    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut default_geometry = Vec::new();
    let level_only = FrameSettings {
        level: 3,
        ..FrameSettings::default()
    };
    compress_stream(
        payload.as_slice(),
        &mut default_geometry,
        &level_only,
        &no_dict(),
    )
    .unwrap();
    let mut small_blocks = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut small_blocks,
        &FrameSettings {
            target_block_size: Some(4096),
            ..level_only
        },
        &no_dict(),
    )
    .unwrap();

    assert!(
        count_blocks(&small_blocks) > count_blocks(&default_geometry),
        "a smaller target must actually produce more, smaller blocks: got {} vs {}",
        count_blocks(&small_blocks),
        count_blocks(&default_geometry)
    );
}

/// Inputs are processed one after another, so an output derived from an early
/// one can land on a file still waiting its turn: `-f foo foo.zst` replaces
/// `foo.zst` before it is ever read, and the original is gone. `-f` says
/// overwrite the output, not destroy another input.
#[test]
fn an_output_may_not_land_on_another_input() {
    let dir = std::env::temp_dir();
    let plain = dir.join(format!("szstd-alias-{}", std::process::id()));
    let archive = PathBuf::from(format!("{}.zst", plain.display()));
    fs::write(&plain, b"first input").unwrap();
    fs::write(&archive, b"second input, would be destroyed").unwrap();

    let mut opts = parse(&["-f", "a", "b"]).unwrap();
    opts.inputs = vec![plain.clone(), archive.clone()];
    let refused = run(opts);

    let survived = fs::read(&archive).unwrap_or_default();
    let _ = fs::remove_file(&plain);
    let _ = fs::remove_file(&archive);
    let err = refused
        .expect_err("an output that is also an input must be refused")
        .to_string();
    assert!(
        err.contains("input"),
        "the refusal must name the collision: {err}"
    );
    assert_eq!(
        survived, b"second input, would be destroyed",
        "the second input must still be there"
    );
}

/// `--rm` deletes an input once its output is safely written, but an input that
/// is also the `-D` dictionary is the one file the output cannot be read back
/// without. Deleting it leaves an archive nothing can open.
#[test]
fn an_input_that_is_the_dictionary_is_not_removed() {
    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-rmdict-{}", std::process::id()));
    let archive = PathBuf::from(format!("{}.zst", sample.display()));
    fs::write(&sample, vec![3u8; 8192]).unwrap();

    let mut opts = parse(&["--rm", "-f", "a"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.dict = Some(sample.clone());
    let refused = run(opts);

    let survived = fs::read(&sample).unwrap_or_default();
    let _ = fs::remove_file(&sample);
    let _ = fs::remove_file(&archive);
    let err = refused
        .expect_err("the dictionary must not be deleted as a source")
        .to_string();
    assert!(
        err.contains("dictionary"),
        "the refusal must name the collision: {err}"
    );
    assert_eq!(survived.len(), 8192, "the dictionary must still be there");
}

/// Decompressing is the other direction, and there the dictionary has already
/// done its work by the time anything is written: the plaintext does not need
/// it, and the destination is replaced only once decoding has finished. The
/// reference command allows it, so refusing would break a script for no gain.
#[test]
fn decompressing_over_the_dictionary_is_allowed() {
    use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

    let dir = std::env::temp_dir();
    let dictionary = dir.join(format!("szstd-dover-{}", std::process::id()));
    let archive = dir.join(format!("szstd-dover-{}.zst", std::process::id()));
    fs::write(&dictionary, vec![2u8; 4096]).unwrap();
    fs::write(
        &archive,
        compress_slice_to_vec(b"decoded payload", CompressionLevel::Default),
    )
    .unwrap();

    let mut opts = parse(&["-d", "-f", "a"]).unwrap();
    opts.inputs = vec![archive.clone()];
    opts.dict = Some(dictionary.clone());
    opts.output = Some(dictionary.clone());
    let decoded = run(opts);

    let written = fs::read(&dictionary).unwrap_or_default();
    let _ = fs::remove_file(&dictionary);
    let _ = fs::remove_file(&archive);
    decoded.expect("the plaintext does not need the dictionary it replaced");
    assert_eq!(
        written, b"decoded payload",
        "the output is the decoded content"
    );
}

/// The scan that keeps an output off the dictionary walks the named inputs, and
/// stdin is not one of them: `-f -D dict -o dict` never reached it, so the
/// dictionary was loaded and then replaced by a frame that needs it. The result
/// is an archive whose key it has just overwritten.
#[test]
fn a_stdin_output_may_not_land_on_the_dictionary() {
    let dir = std::env::temp_dir();
    let dictionary = dir.join(format!("szstd-stdindict-{}", std::process::id()));
    fs::write(&dictionary, vec![4u8; 8192]).unwrap();

    let mut opts = parse(&["-f", "-c"]).unwrap();
    opts.to_stdout = false;
    opts.inputs = Vec::new();
    opts.dict = Some(dictionary.clone());
    opts.output = Some(dictionary.clone());
    let refused = run(opts);

    let survived = fs::read(&dictionary).unwrap_or_default();
    let _ = fs::remove_file(&dictionary);
    let err = refused
        .expect_err("the output must not land on the dictionary it needs")
        .to_string();
    assert!(
        err.contains("dictionary"),
        "the refusal must name the collision: {err}"
    );
    assert_eq!(survived.len(), 8192, "the dictionary must still be there");
}

/// The same collision reached through a symlink. Resolving only the directory
/// leaves the last component as written, so `dict-link -> data` and `data` read
/// as two files; `--rm -D dict-link data` then deletes `data`, the link dangles,
/// and the archive it just made can never be opened again.
#[cfg(unix)]
#[test]
fn a_symlink_to_the_input_is_still_the_dictionary() {
    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-symdict-{}", std::process::id()));
    let link = dir.join(format!("szstd-symdict-link-{}", std::process::id()));
    let archive = PathBuf::from(format!("{}.zst", sample.display()));
    let _ = fs::remove_file(&link);
    fs::write(&sample, vec![5u8; 8192]).unwrap();
    std::os::unix::fs::symlink(&sample, &link).unwrap();

    let mut opts = parse(&["--rm", "-f", "a"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.dict = Some(link.clone());
    let refused = run(opts);

    let survived = fs::read(&sample).unwrap_or_default();
    let _ = fs::remove_file(&sample);
    let _ = fs::remove_file(&link);
    let _ = fs::remove_file(&archive);
    refused.expect_err("a link to the input is the dictionary just the same");
    assert_eq!(survived.len(), 8192, "the dictionary must still be there");
}

/// And the same collision with no name to resolve at all: a hard link is a
/// second name for one file, so canonical paths differ while the bytes are the
/// same. Deleting the input still empties the dictionary.
#[cfg(unix)]
#[test]
fn a_hard_link_to_the_input_is_still_the_dictionary() {
    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-harddict-{}", std::process::id()));
    let link = dir.join(format!("szstd-harddict-link-{}", std::process::id()));
    let archive = PathBuf::from(format!("{}.zst", sample.display()));
    let _ = fs::remove_file(&link);
    fs::write(&sample, vec![6u8; 8192]).unwrap();
    fs::hard_link(&sample, &link).unwrap();

    let mut opts = parse(&["--rm", "-f", "a"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.dict = Some(link.clone());
    let refused = run(opts);

    let survived = fs::read(&link).unwrap_or_default();
    let _ = fs::remove_file(&sample);
    let _ = fs::remove_file(&link);
    let _ = fs::remove_file(&archive);
    refused.expect_err("another name for the input is the dictionary just the same");
    assert_eq!(survived.len(), 8192, "the dictionary must still be there");
}

/// That scan derives the output each input would produce, and testing produces
/// none: `-t archive.zst` names no destination and has no suffix rule to apply.
/// Running the scan for it asks for a path that does not exist, and the whole
/// integrity check dies before a byte is decoded.
#[test]
fn testing_an_archive_does_not_ask_for_an_output_path() {
    let dir = std::env::temp_dir();
    let archive = dir.join(format!("szstd-testmode-{}.zst", std::process::id()));
    use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};
    let frame = compress_slice_to_vec(b"payload to verify", CompressionLevel::Default);
    fs::write(&archive, &frame).unwrap();

    let mut opts = parse(&["-t", "a"]).unwrap();
    opts.inputs = vec![archive.clone()];
    let tested = run(opts);

    let _ = fs::remove_file(&archive);
    tested.expect("a sound archive must test OK");
}

/// Training writes its result over the samples' own directory, and the default
/// destination is a plain `dictionary`. If a sample carries that name, the run
/// reads it and then replaces it — the corpus loses a file to the dictionary
/// built from it, with no `--rm` anywhere in sight.
#[test]
fn training_refuses_to_write_over_its_own_sample() {
    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-trainalias-{}", std::process::id()));
    fs::write(&sample, vec![7u8; 4096]).unwrap();

    let mut opts = parse(&["--train", "-f", "s"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.output = Some(sample.clone());
    let refused = train_dictionary(&opts);

    let survived = fs::read(&sample).unwrap_or_default();
    let _ = fs::remove_file(&sample);
    let err = refused
        .expect_err("a sample must not be replaced by the dictionary trained from it")
        .to_string();
    assert!(
        err.contains("sample"),
        "the refusal must name the collision: {err}"
    );
    assert_eq!(survived.len(), 4096, "the sample must still be there");
}

/// A dictionary cannot be smaller than its own header plus the offset history
/// the format requires, so `--maxdict=1` can only fail. Discovering that after
/// reading the corpus spends the whole input's I/O and memory on a command that
/// was never going to produce anything — the test names a sample that cannot be
/// read, so only a check made first can be what answers.
#[test]
fn an_impossible_dictionary_size_is_refused_before_the_samples_are_read() {
    let missing = std::env::temp_dir().join(format!("szstd-nosuch-{}", std::process::id()));
    let _ = fs::remove_file(&missing);

    let mut opts = parse(&["--train", "--maxdict=1", "-o", "d", "s"]).unwrap();
    opts.inputs = vec![missing];
    opts.output = Some(std::env::temp_dir().join(format!("szstd-nodict-{}", std::process::id())));
    let err = train_dictionary(&opts)
        .expect_err("one byte cannot hold a dictionary")
        .to_string();

    assert!(
        err.contains("--maxdict"),
        "the size must be what is refused, before the unreadable sample: {err}"
    );
}

/// `-c` and `-o` clear one another, so `--train -o wanted.dict -c` leaves no
/// destination at all and the default `dictionary` stands in — the run writes a
/// file nobody named, and with `-f` over whatever was there. The reference
/// command fails on this pair too; it must not silently write somewhere else.
#[test]
fn training_to_stdout_is_refused_rather_than_redirected() {
    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-trainstdout-{}", std::process::id()));
    fs::write(&sample, vec![9u8; 4096]).unwrap();

    let mut opts = parse(&["--train", "-f", "-o", "wanted.dict", "-c", "s"]).unwrap();
    assert!(opts.to_stdout, "-c is the later flag, so it wins");
    assert!(opts.output.is_none(), "and it cleared the -o");
    opts.inputs = vec![sample.clone()];
    let refused = train_dictionary(&opts);

    let _ = fs::remove_file(&sample);
    let err = refused
        .expect_err("a dictionary cannot be trained to stdout here")
        .to_string();
    assert!(
        err.contains("stdout"),
        "the refusal must name what cannot be done: {err}"
    );
    assert!(
        !PathBuf::from("dictionary").exists(),
        "and nothing may be written to the default name"
    );
}

/// The same refusal, for a sample that is the output under another spelling.
/// `dictionary` and `sub/../dictionary` are one file, and the trainer reads its
/// samples before it writes: comparing the two as paths lets the run consume the
/// sample and then replace it with what it learned from it.
#[test]
fn training_refuses_a_sample_that_is_the_output_spelled_differently() {
    let dir = fs::canonicalize(std::env::temp_dir()).unwrap();
    let name = format!("szstd-trainspell-{}", std::process::id());
    let sample = dir.join(&name);
    let detour = dir.join(format!("szstd-trainspell-dir-{}", std::process::id()));
    fs::create_dir_all(&detour).unwrap();
    fs::write(&sample, vec![7u8; 4096]).unwrap();

    let mut opts = parse(&["--train", "-f", "s"]).unwrap();
    opts.inputs = vec![sample.clone()];
    // The same file, reached by walking into a directory and back out of it.
    opts.output = Some(detour.join("..").join(&name));
    let refused = train_dictionary(&opts);
    let _ = fs::remove_dir(&detour);

    let survived = fs::read(&sample).unwrap_or_default();
    let _ = fs::remove_file(&sample);
    let err = refused
        .expect_err("a sample must not be replaced by the dictionary trained from it")
        .to_string();
    assert!(
        err.contains("sample"),
        "the refusal must name the collision: {err}"
    );
    assert_eq!(survived.len(), 4096, "the sample must still be there");
}

/// The `-D` dictionary is what a frame will need to be read back, so an output
/// landing on it destroys the key to the file just written. `-f` permits
/// replacing the output, not the dictionary that gives it meaning.
#[test]
fn an_output_may_not_land_on_the_dictionary() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-dictalias-{}", std::process::id()));
    let dictionary = PathBuf::from(format!("{}.zst", input.display()));
    fs::write(&input, b"data to compress").unwrap();
    fs::write(&dictionary, vec![0u8; 4096]).unwrap();

    let mut opts = parse(&["-f", "a"]).unwrap();
    opts.inputs = vec![input.clone()];
    opts.dict = Some(dictionary.clone());
    let refused = run(opts);

    let survived = fs::read(&dictionary).unwrap_or_default();
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&dictionary);
    let err = refused
        .expect_err("an output that is also the dictionary must be refused")
        .to_string();
    assert!(
        err.contains("dictionary"),
        "the refusal must name the collision: {err}"
    );
    assert_eq!(survived.len(), 4096, "the dictionary must still be there");
}

/// The same file can be named many ways — `foo.zst`, `./foo.zst`,
/// `dir/../dir/foo.zst` — and a guard that compares the spelling rather than
/// the file lets every one of those through. The second input is destroyed just
/// the same.
#[test]
fn an_output_may_not_land_on_another_input_spelled_differently() {
    let dir = std::env::temp_dir();
    let plain = dir.join(format!("szstd-spelling-{}", std::process::id()));
    let archive = PathBuf::from(format!("{}.zst", plain.display()));
    fs::write(&plain, b"first input").unwrap();
    fs::write(&archive, b"second input, would be destroyed").unwrap();

    // The same archive, reached through the directory rather than named
    // directly: a different string for one file.
    let indirect = dir
        .join("..")
        .join(dir.file_name().expect("a temp dir has a name"))
        .join(archive.file_name().expect("the archive has a name"));

    let mut opts = parse(&["-f", "a", "b"]).unwrap();
    opts.inputs = vec![plain.clone(), indirect];
    let refused = run(opts);

    let survived = fs::read(&archive).unwrap_or_default();
    let _ = fs::remove_file(&plain);
    let _ = fs::remove_file(&archive);
    refused.expect_err("the collision is the file, not the spelling");
    assert_eq!(
        survived, b"second input, would be destroyed",
        "the second input must still be there"
    );
}

/// Training reads every sample whole, so a sample has to be a file with an end:
/// a FIFO blocks on a read that never returns and a character device grows the
/// corpus until the allocator gives up — the same reason benchmarking and
/// listing already refuse them.
#[cfg(unix)]
#[test]
fn training_refuses_samples_that_are_not_regular_files() {
    let dir = std::env::temp_dir();
    let fifo = dir.join(format!("szstd-trainfifo-{}", std::process::id()));
    let _ = fs::remove_file(&fifo);
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        return;
    }

    let mut opts = parse(&["--train", "s"]).unwrap();
    opts.inputs = vec![fifo.clone()];
    opts.output = Some(dir.join(format!("szstd-trainfifo-out-{}", std::process::id())));
    let refused = train_dictionary(&opts);
    let _ = fs::remove_file(&fifo);

    let err = refused
        .expect_err("a FIFO is not a training sample")
        .to_string();
    assert!(
        err.contains("regular files"),
        "the refusal must name what is wrong with the sample: {err}"
    );
}

/// The dictionary is read whole, so its size has to be a count of bytes this
/// machine can hold. A 64-bit length cast to a pointer-sized one truncates on a
/// 32-bit target — 4 GiB becomes a capacity of zero — and the read then grows
/// into an allocation that aborts the process instead of returning the refusal
/// the size deserved. Only a 32-bit target can reach it, and the cross-compiled
/// job runs these tests there.
#[test]
fn a_dictionary_bigger_than_the_address_space_is_refused() {
    let dir = std::env::temp_dir();
    let sparse = dir.join(format!("szstd-hugedict-{}", std::process::id()));
    let file = fs::File::create(&sparse).unwrap();
    // Stated, not occupied: a sparse file claims a length without spending a
    // byte of disk on it.
    let stated = u64::from(u32::MAX) + 4096;
    if file.set_len(stated).is_err() {
        let _ = fs::remove_file(&sparse);
        return;
    }
    drop(file);

    let mut opts = parse(&["-d", "a"]).unwrap();
    opts.dict = Some(sparse.clone());
    let loaded = load_dictionary(&opts);
    let _ = fs::remove_file(&sparse);

    if usize::try_from(stated).is_ok() {
        // A machine that can address it may legitimately try to read it; the
        // refusal under test is the one that cannot.
        return;
    }
    let err = loaded
        .expect_err("a dictionary larger than the address space cannot be read")
        .to_string();
    assert!(
        err.contains("address"),
        "the refusal must say the size is the problem: {err}"
    );
}

/// The dictionary is read whole, and its size is taken from the directory entry
/// to bound that read. A FIFO reports zero there, so it clears the memory limit
/// and then blocks in `File::open` until someone opens the other end — the run
/// hangs before it has told anyone what it is waiting for.
#[cfg(unix)]
#[test]
fn a_dictionary_has_to_be_a_regular_file() {
    let dir = std::env::temp_dir();
    let fifo = dir.join(format!("szstd-dictfifo-{}", std::process::id()));
    let _ = fs::remove_file(&fifo);
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        return;
    }

    let mut opts = parse(&["-d", "a"]).unwrap();
    opts.dict = Some(fifo.clone());
    let refused = load_dictionary(&opts);
    let _ = fs::remove_file(&fifo);

    let err = refused.expect_err("a FIFO is not a dictionary").to_string();
    assert!(
        err.contains("regular file"),
        "the refusal must name what is wrong with the dictionary: {err}"
    );
}

/// Compressing does not publish anything: an archive of a private file stays
/// as private as the file was. Creating the output at whatever the umask says
/// hands a 0600 secret to every user on the machine, which is why upstream
/// applies the source's permissions to what it writes.
#[cfg(unix)]
#[test]
fn a_new_output_inherits_the_source_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-perm-{}.txt", std::process::id()));
    fs::write(&input, b"secret payload").unwrap();
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600)).unwrap();

    let mut opts = parse(&["-3", "-q", "f"]).unwrap();
    opts.inputs = vec![input.clone()];
    let result = process_file(&opts, &input, &no_dict(), 1);

    let output = PathBuf::from(format!("{}.zst", input.display()));
    let mode = fs::metadata(&output).map(|m| m.permissions().mode() & 0o777);
    let _ = fs::remove_file(&input);
    let _ = fs::remove_file(&output);
    result.expect("compressing must succeed");
    assert_eq!(
        mode.expect("the archive must exist"),
        0o600,
        "the archive must be no more readable than the file it came from"
    );
}

/// A dictionary carries stretches of its corpus verbatim — that is what makes
/// it a dictionary — so training on private samples and leaving the result at
/// whatever the umask allows publishes fragments of them. It is the same rule
/// an archive follows, and with several samples the answer is the strictest of
/// them: no sample's bits may be readable through the dictionary by anyone who
/// could not read that sample.
#[cfg(unix)]
#[test]
fn a_trained_dictionary_is_no_more_readable_than_its_samples() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir();
    let open = dir.join(format!("szstd-trainperm-open-{}", std::process::id()));
    let private = dir.join(format!("szstd-trainperm-private-{}", std::process::id()));
    let output = dir.join(format!("szstd-trainperm-out-{}", std::process::id()));
    let corpus: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    fs::write(&open, &corpus).unwrap();
    fs::write(&private, &corpus).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();

    let mut opts = parse(&["--train", "-f", "s"]).unwrap();
    opts.inputs = vec![open.clone(), private.clone()];
    opts.output = Some(output.clone());
    let trained = train_dictionary(&opts);

    let mode = fs::metadata(&output).map(|m| m.permissions().mode() & 0o777);
    let _ = fs::remove_file(&open);
    let _ = fs::remove_file(&private);
    let _ = fs::remove_file(&output);
    trained.expect("training must succeed");
    assert_eq!(
        mode.expect("the dictionary must exist"),
        0o600,
        "the strictest sample decides: the dictionary holds bytes from it"
    );
}

/// The `-M` ceiling for a benchmark counts three copies of the input, so the
/// buffers holding them have to be the size they were counted at. A `Vec` grown
/// by appending carries up to twice the bytes it holds, and reading each file
/// into its own buffer first holds the largest one twice over — a run admitted
/// as fitting could then take far more than it was allowed. Read straight into
/// one buffer of the size the files add up to.
#[test]
fn the_benchmark_input_is_the_size_it_was_counted_at() {
    let dir = std::env::temp_dir();
    let first = dir.join(format!("szstd-benchcap-a-{}", std::process::id()));
    let second = dir.join(format!("szstd-benchcap-b-{}", std::process::id()));
    // Sizes where appending would double past the total rather than land on it.
    fs::write(&first, vec![1u8; 8000]).unwrap();
    fs::write(&second, vec![2u8; 1000]).unwrap();

    let combined = read_inputs_bounded(&[first.clone(), second.clone()], &[8000, 1000]);

    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    let combined = combined.expect("both files must be read");
    assert_eq!(combined.len(), 9_000, "the whole of both files");
    assert_eq!(
        combined.capacity(),
        9_000,
        "and no more memory than that was taken to hold them"
    );
    assert_eq!(combined[7999], 1, "the first file's bytes come first");
    assert_eq!(combined[8000], 2, "the second file's follow");
}

/// Permission bits alone do not say who they let in. Two samples at `0640` may
/// belong to different groups, and the dictionary belongs to whichever group the
/// directory it was created in gave it — so keeping the group bits would open
/// corpus fragments to a group that could not read the sample they came from.
/// The owner's bits are safe (every sample was read, so this user was entitled
/// to its bytes) and the world's name nobody, so only the group's are at stake.
#[cfg(unix)]
#[test]
fn group_access_survives_only_when_it_means_the_same_group() {
    // One owner, one group, and the dictionary lands in it: the bits mean what
    // they did on the samples, so they stand.
    assert_eq!(
        output_mode_for_sources(&[(0o640, 100), (0o640, 100)], 100),
        0o640,
        "one group throughout, and the dictionary is in it"
    );
    // Same bits, different groups: `0640` on one sample admits a group that the
    // other's `0640` does not, and the dictionary can only be in one of them.
    assert_eq!(
        output_mode_for_sources(&[(0o640, 100), (0o640, 200)], 100),
        0o600,
        "two groups cannot both be meant"
    );
    // One group among the samples, but the dictionary was created in another —
    // a setgid directory does exactly this.
    assert_eq!(
        output_mode_for_sources(&[(0o640, 100), (0o640, 100)], 200),
        0o600,
        "the dictionary's own group is not the samples'"
    );
    // The world names no principal, so those bits are the plain intersection.
    assert_eq!(
        output_mode_for_sources(&[(0o644, 100), (0o644, 200)], 300),
        0o604,
        "world access survives what group access cannot"
    );
    assert_eq!(
        output_mode_for_sources(&[(0o644, 100), (0o600, 100)], 100),
        0o600,
        "and the strictest sample still decides every bit"
    );
}

/// Set-user-ID and set-group-ID say who a program runs AS, which has nothing to
/// do with who may read the bytes an output carries — and everything to do with
/// privilege. Copying them from a source hands them to a file with a different
/// owner: a privileged run over an attacker's `04755` archive would write a
/// root-owned `04755` file. The reference command carries neither, and neither
/// does this one.
#[cfg(unix)]
#[test]
fn an_output_never_carries_the_privilege_bits_of_its_source() {
    assert_eq!(
        output_mode_for_sources(&[(0o4755, 100)], 100),
        0o755,
        "set-user-ID is not a permission to copy"
    );
    assert_eq!(
        output_mode_for_sources(&[(0o2755, 100)], 100),
        0o755,
        "nor is set-group-ID"
    );
    assert_eq!(
        output_mode_for_sources(&[(0o6644, 100)], 100),
        0o644,
        "and the ordinary bits below them are unaffected"
    );
}

/// The same, through the whole run: the mode is applied after the bytes are
/// written, so an overwrite re-applies it once the kernel's own stripping of
/// those bits on write has already happened.
#[cfg(unix)]
#[test]
fn compressing_a_setuid_file_does_not_produce_a_setuid_archive() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir();
    let source = dir.join(format!("szstd-suid-{}", std::process::id()));
    let archive = dir.join(format!("szstd-suid-out-{}", std::process::id()));
    fs::write(&source, vec![1u8; 4096]).unwrap();
    fs::write(&archive, b"an output that already exists").unwrap();
    if fs::set_permissions(&source, fs::Permissions::from_mode(0o4755)).is_err() {
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&archive);
        return;
    }

    let mut opts = parse(&["-3", "-f", "-q", "s"]).unwrap();
    opts.inputs = vec![source.clone()];
    opts.output = Some(archive.clone());
    let compressed = process_file(&opts, &source, &no_dict(), 1);
    let mode = fs::metadata(&archive).map(|m| m.permissions().mode() & 0o7777);

    let _ = fs::remove_file(&source);
    let _ = fs::remove_file(&archive);
    compressed.expect("compressing must succeed");
    assert_eq!(
        mode.expect("the archive must exist") & 0o6000,
        0,
        "no archive carries set-user-ID or set-group-ID"
    );
}

/// The rule is not the trainer's: an archive carries its source's bytes just as
/// a dictionary carries its samples', and a `0640` source in group A compressed
/// into a setgid directory of group B would hand group B what it could not read.
/// One source is the same question as many, so it takes the same answer.
#[cfg(unix)]
#[test]
fn an_archive_keeps_group_access_only_when_it_means_the_same_group() {
    assert_eq!(
        output_mode_for_sources(&[(0o640, 100)], 100),
        0o640,
        "the archive is in the source's group, so the bits mean what they did"
    );
    assert_eq!(
        output_mode_for_sources(&[(0o640, 100)], 200),
        0o600,
        "a setgid directory put it in another group, which those bits never meant"
    );
    assert_eq!(
        output_mode_for_sources(&[(0o644, 100)], 200),
        0o604,
        "world access is nobody's group, so it survives"
    );
}

/// Replacing an existing file is where a permission rule is easiest to lose:
/// the destination's own mode is restored over the one the source asked for.
/// A `-f` retrain over a world-readable dictionary would publish the corpus it
/// was told to keep private, and compressing a private file over an existing
/// archive would do the same — the reference command applies the source's mode
/// either way.
#[cfg(unix)]
#[test]
fn replacing_a_file_does_not_restore_its_old_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-replace-sample-{}", std::process::id()));
    let dictionary = dir.join(format!("szstd-replace-dict-{}", std::process::id()));
    let corpus: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    fs::write(&sample, &corpus).unwrap();
    fs::write(&dictionary, b"the dictionary that was here before").unwrap();
    fs::set_permissions(&sample, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&dictionary, fs::Permissions::from_mode(0o644)).unwrap();

    let mut opts = parse(&["--train", "-f", "s"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.output = Some(dictionary.clone());
    let trained = train_dictionary(&opts);
    let dictionary_mode = fs::metadata(&dictionary).map(|m| m.permissions().mode() & 0o777);

    // The same rule for an ordinary archive written over one that exists.
    let archive = dir.join(format!("szstd-replace-arch-{}", std::process::id()));
    fs::write(&archive, b"the archive that was here before").unwrap();
    fs::set_permissions(&archive, fs::Permissions::from_mode(0o644)).unwrap();
    let mut opts = parse(&["-3", "-f", "-q", "s"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.output = Some(archive.clone());
    let compressed = process_file(&opts, &sample, &no_dict(), 1);
    let archive_mode = fs::metadata(&archive).map(|m| m.permissions().mode() & 0o777);

    let _ = fs::remove_file(&sample);
    let _ = fs::remove_file(&dictionary);
    let _ = fs::remove_file(&archive);
    trained.expect("training must succeed");
    compressed.expect("compressing must succeed");
    assert_eq!(
        dictionary_mode.expect("the dictionary must exist"),
        0o600,
        "the retrained dictionary holds the private sample's bytes"
    );
    assert_eq!(
        archive_mode.expect("the archive must exist"),
        0o600,
        "the archive holds the private file's bytes"
    );
}

/// Listing walks frame headers and training builds a dictionary from samples;
/// neither reads the one `-D` names. Loading it anyway fails a listing over a
/// missing file that has nothing to do with it, and spends time and memory on a
/// large one that is never consulted.
#[test]
fn a_dictionary_is_loaded_only_where_it_is_used() {
    let missing = PathBuf::from("/nonexistent/dictionary/for/this/test");

    let mut listing = parse(&["-l", "f"]).unwrap();
    listing.dict = Some(missing.clone());
    assert!(
        load_dictionary(&listing)
            .expect("listing does not read it")
            .is_none(),
        "a listing has no use for a dictionary"
    );

    let mut training = parse(&["--train", "s1"]).unwrap();
    training.dict = Some(missing.clone());
    assert!(
        load_dictionary(&training)
            .expect("training does not read it")
            .is_none(),
        "training builds a dictionary rather than using one"
    );

    // The modes that do use it still fail on a missing file.
    let mut decompressing = parse(&["-d", "f"]).unwrap();
    decompressing.dict = Some(missing);
    assert!(load_dictionary(&decompressing).is_err());
}

/// `--stream-size` exists precisely for inputs whose length cannot be stat'd.
/// A named FIFO is one of those, so the per-file size being unavailable is the
/// moment the option matters most — dropping it there leaves the pledge
/// unrecorded for exactly the inputs it was written for.
#[test]
fn an_explicit_stream_size_survives_an_unstattable_input() {
    use structured_zstd::decoding::{FrameContentSize, read_frame_header_info};

    let payload = b"hello world hello world";
    let mut opts = parse(&["-3", "f"]).unwrap();
    opts.pledged_size = Some(payload.len() as u64);

    let mut frame = Vec::new();
    // `None` is what a FIFO or device yields: no reliable size from metadata.
    let processed = stream(
        &opts,
        &no_dict(),
        ProgressMonitor::new(&payload[..], 0, false),
        None,
        &mut frame,
    )
    .expect("compressing must succeed");
    assert_eq!(processed.read, payload.len() as u64);
    assert_eq!(processed.written, frame.len() as u64);

    let info = read_frame_header_info(&frame, false).expect("the frame header must parse");
    assert_eq!(
        info.content_size,
        FrameContentSize::Known(payload.len() as u64),
        "the pledged size must reach the frame even when the input cannot be stat'd"
    );
}

/// A serialized dictionary selects its compression-parameter tier by the size
/// of the whole blob, entropy tables included, not by the content inside it.
/// Parsing the blob and handing over only the content picks a different tier
/// for dictionaries near a boundary, so `-D` would compress differently from
/// the same dictionary given to the library directly.
#[test]
fn a_serialized_dictionary_keeps_the_size_its_tier_is_chosen_by() {
    use structured_zstd::encoding::{CompressionLevel, StreamingEncoder};

    // Sized so the two lengths fall on opposite sides of a tier boundary: the
    // content alone selects the 16 KiB row, the whole blob does not. Built
    // from the fixture's own header so it stays a valid dictionary.
    let fixture = include_bytes!("../../../dict_tests/dictionary");
    let content_len = structured_zstd::decoding::Dictionary::decode_dict(fixture)
        .expect("the fixture must parse")
        .dict_content
        .len();
    let header = &fixture[..fixture.len() - content_len];
    assert!(
        header.len() > 100,
        "the header has to be what puts the blob over the boundary"
    );
    let mut raw = header.to_vec();
    raw.extend_from_slice(&fixture[fixture.len() - content_len..][..16 * 1024 - 498 - 100]);
    assert!(16 * 1024 - 498 - 100 + 498 <= 16 * 1024 && raw.len() + 498 > 16 * 1024);

    let payload: Vec<u8> = (0..40_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();

    let mut through_cli = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut through_cli,
        &FrameSettings {
            level: 11,
            pledged_size: Some(payload.len() as u64),
            ..FrameSettings::default()
        },
        &prepared_dict(&raw),
    )
    .expect("compressing with the dictionary must succeed");

    let mut through_library = StreamingEncoder::new(Vec::new(), CompressionLevel::Level(11));
    through_library.set_content_checksum(true).unwrap();
    through_library
        .set_pledged_content_size(payload.len() as u64)
        .unwrap();
    through_library.set_dictionary_from_bytes(&raw[..]).unwrap();
    through_library.write_all(&payload).unwrap();
    let expected = through_library.finish().unwrap();

    assert_eq!(
        through_cli, expected,
        "`-D` must compress exactly as the same dictionary handed to the library does"
    );
}

/// Zero is a sentinel in three of these options, and each one says so in the
/// API this tool mirrors: no block target, no size hint, no dictionary.
/// Passing the zero through instead turns each into its opposite — the
/// smallest allowed block, an empty-source hint, a dictionary rejected as
/// corrupt.
#[test]
fn zero_means_unset_where_the_api_says_it_does() {
    // `ZSTD_c_targetCBlockSize`: "No target when targetCBlockSize == 0."
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut targeted = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut targeted,
        &FrameSettings {
            level: 3,
            target_block_size: Some(0),
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing must succeed");
    let mut untargeted = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut untargeted,
        &FrameSettings {
            level: 3,
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing must succeed");
    assert_eq!(
        targeted, untargeted,
        "a zero target must leave the block geometry alone"
    );

    // `ZSTD_c_srcSizeHint`: "Hint is not valid when srcSizeHint == 0."
    let mut hinted = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut hinted,
        &FrameSettings {
            level: 3,
            size_hint: Some(0),
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing must succeed");
    assert_eq!(
        hinted, untargeted,
        "a zero hint must not size the encoder for an empty source"
    );

    // `ZSTD_CCtx_loadDictionary`: a zero-size dictionary "returns to
    // no-dictionary mode".
    let mut without = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut without,
        &FrameSettings {
            level: 3,
            ..FrameSettings::default()
        },
        &prepared_dict(&[]),
    )
    .expect("an empty dictionary is no dictionary, not a broken one");
    assert_eq!(without, untargeted, "and produces the same frame");
}

/// An empty file holds no frame, so there is nothing to test and nothing to
/// decode. Answering `OK` for it says the archive checked out when it was never
/// an archive; upstream calls it an unexpected end of file.
#[test]
fn an_empty_stream_is_not_a_valid_archive() {
    decompress_stream(&b""[..], io::sink(), &no_dict(), &DecodeSettings::default())
        .expect_err("an empty input carries no frame to decode");
    // Even under pass-through: an empty file is not an archive either.
    decompress_stream(
        &b""[..],
        io::sink(),
        &no_dict(),
        &DecodeSettings {
            verify_checksum: true,
            pass_through: true,
        },
    )
    .expect_err("nothing to pass through is still nothing");
}

/// Skippable frames sit inside ordinary archives — seekable-zstd puts its index
/// in one, and callers attach their own metadata the same way. Listing has to
/// walk past them like any decoder does, or `-l` refuses files the reference
/// tool lists without complaint.
#[test]
fn list_file_walks_past_skippable_frames() {
    use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

    let mut archive = compress_slice_to_vec(b"first frame", CompressionLevel::Default);
    // Magic 0x184D2A50 (little-endian) + a 4-byte length + that many bytes.
    archive.extend_from_slice(&0x184D_2A50_u32.to_le_bytes());
    archive.extend_from_slice(&4_u32.to_le_bytes());
    archive.extend_from_slice(b"meta");
    archive.extend_from_slice(&compress_slice_to_vec(
        b"second frame",
        CompressionLevel::Default,
    ));

    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-list-skip-{}.zst", std::process::id()));
    fs::write(&path, &archive).unwrap();
    let result = list_file(&path, true, 0);
    let _ = fs::remove_file(&path);
    let summary = result.expect("a skippable frame between two frames must not fail the listing");
    assert_eq!((summary.frames, summary.skips), (3, 1));
}

/// Frame_Content_Size is a declaration, not a measurement: a few bytes of
/// header can claim any size at all. Summing those declarations unchecked lets
/// a tiny crafted file either crash `-l` or have it report a wrapped-around
/// total as fact.
#[test]
fn list_file_refuses_a_content_size_total_that_overflows() {
    /// A frame declaring `content_size` bytes but holding one RLE block.
    fn frame_declaring(content_size: u64) -> Vec<u8> {
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD];
        // Descriptor: 8-byte Frame_Content_Size, no dictionary, no checksum,
        // window descriptor present (not single-segment).
        frame.push(0b11 << 6);
        // Window descriptor: exponent 10, i.e. a 1 MiB window.
        frame.push(10 << 3);
        frame.extend_from_slice(&content_size.to_le_bytes());
        // One RLE block, last of the frame: size 1, type 1, last-block bit set.
        let block_header = (1u32 << 3) | (1 << 1) | 1;
        frame.extend_from_slice(&block_header.to_le_bytes()[..3]);
        frame.push(b'x');
        frame
    }

    let mut archive = frame_declaring(u64::MAX);
    archive.extend_from_slice(&frame_declaring(u64::MAX));

    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-list-overflow-{}.zst", std::process::id()));
    fs::write(&path, &archive).unwrap();
    let result = list_file(&path, false, 0);
    let _ = fs::remove_file(&path);
    result.expect_err("a total that cannot be represented must be reported, not wrapped");
}

#[test]
fn argv0_unzstd_defaults_to_decompress() {
    let preset = program_preset("unzstd");
    assert_eq!(preset.mode, Mode::Decompress);
    assert!(!preset.to_stdout);
    assert!(!preset.force);
    assert_eq!(program_preset("/usr/bin/unzstd").mode, Mode::Decompress);
    assert_eq!(program_preset("unzstd.exe").mode, Mode::Decompress);
}

/// `zstdcat` is `zstd -dcf` with pass-through and the quiet level, as the
/// reference command sets it up: a `zcat` replacement copies a plain file
/// through rather than refusing it, and says nothing on success.
#[test]
fn argv0_zstdcat_decompresses_to_stdout_passing_plain_input_through() {
    for name in ["zstdcat", "zcat", "/usr/local/bin/zstdcat"] {
        let preset = program_preset(name);
        assert_eq!(preset.mode, Mode::Decompress, "{name}");
        assert!(preset.to_stdout, "{name}");
        assert!(preset.force, "{name}");
        assert_eq!(preset.pass_through, Some(true), "{name}");
        assert_eq!(preset.verbosity, 1, "{name}");
    }
    let plain = program_preset("zstd");
    assert_eq!(plain.mode, Mode::Compress);
    assert!(!plain.to_stdout && !plain.force);
    assert_eq!(plain.pass_through, None);
    assert_eq!(plain.verbosity, DEFAULT_LEVEL);
    // `zstdmt` compresses like `zstd`; its worker count has no effect here.
    assert_eq!(program_preset("zstdmt").mode, Mode::Compress);
}

/// The preset is the starting point, not the last word: `zstdcat -v` still
/// raises the level, and `--no-pass-through` still turns pass-through off.
#[test]
fn flags_adjust_the_argv0_preset() {
    let cat = program_preset("zstdcat");
    let quiet = parse_as(&cat, 3, &["a.zst"]).unwrap();
    assert_eq!(quiet.verbosity, 1);
    assert!(quiet.force && quiet.follow_links);
    assert!(
        DecodeSettings::from_options(&quiet).pass_through,
        "zstdcat passes unknown input through"
    );
    let louder = parse_as(&cat, 3, &["-v", "--no-pass-through", "a.zst"]).unwrap();
    assert_eq!(louder.verbosity, 2);
    assert!(!DecodeSettings::from_options(&louder).pass_through);
}

#[test]
fn bare_numeric_flag_is_a_level() {
    let opts = parse(&["-19", "in.txt"]).unwrap();
    assert_eq!(opts.level, 19);
    assert_eq!(opts.mode, Mode::Compress);
    assert_eq!(opts.inputs, vec![PathBuf::from("in.txt")]);
}

/// Levels 20-22 are expensive enough that they have to be asked for by name,
/// but asking without `--ultra` is not an error: upstream warns and compresses
/// at 19 ("Warning : compression level higher than max, reduced to 19", exit
/// 0), so a script that runs `zstd -22` keeps working. Refusing instead breaks
/// it against us.
#[test]
fn levels_above_19_without_ultra_fall_back_to_19() {
    assert_eq!(parse(&["-22", "in.txt"]).unwrap().level, 19);
    assert_eq!(parse(&["-20", "in.txt"]).unwrap().level, 19);
    // Named, they run as asked.
    assert_eq!(parse(&["--ultra", "-22", "in.txt"]).unwrap().level, 22);
    // Benchmarking compresses the range `-b`/`-e` name rather than the level
    // `-N` sets, so the range is what gets clamped: `-b20` reaches an ultra
    // level as surely as `-20` does.
    let opts = parse(&["-b20", "in.txt"]).unwrap();
    assert_eq!((opts.bench_start, opts.bench_end), (19, 19));
    let opts = parse(&["-b3", "-e22", "in.txt"]).unwrap();
    assert_eq!((opts.bench_start, opts.bench_end), (3, 19));
    assert_eq!(
        parse(&["--ultra", "-b20", "in.txt"]).unwrap().bench_start,
        20
    );
    // Below the ultra band nothing moves.
    assert_eq!(parse(&["-19", "in.txt"]).unwrap().level, 19);
}

#[test]
fn fast_flag_maps_to_negative_level() {
    assert_eq!(parse(&["--fast"]).unwrap().level, -1);
    assert_eq!(parse(&["--fast=5"]).unwrap().level, -5);
    // `--fast=0` negates to level 0, which means the ordinary default — the
    // opposite of what the flag was asked for. Upstream calls it an incorrect
    // parameter rather than quietly compressing at another level.
    assert!(parse(&["--fast=0"]).is_err());
}

#[test]
fn fast_level_reads_a_digit_run_and_ignores_the_tail() {
    // upstream zstd (zstdcli.c:1139 -> readU32FromCharChecked, 350-376): the
    // acceleration factor is the LEADING digit run; the parser stops at the
    // first byte that is neither a digit nor a K/M multiplier and `--fast`
    // never looks at what is left. `zstd --fast=3.5` compresses at level -3,
    // so refusing it turns a working command line into an error.
    assert_eq!(parse(&["--fast=3.5"]).unwrap().level, -3);
    assert_eq!(parse(&["--fast=3x"]).unwrap().level, -3);
    assert_eq!(parse(&["--fast=3G"]).unwrap().level, -3);
    // A sign is not a digit, so the run is empty and the factor is zero —
    // which upstream rejects. Rust's own integer parser accepts `+3`, and
    // accepting it here would take a level upstream refuses.
    assert!(parse(&["--fast=+3"]).is_err());
}

#[test]
fn fast_level_honours_the_k_and_m_multipliers() {
    // Same reader, suffix half (zstdcli.c:362-373): `K` shifts by 10, `M` by
    // 20, each with an optional `i` and `B` spelling.
    assert_eq!(parse(&["--fast=1K"]).unwrap().level, -1024);
    assert_eq!(parse(&["--fast=2KiB"]).unwrap().level, -2048);
    // A bare multiplier has no digits before it, so it reads as zero.
    assert!(parse(&["--fast=K"]).is_err());
}

#[test]
fn fast_level_clamps_to_the_minimum_level() {
    // upstream zstd (zstdcli.c:1136-1140): a factor past `-ZSTD_minCLevel()`
    // is CLAMPED, not refused — `zstd --fast=200000` compresses at the
    // lowest level rather than failing.
    let min = structured_zstd::encoding::CompressionLevel::MIN_LEVEL;
    assert_eq!(parse(&["--fast=200000"]).unwrap().level, min);
    assert_eq!(parse(&["--fast=1M"]).unwrap().level, min);
    // The digit run itself still has to fit 32 bits (zstdcli.c:356-359).
    assert!(parse(&["--fast=99999999999"]).is_err());
}

#[test]
fn clustered_short_flags() {
    // -d (decompress) + -c (stdout) + -k (keep) in one token.
    let opts = parse(&["-dck", "a.zst"]).unwrap();
    assert_eq!(opts.mode, Mode::Decompress);
    assert!(opts.to_stdout);
    assert!(opts.keep);
}

#[test]
fn dict_and_output_take_values() {
    let opts = parse(&["-D", "dict.bin", "-o", "out.zst", "in.txt"]).unwrap();
    assert_eq!(opts.dict, Some(PathBuf::from("dict.bin")));
    assert_eq!(opts.output, Some(PathBuf::from("out.zst")));
    // Attached value form: -Ddict.bin
    let opts = parse(&["-Ddict.bin", "in.txt"]).unwrap();
    assert_eq!(opts.dict, Some(PathBuf::from("dict.bin")));
}

/// Several inputs into one `-o` is a concatenation, which the reference
/// command permits after a warning; the command line itself is not wrong.
#[test]
fn output_accepts_multiple_inputs_for_concatenation() {
    let opts = parse(&["-o", "out.zst", "a.txt", "b.txt"]).unwrap();
    assert_eq!(opts.inputs.len(), 2);
    assert_eq!(opts.output, Some(PathBuf::from("out.zst")));
}

#[test]
fn train_flags_parse_and_allow_many_samples_with_output() {
    let opts = parse(&[
        "--train",
        "--maxdict=4096",
        "--dictID=42",
        "-o",
        "dict.bin",
        "s1.txt",
        "s2.txt",
        "s3.txt",
    ])
    .unwrap();
    assert_eq!(opts.mode, Mode::Train);
    assert_eq!(opts.max_dict, 4096);
    assert_eq!(opts.dict_id, Some(42));
    assert_eq!(opts.output, Some(PathBuf::from("dict.bin")));
    // --train legitimately fans many samples into one -o dictionary.
    assert_eq!(opts.inputs.len(), 3);
}

/// Zero is how the dictionary API spells "pick one for me" (`zdict.h`: "force
/// dictID value; 0 means auto mode"). Carrying it through as an explicit id
/// makes the trainer refuse a request that asked for the default.
#[test]
fn a_zero_dict_id_means_automatic() {
    assert_eq!(
        parse(&["--train", "--dictID=0", "s1"]).unwrap().dict_id,
        None
    );
    assert_eq!(
        parse(&["--train", "--dictID=42", "s1"]).unwrap().dict_id,
        Some(42)
    );
}

#[test]
fn list_mode_parses() {
    assert_eq!(parse(&["-l", "a.zst"]).unwrap().mode, Mode::List);
    assert_eq!(parse(&["--list", "a.zst"]).unwrap().mode, Mode::List);
}

#[test]
fn long_flag_enables_ldm() {
    assert!(parse(&["-19", "--long", "in.txt"]).unwrap().long);
    assert!(parse(&["-19", "--long=27", "in.txt"]).unwrap().long);
    assert!(!parse(&["-19", "in.txt"]).unwrap().long);
    // Bare `--long` is `--long=27` upstream, and the window is the point of the
    // flag: keeping the level's own window would reach back nowhere near the
    // distance the caller asked for.
    assert_eq!(
        parse(&["-19", "--long", "in.txt"]).unwrap().long_window_log,
        Some(27)
    );
}

/// `--long=N` names the window the caller wants — the match distance the
/// encoder may reach back over, and with it the memory a decoder will need.
/// Enabling long-distance matching while dropping the N reaches back only as
/// far as the level would have anyway, so the matches the flag was typed for
/// are the ones it does not find.
#[test]
fn long_window_log_reaches_the_encoder() {
    use structured_zstd::decoding::read_frame_header_info;

    let opts = parse(&["-19", "--long=27", "in.txt"]).unwrap();
    assert_eq!(
        opts.long_window_log,
        Some(27),
        "the value must survive parsing"
    );

    let mut frame = Vec::new();
    compress_stream(
        &b"payload"[..],
        &mut frame,
        &FrameSettings {
            level: 19,
            long: true,
            long_window_log: Some(27),
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing with an explicit window log must succeed");
    let info = read_frame_header_info(&frame, false).expect("the frame header must parse");
    assert_eq!(
        info.window_size,
        1 << 27,
        "the frame has to declare the window the caller asked for"
    );
}

/// Training writes a file like any other output, so it answers to `-f` like any
/// other output. Without the gate `--train -o existing` replaces the file with
/// no warning, and naming a sample as the destination destroys the sample.
#[test]
fn training_refuses_to_overwrite_without_force() {
    let dir = std::env::temp_dir();
    let sample = dir.join(format!("szstd-train-sample-{}.bin", std::process::id()));
    let existing = dir.join(format!("szstd-train-out-{}.bin", std::process::id()));
    fs::write(&sample, vec![7u8; 4096]).unwrap();
    fs::write(&existing, b"precious").unwrap();

    // Unreadable, so the answer says which step ran first: a command that
    // cannot write its result should be refused before it spends the memory
    // and minutes of building one.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&sample, fs::Permissions::from_mode(0o000)).unwrap();
    }

    let mut opts = parse(&["--train", "s"]).unwrap();
    opts.inputs = vec![sample.clone()];
    opts.output = Some(existing.clone());
    let refused = train_dictionary(&opts);

    let survived = fs::read(&existing).unwrap_or_default();
    let _ = fs::remove_file(&sample);
    let _ = fs::remove_file(&existing);
    let err = refused
        .expect_err("an existing dictionary must not be replaced without -f")
        .to_string();
    assert!(
        err.contains("already exists"),
        "the refusal must be the destination, before any sample is read: {err}"
    );
    assert_eq!(survived, b"precious", "the existing file must be untouched");
}

/// The trainer flags name algorithms, and the algorithm decides what the
/// dictionary contains. Only FastCOVER is implemented here, so the flags that
/// ask for COVER or the legacy trainer have to say no — running FastCOVER under
/// their name returns a dictionary the caller did not ask for.
#[test]
fn unimplemented_trainers_are_refused_not_substituted() {
    assert_eq!(parse(&["--train", "s1"]).unwrap().mode, Mode::Train);
    assert_eq!(
        parse(&["--train-fastcover", "s1"]).unwrap().mode,
        Mode::Train
    );
    assert!(parse(&["--train-cover", "s1"]).is_err());
    assert!(parse(&["--train-legacy", "s1"]).is_err());
}

/// A window is a promise about how much memory decoding will need, so it is
/// bounded by how much data there is: upstream compresses a 24-byte file with
/// `--long=27` into a frame declaring 24 bytes, not 128 MiB. Declaring the
/// asked-for window regardless makes every small `--long` frame demand the
/// whole ceiling from every decoder that opens it.
#[test]
fn an_explicit_window_is_capped_by_a_known_source_size() {
    use structured_zstd::decoding::read_frame_header_info;

    let payload = b"hello world hello world";
    let mut frame = Vec::new();
    compress_stream(
        &payload[..],
        &mut frame,
        &FrameSettings {
            level: 19,
            long: true,
            long_window_log: Some(27),
            pledged_size: Some(payload.len() as u64),
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing must succeed");

    let info = read_frame_header_info(&frame, false).expect("the frame header must parse");
    assert!(
        info.window_size <= 1 << 20,
        "a {}-byte frame must not declare a {} byte window",
        payload.len(),
        info.window_size
    );

    // A dictionary frame runs the dictionary's own search geometry, but its
    // window is still a statement about memory and answers to the same source.
    let raw = include_bytes!("../../../dict_tests/dictionary");
    let mut dict_frame = Vec::new();
    compress_stream(
        &payload[..],
        &mut dict_frame,
        &FrameSettings {
            level: 19,
            long: true,
            long_window_log: Some(27),
            pledged_size: Some(payload.len() as u64),
            ..FrameSettings::default()
        },
        &prepared_dict(raw),
    )
    .expect("compressing with a dictionary must succeed");

    let dict_info =
        read_frame_header_info(&dict_frame, false).expect("the frame header must parse");
    assert!(
        dict_info.window_size <= 1 << 20,
        "a {}-byte dictionary frame must not declare a {} byte window",
        payload.len(),
        dict_info.window_size
    );
}

/// Long-distance matching lives on the optimal parser here, so the levels below
/// it can widen the window and still never run the matcher the flag names.
/// Taking `--long` at those levels would report success for work that did not
/// happen, and hand back a file compressed the ordinary way.
#[test]
fn long_is_refused_at_levels_that_cannot_run_it() {
    assert!(parse(&["-3", "--long", "in.txt"]).is_err());
    assert!(parse(&["--long", "-15", "in.txt"]).is_err());
    // From the optimal parser up, the matcher is there to run.
    assert!(parse(&["-16", "--long", "in.txt"]).is_ok());
    assert!(parse(&["--ultra", "-22", "--long=27", "in.txt"]).is_ok());
    // Decompression takes the flag as the window hint it is; nothing to run.
    assert!(parse(&["-d", "--long", "in.txt.zst"]).is_ok());
    // Benchmarking compresses the levels `-b`/`-e` name, not the one `-N` set,
    // so those are the levels the flag has to be true of.
    assert!(parse(&["-b16", "--long", "in.txt"]).is_ok());
    assert!(parse(&["-b16", "-e19", "--long", "in.txt"]).is_ok());
    assert!(parse(&["-b3", "--long", "in.txt"]).is_err());
    // The whole range runs with the flag, so a range that starts below the
    // matcher is refused even when it ends above it.
    assert!(parse(&["-b3", "-e19", "--long", "in.txt"]).is_err());
    // And a benchmark range is what counts, not a level that was also typed.
    assert!(parse(&["-16", "-b3", "--long", "in.txt"]).is_err());
    // Benchmarking compresses whatever mode was asked for, so `-d` does not
    // excuse the flag from naming levels that can run it.
    assert!(parse(&["-d", "-b3", "--long", "in.txt"]).is_err());
    assert!(parse(&["-d", "-b16", "--long", "in.txt"]).is_ok());
}

/// The window log has a supported range; a value outside it has to fail rather
/// than be quietly replaced by a working one, or the caller believes in a
/// window the frame does not have.
#[test]
fn out_of_range_long_window_log_is_refused() {
    assert!(parse(&["-19", "--long=99", "in.txt"]).is_err());
    // The encoder would accept up to 30, but this build's decoder refuses any
    // frame declaring a window above 128 MiB — so those levels only produce
    // files it cannot read back. Refuse them at the flag instead.
    assert!(parse(&["-19", "--long=27", "in.txt"]).is_ok());
    assert!(parse(&["-19", "--long=28", "in.txt"]).is_err());
}

/// A benchmark range is two levels, and both of them run. Checking only the
/// top lets `-b-200000` through parsing, so the files are stat'd and read whole
/// before the first level is rejected — work for a command that was never going
/// to run.
#[test]
fn a_benchmark_range_is_validated_at_both_ends() {
    assert!(parse(&["-b-200000", "in.txt"]).is_err());
    assert!(parse(&["-b3", "-e200000", "in.txt"]).is_err());
    assert!(parse(&["-b1", "-e19", "in.txt"]).is_ok());
}

/// Every other operation flag lets the last one typed win. `-b` set a separate
/// switch that nothing cleared, so `-b3 --list` benchmarked the file the caller
/// had just asked to list.
#[test]
fn a_later_mode_flag_turns_the_benchmark_off() {
    let listing = parse(&["-b3", "--list", "a.zst"]).unwrap();
    assert!(!listing.bench, "--list came last, so it wins");
    assert_eq!(listing.mode, Mode::List);

    // And the other way: `-b` after a mode flag selects benchmarking.
    let benching = parse(&["--list", "-b3", "a.zst"]).unwrap();
    assert!(benching.bench, "-b came last, so it wins");
}

#[test]
fn benchmark_flags_parse_level_range() {
    let opts = parse(&["-b3", "-e7", "in.txt"]).unwrap();
    assert!(opts.bench);
    assert_eq!(opts.bench_start, 3);
    assert_eq!(opts.bench_end, 7);
    // Bare `-b` benchmarks the default level (single-level range).
    let opts = parse(&["-b", "in.txt"]).unwrap();
    assert!(opts.bench);
    assert_eq!(opts.bench_end, opts.bench_start);
}

#[test]
fn dash_is_a_stdin_input() {
    let opts = parse(&["-d", "-"]).unwrap();
    assert_eq!(opts.inputs, vec![PathBuf::from("-")]);
}

#[test]
fn double_dash_forces_positional() {
    let opts = parse(&["--", "-weird-name.txt"]).unwrap();
    assert_eq!(opts.inputs, vec![PathBuf::from("-weird-name.txt")]);
}

#[test]
fn unknown_flag_errors() {
    assert!(parse(&["--definitely-not-a-flag"]).is_err());
    assert!(parse(&["-Z"]).is_err());
}

#[test]
fn fast_and_long_match_exactly_not_by_prefix() {
    // Exact options succeed.
    assert_eq!(parse(&["--fast"]).unwrap().level, -1);
    assert_eq!(parse(&["--fast=5"]).unwrap().level, -5);
    assert!(parse(&["-19", "--long"]).unwrap().long);
    // Typos must NOT be silently accepted as `--fast`/`--long`; they fall
    // through to the unknown-option path.
    assert!(parse(&["--faster"]).is_err());
    assert!(parse(&["--longer"]).is_err());
    // Invalid payloads are rejected, not silently reinterpreted:
    // `--fast=-5` must not flip into a positive level, and `--long=` /
    // `--long=abc` must not be accepted as a no-op.
    assert!(parse(&["--fast=-5"]).is_err());
    assert!(parse(&["-19", "--long="]).is_err());
    assert!(parse(&["-19", "--long=abc"]).is_err());
    assert!(parse(&["-19", "--long=27"]).unwrap().long);
}

/// The wire-format switches reach the frame: `--no-check` drops the
/// checksum, `--no-content-size` the Frame_Content_Size field, and the later
/// of `--no-check` / `-C` wins, as the last flag does elsewhere.
#[test]
fn wire_format_flags_reach_the_frame_header() {
    use structured_zstd::decoding::{FrameContentSize, read_frame_header_info};

    let plain = parse(&["f"]).unwrap();
    assert!(plain.checksum && plain.content_size_flag && plain.dict_id_flag);
    let stripped = parse(&["--no-check", "--no-content-size", "--no-dictID", "f"]).unwrap();
    assert!(!stripped.checksum && !stripped.content_size_flag && !stripped.dict_id_flag);
    assert!(parse(&["--no-check", "-C", "f"]).unwrap().checksum);
    assert!(parse(&["--no-check", "--check", "f"]).unwrap().checksum);
    assert!(
        parse(&["--no-content-size", "--content-size", "f"])
            .unwrap()
            .content_size_flag
    );

    let payload = b"payload whose header is inspected";
    let mut bare = Vec::new();
    compress_stream(
        &payload[..],
        &mut bare,
        &FrameSettings {
            level: 3,
            pledged_size: Some(payload.len() as u64),
            checksum: false,
            content_size_flag: false,
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .unwrap();
    let info = read_frame_header_info(&bare, false).unwrap();
    assert!(!info.content_checksum, "--no-check leaves the checksum out");
    assert_eq!(
        info.content_size,
        FrameContentSize::Unknown,
        "--no-content-size leaves the size out of the header"
    );

    let mut full = Vec::new();
    compress_stream(
        &payload[..],
        &mut full,
        &FrameSettings {
            level: 3,
            pledged_size: Some(payload.len() as u64),
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .unwrap();
    let info = read_frame_header_info(&full, false).unwrap();
    assert!(info.content_checksum, "the default frame is checksummed");
    assert_eq!(
        info.content_size,
        FrameContentSize::Known(payload.len() as u64)
    );
}

/// `--no-dictID` keeps the dictionary's ID out of a dictionary frame: the
/// decoder then has to be told which dictionary to use, and cannot check.
#[test]
fn no_dict_id_leaves_the_id_out_of_a_dictionary_frame() {
    use structured_zstd::decoding::read_frame_header_info;

    let raw = include_bytes!("../../../dict_tests/dictionary");
    let dicts = prepared_dict(raw);
    let payload: Vec<u8> = (0..4000u32).map(|i| (i % 97) as u8).collect();
    let mut with_id = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut with_id,
        &FrameSettings {
            level: 3,
            ..FrameSettings::default()
        },
        &dicts,
    )
    .unwrap();
    assert!(
        read_frame_header_info(&with_id, false)
            .unwrap()
            .dictionary_id
            .is_some(),
        "a dictionary frame names its dictionary by default"
    );
    let mut anonymous = Vec::new();
    compress_stream(
        payload.as_slice(),
        &mut anonymous,
        &FrameSettings {
            level: 3,
            dict_id_flag: false,
            ..FrameSettings::default()
        },
        &dicts,
    )
    .unwrap();
    assert!(
        read_frame_header_info(&anonymous, false)
            .unwrap()
            .dictionary_id
            .is_none(),
        "--no-dictID keeps the id out"
    );
    // Told the dictionary outright, the decoder still reads the frame.
    let mut out = Vec::new();
    decompress_stream(
        anonymous.as_slice(),
        &mut out,
        &dicts,
        &DecodeSettings::default(),
    )
    .expect("an explicit dictionary decodes an anonymous frame");
    assert_eq!(out, payload);
}

/// `-q` and `-v` move the display level one step per occurrence, from the
/// default of 2, so `-qq` reaches the level that silences errors too.
#[test]
fn quiet_and_verbose_move_the_display_level() {
    assert_eq!(parse(&["f"]).unwrap().verbosity, DEFAULT_LEVEL);
    assert_eq!(parse(&["-q", "f"]).unwrap().verbosity, 1);
    assert_eq!(parse(&["-qq", "f"]).unwrap().verbosity, 0);
    assert_eq!(parse(&["--quiet", "--quiet", "f"]).unwrap().verbosity, 0);
    assert_eq!(parse(&["-v", "f"]).unwrap().verbosity, 3);
    assert_eq!(parse(&["-vvv", "f"]).unwrap().verbosity, 5);
    assert_eq!(parse(&["--verbose", "-q", "f"]).unwrap().verbosity, 2);
}

/// A mistaken command line reports the level the flags before it had reached,
/// so `-q --bogus` gets the error alone while the default level adds usage.
#[test]
fn a_parse_failure_carries_the_display_level_reached() {
    let owned: Vec<OsString> = ["-q", "--bogus"].iter().map(OsString::from).collect();
    let failure = parse_args(&owned, &plain(), CompressionLevel::DEFAULT_LEVEL)
        .err()
        .expect("an unknown option fails");
    assert_eq!(failure.verbosity, 1);
    assert!(failure.error.to_string().contains("--bogus"));
}

/// `ZSTD_CLEVEL` replaces the default level only: a level on the command line
/// still wins, and the benchmark's default start follows it too.
#[test]
fn the_environment_level_is_the_default_the_command_line_overrides() {
    assert_eq!(parse_as(&plain(), 11, &["f"]).unwrap().level, 11);
    assert_eq!(parse_as(&plain(), 11, &["-3", "f"]).unwrap().level, 3);
    assert_eq!(
        parse_as(&plain(), 11, &["-b", "f"]).unwrap().bench_start,
        11
    );
    // An environment level above the ceiling is reduced like a typed one.
    assert_eq!(parse_as(&plain(), 22, &["f"]).unwrap().level, 19);
}

/// `ZSTD_CLEVEL` is read the way the reference command reads it: a sign,
/// digits, an optional `K`/`M`; anything else is ignored with a warning and
/// the built-in default stands.
#[test]
fn the_environment_level_is_read_like_the_reference_reads_it() {
    let read = |value: &str| level_from_env(Some(OsStr::new(value)), 0);
    assert_eq!(read("11"), 11);
    assert_eq!(read("-3"), -3);
    assert_eq!(read("+2"), 2);
    assert_eq!(read("1K"), 1024);
    assert_eq!(
        read("-999999999"),
        CompressionLevel::MIN_LEVEL,
        "a level below the scale is clamped, as the library clamps it"
    );
    assert_eq!(read("abc"), CompressionLevel::DEFAULT_LEVEL);
    assert_eq!(read(""), CompressionLevel::DEFAULT_LEVEL);
    assert_eq!(read("3x"), CompressionLevel::DEFAULT_LEVEL);
    assert_eq!(read("99999999999"), CompressionLevel::DEFAULT_LEVEL);
    assert_eq!(read("-"), CompressionLevel::DEFAULT_LEVEL);
    assert_eq!(level_from_env(None, 0), CompressionLevel::DEFAULT_LEVEL);
    // The thread count is validated the same way and never fails the run.
    check_threads_env(Some(OsStr::new("4")), 0);
    check_threads_env(Some(OsStr::new("nope")), 0);
    check_threads_env(None, 0);
}

/// Options that take a value take it attached or as the next argument, the
/// way the reference command's `NEXT_FIELD` reads them; a missing value, or
/// another option where the value should be, is a broken command line.
#[test]
fn valued_long_options_take_the_next_argument_or_an_attached_value() {
    let separated = parse(&[
        "--filelist",
        "list.txt",
        "--output-dir-flat",
        "out",
        "--maxdict",
        "4096",
        "f",
    ])
    .unwrap();
    assert_eq!(separated.filelists, vec![PathBuf::from("list.txt")]);
    assert_eq!(separated.output_dir, Some(PathBuf::from("out")));
    assert_eq!(separated.max_dict, 4096);
    assert_eq!(separated.inputs, vec![PathBuf::from("f")]);

    let attached = parse(&[
        "--filelist=a.txt",
        "--filelist=b.txt",
        "--output-dir-mirror=tree",
        "--stream-size",
        "4K",
        "f",
    ])
    .unwrap();
    assert_eq!(
        attached.filelists,
        vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]
    );
    assert_eq!(attached.output_dir_mirror, Some(PathBuf::from("tree")));
    assert_eq!(attached.pledged_size, Some(4096));

    assert!(parse(&["--filelist"]).is_err(), "a value is required");
    assert!(
        parse(&["--output-dir-flat", "-f", "f"]).is_err(),
        "an option is not a value"
    );
    assert!(
        parse(&["--output-dir-flat="]).is_err(),
        "an empty directory"
    );
    assert!(parse(&["--output-dir-mirror", "", "f"]).is_err());
    assert!(
        parse(&["--maxdict", "big", "f"]).is_err(),
        "a number is a number"
    );
}

/// The remaining file-selection and decode flags parse and land in the
/// options they steer.
#[test]
fn file_selection_flags_parse() {
    let opts = parse(&[
        "-r",
        "--exclude-compressed",
        "--pass-through",
        "--progress",
        "dir",
    ])
    .unwrap();
    assert!(opts.recursive && opts.exclude_compressed);
    assert_eq!(opts.pass_through, Some(true));
    assert_eq!(opts.progress, Progress::Always);
    let opts = parse(&["--no-pass-through", "--no-progress", "-C", "f"]).unwrap();
    assert_eq!(opts.pass_through, Some(false));
    assert_eq!(opts.progress, Progress::Never);
    assert!(opts.checksum);
    // `-f` follows links and admits a terminal on stdin, as the reference
    // command's does; without it neither happens.
    let forced = parse(&["-f", "f"]).unwrap();
    assert!(forced.force && forced.follow_links && forced.force_stdin);
    let plain_run = parse(&["f"]).unwrap();
    assert!(!plain_run.follow_links && !plain_run.force_stdin);
}

/// The help and version texts carry no typographic dash and the version
/// line names both the reference version followed and this build's own.
#[test]
fn help_and_version_texts_are_plain_ascii_punctuation() {
    assert!(!HELP_ADVANCED.contains('\u{2014}'));
    let mut usage = Vec::new();
    write_short_usage(&mut usage, "zstd").unwrap();
    let usage = String::from_utf8(usage).unwrap();
    assert!(!usage.contains('\u{2014}'));
    assert!(usage.contains("Usage: zstd [OPTIONS...] [INPUT... | -] [-o OUTPUT]"));
    assert!(usage.contains("-H, --help"));
    assert_eq!(UPSTREAM_VERSION, "1.5.7");
}

#[test]
fn decompress_suffix_stripping() {
    let opts = parse(&["-d", "archive.tar.zst"]).unwrap();
    assert_eq!(
        derive_output_path(&opts, Path::new("archive.tar.zst")).unwrap(),
        PathBuf::from("archive.tar")
    );
    // The reference command's suffix list: `.zstd` is dropped like `.zst`,
    // and a `.tzst` tarball comes back as `.tar`.
    assert_eq!(
        derive_output_path(&opts, Path::new("data.zstd")).unwrap(),
        PathBuf::from("data")
    );
    assert_eq!(
        derive_output_path(&opts, Path::new("backup.tzst")).unwrap(),
        PathBuf::from("backup.tar")
    );
    let err = derive_output_path(&opts, Path::new("noext"))
        .expect_err("no suffix, no derived name")
        .to_string();
    assert!(err.contains("unknown suffix"), "{err}");
    assert!(derive_output_path(&opts, Path::new("archive.gz")).is_err());

    // A path is bytes, not text. Rebuilding it through a lossy conversion
    // renames what it decompresses — and two different inputs can end up
    // fighting over one replacement-character name.
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let input = PathBuf::from(OsStr::from_bytes(b"\xff\xfename.zst"));
        assert_eq!(
            derive_output_path(&opts, &input).unwrap(),
            PathBuf::from(OsStr::from_bytes(b"\xff\xfename")),
            "the original bytes have to survive the suffix strip"
        );
    }
}

/// Upstream accepts a set of flags that only steer *how* the work is done —
/// thread count, memory ceiling, progress display, size hints. We are
/// single-threaded and derive our own limits, so the result is the same valid
/// zstd stream either way. Rejecting them would break the drop-in contract for
/// no benefit: a script that says `zstd -T4 -M512 file` must not fail here.
#[test]
fn performance_only_flags_are_accepted() {
    for args in [
        &["-T4", "f"][..],
        &["-T0", "f"][..],
        &["--single-thread", "f"][..],
        &["--auto-threads=logical", "f"][..],
        &["-M512", "f"][..],
        &["--memory=512MB", "f"][..],
        &["--memlimit=512MB", "f"][..],
        &["--adapt", "f"][..],
        &["--progress", "f"][..],
        &["--no-progress", "f"][..],
        &["--check", "f"][..],
        &["--no-sparse", "f"][..],
        &["--sparse", "f"][..],
        &["--no-asyncio", "f"][..],
        &["--no-mmap-dict", "f"][..],
        &["--no-row-match-finder", "f"][..],
    ] {
        let parsed = parse(args);
        assert!(
            parsed.is_ok(),
            "upstream accepts {args:?}; we must too: {:?}",
            parsed.err()
        );
    }
}

/// Sizes are accepted in the shapes upstream takes, suffixes included, and a
/// malformed one is an error rather than a silent zero.
#[test]
fn size_arguments_parse_upstream_spellings() {
    assert_eq!(
        parse(&["--stream-size=4096", "f"]).unwrap().pledged_size,
        Some(4096)
    );
    assert_eq!(
        parse(&["--stream-size=4KB", "f"]).unwrap().pledged_size,
        Some(4096)
    );
    assert_eq!(
        parse(&["--size-hint=1M", "f"]).unwrap().size_hint,
        Some(1 << 20)
    );
    assert!(parse(&["--stream-size=abc", "f"]).is_err());
    assert!(parse(&["--size-hint=", "f"]).is_err());
}

/// The other half of the contract: a flag that would change the OUTPUT, and
/// that we do not implement, must fail loudly. Silently ignoring these would
/// hand back a file the caller did not ask for — a `.gz` request answered with
/// a zstd frame, or a patch built against no reference at all.
#[test]
fn unimplemented_output_changing_flags_are_rejected() {
    for args in [
        &["--format=gzip", "f"][..],
        &["--format=xz", "f"][..],
        &["--patch-from=ref", "f"][..],
        &["--rsyncable", "f"][..],
    ] {
        assert!(
            parse(args).is_err(),
            "{args:?} changes the output and is not implemented; it must not be silently accepted"
        );
    }
}

/// `--format=zstd` is the default and names what we actually produce.
#[test]
fn explicit_zstd_format_is_accepted() {
    assert!(parse(&["--format=zstd", "f"]).is_ok());
}

/// `--stream-size` and `--size-hint` are not synonyms: the first pledges the
/// exact length, which lands in the frame header and must match, while the
/// second is an estimate used only to size the encoder. Feeding an estimate to
/// the pledge would make a wrong guess fail the compression outright, so they
/// are parsed into separate fields.
#[test]
fn stream_size_pledges_and_size_hint_only_advises() {
    let pledged = parse(&["--stream-size=4096", "f"]).unwrap();
    assert_eq!(pledged.pledged_size, Some(4096));
    assert_eq!(pledged.size_hint, None);

    let advisory = parse(&["--size-hint=8192", "f"]).unwrap();
    assert_eq!(advisory.size_hint, Some(8192));
    assert_eq!(advisory.pledged_size, None);
}

/// `-M` is a safety promise, not a hint: it caps how much memory decompressing
/// an untrusted frame may demand. This build enforces a fixed 128 MiB window
/// ceiling, so a limit at or above that is already honoured — and one BELOW it
/// is a promise we cannot keep, which must be refused rather than accepted and
/// ignored.
#[test]
fn memory_limit_is_honoured_or_refused_never_ignored() {
    // Comfortably above the window plus the decoder's auxiliary buffers.
    assert!(parse(&["-d", "-M256", "f"]).is_ok());
    // Exactly the window is NOT enough: the decoder also holds literal, block,
    // sequence and entropy-table buffers, so a promise of 128 MiB flat is one
    // we would break.
    assert!(parse(&["-d", "--memory=128MB", "f"]).is_err());
    assert!(parse(&["-d", "--memory=160MB", "f"]).is_ok());
    // Below it: refuse rather than pretend.
    let err = match parse(&["-d", "-M8", "f"]) {
        Ok(_) => panic!("a limit below our own ceiling must be refused"),
        Err(err) => err.to_string(),
    };
    assert!(
        err.contains("128"),
        "the refusal should name the ceiling we do enforce, got: {err}"
    );
    // A bare number is MiB, a suffix means what it says. Applying the implicit
    // MiB on top of an explicit suffix turns `-M1M` into a terabyte, which sails
    // past the very check the flag exists for.
    assert!(parse(&["-d", "-M1M", "f"]).is_err());
    assert!(parse(&["-d", "-M128MB", "f"]).is_err());
    assert!(parse(&["-d", "-M256MB", "f"]).is_ok());
    // Zero is the parameter's way of saying "the default ceiling", which is the
    // one this build already enforces — so it asks for nothing and is refused
    // by nothing. Read as a limit of zero bytes it would fail every run, which
    // turns an explicit request for the default into an error.
    let zero = parse(&["-d", "-M0", "f"]).expect("zero asks for the default ceiling");
    assert_eq!(
        zero.memory_limit, None,
        "and is recorded as no custom limit at all"
    );
    let zero_long = parse(&["-d", "--memory=0", "f"]).expect("the long spelling too");
    assert_eq!(zero_long.memory_limit, None);
    // The long spelling has the same default unit as the short one.
    assert!(parse(&["-d", "--memory=256", "f"]).is_ok());
    assert!(parse(&["-d", "--memory=8", "f"]).is_err());
    // `-t` decodes too, so the ceiling applies there as well.
    assert!(parse(&["-t", "--memory=8", "f"]).is_err());
}

/// The ceiling describes decompression. Compressing, listing or training
/// creates no decoder, so a limit that decoding could not keep says nothing
/// about those runs — upstream accepts it there, and refusing would fail
/// commands that never allocate what the limit is about.
#[test]
fn the_memory_limit_only_binds_the_paths_that_decode() {
    assert!(parse(&["--memory=8", "f"]).is_ok());
    assert!(parse(&["-l", "--memory=8", "f"]).is_ok());
    assert!(parse(&["--train", "-M256", "s1", "s2"]).is_ok());
    // A mode flag after the limit still decides: the check waits for it.
    assert!(parse(&["--memory=8", "-d", "f"]).is_err());
    // Benchmarking decompresses at every level it measures, so it decodes and
    // the ceiling binds there too, whatever the mode field says.
    assert!(parse(&["-b3", "-M8", "f"]).is_err());
    assert!(parse(&["-b3", "-M256", "f"]).is_ok());
}

/// `-M` promises a bound on what decompression will take, and a dictionary is
/// part of that: it is read whole and then parsed into the decoder, so a large
/// one adds well past the window the limit was checked against. Accepting a
/// limit the dictionary alone will break makes the flag a decoration.
#[test]
fn the_memory_limit_counts_the_dictionary_it_was_given() {
    // A limit that clears the decoder's own floor with room to spare.
    let generous = 300 * (1 << 20);
    check_memory_limit(generous, 0, 2).expect("no dictionary, comfortably above the floor");
    check_memory_limit(generous, 4096, 2).expect("a small dictionary still fits");
    // The same limit, against a dictionary that eats the headroom.
    let err = check_memory_limit(generous, 120 * (1 << 20), 2)
        .expect_err("a dictionary this size does not fit under the requested limit");
    assert!(
        err.to_string().contains("whole-file buffers"),
        "the refusal should say what does not fit, got: {err}"
    );
}

/// A memory limit is a promise about what the process will allocate, so a
/// dictionary that breaks it has to be refused BEFORE it is read. Loading the
/// whole file and then reporting that it does not fit performs exactly the
/// allocation the caller asked to be spared.
#[test]
fn an_oversized_dictionary_is_refused_before_it_is_read() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-bigdict-{}.bin", std::process::id()));
    // 8 KiB of dictionary is counted twice, so a limit with only 4 KiB of
    // headroom above the decoder's own floor cannot cover it.
    fs::write(&path, vec![0u8; 8 * 1024]).unwrap();

    // Unreadable, so the answer says which step ran first: refusing on the
    // size means the limit was weighed before the file was opened, while a
    // read error means the contents were reached for regardless.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    }

    let mut opts = parse(&["-d", "f"]).unwrap();
    opts.dict = Some(path.clone());
    opts.memory_limit =
        Some(structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE + (1 << 20) + 4096);
    let refused = load_dictionary(&opts);
    let _ = fs::remove_file(&path);

    let err = refused
        .expect_err("a dictionary that does not fit the limit must be refused")
        .to_string();
    assert!(
        err.contains("memory limit"),
        "the refusal must be the limit, not a read that should never have been \
         attempted: {err}"
    );
}

/// Benchmarking holds the whole input and a decompressed copy of it, which is
/// the largest thing the run allocates by far. A ceiling that weighs the
/// decoder's window and ignores those buffers is a promise kept in the small
/// and broken in the large.
#[test]
fn the_memory_limit_counts_what_a_benchmark_holds() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-benchmem-{}.bin", std::process::id()));
    fs::write(&path, vec![0u8; 64 * 1024]).unwrap();

    let mut opts = parse(&["-b3", "f"]).unwrap();
    opts.inputs = vec![path.clone()];
    // 64 KiB in and 64 KiB back out, against a limit with 32 KiB of headroom
    // above the decoder's own floor.
    opts.memory_limit =
        Some(structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE + (1 << 20) + 32 * 1024);
    let refused = run_benchmark(&opts, None);

    opts.memory_limit = Some(300 * (1 << 20));
    let accepted = run_benchmark(&opts, None);
    let _ = fs::remove_file(&path);

    let err = refused
        .expect_err("a benchmark that cannot fit its own buffers must be refused")
        .to_string();
    assert!(
        err.contains("memory limit"),
        "the refusal must be the limit: {err}"
    );
    accepted.expect("a limit that covers the buffers must let the benchmark run");
}

/// `-S` benchmarks each file on its own, which is the whole point of it: one
/// row per file instead of one row for the concatenation, so heterogeneous
/// inputs can be compared. Accepting the flag and still measuring the combined
/// stream reports a ratio and a throughput that describe neither file.
#[test]
fn separate_benchmarking_measures_one_file_at_a_time() {
    let dir = std::env::temp_dir();
    let one = dir.join(format!("szstd-sep1-{}.bin", std::process::id()));
    let two = dir.join(format!("szstd-sep2-{}.bin", std::process::id()));
    fs::write(&one, vec![b'a'; 32 * 1024]).unwrap();
    fs::write(&two, vec![b'b'; 32 * 1024]).unwrap();

    assert!(parse(&["-b3", "-S", "f"]).unwrap().bench_separately);
    assert!(!parse(&["-b3", "f"]).unwrap().bench_separately);

    // Measured one at a time, only one file is in memory at once — so a limit
    // that fits a single file is enough, while the concatenation needs both.
    // Sized the way the run sizes what it holds, for one file.
    let mut opts = parse(&["-b3", "-S", "f"]).unwrap();
    opts.inputs = vec![one.clone(), two.clone()];
    opts.memory_limit = Some(benchmark_budget(32 * 1024, 3..=3));
    let separately = run_benchmark(&opts, None);

    opts.bench_separately = false;
    let together = run_benchmark(&opts, None);
    let _ = fs::remove_file(&one);
    let _ = fs::remove_file(&two);

    separately.expect("one file at a time fits under this limit");
    together.expect_err("both files at once do not");
}

/// Benchmarking reads its inputs whole, so it needs files with an end and a
/// length that means something. A FIFO blocks on a read that never returns and
/// a character device grows the buffer until the allocator gives up; neither
/// reports a size the memory ceiling could be weighed against.
#[cfg(unix)]
#[test]
fn benchmarking_refuses_inputs_that_are_not_regular_files() {
    let dir = std::env::temp_dir();
    let fifo = dir.join(format!("szstd-benchfifo-{}", std::process::id()));
    let _ = fs::remove_file(&fifo);
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        return;
    }

    let mut opts = parse(&["-b3", "f"]).unwrap();
    opts.inputs = vec![fifo.clone()];
    let refused = run_benchmark(&opts, None);
    let _ = fs::remove_file(&fifo);

    let err = refused
        .expect_err("a FIFO is not something to benchmark")
        .to_string();
    assert!(
        err.contains("regular files"),
        "the refusal must name what is wrong with the input: {err}"
    );
}

/// Listing walks frame headers by seeking, so it needs a file that can seek.
/// Opening a FIFO instead blocks until a writer appears — the command hangs
/// where it should have said what was wrong with the argument.
#[cfg(unix)]
#[test]
fn listing_refuses_inputs_that_are_not_regular_files() {
    let dir = std::env::temp_dir();
    let fifo = dir.join(format!("szstd-listfifo-{}", std::process::id()));
    let _ = fs::remove_file(&fifo);
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        return;
    }

    let refused = list_file(&fifo, false, 0);
    let mut opts = parse(&["-l", "-qq", "f"]).unwrap();
    opts.inputs = vec![fifo.clone()];
    let failed = run(opts);
    let _ = fs::remove_file(&fifo);

    let err = refused.expect_err("a FIFO cannot be listed").to_string();
    assert!(
        err.contains("is not a file"),
        "the refusal must name what is wrong with the input: {err}"
    );
    assert_eq!(
        failed.expect("a listing reports per file and goes on"),
        1,
        "and the run counts it as failed"
    );
}

/// A size that is not an allocation — a sparse file's apparent length — can be
/// enormous, and the accounting must answer it rather than panic on the
/// arithmetic or wrap into a number that accepts a limit it cannot keep.
#[test]
fn the_memory_accounting_answers_absurd_sizes_instead_of_overflowing() {
    let err = check_memory_limit(u64::MAX, u64::MAX, 2)
        .expect_err("a size that large cannot fit under any limit");
    assert!(
        err.to_string().contains("memory limit"),
        "the refusal must be the limit, not an arithmetic accident: {err}"
    );
    // Just under the doubling boundary, which the naive multiply wraps through.
    assert!(check_memory_limit(u64::MAX, u64::MAX / 2 + 1, 2).is_err());
}

/// Benchmarking holds the input, its compressed form and the decompressed copy
/// at once. On incompressible input the compressed form is no smaller than the
/// input, so a ceiling that counts two buffers is exceeded by a third.
#[test]
fn the_memory_limit_counts_the_compressed_benchmark_buffer() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-incompressible-{}.bin", std::process::id()));
    // Counter bytes: nothing to match, so the frame is no smaller than the input.
    let payload: Vec<u8> = (0..64u32 * 1024)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    fs::write(&path, &payload).unwrap();

    let mut opts = parse(&["-b3", "f"]).unwrap();
    opts.inputs = vec![path.clone()];
    // Room for two 64 KiB buffers above the decoder's floor, not for three.
    opts.memory_limit =
        Some(structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE + (1 << 20) + 160 * 1024);
    let refused = run_benchmark(&opts, None);
    let _ = fs::remove_file(&path);

    let err = refused
        .expect_err("three buffers do not fit a limit sized for two")
        .to_string();
    assert!(
        err.contains("memory limit"),
        "the refusal must be the limit: {err}"
    );
}

/// Every other path reads `-` as stdin, and a benchmark cannot: it needs an
/// input with an end and a length to weigh. Statting it instead means the
/// marker names a file when one happens to sit in the working directory under
/// that name, and stdin everywhere else in the same command line.
#[test]
fn benchmarking_refuses_the_stdin_marker() {
    let dir = std::env::temp_dir().join(format!("szstd-dash-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let dash = dir.join("-");
    fs::write(&dash, vec![1u8; 4096]).unwrap();

    let mut opts = parse(&["-b3", "f"]).unwrap();
    // Exactly as the command line spells it, with the file there to be found.
    opts.inputs = vec![PathBuf::from("-")];
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let refused = run_benchmark(&opts, None);
    std::env::set_current_dir(previous).unwrap();

    let _ = fs::remove_file(&dash);
    let _ = fs::remove_dir(&dir);
    let err = refused
        .expect_err("the marker means stdin, which a benchmark cannot measure")
        .to_string();
    assert!(
        err.contains("stdin"),
        "the refusal must say what is wrong with it: {err}"
    );
}

/// The buffer is sized from what the directory entries claim, and the ceiling
/// was weighed against that same figure — so the read has to stop there. A file
/// that grows between the two, or one whose length underreports what it will
/// yield, would otherwise grow the buffer past the size a caller approved, on
/// the one path that exists to hold whole files.
#[test]
fn the_benchmark_read_stops_at_the_size_it_was_weighed_against() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-grew-{}", std::process::id()));
    fs::write(&input, vec![1u8; 4096]).unwrap();

    // Read the sizes, then grow the file before the bytes are taken — the same
    // window a writer would use.
    let sizes: Vec<u64> = vec![4096];
    fs::write(&input, vec![1u8; 16384]).unwrap();
    let read = read_inputs_bounded(std::slice::from_ref(&input), &sizes);
    let _ = fs::remove_file(&input);

    let err = read
        .expect_err("a file that grew past its weighed size must not be read past it")
        .to_string();
    assert!(
        err.contains("grew"),
        "the refusal must say what changed under it: {err}"
    );
}

/// Training is the third mode that takes named inputs, and `-` is stdin in all
/// of them. It reads every sample whole and rewinds nothing, so a stream is no
/// more usable here than for a benchmark or a listing.
#[test]
fn training_refuses_the_stdin_marker() {
    let dir = std::env::temp_dir().join(format!("szstd-traindash-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let dash = dir.join("-");
    fs::write(&dash, vec![3u8; 8192]).unwrap();

    let mut opts = parse(&["--train", "-f", "s"]).unwrap();
    opts.inputs = vec![PathBuf::from("-")];
    opts.output = Some(dir.join("out.dict"));
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let refused = train_dictionary(&opts);
    std::env::set_current_dir(previous).unwrap();

    let _ = fs::remove_file(&dash);
    let _ = fs::remove_file(dir.join("out.dict"));
    let _ = fs::remove_dir(&dir);
    let err = refused
        .expect_err("the marker means stdin, which training cannot read")
        .to_string();
    assert!(
        err.contains("stdin"),
        "the refusal must say what is wrong with it: {err}"
    );
}

/// The listing walk has the same blind spot benchmarking had: `-` is stdin
/// everywhere else, and listing cannot read a stream — it seeks between frame
/// headers. Statting it means the marker names a file whenever one sits in the
/// working directory under that name, and stdin whenever one does not.
#[test]
fn listing_refuses_the_stdin_marker() {
    let dir = std::env::temp_dir().join(format!("szstd-listdash-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let dash = dir.join("-");
    // A real archive, so nothing but the marker check can refuse it.
    fs::write(
        &dash,
        structured_zstd::encoding::compress_slice_to_vec(
            b"listable payload",
            structured_zstd::encoding::CompressionLevel::Default,
        ),
    )
    .unwrap();

    let mut opts = parse(&["--list", "f"]).unwrap();
    opts.inputs = vec![PathBuf::from("-")];
    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    let refused = run(opts);
    std::env::set_current_dir(previous).unwrap();

    let _ = fs::remove_file(&dash);
    let _ = fs::remove_dir(&dir);
    let err = refused
        .expect_err("the marker means stdin, which listing cannot walk")
        .to_string();
    assert!(
        err.contains("standard input"),
        "the refusal must say what is wrong with it: {err}"
    );
}

/// `--long` is not a preset: it widens the window the frame keeps and adds a
/// hash table of its own, neither of which the level's own figures describe. A
/// ceiling weighed against the preset is one such a run walks straight past.
#[test]
fn the_memory_limit_counts_what_long_distance_matching_adds() {
    let plain = structured_zstd::encoding::estimated_compression_workspace_bytes_for_run(
        structured_zstd::encoding::CompressionLevel::Level(16),
        Some(8 * 1024 * 1024),
        None,
        false,
        None,
    );
    let long = structured_zstd::encoding::estimated_compression_workspace_bytes_for_run(
        structured_zstd::encoding::CompressionLevel::Level(16),
        Some(8 * 1024 * 1024),
        Some(27),
        true,
        None,
    );
    assert!(
        long > plain,
        "the wider window and the matcher's own table are more than the preset: \
         {long} vs {plain}"
    );

    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-longmem-{}.bin", std::process::id()));
    fs::write(&input, vec![0u8; 32 * 1024]).unwrap();

    let mut opts = parse(&["--ultra", "-b16", "--long=27", "f"]).unwrap();
    opts.inputs = vec![input.clone()];
    assert!(opts.long, "--long is what the run was asked for");
    assert_eq!(opts.long_window_log, Some(27));

    // What the run would hold without `--long` — the ceiling has to want more.
    opts.memory_limit = Some(benchmark_budget(32 * 1024, 16..=16));
    let refused = run_benchmark(&opts, None);
    let _ = fs::remove_file(&input);

    refused.expect_err("the preset's figure does not cover the window --long asked for");
}

/// Every compression pass allocates a match finder, and at the higher levels
/// its tables dwarf everything else the run holds — hundreds of MiB where the
/// buffers are tens. A ceiling that counts the buffers and a fixed decoder
/// allowance but not the encoder is one the run walks straight past.
#[test]
fn the_memory_limit_counts_the_encoder_the_benchmark_builds() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-encws-{}.bin", std::process::id()));
    fs::write(&input, vec![0u8; 32 * 1024]).unwrap();

    let encoder = structured_zstd::encoding::estimated_compression_workspace_bytes_for_source(
        structured_zstd::encoding::CompressionLevel::Level(3),
        Some(32 * 1024),
    ) as u64;
    assert!(
        encoder > 0,
        "a compression pass allocates something to match with"
    );

    let mut opts = parse(&["-b3", "f"]).unwrap();
    opts.inputs = vec![input.clone()];

    // Room for everything but the encoder: it still has to fit beside them.
    opts.memory_limit = Some(benchmark_budget(32 * 1024, 3..=3) - encoder);
    let refused = run_benchmark(&opts, None);

    // Room for all of it.
    opts.memory_limit = Some(benchmark_budget(32 * 1024, 3..=3));
    let accepted = run_benchmark(&opts, None);
    let _ = fs::remove_file(&input);

    refused.expect_err("the buffers alone do not cover the encoder beside them");
    accepted.expect("with room for both it runs");
}

/// A benchmark holds the dictionary more than once: it measures both
/// directions, so the blob is parsed into an encoder's tables and a decoder's,
/// and the blob itself is still there while they are built from it. Counting it
/// once admits a run that then takes three times what it was allowed.
#[test]
fn the_memory_limit_counts_every_copy_of_the_dictionary() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-dictcopies-{}.bin", std::process::id()));
    fs::write(&input, vec![0u8; 32 * 1024]).unwrap();
    let dictionary = vec![0u8; 32 * 1024];

    let buffers = benchmark_budget(32 * 1024, 3..=3);

    let mut opts = parse(&["-b3", "f"]).unwrap();
    opts.inputs = vec![input.clone()];

    // Room for the buffers and two dictionaries: still one short.
    opts.memory_limit = Some(buffers + 2 * 32 * 1024);
    let refused = run_benchmark(&opts, Some(dictionary.clone()));

    // Room for all three: accepted.
    opts.memory_limit = Some(buffers + 3 * 32 * 1024);
    let accepted = run_benchmark(&opts, Some(dictionary));
    let _ = fs::remove_file(&input);

    refused.expect_err("two dictionaries' worth of room does not cover three");
    accepted.expect("three does");
}

/// The dictionary and the benchmark's buffers are held at the same time, so a
/// ceiling that clears each of them separately still lets the pair through.
/// Checking them apart is arithmetic that answers the wrong question.
#[test]
fn the_memory_limit_counts_the_dictionary_and_the_benchmark_together() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-bothmem-{}.bin", std::process::id()));
    fs::write(&input, vec![0u8; 32 * 1024]).unwrap();
    let dictionary = vec![0u8; 32 * 1024];

    let mut opts = parse(&["-b3", "f"]).unwrap();
    opts.inputs = vec![input.clone()];
    // Room for exactly what the benchmark holds, and so none to spare for a
    // dictionary beside it.
    opts.memory_limit = Some(benchmark_budget(32 * 1024, 3..=3));
    let refused = run_benchmark(&opts, Some(dictionary));
    let alone = run_benchmark(&opts, None);
    let _ = fs::remove_file(&input);

    alone.expect("the input alone fits under this limit");
    let err = refused
        .expect_err("the input and the dictionary together do not fit")
        .to_string();
    assert!(
        err.contains("memory limit"),
        "the refusal must be the limit: {err}"
    );
}

/// Outputs land where the directory flags say: `--output-dir-flat` beside
/// nothing but the file name, `--output-dir-mirror` under the replayed source
/// directory, and the mirror wins when both are given, as it does in the
/// reference command. A source the mirror cannot place is an error for that
/// input.
#[test]
fn output_directories_place_the_derived_name() {
    let flat = parse(&["--output-dir-flat", "out", "f"]).unwrap();
    assert_eq!(
        derive_output_path(&flat, Path::new("a/b/c.txt")).unwrap(),
        PathBuf::from("out/c.txt.zst")
    );
    let flat_decompress = parse(&["-d", "--output-dir-flat", "out", "f"]).unwrap();
    assert_eq!(
        derive_output_path(&flat_decompress, Path::new("a/b/c.txt.zst")).unwrap(),
        PathBuf::from("out/c.txt")
    );
    let mirror = parse(&["--output-dir-mirror", "tree", "f"]).unwrap();
    assert_eq!(
        derive_output_path(&mirror, Path::new("a/b/c.txt")).unwrap(),
        PathBuf::from("tree/a/b/c.txt.zst")
    );
    assert_eq!(
        derive_output_path(&mirror, Path::new("/abs/c.txt")).unwrap(),
        PathBuf::from("tree/abs/c.txt.zst")
    );
    let err = derive_output_path(&mirror, Path::new("../c.txt"))
        .expect_err("a source that climbs cannot be mirrored")
        .to_string();
    assert!(err.contains("--output-dir-mirror cannot compress"), "{err}");
    let both = parse(&[
        "--output-dir-flat",
        "out",
        "--output-dir-mirror",
        "tree",
        "f",
    ])
    .unwrap();
    assert_eq!(
        derive_output_path(&both, Path::new("a/c.txt")).unwrap(),
        PathBuf::from("tree/a/c.txt.zst"),
        "the mirror takes precedence"
    );
    // `-o` names the destination outright, whatever directory flags say.
    let named = parse(&["-o", "exact.zst", "--output-dir-flat", "out", "f"]).unwrap();
    assert_eq!(
        derive_output_path(&named, Path::new("a/c.txt")).unwrap(),
        PathBuf::from("exact.zst")
    );
}

/// `--exclude-compressed` leaves a file whose extension says it is already
/// compressed alone: no output, no failure. A directory named without `-r` is
/// refused as the reference command refuses it.
#[test]
fn already_compressed_inputs_are_skipped_and_directories_refused() {
    let scratch = Scratch::new("exclude");
    let archive = scratch.file("data.gz", b"pretend gzip");
    let plain = scratch.file("data.txt", b"plain text to compress");

    let mut opts = parse(&["--exclude-compressed", "-q", "f"]).unwrap();
    opts.inputs = vec![archive.clone(), plain.clone()];
    let skipped = process_file(&opts, &archive, &no_dict(), 2).expect("skipping is not an error");
    assert!(matches!(skipped, Outcome::Skipped));
    assert!(
        !scratch.path().join("data.gz.zst").exists(),
        "nothing is written for a skipped input"
    );
    let done = process_file(&opts, &plain, &no_dict(), 2).expect("the plain file compresses");
    assert!(matches!(done, Outcome::Done(_)));
    assert!(scratch.path().join("data.txt.zst").exists());

    let err = open_input(&opts, scratch.path())
        .expect_err("a directory is not an input without -r")
        .to_string();
    assert!(err.contains("is a directory"), "{err}");
    let err = open_input(&opts, &scratch.path().join("missing"))
        .expect_err("a missing input is reported")
        .to_string();
    assert!(err.contains("can't stat"), "{err}");
}

/// One failing input does not end the run: the rest are processed and the
/// failure count comes back to become the exit status, as the reference
/// command's does.
#[test]
fn a_failing_input_is_reported_and_the_rest_are_processed() {
    let scratch = Scratch::new("continue");
    let good = scratch.file("good.txt", b"good bytes to compress");
    let missing = scratch.path().join("missing.txt");

    let mut opts = parse(&["-qq", "f", "g"]).unwrap();
    opts.inputs = vec![missing, good.clone()];
    let failed = run(opts).expect("the run itself completes");
    assert_eq!(failed, 1, "one input failed");
    assert!(
        scratch.path().join("good.txt.zst").exists(),
        "the good input was still compressed"
    );
}

/// Without `-f` an existing output is not replaced. Below the default display
/// level no question can be asked, so the input is refused and counted as
/// failed, with the existing file untouched.
#[test]
fn an_existing_output_is_not_replaced_quietly() {
    let scratch = Scratch::new("overwrite");
    let input = scratch.file("in.txt", b"new content");
    let existing = scratch.file("in.txt.zst", b"precious bytes");

    let mut opts = parse(&["-q", "f"]).unwrap();
    opts.inputs = vec![input.clone()];
    let outcome = process_file(&opts, &input, &no_dict(), 1).expect("a refusal is not an error");
    assert!(matches!(outcome, Outcome::Refused));
    assert_eq!(fs::read(&existing).unwrap(), b"precious bytes");

    let mut forced = parse(&["-q", "-f", "f"]).unwrap();
    forced.inputs = vec![input.clone()];
    let outcome = process_file(&forced, &input, &no_dict(), 1).expect("-f replaces it");
    assert!(matches!(outcome, Outcome::Done(_)));
    assert_ne!(fs::read(&existing).unwrap(), b"precious bytes");
}

/// Several inputs into one `-o` are concatenated as frames into that file,
/// which decodes to the inputs in order. It is a destructive shape, so
/// `--rm` is set aside and the sources stay; without `-f` and with no way to
/// ask, the run refuses and writes nothing.
#[test]
fn several_inputs_into_one_output_are_concatenated_and_keep_their_sources() {
    let scratch = Scratch::new("concat");
    let a = scratch.file("a.txt", b"first part, ");
    let b = scratch.file("b.txt", b"second part");
    let output = scratch.path().join("both.zst");

    let mut refused = parse(&["-q", "--rm", "-o", "x", "a", "b"]).unwrap();
    refused.inputs = vec![a.clone(), b.clone()];
    refused.output = Some(output.clone());
    assert_eq!(
        run(refused).expect("a refusal is a failed run, not an error"),
        2,
        "every input counts as failed"
    );
    assert!(!output.exists(), "nothing is written without -f");

    let mut opts = parse(&["-q", "-f", "--rm", "-o", "x", "a", "b"]).unwrap();
    opts.inputs = vec![a.clone(), b.clone()];
    opts.output = Some(output.clone());
    assert_eq!(run(opts).expect("the concatenation runs"), 0);
    assert_eq!(
        decoded(&fs::read(&output).unwrap()).unwrap(),
        b"first part, second part"
    );
    assert!(
        a.exists() && b.exists(),
        "--rm is set aside for a concatenation"
    );
}

/// A single input into `-o` keeps the reference command's single-file path:
/// `--rm` applies, and the output takes the source's permissions.
#[test]
fn a_single_input_into_a_named_output_is_removed_on_request() {
    let scratch = Scratch::new("single");
    let input = scratch.file("only.txt", b"only bytes");
    let output = scratch.path().join("only.zst");

    let mut opts = parse(&["-q", "--rm", "-o", "x", "a"]).unwrap();
    opts.inputs = vec![input.clone()];
    opts.output = Some(output.clone());
    assert_eq!(run(opts).unwrap(), 0);
    assert_eq!(decoded(&fs::read(&output).unwrap()).unwrap(), b"only bytes");
    assert!(!input.exists(), "--rm removes the one source");
}

/// A run pointed only at empty directories has nothing to do: it says so and
/// succeeds, rather than falling back to reading stdin.
#[test]
fn empty_directories_are_nothing_to_do_not_a_request_for_stdin() {
    let scratch = Scratch::new("emptydir");
    fs::create_dir_all(scratch.path().join("empty")).unwrap();
    let mut opts = parse(&["-r", "-q", "d"]).unwrap();
    opts.inputs = vec![scratch.path().join("empty")];
    assert_eq!(run(opts).expect("nothing to do is not an error"), 0);

    // And a directory named without `-r` is one failed input.
    let mut opts = parse(&["-qq", "d"]).unwrap();
    opts.inputs = vec![scratch.path().join("empty")];
    assert_eq!(run(opts).unwrap(), 1);
}

/// Decompression reports how many bytes came out, which is what `-t` and the
/// summaries print; a corrupted checksum is ignored under `--no-check`.
#[test]
fn decoding_counts_its_output_and_no_check_ignores_the_checksum() {
    let payload = b"payload whose checksum will be corrupted";
    let mut frame = frame_of(payload);
    let written = decompress_stream(
        frame.as_slice(),
        io::sink(),
        &no_dict(),
        &DecodeSettings::default(),
    )
    .unwrap();
    assert_eq!(written, payload.len() as u64);

    let last = frame.len() - 1;
    frame[last] ^= 0xFF;
    decompress_stream(
        frame.as_slice(),
        io::sink(),
        &no_dict(),
        &DecodeSettings::default(),
    )
    .expect_err("verified by default");
    let ignored = decompress_stream(
        frame.as_slice(),
        io::sink(),
        &no_dict(),
        &DecodeSettings {
            verify_checksum: false,
            pass_through: false,
        },
    )
    .expect("--no-check decodes it regardless");
    assert_eq!(ignored, payload.len() as u64);
    assert!(
        !DecodeSettings::from_options(&parse(&["-d", "--no-check", "f"]).unwrap()).verify_checksum
    );
}

/// Input that is not a zstd stream is copied through under `--pass-through`,
/// bytes intact, and refused otherwise; a stream too short to hold a magic
/// number is treated the same way, as the reference command treats it.
#[test]
fn plain_input_is_passed_through_or_refused() {
    let pass = DecodeSettings {
        verify_checksum: true,
        pass_through: true,
    };
    let mut out = Vec::new();
    let written = decompress_stream(&b"plain text, not a frame"[..], &mut out, &no_dict(), &pass)
        .expect("pass-through copies it");
    assert_eq!(out, b"plain text, not a frame");
    assert_eq!(written, out.len() as u64);

    let mut short = Vec::new();
    decompress_stream(&b"ab"[..], &mut short, &no_dict(), &pass).unwrap();
    assert_eq!(short, b"ab", "fewer than four bytes are passed through too");

    let err = decompress_stream(
        &b"plain text, not a frame"[..],
        io::sink(),
        &no_dict(),
        &DecodeSettings::default(),
    )
    .expect_err("refused without pass-through")
    .to_string();
    assert!(err.contains("unsupported format"), "{err}");
    let err = decompress_stream(
        &b"ab"[..],
        io::sink(),
        &no_dict(),
        &DecodeSettings::default(),
    )
    .expect_err("a stump is refused too")
    .to_string();
    assert!(err.contains("unknown header"), "{err}");

    // A real frame followed by plain bytes: the frame decodes, and the tail
    // is passed through after it, as the reference command's loop does.
    let mut mixed = frame_of(b"framed");
    mixed.extend_from_slice(b" then plain");
    let mut out = Vec::new();
    decompress_stream(mixed.as_slice(), &mut out, &no_dict(), &pass).unwrap();
    assert_eq!(out, b"framed then plain");

    // The default follows the reference command: on when forced and writing
    // to stdout (`zstd -dcf`), off otherwise.
    assert!(DecodeSettings::from_options(&parse(&["-dcf", "f"]).unwrap()).pass_through);
    assert!(!DecodeSettings::from_options(&parse(&["-dc", "f"]).unwrap()).pass_through);
    assert!(!DecodeSettings::from_options(&parse(&["-df", "f"]).unwrap()).pass_through);
    assert!(
        DecodeSettings::from_options(&parse(&["-df", "--pass-through", "f"]).unwrap()).pass_through
    );
}

/// Data to stdout silences the result summary and sets `--rm` aside; the
/// verbosity and the removal flag are what the run computes from the inputs
/// and destination together.
#[test]
fn stdout_output_is_recognised_from_the_inputs_and_destination() {
    assert!(writes_stdout(&parse(&["-c", "f"]).unwrap()));
    assert!(
        writes_stdout(&parse(&[]).unwrap()),
        "stdin in, nothing named: stdout out"
    );
    assert!(writes_stdout(&parse(&["-"]).unwrap()));
    assert!(!writes_stdout(&parse(&["-o", "out"]).unwrap()));
    assert!(!writes_stdout(&parse(&["f"]).unwrap()));
    assert!(
        !writes_stdout(&parse(&["f", "-"]).unwrap()),
        "a file among them gets its own"
    );
    assert!(reads_stdin(&[]));
    assert!(reads_stdin(&[PathBuf::from("a"), PathBuf::from("-")]));
    assert!(!reads_stdin(&[PathBuf::from("a")]));
}

/// `--adapt` also comes parameterised upstream (`--adapt=min=1,max=9`).
/// Accepting the bare form but choking on the documented one would fail a
/// script for a reason that has nothing to do with what we support.
#[test]
fn parameterised_adapt_is_accepted() {
    assert!(parse(&["--adapt", "f"]).is_ok());
    assert!(parse(&["--adapt=min=1,max=9", "f"]).is_ok());
}

/// The help text promises that `-f` is what allows output to a terminal, and
/// upstream refuses without it. Writing a compressed frame into an interactive
/// terminal corrupts the session and loses the data, so the guard has to exist
/// rather than just be advertised.
#[test]
fn binary_output_to_a_terminal_needs_force() {
    // Not a terminal: always fine, `-f` or not.
    assert!(guard_binary_stdout(false, false).is_ok());
    assert!(guard_binary_stdout(false, true).is_ok());
    // A terminal: refused, unless forced.
    assert!(guard_binary_stdout(true, false).is_err());
    assert!(guard_binary_stdout(true, true).is_ok());
}

/// Ignoring what a flag *does* is not the same as ignoring what it *says*: a
/// typo in an attached value is still a broken command line, and swallowing it
/// hides the mistake instead of reporting it.
#[test]
fn attached_short_option_values_are_validated() {
    // Well-formed values are accepted and have no effect.
    assert!(parse(&["-T4", "f"]).is_ok());
    assert!(parse(&["-B128", "f"]).is_ok());
    // Malformed ones are errors, not silence.
    assert!(parse(&["-Tinvalid", "f"]).is_err());
    assert!(parse(&["-Binvalid", "f"]).is_err());
    assert!(parse(&["-Minvalid", "f"]).is_err());
    // A thread count is a count. Sizes suffixes belong to `-B`, which is a
    // size, and `--threads=` already refuses them — the short spelling has to
    // agree with the long one.
    assert!(parse(&["-B4K", "f"]).is_ok());
    assert!(parse(&["-T4K", "f"]).is_err());
    assert!(parse(&["--threads=4K", "f"]).is_err());
}

/// Ignoring what `--adapt` does is not a licence to ignore what it says. The
/// documented form is `min=`/`max=` numbers; anything else is a broken command
/// line and has to be reported, not swallowed.
#[test]
fn adapt_parameters_are_validated() {
    assert!(parse(&["--adapt", "f"]).is_ok());
    assert!(parse(&["--adapt=min=1", "f"]).is_ok());
    assert!(parse(&["--adapt=min=1,max=9", "f"]).is_ok());
    assert!(parse(&["--adapt=max=9,min=1", "f"]).is_ok());

    assert!(parse(&["--adapt=", "f"]).is_err(), "empty parameter list");
    assert!(parse(&["--adapt=garbage", "f"]).is_err(), "not a key=value");
    assert!(parse(&["--adapt=nim=1", "f"]).is_err(), "misspelled key");
    assert!(parse(&["--adapt=min=x", "f"]).is_err(), "non-numeric value");
    assert!(
        parse(&["--adapt=min=1,", "f"]).is_err(),
        "trailing separator"
    );
}

/// `-t` exists to answer one question: is this frame intact? Answering "OK"
/// for a frame whose stored checksum disagrees with the data is the one
/// failure this mode cannot have — the caller would keep a corrupt archive on
/// the strength of it. The decoder computes the digest by default but does not
/// compare it, so verification has to be asked for explicitly.
#[test]
fn corrupted_checksum_is_reported_not_passed() {
    // Built through the tool's own compression path, which turns the content
    // checksum on the way the reference command does — the library default
    // omits it, and a frame without one has nothing to verify.
    let payload = b"payload whose checksum will be corrupted";
    let mut frame = Vec::new();
    compress_stream(
        &payload[..],
        &mut frame,
        &FrameSettings {
            level: 3,
            ..FrameSettings::default()
        },
        &no_dict(),
    )
    .expect("compressing the fixture must succeed");
    // The trailing four bytes are the frame's XXH64 check field.
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;

    let err = decompress_stream(
        frame.as_slice(),
        io::sink(),
        &no_dict(),
        &DecodeSettings::default(),
    )
    .expect_err("a corrupted checksum must fail the decode");
    let text = err.to_string();
    assert!(
        text.to_ascii_lowercase().contains("checksum"),
        "the failure should name the checksum, got: {text}"
    );
}

/// `--rm` deletes the input once the output is safely written. With `-c` the
/// output went to stdout, which may be a pipe that was closed, a terminal, or
/// anything else we cannot re-read — there is no saved copy to justify the
/// deletion. Upstream keeps the file in that case; deleting it would be data
/// loss on the user's behalf.
#[test]
fn rm_keeps_the_source_when_output_went_to_stdout() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("szstd-rm-test-{}.txt", std::process::id()));
    fs::write(&path, b"payload").unwrap();

    let mut opts = parse(&["--rm", "-c", "f"]).unwrap();
    opts.inputs = vec![path.clone()];
    let result = remove_source_if_requested(&opts, &path);

    let survived = path.exists();
    let _ = fs::remove_file(&path);
    result.expect("the no-op path must not error");
    assert!(
        survived,
        "--rm with -c must keep the input: the output went somewhere we cannot verify"
    );
}

/// `-c` and `-o` name competing destinations, so the later one on the command
/// line wins — verified against upstream, where `-c -o f` writes the file and
/// `-o f -c` writes stdout. Letting `-c` win regardless would silently drop
/// the `-o` the caller typed last.
#[test]
fn stdout_and_output_follow_last_option_wins() {
    let file_last = parse(&["-c", "-o", "out.zst", "in.txt"]).unwrap();
    assert_eq!(file_last.output, Some(PathBuf::from("out.zst")));
    assert!(!file_last.to_stdout, "-o came last, so it selects the file");

    let stdout_last = parse(&["-o", "out.zst", "-c", "in.txt"]).unwrap();
    assert!(stdout_last.to_stdout, "-c came last, so it selects stdout");
    assert_eq!(stdout_last.output, None);
}

/// `--[no-]compress-literals` forces literals compressed or stored, which
/// changes the emitted frame. The encoder has no such switch here, so
/// accepting the flag would hand back a frame laid out the other way.
#[test]
fn literal_mode_flags_are_rejected_until_wired() {
    assert!(parse(&["--compress-literals", "f"]).is_err());
    assert!(parse(&["--no-compress-literals", "f"]).is_err());
}

/// Concatenating frames is a documented property of the format: `cat a.zst
/// b.zst` decodes to `a` followed by `b`, which is how `tar` archives and
/// append-style logs are built. Stopping at the first frame loses the rest
/// silently, and makes `-t` answer for a prefix of what it was handed.
#[test]
fn concatenated_frames_are_all_decoded() {
    let mut stream = Vec::new();
    for payload in [&b"first frame payload"[..], &b"second frame payload"[..]] {
        compress_stream(
            payload,
            &mut stream,
            &FrameSettings {
                level: 3,
                ..FrameSettings::default()
            },
            &no_dict(),
        )
        .expect("compressing a fixture frame must succeed");
    }

    let out = decoded(&stream).expect("both frames must decode");
    assert_eq!(
        out, b"first frame payloadsecond frame payload",
        "every frame in the stream has to reach the output"
    );
}

/// Skippable frames carry caller metadata inside an otherwise ordinary stream;
/// the format says a decoder steps over them. Failing on one would reject
/// archives the reference tool reads without complaint.
#[test]
fn skippable_frames_are_stepped_over() {
    let mut stream = Vec::new();
    let level_only = FrameSettings {
        level: 3,
        ..FrameSettings::default()
    };
    compress_stream(&b"payload"[..], &mut stream, &level_only, &no_dict())
        .expect("compressing the fixture must succeed");
    // Magic 0x184D2A50 (little-endian) + a 4-byte length + that many bytes.
    stream.extend_from_slice(&0x184D_2A50_u32.to_le_bytes());
    stream.extend_from_slice(&4_u32.to_le_bytes());
    stream.extend_from_slice(b"meta");
    // A frame after it, so the skip has to land on the right byte rather than
    // merely being tolerated at the end of the stream.
    compress_stream(&b" and more"[..], &mut stream, &level_only, &no_dict())
        .expect("compressing the trailing fixture must succeed");

    let out = decoded(&stream).expect("a skippable frame must not fail the decode");
    assert_eq!(
        out, b"payload and more",
        "skippable content is stepped over, not emitted"
    );
}

/// A dictionary does not merely sit beside the match finder — it decides how
/// big the match finder is. Above the size at which the dictionary stops being
/// searched in place, the frame runs the dictionary's own table geometry, so a
/// small file compressed against a large dictionary allocates tables sized for
/// the dictionary: at level 5, 64 KiB of input against a 256 KiB dictionary
/// asks for 3.4 MiB where the file alone asks for 1.2 MiB, and the gap reaches
/// hundreds of MiB at the top levels. A ceiling weighed on the file alone is
/// one such a run walks straight past.
#[test]
fn the_memory_limit_counts_the_encoder_the_dictionary_asks_for() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("szstd-dictws-{}.bin", std::process::id()));
    fs::write(&input, vec![0u8; 64 * 1024]).unwrap();
    let dictionary = vec![7u8; 256 * 1024];

    let plain = structured_zstd::encoding::estimated_compression_workspace_bytes_for_source(
        structured_zstd::encoding::CompressionLevel::Level(5),
        Some(64 * 1024),
    ) as u64;
    let with_dict = structured_zstd::encoding::estimated_compression_workspace_bytes_for_run(
        structured_zstd::encoding::CompressionLevel::Level(5),
        Some(64 * 1024),
        None,
        false,
        Some(structured_zstd::encoding::DictionarySizes::raw_content(
            dictionary.len(),
        )),
    ) as u64;
    assert!(
        with_dict > plain,
        "tables built to a 256 KiB dictionary are larger than tables built to a \
         64 KiB file: {with_dict} vs {plain}"
    );

    let mut opts = parse(&["-b5", "f"]).unwrap();
    opts.inputs = vec![input.clone()];

    // Everything the run holds, with the encoder weighed on the file alone:
    // the buffers, three copies of the dictionary, and that too-small figure.
    let weighed_without_the_dictionary =
        benchmark_budget(64 * 1024, 5..=5) + 3 * dictionary.len() as u64;
    opts.memory_limit = Some(weighed_without_the_dictionary);
    let refused = run_benchmark(&opts, Some(dictionary.clone()));

    // The same, with the encoder weighed on what the dictionary asks for.
    opts.memory_limit = Some(weighed_without_the_dictionary + (with_dict - plain));
    let accepted = run_benchmark(&opts, Some(dictionary));
    let _ = fs::remove_file(&input);

    refused
        .expect_err("a ceiling weighed on the file alone does not cover the dictionary's tables");
    accepted.expect("weighed on the dictionary's own parameters, the run fits");
}
