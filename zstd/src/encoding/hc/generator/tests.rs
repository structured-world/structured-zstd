use super::*;

#[cfg(test)]
use crate::encoding::CompressionLevel;
#[cfg(test)]
use crate::encoding::Matcher;
#[cfg(test)]
use crate::encoding::dfast::DfastMatchGenerator;
#[cfg(test)]
use crate::encoding::match_generator::MatchGeneratorDriver;
use alloc::vec::Vec;

#[test]
fn dfast_matches_roundtrip_multi_block_pattern() {
    let pattern = [9, 21, 44, 184, 19, 96, 171, 109, 141, 251];
    let first_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();
    let second_block: Vec<u8> = pattern.iter().copied().cycle().take(128 * 1024).collect();

    let mut matcher = DfastMatchGenerator::new(1 << 22);
    let replay_sequence = |decoded: &mut Vec<u8>, seq: Sequence<'_>| match seq {
        Sequence::Literals { literals } => decoded.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            decoded.extend_from_slice(literals);
            let start = decoded.len() - offset;
            for i in 0..match_len {
                let byte = decoded[start + i];
                decoded.push(byte);
            }
        }
    };

    matcher.commit_input(&first_block);
    let mut history = Vec::new();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));
    assert_eq!(history, first_block);

    matcher.commit_input(&second_block);
    let prefix_len = history.len();
    matcher.start_matching(|seq| replay_sequence(&mut history, seq));

    assert_eq!(&history[prefix_len..], second_block.as_slice());
}

/// Regression for the `DFAST_MIN_MATCH_LEN: 6 -> 5` drop. The fixture
/// is built so the longest available match is EXACTLY 5 bytes — a
/// matcher that still effectively requires a 6-byte floor would emit
/// only literals here and the assertion would catch the silent
/// 5-byte miss.
///
/// Fixture layout (34 B):
///   bytes 0..5    `"ABCDE"`  — match source
///   bytes 5..28   `'!'` × 23 — filler that does NOT start with 'A'
///   bytes 28..33  `"ABCDE"`  — match site (repeats the prefix)
///   byte  33      `'F'`      — terminator: differs from byte 5 (`'!'`),
///                              so the forward extension at the match
///                              site stops at exactly length 5.
///
/// A 5-byte match at offset 28 must be emitted; a 6-byte+ match at the
/// same offset must NOT.
#[test]
fn dfast_accepts_exact_five_byte_match() {
    // Layout the input so that:
    //   byte  0      = 'Z'            (lead byte — keeps the match SOURCE off
    //                                  position 0, which the greedy loop never
    //                                  inserts: like the upstream zstd it starts the
    //                                  cursor at ip+1 and hashes only visited
    //                                  positions)
    //   bytes 1..6   = "ABCDE"        (the match source — position 1 IS visited)
    //   bytes 6..29  = 23 filler bytes that do NOT start with 'A'
    //   bytes 29..34 = "ABCDE"        (the 5-byte match site)
    //   byte  34     = 'F'            (differs from byte 6 = '!')
    // The longest available copy at position 29 is exactly 5 bytes:
    // the byte at position 34 ('F') differs from the byte at position 6
    // ('!'), so the forward extension stops at length 5.
    let mut data = Vec::new();
    data.push(b'Z'); // 0
    data.extend_from_slice(b"ABCDE"); // 1..6
    data.extend_from_slice(b"!!!!!!!!!!!!!!!!!!!!!!!"); // 6..29 (23 bytes)
    data.extend_from_slice(b"ABCDE"); // 29..34
    data.push(b'F'); // 34: forces forward extension to stop at length 5
    // Trailing filler so the match site (29) sits at least HASH_READ_SIZE (8)
    // bytes before the block end. The greedy double-fast — like the upstream zstd —
    // stops probing at `ilimit = iend - HASH_READ_SIZE`, so a match in the
    // final 8 bytes is never searched (upstream zstd parity, not a regression).
    data.extend_from_slice(b"GHIJKLMNOPQRSTUVWXYZ"); // 35..55
    assert_eq!(data.len(), 55);

    let mut matcher = DfastMatchGenerator::new(1 << 22);
    matcher.commit_input(&data);

    let mut saw_five_byte_match = false;
    let mut saw_longer_match = false;
    matcher.start_matching(|seq| {
        if let Sequence::Triple {
            offset, match_len, ..
        } = seq
        {
            if offset == 28 && match_len == 5 {
                saw_five_byte_match = true;
            } else if offset == 28 && match_len > 5 {
                saw_longer_match = true;
            }
        }
    });

    assert!(
        saw_five_byte_match,
        "dfast must accept the exact-5-byte match — a 6-byte floor would skip it"
    );
    assert!(
        !saw_longer_match,
        "fixture pinned to length 5 — byte 33 ('F') must terminate the extension"
    );
}

#[test]
fn driver_switches_backends_and_initializes_dfast_via_reset() {
    let mut driver = MatchGeneratorDriver::new(32, 2);

    driver.reset(CompressionLevel::Default);
    assert_eq!(
        driver.active_backend(),
        crate::encoding::strategy::BackendTag::Dfast
    );
    assert_eq!(driver.window_size(), (1u64 << 21));

    driver.commit_input(b"abcabcabcabc");
    assert_eq!(driver.get_last_space(), b"abcabcabcabc");
    driver.skip_matching_with_hint(None);

    driver.commit_input(b"abcabcabcabc");

    let mut reconstructed = b"abcabcabcabc".to_vec();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(reconstructed, b"abcabcabcabcabcabcabcabc");

    driver.reset(CompressionLevel::Fastest);
    assert_eq!(driver.window_size(), (1u64 << 19));
}

#[test]
fn driver_level5_selects_row_backend() {
    let mut driver = MatchGeneratorDriver::new(32, 2);
    driver.reset(CompressionLevel::Level(5));
    assert_eq!(
        driver.active_backend(),
        crate::encoding::strategy::BackendTag::Row
    );
    // Greedy-specific routing assertion: `MatchGeneratorDriver::start_matching`
    // dispatches the Row backend into `start_matching_greedy` iff
    // `self.parse == ParseMode::Greedy`, so assert that actual selector —
    // round-trip alone passes on the lazy parser too. `row_matcher().lazy_depth`
    // is a secondary corroboration of the same routing decision (a mirror of
    // the parse mode); checking `parse` directly catches a regression even if
    // the two ever drift apart.
    assert_eq!(
        driver.parse,
        crate::encoding::strategy::ParseMode::Greedy,
        "L5 must route to start_matching_greedy (parse == Greedy)",
    );
    assert_eq!(
        driver.row_matcher().lazy_depth,
        0,
        "row matcher lazy_depth must mirror the greedy parse mode",
    );
}

/// Level 4 maps to `StrategyTag::Dfast` (the greedy double-fast, upstream zstd
/// `ZSTD_dfast` — "greedy" is the parse discipline, not the Row/Greedy
/// strategy at Level 5). Round-trip alone doesn't pin match quality (a lazy
/// parser would also reconstruct the input correctly), so this test guards the
/// parse output itself: a small repeating pattern must produce at least one
/// `Sequence::Triple`, so a future regression that emits literals-only (e.g. a
/// `min_match` or rep-probe guard regression) is caught.
#[test]
fn driver_level4_greedy_round_trip_single_slice() {
    let mut driver = MatchGeneratorDriver::new(64, 2);
    driver.reset(CompressionLevel::Level(4));
    let input = b"abcdefgh_abcdefgh_abcdefgh_abcdefgh";
    driver.commit_input(input);

    let mut reconstructed: Vec<u8> = Vec::new();
    let mut saw_triple = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            saw_triple = true;
            reconstructed.extend_from_slice(literals);
            let start = reconstructed.len() - offset;
            for i in 0..match_len {
                let byte = reconstructed[start + i];
                reconstructed.push(byte);
            }
        }
    });
    assert_eq!(
        reconstructed,
        input.to_vec(),
        "L4 greedy parse failed to reconstruct repeating-pattern input",
    );
    assert!(
        saw_triple,
        "L4 greedy parse on a repeating pattern must emit at least one match (Triple)",
    );
}

#[test]
fn driver_level4_greedy_round_trip_cross_slice() {
    // Verifies that the greedy parse carries repcode / hash-table state
    // across slice boundaries: the second slice repeats the first byte
    // for byte, so the parse must pick up matches reaching back into
    // the previous slice's history.
    let mut driver = MatchGeneratorDriver::new(32, 4);
    driver.reset(CompressionLevel::Level(4));
    let chunk = b"the quick brown fox jumps over!!";
    assert_eq!(chunk.len(), 32);

    driver.commit_input(chunk);

    let mut first_recon: Vec<u8> = Vec::new();
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => first_recon.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            first_recon.extend_from_slice(literals);
            let start = first_recon.len() - offset;
            for i in 0..match_len {
                let byte = first_recon[start + i];
                first_recon.push(byte);
            }
        }
    });
    assert_eq!(
        first_recon,
        chunk.to_vec(),
        "first slice failed to round-trip"
    );

    driver.commit_input(chunk);

    let mut full = first_recon.clone();
    let mut saw_cross_slice_match = false;
    driver.start_matching(|seq| match seq {
        Sequence::Literals { literals } => full.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            // A match whose offset reaches >= the current slice's literal
            // run plus the second slice's index means we matched into the
            // first slice — exactly the cross-slice behavior under test.
            if offset >= chunk.len() {
                saw_cross_slice_match = true;
            }
            full.extend_from_slice(literals);
            let start = full.len() - offset;
            for i in 0..match_len {
                let byte = full[start + i];
                full.push(byte);
            }
        }
    });
    let mut expected = chunk.to_vec();
    expected.extend_from_slice(chunk);
    assert_eq!(
        full, expected,
        "cross-slice L4 greedy parse failed to reconstruct"
    );
    assert!(
        saw_cross_slice_match,
        "L4 greedy parse must match across slice boundaries (history is shared)",
    );
}
