use super::*;
use crate::decoding::Dictionary;
use std::io::Cursor;
use std::string::ToString;

fn training_data() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..512u32 {
        data.extend_from_slice(
            format!(
                "tenant=demo table=orders key={i} region=eu payload=aaaaabbbbbcccccdddddeeeee\n"
            )
            .as_bytes(),
        );
    }
    data
}

#[test]
fn create_fastcover_dict_from_source_writes_non_empty_output() {
    let sample = training_data();
    let mut out = Vec::new();
    let tuned = create_fastcover_dict_from_source(
        Cursor::new(sample.as_slice()),
        &mut out,
        4096,
        &FastCoverOptions::default(),
        FinalizeOptions::default(),
    )
    .expect("fastcover+finalize should succeed");
    assert!(!out.is_empty());
    assert!(tuned.k > 0);
    assert!(tuned.d > 0);
}

#[test]
fn create_fastcover_raw_dict_from_source_rejects_empty_source() {
    let mut out = Vec::new();
    let err = create_fastcover_raw_dict_from_source(
        Cursor::new(Vec::<u8>::new()),
        &mut out,
        1024,
        &FastCoverOptions::default(),
    )
    .expect_err("empty source must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn create_fastcover_dict_from_source_propagates_finalize_error() {
    let sample = training_data();
    let mut out = Vec::new();
    let err = create_fastcover_dict_from_source(
        Cursor::new(sample.as_slice()),
        &mut out,
        32,
        &FastCoverOptions::default(),
        FinalizeOptions::default(),
    )
    .expect_err("too-small dictionary budget must fail during finalize");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("dictionary size too small"));
}

#[test]
fn create_fastcover_dict_from_source_rejects_empty_source() {
    let mut out = Vec::new();
    let err = create_fastcover_dict_from_source(
        Cursor::new(Vec::<u8>::new()),
        &mut out,
        1024,
        &FastCoverOptions::default(),
        FinalizeOptions::default(),
    )
    .expect_err("empty source must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
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

#[test]
fn train_fastcover_raw_from_slice_rejects_empty_sample() {
    let err = train_fastcover_raw_from_slice(&[], 1024, &FastCoverOptions::default())
        .expect_err("empty sample must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn train_fastcover_raw_from_slice_supports_non_optimized_params() {
    let sample = training_data();
    let options = FastCoverOptions {
        optimize: false,
        k: 128,
        d: 6,
        f: 18,
        ..FastCoverOptions::default()
    };
    let (dict, tuned) =
        train_fastcover_raw_from_slice(sample.as_slice(), 2048, &options).expect("must train");
    assert!(!dict.is_empty());
    assert!(dict.len() <= 2048);
    assert_eq!(tuned.k, 128);
    assert_eq!(tuned.d, 6);
    assert_eq!(tuned.f, 18);
    assert_eq!(tuned.score, 0);
}

#[test]
fn train_fastcover_raw_from_slice_rejects_tiny_sample_with_empty_dict() {
    let sample = b"tiny";
    let err = train_fastcover_raw_from_slice(sample, 1024, &FastCoverOptions::default())
        .expect_err("tiny sample should not produce an empty dictionary successfully");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "training sample is too small for FastCOVER"
    );
}

#[test]
fn train_fastcover_raw_from_slice_normalizes_non_optimized_params() {
    let sample = training_data();
    let options = FastCoverOptions {
        optimize: false,
        k: 8,
        d: 64,
        f: 42,
        ..FastCoverOptions::default()
    };
    let (_, tuned) =
        train_fastcover_raw_from_slice(sample.as_slice(), 2048, &options).expect("must train");
    assert_eq!(tuned.k, 32);
    assert_eq!(tuned.d, 32);
    assert_eq!(tuned.f, 20);
}

#[test]
fn finalize_raw_dict_rejects_empty_raw_content() {
    let sample = training_data();
    let err = finalize_raw_dict(&[], sample.as_slice(), 4096, FinalizeOptions::default())
        .expect_err("empty raw dictionary must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn finalize_raw_dict_rejects_too_small_budget() {
    let sample = training_data();
    let raw = b"some-raw-bytes";
    let err = finalize_raw_dict(raw, sample.as_slice(), 32, FinalizeOptions::default())
        .expect_err("tiny dict_size must fail");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("dictionary size too small"));
}

#[test]
fn finalize_raw_dict_pads_to_minimum_content_size() {
    let sample = training_data();
    let raw = b"x";
    let finalized = finalize_raw_dict(raw, sample.as_slice(), 4096, FinalizeOptions::default())
        .expect("finalize should pad small raw content");
    let parsed = Dictionary::decode_dict(finalized.as_slice()).expect("finalized dict parses");
    assert!(parsed.dict_content.len() >= 8);
    assert_eq!(parsed.dict_content.last(), Some(&b'x'));
}

#[test]
fn finalize_raw_dict_rejects_zero_dict_id() {
    let sample = training_data();
    let raw = b"raw-fastcover-bytes";
    let err = finalize_raw_dict(
        raw,
        sample.as_slice(),
        4096,
        FinalizeOptions { dict_id: Some(0) },
    )
    .expect_err("dict_id=0 must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(err.to_string(), "dictionary id must be non-zero");
}
