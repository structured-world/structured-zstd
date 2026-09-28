use super::*;
use alloc::vec;
use std::format;

fn lines(count: u32) -> (Vec<u8>, Vec<usize>) {
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..count {
        let line =
            format!("tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbccccdddd\n");
        sizes.push(line.len());
        data.extend_from_slice(line.as_bytes());
    }
    (data, sizes)
}

/// A dmer is counted only where it lies wholly inside one sample (upstream
/// zstd `FASTCOVER_computeFrequency`): the positions whose read crosses into
/// the next sample are left out, so five 9-byte samples at `d = 8` count two
/// positions each, not the 38 the concatenation holds.
#[test]
fn dmers_are_counted_inside_their_sample_only() {
    let data: Vec<u8> = (0u8..45).collect();
    let set = SampleSet::new(&data, &[9; 5]).unwrap();
    let ctx = FastCoverContext::new(&set, 5, 8, 20, 1).unwrap();
    assert_eq!(ctx.freqs.iter().map(|&c| u64::from(c)).sum::<u64>(), 10);
    assert_eq!(ctx.nb_dmers, 45 - 8 + 1);
}

/// `accel` counts every `accel`-th position of each sample, and draws the
/// entropy tables from the share of samples upstream's table gives it.
#[test]
fn accel_strides_the_count_and_shrinks_the_finalize_share() {
    let data: Vec<u8> = (0u8..60).collect();
    let set = SampleSet::new(&data, &[12; 5]).unwrap();
    let full = FastCoverContext::new(&set, 5, 8, 20, 1).unwrap();
    let strided = FastCoverContext::new(&set, 5, 8, 20, 2).unwrap();
    let total = |ctx: &FastCoverContext<'_>| ctx.freqs.iter().map(|&c| u64::from(c)).sum::<u64>();
    // Five positions per 12-byte sample; every other one is positions 0, 2, 4.
    assert_eq!(total(&full), 25);
    assert_eq!(total(&strided), 15);
    assert_eq!(full.finalize_samples(100), 100);
    assert_eq!(strided.finalize_samples(100), 50);
    assert_eq!(
        FastCoverContext::new(&set, 5, 8, 20, 10)
            .unwrap()
            .finalize_samples(100),
        10
    );
}

/// Training bytes shorter than one dmer read are refused rather than yielding
/// an empty dictionary.
#[test]
fn training_bytes_shorter_than_a_dmer_are_refused() {
    let data = [1u8; 7];
    let set = SampleSet::new(&data, &[1, 1, 1, 1, 3]).unwrap();
    let err = FastCoverContext::new(&set, 5, 6, 20, 1).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn build_fills_at_most_the_capacity_from_the_training_bytes() {
    let (data, sizes) = lines(500);
    let set = SampleSet::new(&data, &sizes).unwrap();
    let ctx = FastCoverContext::new(&set, sizes.len(), 8, 20, 1).unwrap();
    let mut window = WindowCounts::default();
    let mut freqs = Vec::new();
    let mut out = Vec::new();
    let content = ctx
        .build(&mut freqs, &mut window, &mut out, 4096, 256)
        .unwrap()
        .to_vec();
    assert!(!content.is_empty() && content.len() <= 4096);
    // The last segment is at least a dmer of training bytes.
    let last = &content[content.len() - 8..];
    assert!(data.windows(8).any(|w| w == last));
    // The window counts come back zeroed, the spent frequencies are refilled
    // and the content scratch is overwritten, so the next build on the same
    // scratch repeats the first.
    let again = ctx
        .build(&mut freqs, &mut window, &mut out, 4096, 256)
        .unwrap();
    assert_eq!(content, again);
}

/// A segment longer than 65,535 dmers can hold that many copies of one dmer,
/// which is more than a 16-bit window count holds: on a run of one repeated
/// byte every dmer is the same. Such a `k` trains like any other. A segment
/// scores its distinct dmers, and this corpus has one, so the dictionary is
/// that dmer's bytes; a count that wrapped would score it again at each wrap.
#[test]
fn a_segment_longer_than_a_16_bit_count_trains() {
    let k = 66_000;
    let data = vec![0u8; 10 * k + 1000];
    let each = data.len() / 5;
    let set = SampleSet::new(&data, &[each; 5]).unwrap();
    let ctx = FastCoverContext::new(&set, 5, 8, 20, 1).unwrap();
    let mut out = Vec::new();
    let content = ctx
        .build(
            &mut Vec::new(),
            &mut WindowCounts::default(),
            &mut out,
            k,
            k,
        )
        .unwrap();
    assert_eq!(content, [0u8; 8]);
}

/// A segment far longer than the corpus is capped by it: the epoch floor sized
/// from it must not overflow on the way.
#[test]
fn a_segment_longer_than_the_corpus_trains() {
    let (data, sizes) = lines(100);
    let set = SampleSet::new(&data, &sizes).unwrap();
    let ctx = FastCoverContext::new(&set, sizes.len(), 8, 20, 1).unwrap();
    let mut out = Vec::new();
    let content = ctx
        .build(
            &mut Vec::new(),
            &mut WindowCounts::default(),
            &mut out,
            4096,
            usize::MAX / 4,
        )
        .unwrap();
    assert!(!content.is_empty() && content.len() <= 4096);
}

/// Every table width the interface takes is used as given, including by a
/// frequency scratch carried from a build at another width.
#[test]
fn every_table_width_trains() {
    let (data, sizes) = lines(200);
    let set = SampleSet::new(&data, &sizes).unwrap();
    let mut freqs = Vec::new();
    let mut out = Vec::new();
    for f in [1, 4, 8, 24] {
        let ctx = FastCoverContext::new(&set, sizes.len(), 8, f, 1).unwrap();
        assert_eq!(ctx.freqs.len(), 1 << f);
        let content = ctx
            .build(
                &mut freqs,
                &mut WindowCounts::default(),
                &mut out,
                2048,
                128,
            )
            .unwrap();
        assert!(!content.is_empty(), "f={f}");
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

/// A table that does not fit reaches the caller as an out-of-memory error
/// that names the table and the knob that sizes it.
#[test]
fn a_table_that_does_not_fit_is_an_out_of_memory_error() {
    let err = io::Error::from(TableTooLarge { entries: 1 << 31 });
    assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
    let message = std::string::ToString::to_string(&err);
    assert!(message.contains("2147483648 entries"), "{message}");
    assert!(message.contains("smaller f"), "{message}");
}
