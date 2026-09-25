use super::*;
use alloc::format;
use alloc::string::String;

/// `count` log lines of a few shapes, each line a sample.
fn log_samples(count: u32) -> (Vec<u8>, Vec<usize>) {
    const SHAPES: [&str; 4] = [
        "ts={i} level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts={i} level=WARN msg=\"slow compaction\" tenant=demo table=users region=us-east\n",
        "ts={i} level=INFO msg=\"rotate segment\" tenant=acme table=orders region=eu-west\n",
        "ts={i} level=ERROR msg=\"write stalled\" tenant=acme table=events region=ap-south\n",
    ];
    let mut samples = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..count {
        let line = SHAPES[(i % 4) as usize].replace("{i}", &format!("{:08}", i * 7919));
        sizes.push(line.len());
        samples.extend_from_slice(line.as_bytes());
    }
    (samples, sizes)
}

/// The trainer keeps what the corpus repeats: every segment it returns is a
/// run of the corpus that occurs at least `MIN_RATIO` times, and the content
/// stays within the size asked for.
#[test]
fn the_content_is_made_of_repeated_corpus_runs() {
    let (samples, sizes) = log_samples(400);
    let content = train_legacy_raw(&samples, &sizes, 4096, 0).unwrap();
    assert!(content.len() >= CONTENT_SIZE_MIN && content.len() <= 4096);
    let text = String::from_utf8_lossy(&content);
    for field in [
        "tenant=demo table=orders",
        "msg=\"flush memtable\"",
        "region=eu-west",
    ] {
        assert!(
            text.contains(field),
            "{field} is repeated in every fourth sample"
        );
    }
    // A timestamp is unique per sample, so no whole one survives.
    assert!(!text.contains(&format!("ts={:08}", 3 * 7919)));
}

/// A size below the reference's minimum, a corpus under 512 bytes, and a corpus
/// with nothing repeated are refused, as `ZDICT_trainFromBuffer_legacy` refuses
/// them.
#[test]
fn too_little_to_train_on_is_refused() {
    let (samples, sizes) = log_samples(400);
    assert_eq!(
        train_legacy_raw(&samples, &sizes, DICT_SIZE_MIN - 1, 0),
        Err(TooSmall::Dictionary)
    );
    let (small, small_sizes) = log_samples(5);
    assert!(small.len() < MIN_SAMPLES_SIZE);
    assert_eq!(
        train_legacy_raw(&small, &small_sizes, 4096, 0),
        Err(TooSmall::Corpus)
    );
    let mut state = 0x9E37_79B9u32;
    let noise: Vec<u8> = (0..8192)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    assert_eq!(
        train_legacy_raw(&noise, &[noise.len()], 4096, 0),
        Err(TooSmall::Content)
    );
}

/// Selectivity decides how often a segment must repeat: `samples >> s`, never
/// below four. With many samples a low selectivity demands more repetitions
/// than a rare shape has, so it keeps less than a high one.
#[test]
fn selectivity_sets_how_often_a_segment_must_repeat() {
    let (mut samples, mut sizes) = log_samples(4096);
    // A shape present in 40 samples: kept when 40 repetitions suffice.
    for i in 0..40 {
        let line = format!("rare shape {i:04} backup completed for volume vol-archive-7\n");
        sizes.push(line.len());
        samples.extend_from_slice(line.as_bytes());
    }
    let strict = train_legacy_raw(&samples, &sizes, 64 * 1024, 6).unwrap();
    let loose = train_legacy_raw(&samples, &sizes, 64 * 1024, 9).unwrap();
    let has_rare = |content: &[u8]| String::from_utf8_lossy(content).contains("vol-archive-7");
    // 4136 >> 6 = 64 repetitions needed: the rare shape has 40.
    assert!(!has_rare(&strict));
    // 4136 >> 9 = 8 repetitions needed.
    assert!(has_rare(&loose));
}
