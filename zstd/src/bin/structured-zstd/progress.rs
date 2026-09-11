//! Progress counter for the command-line tool.
//!
//! Written against `std` alone. A progress bar is a few dozen lines of
//! formatting, and the crates that provide one are the reason a tool would
//! otherwise drag an argument parser, a terminal library and a tracing
//! subscriber into a compression library's dependency graph.

use std::{
    fmt::Write as _,
    io::{Read, Write as _},
    time::{Duration, Instant},
};

use super::display::HumanSize;

/// Redraw at most this often. The work between reads is measured in
/// microseconds, so repainting per read would cost more than the compression.
const REDRAW_INTERVAL: Duration = Duration::from_millis(125);

/// Width of the drawn bar, in characters.
const BAR_WIDTH: usize = 32;

/// Room the bar line takes on the terminal, cleared before the summary.
const LINE_WIDTH: usize = BAR_WIDTH + 48;

/// A reader that counts what passes through it and, when asked, draws how far
/// along the total that is.
pub struct ProgressMonitor<R: Read> {
    /// The total amount that the reader will read. Counted in `u64` rather
    /// than `usize`: both directions stream, so a file has to fit the window,
    /// never memory, and a 32-bit counter would refuse archives the work
    /// itself handles fine.
    pub total: u64,
    /// Amount read so far
    pub read: u64,
    /// Whether the reader has reported its end.
    pub finished: bool,
    /// The internal reader
    reader: R,
    last_draw: Instant,
    /// Whether the bar is drawn at all. Decided by the caller from the
    /// verbosity, the `--progress` setting and where stderr goes, so piped
    /// stderr and `-q` runs stay clean of carriage returns.
    shown: bool,
}

impl<R: Read> ProgressMonitor<R> {
    /// Wrap `reader`, expecting `size` bytes; `shown` says whether to draw.
    pub fn new(reader: R, size: u64, shown: bool) -> Self {
        Self {
            reader,
            total: size,
            read: 0,
            last_draw: Instant::now(),
            shown,
            finished: false,
        }
    }

    /// Repaint the bar in place, throttled to [`REDRAW_INTERVAL`].
    fn draw(&mut self) {
        if !self.shown {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_draw) < REDRAW_INTERVAL {
            return;
        }
        self.last_draw = now;
        let fraction = if self.total == 0 {
            0.0
        } else {
            (self.read as f64 / self.total as f64).clamp(0.0, 1.0)
        };
        let filled = (fraction * BAR_WIDTH as f64).round() as usize;
        let mut line = String::with_capacity(LINE_WIDTH);
        line.push('\r');
        line.push('[');
        for i in 0..BAR_WIDTH {
            line.push(if i < filled { '#' } else { '-' });
        }
        let _ = write!(
            &mut line,
            "] {}/{}",
            HumanSize::new(self.read, false),
            HumanSize::new(self.total, false)
        );
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(line.as_bytes());
        let _ = err.flush();
    }

    /// Called after each read, with what that read returned.
    ///
    /// The end of the work is the reader saying it has no more, and nothing
    /// else: the total is a number from a directory entry, which a FIFO reports
    /// as zero and which a file that grew after it was measured has already
    /// passed. Ending on it would clear the bar over a stream still being read.
    /// Waiting for the reader costs one more read, which every caller here
    /// performs to find the end anyway.
    fn update(&mut self, last_read: usize) {
        let done = last_read == 0;
        if done && !self.finished {
            self.finished = true;
            // Clear the bar's line, or its leftovers trail after whatever the
            // caller prints next.
            if self.shown {
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "\r{:width$}\r", "", width = LINE_WIDTH);
                let _ = err.flush();
            }
        } else {
            self.draw();
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
        // monitor before the work did.
        if !buf.is_empty() {
            self.update(out);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
