use super::*;

/// FNV-1a, enough to pin a buffer's content against the reference generators.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The lorem ipsum text is the reference command's, byte for byte, at every
/// size including the cut-and-padded ends (hashes taken from upstream's
/// `LOREM_genBuffer` with seed 0), so a synthetic benchmark measures the same
/// input on both tools.
#[test]
fn lorem_matches_the_reference_generator() {
    let expected = [
        (0, 0xcbf2_9ce4_8422_2325),
        (1, 0xaf63_a34c_8601_8bb1),
        (7, 0x5e77_5822_5b5d_71aa),
        (100, 0x3e7f_a7c7_f609_249e),
        (4096, 0xaee1_0b2d_0e68_d191),
        (1_000_000, 0xe82b_4abf_d1f2_e32b),
        (DEFAULT_SIZE, 0x32a1_2e43_3722_227b),
    ];
    for (size, hash) in expected {
        let text = lorem(size).unwrap();
        assert_eq!(text.len(), size);
        assert_eq!(fnv(&text), hash, "lorem ipsum of {size} bytes");
    }
    assert!(
        lorem(64)
            .unwrap()
            .starts_with(b"Lorem ipsum dolor sit amet, ")
    );
}

/// `-P#` data is upstream's `RDG_genBuffer` with seed 0, byte for byte,
/// across the literal-only, mixed and sparse shapes.
#[test]
fn compressible_matches_the_reference_generator() {
    let expected = [
        (0, 1, 0xaf64_384c_8602_88e0),
        (5, 1, 0xaf63_a74c_8601_927d),
        (50, 1, 0xaf63_b64c_8601_abfa),
        (90, 1, 0xaf63_a84c_8601_9430),
        (100, 1, 0xaf63_bd4c_8601_b7df),
        (0, 7, 0x8b24_31ad_14c8_622e),
        (5, 7, 0x1a24_0731_69bc_21a8),
        (50, 7, 0xdb96_c823_6bf3_2b4c),
        (90, 7, 0x362d_37b8_0e1f_862a),
        (100, 7, 0x778b_1a14_b687_6aa7),
        (0, 100, 0xf43c_a9b6_e9f8_16b0),
        (5, 100, 0xee47_bbb8_46b3_9cbd),
        (50, 100, 0x5b6b_c4b7_92d1_f61e),
        (90, 100, 0xcbe4_51b4_6530_49d1),
        (100, 100, 0x1fc0_5eb3_3785_8375),
        (0, 4096, 0x1fca_be9e_6ee7_7f67),
        (5, 4096, 0x6606_e5c4_b8cb_eec4),
        (50, 4096, 0x9abb_6f27_1479_cbcc),
        (90, 4096, 0x0706_5760_d233_efa7),
        (100, 4096, 0xb93a_0c83_ce3b_6325),
        (0, 1_000_000, 0xe289_3ee4_4496_94b0),
        (5, 1_000_000, 0x1d10_8a9d_4207_e935),
        (50, 1_000_000, 0xd9a7_b1d5_f18a_fd16),
        (90, 1_000_000, 0x8e1b_0f6b_0670_fac1),
        (100, 1_000_000, 0xec6d_84ab_0a18_5fc6),
    ];
    for (percent, size, hash) in expected {
        let data = compressible(size, percent).unwrap();
        assert_eq!(data.len(), size);
        assert_eq!(fnv(&data), hash, "-P{percent} data of {size} bytes");
    }
    assert!(compressible(0, 50).unwrap().is_empty());
    // A size no allocator can give is refused, not an abort.
    assert!(compressible(usize::MAX, 50).is_err());
    assert!(lorem(usize::MAX).is_err());
}
