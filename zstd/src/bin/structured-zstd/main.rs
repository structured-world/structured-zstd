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

use structured_zstd::encoding::CompressionLevel;

/// Error type for the tool: a boxed message, which is all a command-line
/// program does with an error — print it and exit non-zero. Written against
/// `std` rather than an error library so the tool adds no dependency to the
/// crate that hosts it.
type Error = Box<dyn std::error::Error + Send + Sync>;

/// The tool's result alias, shadowing `std::result::Result`'s second parameter.
type Result<T, E = Error> = core::result::Result<T, E>;

/// Build an [`Error`] from a format string.
macro_rules! eyre {
    ($($arg:tt)*) => {
        <$crate::Error as From<String>>::from(format!($($arg)*))
    };
}

/// Return early with a formatted [`Error`].
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(eyre!($($arg)*))
    };
}

/// Attach context to an error, the way `wrap_err` does: the message, then the
/// cause. Kept under the same names so every call site reads unchanged.
trait WrapErr<T> {
    /// Prefix the error with `msg`.
    fn wrap_err(self, msg: impl core::fmt::Display) -> Result<T>;
    /// Prefix the error with a message built only when there is an error.
    fn wrap_err_with<D: core::fmt::Display, F: FnOnce() -> D>(self, msg: F) -> Result<T>;
}

impl<T, E: core::fmt::Display> WrapErr<T> for core::result::Result<T, E> {
    fn wrap_err(self, msg: impl core::fmt::Display) -> Result<T> {
        self.map_err(|source| eyre!("{msg}: {source}"))
    }

    fn wrap_err_with<D: core::fmt::Display, F: FnOnce() -> D>(self, msg: F) -> Result<T> {
        self.map_err(|source| eyre!("{}: {source}", msg()))
    }
}
/// Status line to stderr. A tool this size does not need a tracing subscriber
/// to say "file -> file.zst"; keeping it a macro preserves every call site.
macro_rules! info {
    ($($arg:tt)*) => {
        eprintln!($($arg)*)
    };
}

const ZSTD_SUFFIX: &str = ".zst";

/// Operation selected by mode flags / `argv[0]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Compress,
    Decompress,
    Test,
    List,
    Train,
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
    /// Target dictionary size for `--train` (`--maxdict`, upstream default 112640).
    max_dict: usize,
    /// Explicit dictionary ID for `--train` (`--dictID`).
    dict_id: Option<u32>,
    /// Benchmark mode (`-b`); benchmarks `bench_start..=bench_end`.
    bench: bool,
    bench_start: i32,
    bench_end: i32,
    /// Per-level benchmark time budget in seconds (`-i`, default 1).
    bench_secs: f64,
    /// Long-distance matching (`--long`), enabled on the encoder via the
    /// compression-parameters API.
    long: bool,
    /// Pledged input size from `--stream-size` / `--size-hint`. Used when the
    /// input is not a regular file whose length we can stat, which is exactly
    /// the case upstream documents these for.
    size_hint: Option<u64>,
}

/// Upstream `zstd --maxdict` default (110 KiB).
const DEFAULT_MAX_DICT: usize = 112_640;

/// Parse a size written the way upstream accepts it: a plain count, or one
/// suffixed `KB`/`MB`/`GB` (also spelled `K`/`M`/`G`, any case). Upstream uses
/// powers of two for these, despite the decimal-looking names.
fn parse_size(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    let upper = trimmed.to_ascii_uppercase();
    let (digits, shift) = match upper.as_str() {
        s if s.ends_with("KB") => (&trimmed[..trimmed.len() - 2], 10),
        s if s.ends_with("MB") => (&trimmed[..trimmed.len() - 2], 20),
        s if s.ends_with("GB") => (&trimmed[..trimmed.len() - 2], 30),
        s if s.ends_with('K') => (&trimmed[..trimmed.len() - 1], 10),
        s if s.ends_with('M') => (&trimmed[..trimmed.len() - 1], 20),
        s if s.ends_with('G') => (&trimmed[..trimmed.len() - 1], 30),
        _ => (trimmed, 0),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| eyre!("expected a size, got `{text}`"))?;
    // `checked_shl` then `checked_mul` would be the same test twice: a shift
    // that fits still has to fit as a product. One checked multiply says it.
    value
        .checked_mul(1u64 << shift)
        .ok_or_else(|| eyre!("size `{text}` does not fit in 64 bits"))
}

/// Outcome of argument parsing: either run with `Options`, or a terminal
/// message already handled (help / version).
enum Parsed {
    Run(Options),
    Handled,
}

fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    let prog = raw.first().map(String::as_str).unwrap_or("zstd");
    let (default_mode, argv0_stdout) = program_mode(prog);

    let parsed = parse_args(&raw[1..], default_mode, argv0_stdout)?;
    let options = match parsed {
        Parsed::Run(options) => options,
        Parsed::Handled => return Ok(()),
    };

    // Status goes to stderr through `info!` below, so it never contaminates a
    // `-c` stdout data stream. No subscriber to install: the macro writes
    // there directly.
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
fn parse_args(args: &[String], default_mode: Mode, argv0_stdout: bool) -> Result<Parsed> {
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
        max_dict: DEFAULT_MAX_DICT,
        dict_id: None,
        bench: false,
        bench_start: CompressionLevel::DEFAULT_LEVEL,
        bench_end: 0,
        bench_secs: 1.0,
        long: false,
        size_hint: None,
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
                "train" | "train-fastcover" | "train-cover" | "train-legacy" => {
                    opts.mode = Mode::Train
                }
                "stdout" | "to-stdout" => opts.to_stdout = true,
                "force" => opts.force = true,
                "keep" => opts.keep = true,
                "rm" => opts.remove_source = true,
                "ultra" => ultra = true,
                // Verbosity aliases are honest no-ops (our logging is fixed).
                "quiet" | "verbose" => {}
                // These change the wire format (suppress the checksum, the
                // Frame_Content_Size field, or the Dictionary_ID). They are not
                // wired through to the encoder yet, so accepting them silently
                // would hand the caller the default layout instead of the
                // requested one. Reject until they are honoured.
                "no-check" | "no-content-size" | "no-dictID" => {
                    bail!("--{long} is not supported yet");
                }
                "version" => {
                    print_version();
                    return Ok(Parsed::Handled);
                }
                "help" => {
                    print_help();
                    return Ok(Parsed::Handled);
                }
                // Flags that steer HOW the work is done, not what comes out:
                // thread counts, memory ceilings, IO strategy, progress
                // display, matcher hints. We are single-threaded and pick our
                // own limits, so accepting them yields the same valid stream.
                // Upstream takes them, so a script that passes them must not
                // fail here — that is the whole drop-in contract.
                "single-thread"
                | "adapt"
                | "progress"
                | "no-progress"
                | "check"
                | "sparse"
                | "no-sparse"
                | "asyncio"
                | "no-asyncio"
                | "mmap-dict"
                | "no-mmap-dict"
                | "pass-through"
                | "no-pass-through"
                | "compress-literals"
                | "no-compress-literals"
                | "row-match-finder"
                | "no-row-match-finder"
                | "exclude-compressed" => {}
                _ => {
                    if long == "fast" {
                        // `--fast` is the level -1 alias.
                        opts.level = -1;
                    } else if let Some(v) = long.strip_prefix("fast=") {
                        // `--fast=N` is level -N for a positive N. Parse as
                        // unsigned so `--fast=-5` is rejected rather than flipping
                        // sign into a positive level. Exact-match the prefix so a
                        // typo like `--faster` falls through to unknown-option.
                        let n = v.parse::<u32>().wrap_err("invalid --fast level")?;
                        opts.level = -i32::try_from(n).wrap_err("--fast level too large")?;
                    } else if let Some(path) = long.strip_prefix("use-dict=") {
                        opts.dict = Some(PathBuf::from(path));
                    } else if let Some(v) = long.strip_prefix("maxdict=") {
                        opts.max_dict = v.parse::<usize>().wrap_err("invalid --maxdict size")?;
                    } else if let Some(v) = long.strip_prefix("dictID=") {
                        opts.dict_id = Some(v.parse::<u32>().wrap_err("invalid --dictID")?);
                    } else if let Some(v) = long
                        .strip_prefix("stream-size=")
                        .or_else(|| long.strip_prefix("size-hint="))
                    {
                        // Real information, so it is threaded through rather
                        // than ignored: the encoder sizes its window and tables
                        // from the pledged size.
                        opts.size_hint =
                            Some(parse_size(v).wrap_err("invalid --stream-size / --size-hint")?);
                    } else if let Some(v) = long
                        .strip_prefix("memory=")
                        .or_else(|| long.strip_prefix("memlimit="))
                        .or_else(|| long.strip_prefix("memlimit-decompress="))
                    {
                        // Accepted for compatibility: our decoder derives its
                        // own window ceiling from the frame header, which is
                        // what this flag guards against. Validated so a typo
                        // still fails.
                        let _ = parse_size(v).wrap_err("invalid memory limit")?;
                    } else if let Some(v) = long.strip_prefix("auto-threads=") {
                        // Single-threaded: the choice has no effect, but a bad
                        // value is still a bad command line.
                        if v != "physical" && v != "logical" {
                            bail!("--auto-threads must be `physical` or `logical`, got `{v}`");
                        }
                    } else if let Some(v) = long.strip_prefix("target-compressed-block-size=") {
                        let _ = parse_size(v).wrap_err("invalid --target-compressed-block-size")?;
                    } else if let Some(v) = long.strip_prefix("threads=") {
                        let _ = v.parse::<u32>().wrap_err("invalid --threads")?;
                    } else if let Some(v) = long.strip_prefix("format=") {
                        // Anything but zstd would hand back a file the caller
                        // did not ask for, so it fails rather than silently
                        // producing a `.zst` under a `.gz` name.
                        if v != "zstd" {
                            bail!("--format={v} is not supported; this build only writes zstd");
                        }
                    } else if long == "rsyncable" || long.starts_with("patch-from=") {
                        // Both change the emitted frame, so silence would be a
                        // wrong answer rather than a slower one.
                        bail!("--{long} is not implemented");
                    } else if long == "long" {
                        opts.long = true;
                    } else if let Some(v) = long.strip_prefix("long=") {
                        // `--long=N`: the window-log hint must be numeric (it is
                        // accepted but the encoder derives the LDM window from the
                        // level). Reject `--long=` / `--long=abc` instead of
                        // treating them as a silent no-op. Exact-match so
                        // `--longer` is an unknown option.
                        let _ = v.parse::<u32>().wrap_err("invalid --long window log")?;
                        opts.long = true;
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
                'b' | 'e' | 'i' => {
                    // `-b[N]` benchmark (start level), `-e[N]` end level for a
                    // range, `-i[N]` iteration budget. The number is attached
                    // (`-b19`), upstream-style; bare `-b` benchmarks the default.
                    let rest: String = chars[ci + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        None
                    } else {
                        Some(rest.parse::<i32>().wrap_err("invalid numeric suffix")?)
                    };
                    match c {
                        'b' => {
                            opts.bench = true;
                            if let Some(v) = value {
                                opts.bench_start = v;
                            }
                        }
                        'e' => {
                            if let Some(v) = value {
                                opts.bench_end = v;
                            }
                        }
                        // `-i[N]`: per-level benchmark time budget in seconds.
                        'i' => {
                            if let Some(v) = value {
                                opts.bench_secs = (v.max(1)) as f64;
                            }
                        }
                        _ => unreachable!(),
                    }
                    ci = chars.len();
                    continue;
                }
                'c' => opts.to_stdout = true,
                'f' => opts.force = true,
                'k' => opts.keep = true,
                // `-S` (benchmark each file separately) is the default here;
                // `-q`/`-v` verbosity are accepted no-ops.
                'q' | 'v' | 'S' => {}
                'B' | 'T' | 'M' => {
                    // `-B[N]` benchmark block size, `-T[N]` thread count,
                    // `-M[N]` decompression memory ceiling. All three steer how
                    // the work is done, not what comes out: we use a fixed
                    // block size, run single-threaded, and bound the window
                    // from the frame header. Upstream accepts them, so a script
                    // that passes them must not fail here. Consume any attached
                    // value.
                    ci = chars.len();
                    continue;
                }
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
    // `-o` names a single output, so it can't fan out over multiple inputs —
    // except `--train`, where many sample files legitimately feed one dictionary.
    if opts.mode != Mode::Train && !opts.bench && opts.output.is_some() && opts.inputs.len() > 1 {
        bail!("-o cannot be combined with multiple input files");
    }
    if opts.bench && opts.bench_end < opts.bench_start {
        opts.bench_end = opts.bench_start;
    }
    Ok(Parsed::Run(opts))
}

fn validate_level(level: i32) -> Result<()> {
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
         \x20 -l, --list           list information about .zst files\n\
         \x20 --train FILEs        train a dictionary from sample files\n\
         \x20 -b[N] [-e[N]]        benchmark level N (through e)\n\
         \n\
         Options:\n\
         \x20 -<N>                 compression level (1-19; 20-22 need --ultra)\n\
         \x20 --fast[=N]           ultra-fast negative level\n\
         \x20 --ultra              allow levels 20-22\n\
         \x20 --long[=N]           enable long-distance matching\n\
         \x20 -D FILE              use FILE as a dictionary\n\
         \x20 --maxdict=N          dictionary size cap for --train\n\
         \x20 --dictID=N           dictionary ID for --train\n\
         \x20 -o FILE              write output to FILE\n\
         \x20 -c, --stdout         write to stdout\n\
         \x20 -f, --force          overwrite output / allow stdout to terminal\n\
         \x20 -k, --keep           keep (do not delete) source files\n\
         \x20 --rm                 remove source files after success\n\
         \x20 --stream-size=N      pledge the size of a streamed input\n\
         \x20 --size-hint=N        same, as an estimate\n\
         \x20 -V, --version        print version\n\
         \x20 -h, --help           print this help\n\
         \n\
         Accepted for compatibility, with no effect here: -T/--single-thread/\n\
         --auto-threads (single-threaded), -M/--memory (the window is bounded\n\
         from the frame header), -B, --adapt, --[no-]progress, --[no-]check,\n\
         --[no-]sparse, --[no-]asyncio, --[no-]mmap-dict, --[no-]pass-through,\n\
         --[no-]compress-literals, --[no-]row-match-finder.\n\
         \n\
         With no FILE, or when FILE is `-`, read stdin / write stdout."
    );
}

fn run(opts: Options) -> Result<()> {
    let dict_bytes = match &opts.dict {
        Some(path) => Some(
            fs::read(path)
                .wrap_err_with(|| format!("failed to read dictionary file {}", path.display()))?,
        ),
        None => None,
    };
    let dict = dict_bytes.as_deref();

    // `-b` benchmarks compression/decompression across levels instead of
    // producing output files; handle it before the streaming flow.
    if opts.bench {
        return run_benchmark(&opts, dict);
    }

    // `--train` builds a dictionary from the sample files rather than
    // (de)compressing them; handle it before the streaming flow.
    if opts.mode == Mode::Train {
        return train_dictionary(&opts);
    }

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

/// `-b`: benchmark compression + decompression of the input across the
/// requested level range, reporting ratio and best-of throughput. A simplified
/// `zstd -b#` (per-level row); honours `-D` so dictionary throughput can be
/// measured. Time-budgeted per level rather than fixed-iteration.
fn run_benchmark(opts: &Options, dict: Option<&[u8]>) -> Result<()> {
    use std::time::Instant;

    if opts.inputs.is_empty() || opts.inputs.iter().any(|i| i == "-") {
        bail!("-b requires one or more regular input files to benchmark");
    }
    let mut data = Vec::new();
    for input in &opts.inputs {
        data.extend_from_slice(
            &fs::read(input).wrap_err_with(|| format!("failed to read {input}"))?,
        );
    }
    if data.is_empty() {
        bail!("-b: input is empty");
    }
    // Per-level time budget; best (fastest) pass wins, like upstream's -i loop.
    let mb = data.len() as f64 / 1e6;
    println!(
        "benchmarking {} ({})  levels {}..={}",
        opts.inputs.join(", "),
        fmt_size(data.len() as f64),
        opts.bench_start,
        opts.bench_end,
    );

    for level in opts.bench_start..=opts.bench_end {
        validate_level(level)?;
        let mut compressed = Vec::new();
        let mut best_compress = f64::MAX;
        let start = Instant::now();
        loop {
            compressed.clear();
            let t = Instant::now();
            compress_stream(
                data.as_slice(),
                &mut compressed,
                level,
                opts.store,
                dict,
                Some(data.len() as u64),
                opts.long,
            )?;
            best_compress = best_compress.min(t.elapsed().as_secs_f64());
            if start.elapsed().as_secs_f64() >= opts.bench_secs {
                break;
            }
        }

        let mut best_decompress = f64::MAX;
        let start = Instant::now();
        loop {
            let mut out = Vec::new();
            let t = Instant::now();
            decompress_stream(compressed.as_slice(), &mut out, dict)?;
            best_decompress = best_decompress.min(t.elapsed().as_secs_f64());
            if start.elapsed().as_secs_f64() >= opts.bench_secs {
                break;
            }
        }

        let ratio = data.len() as f64 / compressed.len() as f64;
        let c_speed = if best_compress > 0.0 {
            mb / best_compress
        } else {
            f64::INFINITY
        };
        let d_speed = if best_decompress > 0.0 {
            mb / best_decompress
        } else {
            f64::INFINITY
        };
        println!(
            "{level:>3}  {:>10}  {ratio:>7.3}  {c_speed:>7.1} MB/s comp  {d_speed:>8.1} MB/s decomp",
            fmt_size(compressed.len() as f64),
        );
    }
    Ok(())
}

/// `--train`: build a FastCOVER dictionary from the concatenated sample files
/// and write it to `-o` (default `dictionary`). Mirrors upstream
/// `zstd --train FILEs -o dict --maxdict=N [--dictID=N]`.
fn train_dictionary(opts: &Options) -> Result<()> {
    use structured_zstd::dictionary::{
        FastCoverOptions, FinalizeOptions, create_fastcover_dict_from_source,
    };

    if opts.inputs.is_empty() {
        bail!("--train requires one or more sample files");
    }
    let mut corpus = Vec::new();
    for input in &opts.inputs {
        let bytes =
            fs::read(input).wrap_err_with(|| format!("failed to read training sample {input}"))?;
        corpus.extend_from_slice(&bytes);
    }
    let output = opts
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("dictionary"));

    let mut dict = Vec::new();
    create_fastcover_dict_from_source(
        corpus.as_slice(),
        &mut dict,
        opts.max_dict,
        &FastCoverOptions::default(),
        FinalizeOptions {
            dict_id: opts.dict_id,
        },
    )
    .map_err(|err| eyre!("dictionary training failed: {err}"))?;

    fs::write(&output, &dict)
        .wrap_err_with(|| format!("failed to write dictionary {}", output.display()))?;
    info!(
        "trained {} ({}) from {} sample file(s)",
        output.display(),
        fmt_size(dict.len() as f64),
        opts.inputs.len()
    );
    Ok(())
}

/// Read into `buf` until it is full or EOF, returning the number of bytes read.
/// Unlike a single `read`, this fills as much as the source has, so a header
/// parse sees the whole header even when the OS hands back short reads.
fn read_filling<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err).wrap_err("failed to read frame header"),
        }
    }
    Ok(filled)
}

/// Column header for `--list`, matching upstream's `zstd -l` layout.
fn print_list_header() {
    println!("Frames  Compressed  Uncompressed  Ratio  Check  DictID  Filename");
}

/// Largest possible zstd frame header: 4-byte magic + 1-byte descriptor + up to
/// 8-byte Frame_Content_Size + up to 4-byte Dictionary_ID + 1-byte
/// Window_Descriptor (RFC 8878 §3.1.1.1).
const MAX_FRAME_HEADER_LEN: usize = 18;

/// Print one `--list` row: walk every frame in the file (no body decode),
/// summing compressed + declared content sizes. Decompressed size is `--` when
/// any frame omits the Frame_Content_Size field.
///
/// Reads each frame header, then walks the 3-byte block headers by `seek`ing
/// past every block body, so peak memory stays O(1) regardless of archive size
/// (a multi-GB file is never loaded whole).
fn list_file(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    use structured_zstd::decoding::{FrameContentSize, read_frame_header_info};

    let mut file =
        File::open(path).wrap_err_with(|| format!("failed to open {}", path.display()))?;
    let compressed = file
        .metadata()
        .wrap_err_with(|| format!("failed to stat {}", path.display()))?
        .len();
    // An empty file is not a zstd stream: without this guard the walk loop
    // below never runs and we would print a spurious 0-frame success row.
    if compressed == 0 {
        bail!("{}: not a zstd frame: empty file", path.display());
    }
    let mut offset = 0u64;
    let mut frames = 0u64;
    let mut decompressed = Some(0u64);
    let mut check = false;
    let mut dict_id = None;

    while offset < compressed {
        // Read just enough for the frame header (a short read near EOF is fine —
        // `read_frame_header_info` reports the exact length it consumed).
        file.seek(SeekFrom::Start(offset))?;
        let mut header_buf = [0u8; MAX_FRAME_HEADER_LEN];
        let header_read = read_filling(&mut file, &mut header_buf)?;
        let info = read_frame_header_info(&header_buf[..header_read], false)
            .map_err(|err| eyre!("{}: not a zstd frame: {err:?}", path.display()))?;

        // The frame's Block_Maximum_Size bounds every block (RFC 8878 §3.1.1.2).
        let block_size_max = info.window_size.min(128 * 1024);
        // Walk block headers, seeking past each body, to find the frame's end.
        let mut block_offset = offset + info.header_size as u64;
        loop {
            file.seek(SeekFrom::Start(block_offset))?;
            let mut block_header = [0u8; 3];
            file.read_exact(&mut block_header)
                .map_err(|_| eyre!("{}: truncated mid-frame", path.display()))?;
            let raw = u32::from(block_header[0])
                | (u32::from(block_header[1]) << 8)
                | (u32::from(block_header[2]) << 16);
            let last_block = (raw & 1) != 0;
            let block_type = (raw >> 1) & 0b11;
            let block_size = u64::from(raw >> 3);
            if block_size > block_size_max {
                bail!("{}: block exceeds Block_Maximum_Size", path.display());
            }
            // On-disk bytes after the header: RLE stores one byte, Raw/Compressed
            // store Block_Size, the reserved type is invalid.
            let on_disk = match block_type {
                1 => 1,
                0 | 2 => block_size,
                _ => bail!("{}: reserved block type", path.display()),
            };
            block_offset = block_offset
                .checked_add(3 + on_disk)
                .filter(|end| *end <= compressed)
                .ok_or_else(|| eyre!("{}: truncated mid-frame", path.display()))?;
            if last_block {
                break;
            }
        }
        // A trailing 4-byte content checksum follows the last block when present.
        let frame_end = if info.content_checksum {
            block_offset
                .checked_add(4)
                .filter(|end| *end <= compressed)
                .ok_or_else(|| eyre!("{}: truncated content checksum", path.display()))?
        } else {
            block_offset
        };

        match info.content_size {
            FrameContentSize::Known(n) => decompressed = decompressed.map(|d| d + n),
            FrameContentSize::Unknown => decompressed = None,
        }
        check |= info.content_checksum;
        if frames == 0 {
            dict_id = info.dictionary_id;
        }
        frames += 1;
        offset = frame_end;
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

/// stdin → stdout (or → `-o` file) for a `-` input or no inputs.
fn process_stdin_stdout(opts: &Options, dict: Option<&[u8]>) -> Result<()> {
    let stdin = io::stdin();
    let reader = stdin.lock();
    // `-o` redirects stdin's (de)compressed output to a file, unless `-c`
    // explicitly forces stdout. stdin has no length to stat, which is exactly
    // why upstream offers `--stream-size` / `--size-hint`: pass whatever the
    // caller pledged.
    if let Some(output) = &opts.output
        && !opts.to_stdout
        && matches!(opts.mode, Mode::Compress | Mode::Decompress)
    {
        return write_stream_to_file(opts, reader, output, opts.size_hint, dict);
    }
    match opts.mode {
        Mode::Compress => {
            let stdout = io::stdout();
            compress_stream(
                reader,
                stdout.lock(),
                opts.level,
                opts.store,
                dict,
                opts.size_hint,
                opts.long,
            )
        }
        Mode::Decompress => {
            let stdout = io::stdout();
            decompress_stream(reader, stdout.lock(), dict)
        }
        Mode::Test => decompress_stream(reader, io::sink(), dict).map(|_| {
            info!("stdin: OK");
        }),
        Mode::List | Mode::Train => {
            unreachable!("--list / --train handled in run() before streaming")
        }
    }
}

/// Remove the source file after a successful (de)compression when `--rm` is set
/// (and `-k` was not). A no-op otherwise.
fn remove_source_if_requested(opts: &Options, input: &Path) -> Result<()> {
    if opts.remove_source && !opts.keep {
        fs::remove_file(input).wrap_err("failed to remove source file after success")?;
    }
    Ok(())
}

/// Run the (de)compression core into an arbitrary writer for the current mode.
fn run_stream_core<R: Read, W: Write>(
    opts: &Options,
    reader: R,
    writer: W,
    size_hint: Option<u64>,
    dict: Option<&[u8]>,
) -> Result<()> {
    match opts.mode {
        Mode::Compress => compress_stream(
            reader, writer, opts.level, opts.store, dict, size_hint, opts.long,
        ),
        Mode::Decompress => decompress_stream(reader, writer, dict),
        Mode::Test | Mode::List | Mode::Train => {
            unreachable!("test / list / train modes never stream to a writer here")
        }
    }
}

/// Stream `reader` into `output` atomically: write to a sibling temp file, then
/// rename into place on success (and clean the temp up on failure). Honours the
/// `-f` overwrite gate.
fn write_stream_to_file<R: Read>(
    opts: &Options,
    mut reader: R,
    output: &Path,
    size_hint: Option<u64>,
    dict: Option<&[u8]>,
) -> Result<()> {
    ensure_regular_output_destination(output)?;
    if output.exists() && !opts.force {
        bail!("{} already exists; use -f to overwrite", output.display());
    }
    let (temp_path, temp_file) = create_temporary_output_file(output)?;
    let result: Result<()> = (|| {
        let mut sink = temp_file;
        run_stream_core(opts, &mut reader, &mut sink, size_hint, dict)?;
        sink.flush().wrap_err("failed to flush output")?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    replace_output_file(&temp_path, output)
}

/// Resolve the output path for a file input under the current mode.
fn derive_output_path(opts: &Options, input: &Path) -> Result<PathBuf> {
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
        Mode::Test | Mode::List | Mode::Train => {
            unreachable!("test / list / train modes never write an output file")
        }
    }
}

fn process_file(opts: &Options, input: &Path, dict: Option<&[u8]>) -> Result<()> {
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

    // stdout sink: bypass the temp-file dance. `--rm` still applies to the
    // source file once the stream completes, so run the removal before
    // returning rather than short-circuiting past it.
    if opts.to_stdout {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        run_stream_core(opts, &mut reader, &mut out, Some(source_size as u64), dict)?;
        return remove_source_if_requested(opts, input);
    }

    let output = derive_output_path(opts, input)?;
    ensure_distinct_paths(input, &output)?;
    write_stream_to_file(opts, reader, &output, Some(source_size as u64), dict)?;

    info!("{} -> {}", input.display(), output.display());
    remove_source_if_requested(opts, input)
}

/// Streaming compression core (file or stdout), optionally dictionary-primed.
#[allow(clippy::too_many_arguments)]
fn compress_stream<R: Read, W: Write>(
    mut reader: R,
    writer: W,
    level: i32,
    store: bool,
    dict: Option<&[u8]>,
    size_hint: Option<u64>,
    long: bool,
) -> Result<()> {
    let compression_level = if store {
        CompressionLevel::Uncompressed
    } else {
        CompressionLevel::from_level(level)
    };
    let mut encoder = structured_zstd::encoding::StreamingEncoder::new(writer, compression_level);
    // The reference `zstd` COMMAND defaults the content checksum ON (unlike
    // the library API, whose default is off and which our encoder mirrors) —
    // set it explicitly so CLI output matches `zstd <file>` byte layout.
    encoder
        .set_content_checksum(true)
        .wrap_err("failed to enable content checksum")?;
    // Long-distance matching (`--long`) is a per-knob override applied via the
    // compression-parameters API; skip it for `--store` (raw frames don't match).
    if long && !store {
        let params = structured_zstd::encoding::CompressionParameters::builder(compression_level)
            .enable_long_distance_matching(true)
            .build()
            .map_err(|err| eyre!("failed to build LDM parameters: {err:?}"))?;
        encoder
            .set_parameters(&params)
            .wrap_err("failed to enable long-distance matching")?;
    }
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
) -> Result<()> {
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

fn ensure_distinct_paths(input: &Path, output: &Path) -> Result<()> {
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

fn paths_point_to_same_file(input: &Path, output: &Path) -> Result<bool> {
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
fn create_temporary_output_path(output: &Path) -> Result<PathBuf> {
    let (path, file) = create_temporary_output_file(output)?;
    drop(file);
    fs::remove_file(&path).wrap_err("failed to reserve temporary output path")?;
    Ok(path)
}

fn create_temporary_output_file(output: &Path) -> Result<(PathBuf, File)> {
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

fn replace_output_file(temporary_output_path: &Path, output: &Path) -> Result<()> {
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

fn output_destination_kind(output: &Path) -> Result<Option<std::fs::FileType>> {
    match fs::symlink_metadata(output) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err("failed to inspect existing output path"),
    }
}

fn ensure_regular_output_destination(output: &Path) -> Result<()> {
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
mod tests;
