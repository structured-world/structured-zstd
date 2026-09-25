use super::*;

/// The array a comparison sort gives: the definition, not an algorithm.
fn naive(text: &[u8]) -> Vec<u32> {
    let mut sa: Vec<u32> = (0..text.len() as u32).collect();
    sa.sort_by(|&a, &b| text[a as usize..].cmp(&text[b as usize..]));
    sa
}

fn lcg_bytes(seed: u64, len: usize, alphabet: u8) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % u64::from(alphabet)) as u8
        })
        .collect()
}

/// Induced sorting agrees with the definition on the shapes that exercise its
/// branches: tiny texts (the direct cases), runs of one byte (all L-type, no
/// LMS positions), periodic text (equal LMS substrings, so the recursion runs
/// on repeated names), small and full alphabets.
#[test]
fn induced_sorting_matches_the_definition() {
    let fixed: [&[u8]; 9] = [
        b"",
        b"a",
        b"ab",
        b"ba",
        b"banana",
        b"mississippi",
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        b"abababababababababababababababab",
        b"abracadabra abracadabra abracadabra",
    ];
    for text in fixed {
        assert_eq!(suffix_array(text), naive(text), "text {text:?}");
    }
    for (seed, alphabet) in [(1u64, 2u8), (2, 3), (3, 4), (4, 26), (5, 255)] {
        for len in [9usize, 10, 11, 40, 257, 4096] {
            let text = lcg_bytes(seed, len, alphabet);
            assert_eq!(
                suffix_array(&text),
                naive(&text),
                "len {len}, alphabet {alphabet}"
            );
        }
    }
    let mut periodic = Vec::new();
    for _ in 0..300 {
        periodic.extend_from_slice(b"tenant=demo key=");
    }
    assert_eq!(suffix_array(&periodic), naive(&periodic));
}
