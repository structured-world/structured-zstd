use super::*;
use alloc::{vec, vec::Vec};

#[test]
fn lifecycle_attach_prime_invalidate() {
    let mut da: DictAttach<Vec<u32>> = DictAttach::new();
    assert!(!da.is_attached());
    assert!(!da.is_primed());
    assert_eq!(da.region_len(), 0);

    da.set_region_len(128);
    // mark_primed is a no-op while no table exists.
    da.mark_primed();
    assert!(!da.is_primed());

    da.table_mut_or_init(|| vec![0u32; 16]).fill(7);
    assert!(da.is_attached());
    assert_eq!(da.table().unwrap()[0], 7);

    da.mark_primed();
    assert!(da.is_primed());
    assert_eq!(da.region_len(), 128);

    da.invalidate();
    assert!(!da.is_attached());
    assert!(!da.is_primed());
    assert_eq!(da.region_len(), 0);
}

/// A built table of `len` slots holding `value`, attached and primed, with
/// boundary `region` and fill origin `region - 1`.
fn built(len: usize, value: u32, region: usize) -> DictAttach<Vec<u32>> {
    let mut da = DictAttach::new();
    da.table_mut_or_init(|| vec![value; len]);
    da.set_region_len(region);
    da.set_next_to_update(region - 1);
    da.mark_primed();
    da
}

/// A parked table comes back whole, built state included, when accepted,
/// and is detached meanwhile.
#[test]
fn a_parked_table_revives_as_it_was() {
    let mut da = built(16, 7, 100);
    let mut keep = DictKeep::new();
    keep.park(&mut da);
    assert!(!da.is_attached());
    assert!(!da.is_primed());
    assert_eq!(da.region_len(), 0);
    assert_eq!(keep.table().map(Vec::len), Some(16));

    let mut seen = None;
    assert!(keep.revive(&mut da, |table, region| {
        seen = Some((table[0], region));
        true
    }));
    assert_eq!(seen, Some((7, 100)));
    assert!(da.is_attached() && da.is_primed());
    assert_eq!(da.region_len(), 100);
    assert_eq!(da.next_to_update(), 99);
    assert!(keep.table().is_none());
}

/// A parked table the next prime does not take is kept only for its
/// allocation: it never comes back built, and a build can take it.
#[test]
fn a_refused_table_becomes_the_spare() {
    let mut da = built(16, 7, 100);
    let mut keep = DictKeep::new();
    keep.park(&mut da);
    assert!(!keep.revive(&mut da, |_, _| false));
    assert!(!da.is_attached());
    assert!(!keep.revive(&mut da, |_, _| true));
    assert_eq!(keep.take_spare().map(|t| t.len()), Some(16));
    assert!(keep.take_spare().is_none());
}

/// A parked table is not a spare: a build cannot empty it, and a table
/// parked before it was fully built does not displace it.
#[test]
fn a_parked_table_is_not_spare() {
    let mut da = built(16, 7, 100);
    let mut keep = DictKeep::new();
    keep.park(&mut da);
    assert!(keep.take_spare().is_none());

    da.table_mut_or_init(|| vec![1u32; 8]);
    keep.park(&mut da);
    assert!(!da.is_attached());
    assert!(keep.revive(&mut da, |table, region| table[0] == 7 && region == 100));
}

/// A dictionary change keeps one table only as the spare; releasing drops it.
#[test]
fn a_dictionary_change_keeps_only_an_allocation() {
    let mut da = built(16, 7, 100);
    let mut keep = DictKeep::new();
    keep.forget(&mut da);
    assert!(!da.is_attached());
    assert!(!keep.revive(&mut da, |_, _| true));
    assert_eq!(keep.take_spare().map(|t| t.len()), Some(16));

    let mut da = built(16, 7, 100);
    keep.park(&mut da);
    keep.forget(&mut da);
    assert!(!keep.revive(&mut da, |_, _| true));
    keep.release();
    assert!(keep.table().is_none());
}

/// What a holder keeps is a cache of its own: a clone of it is empty.
#[test]
fn a_clone_keeps_nothing() {
    let mut da = built(16, 7, 100);
    let mut keep = DictKeep::new();
    keep.park(&mut da);
    assert!(keep.clone().table().is_none());
    assert!(keep.table().is_some());
}
