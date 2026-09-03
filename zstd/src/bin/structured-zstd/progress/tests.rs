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

/// The seconds are shown rounded, and a value that rounds up to a full minute
/// belongs in the minutes: printed as it stands it reads `1m 60s`, a duration
/// no clock shows. The carry runs all the way up, so 59m 59.6s is an hour.
#[test]
fn rounded_seconds_carry_into_the_next_minute() {
    assert_eq!(&fmt_duration(Duration::from_millis(119_500)), "2m");
    // Under a minute the seconds carry a decimal, so the carry is what that
    // decimal rounds to: 59.96 shown to one place is a minute.
    assert_eq!(&fmt_duration(Duration::from_millis(59_960)), "1m");
    assert_eq!(
        &fmt_duration(Duration::from_millis(3_599_600)),
        "1h",
        "the carry runs past the minutes as well"
    );
}
