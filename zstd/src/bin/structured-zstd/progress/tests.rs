use std::io::{self, Read};

use super::ProgressMonitor;

/// The monitor is done when the reader is done, and "done" is the reader
/// saying so, not the byte count matching a number from a directory entry. A
/// FIFO reports zero, a file can shrink after it was measured, and in both
/// cases a monitor that waits for the two to meet waits forever.
#[test]
fn progress_finishes_when_the_reader_does_not_when_the_count_matches() {
    // A reader with more bytes than the total it was created with.
    let mut monitor = ProgressMonitor::new(&b"bytes that were not counted"[..], 0, false);
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
    let mut monitor = ProgressMonitor::new(&b"payload"[..], 7, false);
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
    let monitor = ProgressMonitor::new(&b""[..], huge, false);
    assert_eq!(
        monitor.total, huge,
        "a length larger than a 32-bit pointer must survive"
    );
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
    let mut monitor = ProgressMonitor::new(Broken, 10, false);
    let mut buf = [0u8; 4];
    let err = monitor.read(&mut buf).expect_err("the error must surface");
    assert_eq!(err.to_string(), "disk on fire");
    assert_eq!(monitor.read, 0, "nothing was read");
    assert!(!monitor.finished, "an error is not the end of the stream");
}
