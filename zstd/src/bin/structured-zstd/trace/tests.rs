use std::time::Duration;

use super::{Frame, HEADER, Method, Trace, line};

/// A line carries the reference's fields in its order and formatting:
/// version, method, mode, level, workers, dictionary, sizes, nanoseconds,
/// then ratio and speed to two decimals.
#[test]
fn a_line_is_the_reference_format() {
    let frame = Frame {
        method: Method::Compress,
        level: 19,
        dictionary_size: 4096,
        uncompressed_size: 1_000_000,
        compressed_size: 250_000,
        duration: Duration::from_millis(2),
    };
    assert_eq!(
        line(10507, false, &frame),
        "zstd, 10507, compress, streaming, 19, 0, 4096, 1000000, 250000, 2000000, 4.00, 500.00\n"
    );
    let decoded = Frame {
        method: Method::Decompress,
        level: 0,
        ..frame
    };
    assert!(line(10507, true, &decoded).starts_with("zstd, 10507, decompress, single-pass, 0, 0,"));
}

/// A frame decoded in no measurable time is reported at a tenth of a
/// nanosecond for its speed, as the reference reports it, not as a division
/// by zero.
#[test]
fn a_zero_duration_reads_as_a_tenth_of_a_nanosecond() {
    let frame = Frame {
        method: Method::Decompress,
        level: 0,
        dictionary_size: 0,
        uncompressed_size: 10,
        compressed_size: 5,
        duration: Duration::ZERO,
    };
    assert!(line(10507, false, &frame).ends_with(", 0, 2.00, 100000.00\n"));
}

/// The header is written once, to a file that was not there; reopening the
/// file appends lines under the header already in it.
#[test]
fn the_header_is_written_only_to_a_new_file() {
    let dir = std::env::temp_dir().join(format!("structured-zstd-trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.csv");
    let _ = std::fs::remove_file(&path);
    let frame = Frame {
        method: Method::Compress,
        level: 3,
        dictionary_size: 0,
        uncompressed_size: 8,
        compressed_size: 4,
        duration: Duration::from_nanos(8),
    };
    for _ in 0..2 {
        let mut trace = Trace::open(&path, 10507, false).unwrap();
        trace.record(&frame).unwrap();
    }
    let written = std::fs::read_to_string(&path).unwrap();
    let expected_line = line(10507, false, &frame);
    assert_eq!(written, format!("{HEADER}{expected_line}{expected_line}"));
    std::fs::remove_dir_all(&dir).unwrap();
}
