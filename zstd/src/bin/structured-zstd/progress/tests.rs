use std::io::{self, Read};

use super::ProgressMonitor;

/// The monitor is done when the reader is done, and "done" is the reader
/// saying so, not the byte count matching a number from a directory entry. A
/// FIFO reports zero, a file can shrink after it was measured, and in both
/// cases a monitor that waits for the two to meet waits forever.
#[test]
fn progress_finishes_when_the_reader_does_not_when_the_count_matches() {
    // A reader with more bytes than the total it was created with.
    let mut monitor = ProgressMonitor::new(&b"bytes that were not counted"[..], Some(0), false);
    let mut sink = Vec::new();
    io::copy(&mut monitor, &mut sink).expect("copying must succeed");
    assert!(
        monitor.finished,
        "the reader reached its end, so the monitor has to be finished too"
    );
    assert_eq!(monitor.read, sink.len() as u64, "and count what it read");
}

/// `Read::read` answers `Ok(0)` for an empty buffer as well as for the end of
/// the stream, as the contract says. Treating the first as the second finishes
/// the monitor before any bytes have moved.
#[test]
fn an_empty_buffer_read_is_not_the_end_of_the_stream() {
    let mut monitor = ProgressMonitor::new(&b"payload"[..], Some(7), false);
    assert_eq!(monitor.read(&mut []).unwrap(), 0);
    assert!(
        !monitor.finished,
        "an empty buffer says nothing about the reader"
    );

    let mut buf = [0u8; 7];
    assert_eq!(monitor.read(&mut buf).unwrap(), 7);
    assert!(
        !monitor.finished,
        "a total taken from a directory entry does not end the stream"
    );
    assert_eq!(monitor.read(&mut buf).unwrap(), 0);
    assert!(monitor.finished, "the reader saying so does");
}

/// Both directions stream, so a file only has to fit the window, never memory.
/// Measuring its length in `usize` puts a 4 GiB ceiling on 32-bit targets that
/// has nothing to do with what the work needs: the progress counter would be
/// deciding which archives the tool can open.
#[test]
fn a_file_length_is_not_narrowed_to_the_pointer_width() {
    let huge = u64::from(u32::MAX) + 1;
    let monitor = ProgressMonitor::new(&b""[..], Some(huge), false);
    assert_eq!(
        monitor.total,
        Some(huge),
        "a length larger than a 32-bit pointer must survive"
    );
}

/// The bar fills in proportion to what has been read of the total: half of
/// it is half the bar, a read past a total that turned out short fills it and
/// no more, and a total of zero (a FIFO's directory entry) fills none. With
/// no total at all there is no bar, only the count.
#[test]
fn the_bar_fills_with_the_share_read() {
    let filled = |read: u64, total: Option<u64>| {
        let mut monitor = ProgressMonitor::new(&b""[..], total, true);
        monitor.read = read;
        let line = monitor.line();
        assert!(
            line.starts_with('\r'),
            "{line:?}: drawn from the line start"
        );
        line.matches('#').count()
    };
    assert_eq!(filled(1000, Some(2000)), super::BAR_WIDTH / 2);
    assert_eq!(filled(5000, Some(2000)), super::BAR_WIDTH);
    assert_eq!(filled(1000, Some(0)), 0);
    let mut counting = ProgressMonitor::new(&b""[..], None, true);
    counting.read = 1000;
    let line = counting.line();
    assert!(line.starts_with("\rRead : "), "{line:?}");
    assert!(!line.contains('['), "{line:?}: no bar without a total");
}

/// A shown monitor redraws no more often than the interval, draws once the
/// interval has passed, and clears its line when the reader ends.
#[test]
fn a_shown_monitor_redraws_on_the_interval_and_clears_at_the_end() {
    let data = [7u8; 64];
    let mut monitor = ProgressMonitor::new(&data[..], Some(data.len() as u64), true);
    assert!(monitor.is_shown());
    let mut buf = [0u8; 16];
    let drawn_at = monitor.last_draw;
    assert_eq!(monitor.read(&mut buf).unwrap(), buf.len());
    assert_eq!(
        monitor.last_draw, drawn_at,
        "within the interval: no redraw"
    );
    let overdue = std::time::Instant::now()
        .checked_sub(super::REDRAW_INTERVAL * 2)
        .expect("the clock reaches back one interval");
    monitor.last_draw = overdue;
    assert_eq!(monitor.read(&mut buf).unwrap(), buf.len());
    assert!(monitor.last_draw > overdue, "past the interval: redrawn");
    let mut sink = Vec::new();
    io::copy(&mut monitor, &mut sink).unwrap();
    assert!(monitor.finished, "and the end clears it");
}

/// A read error is the reader's to report; the monitor passes it through
/// without counting bytes that never arrived or declaring the stream done.
#[test]
fn a_read_error_passes_through_uncounted() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("disk on fire"))
        }
    }
    let mut monitor = ProgressMonitor::new(Broken, Some(10), false);
    let mut buf = [0u8; 4];
    let err = monitor.read(&mut buf).expect_err("the error must surface");
    assert_eq!(err.to_string(), "disk on fire");
    assert_eq!(monitor.read, 0, "nothing was read");
    assert!(!monitor.finished, "an error is not the end of the stream");
}
