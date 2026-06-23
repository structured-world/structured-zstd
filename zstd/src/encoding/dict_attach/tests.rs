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
