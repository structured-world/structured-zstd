//! Contains `compute_frequency`, a function
//! that uses a rolling Karp-Rabin hash to
//! efficiently count the number of occurences
//! of a given k-mer within a set.

/// Computes a best effort guess as to how many times `pattern` occurs within
/// `body`. While not 100% accurate, it will be accurate the vast majority of time
pub fn estimate_frequency(pattern: &[u8], body: &[u8]) -> usize {
    if pattern.is_empty() {
        return 0;
    }
    assert!(body.len() >= pattern.len());
    // A prime number for modulo operations to reduce collisions (q)
    const PRIME: i64 = 2654435761;
    // Number of characters in the input alphabet (d)
    const ALPHABET_SIZE: i64 = 256;
    // Hash of input pattern (p)
    let mut pattern_hash: i64 = 0;
    // Hash of the current window of text (t)
    let mut window_hash: i64 = 0;
    // High-order digit multiplier: h = ALPHABET_SIZE^(pattern.len()-1) mod PRIME
    let mut h: i64 = 1;
    for _ in 0..pattern.len().saturating_sub(1) {
        h = (h * ALPHABET_SIZE) % PRIME;
    }

    // Compute initial hash values
    for i in 0..pattern.len() {
        pattern_hash = (ALPHABET_SIZE * pattern_hash + pattern[i] as i64) % PRIME;
        window_hash = (ALPHABET_SIZE * window_hash + body[i] as i64) % PRIME;
    }

    let mut num_occurrences = 0;
    for i in 0..=body.len() - pattern.len() {
        if pattern_hash == window_hash {
            num_occurrences += 1;
        }

        // Compute hash for next window using rolling hash
        if i < body.len() - pattern.len() {
            window_hash = (ALPHABET_SIZE * (window_hash - body[i] as i64 * h)
                + body[i + pattern.len()] as i64)
                % PRIME;
            // Ensure non-negative (Euclidean modulo)
            if window_hash < 0 {
                window_hash += PRIME;
            }
        }
    }

    num_occurrences
}

#[cfg(test)]
mod tests {
    use super::estimate_frequency;
    #[test]
    fn dead_beef() {
        assert_eq!(
            estimate_frequency(&[0xde, 0xad], &[0xde, 0xad, 0xbe, 0xef, 0xde, 0xad]),
            2
        );
    }

    #[test]
    fn smallest_body() {
        assert_eq!(estimate_frequency(&[0x00, 0xff], &[0x00, 0xff]), 1);
    }

    #[test]
    fn no_match() {
        assert_eq!(
            estimate_frequency(&[0xff, 0xff], &[0xde, 0xad, 0xbe, 0xef, 0xde, 0xad]),
            0
        );
    }
}
