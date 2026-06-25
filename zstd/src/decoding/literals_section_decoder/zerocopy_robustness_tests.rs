//! Regression coverage for `decode_literals_zerocopy` on
//! truncated / corrupt payloads: every branch must return a
//! structured error instead of panicking on out-of-bounds
//! slice indexing. Hit each `*[..n]` / `*[0]` index in the
//! function with a payload one byte short of what the header
//! declares.
//
// Tests live in a separate module so the broader `burst_gate_tests`
// module's helpers don't have to depend on truncated-input
// builders.
use super::{LiteralsView, decode_literals_zerocopy};
use crate::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
use crate::decoding::scratch::HuffmanScratch;
use alloc::vec::Vec;

fn raw_section(regen: u32) -> LiteralsSection {
    LiteralsSection {
        ls_type: LiteralsSectionType::Raw,
        regenerated_size: regen,
        compressed_size: None,
        num_streams: None,
    }
}

fn rle_section(regen: u32) -> LiteralsSection {
    LiteralsSection {
        ls_type: LiteralsSectionType::RLE,
        regenerated_size: regen,
        compressed_size: None,
        num_streams: None,
    }
}

fn fresh_scratch() -> HuffmanScratch {
    HuffmanScratch::new()
}

#[test]
fn raw_truncated_source_returns_error_no_panic() {
    // Header claims 10 raw literal bytes, source carries 3.
    // Indexing `source[0..10]` would panic; the fix must turn
    // it into a structured DecompressLiteralsError.
    let section = raw_section(10);
    let source: [u8; 3] = [1, 2, 3];
    let mut target: Vec<u8> = Vec::new();
    let mut scratch = fresh_scratch();
    let result = decode_literals_zerocopy(&section, &mut scratch, None, &source, &mut target);
    assert!(
        result.is_err(),
        "truncated raw source must error, not panic; got {:?}",
        result.map(|_| ())
    );
}

#[test]
fn rle_empty_source_returns_error_no_panic() {
    // RLE section needs at least one source byte (the fill byte).
    // Indexing `source[0]` on an empty slice would panic.
    let section = rle_section(10);
    let source: [u8; 0] = [];
    let mut target: Vec<u8> = Vec::new();
    let mut scratch = fresh_scratch();
    let result = decode_literals_zerocopy(&section, &mut scratch, None, &source, &mut target);
    assert!(
        result.is_err(),
        "empty RLE source must error, not panic; got {:?}",
        result.map(|_| ())
    );
}

#[test]
fn compressed_truncated_source_returns_error_no_panic() {
    // Header claims compressed_size = 10 but the source carries 3 bytes.
    // Slicing `source[0..10]` for a Compressed/Treeless section would
    // panic (decoder DoS on truncated input); the fix must turn it into
    // a structured DecompressLiteralsError, matching the Raw/RLE paths.
    let section = LiteralsSection {
        ls_type: LiteralsSectionType::Compressed,
        regenerated_size: 5,
        compressed_size: Some(10),
        num_streams: Some(1),
    };
    let source: [u8; 3] = [1, 2, 3];
    let mut target: Vec<u8> = Vec::new();
    let mut scratch = fresh_scratch();
    let result = decode_literals_zerocopy(&section, &mut scratch, None, &source, &mut target);
    // Pin the EXACT contract: a truncated Compressed section must report
    // MissingBytesForLiterals with the precise got/needed, not just "some
    // error" (a weaker is_err() would also pass on MissingNumStreams /
    // UninitializedHuffmanTable, missing the real regression).
    assert!(
        matches!(
            &result,
            Err(
                crate::decoding::errors::DecompressLiteralsError::MissingBytesForLiterals {
                    got: 3,
                    needed: 10,
                }
            )
        ),
        "truncated compressed source must report MissingBytesForLiterals, got {:?}",
        result.map(|_| ())
    );
}

#[test]
fn rle_view_excludes_pre_existing_target_bytes() {
    // Even if the caller forgot to clear `target`, the returned
    // LiteralsView::data must point only at the bytes this call
    // produced. The API hardening (`&target[base..]`) is what
    // makes this hold.
    let mut target: Vec<u8> = Vec::from([0xAA, 0xBB, 0xCC]);
    let section = rle_section(4);
    let source: [u8; 1] = [0x42];
    let mut scratch = fresh_scratch();
    let view = decode_literals_zerocopy(&section, &mut scratch, None, &source, &mut target)
        .expect("RLE with valid source must succeed");
    assert_eq!(view.data.len(), 4, "view length must match regen_size");
    assert!(
        view.data.iter().all(|&b| b == 0x42),
        "view must contain only the newly-RLE-expanded bytes, got {:?}",
        view.data
    );
    // Silence unused-warning if the compiler ever strips
    // LiteralsView fields — read bytes_used too.
    let _ = LiteralsView {
        data: view.data,
        bytes_used: view.bytes_used,
    };
}
