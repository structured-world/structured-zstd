use super::*;
use std::collections::BTreeMap;

/// Upstream zstd `COVER_computeEpochs`: the target count stands while epochs
/// stay ten segments long, and otherwise epochs are ten segments each.
#[test]
fn epochs_follow_the_reference_formula() {
    assert_eq!(
        compute_epochs(4096, 100_000, 100, 4),
        Epochs {
            num: 10,
            size: 10_000
        }
    );
    assert_eq!(
        compute_epochs(4096, 5_000, 100, 4),
        Epochs {
            num: 5,
            size: 1_000
        }
    );
    // Fewer dmers than one floor-sized epoch: one epoch of all of them.
    assert_eq!(
        compute_epochs(4096, 300, 100, 4),
        Epochs { num: 1, size: 300 }
    );
    // A `k` whose floor overflows is capped by the corpus.
    assert_eq!(
        compute_epochs(4096, 300, usize::MAX / 4, 1),
        Epochs { num: 1, size: 300 }
    );
}

/// Five floor-sized epochs of 1,000 dmers leave 500 of 5,500 over; the last
/// epoch takes them, so no dmer of the corpus goes unscanned.
#[test]
fn the_last_epoch_runs_to_the_end_of_the_corpus() {
    let epochs = compute_epochs(4096, 5_500, 100, 4);
    assert_eq!(
        epochs,
        Epochs {
            num: 5,
            size: 1_000
        }
    );
    assert_eq!(epochs.bounds(0, 5_500), (0, 1_000));
    assert_eq!(epochs.bounds(3, 5_500), (3_000, 4_000));
    assert_eq!(epochs.bounds(4, 5_500), (4_000, 5_500));
}

/// A dmer's frequency is the number of samples it lies in: repeats inside
/// one sample count once, the same dmer in another sample counts again.
#[test]
fn a_dmer_counts_once_per_sample() {
    // Five samples; "ABCDEFGH" twice in each of the first three.
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..5u8 {
        let sample: Vec<u8> = if i < 3 {
            b"ABCDEFGH-ABCDEFGH-".to_vec()
        } else {
            std::vec![b'0' + i; 18]
        };
        sizes.push(sample.len());
        data.extend_from_slice(&sample);
    }
    let set = SampleSet::new(&data, &sizes).unwrap();
    let ctx = CoverContext::new(&set, 5, 8).unwrap();
    let id = ctx.dmer_at[0] as usize;
    assert_eq!(ctx.dmer_at[9] as usize, id, "equal dmers share an id");
    assert_eq!(ctx.dmer_at[18] as usize, id);
    assert_eq!(ctx.initial[id].freq, 3);
}

/// Every position gets the id of its dmer: equal dmers one id, distinct ones
/// distinct ids, frequencies the number of samples each lies wholly inside;
/// a dmer that spills into the next sample earns nothing there. Every position
/// a whole dmer starts at is indexed, a short one near the end of the corpus
/// included. Checked against a plain map, past the index's first growth, for a
/// short and a long dmer size.
#[test]
fn dmer_ids_agree_with_a_plain_map() {
    // A deterministic stream over a small alphabet: many distinct dmers, many
    // repeats.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let data: Vec<u8> = (0..60_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            b'a' + (state % 4) as u8
        })
        .collect();
    let sizes = [12_000usize; 5];
    let set = SampleSet::new(&data, &sizes).unwrap();
    for d in [6usize, 12] {
        let ctx = CoverContext::new(&set, 5, d).unwrap();
        let nb_dmers = data.len() - d + 1;
        assert_eq!(ctx.dmer_at.len(), nb_dmers);
        let mut ids: BTreeMap<&[u8], u32> = BTreeMap::new();
        let mut samples_of: BTreeMap<&[u8], Vec<usize>> = BTreeMap::new();
        for pos in 0..nb_dmers {
            let dmer = &data[pos..pos + d];
            let id = *ids.entry(dmer).or_insert(ctx.dmer_at[pos]);
            assert_eq!(ctx.dmer_at[pos], id, "d={d} pos={pos}");
            let seen = samples_of.entry(dmer).or_default();
            let sample = pos / 12_000;
            let inside = (pos + d - 1) / 12_000 == sample;
            if inside && seen.last() != Some(&sample) {
                seen.push(sample);
            }
        }
        assert!(ids.len() > 2048, "the index grew past its first size");
        assert_eq!(ctx.initial.len(), ids.len(), "one id per distinct dmer");
        for (dmer, id) in &ids {
            assert_eq!(
                ctx.initial[*id as usize].freq as usize,
                samples_of[dmer].len(),
                "d={d}"
            );
        }
    }
}

/// The segment worth most comes first and lands last; content stays within
/// the capacity, is made of training bytes, and a second build on the same
/// spent scratch repeats the first.
#[test]
fn build_places_the_best_segment_last() {
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..40u32 {
        let line = std::format!("shared-header-{:03} value={}\n", i % 7, i * 31);
        sizes.push(line.len());
        data.extend_from_slice(line.as_bytes());
    }
    let set = SampleSet::new(&data, &sizes).unwrap();
    let ctx = CoverContext::new(&set, sizes.len(), 8).unwrap();
    let mut state = Vec::new();
    let mut out = Vec::new();
    let content = ctx.build(&mut state, &mut out, 256, 32).to_vec();
    assert!(!content.is_empty() && content.len() <= 256);
    // "shared-header-" starts every sample, so it is the most frequent run.
    let tail = &content[content.len() - content.len().min(40)..];
    assert!(
        tail.windows(8).any(|w| w == b"shared-h"),
        "{:?}",
        std::string::String::from_utf8_lossy(&content)
    );
    // The same spent scratch, content buffer included, repeats the build.
    assert_eq!(content, ctx.build(&mut state, &mut out, 256, 32));
}

#[test]
fn training_bytes_shorter_than_a_dmer_are_refused() {
    let data = [1u8; 7];
    let set = SampleSet::new(&data, &[1, 1, 1, 1, 3]).unwrap();
    let err = CoverContext::new(&set, 5, 6).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}
