use super::*;

fn parse(args: &[&str]) -> Result<Options> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    match parse_args(&owned, Mode::Compress, false)? {
        Parsed::Run(opts) => Ok(opts),
        Parsed::Handled => bail!("parse handled (help/version) unexpectedly"),
    }
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
    let result = list_file(&path);
    let _ = fs::remove_file(&path);
    result.expect("list_file must walk both frames without error");
}

#[test]
fn argv0_unzstd_defaults_to_decompress() {
    assert_eq!(program_mode("unzstd"), (Mode::Decompress, false));
    assert_eq!(program_mode("/usr/bin/unzstd"), (Mode::Decompress, false));
}

#[test]
fn argv0_zstdcat_decompresses_to_stdout() {
    assert_eq!(program_mode("zstdcat"), (Mode::Decompress, true));
    assert_eq!(program_mode("zstd"), (Mode::Compress, false));
}

#[test]
fn bare_numeric_flag_is_a_level() {
    let opts = parse(&["-19", "in.txt"]).unwrap();
    assert_eq!(opts.level, 19);
    assert_eq!(opts.mode, Mode::Compress);
    assert_eq!(opts.inputs, vec!["in.txt".to_string()]);
}

#[test]
fn levels_above_19_require_ultra() {
    assert!(parse(&["-22", "in.txt"]).is_err());
    let opts = parse(&["--ultra", "-22", "in.txt"]).unwrap();
    assert_eq!(opts.level, 22);
}

#[test]
fn fast_flag_maps_to_negative_level() {
    assert_eq!(parse(&["--fast"]).unwrap().level, -1);
    assert_eq!(parse(&["--fast=5"]).unwrap().level, -5);
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

#[test]
fn output_rejects_multiple_inputs() {
    assert!(parse(&["-o", "out.zst", "a.txt", "b.txt"]).is_err());
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

#[test]
fn list_mode_parses() {
    assert_eq!(parse(&["-l", "a.zst"]).unwrap().mode, Mode::List);
    assert_eq!(parse(&["--list", "a.zst"]).unwrap().mode, Mode::List);
}

#[test]
fn long_flag_enables_ldm() {
    assert!(parse(&["-19", "--long", "in.txt"]).unwrap().long);
    assert!(parse(&["--long=27", "in.txt"]).unwrap().long);
    assert!(!parse(&["-19", "in.txt"]).unwrap().long);
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
    assert_eq!(opts.inputs, vec!["-".to_string()]);
}

#[test]
fn double_dash_forces_positional() {
    let opts = parse(&["--", "-weird-name.txt"]).unwrap();
    assert_eq!(opts.inputs, vec!["-weird-name.txt".to_string()]);
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
    assert!(parse(&["--long"]).unwrap().long);
    // Typos must NOT be silently accepted as `--fast`/`--long`; they fall
    // through to the unknown-option path.
    assert!(parse(&["--faster"]).is_err());
    assert!(parse(&["--longer"]).is_err());
    // Invalid payloads are rejected, not silently reinterpreted:
    // `--fast=-5` must not flip into a positive level, and `--long=` /
    // `--long=abc` must not be accepted as a no-op.
    assert!(parse(&["--fast=-5"]).is_err());
    assert!(parse(&["--long="]).is_err());
    assert!(parse(&["--long=abc"]).is_err());
    assert!(parse(&["--long=27"]).unwrap().long);
}

#[test]
fn unsupported_format_flags_are_rejected_not_ignored() {
    // These change the wire format but are not wired through yet — accepting
    // them silently would hand the caller the wrong frame layout.
    assert!(parse(&["--no-check"]).is_err());
    assert!(parse(&["--no-content-size"]).is_err());
    assert!(parse(&["--no-dictID"]).is_err());
    // Verbosity aliases stay honest no-ops.
    assert!(parse(&["--quiet"]).is_ok());
    assert!(parse(&["--verbose"]).is_ok());
}

#[test]
fn decompress_suffix_stripping() {
    let opts = Options {
        mode: Mode::Decompress,
        level: 3,
        store: false,
        dict: None,
        to_stdout: false,
        output: None,
        force: false,
        keep: false,
        remove_source: false,
        inputs: vec!["archive.tar.zst".to_string()],
        max_dict: DEFAULT_MAX_DICT,
        dict_id: None,
        bench: false,
        bench_start: 3,
        bench_end: 0,
        bench_secs: 1.0,
        long: false,
        pledged_size: None,
        size_hint: None,
    };
    assert_eq!(
        derive_output_path(&opts, Path::new("archive.tar.zst")).unwrap(),
        PathBuf::from("archive.tar")
    );
    assert!(derive_output_path(&opts, Path::new("noext")).is_err());
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
        &["--compress-literals", "f"][..],
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
}

/// Flags whose whole purpose is to change which files are touched, or what
/// happens to input that is not compressed, cannot be accepted as no-ops: the
/// caller would get compression where they asked for a skip, or an error where
/// they asked for a copy.
#[test]
fn unimplemented_behaviour_flags_are_rejected() {
    for args in [
        &["--exclude-compressed", "f"][..],
        &["-d", "--pass-through", "f"][..],
    ] {
        assert!(
            parse(args).is_err(),
            "{args:?} changes which files are processed and is not implemented"
        );
    }
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
    compress_stream(&payload[..], &mut frame, 3, false, None, None, None, false)
        .expect("compressing the fixture must succeed");
    // The trailing four bytes are the frame's XXH64 check field.
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;

    let err = decompress_stream(frame.as_slice(), io::sink(), None)
        .expect_err("a corrupted checksum must fail the decode");
    let text = err.to_string();
    assert!(
        text.to_ascii_lowercase().contains("checksum"),
        "the failure should name the checksum, got: {text}"
    );
}
