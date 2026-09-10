use super::*;

#[test]
fn strategy_ordinals_round_trip() {
    for ordinal in 1..=9 {
        let s = Strategy::from_ordinal(ordinal).expect("valid ordinal");
        assert_eq!(s.ordinal(), ordinal);
    }
    assert_eq!(Strategy::from_ordinal(0), None);
    assert_eq!(Strategy::from_ordinal(10), None);
}

#[test]
fn builder_default_overrides_nothing() {
    let p = CompressionParameters::builder(CompressionLevel::Level(7))
        .build()
        .unwrap();
    assert!(p.overrides().is_empty());
    assert_eq!(p.level(), CompressionLevel::Level(7));
    assert!(!p.long_distance_matching_enabled());
}

#[test]
fn builder_records_each_knob() {
    let p = CompressionParameters::builder(CompressionLevel::Level(19))
        .window_log(22)
        .hash_log(23)
        .chain_log(24)
        .search_log(7)
        .min_match(4)
        .target_length(256)
        .strategy(Strategy::Btultra2)
        .build()
        .unwrap();
    let o = p.overrides();
    assert_eq!(o.window_log, Some(22));
    assert_eq!(o.hash_log, Some(23));
    assert_eq!(o.chain_log, Some(24));
    assert_eq!(o.search_log, Some(7));
    assert_eq!(o.min_match, Some(4));
    assert_eq!(o.target_length, Some(256));
    assert_eq!(o.strategy, Some(Strategy::Btultra2));
    assert!(!o.is_empty());
}

/// The literal mode is a knob like the others: recorded by the builder, read
/// back from the parameters, and an override of the level when it is not
/// `Auto`, since a non-empty override set is what the reset path acts on.
#[test]
fn literal_compression_mode_is_recorded_and_counts_as_an_override() {
    let auto = CompressionParameters::builder(CompressionLevel::Level(3))
        .build()
        .unwrap();
    assert_eq!(
        auto.literal_compression_mode(),
        LiteralCompressionMode::Auto
    );
    assert!(auto.overrides().is_empty());

    for mode in [
        LiteralCompressionMode::Enable,
        LiteralCompressionMode::Disable,
    ] {
        let p = CompressionParameters::builder(CompressionLevel::Level(3))
            .literal_compression(mode)
            .build()
            .unwrap();
        assert_eq!(p.literal_compression_mode(), mode);
        assert!(!p.overrides().is_empty(), "{mode:?} overrides the level");
    }
}

/// `Disable` stores every literal raw, so text compresses worse than the
/// level's default; `Enable` on a negative level, where the default is raw,
/// compresses better. Both frames still decode to the input.
#[test]
fn literal_compression_mode_changes_the_frame() {
    use crate::decoding::StreamingDecoder;
    use crate::encoding::compress_with_parameters;
    use crate::io::Read;

    // Literal-heavy input: 32 distinct symbols in a sequence with no repeats
    // for the match finder, so the frame is all literals and only their
    // entropy coding can shrink it (5 bits a symbol against 8 raw).
    let text: alloc::vec::Vec<u8> = (0..8192u32)
        .map(|i| b'a' + (i.wrapping_mul(2_654_435_761) >> 27) as u8)
        .collect();
    let frame_with = |level: i32, mode: LiteralCompressionMode| {
        let params = CompressionParameters::builder(CompressionLevel::Level(level))
            .literal_compression(mode)
            .build()
            .unwrap();
        compress_with_parameters(&text[..], &params)
    };
    let decoded = |frame: &[u8]| {
        let mut source = frame;
        let mut out = alloc::vec::Vec::new();
        StreamingDecoder::new(&mut source)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    };

    let auto_l3 = frame_with(3, LiteralCompressionMode::Auto);
    let raw_l3 = frame_with(3, LiteralCompressionMode::Disable);
    assert!(
        raw_l3.len() > auto_l3.len(),
        "raw literals cost bytes on text: {} vs {}",
        raw_l3.len(),
        auto_l3.len()
    );
    assert_eq!(decoded(&raw_l3), text);

    let auto_fast = frame_with(-3, LiteralCompressionMode::Auto);
    let coded_fast = frame_with(-3, LiteralCompressionMode::Enable);
    assert!(
        coded_fast.len() < auto_fast.len(),
        "compressed literals save bytes at a negative level: {} vs {}",
        coded_fast.len(),
        auto_fast.len()
    );
    assert_eq!(decoded(&coded_fast), text);
    // `Auto` at a negative level is raw literals, which is what `Disable`
    // spells out, so the two agree there.
    assert_eq!(
        auto_fast,
        frame_with(-3, LiteralCompressionMode::Disable),
        "the negative level's default is raw literals"
    );
}

#[test]
fn enable_ldm_sets_override_block() {
    let p = CompressionParameters::builder(CompressionLevel::Level(19))
        .enable_long_distance_matching(true)
        .build()
        .unwrap();
    assert!(p.long_distance_matching_enabled());
    assert_eq!(p.overrides().ldm, Some(LdmOverride::default()));
}

#[test]
fn ldm_knob_implies_enable() {
    let p = CompressionParameters::builder(CompressionLevel::Level(19))
        .ldm_hash_log(24)
        .ldm_min_match(64)
        .ldm_bucket_size_log(4)
        .ldm_hash_rate_log(7)
        .build()
        .unwrap();
    assert!(p.long_distance_matching_enabled());
    let ldm = p.overrides().ldm.unwrap();
    assert_eq!(ldm.hash_log, Some(24));
    assert_eq!(ldm.min_match, Some(64));
    assert_eq!(ldm.bucket_size_log, Some(4));
    assert_eq!(ldm.hash_rate_log, Some(7));
}

#[test]
fn out_of_bounds_window_log_rejected() {
    let err = CompressionParameters::builder(CompressionLevel::Default)
        .window_log(31)
        .build()
        .unwrap_err();
    match err {
        ParameterError::OutOfBounds {
            parameter, value, ..
        } => {
            assert_eq!(parameter, CParameter::WindowLog);
            assert_eq!(value, 31);
        }
    }
}

#[test]
fn out_of_bounds_min_match_rejected() {
    let err = CompressionParameters::builder(CompressionLevel::Default)
        .min_match(2)
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        ParameterError::OutOfBounds {
            parameter: CParameter::MinMatch,
            ..
        }
    ));
}

#[test]
fn ldm_bounds_only_checked_when_enabled() {
    // An out-of-range LDM knob is only rejected when LDM is on. A
    // builder that never enables LDM ignores the (unreachable)
    // values entirely.
    let err = CompressionParameters::builder(CompressionLevel::Default)
        .ldm_bucket_size_log(9)
        .build()
        .unwrap_err();
    assert!(matches!(
        err,
        ParameterError::OutOfBounds {
            parameter: CParameter::LdmBucketSizeLog,
            ..
        }
    ));
}

#[test]
fn bounds_match_c_reference() {
    assert_eq!(
        CParameter::WindowLog.bounds(),
        Bounds {
            lower_bound: 10,
            upper_bound: 30
        }
    );
    assert_eq!(
        CParameter::Strategy.bounds(),
        Bounds {
            lower_bound: 1,
            upper_bound: 9
        }
    );
    assert_eq!(
        CParameter::TargetLength.bounds(),
        Bounds {
            lower_bound: 0,
            upper_bound: 131_072
        }
    );
    assert!(CParameter::MinMatch.bounds().contains(3));
    assert!(CParameter::MinMatch.bounds().contains(7));
    assert!(!CParameter::MinMatch.bounds().contains(8));
}
