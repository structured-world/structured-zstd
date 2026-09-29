use super::*;
use crate::decoding::Dictionary;
use crate::encoding::{CompressionLevel, EncoderDictionary, FrameCompressor};
use std::io::Cursor;
use std::string::ToString;
use std::vec;

fn training_data() -> Vec<u8> {
    training_samples().0
}

/// Log lines, one sample each.
fn training_samples() -> (Vec<u8>, Vec<usize>) {
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..512u32 {
        let line = format!(
            "tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbcccccdddddeeeee op={}\n",
            ["put", "get", "scan", "delete"][i as usize % 4]
        );
        sizes.push(line.len());
        data.extend_from_slice(line.as_bytes());
    }
    (data, sizes)
}

/// What a dictionary costs on `samples`: its size plus each sample compressed
/// with it at the default level, the measure the optimizers minimise.
fn price(dict: &[u8], data: &[u8], sizes: &[usize]) -> usize {
    let mut compressor: FrameCompressor = FrameCompressor::new(CompressionLevel::Default);
    compressor
        .set_encoder_dictionary(EncoderDictionary::from_bytes(dict).unwrap())
        .unwrap();
    let mut total = dict.len();
    let mut start = 0;
    for &size in sizes {
        total += compressor
            .compress_independent_frame(&data[start..start + size])
            .len();
        start += size;
    }
    total
}

fn fixed_cover(k: u32, d: u32) -> CoverOptions {
    CoverOptions {
        k,
        d,
        ..CoverOptions::default()
    }
}

fn fixed_fastcover(k: u32, d: u32) -> FastCoverOptions {
    FastCoverOptions {
        cover: CoverOptions {
            k,
            d,
            ..FastCoverOptions::default().cover
        },
        ..FastCoverOptions::default()
    }
}

/// A trained dictionary parses back, fits the size asked for, and carries the
/// id it was given.
#[test]
fn plain_trainers_write_a_parseable_dictionary() {
    let (data, sizes) = training_samples();
    let finalize = FinalizeOptions {
        dict_id: Some(77),
        ..FinalizeOptions::default()
    };
    let cover = train_cover_dict(&data, &sizes, 4096, &fixed_cover(128, 8), finalize).unwrap();
    let fast =
        train_fastcover_dict(&data, &sizes, 4096, &fixed_fastcover(128, 8), finalize).unwrap();
    for dict in [cover, fast] {
        assert!(dict.len() <= 4096);
        let parsed = Dictionary::decode_dict(&dict).expect("the dictionary parses back");
        assert_eq!(parsed.id, 77);
        assert!(!parsed.dict_content.is_empty());
    }
}

/// The plain trainers take `k` and `d` as given; zero asks for a search, which
/// only the optimizers run (upstream zstd rejects it the same way).
#[test]
fn plain_trainers_require_k_and_d() {
    let (data, sizes) = training_samples();
    for options in [fixed_cover(0, 8), fixed_cover(128, 0)] {
        let err = train_cover_dict(&data, &sizes, 4096, &options, FinalizeOptions::default())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
    let err = train_fastcover_dict(
        &data,
        &sizes,
        4096,
        &fixed_fastcover(0, 8),
        FinalizeOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// Fewer than five samples, sizes that do not add up to the corpus, and a
/// dictionary under 256 bytes are refused before any training.
#[test]
fn trainers_refuse_what_they_cannot_train_on() {
    let (data, sizes) = training_samples();
    let cases: [(&[u8], &[usize], usize); 4] = [
        (&data[..sizes[..4].iter().sum::<usize>()], &sizes[..4], 4096),
        (&data, &sizes[..10], 4096),
        (&data[..10], &[20, 20, 20, 20, 20], 4096),
        (&data, &sizes, 255),
    ];
    for (samples, sample_sizes, dict_size) in cases {
        let cover = optimize_cover_dict(
            samples,
            sample_sizes,
            dict_size,
            &CoverOptions::default(),
            FinalizeOptions::default(),
        )
        .unwrap_err();
        assert_eq!(cover.kind(), io::ErrorKind::InvalidInput, "{cover}");
        let fast = optimize_fastcover_dict(
            samples,
            sample_sizes,
            dict_size,
            &FastCoverOptions::default(),
            FinalizeOptions::default(),
        )
        .unwrap_err();
        assert_eq!(fast.kind(), io::ErrorKind::InvalidInput, "{fast}");
    }
}

/// The optimizer keeps the candidate the scoring samples compress best with.
/// With every sample both building and scoring, each candidate is exactly the
/// plain trainer's dictionary for its `k` and `d`, so pricing those directly
/// finds the same minimum the optimizer reports.
#[test]
fn the_optimizer_keeps_the_cheapest_candidate() {
    let (data, sizes) = training_samples();
    let options = CoverOptions {
        k: 0,
        d: 8,
        steps: 4,
        split_point: 1.0,
        ..CoverOptions::default()
    };
    let (dict, chosen) =
        optimize_cover_dict(&data, &sizes, 2048, &options, FinalizeOptions::default()).unwrap();
    // 50..=2000 in four strides of 487.
    let grid = [50u32, 537, 1024, 1511, 1998];
    assert!(grid.contains(&chosen.k), "k={}", chosen.k);
    assert_eq!((chosen.d, chosen.steps, chosen.split_point), (8, 4, 1.0));
    let chosen_price = price(&dict, &data, &sizes);
    for k in grid {
        let candidate = train_cover_dict(
            &data,
            &sizes,
            2048,
            &fixed_cover(k, 8),
            FinalizeOptions::default(),
        )
        .unwrap();
        if k == chosen.k {
            assert_eq!(candidate, dict);
        }
        assert!(
            chosen_price <= price(&candidate, &data, &sizes),
            "k={k} is cheaper than the chosen k={}",
            chosen.k
        );
    }
}

/// The FastCOVER optimizer searches the same `k` grid and reports the table
/// width and acceleration it ran with.
#[test]
fn the_fastcover_optimizer_reports_what_it_ran_with() {
    let (data, sizes) = training_samples();
    let (dict, chosen) = optimize_fastcover_dict(
        &data,
        &sizes,
        2048,
        &FastCoverOptions::default(),
        FinalizeOptions::default(),
    )
    .unwrap();
    assert!(Dictionary::decode_dict(&dict).is_ok());
    assert!([50u32, 537, 1024, 1511, 1998].contains(&chosen.cover.k));
    assert_eq!(
        (
            chosen.cover.d,
            chosen.f,
            chosen.accel,
            chosen.cover.split_point
        ),
        (8, 20, 1, 0.75)
    );
    // A zero `d` tries both 6 and 8.
    let (_, either) = optimize_fastcover_dict(
        &data,
        &sizes,
        2048,
        &FastCoverOptions {
            cover: CoverOptions {
                d: 0,
                ..FastCoverOptions::default().cover
            },
            ..FastCoverOptions::default()
        },
        FinalizeOptions::default(),
    )
    .unwrap();
    assert!([6, 8].contains(&either.cover.d));
}

/// With `shrink`, the smallest trailing share of the content whose scoring
/// cost stays within the allowed regression is kept. Allowing a large
/// regression takes the first, 256-byte candidate; allowing none keeps a
/// dictionary no larger than the unshrunk one and no costlier.
#[test]
fn shrink_keeps_a_smaller_dictionary_within_the_regression() {
    let (data, sizes) = training_samples();
    let full = train_cover_dict(
        &data,
        &sizes,
        8192,
        &fixed_cover(256, 8),
        FinalizeOptions::default(),
    )
    .unwrap();
    let full_price = price(&full, &data, &sizes);
    let generous = train_cover_dict(
        &data,
        &sizes,
        8192,
        &CoverOptions {
            shrink: Some(1000),
            ..fixed_cover(256, 8)
        },
        FinalizeOptions::default(),
    )
    .unwrap();
    let content_len = |dict: &[u8]| Dictionary::decode_dict(dict).unwrap().dict_content.len();
    assert_eq!(content_len(&generous), 256);
    assert!(price(&generous, &data, &sizes) as f64 <= full_price as f64 * 11.0);
    let strict = train_cover_dict(
        &data,
        &sizes,
        8192,
        &CoverOptions {
            shrink: Some(0),
            ..fixed_cover(256, 8)
        },
        FinalizeOptions::default(),
    )
    .unwrap();
    assert!(strict.len() <= full.len());
    assert!(price(&strict, &data, &sizes) <= full_price);
}

/// In a search, `shrink` cuts the winner down rather than weighing every
/// candidate at every size: the parameters chosen are the ones the search
/// chooses without it, and the dictionary is that winner's, cut or whole.
#[test]
fn shrink_cuts_the_search_winner() {
    let (data, sizes) = training_samples();
    let options = CoverOptions {
        split_point: 0.75,
        ..CoverOptions::default()
    };
    let (whole, chosen) =
        optimize_cover_dict(&data, &sizes, 8192, &options, FinalizeOptions::default()).unwrap();
    let (cut, chosen_with_shrink) = optimize_cover_dict(
        &data,
        &sizes,
        8192,
        &CoverOptions {
            shrink: Some(1000),
            ..options
        },
        FinalizeOptions::default(),
    )
    .unwrap();
    assert_eq!(
        (chosen_with_shrink.k, chosen_with_shrink.d),
        (chosen.k, chosen.d)
    );
    let content = |dict: &[u8]| Dictionary::decode_dict(dict).unwrap().dict_content;
    assert!(cut.len() < whole.len());
    assert!(content(&whole).ends_with(&content(&cut)));
}

/// A split below 1 scores on the trailing samples only: a split that leaves
/// none of them, or fewer than five to build from, is refused.
#[test]
fn the_split_must_leave_samples_on_both_sides() {
    let (data, sizes) = training_samples();
    for split_point in [0.005, 1.5] {
        let err = optimize_cover_dict(
            &data,
            &sizes,
            4096,
            &CoverOptions {
                split_point,
                ..CoverOptions::default()
            },
            FinalizeOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{split_point}");
    }
}

/// A split point that is not a number lies outside `(0, 1]` too: every
/// comparison with NaN is false, so it must be refused explicitly rather than
/// slip through as "every sample builds and scores".
#[test]
fn a_split_point_that_is_not_a_number_is_refused() {
    let (data, sizes) = training_samples();
    for split_point in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = optimize_cover_dict(
            &data,
            &sizes,
            4096,
            &CoverOptions {
                split_point,
                ..CoverOptions::default()
            },
            FinalizeOptions::default(),
        )
        .unwrap_err();
        assert_eq!(
            TrainingError::of(&err),
            Some(TrainingError::Parameter),
            "{split_point}"
        );
    }
}

/// A dmer shorter than eight bytes that starts in the corpus's last eight
/// bytes is indexed too: here it is the only one that lies inside a sample
/// (the last, six bytes long), and dropping it left nothing to train from.
#[test]
fn a_short_dmer_at_the_end_of_the_corpus_is_indexed() {
    let data = *b"abcdefghij";
    let sizes = [1usize, 1, 1, 1, 6];
    let cover = CoverOptions {
        k: 6,
        d: 6,
        ..CoverOptions::default()
    };
    let dict = train_cover_dict(&data, &sizes, 1024, &cover, FinalizeOptions::default())
        .expect("the last sample holds a dmer");
    assert!(dict.ends_with(b"efghij"));
}

/// A search over dmer sizes keeps what one size built when another cannot
/// train: six-byte samples hold a COVER dmer of 6 and none of 8, so the search
/// returns its `d = 6` winner rather than the `d = 8` refusal. (FastCOVER
/// reads eight bytes for any `d` of 8 or less, as upstream zstd's does, so the
/// same samples hold none of its dmers at either size.)
#[test]
fn a_dmer_size_the_samples_cannot_hold_is_skipped_in_a_search() {
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..40u8 {
        data.extend_from_slice(&[b'a' + i % 7, b'b', b'c', b'd', i % 5, b'e']);
        sizes.push(6);
    }
    let cover = CoverOptions {
        k: 8,
        d: 0,
        ..CoverOptions::default()
    };
    let (_, chosen) = optimize_cover_dict(&data, &sizes, 1024, &cover, FinalizeOptions::default())
        .expect("d = 6 trains");
    assert_eq!(chosen.d, 6);
}

/// A shrunk dictionary costing exactly the tolerated percentage more is kept:
/// the bound is compared in integers, where 300 bytes at 13% allow 339, not in
/// floating point, where 300 * 1.13 comes out just under 339.
#[test]
fn a_shrunk_dictionary_exactly_at_the_bound_is_kept() {
    assert!(selection::within_regression(339, 300, 13));
    assert!(!selection::within_regression(340, 300, 13));
    assert!(selection::within_regression(300, 300, 0));
    assert!(!selection::within_regression(301, 300, 0));
    assert!(selection::within_regression(
        usize::MAX,
        usize::MAX,
        u32::MAX
    ));
}

/// A dmer counts only inside one sample, so samples all shorter than `d` hold
/// none however long their concatenation is: nothing can be trained, and the
/// samples are refused as samples before an index of the whole corpus is built
/// for nothing.
#[test]
fn samples_too_short_for_a_dmer_are_refused() {
    let data: Vec<u8> = (0..4096u32).map(|i| (i * 7) as u8).collect();
    let sizes = vec![2usize; data.len() / 2];
    let cover = CoverOptions {
        k: 64,
        d: 8,
        ..CoverOptions::default()
    };
    let err =
        train_cover_dict(&data, &sizes, 4096, &cover, FinalizeOptions::default()).unwrap_err();
    assert_eq!(
        TrainingError::of(&err),
        Some(TrainingError::Samples),
        "{err}"
    );
    let fastcover = FastCoverOptions {
        cover,
        ..FastCoverOptions::default()
    };
    let err = train_fastcover_dict(&data, &sizes, 4096, &fastcover, FinalizeOptions::default())
        .unwrap_err();
    assert_eq!(
        TrainingError::of(&err),
        Some(TrainingError::Samples),
        "{err}"
    );
}

/// When a request fails two checks, the preflight names the cause the trainer
/// names: a segment that fits no dictionary of that size is checked before the
/// size itself, as upstream zstd's `COVER_checkParameters` runs before its
/// `ZDICT_DICTSIZE_MIN` check. A caller that maps the cause to an error code
/// must get one answer for one request.
#[test]
fn the_preflight_names_the_cause_the_trainer_names() {
    let (data, sizes) = training_samples();
    let dict_size = MIN_TRAINED_DICT_SIZE - 1;
    let cover = CoverOptions {
        k: 300,
        d: 8,
        ..CoverOptions::default()
    };
    let trained =
        train_cover_dict(&data, &sizes, dict_size, &cover, FinalizeOptions::default()).unwrap_err();
    let checked = check_cover_options(&cover, dict_size).unwrap_err();
    assert_eq!(TrainingError::of(&checked), TrainingError::of(&trained));
    let fastcover = FastCoverOptions {
        cover,
        ..FastCoverOptions::default()
    };
    let trained = train_fastcover_dict(
        &data,
        &sizes,
        dict_size,
        &fastcover,
        FinalizeOptions::default(),
    )
    .unwrap_err();
    let checked = check_fastcover_options(&fastcover, dict_size).unwrap_err();
    assert_eq!(TrainingError::of(&checked), TrainingError::of(&trained));
}

/// The preflight refuses what the trainer it stands in for refuses: a
/// dictionary under the trainers' minimum fails training whatever segment
/// fits, so the check a caller runs before loading a corpus fails it too, with
/// the same cause.
#[test]
fn the_preflight_refuses_a_dictionary_under_the_trainer_minimum() {
    let (data, sizes) = training_samples();
    let dict_size = MIN_TRAINED_DICT_SIZE - 1;
    let cover = CoverOptions::default();
    let fastcover = FastCoverOptions::default();
    let trained = optimize_cover_dict(&data, &sizes, dict_size, &cover, FinalizeOptions::default())
        .unwrap_err();
    assert_eq!(
        TrainingError::of(&trained),
        Some(TrainingError::DictionaryTooSmall)
    );

    let cover_check = check_cover_options(&cover, dict_size).unwrap_err();
    assert_eq!(
        TrainingError::of(&cover_check),
        Some(TrainingError::DictionaryTooSmall)
    );
    let fastcover_check = check_fastcover_options(&fastcover, dict_size).unwrap_err();
    assert_eq!(
        TrainingError::of(&fastcover_check),
        Some(TrainingError::DictionaryTooSmall)
    );
}

/// A dictionary too small is refused before the sample sizes are checked
/// against the samples, as upstream zstd's `ZDICT_trainFromBuffer_cover` does:
/// the samples are only walked once the request can produce a dictionary.
#[test]
fn an_undersized_dictionary_is_refused_before_the_sample_sizes_are_checked() {
    let samples = [7u8; 64];
    // Sizes that overrun the samples: walking them is its own refusal.
    let sizes = [40usize, 40];
    let options = CoverOptions {
        k: 64,
        d: 8,
        ..CoverOptions::default()
    };
    let err =
        train_cover_dict(&samples, &sizes, 100, &options, FinalizeOptions::default()).unwrap_err();
    assert_eq!(
        TrainingError::of(&err),
        Some(TrainingError::DictionaryTooSmall),
        "{err}"
    );
}

/// A frequency table or acceleration past the reference's range is refused,
/// as is a dmer shorter than the hash reads.
#[test]
fn fastcover_refuses_knobs_out_of_range() {
    let (data, sizes) = training_samples();
    let base = fixed_fastcover(128, 8);
    for options in [
        FastCoverOptions { f: 32, ..base },
        FastCoverOptions { accel: 11, ..base },
        fixed_fastcover(128, 3),
    ] {
        let err = train_fastcover_dict(&data, &sizes, 4096, &options, FinalizeOptions::default())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}

/// The widest frequency table the trainer takes (`f = 31`, 2^31 counts) is
/// larger than a 32-bit target can lay out. Training at that width reports
/// that as an error through the `io::Result` rather than panicking on the
/// allocation.
#[cfg(target_pointer_width = "32")]
#[test]
fn a_frequency_table_too_wide_for_the_target_is_an_error() {
    let (data, sizes) = training_samples();
    let options = FastCoverOptions {
        f: 31,
        ..fixed_fastcover(256, 8)
    };
    let err = train_fastcover_dict(&data, &sizes, 4096, &options, FinalizeOptions::default())
        .expect_err("a table this wide does not fit a 32-bit target");
    assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
}

/// A corpus of one repeated byte leaves the literal counts flat: every symbol
/// at eight bits, which no Huffman description can express. The finalizer then
/// describes a mostly flat distribution that can be written, and training on
/// such a corpus must not take the process down with it.
#[test]
fn training_on_a_single_repeated_byte_does_not_crash() {
    let sample = vec![7u8; 4096];
    let dict = train_fastcover_dict(
        &sample,
        &[512; 8],
        4096,
        &fixed_fastcover(256, 8),
        FinalizeOptions {
            dict_id: Some(1),
            ..FinalizeOptions::default()
        },
    )
    .expect("a uniform corpus must train");
    Dictionary::decode_dict(&dict).expect("the trained dictionary must parse back");
}

#[test]
fn create_raw_dict_from_source_early_returns_on_zero_dict_size() {
    let sample = training_data();
    let mut out = Vec::new();
    create_raw_dict_from_source(Cursor::new(sample.as_slice()), sample.len(), &mut out, 0)
        .expect("zero dict size should no-op");
    assert!(out.is_empty());
}

#[test]
fn create_raw_dict_from_source_treats_source_size_as_hint() {
    let sample = training_data();
    let mut out = Vec::new();
    create_raw_dict_from_source(Cursor::new(sample.as_slice()), 0, &mut out, 1024)
        .expect("raw dictionary training should succeed");
    assert!(!out.is_empty());
}

/// Training from a slice is the reader path without its buffering copy, so
/// the two must write the same dictionary, byte for byte, at every size the
/// tiny-source and epoch paths take.
#[test]
fn create_raw_dict_from_slice_matches_the_reader_path() {
    let sample = training_data();
    for (corpus, dict_size) in [
        (sample.as_slice(), 1024),
        (sample.as_slice(), 0),
        (&b"short"[..], 3),
        (&b""[..], 64),
    ] {
        let mut from_reader = Vec::new();
        create_raw_dict_from_source(
            Cursor::new(corpus),
            corpus.len(),
            &mut from_reader,
            dict_size,
        )
        .unwrap();
        let mut from_slice = Vec::new();
        create_raw_dict_from_slice(corpus, &mut from_slice, dict_size).unwrap();
        assert_eq!(from_slice, from_reader, "dict_size {dict_size}");
    }
}

#[test]
fn create_raw_dict_from_source_handles_tiny_source_without_epochs() {
    let sample = b"short";
    let mut out = Vec::new();
    create_raw_dict_from_source(Cursor::new(sample.as_slice()), sample.len(), &mut out, 3)
        .expect("tiny source path should succeed");
    assert_eq!(out, b"ort");
}

#[test]
fn create_raw_dict_from_source_propagates_read_error() {
    struct FailingReader;
    impl io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }

    let mut out = Vec::new();
    let err = create_raw_dict_from_source(FailingReader, 1024, &mut out, 1024)
        .expect_err("read failures must be returned");
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert_eq!(err.to_string(), "read failed");
}

#[test]
fn create_raw_dict_from_source_propagates_write_error() {
    struct FailingWriter;
    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let sample = b"short";
    let mut out = FailingWriter;
    let err =
        create_raw_dict_from_source(Cursor::new(sample.as_slice()), sample.len(), &mut out, 3)
            .expect_err("write failures must be returned");
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert_eq!(err.to_string(), "write failed");
}

#[test]
fn create_raw_dict_from_source_never_exceeds_requested_size() {
    let dict_size = 4096usize;
    let source: Vec<u8> = core::iter::repeat_n(b'a', 320_001).collect();
    let mut out = Vec::new();
    create_raw_dict_from_source(
        Cursor::new(source.as_slice()),
        source.len(),
        &mut out,
        dict_size,
    )
    .expect("raw dictionary training should succeed");
    assert!(
        out.len() <= dict_size,
        "raw dictionary exceeded requested size: {} > {}",
        out.len(),
        dict_size
    );
}

/// Raw content is what `zstd --train`'s search picks over the corpus cut into
/// samples: the dictionary the search finalizes ends with it (the finalizer
/// only cuts the front to make room for its header).
#[test]
fn raw_content_is_the_fastcover_search_winner() {
    let corpus = training_data();
    let dict_size = 4096;
    let mut raw = Vec::new();
    create_raw_dict_from_slice(&corpus, &mut raw, dict_size).unwrap();
    assert!(!raw.is_empty() && raw.len() <= dict_size);

    let cut = corpus.len().div_ceil(RAW_SAMPLES_MIN).min(RAW_SAMPLE_MAX);
    let sizes: Vec<usize> = corpus.chunks(cut).map(<[u8]>::len).collect();
    let (dict, _) = optimize_fastcover_dict(
        &corpus,
        &sizes,
        dict_size,
        &FastCoverOptions::default(),
        FinalizeOptions::default(),
    )
    .unwrap();
    let finalized = Dictionary::decode_dict(&dict).unwrap();
    assert!(raw.ends_with(&finalized.dict_content));
}

/// A corpus no larger than the dictionary is all of its content.
#[test]
fn a_corpus_that_fits_is_its_own_raw_content() {
    let corpus = &training_data()[..3000];
    let mut raw = Vec::new();
    create_raw_dict_from_slice(corpus, &mut raw, 4096).unwrap();
    assert_eq!(raw, corpus);
}

/// A directory trains one sample per file, and its raw dictionary shortens a
/// small frame of the same kind of data.
#[test]
fn raw_content_from_a_directory_helps_a_small_frame() {
    let dir = std::env::temp_dir().join(std::format!("szstd-raw-dir-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    for file in 0..40u32 {
        let mut body = std::string::String::new();
        for line in 0..30u32 {
            body.push_str(&std::format!(
                "tenant=demo table=orders key={} region=eu status=shipped\n",
                file * 30 + line
            ));
        }
        fs::write(dir.join(std::format!("s{file:02}")), body).unwrap();
    }
    let mut raw = Vec::new();
    let trained = create_raw_dict_from_dir(&dir, &mut raw, 2048);
    fs::remove_dir_all(&dir).unwrap();
    trained.unwrap();
    assert!(!raw.is_empty() && raw.len() <= 2048);

    let frame = b"tenant=demo table=orders key=77 region=eu status=shipped\n";
    let compress = |dict: Option<&[u8]>| {
        let mut out = Vec::new();
        let mut compressor = FrameCompressor::new(CompressionLevel::Default);
        if let Some(dict) = dict {
            compressor
                .set_dictionary(Dictionary::from_raw_content(7, dict.to_vec()).unwrap())
                .unwrap();
        }
        compressor.set_source(&frame[..]);
        compressor.set_drain(&mut out);
        compressor.compress();
        out.len()
    };
    assert!(compress(Some(&raw)) < compress(None));
}

#[test]
fn finalize_raw_dict_rejects_empty_raw_content() {
    let (data, sizes) = training_samples();
    let err = finalize_raw_dict(&[], &data, &sizes, 4096, FinalizeOptions::default())
        .expect_err("empty raw dictionary must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn finalize_raw_dict_rejects_too_small_budget() {
    let (data, sizes) = training_samples();
    let raw = b"some-raw-bytes";
    for dict_size in [32, MIN_TRAINED_DICT_SIZE - 1] {
        let err = finalize_raw_dict(raw, &data, &sizes, dict_size, FinalizeOptions::default())
            .expect_err("tiny dict_size must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("dictionary size too small"));
    }
}

#[test]
fn finalize_raw_dict_pads_to_minimum_content_size() {
    let (data, sizes) = training_samples();
    let raw = b"x";
    let finalized = finalize_raw_dict(raw, &data, &sizes, 4096, FinalizeOptions::default())
        .expect("finalize should pad small raw content");
    let parsed = Dictionary::decode_dict(finalized.as_slice()).expect("finalized dict parses");
    assert!(parsed.dict_content.len() >= 8);
    assert_eq!(parsed.dict_content.last(), Some(&b'x'));
}

#[test]
fn finalize_raw_dict_rejects_zero_dict_id() {
    let (data, sizes) = training_samples();
    let raw = b"raw-fastcover-bytes";
    let err = finalize_raw_dict(
        raw,
        &data,
        &sizes,
        4096,
        FinalizeOptions {
            dict_id: Some(0),
            ..FinalizeOptions::default()
        },
    )
    .expect_err("dict_id=0 must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(err.to_string(), "dictionary id must be non-zero");
}

/// A dictionary too small is refused before the samples are walked, as
/// upstream's `ZDICT_finalizeDictionary` checks the capacity first: sizes that
/// do not describe the samples are not what the refusal reports.
#[test]
fn finalize_raw_dict_refuses_an_undersized_dictionary_before_the_samples() {
    let (data, sizes) = training_samples();
    let err = finalize_raw_dict(
        b"content",
        &data,
        &sizes[1..],
        MIN_TRAINED_DICT_SIZE - 1,
        FinalizeOptions::default(),
    )
    .unwrap_err();
    assert_eq!(
        TrainingError::of(&err),
        Some(TrainingError::DictionaryTooSmall)
    );
}

/// The sample sizes must describe the samples exactly.
#[test]
fn finalize_raw_dict_rejects_sizes_that_do_not_add_up() {
    let (data, sizes) = training_samples();
    let err = finalize_raw_dict(
        b"content",
        &data,
        &sizes[1..],
        4096,
        FinalizeOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}
