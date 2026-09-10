//! `zstd` command-line interface with upstream-compatible flag dispatch.
//!
//! The argument model mirrors upstream zstd v1.5.7: mode + level FLAGS (not
//! subcommands), `argv[0]` dispatch (`unzstd` / `zstdcat` change the default
//! mode), stdin/stdout streaming, `-r` / `--filelist` / `--output-dir-*` file
//! selection, the `ZSTD_CLEVEL` / `ZSTD_NBTHREADS` environment, the `-q` /
//! `-v` display levels, and the conventional `-o`/`-f`/`-k`/`-D` file flags.
//! Compression/decompression run through the streaming codec, so peak memory
//! stays O(window), not O(file). Exit status is 0, 1 when any input failed,
//! and 2 when interrupted, as the reference command's is.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, IsTerminal, Read, Write};
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
/// Say something on stderr when `$verbosity` reaches `$level`: `1` carries
/// errors, `2` results and warnings, `3` progress, `4` detail. A tool this
/// size does not need a tracing subscriber; a macro keeps every call site a
/// line.
macro_rules! display {
    ($verbosity:expr, $level:expr, $($arg:tt)*) => {
        if $verbosity >= $level {
            eprintln!($($arg)*);
        }
    };
}

mod display;
mod inputs;
mod interrupt;
mod progress;

use display::{DEFAULT_LEVEL, HumanSize, Progress, confirm};
use inputs::Selection;
use progress::ProgressMonitor;

const ZSTD_SUFFIX: &str = ".zst";

/// Suffixes a decompressed name is derived from, each with what replaces it:
/// `.zst` and `.zstd` are dropped, `.tzst` becomes `.tar`, as the reference
/// command's suffix list has it.
const DECOMPRESS_SUFFIXES: [(&str, &str); 3] = [("zst", ""), ("zstd", ""), ("tzst", "tar")];

/// The reference command version whose command line this tool follows.
const UPSTREAM_VERSION: &str = "1.5.7";

/// How the reference command names stdout in a summary line.
const STDOUT_MARK: &str = "/*stdout*\\";

/// Highest level the CLI compresses at when `--ultra` was not given (upstream
/// `ZSTDCLI_CLEVEL_MAX`). Asking for more without naming `--ultra` reduces to
/// this with a warning rather than failing.
const CLI_MAX_LEVEL_WITHOUT_ULTRA: i32 = 19;

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
    /// Positional inputs; empty or a lone `-` means stdin. Held as paths
    /// rather than text: a filename is bytes and need not be UTF-8.
    inputs: Vec<PathBuf>,
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
    /// Measure each input on its own (`-S`) instead of as one stream, so the
    /// reported ratio and throughput describe a file rather than a mixture.
    bench_separately: bool,
    /// Long-distance matching (`--long`), enabled on the encoder via the
    /// compression-parameters API.
    long: bool,
    /// Back-reference window from `--long=N`. Bare `--long` leaves it unset, so
    /// the level's own window applies.
    long_window_log: Option<u32>,
    /// Decompression memory ceiling from `-M` / `--memory`, in bytes. Kept
    /// rather than checked and dropped, so the `-D` dictionary can be weighed
    /// against it once its size is known.
    memory_limit: Option<u64>,
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
    /// Display level: `-v` raises it, `-q` lowers it (see [`display`]).
    verbosity: i32,
    /// Whether frames carry a content checksum (`-C` / `--[no-]check`), and
    /// whether decoding verifies one. On by default, as the reference
    /// command's is.
    checksum: bool,
    /// Whether a known input length is written into the frame header
    /// (`--[no-]content-size`).
    content_size_flag: bool,
    /// Whether a dictionary frame records the dictionary's ID
    /// (`--no-dictID`).
    dict_id_flag: bool,
    /// Copy input that is not a zstd stream through unchanged when
    /// decompressing (`--[no-]pass-through`). `None` is the reference
    /// command's default: on only when forced and writing to stdout, which
    /// is what `zstdcat` does.
    pass_through: Option<bool>,
    /// Skip inputs whose extension says they are already compressed
    /// (`--exclude-compressed`).
    exclude_compressed: bool,
    /// Replace directories among the inputs by the files beneath them (`-r`).
    recursive: bool,
    /// Process symbolic links rather than skipping them; part of `-f`.
    follow_links: bool,
    /// Read stdin even when it is a terminal; part of `-f`.
    force_stdin: bool,
    /// Files naming further inputs, one per line (`--filelist`).
    filelists: Vec<PathBuf>,
    /// Directory every output is written into, by file name
    /// (`--output-dir-flat`).
    output_dir: Option<PathBuf>,
    /// Root under which each input's directory is replayed for its output
    /// (`--output-dir-mirror`). Wins over `output_dir` when both are given.
    output_dir_mirror: Option<PathBuf>,
    /// Whether the progress counter is drawn (`--[no-]progress`).
    progress: Progress,
}

/// Upstream `zstd --maxdict` default (110 KiB).
const DEFAULT_MAX_DICT: usize = 112_640;

/// Window log a bare `--long` selects, as upstream documents (128 MiB).
const DEFAULT_LONG_WINDOW_LOG: u32 = 27;

/// Lowest level whose matcher actually runs long-distance matching. Below it
/// the encoder uses a strategy that carries no long-distance producer, so
/// `--long` would be a wider window and nothing else.
const MIN_LONG_LEVEL: i32 = 16;

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

/// Read the leading unsigned number the way upstream's argument reader does
/// (`readU32FromCharChecked`, zstdcli.c:350-376): a run of decimal digits,
/// then an optional `K` or `M` multiplier which may be spelled `KiB` / `MB`.
/// Reading STOPS there and the remainder is handed back — no sign is accepted,
/// and an empty digit run reads as zero, which every caller treats as invalid.
fn read_leading_u32(text: &str) -> Result<(u32, &str)> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut value: u32 = 0;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u32::from(bytes[at] - b'0')))
            .ok_or_else(|| eyre!("numeric value `{text}` overflows 32-bit unsigned int"))?;
        at += 1;
    }
    if at < bytes.len() && matches!(bytes[at], b'K' | b'M') {
        let shifts = if bytes[at] == b'M' { 2 } else { 1 };
        for _ in 0..shifts {
            value = value
                .checked_mul(1024)
                .ok_or_else(|| eyre!("numeric value `{text}` overflows 32-bit unsigned int"))?;
        }
        at += 1;
        // `KiB` and `KB` are the same multiplier spelled longer.
        at += usize::from(bytes.get(at) == Some(&b'i'));
        at += usize::from(bytes.get(at) == Some(&b'B'));
    }
    Ok((value, &text[at..]))
}

/// Parse a `-M` / `--memory` value into bytes, or `None` for "the default".
///
/// The default unit is MiB, as upstream documents, so `-M256` is 256 MiB. A
/// suffix means what it says and is not multiplied again: `-M1M` is one
/// mebibyte, a limit far below what decompression needs, and has to be refused
/// as such rather than read as a terabyte that trivially passes.
///
/// Zero asks for no custom ceiling at all: the parameter contract says "value 0
/// means use default maximum windowLog" (`zstd.h`, `ZSTD_d_windowLogMax`), and
/// the default is what this build enforces anyway. Read as a limit of zero
/// bytes it would refuse every run, turning an explicit request for the default
/// into an error.
fn parse_memory_limit(text: &str) -> Result<Option<u64>> {
    let value = parse_size(text)?;
    let bytes = if text.trim().ends_with(|c: char| c.is_ascii_digit()) {
        value
            .checked_mul(1 << 20)
            .ok_or_else(|| eyre!("memory limit `{text}` MiB does not fit in 64 bits"))?
    } else {
        value
    };
    Ok((bytes != 0).then_some(bytes))
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
///
/// `whole_file_bytes` is what this run holds in full rather than streams, and
/// `copies` is how many forms of it exist at once: the `-D` dictionary is held
/// as read and again as parsed, a benchmark holds its input, the compressed
/// frame and the decompressed copy. Any of them can break the promise alone, so
/// they are weighed from the file sizes before anything is loaded.
fn check_memory_limit(requested: u64, whole_file_bytes: u64, copies: u64) -> Result<()> {
    /// Window ceiling the decoder refuses to exceed.
    const WINDOW: u64 = structured_zstd::decoding::MAXIMUM_ALLOWED_WINDOW_SIZE;
    /// What the decoder holds BESIDES the window, rounded well up: the literal
    /// and block buffers (a block is capped at 128 KiB each), the sequence
    /// storage, the Huffman and FSE tables, and the tool's own I/O buffers.
    /// Counted because the promise is about total memory, not about one
    /// allocation: a limit equal to the window alone is one we would break.
    const AUXILIARY: u64 = 1 << 20;
    // A size here is what a directory entry claims, not what was allocated, and
    // a sparse file can claim more than memory could ever hold. Arithmetic that
    // leaves the type is therefore a real input, not a theoretical one: a
    // wrapped total would accept a limit nothing could keep. Nothing that
    // overflows fits under any limit, so the failure is the ordinary refusal.
    let buffers = whole_file_bytes
        .checked_mul(copies)
        .and_then(|buffers| buffers.checked_add(WINDOW + AUXILIARY));
    let Some(floor) = buffers else {
        bail!(
            "requested memory limit {requested} B cannot cover this run: the files it \
             would hold add up to more than any machine can address."
        );
    };
    if requested < floor {
        if floor > WINDOW + AUXILIARY {
            // A caller that holds one thing in several forms says how many, and
            // the count is worth naming: it is usually the surprise. One that
            // has already added its buffers up passes 1, and then the count
            // says nothing worth reading.
            let held = if copies == 1 {
                String::new()
            } else {
                format!(", each held in {copies} forms at once")
            };
            bail!(
                "requested memory limit {requested} B does not cover this run: a {} MiB \
                 window, about {} MiB of literal, block, sequence and table buffers, and \
                 {} MiB of whole-file buffers{held}.",
                WINDOW / (1 << 20),
                AUXILIARY / (1 << 20),
                (floor - WINDOW - AUXILIARY) / (1 << 20),
            );
        }
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
    /// Boxed: the options are a few hundred bytes, and the other variant is
    /// nothing at all.
    Run(Box<Options>),
    Handled,
}

/// A command line that could not be parsed, with the display level the
/// flags before the mistake had reached: `-q --bogus` reports the mistake
/// alone, where the default level adds the short usage under it.
struct ParseFailure {
    error: Error,
    verbosity: i32,
}

fn main() {
    // `args_os`, not `args`: the latter panics on an argument that is not
    // UTF-8, which on Unix is a legitimate filename rather than a mistake.
    let raw: Vec<OsString> = std::env::args_os().collect();
    let prog = raw
        .first()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_else(|| "zstd".to_string());
    let preset = program_preset(&prog);
    // The environment is read before the command line, at the default
    // level: `-q` further along cannot silence a warning about a variable
    // that was already applied when it was met.
    let default_level = level_from_env(std::env::var_os("ZSTD_CLEVEL").as_deref(), DEFAULT_LEVEL);
    check_threads_env(std::env::var_os("ZSTD_NBTHREADS").as_deref(), DEFAULT_LEVEL);

    let options = match parse_args(&raw[1..], &preset, default_level) {
        Ok(Parsed::Run(options)) => *options,
        Ok(Parsed::Handled) => return,
        Err(failure) => {
            display!(failure.verbosity, 1, "zstd: {}", failure.error);
            if failure.verbosity >= DEFAULT_LEVEL {
                let mut stderr = io::stderr().lock();
                let _ = write_short_usage(&mut stderr, &preset.name);
            }
            std::process::exit(1);
        }
    };
    let verbosity = options.verbosity;
    // Status goes to stderr through `display!`, so it never contaminates a
    // `-c` stdout data stream.
    let status = match run(options) {
        Ok(0) => 0,
        Ok(_failed_inputs) => 1,
        Err(err) => {
            display!(verbosity, 1, "zstd: {err}");
            1
        }
    };
    std::process::exit(status);
}

/// What `argv[0]` presets before any flag is read: the conventional symlink
/// dispatch. `unzstd` decompresses; `zstdcat` and `zcat` decompress to
/// stdout, overwriting, passing non-zstd input through, and quietly, as the
/// reference command sets them up; `zstdmt` compresses like `zstd` (its
/// worker count has no effect here).
struct ProgramPreset {
    /// The name the tool was invoked by, as the usage text shows it.
    name: String,
    mode: Mode,
    to_stdout: bool,
    force: bool,
    pass_through: Option<bool>,
    verbosity: i32,
}

fn program_preset(prog: &str) -> ProgramPreset {
    let name = Path::new(prog)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(prog);
    // Strip a trailing `.exe` for Windows symlink names.
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    let plain = ProgramPreset {
        name: name.to_string(),
        mode: Mode::Compress,
        to_stdout: false,
        force: false,
        pass_through: None,
        verbosity: DEFAULT_LEVEL,
    };
    match stem {
        "unzstd" => ProgramPreset {
            mode: Mode::Decompress,
            ..plain
        },
        "zstdcat" | "zcat" => ProgramPreset {
            mode: Mode::Decompress,
            to_stdout: true,
            force: true,
            pass_through: Some(true),
            verbosity: 1,
            ..plain
        },
        // "zstd", "zstdmt", anything else: compress by default.
        _ => plain,
    }
}

/// The default compression level, from `ZSTD_CLEVEL` when it is set.
///
/// Read the way the reference command reads it: an optional sign, then the
/// leading unsigned number with its `K`/`M` multiplier. A value that is not
/// that is ignored with a warning rather than failing the run, since an
/// environment variable is set far from the command that trips over it. The
/// variable replaces the DEFAULT only; `-#` on the command line still wins.
fn level_from_env(value: Option<&OsStr>, verbosity: i32) -> i32 {
    let Some(value) = value else {
        return CompressionLevel::DEFAULT_LEVEL;
    };
    let text = value.to_string_lossy();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(&text)),
    };
    if digits.starts_with(|c: char| c.is_ascii_digit()) {
        match read_leading_u32(digits) {
            Ok((magnitude, "")) => {
                // The scale runs from `MIN_LEVEL` up; the library clamps a
                // value below it, and the command line's own ceiling reduces
                // one above 19 with the usual warning later.
                return i32::try_from(magnitude)
                    .map(|magnitude| sign * magnitude)
                    .unwrap_or(if sign < 0 { i32::MIN } else { i32::MAX })
                    .max(CompressionLevel::MIN_LEVEL);
            }
            Err(_) => {
                display!(
                    verbosity,
                    2,
                    "Ignore environment variable setting ZSTD_CLEVEL={text}: numeric value too large"
                );
                return CompressionLevel::DEFAULT_LEVEL;
            }
            Ok(_) => {}
        }
    }
    display!(
        verbosity,
        2,
        "Ignore environment variable setting ZSTD_CLEVEL={text}: not a valid integer value"
    );
    CompressionLevel::DEFAULT_LEVEL
}

/// Validate `ZSTD_NBTHREADS` the way the reference command does, warning
/// about a value that is not an unsigned number. The count itself has no
/// effect here: compression runs single-threaded, which is also what the
/// reference command does when built without threads.
fn check_threads_env(value: Option<&OsStr>, verbosity: i32) {
    let Some(value) = value else {
        return;
    };
    let text = value.to_string_lossy();
    if text.starts_with(|c: char| c.is_ascii_digit()) {
        match read_leading_u32(&text) {
            Ok((_, "")) => return,
            Err(_) => {
                display!(
                    verbosity,
                    2,
                    "Ignore environment variable setting ZSTD_NBTHREADS={text}: numeric value too large"
                );
                return;
            }
            Ok(_) => {}
        }
    }
    display!(
        verbosity,
        2,
        "Ignore environment variable setting ZSTD_NBTHREADS={text}: not a valid unsigned value"
    );
}

/// The value of a long option that takes one, given attached
/// (`--name=value`) or as the next argument (`--name value`), the way the
/// reference command's `NEXT_FIELD` reads it. `None` when `long` is not this
/// option at all. The next argument may not start with `-`: an option there
/// means the value was left out, and reading the option as the value would
/// hide the mistake.
fn option_value<'a>(
    long: &str,
    name: &str,
    arg_os: &OsStr,
    rest: &mut impl Iterator<Item = (usize, &'a OsString)>,
) -> Result<Option<PathBuf>> {
    if long == name {
        let Some((_, value)) = rest.next() else {
            bail!("error: missing command argument for --{name}");
        };
        if value.to_string_lossy().starts_with('-') {
            bail!("error: command cannot be separated from its argument by another command");
        }
        return Ok(Some(PathBuf::from(value)));
    }
    if long.len() > name.len() && long.starts_with(name) && long.as_bytes()[name.len()] == b'=' {
        // Past `--`, the name and the `=`, all ASCII.
        return Ok(Some(attached_path(arg_os, 2 + name.len() + 1)));
    }
    Ok(None)
}

/// [`option_value`] for an option whose value is a number or a word rather
/// than a path.
fn option_text<'a>(
    long: &str,
    name: &str,
    arg_os: &OsStr,
    rest: &mut impl Iterator<Item = (usize, &'a OsString)>,
) -> Result<Option<String>> {
    Ok(option_value(long, name, arg_os, rest)?
        .map(|value| value.as_os_str().to_string_lossy().into_owned()))
}

/// [`option_text`] for an option spelled several ways (`--memory`,
/// `--memlimit`): the first spelling that matches supplies the value.
fn first_option_text<'a>(
    long: &str,
    names: &[&str],
    arg_os: &OsStr,
    rest: &mut impl Iterator<Item = (usize, &'a OsString)>,
) -> Result<Option<String>> {
    for name in names {
        if let Some(value) = option_text(long, name, arg_os, rest)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// Manual upstream-style parse: bare `-N` is a level, short flags combine
/// (`-dc`), `-o`/`-D` take a value, `--long-opts` are matched whole. `clap`'s
/// derive cannot model bare numeric levels, so we parse argv directly.
/// `default_level` is what `-#` overrides: the built-in default, or
/// `ZSTD_CLEVEL`.
fn parse_args(
    args: &[OsString],
    preset: &ProgramPreset,
    default_level: i32,
) -> Result<Parsed, ParseFailure> {
    let mut verbosity = preset.verbosity;
    parse_args_into(args, preset, default_level, &mut verbosity)
        .map_err(|error| ParseFailure { error, verbosity })
}

fn parse_args_into(
    args: &[OsString],
    preset: &ProgramPreset,
    default_level: i32,
    verbosity: &mut i32,
) -> Result<Parsed> {
    let mut opts = Options {
        mode: preset.mode,
        level: default_level,
        store: false,
        dict: None,
        to_stdout: preset.to_stdout,
        output: None,
        force: preset.force,
        keep: false,
        remove_source: false,
        inputs: Vec::new(),
        max_dict: DEFAULT_MAX_DICT,
        dict_id: None,
        bench: false,
        bench_start: default_level,
        bench_end: 0,
        bench_secs: 1.0,
        bench_separately: false,
        long: false,
        long_window_log: None,
        memory_limit: None,
        target_block_size: None,
        pledged_size: None,
        size_hint: None,
        verbosity: preset.verbosity,
        checksum: true,
        content_size_flag: true,
        dict_id_flag: true,
        pass_through: preset.pass_through,
        exclude_compressed: false,
        recursive: false,
        follow_links: preset.force,
        force_stdin: false,
        filelists: Vec::new(),
        output_dir: None,
        output_dir_mirror: None,
        progress: Progress::Auto,
    };
    let mut ultra = false;
    let mut iter = args.iter().enumerate().peekable();
    let mut positional_only = false;

    while let Some((idx, arg_os)) = iter.next() {
        // Option spellings and their numeric values are text; a filename is
        // bytes. Matching happens on a lossy view, while anything kept as a
        // path keeps the argument itself, so a name that is not UTF-8 reaches
        // the filesystem as it was given.
        let arg = arg_os.to_string_lossy();
        let arg = arg.as_ref();
        if positional_only || arg == "-" || !arg.starts_with('-') {
            opts.inputs.push(PathBuf::from(arg_os));
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "compress" => select_mode(&mut opts, Mode::Compress),
                "decompress" | "uncompress" => select_mode(&mut opts, Mode::Decompress),
                "test" => select_mode(&mut opts, Mode::Test),
                "list" => select_mode(&mut opts, Mode::List),
                // Plain `--train` selects the same default upstream does,
                // FastCOVER, so the two spellings agree.
                "train" | "train-fastcover" => select_mode(&mut opts, Mode::Train),
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
                // `-f` disables every input and output check at once, as the
                // reference command's does: overwriting, a terminal on either
                // end, and symbolic links.
                "force" => {
                    opts.force = true;
                    opts.force_stdin = true;
                    opts.follow_links = true;
                }
                "keep" => opts.keep = true,
                "rm" => opts.remove_source = true,
                "ultra" => ultra = true,
                "quiet" => *verbosity -= 1,
                "verbose" => *verbosity += 1,
                // The wire-format switches: the checksum, the
                // Frame_Content_Size field, the Dictionary_ID. Each reaches
                // the encoder, so the frame that comes out is the one asked
                // for.
                "check" => opts.checksum = true,
                "no-check" => opts.checksum = false,
                "content-size" => opts.content_size_flag = true,
                "no-content-size" => opts.content_size_flag = false,
                "no-dictID" => opts.dict_id_flag = false,
                "pass-through" => opts.pass_through = Some(true),
                "no-pass-through" => opts.pass_through = Some(false),
                "exclude-compressed" => opts.exclude_compressed = true,
                "progress" => opts.progress = Progress::Always,
                "no-progress" => opts.progress = Progress::Never,
                "version" => {
                    print_version(*verbosity);
                    return Ok(Parsed::Handled);
                }
                "help" => {
                    print_help(*verbosity, &preset.name);
                    return Ok(Parsed::Handled);
                }
                // Flags that steer HOW the work is done, not what comes out:
                // thread counts, IO strategy, matcher hints. We are
                // single-threaded and pick our own limits, so accepting them
                // yields the same valid stream. Upstream takes them, so a
                // script that passes them must not fail here: that is the
                // whole drop-in contract.
                "single-thread"
                | "adapt"
                | "sparse"
                | "no-sparse"
                | "asyncio"
                | "no-asyncio"
                | "mmap-dict"
                | "no-mmap-dict"
                | "row-match-finder"
                | "no-row-match-finder" => {}
                // Forces literals compressed or stored, which changes the
                // frame that comes out. The encoder has no such switch here,
                // so accepting the flag would hand back the other layout.
                "compress-literals" | "no-compress-literals" => {
                    bail!("--{long} is not implemented");
                }
                _ => {
                    if long == "fast" {
                        // `--fast` is the level -1 alias.
                        opts.level = -1;
                    } else if let Some(v) = long.strip_prefix("fast=") {
                        // `--fast=N` is level -N (upstream zstd,
                        // zstdcli.c:1133-1153). The factor is the LEADING
                        // number only, so `--fast=3.5` is level -3 and the tail
                        // is dropped; a factor past the minimum level clamps
                        // rather than failing; only a zero factor is an error.
                        // Exact-match the prefix so a typo like `--faster`
                        // falls through to unknown-option.
                        let (n, _tail) = read_leading_u32(v).wrap_err("invalid --fast level")?;
                        // Zero would negate to level 0, which is the ordinary
                        // default rather than a fast one.
                        if n == 0 {
                            bail!("--fast level must be at least 1, got 0");
                        }
                        let capped = n.min(CompressionLevel::MIN_LEVEL.unsigned_abs());
                        opts.level = -i32::try_from(capped)
                            .expect("capped at |MIN_LEVEL|, which is an i32 magnitude");
                    } else if long.starts_with("use-dict=") {
                        opts.dict = Some(attached_path(arg_os, "--use-dict=".len()));
                    } else if let Some(v) = option_text(long, "maxdict", arg_os, &mut iter)? {
                        opts.max_dict = v.parse::<usize>().wrap_err("invalid --maxdict size")?;
                    } else if let Some(v) = option_text(long, "dictID", arg_os, &mut iter)? {
                        // Zero is how the dictionary API spells "choose one for
                        // me", so it selects the default rather than being
                        // carried through as an id the trainer would refuse.
                        let id = v.parse::<u32>().wrap_err("invalid --dictID")?;
                        opts.dict_id = (id != 0).then_some(id);
                    } else if let Some(v) = option_text(long, "stream-size", arg_os, &mut iter)? {
                        // An exact pledge: it goes into the frame header, so a
                        // stream of a different length is an error.
                        opts.pledged_size = Some(parse_size(&v).wrap_err("invalid --stream-size")?);
                    } else if let Some(v) = option_text(long, "size-hint", arg_os, &mut iter)? {
                        // An estimate: it sizes the encoder and nothing else,
                        // so a wrong guess costs ratio rather than failing.
                        opts.size_hint = Some(parse_size(&v).wrap_err("invalid --size-hint")?);
                    } else if let Some(v) = first_option_text(
                        long,
                        &["memory", "memlimit", "memlimit-decompress"],
                        arg_os,
                        &mut iter,
                    )? {
                        // Recorded now, checked once the mode is final: the
                        // ceiling describes decoding, and a later flag can
                        // still decide this run does none.
                        opts.memory_limit =
                            parse_memory_limit(&v).wrap_err("invalid memory limit")?;
                    } else if let Some(params) = long.strip_prefix("adapt=") {
                        // Parameterised form (`--adapt=min=1,max=9`). We do not
                        // vary the level, so the bounds change nothing, but a
                        // misspelled key or a non-numeric bound is still a
                        // broken command line, and the contract is that ignored
                        // options validate what they are given.
                        parse_adapt_params(params)?;
                    } else if let Some(v) = option_text(long, "auto-threads", arg_os, &mut iter)? {
                        // Single-threaded: the choice has no effect, but a bad
                        // value is still a bad command line.
                        if v != "physical" && v != "logical" {
                            bail!("--auto-threads must be `physical` or `logical`, got `{v}`");
                        }
                    } else if let Some(v) =
                        option_text(long, "target-compressed-block-size", arg_os, &mut iter)?
                    {
                        let target =
                            parse_size(&v).wrap_err("invalid --target-compressed-block-size")?;
                        opts.target_block_size = Some(u32::try_from(target).map_err(|_| {
                            eyre!("--target-compressed-block-size={v} is too large")
                        })?);
                    } else if let Some(v) = option_text(long, "threads", arg_os, &mut iter)? {
                        let _ = v.parse::<u32>().wrap_err("invalid --threads")?;
                    } else if let Some(v) = option_text(long, "block-size", arg_os, &mut iter)? {
                        // The job size of a multi-threaded run: nothing here,
                        // but a malformed size is still a broken command line.
                        parse_size(&v).wrap_err("invalid --block-size")?;
                    } else if let Some(list) = option_value(long, "filelist", arg_os, &mut iter)? {
                        opts.filelists.push(list);
                    } else if let Some(dir) =
                        option_value(long, "output-dir-flat", arg_os, &mut iter)?
                    {
                        if dir.as_os_str().is_empty() {
                            bail!(
                                "error: output dir cannot be empty string (did you mean to pass '.' instead?)"
                            );
                        }
                        opts.output_dir = Some(dir);
                    } else if let Some(dir) =
                        option_value(long, "output-dir-mirror", arg_os, &mut iter)?
                    {
                        if dir.as_os_str().is_empty() {
                            bail!(
                                "error: output dir cannot be empty string (did you mean to pass '.' instead?)"
                            );
                        }
                        opts.output_dir_mirror = Some(dir);
                    } else if let Some(v) = long.strip_prefix("format=") {
                        // Anything but zstd would hand back a file the caller
                        // did not ask for, so it fails rather than silently
                        // producing a `.zst` under a `.gz` name.
                        if v != "zstd" {
                            bail!("--format={v} is not supported; this build only writes zstd");
                        }
                    } else if long == "rsyncable" || long.starts_with("patch-from") {
                        // Both change the emitted frame, so silence would be a
                        // wrong answer rather than a slower one.
                        bail!("--{long} is not implemented");
                    } else if long == "long" {
                        // Bare `--long` is `--long=27` upstream. The window is
                        // the point of the flag, so leaving the level's own one
                        // would reach back nowhere near the asked-for distance.
                        opts.long = true;
                        opts.long_window_log = Some(DEFAULT_LONG_WINDOW_LOG);
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
                'd' => select_mode(&mut opts, Mode::Decompress),
                'z' => select_mode(&mut opts, Mode::Compress),
                't' => select_mode(&mut opts, Mode::Test),
                'l' => select_mode(&mut opts, Mode::List),
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
                'f' => {
                    opts.force = true;
                    opts.force_stdin = true;
                    opts.follow_links = true;
                }
                'k' => opts.keep = true,
                // `-S` measures each input on its own.
                'S' => opts.bench_separately = true,
                'q' => *verbosity -= 1,
                'v' => *verbosity += 1,
                'C' => opts.checksum = true,
                'r' => opts.recursive = true,
                'B' | 'T' => {
                    // `-B[N]` job / block size, `-T[N]` thread count. Both
                    // steer how the work is done, not what comes out: we use a
                    // fixed block size and run single-threaded. Upstream
                    // accepts them, so a script that passes them must not fail
                    // here — but the VALUE is still parsed: ignoring what a
                    // flag does is not a reason to ignore what it says, and a
                    // typo is a broken command line either way. A size takes a
                    // size suffix; a thread count is a plain count, the way
                    // `--threads=` reads it.
                    let rest: String = chars[ci + 1..].iter().collect();
                    if !rest.is_empty() {
                        if c == 'B' {
                            parse_size(&rest).wrap_err("invalid -B value")?;
                        } else {
                            rest.parse::<u32>().wrap_err("invalid -T thread count")?;
                        }
                    }
                    ci = chars.len();
                    continue;
                }
                'M' => {
                    // `-M[N]` is a decompression memory ceiling — a safety
                    // promise, so it is checked against the one this build
                    // enforces rather than swallowed.
                    //
                    // A bare `-M` sets no ceiling, which is what upstream does
                    // with it: the value is attached or it is nothing, and the
                    // next argument stays a filename (`zstd -d -M 8 f.zst`
                    // reads `8` as a file). Erroring here would refuse a
                    // command line the reference tool accepts.
                    let rest: String = chars[ci + 1..].iter().collect();
                    if !rest.is_empty() {
                        opts.memory_limit =
                            parse_memory_limit(&rest).wrap_err("invalid -M memory limit")?;
                    }
                    ci = chars.len();
                    continue;
                }
                'V' => {
                    print_version(*verbosity);
                    return Ok(Parsed::Handled);
                }
                'H' => {
                    print_help(*verbosity, &preset.name);
                    return Ok(Parsed::Handled);
                }
                'h' => {
                    let mut stdout = io::stdout().lock();
                    write_short_usage(&mut stdout, &preset.name)
                        .wrap_err("failed to write usage")?;
                    return Ok(Parsed::Handled);
                }
                'D' | 'o' => {
                    // Value is the rest of this token, or the next argument.
                    // Either way it comes off the original argument, so a path
                    // that is not UTF-8 reaches the filesystem as given. The
                    // flags scanned so far are all ASCII, one byte each, so the
                    // value starts at `ci + 2`: the leading `-` plus them.
                    let value = if ci + 1 == chars.len() {
                        iter.next()
                            .map(|(_, v)| PathBuf::from(v))
                            .ok_or_else(|| eyre!("option -{c} requires a value"))?
                    } else {
                        attached_path(arg_os, ci + 2)
                    };
                    if c == 'D' {
                        opts.dict = Some(value);
                    } else {
                        // Clears `-c`, so the later of the two wins.
                        opts.output = Some(value);
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
    opts.verbosity = *verbosity;

    // `-M` bounds decompression, so it is weighed only on the runs that decode.
    // Compressing, listing or training allocates no decoder, and upstream takes
    // the flag there without complaint. A mode flag may follow the limit, which
    // is why this waits for the whole command line. The `-D` dictionary is
    // weighed on top in `run`, once its size is known.
    if let Some(limit) = opts.memory_limit
        && decodes(&opts)
    {
        check_memory_limit(limit, 0, 0)?;
    }
    validate_level(opts.level)?;
    if opts.bench && opts.bench_end < opts.bench_start {
        opts.bench_end = opts.bench_start;
    }
    // The levels 20-22 are expensive enough that upstream asks for them by
    // name. Benchmarking compresses the range `-b`/`-e` give rather than the
    // level `-N` set, so the gate reads the range's top when there is one —
    // `-b20` reaches an ultra level as surely as `-20` does.
    let (lowest_level, highest_level) = if opts.bench {
        (
            opts.bench_start.min(opts.bench_end),
            opts.bench_start.max(opts.bench_end),
        )
    } else {
        (opts.level, opts.level)
    };
    // Both ends run, so both are checked here rather than at the level that
    // reaches them: a range starting below the scale would otherwise stat and
    // read every input before the first pass refused it.
    validate_level(lowest_level)?;
    validate_level(highest_level)?;
    // Unnamed, an ultra level is not refused but reduced, with a warning, the
    // way upstream reduces it — a script that runs `zstd -22` compresses at 19
    // rather than failing, and refusing here is what would break it.
    if !ultra && highest_level > CLI_MAX_LEVEL_WITHOUT_ULTRA {
        display!(
            *verbosity,
            2,
            "Warning : compression level higher than max, reduced to {CLI_MAX_LEVEL_WITHOUT_ULTRA}"
        );
        opts.level = opts.level.min(CLI_MAX_LEVEL_WITHOUT_ULTRA);
        opts.bench_start = opts.bench_start.min(CLI_MAX_LEVEL_WITHOUT_ULTRA);
        opts.bench_end = opts.bench_end.min(CLI_MAX_LEVEL_WITHOUT_ULTRA);
    }
    // Long-distance matching runs on the optimal parser here, so below it the
    // flag would widen the window and never run the matcher it names. Settled
    // after the whole command line, since the level may follow the flag — and
    // read from the benchmark range when there is one, because those are the
    // levels that will actually compress. The whole range runs with the flag,
    // so its lowest level is the one that has to carry the matcher.
    let long_level = if opts.bench {
        opts.bench_start.min(opts.bench_end)
    } else {
        opts.level
    };
    if opts.long && compresses(&opts) && long_level < MIN_LONG_LEVEL {
        bail!(
            "--long needs level {MIN_LONG_LEVEL} or above, where long-distance \
             matching runs; at level {long_level} it would only widen the window",
        );
    }
    Ok(Parsed::Run(Box::new(opts)))
}

/// The part of `arg` from byte `at`, as the bytes it was given in.
///
/// An attached path — `-Dname`, `--use-dict=name` — is the same filename as one
/// passed separately and has to survive the same way, so it comes off the
/// original argument rather than off the lossy view the option spelling was
/// matched against. `at` must land after ASCII only, which every option
/// spelling is: a lossy conversion replaces non-ASCII sequences alone, so a
/// prefix that matched there is byte-for-byte the same at the front of `arg`.
fn attached_path(arg: &std::ffi::OsStr, at: usize) -> PathBuf {
    let bytes = arg.as_encoded_bytes();
    debug_assert!(
        bytes[..at].is_ascii(),
        "the split point must follow ASCII, or it is not a boundary"
    );
    // SAFETY: `bytes` came from an `OsStr` and is split at the end of an ASCII
    // run, so the remainder is a valid encoded `OsStr` — exactly the
    // precondition `from_encoded_bytes_unchecked` documents.
    let rest = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&bytes[at..]) };
    PathBuf::from(rest)
}

/// Select the operation, replacing whatever was chosen before it.
///
/// Benchmarking is a mode like any other from the command line's point of view,
/// even though it is carried in its own field, so naming an operation after
/// `-b` has to turn the benchmark off — the last flag typed is the one that
/// runs, which is how every other operation flag here behaves.
fn select_mode(opts: &mut Options, mode: Mode) {
    opts.mode = mode;
    opts.bench = false;
}

/// Whether this run will decode anything, and so whether `-M` binds it.
///
/// Not the same question as the mode: benchmarking keeps `Mode::Compress` and
/// still decompresses at every level it measures.
fn decodes(opts: &Options) -> bool {
    opts.bench || matches!(opts.mode, Mode::Decompress | Mode::Test)
}

/// Whether this run will compress anything, and so whether the encoder's own
/// flags have to hold. Benchmarking compresses whatever mode was asked for.
fn compresses(opts: &Options) -> bool {
    opts.bench || matches!(opts.mode, Mode::Compress)
}

fn validate_level(level: i32) -> Result<()> {
    let (min, max) = (CompressionLevel::MIN_LEVEL, CompressionLevel::MAX_LEVEL);
    if !(min..=max).contains(&level) {
        bail!("compression level {level} out of range [{min}, {max}]");
    }
    Ok(())
}

/// `--version`: the reference command version this tool follows, and this
/// build's own. Below the default display level (`-qV`) only the bare
/// version number is printed, for a script to read; at `-vV` the supported
/// formats follow, as they do there.
fn print_version(verbosity: i32) {
    if verbosity < DEFAULT_LEVEL {
        println!("{UPSTREAM_VERSION}");
        return;
    }
    println!(
        "zstd version {UPSTREAM_VERSION} (structured-zstd v{})",
        env!("CARGO_PKG_VERSION")
    );
    if verbosity >= 3 {
        println!("*** supports: zstd");
    }
}

/// The short usage: `-h`, and what a mistaken command line gets under its
/// error. Laid out as the reference command lays it out.
fn write_short_usage(out: &mut impl Write, program: &str) -> io::Result<()> {
    writeln!(
        out,
        "Compress or decompress the INPUT file(s); reads from STDIN if INPUT is `-` or not provided.\n"
    )?;
    writeln!(
        out,
        "Usage: {program} [OPTIONS...] [INPUT... | -] [-o OUTPUT]\n"
    )?;
    writeln!(out, "Options:")?;
    writeln!(
        out,
        "  -o OUTPUT                     Write output to a single file, OUTPUT."
    )?;
    writeln!(
        out,
        "  -k, --keep                    Preserve INPUT file(s). [Default]"
    )?;
    writeln!(
        out,
        "  --rm                          Remove INPUT file(s) after successful (de)compression.\n"
    )?;
    writeln!(
        out,
        "  -#                            Desired compression level, where `#` is a number between 1 and {CLI_MAX_LEVEL_WITHOUT_ULTRA};"
    )?;
    writeln!(
        out,
        "                                lower numbers provide faster compression, higher numbers yield"
    )?;
    writeln!(
        out,
        "                                better compression ratios. [Default: {}]\n",
        CompressionLevel::DEFAULT_LEVEL
    )?;
    writeln!(
        out,
        "  -d, --decompress              Perform decompression."
    )?;
    writeln!(
        out,
        "  -D DICT                       Use DICT as the dictionary for compression or decompression.\n"
    )?;
    writeln!(
        out,
        "  -f, --force                   Disable input and output checks. Allows overwriting existing files,"
    )?;
    writeln!(
        out,
        "                                receiving input from the console, printing output to STDOUT, and"
    )?;
    writeln!(
        out,
        "                                operating on links, block devices, etc. Unrecognized formats will be"
    )?;
    writeln!(
        out,
        "                                passed through as-is.\n"
    )?;
    writeln!(
        out,
        "  -h                            Display short usage and exit."
    )?;
    writeln!(
        out,
        "  -H, --help                    Display full help and exit."
    )?;
    writeln!(
        out,
        "  -V, --version                 Display the program version and exit.\n"
    )
}

/// The full help (`-H`, `--help`): the short usage, then every option this
/// build honours, then the ones it accepts without effect and the ones it
/// refuses, so a reader is not surprised by either.
fn print_help(verbosity: i32, program: &str) {
    print_version(verbosity.max(DEFAULT_LEVEL));
    println!();
    let mut stdout = io::stdout().lock();
    let _ = write_short_usage(&mut stdout, program);
    let _ = stdout.write_all(HELP_ADVANCED.as_bytes());
}

/// The part of the full help below the short usage.
const HELP_ADVANCED: &str = "\
Advanced options:
  -c, --stdout                  Write to STDOUT (even if it is a console) and keep the INPUT file(s).

  -v, --verbose                 Enable verbose output; pass multiple times to increase verbosity.
  -q, --quiet                   Suppress warnings; pass twice to suppress errors.

  --[no-]progress               Forcibly show/hide the progress counter. NOTE: Any (de)compressed
                                output to terminal will mix with progress counter text.

  -r                            Operate recursively on directories.
  --filelist LIST               Read a list of files to operate on from LIST.
  --output-dir-flat DIR         Store processed files in DIR.
  --output-dir-mirror DIR       Store processed files in DIR, respecting original directory structure.

  --[no-]check                  Add XXH64 integrity checksums during compression. [Default: Add, Validate]
                                If `-d` is present, ignore/validate checksums during decompression.

  --                            Treat remaining arguments after `--` as files.

Advanced compression options:
  --ultra                       Enable levels beyond 19, up to 22; requires more memory.
  --fast[=#]                    Use to very fast compression levels. [Default: 1]
  --long[=#]                    Enable long distance matching with window log #. [Default: 27]
                                Available from level 16 up, where long-distance matching runs;
                                capped at 27, the window this build can read back.
  --exclude-compressed          Only compress files that are not already compressed.

  --stream-size=#               Specify size of streaming input from STDIN.
  --size-hint=#                 Optimize compression parameters for streaming input of approximately size #.
  --target-compressed-block-size=#
                                Generate compressed blocks of approximately # size.

  --no-dictID                   Don't write `dictID` into the header (dictionary compression only).
  --[no-]content-size           Write the input size into the frame header when it is known. [Default: Write]

  --format=zstd                 Compress files to the `.zst` format. [Default]

Advanced decompression options:
  -l                            Print information about Zstandard-compressed files.
  --test                        Test compressed file integrity.
  -M#                           Set the memory usage limit to # megabytes.
  --[no-]pass-through           Pass through uncompressed files as-is. [Default: Disabled; Enabled for zstdcat]

Dictionary builder:
  --train                       Create a dictionary from a training set of files.
  --train-fastcover             Use the fast cover algorithm (the trainer --train also runs).
  -o NAME                       Use NAME as dictionary name. [Default: dictionary]
  --maxdict=#                   Limit dictionary to specified size #. [Default: 112640]
  --dictID=#                    Force dictionary ID to #. [Default: Random]

Benchmark options:
  -b#                           Perform benchmarking with compression level #. [Default: 3]
  -e#                           Test all compression levels up to #; starting level is `-b#`. [Default: 1]
  -i#                           Set the minimum evaluation to time # seconds. [Default: 1]
  -S                            Output one benchmark result per input file. [Default: Consolidated result]
  -D dictionary                 Benchmark using dictionary

Environment: ZSTD_CLEVEL sets the default compression level; ZSTD_NBTHREADS is read and validated.

Accepted for compatibility, with no effect here: -T#/--threads=#, --single-thread,
--auto-threads, -B#, --block-size=#, --adapt, --[no-]sparse, --[no-]asyncio,
--[no-]mmap-dict, --[no-]row-match-finder (compression runs single-threaded).

Rejected rather than ignored, because they would change the result: --format=
other than zstd, --patch-from, --rsyncable, --[no-]compress-literals,
--train-cover, --train-legacy, and -M/--memory below the enforced ceiling when
decoding. A new output file keeps its source's permissions.
";

/// Read the `-D` dictionary, if there is one, without breaking `-M` to do it.
///
/// The limit was already weighed against what decoding alone needs; the
/// dictionary is the other half of that promise. Its size comes from the
/// directory entry, so an oversized one is refused before it is read rather
/// than after the allocation the limit was supposed to prevent. The read is
/// then bounded by that same size, and a file that grew in between is an error
/// rather than a silent truncation, which would corrupt the dictionary.
fn load_dictionary(opts: &Options) -> Result<Option<Vec<u8>>> {
    let Some(path) = &opts.dict else {
        return Ok(None);
    };
    // Listing walks frame headers and training builds a dictionary from its
    // samples; neither consults the one `-D` names. Reading it anyway would
    // fail a listing over a missing file that has nothing to do with it, and
    // spend time and memory on a large one nothing looks at.
    if !opts.bench && matches!(opts.mode, Mode::List | Mode::Train) {
        return Ok(None);
    }
    // Asked of the path first, because opening a FIFO blocks until a writer
    // turns up and the point of the check is not to wait for one. Everything
    // the read depends on is then re-asked of the OPEN FILE below, since a path
    // is only a name: between this answer and the open it can be made to name
    // something else, and it is the thing actually read whose type and length
    // have to hold. (A swap to a FIFO inside that window still blocks in the
    // open; refusing to wait would need a non-blocking open, which needs a
    // platform flag this crate has no dependency to name.)
    let named = fs::metadata(path)
        .wrap_err_with(|| format!("failed to inspect dictionary file {}", path.display()))?;
    if !named.is_file() {
        bail!("-D needs a regular file: {} is not one", path.display());
    }
    // What the read will cost, answerable from a size alone. Asked of the name
    // before the file is opened — a dictionary that cannot fit the promise is
    // refused without spending a descriptor on it — and asked again below of
    // the file that was actually opened.
    let weigh = |size: u64| -> Result<usize> {
        // The size bounds the read, so it has to be a count of bytes this
        // machine can hold: a 64-bit length cast to a pointer-sized one
        // truncates on a 32-bit target, where 4 GiB would become a capacity of
        // zero that the read then grows into an allocation which aborts rather
        // than refuses.
        let capacity = usize::try_from(size).map_err(|_| {
            eyre!(
                "dictionary file {} is {size} bytes, more than this machine can address",
                path.display()
            )
        })?;
        if let Some(limit) = opts.memory_limit
            && decodes(opts)
        {
            // As read and again as parsed.
            check_memory_limit(limit, size, 2)?;
        }
        Ok(capacity)
    };
    weigh(named.len())?;

    let file = File::open(path)
        .wrap_err_with(|| format!("failed to open dictionary file {}", path.display()))?;
    // And asked again of the FILE, because a path is only a name: between the
    // answer above and this open it can be made to name something else, and it
    // is the thing actually read whose type and length have to hold. (A swap to
    // a FIFO inside that window still blocks in the open; refusing to wait
    // would need a non-blocking open, which needs a platform flag this crate
    // has no dependency to name.)
    let opened = file
        .metadata()
        .wrap_err_with(|| format!("failed to inspect dictionary file {}", path.display()))?;
    if !opened.is_file() {
        bail!("-D needs a regular file: {} is not one", path.display());
    }
    let size = opened.len();
    let capacity = weigh(size)?;

    let mut bytes = Vec::with_capacity(capacity);
    // One byte past the size the check cleared: reading it means the file grew.
    file.take(size + 1)
        .read_to_end(&mut bytes)
        .wrap_err_with(|| format!("failed to read dictionary file {}", path.display()))?;
    if bytes.len() as u64 > size {
        bail!("{} grew while it was being read; run again", path.display());
    }
    Ok(Some(bytes))
}

/// The `-D` dictionary in the forms the two codecs take, parsed once per run.
///
/// Each side turns the blob into its own tables, and priming happens per frame
/// — under `-b`, per timed iteration. Parsing there would put the dictionary's
/// own setup inside the measurement and report it as throughput, which for a
/// large dictionary over a small input is most of what the number would be.
/// So the blob is parsed here, once, and every frame attaches what came out.
#[derive(Default)]
struct Dictionaries {
    encoder: Option<structured_zstd::encoding::EncoderDictionary>,
    decoder: Option<structured_zstd::decoding::DictionaryHandle>,
}

impl Dictionaries {
    /// Parse the blob into the forms this run will use, and no others: a run
    /// that only compresses has no use for the decoder's tables. `-b` asks for
    /// both, since it measures the two directions in turn.
    ///
    /// An empty file is no dictionary rather than a broken one — loading a
    /// zero-size dictionary returns to no-dictionary mode — so `-D` on an empty
    /// file compresses plainly instead of failing.
    fn prepare(raw: Option<&[u8]>, for_compression: bool, for_decoding: bool) -> Result<Self> {
        let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
            return Ok(Self::default());
        };
        let mut prepared = Self::default();
        if for_compression {
            // Through the constructor that keeps the blob's own length, since
            // that is what the compression-parameter tier is chosen by —
            // parsing first and handing over the content would key it on the
            // wrong size. Whatever `-D` was pointed at: a trained dictionary,
            // or any file at all, taken as raw content the way upstream does.
            prepared.encoder = Some(
                structured_zstd::encoding::EncoderDictionary::from_serialized_or_raw_content(raw)
                    .map_err(|err| eyre!("invalid dictionary: {err:?}"))?,
            );
        }
        if for_decoding {
            prepared.decoder = Some(
                structured_zstd::decoding::DictionaryHandle::from_dictionary(
                    structured_zstd::decoding::Dictionary::from_serialized_or_raw_content(raw)
                        .map_err(|err| eyre!("failed to parse dictionary: {err:?}"))?,
                ),
            );
        }
        Ok(prepared)
    }
}

/// How the reference command names stdin in a summary line.
const STDIN_MARK: &str = "/*stdin*\\";

/// Whether the run reads stdin: no input named, or `-` among them.
fn reads_stdin(inputs: &[PathBuf]) -> bool {
    inputs.is_empty() || inputs.iter().any(|input| input == Path::new("-"))
}

/// Whether the run's data goes to stdout: `-c`, or stdin in and no `-o` out.
/// The reference command's `hasStdout`, which silences the result summary
/// and disables `--rm`.
fn writes_stdout(opts: &Options) -> bool {
    opts.to_stdout
        || (opts.output.is_none() && opts.inputs.iter().all(|input| input == Path::new("-")))
}

/// Run the command line. The count of inputs that failed comes back; the
/// exit status is 1 when it is not zero, as the reference command's is,
/// while an error that ends the run early is returned outright.
fn run(mut opts: Options) -> Result<usize> {
    let Selection { files, named } = inputs::select_inputs(
        std::mem::take(&mut opts.inputs),
        &opts.filelists,
        opts.recursive,
        opts.follow_links,
        opts.verbosity,
    )?;
    if files.is_empty() && named > 0 {
        // Pointed at empty directories: nothing to do, and not a request to
        // read stdin. The reference command says so and exits 0.
        display!(
            opts.verbosity,
            1,
            "please provide correct input file(s) or non-empty directories -- ignored"
        );
        return Ok(0);
    }
    opts.inputs = files;

    // Listing, training and benchmarking take named files only and refuse
    // stdin with their own reasons; the streaming modes read it, and refuse to
    // read it from a terminal unless forced, as the reference command does.
    let streams =
        matches!(opts.mode, Mode::Compress | Mode::Decompress | Mode::Test) && !opts.bench;
    if streams && reads_stdin(&opts.inputs) && !opts.force_stdin && io::stdin().is_terminal() {
        bail!("stdin is a console, aborting");
    }
    let has_stdout_output = matches!(opts.mode, Mode::Compress | Mode::Decompress)
        && !opts.bench
        && writes_stdout(&opts);
    // No status message by default when the data goes to stdout.
    if has_stdout_output && opts.verbosity == DEFAULT_LEVEL {
        opts.verbosity = 1;
    }
    // When stderr is not a terminal, do not pollute it with progress updates
    // unless asked.
    if !io::stderr().is_terminal() && opts.progress != Progress::Always {
        opts.progress = Progress::Never;
    }
    if has_stdout_output && opts.remove_source {
        display!(
            opts.verbosity,
            3,
            "Note: src files are not removed when output is stdout"
        );
        opts.remove_source = false;
    }
    if opts.mode == Mode::Test {
        opts.remove_source = false;
    }

    let dict_bytes = load_dictionary(&opts)?;

    // `-b` benchmarks compression/decompression across levels instead of
    // producing output files; handle it before the streaming flow. It takes the
    // blob rather than the parsed forms, since its own memory ceiling has to be
    // weighed before anything is built from them.
    if opts.bench {
        run_benchmark(&opts, dict_bytes)?;
        return Ok(0);
    }
    let dicts = Dictionaries::prepare(dict_bytes.as_deref(), compresses(&opts), decodes(&opts))?;
    // Everything from here on primes from the parsed form, so the blob it was
    // parsed out of is released rather than held for the length of the run
    // beside the thing that replaced it.
    drop(dict_bytes);

    // `--train` builds a dictionary from the sample files rather than
    // (de)compressing them; handle it before the streaming flow.
    if opts.mode == Mode::Train {
        train_dictionary(&opts)?;
        return Ok(0);
    }

    // `--list` walks frame headers without decoding; it needs a seekable file
    // (not a stream), so it is handled separately from the (de)compress flow.
    if opts.mode == Mode::List {
        return list_files(&opts);
    }

    // A destination named outright belongs to the whole run, whatever it reads:
    // stdin, an explicit `-`, or files. The dictionary is what the frame being
    // written will need to be read back, so an `-o` pointing at it destroys the
    // key to the archive it is producing. Checked here rather than in the
    // per-input scan below, which sees neither stdin nor `-`.
    //
    // Compression only. Decompressing has already finished with the dictionary
    // by the time anything is written, and what it writes is plaintext that
    // never needed it: the reference command permits that, and refusing would
    // break a working script to protect nothing.
    if let (Some(output), Some(dict)) = (&opts.output, &opts.dict)
        && !opts.to_stdout
        && opts.mode == Mode::Compress
        && names_the_same_file(output, dict)?
    {
        bail!(
            "{} would be written over the dictionary {}",
            output.display(),
            dict.display(),
        );
    }
    let total = opts.inputs.len().max(1);
    match (opts.to_stdout, &opts.output) {
        (true, _) => process_concatenated(&opts, &dicts, None, total),
        // `-t` writes nothing, so a destination it was given is set aside.
        (false, Some(output)) if opts.mode != Mode::Test => {
            process_concatenated(&opts, &dicts, Some(output), total)
        }
        _ => process_separately(&opts, &dicts, total),
    }
}

/// Every input into one destination: stdout, or the `-o` file.
///
/// Several inputs concatenated lose their names and boundaries, so the
/// reference command warns, disables `--rm`, and, for a file it was not
/// forced to write, asks first. An input that cannot be opened is reported
/// and skipped; one that fails while streaming ends the run, since a partial
/// frame would already be in the shared output.
fn process_concatenated(
    opts: &Options,
    dicts: &Dictionaries,
    output: Option<&Path>,
    total: usize,
) -> Result<usize> {
    let mut remove_source = opts.remove_source;
    if total > 1 {
        match output {
            None => display!(
                opts.verbosity,
                2,
                "zstd: WARNING: all input files will be processed and concatenated into stdout."
            ),
            Some(output) => display!(
                opts.verbosity,
                2,
                "zstd: WARNING: all input files will be processed and concatenated into a single output file: {}",
                output.display()
            ),
        }
        display!(
            opts.verbosity,
            2,
            "The concatenated output CANNOT regenerate original file names nor directory structure."
        );
        if remove_source {
            display!(
                opts.verbosity,
                2,
                "Since it's a destructive operation, input files will not be removed."
            );
            remove_source = false;
        }
        if output.is_some() && !opts.force {
            if opts.verbosity <= 1 {
                // Quiet mode: no prompt is possible, so the run refuses.
                display!(
                    opts.verbosity,
                    1,
                    "Concatenating multiple processed inputs into a single output loses file metadata."
                );
                display!(opts.verbosity, 1, "Aborting.");
                return Ok(total);
            }
            if !confirm(
                "Proceed? (y/n): ",
                "Aborting...",
                reads_stdin(&opts.inputs),
                &mut io::stdin().lock(),
            ) {
                return Ok(total);
            }
        }
    }
    let mut tally = Tally::default();
    let inputs: Vec<&Path> = if opts.inputs.is_empty() {
        vec![Path::new("-")]
    } else {
        opts.inputs.iter().map(PathBuf::as_path).collect()
    };
    match output {
        None => {
            let stdout = io::stdout();
            // Only compression produces binary; `-d` to a terminal is text the
            // user asked for, which the reference command also allows.
            if opts.mode == Mode::Compress {
                guard_binary_stdout(stdout.is_terminal(), opts.force)?;
            }
            let mut sink = stdout.lock();
            for input in inputs {
                let outcome = stream_input_to(opts, dicts, input, &mut sink, STDOUT_MARK, total)?;
                tally.record(outcome);
            }
        }
        Some(output) => {
            // One input keeps the reference command's single-file path, where
            // the output takes the source's permissions; several inputs share
            // an output that takes none of theirs.
            if let [input] = inputs[..] {
                let (source, metadata) = if input == Path::new("-") {
                    (None, None)
                } else {
                    match open_input(opts, input) {
                        Ok(Some((source, metadata))) => (Some(source), Some(metadata)),
                        Ok(None) => return Ok(0),
                        Err(err) => {
                            display!(opts.verbosity, 1, "zstd: {err}");
                            return Ok(1);
                        }
                    }
                };
                let name = if source.is_some() {
                    input.display().to_string()
                } else {
                    STDIN_MARK.to_string()
                };
                let written =
                    write_output_file(opts, output, metadata.as_ref(), |sink| match source {
                        Some(source) => stream_opened(
                            opts,
                            dicts,
                            source,
                            metadata.as_ref().expect("an opened input has metadata"),
                            sink,
                        ),
                        None => stream_stdin(opts, dicts, sink),
                    })?;
                match written {
                    Some(processed) => {
                        file_summary(
                            opts,
                            total,
                            &name,
                            &output.display().to_string(),
                            &processed,
                        );
                        if remove_source && input != Path::new("-") {
                            remove_source_if_requested(opts, input)?;
                        }
                        tally.record(Outcome::Done(processed));
                    }
                    None => tally.record(Outcome::Refused),
                }
            } else {
                let written = write_output_file(opts, output, None, |sink| {
                    let mut tally = Tally::default();
                    for input in &inputs {
                        let outcome = stream_input_to(
                            opts,
                            dicts,
                            input,
                            &mut *sink,
                            &output.display().to_string(),
                            total,
                        )?;
                        tally.record(outcome);
                    }
                    Ok(tally)
                })?;
                match written {
                    Some(inner) => tally = inner,
                    None => tally.failed = total,
                }
            }
        }
    }
    multi_summary(opts, total, &tally);
    Ok(tally.failed)
}

/// Every input into its own output, placed beside it or under the output
/// directory; stdin, when it is among them, goes to stdout. An input that
/// fails is reported and the rest are still processed, as the reference
/// command does; the count that failed decides the exit status.
fn process_separately(opts: &Options, dicts: &Dictionaries, total: usize) -> Result<usize> {
    // The inputs are processed one after another, so an output derived from an
    // early one can land on a file still waiting its turn: `-f foo foo.zst`
    // would replace `foo.zst` before it is ever read. The `-D` dictionary is a
    // file this run needs too, and the one a frame will need to be read back,
    // so writing over it destroys the key to what was just produced. `-f`
    // permits overwriting the output, not destroying either, so everything the
    // run reads is checked before the first byte is written.
    //
    // Only for the modes that write one. Testing decodes into a sink and names
    // no destination, so asking what it would produce has no answer. An input
    // whose output cannot be derived is left for the loop below to report.
    if matches!(opts.mode, Mode::Compress | Mode::Decompress) {
        for input in &opts.inputs {
            if input == Path::new("-") {
                continue;
            }
            let Ok(output) = derive_output_path(opts, input) else {
                continue;
            };
            // Compared as files rather than as spellings: `foo.zst`,
            // `./foo.zst` and `dir/../dir/foo.zst` name one file, and a match
            // on the string alone would miss two of the three. Compression
            // only, for the reason given at the `-o` check in `run`.
            if let Some(dict) = &opts.dict
                && opts.mode == Mode::Compress
                && names_the_same_file(&output, dict)?
            {
                bail!(
                    "{} would be written over the dictionary {}",
                    input.display(),
                    dict.display(),
                );
            }
            // `--rm` deletes an input once its output is written, and an input
            // that is itself the dictionary is the one file that output cannot
            // be read back without: `--rm -D data data` would leave an archive
            // nothing can open. Removing the source is a convenience, so it
            // yields to the file that gives the result meaning. Both files are
            // there to be compared, so identity is asked of the filesystem as
            // well: a hard link is a second name for one file, and no amount of
            // resolving either name tells them apart.
            if let Some(dict) = &opts.dict
                && opts.remove_source
                && !opts.keep
                && (names_the_same_file(input, dict)?
                    || (dict.exists() && paths_point_to_same_file(input, dict)?))
            {
                bail!(
                    "--rm would delete {}, which is also the dictionary {} needed to read the result",
                    input.display(),
                    dict.display(),
                );
            }
            for other in &opts.inputs {
                if other != Path::new("-") && names_the_same_file(&output, other)? {
                    bail!(
                        "{} would be written over {}, which is also an input",
                        input.display(),
                        other.display(),
                    );
                }
            }
        }
    }
    let mut tally = Tally::default();
    if opts.inputs.is_empty() {
        let outcome = stream_input_to(
            opts,
            dicts,
            Path::new("-"),
            io::stdout().lock(),
            STDOUT_MARK,
            total,
        )?;
        tally.record(outcome);
    }
    for input in &opts.inputs {
        let outcome = if input == Path::new("-") {
            stream_input_to(opts, dicts, input, io::stdout().lock(), STDOUT_MARK, total)?
        } else {
            match process_file(opts, input, dicts, total) {
                Ok(outcome) => outcome,
                Err(err) => {
                    display!(opts.verbosity, 1, "zstd: {err}");
                    Outcome::Refused
                }
            }
        };
        tally.record(outcome);
    }
    // Under `--output-dir-flat` two inputs with one name land on one output,
    // the later replacing the earlier; the reference command warns after the
    // run, once per shared name.
    if opts.output_dir.is_some() && opts.output_dir_mirror.is_none() {
        for name in inputs::shared_file_names(&opts.inputs) {
            display!(
                opts.verbosity,
                2,
                "WARNING: Two files have same filename: {}",
                Path::new(&name).display()
            );
        }
    }
    multi_summary(opts, total, &tally);
    Ok(tally.failed)
}

/// What became of one input.
enum Outcome {
    /// Processed, with what went in and came out.
    Done(Processed),
    /// Deliberately left alone (`--exclude-compressed`); not a failure.
    Skipped,
    /// Not processed, and already reported.
    Refused,
}

/// Bytes an input contributed: read from it, and written for it.
struct Processed {
    read: u64,
    written: u64,
}

/// The run's running totals, for the multi-file summary and the exit status.
#[derive(Default)]
struct Tally {
    processed: usize,
    failed: usize,
    read: u64,
    written: u64,
}

impl Tally {
    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Done(processed) => {
                self.processed += 1;
                self.read += processed.read;
                self.written += processed.written;
            }
            Outcome::Skipped => {}
            Outcome::Refused => self.failed += 1,
        }
    }
}

/// The per-file result line, in the reference command's layout, shown for a
/// single input or under `-v` for each of several.
fn file_summary(
    opts: &Options,
    total: usize,
    name: &str,
    destination: &str,
    processed: &Processed,
) {
    if total > 1 && opts.verbosity < 3 {
        return;
    }
    let verbose = opts.verbosity > 3;
    match opts.mode {
        Mode::Compress => {
            let read = HumanSize::new(processed.read, verbose);
            let written = HumanSize::new(processed.written, verbose);
            if processed.read == 0 {
                display!(
                    opts.verbosity,
                    2,
                    "{name:<20} :  ({read:>6} => {written:>6}, {destination})"
                );
            } else {
                display!(
                    opts.verbosity,
                    2,
                    "{name:<20} :{:>6.2}%   ({read:>6} => {written:>6}, {destination})",
                    processed.written as f64 / processed.read as f64 * 100.0
                );
            }
        }
        Mode::Decompress | Mode::Test => {
            display!(opts.verbosity, 2, "{name:<20}: {} bytes", processed.written);
        }
        Mode::List | Mode::Train => {}
    }
}

/// The closing line of a run over several inputs, when at least one went
/// through.
fn multi_summary(opts: &Options, total: usize, tally: &Tally) {
    if tally.processed < 1 || total <= 1 {
        return;
    }
    let verbose = opts.verbosity > 3;
    match opts.mode {
        Mode::Compress => {
            let read = HumanSize::new(tally.read, verbose);
            let written = HumanSize::new(tally.written, verbose);
            if tally.read == 0 {
                display!(
                    opts.verbosity,
                    2,
                    "{:>3} files compressed : ({} => {})",
                    tally.processed,
                    read.columns(6),
                    written.columns(6)
                );
            } else {
                display!(
                    opts.verbosity,
                    2,
                    "{:>3} files compressed : {:.2}% ({} => {})",
                    tally.processed,
                    tally.written as f64 / tally.read as f64 * 100.0,
                    read.columns(6),
                    written.columns(6)
                );
            }
        }
        Mode::Decompress | Mode::Test => {
            display!(
                opts.verbosity,
                2,
                "{} files decompressed : {:>6} bytes total",
                tally.processed,
                tally.written
            );
        }
        Mode::List | Mode::Train => {}
    }
}

/// `-b`: benchmark compression + decompression of the input across the
/// requested level range, reporting ratio and best-of throughput. A simplified
/// `zstd -b#` (per-level row); honours `-D` so dictionary throughput can be
/// measured. Time-budgeted per level rather than fixed-iteration.
fn run_benchmark(opts: &Options, dict: Option<Vec<u8>>) -> Result<()> {
    if opts.inputs.is_empty() {
        bail!("-b requires one or more regular input files to benchmark");
    }
    // `-` is stdin everywhere else in this tool, and a benchmark cannot measure
    // it: the whole input is held and read again per pass, which a stream
    // affords neither. Answered before the stat below, or the marker would name
    // a file whenever one happens to sit in the working directory under that
    // name — and stdin whenever one does not. A file really called `-` is still
    // reachable, spelled `./-`.
    if opts.inputs.iter().any(|input| input == Path::new("-")) {
        bail!("-b cannot benchmark stdin; name a file (`./-` for one called `-`)");
    }
    // Benchmarking is the one path that holds whole files, so it needs inputs
    // with an end and a length that means something. A FIFO would block on the
    // read that never returns, a character device would grow the buffer until
    // the allocator gave up, and neither reports a size the ceiling below could
    // be weighed against. Settled before any of them is opened.
    let mut sum = 0u64;
    let mut largest = 0u64;
    // Kept, not just summed: these are the lengths the ceiling below is weighed
    // against, and the read is bounded by the same ones — so what was approved
    // and what is taken cannot be two different figures.
    let mut sizes = Vec::with_capacity(opts.inputs.len());
    for input in &opts.inputs {
        let metadata = fs::metadata(input)
            .wrap_err_with(|| format!("failed to inspect {}", input.display()))?;
        if !metadata.is_file() {
            bail!("-b needs regular files: {} is not one", input.display());
        }
        sum = sum
            .checked_add(metadata.len())
            .ok_or_else(|| eyre!("the inputs add up to more than any machine can address"))?;
        largest = largest.max(metadata.len());
        sizes.push(metadata.len());
    }

    // Those whole-file buffers dwarf the decoder's own workspace, so a ceiling
    // that ignored them would be kept in the small and broken in the large.
    if let Some(limit) = opts.memory_limit {
        // With `-S` only one input is in memory at a time, so the largest file
        // is what has to fit rather than their sum.
        let inputs = if opts.bench_separately { largest } else { sum };
        // Three buffers exist at once: the input, the frame it compresses to,
        // and the decoded copy. Each is allocated at the size named here and
        // never grows past it, so this is what the run actually holds rather
        // than a lower bound on it. The frame's is `compress_bound`, which is
        // the input plus the framing an incompressible input still pays — the
        // case a ceiling has to survive.
        let frame = usize::try_from(inputs)
            .map(structured_zstd::encoding::compress_bound)
            .map(|bound| bound as u64)
            .ok();
        // Beside them stands the match finder every compression pass builds,
        // whose tables are the largest thing at the higher levels — hundreds of
        // MiB where the buffers are tens. It is sized by the level, by the
        // source (both cap the window and the tables) and by what the run has
        // asked for on top: `--long` widens the window and adds a matcher with
        // a table of its own, neither of them in the level's own figures. So it
        // is asked for the levels this run will measure, the input it will
        // measure them on, and the parameters it will measure them with. One
        // level runs at a time and its encoder is dropped before the next, so
        // the largest of them is what stands at the peak.
        //
        // The dictionary is one of those parameters, and the one that moves the
        // figure most: past the size at which it stops being searched in place,
        // the frame runs the dictionary's own table geometry rather than the
        // source's, so 64 KiB measured against a 256 KiB dictionary asks for
        // three times what the file alone does, and a large dictionary at the
        // top levels asks for hundreds of MiB more. Weighed here from the
        // blob's own length, which is what the parameters are chosen by; the
        // content of a trained dictionary is smaller than the blob it arrived
        // in, and overstating it can only pick the larger of the two geometries.
        let dictionary = dict
            .as_ref()
            .map(|bytes| structured_zstd::encoding::DictionarySizes::raw_content(bytes.len()));
        let encoder = (opts.bench_start..=opts.bench_end)
            .map(|level| {
                structured_zstd::encoding::estimated_compression_workspace_bytes_for_run(
                    structured_zstd::encoding::CompressionLevel::Level(level),
                    Some(inputs),
                    opts.long
                        .then_some(opts.long_window_log)
                        .flatten()
                        .and_then(|log| u8::try_from(log).ok()),
                    opts.long && !opts.store,
                    dictionary,
                ) as u64
            })
            .max()
            .unwrap_or(0);
        let buffers = frame
            .and_then(|frame| {
                inputs
                    .checked_mul(2)?
                    .checked_add(frame)?
                    .checked_add(encoder)
            })
            .ok_or_else(|| eyre!("the inputs add up to more than any machine can address"))?;
        check_memory_limit(limit, buffers, 1)?;
        // The dictionary is held alongside them, and more than once: a
        // benchmark measures both directions, so the blob is parsed into an
        // encoder's tables and a decoder's, and the blob itself is still there
        // while they are built from it. All of it is counted with the buffers
        // rather than on its own, since allocations that each clear the ceiling
        // separately can still exceed it together.
        if let Some(bytes) = &dict {
            let total = (bytes.len() as u64)
                .checked_mul(3)
                .and_then(|dictionaries| buffers.checked_add(dictionaries))
                .ok_or_else(|| eyre!("the inputs add up to more than any machine can address"))?;
            check_memory_limit(limit, total, 1)?;
        }
    }

    // Both directions are measured in turn, so both forms are wanted — parsed
    // here, once, rather than inside the timed loops below. The blob is then
    // done with: it is released before the measuring starts rather than held
    // beside the two forms parsed out of it for the rest of the run.
    let dicts = &Dictionaries::prepare(dict.as_deref(), true, true)?;
    drop(dict);

    if opts.bench_separately {
        for (input, size) in opts.inputs.iter().zip(&sizes) {
            let data =
                read_inputs_bounded(std::slice::from_ref(input), std::slice::from_ref(size))?;
            benchmark_one(opts, dicts, &input.display().to_string(), &data)?;
        }
        return Ok(());
    }

    let data = read_inputs_bounded(&opts.inputs, &sizes)?;
    let label = opts
        .inputs
        .iter()
        .map(|input| input.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    benchmark_one(opts, dicts, &label, &data)
}

/// Read every input into one buffer, taking no more room — and no more bytes —
/// than the `sizes` it was weighed against.
///
/// The `-M` ceiling counts three copies of this buffer, so it has to be the size
/// it was counted at: appending file by file leaves a `Vec` holding up to twice
/// the bytes it needs, and reading each file into its own buffer first holds the
/// largest one twice over. The room is taken once, from those same sizes, and
/// each file is read straight into it and no further than its own — a file that
/// grew since is an error rather than a buffer past what was approved.
///
/// `sizes` are the lengths the ceiling saw, in the order of `inputs`; passing
/// the pair keeps the figure that was checked and the figure that is read the
/// same one.
fn read_inputs_bounded(inputs: &[PathBuf], sizes: &[u64]) -> Result<Vec<u8>> {
    let mut total = 0usize;
    for size in sizes {
        total = usize::try_from(*size)
            .ok()
            .and_then(|size| total.checked_add(size))
            .ok_or_else(|| eyre!("the inputs add up to more than this machine can hold"))?;
    }
    let mut data = Vec::with_capacity(total);
    for (input, size) in inputs.iter().zip(sizes) {
        let file = File::open(input)
            .wrap_err_with(|| format!("failed to open input file {}", input.display()))?;
        let before = data.len();
        // One byte past the size that was weighed: reading it means the file
        // holds more than the ceiling was told, so the buffer would grow past
        // what a caller approved. A length that underreports its content — a
        // file being written, or one whose directory entry is not a byte count
        // at all — stops here rather than filling memory nobody agreed to.
        file.take(size + 1)
            .read_to_end(&mut data)
            .wrap_err_with(|| format!("failed to read {}", input.display()))?;
        if (data.len() - before) as u64 > *size {
            bail!(
                "{} grew while it was being read; run again",
                input.display()
            );
        }
    }
    Ok(data)
}

/// Measure one benchmark subject: the whole input as one stream, or a single
/// file under `-S`. Split out so the two modes differ only in what they hand
/// over, not in how the measurement is taken.
fn benchmark_one(opts: &Options, dicts: &Dictionaries, label: &str, data: &[u8]) -> Result<()> {
    use std::time::Instant;

    if data.is_empty() {
        bail!("-b: {label} is empty");
    }
    // Per-level time budget; best (fastest) pass wins, like upstream's -i loop.
    let mb = data.len() as f64 / 1e6;
    println!(
        "benchmarking {label} ({})  levels {}..={}",
        HumanSize::new(data.len() as u64, false),
        opts.bench_start,
        opts.bench_end,
    );

    // The two buffers the measurement fills, sized once from what they will
    // hold: the frame can be no larger than `compress_bound` says, and the
    // decoded copy is exactly the input again. That keeps them the size the `-M`
    // ceiling counted them at instead of the doubled capacity a growing `Vec`
    // ends up with — and it keeps the growth out of the timed sections, which
    // would otherwise be reported as compression and decompression speed.
    let mut compressed = Vec::with_capacity(structured_zstd::encoding::compress_bound(data.len()));
    let mut decoded = Vec::with_capacity(data.len());
    for level in opts.bench_start..=opts.bench_end {
        validate_level(level)?;
        let mut best_compress = f64::MAX;
        let start = Instant::now();
        loop {
            compressed.clear();
            let t = Instant::now();
            compress_stream(
                data,
                &mut compressed,
                &FrameSettings {
                    level,
                    // The benchmark holds the whole input, so the length is
                    // exact and there is no estimate to fall back on.
                    pledged_size: Some(data.len() as u64),
                    size_hint: None,
                    ..FrameSettings::from_options(opts)
                },
                dicts,
            )?;
            best_compress = best_compress.min(t.elapsed().as_secs_f64());
            if start.elapsed().as_secs_f64() >= opts.bench_secs {
                break;
            }
        }

        let mut best_decompress = f64::MAX;
        let start = Instant::now();
        loop {
            decoded.clear();
            let t = Instant::now();
            decompress_stream(
                compressed.as_slice(),
                &mut decoded,
                dicts,
                &DecodeSettings::from_options(opts),
            )?;
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
            HumanSize::new(compressed.len() as u64, false),
        );
    }
    Ok(())
}

/// `--train`: build a FastCOVER dictionary from the concatenated sample files
/// and write it to `-o` (default `dictionary`). Mirrors upstream
/// `zstd --train FILEs -o dict --maxdict=N [--dictID=N]`.
fn train_dictionary(opts: &Options) -> Result<()> {
    use structured_zstd::dictionary::{
        FastCoverOptions, FinalizeOptions, create_fastcover_dict_from_slice,
    };

    if opts.inputs.iter().any(|input| input == Path::new("-")) {
        // The third mode that takes named inputs, and `-` is stdin in all of
        // them: training reads every sample whole and rewinds nothing, so a
        // stream is no more usable here than for `-b` or `-l`. Answered before
        // the stat below, or the marker would name a file whenever one happens
        // to sit in the working directory under that name.
        bail!("--train cannot read stdin; name the sample files (`./-` for one called `-`)");
    }
    if opts.inputs.is_empty() {
        bail!("--train requires one or more sample files");
    }
    // `-c` and `-o` clear one another, so this pair leaves no destination and
    // the default below would stand in: the run would write a file nobody
    // named, and with `-f` over whatever was already there. The reference
    // command fails on the combination as well, so refuse it rather than
    // choose a destination on the caller's behalf.
    if opts.to_stdout {
        bail!("--train cannot write to stdout; name the dictionary with -o");
    }
    // A dictionary cannot be smaller than its own header and the offset history
    // the format requires, so a size below that can only fail — and finding out
    // inside the trainer means every sample has been read and concatenated
    // first. The real bound is higher and depends on the entropy tables the
    // corpus produces, which is why the trainer still checks; this only settles
    // the part that is knowable without reading anything.
    if opts.max_dict < structured_zstd::dictionary::MIN_TRAINED_DICT_SIZE {
        bail!(
            "--maxdict must be at least {} bytes; a dictionary cannot be smaller than its header",
            structured_zstd::dictionary::MIN_TRAINED_DICT_SIZE
        );
    }
    let output = opts
        .output
        .clone()
        .unwrap_or_else(|| PathBuf::from("dictionary"));
    // Settled before the corpus is read: whether this run may write at all is
    // knowable now, and a command that is going to be refused should not first
    // load every sample and spend minutes training a dictionary to throw away.
    ensure_regular_output_destination(&output)?;
    if output.exists() && !opts.force {
        bail!("{} already exists; use -f to overwrite", output.display());
    }
    // The default destination is a plain `dictionary`, which a sample can
    // easily be named: the run would read that file and then replace it with
    // what it learned from it. `-f` permits replacing the output, not spending
    // a sample to make one. Compared as files rather than as paths, because
    // `dictionary` and `sub/../dictionary` are one file and only the second
    // spelling has to appear on the command line for a path comparison to miss
    // it; a sample hard-linked to an existing output is the same collision
    // reached a third way.
    for sample in &opts.inputs {
        let clashes = names_the_same_file(sample, &output)?
            || (output.exists() && paths_point_to_same_file(sample, &output)?);
        if clashes {
            bail!(
                "{} is a training sample; the dictionary cannot be written over it",
                sample.display()
            );
        }
    }

    // Training reads every sample whole, so a sample needs an end and a size
    // that means something: a FIFO would block on a read that never returns
    // and a character device would grow the corpus until the allocator gave
    // up. Settled for all of them before the first is opened.
    for input in &opts.inputs {
        let metadata = fs::metadata(input)
            .wrap_err_with(|| format!("failed to inspect {}", input.display()))?;
        if !metadata.is_file() {
            bail!(
                "--train needs regular files: {} is not one",
                input.display()
            );
        }
    }

    // Each sample is opened once, and what the dictionary may carry is taken
    // from that same open file rather than from its path afterwards. A path
    // answers about whatever it names at the moment it is asked, and training
    // takes long enough for a sample to be replaced while it runs: asking again
    // at the end could describe a file whose bytes are not the ones now inside
    // the dictionary, and grant its permissions to theirs.
    let mut corpus = Vec::new();
    let mut samples = Vec::with_capacity(opts.inputs.len());
    for input in &opts.inputs {
        let mut file = File::open(input)
            .wrap_err_with(|| format!("failed to open training sample {}", input.display()))?;
        let metadata = file
            .metadata()
            .wrap_err_with(|| format!("failed to inspect {}", input.display()))?;
        if !metadata.is_file() {
            bail!(
                "--train needs regular files: {} is not one",
                input.display()
            );
        }
        file.read_to_end(&mut corpus)
            .wrap_err_with(|| format!("failed to read training sample {}", input.display()))?;
        samples.push(metadata);
    }

    let mut dict = Vec::new();
    // From the slice, not through a reader: the corpus is the largest thing
    // this run holds, and the reader path buffers it a second time inside.
    create_fastcover_dict_from_slice(
        corpus.as_slice(),
        &mut dict,
        opts.max_dict,
        &FastCoverOptions::default(),
        FinalizeOptions {
            dict_id: opts.dict_id,
        },
    )
    .map_err(|err| eyre!("dictionary training failed: {err}"))?;

    // A trained dictionary is an output file like any other, so it is written
    // through a temporary that is renamed into place: an interrupted run
    // leaves the previous dictionary intact rather than a half-written one.
    // The overwrite gate ran before the corpus was read.
    let (temp_path, mut temp_file) = create_temporary_output_file(&output)?;
    // And like any other output it is no more readable than what it was made
    // from. A dictionary carries stretches of its corpus verbatim, so training
    // on private samples and leaving the result at whatever the umask allows
    // hands those stretches to everyone; with several samples the strictest
    // decides, since the bytes of each are in there. Applied to the temporary
    // file so the dictionary is never briefly readable under the old mode, and
    // handed to the replace below so a `-f` retrain over a permissive name does
    // not restore it.
    // Asked of the temporary file, which is the one that gets renamed into
    // place: it is where the group the dictionary will belong to is decided.
    let sample_permissions = strictest_sample_permissions(&samples, &temp_path)?;
    if let Some(permissions) = sample_permissions.clone()
        && let Err(err) = fs::set_permissions(&temp_path, permissions)
    {
        let _ = fs::remove_file(&temp_path);
        return Err(err).wrap_err("failed to apply the samples' permissions to the dictionary");
    }
    let written = temp_file
        .write_all(&dict)
        .and_then(|()| temp_file.flush())
        .wrap_err_with(|| format!("failed to write dictionary {}", output.display()));
    if let Err(err) = written {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    drop(temp_file);
    replace_output_file(&temp_path, &output, sample_permissions)?;
    display!(
        opts.verbosity,
        2,
        "trained {} ({}) from {} sample file(s)",
        output.display(),
        HumanSize::new(dict.len() as u64, false),
        opts.inputs.len()
    );
    Ok(())
}

/// The permissions a file made from all of `samples` may carry: every bit that
/// each of them grants, and no other.
///
/// A dictionary holds pieces of every sample, so anyone who could not read one
/// of them must not be able to read it. On Unix that is the bitwise AND of the
/// modes. Elsewhere `Permissions` says only whether a file is read-only, which
/// answers a different question and would make the dictionary unwritable rather
/// than unreadable, so nothing is applied and the platform's own inheritance
/// stands.
#[cfg(unix)]
fn strictest_sample_permissions(
    samples: &[fs::Metadata],
    destination: &Path,
) -> Result<Option<fs::Permissions>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if samples.is_empty() {
        return Ok(None);
    }
    let samples_by_group: Vec<(u32, u32)> = samples
        .iter()
        .map(|sample| (sample.permissions().mode(), sample.gid()))
        .collect();
    // The group the file itself is in, which is the one its group bits would
    // admit. It is the process's, or the directory's when that is setgid, so it
    // is read from the file rather than assumed.
    let output_gid = fs::metadata(destination)
        .wrap_err_with(|| format!("failed to inspect {}", destination.display()))?
        .gid();
    Ok(Some(fs::Permissions::from_mode(output_mode_for_sources(
        &samples_by_group,
        output_gid,
    ))))
}

/// The mode a file built from all of `sources` may carry, given the group it
/// will itself belong to.
///
/// An output carries its sources' bytes — an archive its file's, a dictionary
/// stretches of every sample's — so nobody may reach it who could not reach
/// them. Each source contributes its mode and the group those bits admit, and
/// the intersection of the modes is where the answer starts.
///
/// Bits alone do not say WHO they let in, though: `0640` on two sources of
/// different groups admits two different sets of people, and a file belongs to
/// one group — which is not even necessarily theirs, since a setgid directory
/// gives the file its own. So the group bits survive only when every source
/// names one group and the file is in it. The owner's bits stand, because every
/// source was read to build this and so this user was entitled to its bytes,
/// and the world's name no one in particular, so their intersection means what
/// it says.
#[cfg(unix)]
fn output_mode_for_sources(sources: &[(u32, u32)], output_gid: u32) -> u32 {
    // Only the bits that say who may read, write and run the file. Set-user-ID
    // and set-group-ID say who a program runs AS, which is not a permission the
    // bytes carry and not one to hand to a file with a different owner: a
    // privileged run over someone else's `04755` archive would otherwise write
    // a root-owned `04755` file. The reference command carries neither.
    let mut mode = 0o0777;
    let mut group = None;
    let mut one_group = true;
    for (source_mode, source_gid) in sources {
        mode &= *source_mode;
        match group {
            None => group = Some(*source_gid),
            Some(seen) => one_group &= seen == *source_gid,
        }
    }
    if !one_group || group != Some(output_gid) {
        mode &= !0o070;
    }
    mode
}

#[cfg(not(unix))]
fn strictest_sample_permissions(
    _samples: &[fs::Metadata],
    _destination: &Path,
) -> Result<Option<fs::Permissions>> {
    Ok(None)
}

/// The mode an output built from one source may carry, by the same rule the
/// trainer applies to its samples: an archive holds its source's bytes, so
/// nobody may read it who could not read that source.
#[cfg(unix)]
fn permissions_from_source(
    source: &fs::Metadata,
    destination: &Path,
) -> Result<Option<fs::Permissions>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let output_gid = fs::metadata(destination)
        .wrap_err_with(|| format!("failed to inspect {}", destination.display()))?
        .gid();
    Ok(Some(fs::Permissions::from_mode(output_mode_for_sources(
        &[(source.permissions().mode(), source.gid())],
        output_gid,
    ))))
}

/// Elsewhere `Permissions` says only whether a file is read-only, which answers
/// a different question than who may read it, so the source's own is carried
/// over unchanged and the platform decides the rest.
#[cfg(not(unix))]
fn permissions_from_source(
    source: &fs::Metadata,
    _destination: &Path,
) -> Result<Option<fs::Permissions>> {
    Ok(Some(source.permissions()))
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

/// `--list`: one row per archive in the reference command's layout, or one
/// block per archive under `-v`, then a total when there are several. An
/// archive that cannot be listed is reported and the rest are still listed;
/// the count that failed comes back.
fn list_files(opts: &Options) -> Result<usize> {
    if opts.inputs.is_empty() {
        bail!("No files given");
    }
    // `-` is stdin, which the walk cannot seek through any more than a FIFO.
    // Answered before any file is opened, or the marker would name a file
    // whenever one happens to sit in the working directory under that name,
    // and stdin whenever one does not. A file really called `-` is still
    // reachable, spelled `./-`.
    if opts.inputs.iter().any(|input| input == Path::new("-")) {
        bail!("--list does not support reading from standard input");
    }
    let verbose = opts.verbosity > DEFAULT_LEVEL;
    if !verbose {
        println!("Frames  Skips  Compressed  Uncompressed  Ratio  Check  Filename");
    }
    let mut total = ListTotal::default();
    let mut failed = 0;
    for input in &opts.inputs {
        match list_file(input, verbose, opts.verbosity) {
            Ok(summary) => total.add(&summary),
            Err(err) => {
                display!(opts.verbosity, 1, "zstd: {err}");
                failed += 1;
            }
        }
    }
    if opts.inputs.len() > 1 && !verbose {
        total.print();
    }
    Ok(failed)
}

/// What the archives listed so far add up to, for the closing row.
#[derive(Default)]
struct ListTotal {
    frames: u64,
    skips: u64,
    compressed: u64,
    decompressed: u64,
    /// Some archive omitted a Frame_Content_Size, so the total is unknowable.
    decompressed_unknown: bool,
    /// Some archive carries no checksum, so the total's `Check` says nothing.
    without_check: bool,
    files: usize,
}

impl ListTotal {
    fn add(&mut self, summary: &ArchiveSummary) {
        self.frames += summary.frames;
        self.skips += summary.skips;
        self.compressed += summary.compressed;
        match summary.decompressed {
            Some(decompressed) => self.decompressed += decompressed,
            None => self.decompressed_unknown = true,
        }
        self.without_check |= !summary.check;
        self.files += 1;
    }

    fn print(&self) {
        println!("----------------------------------------------------------------- ");
        let compressed = HumanSize::new(self.compressed, false);
        let check = if self.without_check { "" } else { "XXH64" };
        if self.decompressed_unknown {
            println!(
                "{:>6}  {:>5}  {}                       {:>5}  {} files",
                self.frames,
                self.skips,
                compressed.columns(6),
                check,
                self.files
            );
        } else {
            let ratio = if self.compressed == 0 {
                0.0
            } else {
                self.decompressed as f64 / self.compressed as f64
            };
            println!(
                "{:>6}  {:>5}  {}  {}  {:>5.3}  {:>5}  {} files",
                self.frames,
                self.skips,
                compressed.columns(6),
                HumanSize::new(self.decompressed, false).columns(8),
                ratio,
                check,
                self.files
            );
        }
    }
}

/// Largest possible zstd frame header: 4-byte magic + 1-byte descriptor + up to
/// 8-byte Frame_Content_Size + up to 4-byte Dictionary_ID + 1-byte
/// Window_Descriptor (RFC 8878 §3.1.1.1).
const MAX_FRAME_HEADER_LEN: usize = 18;

/// What one `--list` row says about an archive.
#[derive(Debug)]
struct ArchiveSummary {
    frames: u64,
    skips: u64,
    compressed: u64,
    /// `None` when any frame omits its Frame_Content_Size, since the total is
    /// then unknowable without decoding.
    decompressed: Option<u64>,
    /// Whether any data frame carries a content checksum.
    ///
    /// That is the question the reference tool's `Check` column answers, not
    /// whether every frame carries one: zstd 1.5.7 prints `XXH64` for an
    /// archive of one checksummed and one unchecked frame — in either order —
    /// and `None` only when no frame has one. Reporting a mixed state here
    /// would say something true about the archive in a column scripts read as
    /// upstream's, so the answer stays the one they parse. Verification is
    /// per frame regardless: `-t` and `-d` compare every checksum that is
    /// there, so a frame without one is unchecked whatever this column says.
    check: bool,
    /// The dictionary the archive needs, and `None` both when it needs none and
    /// when its frames name different ones — an archive with no single answer
    /// has no id to print, and claiming the first frame's would send the reader
    /// after a dictionary that decodes only part of it.
    dict_id: Option<u32>,
    /// Whether the data frames named one dictionary between them. False makes
    /// the id above absent rather than wrong.
    dict_ids_agree: bool,
    /// The window the last data frame declares, which is what a decoder will
    /// need in memory to read it.
    window_size: u64,
    /// The stored checksum of the last data frame that carries one; shown
    /// under `-lv` for a single-frame archive, as the reference command
    /// shows it.
    checksum: Option<[u8; 4]>,
}

/// Walk every frame in the file (no body decode), summing compressed and
/// declared content sizes.
///
/// Reads each frame header, then walks the 3-byte block headers by `seek`ing
/// past every block body, so peak memory stays O(1) regardless of archive size
/// (a multi-GB file is never loaded whole).
fn summarize_archive(path: &Path) -> Result<ArchiveSummary> {
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
    // `Frames` counts every frame in the file and `Skips` says how many of them
    // were skippable, which is how the reference tool fills these columns:
    // zstd 1.5.7 prints `Frames 3, Skips 1` for an archive of two data frames
    // around one metadata frame. Reporting 2 and 1 for that file would read as
    // four frames to anyone adding the columns up.
    let mut frames = 0u64;
    let mut data_frames = 0u64;
    let mut skips = 0u64;
    let mut decompressed = Some(0u64);
    let mut check = false;
    let mut dict_id = None;
    let mut dict_ids_agree = true;
    let mut window_size = 0u64;
    let mut checksum = None;

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
            Err(err) => bail!(
                "File \"{}\" not compressed by zstd ({err:?})",
                path.display()
            ),
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
            let end = block_offset
                .checked_add(4)
                .filter(|end| *end <= compressed)
                .ok_or_else(|| eyre!("{}: truncated content checksum", path.display()))?;
            file.seek(SeekFrom::Start(block_offset))?;
            let mut stored = [0u8; 4];
            file.read_exact(&mut stored)
                .map_err(|_| eyre!("{}: truncated content checksum", path.display()))?;
            checksum = Some(stored);
            end
        } else {
            block_offset
        };
        window_size = info.window_size;

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
        // Any frame carrying one makes the archive checksummed, which is what
        // the reference tool reports for a mixed file as well.
        check |= info.content_checksum;
        // The first DATA frame sets the id; a leading metadata frame counts
        // towards `frames` but carries no header to read one from. Every later
        // frame has to agree, because one id is what the column can say: an
        // archive whose frames name different dictionaries needs more than one
        // to be read, and the first frame's alone would decode only its own
        // part.
        if data_frames == 0 {
            dict_id = info.dictionary_id;
        } else if dict_id != info.dictionary_id {
            dict_ids_agree = false;
        }
        data_frames += 1;
        frames += 1;
        offset = frame_end;
    }

    Ok(ArchiveSummary {
        frames,
        skips,
        compressed,
        decompressed,
        check,
        dict_id: dict_ids_agree.then_some(dict_id).flatten(),
        dict_ids_agree,
        window_size,
        checksum,
    })
}

/// Print one `--list` entry: a row in the reference command's `zstd -l`
/// layout, or, when `verbose`, the block its `-lv` prints. The decompressed
/// columns are left blank when any frame omits its Frame_Content_Size.
fn list_file(path: &Path, verbose: bool, verbosity: i32) -> Result<ArchiveSummary> {
    // The walk seeks between frame headers, so it needs a file that can seek.
    // Settled before the file is opened: opening a FIFO blocks until a writer
    // appears, and the failure would then arrive from the seek rather than
    // from the thing that was wrong.
    let metadata =
        fs::metadata(path).wrap_err_with(|| format!("Error : {} is not a file", path.display()))?;
    if !metadata.is_file() {
        bail!("Error : {} is not a file", path.display());
    }
    let summary = summarize_archive(path)?;
    // The id column holds one id, so an archive built from several
    // dictionaries has nothing to put there. Said out loud rather than left to
    // the `0`, which on its own reads as "no dictionary needed".
    if !summary.dict_ids_agree {
        display!(
            verbosity,
            2,
            "WARNING: File contains multiple frames with different dictionary IDs. Showing dictID 0 instead"
        );
    }
    let ratio = summary
        .decompressed
        .map(|decompressed| decompressed as f64 / summary.compressed as f64);
    let check = if summary.check { "XXH64" } else { "None" };
    let compressed = HumanSize::new(summary.compressed, false);
    if !verbose {
        match (summary.decompressed, ratio) {
            (Some(decompressed), Some(ratio)) => println!(
                "{:>6}  {:>5}  {}  {}  {ratio:>5.3}  {check:>5}  {}",
                summary.frames,
                summary.skips,
                compressed.columns(6),
                HumanSize::new(decompressed, false).columns(8),
                path.display()
            ),
            _ => println!(
                "{:>6}  {:>5}  {}                       {check:>5}  {}",
                summary.frames,
                summary.skips,
                compressed.columns(6),
                path.display()
            ),
        }
        return Ok(summary);
    }
    let data_frames = summary.frames - summary.skips;
    println!("{} ", path.display());
    println!("# Zstandard Frames: {data_frames}");
    if summary.skips > 0 {
        println!("# Skippable Frames: {}", summary.skips);
    }
    println!("DictID: {}", summary.dict_id.unwrap_or(0));
    println!(
        "Window Size: {} ({} B)",
        HumanSize::new(summary.window_size, false),
        summary.window_size
    );
    println!("Compressed Size: {compressed} ({} B)", summary.compressed);
    if let (Some(decompressed), Some(ratio)) = (summary.decompressed, ratio) {
        println!(
            "Decompressed Size: {} ({decompressed} B)",
            HumanSize::new(decompressed, false)
        );
        println!("Ratio: {ratio:.4}");
    }
    match summary.checksum {
        Some(stored) if summary.check && data_frames == 1 => println!(
            "Check: {check} {:02x}{:02x}{:02x}{:02x}",
            stored[3], stored[2], stored[1], stored[0]
        ),
        _ => println!("Check: {check}"),
    }
    println!();
    Ok(summary)
}

/// Remove the source file after a successful (de)compression when `--rm` is set
/// (and `-k` was not). A no-op otherwise.
fn remove_source_if_requested(opts: &Options, input: &Path) -> Result<()> {
    // Never when the output went to stdout: that may have been a pipe whose
    // reader is gone, a terminal, or anything else we cannot read back, so
    // there is no saved copy to justify deleting the original. Upstream keeps
    // the file for `-c` too.
    if opts.remove_source && !opts.keep && !opts.to_stdout {
        // Removing the source is past the point where an interruption should
        // delete anything: the output is in place, and the guard would take
        // it along with the source.
        interrupt::clear();
        fs::remove_file(input).wrap_err("failed to remove source file after success")?;
    }
    Ok(())
}

/// Counts what passes through to the writer beneath, so the summary can say
/// how much came out of a compression whatever the sink was.
struct CountingWriter<W: Write> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Whether a file type is a named pipe.
fn is_fifo_type(kind: &fs::FileType) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        kind.is_fifo()
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        false
    }
}

/// Whether a file type is a block device.
fn is_block_device_type(kind: &fs::FileType) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        kind.is_block_device()
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        false
    }
}

/// Open one named input for streaming, or say why it cannot be.
///
/// `Ok(None)` is an input deliberately left alone under
/// `--exclude-compressed`. A directory, a socket or a device is refused the
/// way the reference command refuses them ("-- ignored"); `-f` admits block
/// devices. The kind and length that matter are those of the OPEN file: a
/// path is only a name, and can be made to name something else between the
/// look and the open.
fn open_input(opts: &Options, input: &Path) -> Result<Option<(File, fs::Metadata)>> {
    let named = fs::metadata(input)
        .map_err(|err| eyre!("can't stat {} : {err} -- ignored", input.display()))?;
    if named.is_dir() {
        bail!("{} is a directory -- ignored", input.display());
    }
    let admissible = |kind: &fs::FileType| {
        kind.is_file() || is_fifo_type(kind) || (opts.force && is_block_device_type(kind))
    };
    if !admissible(&named.file_type()) {
        bail!("{} is not a regular file -- ignored", input.display());
    }
    if compresses(opts) && opts.exclude_compressed && inputs::has_compressed_extension(input) {
        display!(
            opts.verbosity,
            4,
            "File is already compressed : {}",
            input.display()
        );
        return Ok(None);
    }
    let source = File::open(input).wrap_err_with(|| input.display().to_string())?;
    let metadata = source
        .metadata()
        .wrap_err_with(|| format!("failed to inspect {}", input.display()))?;
    if metadata.is_dir() || !admissible(&metadata.file_type()) {
        bail!("{} is not a regular file -- ignored", input.display());
    }
    Ok(Some((source, metadata)))
}

/// Stream one opened input through the codec into `sink`.
fn stream_opened<W: Write>(
    opts: &Options,
    dicts: &Dictionaries,
    source: File,
    metadata: &fs::Metadata,
    sink: W,
) -> Result<Processed> {
    // Kept as the `u64` the filesystem reports: the work streams, so a file
    // only has to fit the window, and narrowing to a pointer would refuse
    // 4 GiB archives on 32-bit targets for no reason the work has.
    let source_size = metadata.len();
    // Only a regular file's length says how many bytes will be read. A FIFO,
    // a device or a socket reports something unrelated (commonly zero), and
    // pledging that turns a perfectly good stream into a length mismatch.
    let pledged_size = metadata.is_file().then_some(source_size);
    let shown = opts
        .progress
        .shown(opts.verbosity, io::stderr().is_terminal());
    let reader = ProgressMonitor::new(BufReader::new(source), source_size, shown);
    stream(opts, dicts, reader, pledged_size, sink)
}

/// Stream stdin through the codec into `sink`. stdin has no length to stat,
/// which is exactly why `--stream-size` / `--size-hint` exist: whatever the
/// caller pledged travels in `opts`.
fn stream_stdin<W: Write>(opts: &Options, dicts: &Dictionaries, sink: W) -> Result<Processed> {
    let stdin = io::stdin();
    let reader = ProgressMonitor::new(stdin.lock(), 0, false);
    stream(opts, dicts, reader, None, sink)
}

/// Run the mode's codec from `reader` into `sink` and count both sides.
/// `pledged_size` is the exact length of THIS input when it has one to stat;
/// `--stream-size` stands in when it does not. `-t` decodes into nothing,
/// whatever sink it was handed.
fn stream<R: Read, W: Write>(
    opts: &Options,
    dicts: &Dictionaries,
    mut reader: ProgressMonitor<R>,
    pledged_size: Option<u64>,
    mut sink: W,
) -> Result<Processed> {
    let written = match opts.mode {
        Mode::Compress => {
            let mut counting = CountingWriter {
                inner: &mut sink,
                written: 0,
            };
            compress_stream(
                &mut reader,
                &mut counting,
                &FrameSettings {
                    pledged_size: pledged_size.or(opts.pledged_size),
                    ..FrameSettings::from_options(opts)
                },
                dicts,
            )?;
            counting.written
        }
        Mode::Decompress => decompress_stream(
            &mut reader,
            &mut sink,
            dicts,
            &DecodeSettings::from_options(opts),
        )?,
        Mode::Test => decompress_stream(
            &mut reader,
            io::sink(),
            dicts,
            &DecodeSettings::from_options(opts),
        )?,
        Mode::List | Mode::Train => unreachable!("list / train never stream"),
    };
    sink.flush().wrap_err("failed to flush output")?;
    Ok(Processed {
        read: reader.read,
        written,
    })
}

/// One input into a destination that is already open and shared: stdout, or
/// the `-o` file being filled. An input that cannot be opened is reported
/// here and refused; a failure while streaming is returned for the caller to
/// end the run on, since the shared output now holds a partial frame.
fn stream_input_to<W: Write>(
    opts: &Options,
    dicts: &Dictionaries,
    input: &Path,
    mut sink: W,
    destination: &str,
    total: usize,
) -> Result<Outcome> {
    let (name, processed) = if input == Path::new("-") {
        (
            STDIN_MARK.to_string(),
            stream_stdin(opts, dicts, &mut sink)?,
        )
    } else {
        match open_input(opts, input) {
            Ok(Some((source, metadata))) => (
                input.display().to_string(),
                stream_opened(opts, dicts, source, &metadata, &mut sink)?,
            ),
            Ok(None) => return Ok(Outcome::Skipped),
            Err(err) => {
                display!(opts.verbosity, 1, "zstd: {err}");
                return Ok(Outcome::Refused);
            }
        }
    };
    file_summary(opts, total, &name, destination, &processed);
    Ok(Outcome::Done(processed))
}

/// Fill `output` atomically: `fill` writes into a sibling temporary, which is
/// renamed into place on success and removed on failure or interruption.
///
/// Honours the `-f` overwrite gate the way the reference command does: an
/// existing output is refused outright below the default display level, where
/// no question can be asked, and asked about at it. `Ok(None)` is such a
/// refusal, already reported. `source` is the file whose permissions the
/// output takes; `None` (stdin, a concatenation) leaves the umask to decide.
fn write_output_file<T>(
    opts: &Options,
    output: &Path,
    source: Option<&fs::Metadata>,
    fill: impl FnOnce(&mut File) -> Result<T>,
) -> Result<Option<T>> {
    ensure_regular_output_destination(output)?;
    if output.exists() && !opts.force {
        if opts.verbosity <= 1 {
            display!(
                opts.verbosity,
                1,
                "zstd: {} already exists; not overwritten",
                output.display()
            );
            return Ok(None);
        }
        eprint!("zstd: {} already exists; ", output.display());
        if !confirm(
            "overwrite (y/n) ? ",
            "Not overwritten",
            reads_stdin(&opts.inputs),
            &mut io::stdin().lock(),
        ) {
            return Ok(None);
        }
    }
    let (temp_path, temp_file) = create_temporary_output_file(output)?;
    // From here until the rename, an interruption removes the temporary
    // rather than leaving it beside the source.
    interrupt::guard(&temp_path);
    let abandon = |temp_path: &Path| {
        let _ = fs::remove_file(temp_path);
        interrupt::clear();
    };
    // The output is as private as its source, whether it is created or replaced:
    // an archive of a 0600 secret must not arrive at whatever the umask allows,
    // and must not take a world-readable mode from the name it lands on either.
    // The reference command applies the source's mode in both directions. Only a
    // source with no mode of its own (stdin) leaves the destination's alone.
    // Asked of the temporary file, since that is the one renamed into place and
    // so the one whose own group the mode's group bits would admit.
    let source_permissions = match source {
        Some(metadata) => match permissions_from_source(metadata, &temp_path) {
            Ok(permissions) => permissions,
            Err(err) => {
                abandon(&temp_path);
                return Err(err);
            }
        },
        None => None,
    };
    if let Some(permissions) = source_permissions.clone()
        && let Err(err) = fs::set_permissions(&temp_path, permissions)
    {
        abandon(&temp_path);
        return Err(err).wrap_err("failed to apply the source's permissions to the output");
    }
    let result: Result<T> = (|| {
        let mut sink = temp_file;
        let value = fill(&mut sink)?;
        sink.flush().wrap_err("failed to flush output")?;
        Ok(value)
    })();
    let value = match result {
        Ok(value) => value,
        Err(err) => {
            abandon(&temp_path);
            return Err(err);
        }
    };
    // `replace_output_file` removes the temporary itself when it fails.
    let replaced = replace_output_file(&temp_path, output, source_permissions);
    interrupt::clear();
    replaced.map(|()| Some(value))
}

/// Resolve the output path for a file input under the current mode: the
/// input's name with the suffix added or removed, placed beside it, under
/// `--output-dir-flat`, or under the mirrored tree of `--output-dir-mirror`.
fn derive_output_path(opts: &Options, input: &Path) -> Result<PathBuf> {
    if let Some(out) = &opts.output {
        return Ok(out.clone());
    }
    let beside = match opts.mode {
        Mode::Compress => add_extension(input, ZSTD_SUFFIX),
        Mode::Decompress => {
            // Drop the extension as a path component rather than as text: a
            // path is bytes, and rebuilding it from a lossy string renames the
            // file it decompresses, with different inputs colliding on one
            // replacement-character name.
            let replacement = input.extension().and_then(|extension| {
                DECOMPRESS_SUFFIXES
                    .iter()
                    .find(|(known, _)| extension == *known)
                    .map(|(_, replacement)| *replacement)
            });
            let Some(replacement) = replacement else {
                bail!(
                    "{}: unknown suffix (.zst/.tzst/.zstd expected). Can't derive the output \
                     file name. Specify it with -o dstFileName. Ignoring.",
                    input.display()
                );
            };
            input.with_extension(replacement)
        }
        Mode::Test | Mode::List | Mode::Train => {
            unreachable!("test / list / train modes never write an output file")
        }
    };
    if let Some(root) = &opts.output_dir_mirror {
        let verb = if opts.mode == Mode::Compress {
            "compress"
        } else {
            "decompress"
        };
        let directory = inputs::mirrored_output_dir(input, root).ok_or_else(|| {
            eyre!(
                "--output-dir-mirror cannot {verb} '{}' into '{}'",
                input.display(),
                root.display()
            )
        })?;
        return Ok(directory.join(beside.file_name().unwrap_or(beside.as_os_str())));
    }
    if let Some(directory) = &opts.output_dir {
        return Ok(inputs::flat_output_path(&beside, directory));
    }
    Ok(beside)
}

/// One named input into an output of its own, derived from its name.
fn process_file(
    opts: &Options,
    input: &Path,
    dicts: &Dictionaries,
    total: usize,
) -> Result<Outcome> {
    let Some((source, metadata)) = open_input(opts, input)? else {
        return Ok(Outcome::Skipped);
    };
    let name = input.display().to_string();

    // Test mode: decompress into the void, report what was there.
    if opts.mode == Mode::Test {
        let processed = stream_opened(opts, dicts, source, &metadata, io::sink())?;
        file_summary(opts, total, &name, "", &processed);
        return Ok(Outcome::Done(processed));
    }

    let output = derive_output_path(opts, input)?;
    ensure_distinct_paths(input, &output)?;
    if let Some(root) = &opts.output_dir_mirror {
        inputs::create_mirrored_dirs(input, root)?;
    }
    let Some(processed) = write_output_file(opts, &output, Some(&metadata), |sink| {
        stream_opened(opts, dicts, source, &metadata, sink)
    })?
    else {
        return Ok(Outcome::Refused);
    };
    file_summary(
        opts,
        total,
        &name,
        &output.display().to_string(),
        &processed,
    );
    remove_source_if_requested(opts, input)?;
    Ok(Outcome::Done(processed))
}

/// Everything the command line says about how one frame is to be built.
///
/// Grouped rather than passed one by one: these travel together through every
/// compression entry point, and a positional list this long invites the caller
/// to line the arguments up wrong.
#[derive(Clone, Copy)]
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
    /// Trailing XXH64 checksum (`--[no-]check`).
    checksum: bool,
    /// Whether a known length is recorded in the header (`--[no-]content-size`).
    content_size_flag: bool,
    /// Whether a dictionary frame records the dictionary's ID (`--no-dictID`).
    dict_id_flag: bool,
}

impl Default for FrameSettings {
    /// The frame the reference COMMAND writes by default: checksummed, with
    /// the content size and the dictionary ID in the header. (The library's
    /// own default omits the checksum, which is why this is spelled out.)
    fn default() -> Self {
        Self {
            level: CompressionLevel::DEFAULT_LEVEL,
            store: false,
            pledged_size: None,
            size_hint: None,
            long: false,
            long_window_log: None,
            target_block_size: None,
            checksum: true,
            content_size_flag: true,
            dict_id_flag: true,
        }
    }
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
            checksum: opts.checksum,
            content_size_flag: opts.content_size_flag,
            dict_id_flag: opts.dict_id_flag,
        }
    }
}

/// How a stream is decoded: whether a stored checksum is compared, and what
/// happens to input that is not a zstd stream.
#[derive(Clone, Copy)]
struct DecodeSettings {
    /// Compare the trailing checksum against the data (`--[no-]check`).
    verify_checksum: bool,
    /// Copy input that is not a zstd stream through unchanged rather than
    /// failing on it (`--pass-through`).
    pass_through: bool,
}

impl Default for DecodeSettings {
    fn default() -> Self {
        Self {
            verify_checksum: true,
            pass_through: false,
        }
    }
}

impl DecodeSettings {
    /// What the command line asked for. Pass-through defaults to the
    /// reference command's rule: on when forced and writing to stdout, which
    /// is how `zstdcat` and `zstd -dcf` behave.
    fn from_options(opts: &Options) -> Self {
        Self {
            verify_checksum: opts.checksum,
            pass_through: opts
                .pass_through
                .unwrap_or(opts.force && writes_stdout(opts)),
        }
    }
}

/// Streaming compression core (file or stdout), optionally dictionary-primed.
fn compress_stream<R: Read, W: Write>(
    mut reader: R,
    writer: W,
    settings: &FrameSettings,
    dicts: &Dictionaries,
) -> Result<()> {
    let &FrameSettings {
        level,
        store,
        pledged_size,
        size_hint,
        long,
        long_window_log,
        target_block_size,
        checksum,
        content_size_flag,
        dict_id_flag,
    } = settings;
    let compression_level = if store {
        CompressionLevel::Uncompressed
    } else {
        CompressionLevel::from_level(level)
    };
    let mut encoder = structured_zstd::encoding::StreamingEncoder::new(writer, compression_level);
    // The reference `zstd` COMMAND defaults the content checksum ON (unlike
    // the library API, whose default is off and which our encoder mirrors), so
    // it is set explicitly either way: on by default, off under `--no-check`.
    encoder
        .set_content_checksum(checksum)
        .wrap_err("failed to set the content checksum flag")?;
    encoder
        .set_content_size_flag(content_size_flag)
        .wrap_err("failed to set the content size flag")?;
    encoder
        .set_dictionary_id_flag(dict_id_flag)
        .wrap_err("failed to set the dictionary ID flag")?;
    // A smaller block target is what the caller asked for when they want
    // bounded latency; the encoder clamps it to the format's own range. Zero
    // is the parameter's own way of saying "no target", so it stays off rather
    // than being clamped up into the smallest block the format allows.
    if let Some(target) = target_block_size.filter(|target| *target != 0) {
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
    } else if let Some(size) = size_hint.filter(|size| *size != 0) {
        // Only an estimate (`--size-hint`): it steers the encoder's geometry
        // and must NOT reach the header, or a wrong guess would turn a
        // successful compression into a failure. Zero is not a hint — the
        // parameter says so — and taking it as one would size the encoder for
        // an empty source and shrink the window a real stream needs.
        encoder
            .set_source_size_hint(size)
            .wrap_err("failed to set source size hint")?;
    }
    // Parsed once for the whole run; the encoder takes ownership of what it is
    // primed with, so each frame gets a copy of those tables rather than
    // building them again from the blob.
    if let Some(prepared) = &dicts.encoder {
        encoder
            .set_encoder_dictionary(prepared.clone())
            .wrap_err("failed to load dictionary for compression")?;
    }
    io::copy(&mut reader, &mut encoder).wrap_err("streaming compression failed")?;
    encoder.finish().wrap_err("failed to finalize zstd frame")?;
    Ok(())
}

/// The magic number every zstd frame opens with (RFC 8878 §3.1.1).
const FRAME_MAGIC: u32 = 0xFD2F_B528;

/// The 16 magic numbers a skippable frame may open with, less their low
/// nibble (RFC 8878 §3.1.2).
const SKIPPABLE_MAGIC_BASE: u32 = 0x184D_2A50;

/// Streaming decompression core (file, stdout, or sink), optionally
/// dict-primed. Returns the number of bytes written.
///
/// A zstd stream is a sequence of frames: `cat a.zst b.zst` is a valid archive
/// that decodes to `a` then `b`, and skippable frames may sit between them. The
/// decoder's `Read` ends at the first frame, so the loop below re-initialises it
/// on whatever follows until the source is exhausted. The library's
/// `read_to_end` walks frames too, but only by buffering the whole stream in
/// memory, which a command-line tool handed a multi-gigabyte archive cannot do.
///
/// Each frame is recognised by its magic number before a decoder is built on
/// it, the way the reference command looks before it decodes: input that is
/// not a zstd stream is then copied through under `--pass-through`, or
/// refused as an unknown format.
fn decompress_stream<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    dicts: &Dictionaries,
    settings: &DecodeSettings,
) -> Result<u64> {
    use structured_zstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};

    // Parsed once for the whole run rather than per stream or per frame: every
    // frame here is primed with the same dictionary, and the handle is shared,
    // so priming costs a reference rather than a rebuild.
    let handle = dicts.decoder.as_ref();
    let mut source = BufReader::new(reader);
    let mut frames = 0u64;
    let mut written = 0u64;
    loop {
        // The magic is read ahead of the decoder and handed back to it in
        // front of the rest of the stream, so the source itself is never
        // rewound.
        let mut magic = [0u8; 4];
        let read = read_filling(&mut source, &mut magic)?;
        if read == 0 {
            // End of the last frame is success; end before the first one means
            // the input never held a frame at all, which is not an archive that
            // decodes to nothing.
            if frames == 0 {
                bail!("unexpected end of file");
            }
            return Ok(written);
        }
        let is_frame = read == 4 && {
            let magic = u32::from_le_bytes(magic);
            magic == FRAME_MAGIC || magic & 0xFFFF_FFF0 == SKIPPABLE_MAGIC_BASE
        };
        if !is_frame {
            if settings.pass_through {
                writer
                    .write_all(&magic[..read])
                    .wrap_err("failed to write the passed-through input")?;
                written += read as u64;
                written += io::copy(&mut source, &mut writer)
                    .wrap_err("failed to pass the input through")?;
                return Ok(written);
            }
            if read < 4 {
                bail!("unknown header");
            }
            bail!("unsupported format");
        }
        frames += 1;
        let mut stream = io::Cursor::new(magic).chain(&mut source);
        // Borrowed, not moved: a frame that turns out to be skippable leaves
        // the reader with us to step over it and carry on.
        let built = match &handle {
            // The dictionary constructors FORCE the supplied dictionary, which
            // the registration path does not: a frame may legitimately omit the
            // optional dictionary ID, and then nothing would select it.
            Some(h) => structured_zstd::decoding::StreamingDecoder::new_with_dictionary_handle(
                &mut stream,
                h,
            ),
            None => structured_zstd::decoding::StreamingDecoder::new(&mut stream),
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
                    &mut stream.by_ref().take(u64::from(length)),
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
        // made: the reference command validates by default, and `-t` exists to
        // answer exactly this question, so a frame whose stored checksum
        // disagrees with its data has to fail rather than decode quietly.
        // `--no-check` asks for the opposite. Read at the end of the frame, so
        // setting it after construction is in time.
        decoder
            .decoder_mut()
            .set_content_checksum(if settings.verify_checksum {
                structured_zstd::decoding::ContentChecksum::Verify
            } else {
                structured_zstd::decoding::ContentChecksum::None
            });
        written +=
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

/// Whether two paths name the same file, for a path that may not exist yet.
///
/// A path that is there is resolved whole, so a symlink counts as the file it
/// points at: `dict-link -> data` and `data` are one file, and a guard that
/// read the link's own name would let the pair through. The preflight also
/// compares a derived OUTPUT, which is usually still to be created and which
/// `canonicalize` therefore cannot resolve; that side falls back to its
/// canonical directory plus its own file name, which still collapses `.`, `..`
/// and any symlinked directory above it. A path whose directory cannot be
/// resolved either names nothing this run could collide with, so it simply does
/// not match.
fn names_the_same_file(left: &Path, right: &Path) -> Result<bool> {
    fn resolved(path: &Path) -> Option<PathBuf> {
        if let Ok(whole) = fs::canonicalize(path) {
            return Some(whole);
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        let directory = fs::canonicalize(parent.unwrap_or_else(|| Path::new("."))).ok()?;
        Some(directory.join(path.file_name()?))
    }
    match (resolved(left), resolved(right)) {
        (Some(left), Some(right)) => Ok(left == right),
        _ => Ok(false),
    }
}

/// Whether two different paths name one file, as a hard link does.
///
/// Only answers for the identity the platform lets us read. Unix compares the
/// device and inode. Windows would need the volume serial and file index, which
/// std exposes only on nightly (`windows_by_handle`) and which no dependency
/// here can reach, so hard links go unnoticed there — the caller's canonical
/// path comparison still catches the same path by another name, which is the
/// case that actually comes up.
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

    #[cfg(not(unix))]
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
            // Named after the output rather than the temporary: the reader
            // knows the output they asked for, and the usual cause is its
            // directory not being there.
            Err(err) => {
                return Err(err).wrap_err_with(|| output.display().to_string());
            }
        }
    }
    Err(eyre!("failed to allocate unique temporary output file"))
}

/// Move the finished temporary file into place.
///
/// `source_permissions` is the mode the content asks for — the file it came
/// from, or the strictest of the samples a dictionary was trained on. When
/// there is one it decides, replacing whatever the destination happened to
/// carry: an archive of a private file is private even if it lands on a
/// world-readable name, which is what the reference command does in both
/// directions. Only when the content names no mode of its own (stdin has no
/// file to take one from) does the destination keep the permissions it had.
fn replace_output_file(
    temporary_output_path: &Path,
    output: &Path,
    source_permissions: Option<fs::Permissions>,
) -> Result<()> {
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
    let original_permissions = match source_permissions {
        Some(permissions) => permissions,
        None => fs::metadata(output)
            .wrap_err("failed to read existing output file metadata")
            .inspect_err(|_err| {
                let _ = fs::remove_file(temporary_output_path);
            })?
            .permissions(),
    };
    if let Err(err) = fs::set_permissions(temporary_output_path, original_permissions.clone()) {
        let _ = fs::remove_file(temporary_output_path);
        return Err(err).wrap_err("failed to apply the output's permissions to temporary file");
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
