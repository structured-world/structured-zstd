//! Progress display for the command-line tool.
//!
//! Written against `std` alone. A progress bar is a few dozen lines of
//! formatting, and the crates that provide one are the reason a tool would
//! otherwise drag an argument parser, a terminal library and a tracing
//! subscriber into a compression library's dependency graph.

use std::{
    fmt::Write as _,
    io::{IsTerminal, Read, Write as _},
    time::{Duration, Instant},
};

/// Redraw at most this often. The work between reads is measured in
/// microseconds, so repainting per read would cost more than the compression.
const REDRAW_INTERVAL: Duration = Duration::from_millis(125);

/// Width of the drawn bar, in characters.
const BAR_WIDTH: usize = 32;

/// A generic wrapper around a reader that keeps track of how many bytes have been read
/// from the total.
pub struct ProgressMonitor<R: Read> {
    /// The total amount that the reader will read. Counted in `u64` rather
    /// than `usize`: both directions stream, so a file has to fit the window,
    /// never memory, and a 32-bit counter would refuse archives the work
    /// itself handles fine.
    pub total: u64,
    /// Amount read so far
    pub read: u64,
    /// Whether the summary has been printed, which happens once the reader
    /// says it is done.
    pub finished: bool,
    /// The internal reader
    reader: R,
    started: Instant,
    last_draw: Instant,
    /// Only draw when stderr is a terminal: piped output must stay clean.
    interactive: bool,
}

impl<R: Read> ProgressMonitor<R> {
    /// Create a new progress monitor, initialized with zero bytes read
    pub fn new(reader: R, size: u64) -> Self {
        let now = Instant::now();
        Self {
            reader,
            total: size,
            read: 0,
            started: now,
            last_draw: now,
            interactive: std::io::stderr().is_terminal(),
            finished: false,
        }
    }

    /// Repaint the bar in place, throttled to [`REDRAW_INTERVAL`].
    fn draw(&mut self, force: bool) {
        if !self.interactive {
            return;
        }
        let now = Instant::now();
        if !force && now.duration_since(self.last_draw) < REDRAW_INTERVAL {
            return;
        }
        self.last_draw = now;
        let fraction = if self.total == 0 {
            0.0
        } else {
            (self.read as f64 / self.total as f64).clamp(0.0, 1.0)
        };
        let filled = (fraction * BAR_WIDTH as f64).round() as usize;
        let mut line = String::with_capacity(BAR_WIDTH + 48);
        line.push('\r');
        line.push('[');
        for i in 0..BAR_WIDTH {
            line.push(if i < filled { '#' } else { '-' });
        }
        let _ = write!(
            &mut line,
            "] {}/{}",
            fmt_size(self.read as f64),
            fmt_size(self.total as f64)
        );
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(line.as_bytes());
        let _ = err.flush();
    }

    /// Called after each read, with what that read returned.
    ///
    /// The end of the work is the reader saying it has no more, not the byte
    /// count meeting a number from a directory entry: a FIFO reports zero, and
    /// a file can shrink after it was measured. Waiting for the two to meet
    /// leaves the bar redrawing and the summary unprinted. A known total that
    /// is reached still finishes right away, so a regular file's summary
    /// arrives with its last byte rather than a read later.
    fn update(&mut self, last_read: usize) {
        let done = last_read == 0 || (self.total > 0 && self.read >= self.total);
        if done && !self.finished {
            self.finished = true;
            // Clear the bar's line before the summary, or the leftovers of the
            // longer bar line trail after it.
            if self.interactive {
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "\r{:width$}\r", "", width = BAR_WIDTH + 48);
                let _ = err.flush();
            }
            // Reported from what was actually read: the declared total can be
            // zero for a stream, or stale for a file that changed size.
            let elapsed = self.started.elapsed();
            let rate = if elapsed.as_secs_f64() > 0.0 {
                fmt_size(self.read as f64 / elapsed.as_secs_f64())
            } else {
                fmt_size(self.read as f64)
            };
            eprintln!(
                "processed {} in {} ({rate}/s avg)",
                fmt_size(self.read as f64),
                fmt_duration(elapsed),
            );
        } else {
            self.draw(false);
        }
    }
}

impl<R: Read> Read for ProgressMonitor<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Fall back on the internally stored reader, but filch the number of bytes read
        // along the way
        let out = self.reader.read(buf)?;
        // One read is bounded by the buffer, so only the running total needs
        // the wider type.
        self.read += out as u64;
        // `Ok(0)` means the end of the stream only when there was room to read
        // into: the contract gives the same answer for an empty buffer, which
        // says nothing about the reader. Taking it as the end would finish the
        // monitor before the work did, and the summary would never come.
        if !buf.is_empty() {
            self.update(out);
        }
        Ok(out)
    }
}

/// Converts a quantity in bytes to a human readable size, "GiB, MiB, KiB, etc"
pub fn fmt_size(size_in_bytes: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let order_of_magnitude = (size_in_bytes).log10() as usize;
    // Overflow to the next order of magnitude if there are more than `upper_bound` figures
    // before the decimal
    let upper_bound = 3;
    let unit_index = (order_of_magnitude / upper_bound).clamp(0, units.len() - 1);
    let decimal = size_in_bytes / 2_f64.powi((unit_index * 10) as i32);
    // Only use a decimal if displaying a unit larger than a byte
    if unit_index > 0 {
        format!("{:.2}{}", decimal, units[unit_index])
    } else {
        format!("{:.0}{}", decimal, units[unit_index])
    }
}

/// Converts a [`std::time::Duration`] to a human readable format
fn fmt_duration(duration: Duration) -> String {
    let as_secs = duration.as_secs_f64();
    let as_min = (as_secs / 60.0).floor() as usize;
    // When displayed in long form, the value shown
    let secs_portion: f64 = as_secs % 60.0;
    let min_portion: usize = ((as_secs - secs_portion) as usize / 60) % 60;
    let hr_portion: usize = ((as_min - min_portion) / 60) % 60;

    let mut output = String::with_capacity(8);
    if hr_portion > 0 {
        write!(&mut output, "{hr_portion}h ").unwrap();
    }
    if min_portion > 0 {
        write!(&mut output, "{min_portion}m ").unwrap();
    }
    // Formatting for seconds is fairly manual
    // to provide a "useful" level of precision
    if as_secs > 60.0 && secs_portion != 0.0 {
        // Zero points of precision
        write!(&mut output, "{:.0}s", secs_portion.round()).unwrap();
    } else if secs_portion > 4.0 {
        // One point of precision
        write!(&mut output, "{secs_portion:.1}s").unwrap();
    } else if secs_portion > 1.0 {
        // Two points of precision
        write!(&mut output, "{secs_portion:.2}s").unwrap();
    } else if secs_portion > 0.0 {
        // Display as ms with two units of precision
        write!(&mut output, "{:.2}ms", secs_portion * 1000.0).unwrap();
    }
    output.trim().to_string()
}

#[cfg(test)]
mod tests;
