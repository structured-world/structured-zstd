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

/// The open trace file, and what every line of this run shares.
pub struct Trace {
    file: File,
    /// The reference command's version number (`ZSTD_VERSION_NUMBER`).
    version: u32,
    /// Whether frames are (de)compressed from and into whole buffers, as the
    /// benchmark does (`single-pass`), rather than streamed (`streaming`).
    single_pass: bool,
}

impl Trace {
    /// Open `path` for appending, writing the header first when it was not a
    /// regular file, as the reference does (`TRACE_enable`).
    pub fn open(path: &Path, version: u32, single_pass: bool) -> io::Result<Self> {
        let write_header = !fs::metadata(path).is_ok_and(|metadata| metadata.is_file());
        let mut file = OpenOptions::new().append(true).create(true).open(path)?;
        if write_header {
            file.write_all(HEADER.as_bytes())?;
        }
        Ok(Self {
            file,
            version,
            single_pass,
        })
    }

    /// Append the line for `frame` (`TRACE_log`).
    pub fn record(&mut self, frame: &Frame) -> io::Result<()> {
        let line = line(self.version, self.single_pass, frame);
        self.file.write_all(line.as_bytes())
    }
}

/// The trace line for `frame`, in the reference's format: a duration of zero
/// counts as a tenth of a nanosecond, and the ratio and speed take two
/// decimals. Speed is bytes per nanosecond times a thousand, decimal MB/s.
fn line(version: u32, single_pass: bool, frame: &Frame) -> String {
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
    format!(
        "zstd, {version}, {method}, {mode}, {}, 0, {}, {}, {}, {nanos}, {ratio:.2}, {speed:.2}\n",
        frame.level, frame.dictionary_size, frame.uncompressed_size, frame.compressed_size,
    )
}

#[cfg(test)]
mod tests;
