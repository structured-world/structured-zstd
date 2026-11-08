use super::{PARALLEL_COUNT_THRESHOLD, count_bytes, count_bytes_scalar, count_bytes_uninit};

fn make_data(len: usize, seed: u64) -> alloc::vec::Vec<u8> {
    let mut state = seed;
    let mut out = alloc::vec![0u8; len];
    for byte in out.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        *byte = (state >> 32) as u8;
    }
    out
}

#[test]
fn count_bytes_matches_scalar_for_large_input() {
    let data = make_data(8192, 0xDEADBEEF);
    let mut fast = [0usize; 256];
    let mut scalar = [0usize; 256];

    let fast_meta = count_bytes(&data, &mut fast);
    let scalar_meta = count_bytes_scalar(&data, &mut scalar);

    assert_eq!(fast, scalar);
    assert_eq!(fast_meta, scalar_meta);
}

#[test]
fn count_bytes_handles_empty_input() {
    let mut counts = [123usize; 256];
    let meta = count_bytes(&[], &mut counts);

    assert_eq!(meta, (0, 0));
    assert!(counts.iter().all(|value| *value == 0));
}

/// The uninitialised-storage entry must ignore whatever the storage held, on
/// both the scalar (increment) and the striped (overwrite) path: stale counts
/// left in it would skew the Huffman table built from them.
#[test]
fn count_bytes_uninit_ignores_stale_storage() {
    for len in [PARALLEL_COUNT_THRESHOLD - 1, PARALLEL_COUNT_THRESHOLD + 7] {
        let data = make_data(len, 7);
        let mut scalar = [0usize; 256];
        let scalar_meta = count_bytes_scalar(&data, &mut scalar);

        let mut storage = core::mem::MaybeUninit::new([0x5555usize; 256]);
        let (counts, max_symbol, largest) = count_bytes_uninit(&data, &mut storage);

        assert_eq!(*counts, scalar);
        assert_eq!((max_symbol, largest), scalar_meta);
    }
}

#[test]
fn count_bytes_parallel_handles_tail() {
    let data = make_data(PARALLEL_COUNT_THRESHOLD + 7, 42);
    let mut fast = [0usize; 256];
    let mut scalar = [0usize; 256];

    let fast_meta = count_bytes(&data, &mut fast);
    let scalar_meta = count_bytes_scalar(&data, &mut scalar);

    assert_eq!(fast, scalar);
    assert_eq!(fast_meta, scalar_meta);
}
