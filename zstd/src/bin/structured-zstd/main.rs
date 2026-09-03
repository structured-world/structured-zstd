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
use std::io::{self, BufRead, BufReader, ErrorKind, IsTerminal, Read, Write};
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
    /// Back-reference window from `--long=N`. Bare `--long` leaves it unset, so
    /// the level's own window applies.
    long_window_log: Option<u32>,
    /// Block-size target from `--target-compressed-block-size`. A soft target,
    /// as upstream documents it: it bounds what goes into a block, so blocks
    /// flush sooner and stay near the requested size.
    target_block_size: Option<u32>,
    /// Exact input length from `--stream-size`. Recorded in the frame header,
    /// so the stream must actually be this long — a wrong value is an error,
    /// not a worse ratio.
    pledged_size: Option<u64>,
    /// Estimated input length from `--size-hint`. Steers the encoder's window
    /// and table sizing only; being wrong costs ratio, never correctness, so
    /// it must NOT reach the header.
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

/// Parse a `-M` / `--memory` value into bytes.
///
/// The default unit is MiB, as upstream documents, so `-M256` is 256 MiB. A
/// suffix means what it says and is not multiplied again: `-M1M` is one
/// mebibyte, a limit far below what decompression needs, and has to be refused
/// as such rather than read as a terabyte that trivially passes.
fn parse_memory_limit(text: &str) -> Result<u64> {
    let value = parse_size(text)?;
    if text.trim().ends_with(|c: char| c.is_ascii_digit()) {
        value
            .checked_mul(1 << 20)
            .ok_or_else(|| eyre!("memory limit `{text}` MiB does not fit in 64 bits"))
    } else {
        Ok(value)
    }
}

/// Check a requested decompression memory ceiling against the one this build
/// actually enforces.
///
/// `-M` is a safety promise about untrusted input, not a performance hint, so
/// it is either kept or refused. The decoder rejects any frame whose window
/// exceeds a fixed ceiling, which makes a request at or above that ceiling
/// already satisfied — we are the stricter of the two. A request BELOW it is a
/// promise this build cannot make, and saying nothing would leave the caller
/// believing a bound that is not there.
fn check_memory_limit(requested: u64) -> Result<()> {
    /// Window ceiling the decoder refuses to exceed.
    const WINDOW: u64 = structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE;
    /// What the decoder holds BESIDES the window, rounded well up: the literal
    /// and block buffers (a block is capped at 128 KiB each), the sequence
    /// storage, the Huffman and FSE tables, and the tool's own I/O buffers.
    /// Counted because the promise is about total memory, not about one
    /// allocation: a limit equal to the window alone is one we would break.
    const AUXILIARY: u64 = 1 << 20;
    let floor = WINDOW + AUXILIARY;
    if requested < floor {
        bail!(
            "requested memory limit {requested} B is below what decompression can need \
             here: a {} MiB window plus about {} MiB of literal, block, sequence and \
             table buffers. This build cannot be tightened below that.",
            WINDOW / (1 << 20),
            AUXILIARY / (1 << 20),
        );
    }
    Ok(())
}

/// Check a `--long=N` window log against what this build can both write and
/// read back.
///
/// The encoder reaches further than the decoder: it accepts window logs up to
/// 30, while decoding refuses any frame declaring a window above
/// [`MAXIMUM_ALLOWED_WINDOW_SIZE`](structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE).
/// The lower of the two is the honest limit, since the values in between only
/// produce files this tool cannot open. Validated at parse time rather than at
/// the first frame, so a wrong value is reported before any output is written.
fn check_window_log(log: u32) -> Result<()> {
    use structured_zstd::encoding::CParameter;

    let bounds = CParameter::WindowLog.bounds();
    let decodable = structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE.ilog2();
    let upper = bounds.upper_bound.min(i64::from(decodable));
    if i64::from(log) < bounds.lower_bound || i64::from(log) > upper {
        bail!(
            "--long window log {log} is outside the supported range {}..={upper} \
             (above {decodable} the frame would declare a window this build \
             refuses to decode)",
            bounds.lower_bound,
        );
    }
    Ok(())
}

/// Validate the parameter list of `--adapt=min=N,max=N`.
///
/// The bounds have no effect here — the level does not vary — but a command
/// line that misspells a key or passes a non-number is broken whether or not
/// this build acts on it, and reporting that is the whole difference between
/// ignoring a flag and hiding a mistake.
fn parse_adapt_params(params: &str) -> Result<()> {
    if params.is_empty() {
        bail!("--adapt= needs parameters, e.g. --adapt=min=1,max=9");
    }
    for field in params.split(',') {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| eyre!("--adapt parameter `{field}` is not `key=value`"))?;
        if key != "min" && key != "max" {
            bail!("--adapt has no `{key}` parameter; expected `min` or `max`");
        }
        value
            .parse::<i32>()
            .map_err(|_| eyre!("--adapt {key} must be a number, got `{value}`"))?;
    }
    Ok(())
}

/// Refuse to write binary output into an interactive terminal unless forced.
///
/// A compressed frame painted into a terminal scrambles the session and the
/// data is lost either way, so upstream requires `-f` for it and so do we. The
/// decision is a pure function of the two inputs, which is what makes it
/// testable: the caller supplies whether stdout is a terminal.
fn guard_binary_stdout(stdout_is_terminal: bool, force: bool) -> Result<()> {
    if stdout_is_terminal && !force {
        bail!(
            "refusing to write compressed data to a terminal; \
             redirect the output, use -o FILE, or pass -f to force it"
        );
    }
    Ok(())
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
        long_window_log: None,
        target_block_size: None,
        pledged_size: None,
        size_hint: None,
    };
    let mut ultra = false;
    // `--train` may appear after `-M`, so the conflict is settled once the
    // whole command line is known rather than at the flag itself.
    let mut memory_limit_given = false;
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
                // Plain `--train` selects the same default upstream does,
                // FastCOVER, so the two spellings agree.
                "train" | "train-fastcover" => opts.mode = Mode::Train,
                // The other trainers produce different dictionaries. Accepting
                // the flag and running FastCOVER anyway would hand back a
                // dictionary the caller did not ask for, with nothing to say so.
                "train-cover" | "train-legacy" => {
                    bail!(
                        "--{long} is not implemented; --train / --train-fastcover trains with FastCOVER"
                    )
                }
                // `-c` and `-o` name competing destinations, so each clears the
                // other and the later one on the command line wins, as upstream
                // does. Setting only one of them lets `-c -o f` ignore the `-o`.
                "stdout" | "to-stdout" => {
                    opts.to_stdout = true;
                    opts.output = None;
                }
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
                | "no-pass-through"
                | "row-match-finder"
                | "no-row-match-finder" => {}
                // Forces literals compressed or stored, which changes the
                // frame that comes out. The encoder has no such switch here,
                // so accepting the flag would hand back the other layout.
                "compress-literals" | "no-compress-literals" => {
                    bail!("--{long} is not implemented");
                }
                // These decide WHICH files are processed, or what happens to
                // input that is not compressed. Accepting them without doing
                // the work would compress a file the caller asked to skip, or
                // fail on one they asked to copy through — a wrong answer, not
                // a slower one.
                "pass-through" | "exclude-compressed" => {
                    bail!("--{long} is not implemented");
                }
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
                    } else if let Some(v) = long.strip_prefix("stream-size=") {
                        // An exact pledge: it goes into the frame header, so a
                        // stream of a different length is an error.
                        opts.pledged_size = Some(parse_size(v).wrap_err("invalid --stream-size")?);
                    } else if let Some(v) = long.strip_prefix("size-hint=") {
                        // An estimate: it sizes the encoder and nothing else,
                        // so a wrong guess costs ratio rather than failing.
                        opts.size_hint = Some(parse_size(v).wrap_err("invalid --size-hint")?);
                    } else if let Some(v) = long
                        .strip_prefix("memory=")
                        .or_else(|| long.strip_prefix("memlimit="))
                        .or_else(|| long.strip_prefix("memlimit-decompress="))
                    {
                        check_memory_limit(
                            parse_memory_limit(v).wrap_err("invalid memory limit")?,
                        )?;
                        memory_limit_given = true;
                    } else if let Some(params) = long.strip_prefix("adapt=") {
                        // Parameterised form (`--adapt=min=1,max=9`). We do not
                        // vary the level, so the bounds change nothing — but a
                        // misspelled key or a non-numeric bound is still a
                        // broken command line, and the contract is that ignored
                        // options validate what they are given.
                        parse_adapt_params(params)?;
                    } else if let Some(v) = long.strip_prefix("auto-threads=") {
                        // Single-threaded: the choice has no effect, but a bad
                        // value is still a bad command line.
                        if v != "physical" && v != "logical" {
                            bail!("--auto-threads must be `physical` or `logical`, got `{v}`");
                        }
                    } else if let Some(v) = long.strip_prefix("target-compressed-block-size=") {
                        let target =
                            parse_size(v).wrap_err("invalid --target-compressed-block-size")?;
                        opts.target_block_size = Some(u32::try_from(target).map_err(|_| {
                            eyre!("--target-compressed-block-size={v} is too large")
                        })?);
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
                        // `--long=N` names the back-reference window, so the N is
                        // carried through to the encoder rather than dropped.
                        // Reject `--long=` / `--long=abc` instead of treating
                        // them as a silent no-op. Exact-match so `--longer` is an
                        // unknown option.
                        let log: u32 = v.parse().wrap_err("invalid --long window log")?;
                        check_window_log(log)?;
                        opts.long = true;
                        opts.long_window_log = Some(log);
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
                'c' => {
                    // Clears `-o`; see the `--stdout` arm for why.
                    opts.to_stdout = true;
                    opts.output = None;
                }
                'f' => opts.force = true,
                'k' => opts.keep = true,
                // `-S` (benchmark each file separately) is the default here;
                // `-q`/`-v` verbosity are accepted no-ops.
                'q' | 'v' | 'S' => {}
                'B' | 'T' => {
                    // `-B[N]` job / block size, `-T[N]` thread count. Both
                    // steer how the work is done, not what comes out: we use a
                    // fixed block size and run single-threaded. Upstream
                    // accepts them, so a script that passes them must not fail
                    // here — but the VALUE is still parsed: ignoring what a
                    // flag does is not a reason to ignore what it says, and a
                    // typo is a broken command line either way.
                    let rest: String = chars[ci + 1..].iter().collect();
                    if !rest.is_empty() {
                        parse_size(&rest).wrap_err_with(|| format!("invalid -{c} value"))?;
                    }
                    ci = chars.len();
                    continue;
                }
                'M' => {
                    // `-M[N]` is a decompression memory ceiling — a safety
                    // promise, so it is checked against the one this build
                    // enforces rather than swallowed.
                    let rest: String = chars[ci + 1..].iter().collect();
                    if !rest.is_empty() {
                        check_memory_limit(
                            parse_memory_limit(&rest).wrap_err("invalid -M memory limit")?,
                        )?;
                        memory_limit_given = true;
                    }
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
                        // Clears `-c`, so the later of the two wins.
                        opts.output = Some(PathBuf::from(value));
                        opts.to_stdout = false;
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

    // `-M` bounds decompression. Training loads every sample into memory
    // instead, a path the limit says nothing about, so accepting it here would
    // imply a bound over the one thing it does not cover.
    if memory_limit_given && opts.mode == Mode::Train {
        bail!("--memory / -M bounds decompression and does not apply to --train");
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
         --auto-threads (single-threaded), -B, --adapt, --[no-]progress,\n\
         --check, --[no-]sparse, --[no-]asyncio, --[no-]mmap-dict,\n\
         --no-pass-through, --[no-]row-match-finder.\n\
         \n\
         --target-compressed-block-size=N bounds what goes into each block, so\n\
         blocks flush sooner and stay near N. --long=N is capped at 27: above\n\
         that the frame would declare a window this build refuses to decode.\n\
         \n\
         Rejected rather than ignored, because they would change the result:\n\
         --no-check, --no-content-size, --no-dictID, --format= (other than\n\
         zstd), --patch-from, --rsyncable, --pass-through,\n\
         --exclude-compressed, --[no-]compress-literals, -M/--memory below\n\
         the enforced ceiling, -M with --train, and --train-cover /\n\
         --train-legacy (--train and --train-fastcover train with FastCOVER).\n\
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
                &FrameSettings {
                    level,
                    // The benchmark holds the whole input, so the length is
                    // exact and there is no estimate to fall back on.
                    pledged_size: Some(data.len() as u64),
                    size_hint: None,
                    ..FrameSettings::from_options(opts)
                },
                dict,
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

    // A trained dictionary is an output file like any other, so it answers to
    // the same rules: no clobbering without `-f`, and written through a
    // temporary that is renamed into place, so an interrupted run leaves the
    // previous dictionary intact rather than a half-written one.
    ensure_regular_output_destination(&output)?;
    if output.exists() && !opts.force {
        bail!("{} already exists; use -f to overwrite", output.display());
    }
    let (temp_path, mut temp_file) = create_temporary_output_file(&output)?;
    let written = temp_file
        .write_all(&dict)
        .and_then(|()| temp_file.flush())
        .wrap_err_with(|| format!("failed to write dictionary {}", output.display()));
    if let Err(err) = written {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    drop(temp_file);
    replace_output_file(&temp_path, &output)?;
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
    println!("Frames  Skips  Compressed  Uncompressed  Ratio  Check  DictID  Filename");
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
    use structured_zstd::decoding::errors::ReadFrameHeaderError;
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
    let mut data_frames = 0u64;
    let mut skips = 0u64;
    let mut decompressed = Some(0u64);
    let mut check = false;
    let mut dict_id = None;

    while offset < compressed {
        // Read just enough for the frame header (a short read near EOF is fine —
        // `read_frame_header_info` reports the exact length it consumed).
        file.seek(SeekFrom::Start(offset))?;
        let mut header_buf = [0u8; MAX_FRAME_HEADER_LEN];
        let header_read = read_filling(&mut file, &mut header_buf)?;
        let info = match read_frame_header_info(&header_buf[..header_read], false) {
            Ok(info) => info,
            // Metadata frames sit inside ordinary archives (a seekable-zstd
            // index is one), so the walk steps over them: 4-byte magic +
            // 4-byte length + the payload.
            Err(ReadFrameHeaderError::SkipFrame { length, .. }) => {
                offset = offset
                    .checked_add(8 + u64::from(length))
                    .filter(|end| *end <= compressed)
                    .ok_or_else(|| eyre!("{}: truncated skippable frame", path.display()))?;
                // Upstream counts a skippable frame in both columns: `Frames`
                // is every frame in the file, `Skips` the metadata ones.
                skips += 1;
                frames += 1;
                continue;
            }
            Err(err) => bail!("{}: not a zstd frame: {err:?}", path.display()),
        };

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
            // Declared, not measured: a handful of header bytes can claim any
            // size, so the running total is checked rather than trusted.
            FrameContentSize::Known(n) => {
                decompressed = match decompressed {
                    Some(total) => Some(total.checked_add(n).ok_or_else(|| {
                        eyre!(
                            "{}: declared content sizes total more than 2^64 bytes",
                            path.display()
                        )
                    })?),
                    None => None,
                }
            }
            FrameContentSize::Unknown => decompressed = None,
        }
        check |= info.content_checksum;
        // The reported dictionary is the first DATA frame's; a leading metadata
        // frame counts towards `frames` but carries no header to read it from.
        if data_frames == 0 {
            dict_id = info.dictionary_id;
        }
        data_frames += 1;
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
        "{frames:>6}  {skips:>5}  {:>10}  {:>12}  {ratio:>5}  {:>5}  {:>6}  {}",
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
        return write_stream_to_file(opts, reader, output, opts.pledged_size, dict);
    }
    match opts.mode {
        Mode::Compress => {
            let stdout = io::stdout();
            guard_binary_stdout(stdout.is_terminal(), opts.force)?;
            compress_stream(
                reader,
                stdout.lock(),
                &FrameSettings::from_options(opts),
                dict,
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
    // Never when the output went to stdout: that may have been a pipe whose
    // reader is gone, a terminal, or anything else we cannot read back, so
    // there is no saved copy to justify deleting the original. Upstream keeps
    // the file for `-c` too.
    if opts.remove_source && !opts.keep && !opts.to_stdout {
        fs::remove_file(input).wrap_err("failed to remove source file after success")?;
    }
    Ok(())
}

/// Run the (de)compression core into an arbitrary writer for the current mode.
fn run_stream_core<R: Read, W: Write>(
    opts: &Options,
    reader: R,
    writer: W,
    // Exact length when the caller knows it (a stat'd file, or
    // `--stream-size`); `--size-hint` travels separately in `opts`.
    pledged_size: Option<u64>,
    dict: Option<&[u8]>,
) -> Result<()> {
    match opts.mode {
        Mode::Compress => compress_stream(
            reader,
            writer,
            &FrameSettings {
                pledged_size,
                ..FrameSettings::from_options(opts)
            },
            dict,
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
        // Only compression produces binary; `-d` to a terminal is text the
        // user asked for, which upstream also allows.
        if matches!(opts.mode, Mode::Compress) {
            guard_binary_stdout(stdout.is_terminal(), opts.force)?;
        }
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

/// Everything the command line says about how one frame is to be built.
///
/// Grouped rather than passed one by one: these travel together through every
/// compression entry point, and a positional list this long invites the caller
/// to line the arguments up wrong.
#[derive(Clone, Copy, Default)]
struct FrameSettings {
    /// Numeric compression level, ignored when `store` is set.
    level: i32,
    /// `--format=zstd` with no compression: emit raw blocks.
    store: bool,
    /// Exact input length, recorded in the frame header.
    pledged_size: Option<u64>,
    /// Estimated input length; steers geometry, never reaches the header.
    size_hint: Option<u64>,
    /// Long-distance matching, with the window log if `--long=N` gave one.
    long: bool,
    long_window_log: Option<u32>,
    /// Soft block-size target from `--target-compressed-block-size`.
    target_block_size: Option<u32>,
}

impl FrameSettings {
    /// The settings the command line asked for, less the per-input size, which
    /// each caller knows and fills in.
    fn from_options(opts: &Options) -> Self {
        Self {
            level: opts.level,
            store: opts.store,
            pledged_size: opts.pledged_size,
            size_hint: opts.size_hint,
            long: opts.long,
            long_window_log: opts.long_window_log,
            target_block_size: opts.target_block_size,
        }
    }
}

/// Streaming compression core (file or stdout), optionally dictionary-primed.
fn compress_stream<R: Read, W: Write>(
    mut reader: R,
    writer: W,
    settings: &FrameSettings,
    dict: Option<&[u8]>,
) -> Result<()> {
    let &FrameSettings {
        level,
        store,
        pledged_size,
        size_hint,
        long,
        long_window_log,
        target_block_size,
    } = settings;
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
    // A smaller block target is what the caller asked for when they want
    // bounded latency; the encoder clamps it to the format's own range.
    if let Some(target) = target_block_size {
        encoder
            .set_target_block_size(Some(target))
            .wrap_err("failed to set the block-size target")?;
    }
    // Long-distance matching (`--long`) is a per-knob override applied via the
    // compression-parameters API; skip it for `--store` (raw frames don't match).
    if long && !store {
        let mut builder =
            structured_zstd::encoding::CompressionParameters::builder(compression_level)
                .enable_long_distance_matching(true);
        // `--long=N` asked for a specific back-reference distance; without it
        // the level's own window stands.
        if let Some(log) = long_window_log {
            builder = builder.window_log(log);
        }
        let params = builder
            .build()
            .map_err(|err| eyre!("failed to build LDM parameters: {err:?}"))?;
        encoder
            .set_parameters(&params)
            .wrap_err("failed to enable long-distance matching")?;
    }
    if let Some(size) = pledged_size {
        // The size is known exactly (a regular file, or `--stream-size`), so
        // pledge it: the frame records Frame_Content_Size (decoders can
        // pre-allocate, `zstd -l` reports it) and the matcher sizes its tables
        // to the source. A stream that then differs in length is an error.
        encoder
            .set_pledged_content_size(size)
            .wrap_err("failed to set pledged content size")?;
    } else if let Some(size) = size_hint {
        // Only an estimate (`--size-hint`): it steers the encoder's geometry
        // and must NOT reach the header, or a wrong guess would turn a
        // successful compression into a failure.
        encoder
            .set_source_size_hint(size)
            .wrap_err("failed to set source size hint")?;
    }
    if let Some(raw) = dict {
        // Whatever `-D` was pointed at: a trained dictionary, or any file at
        // all, taken as raw content the way upstream does.
        let parsed = structured_zstd::decoding::Dictionary::from_serialized_or_raw_content(raw)
            .map_err(|err| eyre!("invalid dictionary: {err:?}"))?;
        encoder
            .set_encoder_dictionary(
                structured_zstd::encoding::EncoderDictionary::from_dictionary(parsed),
            )
            .wrap_err("failed to load dictionary for compression")?;
    }
    io::copy(&mut reader, &mut encoder).wrap_err("streaming compression failed")?;
    encoder.finish().wrap_err("failed to finalize zstd frame")?;
    Ok(())
}

/// Streaming decompression core (file, stdout, or sink), optionally dict-primed.
///
/// A zstd stream is a sequence of frames: `cat a.zst b.zst` is a valid archive
/// that decodes to `a` then `b`, and skippable frames may sit between them. The
/// decoder's `Read` ends at the first frame, so the loop below re-initialises it
/// on whatever follows until the source is exhausted. The library's
/// `read_to_end` walks frames too, but only by buffering the whole stream in
/// memory, which a command-line tool handed a multi-gigabyte archive cannot do.
fn decompress_stream<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    dict: Option<&[u8]>,
) -> Result<()> {
    use structured_zstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};

    // Decoded once rather than per frame: every frame in the stream is primed
    // with the same dictionary.
    let handle = match dict {
        Some(raw) => Some(
            structured_zstd::decoding::DictionaryHandle::from_dictionary(
                structured_zstd::decoding::Dictionary::from_serialized_or_raw_content(raw)
                    .map_err(|err| eyre!("failed to parse dictionary: {err:?}"))?,
            ),
        ),
        None => None,
    };
    // Buffered so the end of the stream can be told from the start of another
    // frame without consuming the bytes that answer the question.
    let mut source = BufReader::new(reader);
    let mut frames = 0u64;
    loop {
        if source
            .fill_buf()
            .wrap_err("failed to read the compressed stream")?
            .is_empty()
        {
            // End of the last frame is success; end before the first one means
            // the input never held a frame at all, which is not an archive that
            // decodes to nothing.
            if frames == 0 {
                bail!("unexpected end of input: no zstd frame");
            }
            return Ok(());
        }
        frames += 1;
        // Borrowed, not moved: a frame that turns out to be skippable leaves
        // the reader with us to step over it and carry on.
        let built = match &handle {
            // The dictionary constructors FORCE the supplied dictionary, which
            // the registration path does not: a frame may legitimately omit the
            // optional dictionary ID, and then nothing would select it.
            Some(h) => structured_zstd::decoding::StreamingDecoder::new_with_dictionary_handle(
                &mut source,
                h,
            ),
            None => structured_zstd::decoding::StreamingDecoder::new(&mut source),
        };
        let mut decoder = match built {
            Ok(decoder) => decoder,
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                // Metadata a decoder is required to step over. The header is
                // already consumed, so only the payload is left to discard.
                let skipped = io::copy(
                    &mut source.by_ref().take(u64::from(length)),
                    &mut io::sink(),
                )
                .wrap_err("failed to skip a skippable frame")?;
                if skipped != u64::from(length) {
                    bail!("skippable frame is truncated: {skipped} of {length} bytes");
                }
                continue;
            }
            Err(err) => bail!("invalid zstd frame: {err:?}"),
        };
        // The library computes the digest but does not compare it, leaving the
        // decision to the caller. For a command-line tool that decision is
        // made: upstream validates by default, and `-t` exists to answer
        // exactly this question, so a frame whose stored checksum disagrees
        // with its data has to fail rather than decode quietly. Read at the end
        // of the frame, so setting it after construction is in time.
        decoder
            .decoder_mut()
            .set_content_checksum(structured_zstd::decoding::ContentChecksum::Verify);
        io::copy(&mut decoder, &mut writer).wrap_err("streaming decompression failed")?;
    }
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
