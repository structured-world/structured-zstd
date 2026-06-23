
use super::*;

fn parse(args: &[&str]) -> color_eyre::Result<Options> {
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
    };
    assert_eq!(
        derive_output_path(&opts, Path::new("archive.tar.zst")).unwrap(),
        PathBuf::from("archive.tar")
    );
    assert!(derive_output_path(&opts, Path::new("noext")).is_err());
}
