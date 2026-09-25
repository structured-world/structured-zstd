//! The legacy trainer against the reference's `ZDICT_trainFromBuffer_legacy`:
//! both walk the same suffix array with the same selection rules, so the
//! content they choose has to be the same bytes. Only the header in front of it
//! differs, since the entropy tables are built by each side's own finalizer.
#![cfg(all(feature = "bench-internals", feature = "dict-builder"))]

use structured_zstd::testing::legacy_dict_content;
use zstd::zstd_safe::zstd_sys;

/// Every sample back to back, and the length of each.
type Samples = (Vec<u8>, Vec<usize>);

/// The reference's dictionary content for the same corpus: its dictionary with
/// the header (magic, id, entropy tables, repeat offsets) cut off.
fn reference_content(
    samples: &[u8],
    sizes: &[usize],
    dict_size: usize,
    selectivity: u32,
) -> Option<Vec<u8>> {
    let mut dict = vec![0u8; dict_size];
    let params = zstd_sys::ZDICT_legacy_params_t {
        selectivityLevel: selectivity,
        zParams: zstd_sys::ZDICT_params_t {
            compressionLevel: 0,
            notificationLevel: 0,
            dictID: 0,
        },
    };
    // SAFETY: every buffer is valid for the length passed with it, and
    // `sizes` holds `sizes.len()` entries summing to `samples.len()`.
    let written = unsafe {
        zstd_sys::ZDICT_trainFromBuffer_legacy(
            dict.as_mut_ptr().cast(),
            dict.len(),
            samples.as_ptr().cast(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    // SAFETY: plain query on a return code.
    if written == 0 || unsafe { zstd_sys::ZDICT_isError(written) } != 0 {
        return None;
    }
    dict.truncate(written);
    // SAFETY: `dict` holds `written` bytes of the dictionary just built.
    let header = unsafe { zstd_sys::ZDICT_getDictHeaderSize(dict.as_ptr().cast(), dict.len()) };
    // SAFETY: plain query on a return code.
    assert_eq!(unsafe { zstd_sys::ZDICT_isError(header) }, 0);
    Some(dict[header..].to_vec())
}

/// Log lines of a few shapes, one sample each.
fn log_lines(count: u32) -> Samples {
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

/// The systemd unit files under `dict_tests/files`, one sample each.
fn unit_files() -> Samples {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../zstd/dict_tests/files");
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("the fixture directory exists")
        .map(|entry| entry.expect("readable entry").path())
        .filter(|path| path.extension().is_none_or(|ext| ext != "zst"))
        .collect();
    names.sort();
    let mut samples = Vec::new();
    let mut sizes = Vec::new();
    for name in names {
        let bytes = std::fs::read(&name).expect("readable fixture");
        if bytes.is_empty() {
            continue;
        }
        sizes.push(bytes.len());
        samples.extend_from_slice(&bytes);
    }
    (samples, sizes)
}

/// The decodecorpus file cut into 4 KiB samples, as `--train -B4096` cuts it.
fn corpus_blocks() -> Samples {
    let bytes = include_bytes!("../../zstd/decodecorpus_files/z000033").to_vec();
    let sizes = bytes.chunks(4096).map(<[u8]>::len).collect();
    (bytes, sizes)
}

/// Same corpus, same selectivity, same size: the same content, byte for byte.
#[test]
fn the_legacy_trainer_selects_the_references_content() {
    let fixtures: [(&str, Samples); 3] = [
        ("log lines", log_lines(3000)),
        ("unit files", unit_files()),
        ("decodecorpus blocks", corpus_blocks()),
    ];
    for (name, (samples, sizes)) in &fixtures {
        for selectivity in [0u32, 4, 9, 12] {
            for dict_size in [4096usize, 16 * 1024, 112_640] {
                let ours = legacy_dict_content(samples, sizes, dict_size, selectivity);
                let theirs = reference_content(samples, sizes, dict_size, selectivity);
                let (Some(ours), Some(theirs)) = (ours, theirs) else {
                    panic!("{name} s={selectivity} size={dict_size}: one side trained nothing");
                };
                // The reference places its header in front of the content and
                // lets it overwrite the front when both do not fit, so its
                // content is ours or a tail of it.
                assert!(
                    theirs.len() <= ours.len() && ours.ends_with(&theirs),
                    "{name} s={selectivity} size={dict_size}: {} content bytes against the \
                     reference's {}",
                    ours.len(),
                    theirs.len(),
                );
                assert!(
                    theirs.len() * 10 >= ours.len() * 9,
                    "{name} s={selectivity} size={dict_size}: the header should cost a sliver"
                );
            }
        }
    }
}
