//! What the tool says on stderr, and how loudly.
//!
//! Mirrors the reference command's display levels: `0` says nothing, `1`
//! reports errors, `2` (the default) adds the result summary, warnings and
//! interactive prompts, `3` adds progress, `4` adds information. `-v` raises
//! the level and `-q` lowers it, so `-qq` silences errors as well.

use std::fmt;
use std::io::{BufRead, Write};

/// The level a run starts at, before `-q` / `-v` move it.
pub const DEFAULT_LEVEL: i32 = 2;

/// Whether the progress counter is drawn (`--progress` / `--no-progress`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress {
    /// Drawn when stderr is a terminal and the level allows it.
    Auto,
    /// Drawn regardless of where stderr goes.
    Always,
    /// Never drawn; every other message is unaffected.
    Never,
}

impl Progress {
    /// Whether a run at `verbosity` draws the counter, given where stderr goes.
    pub fn shown(self, verbosity: i32, stderr_is_terminal: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => verbosity >= DEFAULT_LEVEL && stderr_is_terminal,
        }
    }
}

/// A byte count scaled to the unit the reference command prints it in.
///
/// Scaled in powers of two, with the precision chosen from the magnitude of
/// the scaled value: three decimals below one, two below ten, one below a
/// hundred, none above that or when the value is a whole number of units.
/// `verbose` keeps the raw byte count instead, as `-vv` does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct HumanSize {
    value: f64,
    precision: usize,
    suffix: &'static str,
}

impl HumanSize {
    /// Scale `bytes` for display.
    pub fn new(bytes: u64, verbose: bool) -> Self {
        if verbose {
            // Past the integral precision of a double the count itself is not
            // representable, so it is scaled once and rounded well up.
            return if bytes >= 1 << 53 {
                Self {
                    value: bytes as f64 / (1u64 << 20) as f64,
                    precision: 2,
                    suffix: " MiB",
                }
            } else {
                Self {
                    value: bytes as f64,
                    precision: 0,
                    suffix: " B",
                }
            };
        }
        const UNITS: [(u32, &str); 6] = [
            (60, " EiB"),
            (50, " PiB"),
            (40, " TiB"),
            (30, " GiB"),
            (20, " MiB"),
            (10, " KiB"),
        ];
        let (value, suffix) = UNITS
            .iter()
            .find(|(shift, _)| bytes >= 1u64 << shift)
            .map_or((bytes as f64, " B"), |(shift, suffix)| {
                (bytes as f64 / (1u64 << shift) as f64, *suffix)
            });
        let precision = if value >= 100.0 || value as u64 as f64 == value {
            0
        } else if value >= 10.0 {
            1
        } else if value > 1.0 {
            2
        } else {
            3
        };
        Self {
            value,
            precision,
            suffix,
        }
    }

    /// The scaled value.
    #[cfg(test)]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Decimals the scaled value is shown with.
    #[cfg(test)]
    pub fn precision(&self) -> usize {
        self.precision
    }

    /// The unit, with its leading space (`" KiB"`).
    #[cfg(test)]
    pub fn suffix(&self) -> &'static str {
        self.suffix
    }

    /// The value in a column `width` wide and the unit in one four wide: the
    /// reference command's `%6.*f%4s` layout for `-l` rows and multi-file
    /// summaries, where `" B"` is padded out to the width of `" KiB"`.
    pub fn columns(&self, width: usize) -> String {
        format!(
            "{:>width$.prec$}{:>4}",
            self.value,
            self.suffix,
            width = width,
            prec = self.precision
        )
    }
}

impl fmt::Display for HumanSize {
    /// The value at its precision, then the unit; a field width applies to the
    /// value alone, the way the reference command's `%6.*f%s` lays it out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match f.width() {
            Some(width) => write!(
                f,
                "{:>width$.prec$}{}",
                self.value,
                self.suffix,
                width = width,
                prec = self.precision
            ),
            None => write!(
                f,
                "{:.prec$}{}",
                self.value,
                self.suffix,
                prec = self.precision
            ),
        }
    }
}

/// Ask the user a yes/no question on stderr and read the answer from `input`.
///
/// Returns `true` only for an answer starting with `y` or `Y`. The reference
/// command refuses to ask at all when stdin is one of the inputs: the answer
/// would be read out of the data. `abort_message` is printed on a refusal so
/// the caller can say what did not happen.
pub fn confirm(
    prompt: &str,
    abort_message: &str,
    stdin_is_input: bool,
    input: &mut impl BufRead,
) -> bool {
    let mut stderr = std::io::stderr().lock();
    if stdin_is_input {
        let _ = writeln!(stderr, "stdin is an input - not proceeding.");
        return false;
    }
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    let mut answer = String::new();
    // The first character decides, as the reference command's `getchar` does:
    // a space before the `y` is not a yes.
    let accepted =
        input.read_line(&mut answer).is_ok() && matches!(answer.chars().next(), Some('y' | 'Y'));
    if !accepted {
        let _ = writeln!(stderr, "{abort_message}");
    }
    accepted
}

#[cfg(test)]
mod tests;
