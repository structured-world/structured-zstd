use super::*;
use std::format;

fn corpus() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..500u32 {
        data.extend_from_slice(
            format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccdddd\n")
                .as_bytes(),
        );
    }
    data
}

#[test]
fn fastcover_raw_produces_non_empty_dict() {
    let sample = corpus();
    let dict = train_fastcover_raw(
        sample.as_slice(),
        4096,
        FastCoverParams {
            k: 256,
            d: 8,
            f: 20,
            accel: 1,
        },
    );
    assert!(!dict.is_empty());
    assert!(dict.len() <= 4096);
}

#[test]
fn fastcover_raw_returns_empty_for_empty_or_zero_budget() {
    let sample = corpus();
    let params = FastCoverParams {
        k: 256,
        d: 8,
        f: 20,
        accel: 1,
    };
    assert!(train_fastcover_raw(&[], 1024, params).is_empty());
    assert!(train_fastcover_raw(sample.as_slice(), 0, params).is_empty());
}

#[test]
fn fastcover_optimizer_selects_valid_params() {
    let sample = corpus();
    let (dict, tuned) = optimize_fastcover_raw(
        sample.as_slice(),
        4096,
        0.75,
        1,
        &[6, 8],
        &[18, 20],
        &[128, 256],
    );
    assert!(!dict.is_empty());
    assert!([6, 8].contains(&tuned.d));
    assert!([18, 20].contains(&tuned.f));
    assert!([128, 256].contains(&tuned.k));
}

#[test]
fn fastcover_optimizer_falls_back_when_k_candidates_empty() {
    let sample = corpus();
    let (dict, tuned) =
        optimize_fastcover_raw(sample.as_slice(), 4096, 0.75, 1, &[6, 8], &[18, 20], &[]);
    assert!(!dict.is_empty());
    assert!(DEFAULT_K_CANDIDATES.contains(&tuned.k));
}

#[test]
fn fastcover_optimizer_handles_one_byte_sample_without_panic() {
    let sample = [0xAB];
    let (dict, tuned) = optimize_fastcover_raw(&sample, 16, 0.75, 1, &[], &[], &[]);
    assert!(!dict.is_empty());
    assert!(dict.len() <= 16);
    assert!(DEFAULT_K_CANDIDATES.contains(&tuned.k));
    assert!(DEFAULT_D_CANDIDATES.contains(&tuned.d));
    assert!(DEFAULT_F_CANDIDATES.contains(&tuned.f));
}

#[test]
fn fastcover_optimizer_seeds_winner_when_all_scores_are_zero() {
    let sample = b"abcdefghijklmnopqrst";
    let (dict, tuned) = optimize_fastcover_raw(sample, 16, 0.9, 1, &[6], &[16], &[8]);
    assert!(!dict.is_empty());
    assert_eq!(tuned.k, 16);
    assert_eq!(tuned.d, 6);
    assert_eq!(tuned.f, 16);
    assert_eq!(tuned.score, 0);
}

#[test]
fn fastcover_optimizer_handles_zero_dict_budget() {
    let sample = corpus();
    let (dict, tuned) = optimize_fastcover_raw(
        sample.as_slice(),
        0,
        0.75,
        1,
        &[6, 8],
        &[18, 20],
        &[128, 256],
    );
    assert!(dict.is_empty());
    assert!([6, 8].contains(&tuned.d));
    assert!([18, 20].contains(&tuned.f));
    assert!([128, 256].contains(&tuned.k));
}

#[test]
fn fastcover_optimizer_clamps_extreme_split_points() {
    let sample = corpus();
    let (dict_low, tuned_low) =
        optimize_fastcover_raw(sample.as_slice(), 2048, 0.0, 1, &[6], &[18], &[128]);
    let (dict_high, tuned_high) =
        optimize_fastcover_raw(sample.as_slice(), 2048, 1.0, 1, &[6], &[18], &[128]);
    assert!(!dict_low.is_empty());
    assert!(!dict_high.is_empty());
    assert_eq!(tuned_low.k, 128);
    assert_eq!(tuned_high.k, 128);
}

#[test]
fn fastcover_optimizer_reports_normalized_params() {
    let sample = corpus();
    let (dict, tuned) =
        optimize_fastcover_raw(sample.as_slice(), 1024, 0.75, 1, &[64], &[42], &[8]);
    assert!(!dict.is_empty());
    assert_eq!(tuned.d, 32);
    assert_eq!(tuned.f, 20);
    assert_eq!(tuned.k, 32);
}
