//! `--trace FILE`: one CSV line per compressed and decompressed frame, in the
//! reference command's format (`programs/zstdcli_trace.c`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// The header the reference writes to a file that was not a regular file.
const HEADER: &str = "Algorithm, Version, Method, Mode, Level, Workers, Dictionary Size, \
                      Uncompressed Size, Compressed Size, Duration Nanos, Compression Ratio, \
                      Speed MB/s\n";

/// Which way a traced frame went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Compress,
    Decompress,
}

/// One frame, as a trace line describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub method: Method,
    /// The compression level; 0 on decompression, as the reference has it.
    pub level: i32,
    pub dictionary_size: u64,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    pub duration: Duration,
}

/// How many bytes of lines are gathered before they are written, as a `FILE*`
/// gathers them.
const BUFFERED: usize = 8 * 1024;

/// The open trace file, and what every line of this run shares. Lines are
/// gathered and written in pieces, as the reference's `FILE*` writes them: a
/// benchmark times its frames with the trace open, and a write per frame
/// would be timed with them. Closing the trace writes what is left.
pub struct Trace {
    file: File,
    /// The reference command's version number (`ZSTD_VERSION_NUMBER`).
    version: u32,
    /// Whether frames are (de)compressed from and into whole buffers, as the
    /// benchmark does (`single-pass`), rather than streamed (`streaming`).
    single_pass: bool,
    /// The lines not yet written, kept so each piece reuses its allocation.
    lines: String,
    /// The length of `lines` that is written at once.
    buffered: usize,
    /// Whether a write that fails is reported on stderr: the run's
    /// verbosity carries errors (`-qq` silences them).
    pub reports_failures: bool,
}

impl Trace {
    /// Open `path` for appending, writing the header first when it was not a
    /// regular file, as the reference does (`TRACE_enable`).
    pub fn open(path: &Path, version: u32, single_pass: bool) -> io::Result<Self> {
        let write_header = !fs::metadata(path).is_ok_and(|metadata| metadata.is_file());
        let file = OpenOptions::new().append(true).create(true).open(path)?;
        let mut lines = String::with_capacity(BUFFERED + HEADER.len());
        if write_header {
            lines.push_str(HEADER);
        }
        Ok(Self {
            file,
            version,
            single_pass,
            lines,
            buffered: BUFFERED,
            reports_failures: true,
        })
    }

    /// A trace over `file` as it was opened, so a test can hand the run one
    /// that refuses writes. Unbuffered, so the first line meets the refusal.
    #[cfg(test)]
    pub fn over(file: File, version: u32, single_pass: bool) -> Self {
        Self {
            file,
            version,
            single_pass,
            lines: String::new(),
            buffered: 0,
            reports_failures: true,
        }
    }

    /// Append the line for `frame` (`TRACE_log`). The lines gathered are
    /// dropped when their write fails, so closing the trace does not retry
    /// it.
    pub fn record(&mut self, frame: &Frame) -> io::Result<()> {
        write_line(&mut self.lines, self.version, self.single_pass, frame);
        if self.lines.len() < self.buffered {
            return Ok(());
        }
        let written = self.file.write_all(self.lines.as_bytes());
        self.lines.clear();
        written
    }
}

impl Drop for Trace {
    /// Write the lines still gathered, reporting a failure as a failed line
    /// is reported.
    fn drop(&mut self) {
        if !self.lines.is_empty()
            && let Err(err) = self.file.write_all(self.lines.as_bytes())
            && self.reports_failures
        {
            eprintln!("zstd: --trace: {err}; the last frames are not traced");
        }
    }
}

/// Write the trace line for `frame` to `out`, in the reference's format: a
/// duration of zero counts as a tenth of a nanosecond for the speed, as
/// upstream's `TRACE_log` does (`duration ? (double)duration : .1`), and the
/// ratio and speed take two decimals. Speed is bytes per nanosecond times a
/// thousand, decimal MB/s.
fn write_line(out: &mut String, version: u32, single_pass: bool, frame: &Frame) {
    use std::fmt::Write as _;
    let method = match frame.method {
        Method::Compress => "compress",
        Method::Decompress => "decompress",
    };
    let mode = if single_pass {
        "single-pass"
    } else {
        "streaming"
    };
    // Past `u64` nanoseconds is past five centuries; the field saturates there
    // rather than wrapping, since no frame takes that long.
    let nanos = u64::try_from(frame.duration.as_nanos()).unwrap_or(u64::MAX);
    let duration = if nanos == 0 { 0.1 } else { nanos as f64 };
    let ratio = frame.uncompressed_size as f64 / frame.compressed_size as f64;
    let speed = frame.uncompressed_size as f64 * 1000.0 / duration;
    // Workers: this build compresses on the calling thread only, which the
    // reference reports as 0.
    writeln!(
        out,
        "zstd, {version}, {method}, {mode}, {}, 0, {}, {}, {}, {nanos}, {ratio:.2}, {speed:.2}",
        frame.level, frame.dictionary_size, frame.uncompressed_size, frame.compressed_size,
    )
    .expect("formatting into a String cannot fail");
}

#[cfg(test)]
mod tests;
