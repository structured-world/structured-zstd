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
