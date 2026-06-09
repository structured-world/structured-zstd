//! `zstd` command-line interface with upstream-compatible flag dispatch.
//!
//! The argument model mirrors upstream zstd v1.5.7: mode + level FLAGS (not
//! subcommands), `argv[0]` dispatch (`unzstd` / `zstdcat` change the default
//! mode), stdin/stdout streaming, and the conventional `-o`/`-f`/`-k`/`-D`
//! file flags. Compression/decompression run through the streaming codec, so
//! peak memory stays O(window), not O(file).

mod progress;
use progress::{ProgressMonitor, fmt_size};

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use color_eyre::eyre::{WrapErr, bail, eyre};
use structured_zstd::encoding::CompressionLevel;
use tracing::info;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const ZSTD_SUFFIX: &str = ".zst";

/// Operation selected by mode flags / `argv[0]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Compress,
    Decompress,
    Test,
    List,
}

/// Parsed command line.
struct Options {
    mode: Mode,
    /// Numeric compression level (zstd scale). `store` overrides it.
    level: i32,
    store: bool,
    /// Raw dictionary blob path (`-D`), applied to both compress and decompress.
    dict: Option<PathBuf>,
    /// Force output to stdout (`-c` / `zstdcat` / a `-` input).
    to_stdout: bool,
    /// Explicit output path (`-o`); at most one input is allowed with it.
    output: Option<PathBuf>,
    force: bool,
    keep: bool,
    /// Remove source files after a successful (de)compression (`--rm`; implied
    /// by upstream's default when not `-k`/stdout, but we keep the input unless
    /// `--rm` is given — safer default for a young tool).
    remove_source: bool,
    /// Positional inputs; empty or a lone `-` means stdin.
    inputs: Vec<String>,
}

/// Outcome of argument parsing: either run with `Options`, or a terminal
/// message already handled (help / version).
enum Parsed {
    Run(Options),
    Handled,
}

fn main() -> color_eyre::Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    let prog = raw.first().map(String::as_str).unwrap_or("zstd");
    let (default_mode, argv0_stdout) = program_mode(prog);

    let parsed = parse_args(&raw[1..], default_mode, argv0_stdout)?;
    let options = match parsed {
        Parsed::Run(options) => options,
        Parsed::Handled => return Ok(()),
    };

    // Logging (with indicatif progress integration) goes to stderr so it never
    // contaminates a `-c` stdout data stream.
    let indicatif_layer = IndicatifLayer::new();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(indicatif_layer.get_stderr_writer())
                .without_time(),
        )
        .with(indicatif_layer)
        .init();

    run(options)
}

/// Default mode + forced-stdout from `argv[0]` (the conventional symlink
/// dispatch). `unzstd` decompresses; `zstdcat` decompresses to stdout.
fn program_mode(prog: &str) -> (Mode, bool) {
    let name = Path::new(prog)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(prog);
    // Strip a trailing `.exe` for Windows symlink names.
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    match stem {
        "unzstd" => (Mode::Decompress, false),
        "zstdcat" | "zcat" => (Mode::Decompress, true),
        // "zstd", "zstdmt", anything else → compress by default.
        _ => (Mode::Compress, false),
    }
}

/// Manual upstream-style parse: bare `-N` is a level, short flags combine
/// (`-dc`), `-o`/`-D` take a value, `--long-opts` are matched whole. `clap`'s
/// derive cannot model bare numeric levels, so we parse argv directly.
fn parse_args(
    args: &[String],
    default_mode: Mode,
    argv0_stdout: bool,
) -> color_eyre::Result<Parsed> {
    let mut opts = Options {
        mode: default_mode,
        level: CompressionLevel::DEFAULT_LEVEL,
        store: false,
        dict: None,
        to_stdout: argv0_stdout,
        output: None,
        force: false,
        keep: false,
        remove_source: false,
        inputs: Vec::new(),
    };
    let mut ultra = false;
    let mut iter = args.iter().enumerate().peekable();
    let mut positional_only = false;

    while let Some((idx, arg)) = iter.next() {
        if positional_only || arg == "-" || !arg.starts_with('-') {
            opts.inputs.push(arg.clone());
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "compress" => opts.mode = Mode::Compress,
                "decompress" | "uncompress" => opts.mode = Mode::Decompress,
                "test" => opts.mode = Mode::Test,
                "list" => opts.mode = Mode::List,
                "stdout" | "to-stdout" => opts.to_stdout = true,
                "force" => opts.force = true,
                "keep" => opts.keep = true,
                "rm" => opts.remove_source = true,
                "ultra" => ultra = true,
                "no-check" | "no-content-size" | "no-dictID" | "quiet" | "verbose" => {}
                "version" => {
                    print_version();
                    return Ok(Parsed::Handled);
                }
                "help" => {
                    print_help();
                    return Ok(Parsed::Handled);
                }
                _ => {
                    if let Some(n) = long.strip_prefix("fast") {
                        // `--fast` or `--fast=N`.
                        opts.level = match n.strip_prefix('=') {
                            Some(v) => -(v.parse::<i32>().wrap_err("invalid --fast level")?),
                            None => -1,
                        };
                    } else if let Some(path) = long.strip_prefix("use-dict=") {
                        opts.dict = Some(PathBuf::from(path));
                    } else if long.starts_with("long") {
                        // `--long[=N]` (LDM) — accepted; LDM wiring is a follow-up.
                    } else {
                        bail!("unknown option: --{long}");
                    }
                }
            }
            continue;
        }
        // Short flag cluster, e.g. `-dcf`, `-19`, `-D dict`, `-o out`.
        let chars: Vec<char> = arg[1..].chars().collect();
        let mut ci = 0;
        while ci < chars.len() {
            let c = chars[ci];
            match c {
                'd' => opts.mode = Mode::Decompress,
                'z' => opts.mode = Mode::Compress,
                't' => opts.mode = Mode::Test,
                'l' => opts.mode = Mode::List,
                'c' => opts.to_stdout = true,
                'f' => opts.force = true,
                'k' => opts.keep = true,
                'q' | 'v' => {}
                'V' => {
                    print_version();
                    return Ok(Parsed::Handled);
                }
                'h' | 'H' => {
                    print_help();
                    return Ok(Parsed::Handled);
                }
                'D' | 'o' => {
                    // Value is the rest of this token, or the next argument.
                    let rest: String = chars[ci + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        iter.next()
                            .map(|(_, v)| v.clone())
                            .ok_or_else(|| eyre!("option -{c} requires a value"))?
                    } else {
                        rest
                    };
                    if c == 'D' {
                        opts.dict = Some(PathBuf::from(value));
                    } else {
                        opts.output = Some(PathBuf::from(value));
                    }
                    ci = chars.len();
                    continue;
                }
                '0'..='9' => {
                    // The rest of the cluster is the (possibly multi-digit) level.
                    let digits: String = chars[ci..].iter().collect();
                    opts.level = digits.parse::<i32>().wrap_err("invalid level")?;
                    ci = chars.len();
                    continue;
                }
                _ => bail!("unknown flag: -{c} (in {arg})"),
            }
            ci += 1;
        }
        let _ = idx;
    }

    if !ultra && opts.level > 19 {
        bail!("level {} requires --ultra (levels 20-22)", opts.level);
    }
    validate_level(opts.level)?;
    if opts.output.is_some() && opts.inputs.len() > 1 {
        bail!("-o cannot be combined with multiple input files");
    }
    Ok(Parsed::Run(opts))
}

fn validate_level(level: i32) -> color_eyre::Result<()> {
    let (min, max) = (CompressionLevel::MIN_LEVEL, CompressionLevel::MAX_LEVEL);
    if !(min..=max).contains(&level) {
        bail!("compression level {level} out of range [{min}, {max}]");
    }
    Ok(())
}

fn print_version() {
    println!(
        "zstd (structured-zstd) {} — pure-Rust Zstandard",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_help() {
    print_version();
    println!(
        "\nUsage: zstd [OPTIONS] [FILE...]\n\
         \n\
         Modes:\n\
         \x20 -z, --compress       compress (default)\n\
         \x20 -d, --decompress     decompress\n\
         \x20 -t, --test           test a compressed file's integrity\n\
         \n\
         Options:\n\
         \x20 -<N>                 compression level (1-19; 20-22 need --ultra)\n\
         \x20 --fast[=N]           ultra-fast negative level\n\
         \x20 --ultra              allow levels 20-22\n\
         \x20 -D FILE              use FILE as a dictionary\n\
         \x20 -o FILE              write output to FILE\n\
         \x20 -c, --stdout         write to stdout\n\
         \x20 -f, --force          overwrite output / allow stdout to terminal\n\
         \x20 -k, --keep           keep (do not delete) source files\n\
         \x20 --rm                 remove source files after success\n\
         \x20 -V, --version        print version\n\
         \x20 -h, --help           print this help\n\
         \n\
         With no FILE, or when FILE is `-`, read stdin / write stdout."
    );
}

fn run(opts: Options) -> color_eyre::Result<()> {
    let dict_bytes = match &opts.dict {
        Some(path) => Some(
            fs::read(path)
                .wrap_err_with(|| format!("failed to read dictionary file {}", path.display()))?,
        ),
        None => None,
    };
    let dict = dict_bytes.as_deref();

    // `--list` walks frame headers without decoding; it needs a seekable file
    // (not a stream), so it is handled separately from the (de)compress flow.
    if opts.mode == Mode::List {
        if opts.inputs.is_empty() || opts.inputs.iter().any(|i| i == "-") {
            bail!("--list requires regular files (cannot list stdin)");
        }
        print_list_header();
        for input in &opts.inputs {
            list_file(Path::new(input))?;
        }
        return Ok(());
    }

    if opts.inputs.is_empty() {
        return process_stdin_stdout(&opts, dict);
    }
    for input in &opts.inputs {
        if input == "-" {
            process_stdin_stdout(&opts, dict)?;
        } else {
            process_file(&opts, Path::new(input), dict)?;
        }
    }
    Ok(())
}

/// Column header for `--list`, matching upstream's `zstd -l` layout.
fn print_list_header() {
    println!("Frames  Compressed  Uncompressed  Ratio  Check  DictID  Filename");
}

/// Print one `--list` row: walk every frame header in the file (no body
/// decode), summing compressed + declared content sizes. Decompressed size is
/// `--` when any frame omits the Frame_Content_Size field.
fn list_file(path: &Path) -> color_eyre::Result<()> {
    use structured_zstd::decoding::{
        FrameContentSize, find_frame_compressed_size, read_frame_header_info,
    };

    let bytes = fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let compressed = bytes.len() as u64;
    let mut offset = 0usize;
    let mut frames = 0u64;
    let mut decompressed = Some(0u64);
    let mut check = false;
    let mut dict_id = None;

    while offset < bytes.len() {
        let rest = &bytes[offset..];
        let info = read_frame_header_info(rest, false)
            .map_err(|err| eyre!("{}: not a zstd frame: {err:?}", path.display()))?;
        let frame_len = find_frame_compressed_size(rest)
            .map_err(|err| eyre!("{}: malformed frame: {err:?}", path.display()))?;
        match info.content_size {
            FrameContentSize::Known(n) => decompressed = decompressed.map(|d| d + n),
            FrameContentSize::Unknown => decompressed = None,
        }
        check |= info.content_checksum;
        if frames == 0 {
            dict_id = info.dictionary_id;
        }
        frames += 1;
        offset += frame_len;
    }

    let ratio = match decompressed {
        Some(d) if d > 0 => format!("{:.3}", d as f64 / compressed as f64),
        _ => "--".to_string(),
    };
    let decompressed_str = match decompressed {
        Some(d) => fmt_size(d as f64),
        None => "--".to_string(),
    };
    println!(
        "{frames:>6}  {:>10}  {:>12}  {ratio:>5}  {:>5}  {:>6}  {}",
        fmt_size(compressed as f64),
        decompressed_str,
        if check { "XXH64" } else { "None" },
        dict_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "0".to_string()),
        path.display(),
    );
    Ok(())
}

/// stdin → stdout for a `-` input or no inputs.
fn process_stdin_stdout(opts: &Options, dict: Option<&[u8]>) -> color_eyre::Result<()> {
    let stdin = io::stdin();
    let reader = stdin.lock();
    match opts.mode {
        Mode::Compress => {
            let stdout = io::stdout();
            compress_stream(reader, stdout.lock(), opts.level, opts.store, dict, None)
        }
        Mode::Decompress => {
            let stdout = io::stdout();
            decompress_stream(reader, stdout.lock(), dict)
        }
        Mode::Test => decompress_stream(reader, io::sink(), dict).map(|_| {
            info!("stdin: OK");
        }),
        Mode::List => unreachable!("--list is handled in run() before streaming"),
    }
}

/// Resolve the output path for a file input under the current mode.
fn derive_output_path(opts: &Options, input: &Path) -> color_eyre::Result<PathBuf> {
    if let Some(out) = &opts.output {
        return Ok(out.clone());
    }
    match opts.mode {
        Mode::Compress => Ok(add_extension(input, ZSTD_SUFFIX)),
        Mode::Decompress => {
            let name = input.to_string_lossy();
            match name.strip_suffix(ZSTD_SUFFIX) {
                Some(stripped) => Ok(PathBuf::from(stripped)),
                None => bail!(
                    "{}: unknown suffix (expected {ZSTD_SUFFIX}); use -o to set the output",
                    input.display()
                ),
            }
        }
        Mode::Test | Mode::List => {
            unreachable!("test / list modes never write an output file")
        }
    }
}

fn process_file(opts: &Options, input: &Path, dict: Option<&[u8]>) -> color_eyre::Result<()> {
    let source = File::open(input)
        .wrap_err_with(|| format!("failed to open input file {}", input.display()))?;
    let source_size: usize = source
        .metadata()?
        .len()
        .try_into()
        .wrap_err("input file too large for this platform")?;
    let mut reader = ProgressMonitor::new(BufReader::new(source), source_size);

    // Test mode: decompress into the void, report integrity.
    if opts.mode == Mode::Test {
        decompress_stream(&mut reader, io::sink(), dict)?;
        info!("{}: OK", input.display());
        return Ok(());
    }

    // stdout sink: bypass the temp-file dance.
    if opts.to_stdout {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        match opts.mode {
            Mode::Compress => compress_stream(
                &mut reader,
                &mut out,
                opts.level,
                opts.store,
                dict,
                Some(source_size as u64),
            )?,
            Mode::Decompress => decompress_stream(&mut reader, &mut out, dict)?,
            Mode::Test | Mode::List => unreachable!(),
        }
        return Ok(());
    }

    let output = derive_output_path(opts, input)?;
    ensure_distinct_paths(input, &output)?;
    ensure_regular_output_destination(&output)?;
    if output.exists() && !opts.force {
        bail!("{} already exists; use -f to overwrite", output.display());
    }
    let (temp_path, temp_file) = create_temporary_output_file(&output)?;
    let result: color_eyre::Result<()> = (|| {
        let mut sink = temp_file;
        match opts.mode {
            Mode::Compress => compress_stream(
                &mut reader,
                &mut sink,
                opts.level,
                opts.store,
                dict,
                Some(source_size as u64),
            )?,
            Mode::Decompress => decompress_stream(&mut reader, &mut sink, dict)?,
            Mode::Test | Mode::List => unreachable!(),
        }
        sink.flush().wrap_err("failed to flush output")?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    replace_output_file(&temp_path, &output)?;

    info!("{} -> {}", input.display(), output.display());
    if opts.remove_source && !opts.keep {
        fs::remove_file(input).wrap_err("failed to remove source file after success")?;
    }
    Ok(())
}

/// Streaming compression core (file or stdout), optionally dictionary-primed.
fn compress_stream<R: Read, W: Write>(
    mut reader: R,
    writer: W,
    level: i32,
    store: bool,
    dict: Option<&[u8]>,
    size_hint: Option<u64>,
) -> color_eyre::Result<()> {
    let compression_level = if store {
        CompressionLevel::Uncompressed
    } else {
        CompressionLevel::from_level(level)
    };
    let mut encoder = structured_zstd::encoding::StreamingEncoder::new(writer, compression_level);
    if let Some(size) = size_hint {
        // The size is known (a regular file), so pledge it: the frame records
        // Frame_Content_Size (decoders can pre-allocate, `zstd -l` reports it)
        // and the matcher sizes its tables to the source. stdin keeps it unset.
        encoder
            .set_pledged_content_size(size)
            .wrap_err("failed to set pledged content size")?;
    }
    if let Some(raw) = dict {
        encoder
            .set_dictionary_from_bytes(raw)
            .wrap_err("failed to load dictionary for compression")?;
    }
    io::copy(&mut reader, &mut encoder).wrap_err("streaming compression failed")?;
    encoder.finish().wrap_err("failed to finalize zstd frame")?;
    Ok(())
}

/// Streaming decompression core (file, stdout, or sink), optionally dict-primed.
fn decompress_stream<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    dict: Option<&[u8]>,
) -> color_eyre::Result<()> {
    let mut decoder = match dict {
        Some(raw) => {
            structured_zstd::decoding::StreamingDecoder::new_with_dictionary_bytes(reader, raw)
                .map_err(|err| eyre!("failed to init dictionary decoder: {err:?}"))?
        }
        None => structured_zstd::decoding::StreamingDecoder::new(reader)
            .map_err(|err| eyre!("invalid zstd frame: {err:?}"))?,
    };
    io::copy(&mut decoder, &mut writer).wrap_err("streaming decompression failed")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// File-output plumbing (atomic temp-write + replace, alias guards). Unchanged
// from the original CLI; shared by compress and decompress.
// ---------------------------------------------------------------------------

fn ensure_distinct_paths(input: &Path, output: &Path) -> color_eyre::Result<()> {
    let canonical_input = match fs::canonicalize(input) {
        Ok(path) => path,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(err).wrap_err("failed to open input file");
        }
        Err(err) => {
            return Err(err).wrap_err("failed to canonicalize input file");
        }
    };
    if output.exists() {
        let canonical_output =
            fs::canonicalize(output).wrap_err("failed to canonicalize existing output file")?;
        if canonical_input == canonical_output || paths_point_to_same_file(input, output)? {
            return Err(eyre!(
                "input and output paths refer to the same file: {input:?} -> {output:?}"
            ));
        }
    }
    Ok(())
}

fn paths_point_to_same_file(input: &Path, output: &Path) -> color_eyre::Result<bool> {
    let input_metadata = fs::metadata(input).wrap_err("failed to inspect input file metadata")?;
    let output_metadata =
        fs::metadata(output).wrap_err("failed to inspect existing output file metadata")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(input_metadata.dev() == output_metadata.dev()
            && input_metadata.ino() == output_metadata.ino())
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(
            input_metadata.volume_serial_number() == output_metadata.volume_serial_number()
                && input_metadata.file_index() == output_metadata.file_index(),
        )
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = input_metadata;
        let _ = output_metadata;
        Ok(false)
    }
}

#[cfg(windows)]
fn create_temporary_output_path(output: &Path) -> color_eyre::Result<PathBuf> {
    let (path, file) = create_temporary_output_file(output)?;
    drop(file);
    fs::remove_file(&path).wrap_err("failed to reserve temporary output path")?;
    Ok(path)
}

fn create_temporary_output_file(output: &Path) -> color_eyre::Result<(PathBuf, File)> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output.zst");
    for attempt in 0..u16::MAX {
        let candidate = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            attempt
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).wrap_err("failed to create temporary output file");
            }
        }
    }
    Err(eyre!("failed to allocate unique temporary output file"))
}

fn replace_output_file(temporary_output_path: &Path, output: &Path) -> color_eyre::Result<()> {
    let output_kind = match output_destination_kind(output).inspect_err(|_err| {
        let _ = fs::remove_file(temporary_output_path);
    })? {
        Some(kind) => kind,
        None => {
            return match fs::rename(temporary_output_path, output) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let _ = fs::remove_file(temporary_output_path);
                    Err(err).wrap_err("failed to move temporary output file into final location")
                }
            };
        }
    };
    if !output_kind.is_file() {
        let _ = fs::remove_file(temporary_output_path);
        return Err(eyre!(
            "output path exists and is not a regular file: {output:?}"
        ));
    }
    let original_permissions = fs::metadata(output)
        .wrap_err("failed to read existing output file metadata")
        .inspect_err(|_err| {
            let _ = fs::remove_file(temporary_output_path);
        })?
        .permissions();
    if let Err(err) = fs::set_permissions(temporary_output_path, original_permissions.clone()) {
        let _ = fs::remove_file(temporary_output_path);
        return Err(err).wrap_err("failed to apply existing output permissions to temporary file");
    }

    #[cfg(not(windows))]
    {
        match fs::rename(temporary_output_path, output) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = fs::remove_file(temporary_output_path);
                Err(err).wrap_err("failed to move temporary output file into final location")
            }
        }
    }

    #[cfg(windows)]
    {
        let backup_output_path = match create_temporary_output_path(output) {
            Ok(path) => path,
            Err(err) => {
                let _ = fs::remove_file(temporary_output_path);
                return Err(err).wrap_err("failed to allocate backup output path");
            }
        };
        if let Err(err) = fs::rename(output, &backup_output_path) {
            let _ = fs::remove_file(temporary_output_path);
            return Err(err).wrap_err("failed to move existing output file into backup location");
        }

        if let Err(err) = fs::rename(temporary_output_path, output) {
            let restore_result = fs::rename(&backup_output_path, output);
            let _ = fs::remove_file(temporary_output_path);
            if let Err(restore_err) = restore_result {
                return Err(err).wrap_err(format!(
                "failed to move temporary output file into final location; also failed to restore backup from {backup_output_path:?}: {restore_err}"
            ));
            }
            return Err(err).wrap_err("failed to move temporary output file into final location");
        }

        let _ = fs::remove_file(&backup_output_path);
        Ok(())
    }
}

fn output_destination_kind(output: &Path) -> color_eyre::Result<Option<std::fs::FileType>> {
    match fs::symlink_metadata(output) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err("failed to inspect existing output path"),
    }
}

fn ensure_regular_output_destination(output: &Path) -> color_eyre::Result<()> {
    if output_destination_kind(output)?.is_some_and(|kind| !kind.is_file()) {
        return Err(eyre!(
            "output path exists and is not a regular file: {output:?}"
        ));
    }
    Ok(())
}

/// Append a file extension to `path` (pending stdlib `PathBuf::add_extension`,
/// stable in 1.91).
fn add_extension<P: AsRef<Path>>(path: &Path, extension: P) -> PathBuf {
    let mut output = path.to_path_buf().into_os_string();
    output.push(extension.as_ref().as_os_str());
    output.into()
}

#[cfg(test)]
mod tests {
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
        };
        assert_eq!(
            derive_output_path(&opts, Path::new("archive.tar.zst")).unwrap(),
            PathBuf::from("archive.tar")
        );
        assert!(derive_output_path(&opts, Path::new("noext")).is_err());
    }
}
