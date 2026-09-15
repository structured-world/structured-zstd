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
    )
    .unwrap();
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
    assert!(train_fastcover_raw(&[], 1024, params).unwrap().is_empty());
    assert!(
        train_fastcover_raw(sample.as_slice(), 0, params)
            .unwrap()
            .is_empty()
    );
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
    )
    .unwrap();
    assert!(!dict.is_empty());
    assert!([6, 8].contains(&tuned.d));
    assert!([18, 20].contains(&tuned.f));
    assert!([128, 256].contains(&tuned.k));
}

#[test]
fn fastcover_optimizer_falls_back_when_k_candidates_empty() {
    let sample = corpus();
    let (dict, tuned) =
        optimize_fastcover_raw(sample.as_slice(), 4096, 0.75, 1, &[6, 8], &[18, 20], &[]).unwrap();
    assert!(!dict.is_empty());
    assert!(DEFAULT_K_CANDIDATES.contains(&tuned.k));
}

#[test]
fn fastcover_optimizer_handles_one_byte_sample_without_panic() {
    let sample = [0xAB];
    let (dict, tuned) = optimize_fastcover_raw(&sample, 16, 0.75, 1, &[], &[], &[]).unwrap();
    assert!(!dict.is_empty());
    assert!(dict.len() <= 16);
    assert!(DEFAULT_K_CANDIDATES.contains(&tuned.k));
    assert!(DEFAULT_D_CANDIDATES.contains(&tuned.d));
    assert!(DEFAULT_F_CANDIDATES.contains(&tuned.f));
}

#[test]
fn fastcover_optimizer_seeds_winner_when_all_scores_are_zero() {
    let sample = b"abcdefghijklmnopqrst";
    let (dict, tuned) = optimize_fastcover_raw(sample, 16, 0.9, 1, &[6], &[16], &[8]).unwrap();
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
    )
    .unwrap();
    assert!(dict.is_empty());
    assert!([6, 8].contains(&tuned.d));
    assert!([18, 20].contains(&tuned.f));
    assert!([128, 256].contains(&tuned.k));
}

/// The split is honoured as given. At 1 the whole corpus both trains and
/// scores, as upstream's `splitPoint == 1.0` does (fastcover.c,
/// `FASTCOVER_ctx_init`), and a small share trains on that share, rather than
/// either being pulled into a fixed band behind the caller's back.
#[test]
fn fastcover_optimizer_honours_the_split_it_is_given() {
    let sample = corpus();
    let params = normalize_fastcover_params(FastCoverParams {
        k: 128,
        d: 6,
        f: 18,
        accel: 1,
    });
    let (whole, _) =
        optimize_fastcover_raw(sample.as_slice(), 2048, 1.0, 1, &[6], &[18], &[128]).unwrap();
    assert_eq!(
        whole,
        build_raw_dict(sample.as_slice(), 2048, params).unwrap()
    );
    let share = (sample.len() as f64 * 0.05) as usize;
    let (small, _) =
        optimize_fastcover_raw(sample.as_slice(), 2048, 0.05, 1, &[6], &[18], &[128]).unwrap();
    assert_eq!(
        small,
        build_raw_dict(&sample[..share], 2048, params).unwrap()
    );
}

#[test]
fn fastcover_optimizer_handles_extreme_split_points() {
    let sample = corpus();
    let (dict_low, tuned_low) =
        optimize_fastcover_raw(sample.as_slice(), 2048, 0.0, 1, &[6], &[18], &[128]).unwrap();
    let (dict_high, tuned_high) =
        optimize_fastcover_raw(sample.as_slice(), 2048, 1.0, 1, &[6], &[18], &[128]).unwrap();
    assert!(!dict_low.is_empty());
    assert!(!dict_high.is_empty());
    assert_eq!(tuned_low.k, 128);
    assert_eq!(tuned_high.k, 128);
}

/// A segment longer than 65,535 dmers can hold that many copies of one dmer,
/// which is more than a 16-bit window count holds: on a run of one repeated
/// byte every dmer is the same. Such a `k` trains like any other. A segment
/// scores its distinct dmers, and this corpus has one, so the dictionary is
/// that dmer's bytes; a count that wrapped would score it again at each wrap.
#[test]
fn fastcover_trains_a_segment_longer_than_a_16_bit_count() {
    let k = 66_000;
    let sample = vec![0u8; 10 * k + 1000];
    let dict = train_fastcover_raw(
        sample.as_slice(),
        k,
        FastCoverParams {
            k,
            d: 8,
            f: 20,
            accel: 1,
        },
    )
    .unwrap();
    assert_eq!(dict, [0u8; 8]);
}

/// `f` is the width of the frequency table, and every width the trainer's
/// interface takes (1..=31) is used as given rather than moved into a
/// narrower band the caller never asked for. Training runs at a width on
/// either side of the band the defaults search.
#[test]
fn fastcover_uses_every_table_width_it_is_given() {
    for f in [1, 4, 8, 20, 24, 31] {
        let params = normalize_fastcover_params(FastCoverParams {
            k: 256,
            d: 8,
            f,
            accel: 1,
        });
        assert_eq!(params.f, f);
    }
    let sample = corpus();
    for f in [4, 24] {
        let dict = train_fastcover_raw(
            sample.as_slice(),
            4096,
            FastCoverParams {
                k: 256,
                d: 8,
                f,
                accel: 1,
            },
        )
        .unwrap();
        assert!(!dict.is_empty(), "f={f}");
    }
}

/// A count table larger than the target can lay out is an error, not a panic
/// on the layout or an abort on the allocation; one that fits comes back
/// zeroed at the length asked for.
#[test]
fn a_count_table_that_does_not_fit_is_an_error() {
    let entries = usize::MAX / 2;
    assert_eq!(
        zeroed_counts::<u32>(entries).unwrap_err(),
        TableTooLarge { entries }
    );
    // One the target can lay out and no allocator can back (a 32-bit
    // address space may still hold that one).
    #[cfg(target_pointer_width = "64")]
    {
        let entries = isize::MAX as usize / 4;
        assert_eq!(
            zeroed_counts::<u32>(entries).unwrap_err(),
            TableTooLarge { entries }
        );
    }
    let table = zeroed_counts::<u16>(1 << 12).unwrap();
    assert_eq!(table.len(), 1 << 12);
    assert!(table.iter().all(|&count| count == 0));
    assert!(zeroed_counts::<u16>(0).unwrap().is_empty());
}

/// A sample shorter than one dmer counts nothing: the table comes back at
/// its width, every count zero.
#[test]
fn a_sample_shorter_than_a_dmer_counts_nothing() {
    let table = build_frequency_table(b"abc", 8, 10, 1).unwrap();
    assert_eq!(table.len(), 1 << 10);
    assert!(table.iter().all(|&count| count == 0));
}

/// A segment far longer than the corpus (a `k` near the top of `usize`, which
/// a 32-bit build reaches from any large command-line value) is capped by the
/// corpus: the epoch floor sized from it must not overflow on the way.
#[test]
fn fastcover_trains_with_a_segment_longer_than_the_corpus() {
    let sample = corpus();
    let dict = train_fastcover_raw(
        sample.as_slice(),
        4096,
        FastCoverParams {
            k: usize::MAX / 4,
            d: 8,
            f: 20,
            accel: 1,
        },
    )
    .unwrap();
    assert!(!dict.is_empty());
    assert!(dict.len() <= 4096);
}

#[test]
fn fastcover_optimizer_reports_normalized_params() {
    let sample = corpus();
    // A width below the trainer's range comes back at its lower end; the
    // upper end is checked without training, a table that wide being
    // gigabytes.
    let (dict, tuned) =
        optimize_fastcover_raw(sample.as_slice(), 1024, 0.75, 1, &[64], &[0], &[8]).unwrap();
    assert!(!dict.is_empty());
    assert_eq!(tuned.d, 32);
    assert_eq!(tuned.f, 1);
    assert_eq!(tuned.k, 32);
    assert_eq!(
        normalize_fastcover_params(FastCoverParams {
            k: 64,
            d: 8,
            f: 42,
            accel: 1
        })
        .f,
        31
    );
}
