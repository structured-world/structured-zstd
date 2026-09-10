use super::{DEFAULT_LEVEL, HumanSize, Progress, confirm};

/// The reference command scales sizes in powers of two and picks the number
/// of decimals from the scaled value, so `-l` columns and result summaries
/// line up with what its own output shows. A whole number of units drops the
/// decimals entirely; anything else keeps three significant figures.
#[test]
fn sizes_scale_and_round_the_way_the_reference_prints_them() {
    assert_eq!(HumanSize::new(0, false).to_string(), "0 B");
    assert_eq!(HumanSize::new(100, false).to_string(), "100 B");
    assert_eq!(HumanSize::new(1023, false).to_string(), "1023 B");
    assert_eq!(HumanSize::new(1024, false).to_string(), "1 KiB");
    assert_eq!(HumanSize::new(1025, false).to_string(), "1.00 KiB");
    assert_eq!(HumanSize::new(1536, false).to_string(), "1.50 KiB");
    assert_eq!(
        HumanSize::new(10 * 1024 + 512, false).to_string(),
        "10.5 KiB"
    );
    assert_eq!(HumanSize::new(12 * 1024, false).to_string(), "12 KiB");
    assert_eq!(HumanSize::new(7 << 20, false).to_string(), "7 MiB");
    assert_eq!(HumanSize::new(123 << 30, false).to_string(), "123 GiB");
    assert_eq!(HumanSize::new(150 << 30, false).to_string(), "150 GiB");
    assert_eq!(HumanSize::new(1 << 40, false).to_string(), "1 TiB");
}

/// Below one unit the value keeps three decimals: a file just under a
/// kibibyte scaled to KiB would otherwise print as `1 KiB`, which it is not.
#[test]
fn a_fraction_of_a_unit_keeps_three_decimals() {
    let size = HumanSize::new((1 << 20) + 1, false);
    assert_eq!(size.precision(), 2);
    let just_over_a_unit = HumanSize::new(1024 + 1, false);
    assert_eq!(just_over_a_unit.to_string(), "1.00 KiB");
    // Only values strictly above one unit reach the two-decimal branch; a
    // value scaled to exactly one unit is whole and prints without decimals.
    assert_eq!(HumanSize::new(1 << 30, false).precision(), 0);
}

/// `-vv` keeps the raw byte count: a summary being pasted into a report wants
/// the exact figure, not a rounded one.
#[test]
fn verbose_sizes_are_raw_byte_counts() {
    assert_eq!(HumanSize::new(1536, true).to_string(), "1536 B");
    assert_eq!(HumanSize::new(0, true).to_string(), "0 B");
    // Past the integral precision of a double the count itself cannot be
    // shown exactly, so it is scaled once.
    assert_eq!(HumanSize::new(1 << 53, true).suffix(), " MiB");
}

/// A field width lays out the number and leaves the unit attached: that is how
/// the reference command's `%6.*f%4s` keeps its columns aligned.
#[test]
fn a_field_width_pads_the_number_not_the_unit() {
    assert_eq!(format!("{:>6}", HumanSize::new(100, false)), "   100 B");
    assert_eq!(format!("{:>6}", HumanSize::new(1536, false)), "  1.50 KiB");
    assert_eq!(HumanSize::new(1536, false).value(), 1.5);
}

/// The progress counter is drawn for a person watching a terminal, and only
/// then unless asked for outright: piped stderr must stay clean of carriage
/// returns, and `-q` says not to decorate.
#[test]
fn progress_is_drawn_for_a_terminal_at_the_default_level_unless_forced() {
    assert!(Progress::Auto.shown(DEFAULT_LEVEL, true));
    assert!(!Progress::Auto.shown(DEFAULT_LEVEL, false));
    assert!(!Progress::Auto.shown(DEFAULT_LEVEL - 1, true));
    assert!(Progress::Always.shown(0, false));
    assert!(!Progress::Never.shown(4, true));
}

/// A prompt is answered by the first character of the line, `y` or `Y`, and
/// nothing else: an empty answer, a refusal, or the end of input all decline.
#[test]
fn only_a_yes_confirms() {
    assert!(confirm("go? ", "no", false, &mut &b"y\n"[..]));
    assert!(confirm("go? ", "no", false, &mut &b"Y\n"[..]));
    assert!(confirm("go? ", "no", false, &mut &b"yes please\n"[..]));
    assert!(!confirm("go? ", "no", false, &mut &b"n\n"[..]));
    assert!(!confirm("go? ", "no", false, &mut &b"\n"[..]));
    assert!(!confirm("go? ", "no", false, &mut &b""[..]));
    // The first character decides, as the reference command reads it.
    assert!(!confirm("go? ", "no", false, &mut &b"  y\n"[..]));
}

/// When stdin is one of the inputs, the answer would be read out of the data
/// being compressed. The reference command refuses to ask; so does this one,
/// and it must not consume a byte of the input while declining.
#[test]
fn no_prompt_is_read_from_an_input_stream() {
    let mut input = &b"y\n"[..];
    assert!(!confirm("go? ", "no", true, &mut input));
    assert_eq!(input, b"y\n", "the input must be left untouched");
}
