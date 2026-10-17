use alloc::vec;
use alloc::vec::Vec;

use super::LdmSequenceStore;
use crate::encoding::opt::ldm::HcRawSeq;

fn seq(lit_length: usize, offset: usize, match_length: usize) -> HcRawSeq {
    HcRawSeq {
        lit_length,
        offset,
        match_length,
    }
}

fn triples(out: &[HcRawSeq]) -> Vec<(usize, usize, usize)> {
    out.iter()
        .map(|s| (s.lit_length, s.offset, s.match_length))
        .collect()
}

/// A block takes its matches with literals counted from its own start, and a
/// later search's literal lengths count from where the previous one stopped.
#[test]
fn blocks_take_their_matches_relative_to_their_start() {
    let mut store = LdmSequenceStore::default();
    store.reset(1000);
    store.searched(&[seq(10, 500, 100), seq(20, 700, 64)], 1300);
    store.searched(&[seq(50, 900, 80)], 1600);
    let mut out = Vec::new();
    store.take(200, 4, &mut out);
    // [1000, 1200): 10 literals, a 100-byte match, 20 literals, a 64-byte one.
    assert_eq!(triples(&out), vec![(10, 500, 100), (20, 700, 64)]);
    store.take(400, 4, &mut out);
    // The third match starts at 1300 + 50, 150 bytes into this block.
    assert_eq!(triples(&out), vec![(150, 900, 80)]);
    assert_eq!(store.taken_end(), 1600);
}

/// A match crossing a block end is cut there and its rest carried into the
/// next block at the same offset; a piece under `min_match` stays literals,
/// on either side.
#[test]
fn a_match_crossing_the_block_end_is_carried() {
    let mut store = LdmSequenceStore::default();
    store.reset(0);
    store.searched(&[seq(10, 300, 200)], 1000);
    let mut out = Vec::new();
    store.take(110, 4, &mut out);
    assert_eq!(triples(&out), vec![(10, 300, 100)]);
    store.take(500, 4, &mut out);
    assert_eq!(triples(&out), vec![(0, 300, 100)]);

    // Cut two bytes into the match: the head is under `min_match` and stays
    // literals, the tail is taken.
    let mut store = LdmSequenceStore::default();
    store.reset(0);
    store.searched(&[seq(10, 300, 200)], 1000);
    store.take(12, 4, &mut out);
    assert!(out.is_empty());
    store.take(500, 4, &mut out);
    assert_eq!(triples(&out), vec![(0, 300, 198)]);

    // And a short tail is literals of the next block.
    let mut store = LdmSequenceStore::default();
    store.reset(0);
    store.searched(&[seq(10, 300, 200), seq(30, 400, 50)], 1000);
    store.take(208, 4, &mut out);
    assert_eq!(triples(&out), vec![(10, 300, 198)]);
    store.take(500, 4, &mut out);
    assert_eq!(triples(&out), vec![(32, 400, 50)]);
}

/// A cut inside a match moves to the match end, or to the window end when the
/// match runs past it; a cut between matches stays.
#[test]
fn a_cut_is_moved_out_of_a_match() {
    let mut store = LdmSequenceStore::default();
    store.reset(100);
    store.searched(&[seq(10, 300, 200), seq(100, 700, 5000)], 10_000);
    // Inside the first match [110, 310): moved to its end, 210 from the start.
    assert_eq!(store.cut_outside_matches(8192, 150), 210);
    // Between the matches: kept.
    assert_eq!(store.cut_outside_matches(8192, 250), 250);
    // Inside the second, which runs past the window: the whole window.
    assert_eq!(store.cut_outside_matches(4096, 1000), 4096);
    // On a match's first byte is not inside it.
    assert_eq!(store.cut_outside_matches(8192, 10), 10);
}

/// Taken matches are dropped once they are the bulk of the store, without
/// moving what is still to come.
#[test]
fn taken_matches_are_dropped() {
    let mut store = LdmSequenceStore::default();
    store.reset(0);
    let found: Vec<HcRawSeq> = (0..100).map(|_| seq(10, 64, 90)).collect();
    store.searched(&found, 10_000);
    let mut out = Vec::new();
    store.take(6000, 4, &mut out);
    assert_eq!(out.len(), 60);
    store.take(4000, 4, &mut out);
    assert_eq!(out.len(), 40);
    assert_eq!(triples(&out)[0], (10, 64, 90));
}
