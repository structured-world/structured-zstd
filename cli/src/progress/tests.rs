
use std::time::Duration;

use super::{fmt_duration, fmt_size};

#[test]
fn human_readable_filesize() {
    // Bytes
    assert_eq!(&fmt_size(100.0), "100B");
    // Kibibytes
    assert_eq!(&fmt_size(12.0 * 2.0_f64.powi(10)), "12.00KiB");
    // Mebibytes
    assert_eq!(&fmt_size(7.0 * 2.0_f64.powi(20)), "7.00MiB");
    // Gibibytes
    assert_eq!(&fmt_size(123.0 * 2.0_f64.powi(30)), "123.00GiB");
}

#[test]
fn human_readable_duration() {
    assert_eq!(&fmt_duration(Duration::from_millis(7)), "7.00ms");
    assert_eq!(&fmt_duration(Duration::from_millis(1500)), "1.50s");
    assert_eq!(&fmt_duration(Duration::from_secs(30)), "30.0s");
    assert_eq!(&fmt_duration(Duration::from_secs(90)), "1m 30s");
    assert_eq!(&fmt_duration(Duration::from_secs(5 * 60)), "5m");
    assert_eq!(&fmt_duration(Duration::from_secs(3 * 60 * 60)), "3h");
    assert_eq!(
        &fmt_duration(Duration::from_secs(60 * 60 + 20 * 60 + 30)),
        "1h 20m 30s"
    );
}
