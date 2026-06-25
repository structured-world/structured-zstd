use super::*;

#[test]
fn huffman() {
    let table = HuffmanTable::build_from_weights(&[2, 2, 2, 1, 1]);
    assert_eq!(table.codes[0], (1, 2));
    assert_eq!(table.codes[1], (2, 2));
    assert_eq!(table.codes[2], (3, 2));
    assert_eq!(table.codes[3], (0, 3));
    assert_eq!(table.codes[4], (1, 3));

    let table = HuffmanTable::build_from_weights(&[4, 3, 2, 0, 1, 1]);
    assert_eq!(table.codes[0], (1, 1));
    assert_eq!(table.codes[1], (1, 2));
    assert_eq!(table.codes[2], (1, 3));
    assert_eq!(table.codes[3], (0, 0));
    assert_eq!(table.codes[4], (0, 4));
    assert_eq!(table.codes[5], (1, 4));
}

// Regression: the non-search literal-table path (fast / negative levels,
// `build_from_counts_gated(use_search=false)`) builds
// `build_from_weights(build_limited_weights(counts, 11))` with NO power-of-two
// guard, so any `counts` whose height-limited weights break the canonical
// `Σ 2^(weight-1)` invariant panics in `build_from_weights`. Height limiting
// must ALWAYS restore that invariant (full Kraft sum). Fuzz to prove it does.
#[test]
fn build_limited_weights_always_power_of_two() {
    // A real-shaped literal histogram: 100 symbols with small, skewed counts
    // (a ~60:1 spread), exactly what a single block produces. Its natural
    // Huffman depth exceeds the 11-bit limit, and the broken limiter left the
    // code under-full here, projecting to non-power-of-two weights that
    // panicked the table builder on the non-search literal path.
    const TRIGGER: &[usize] = &[
        53, 53, 45, 13, 21, 31, 36, 16, 59, 25, 27, 19, 50, 56, 49, 34, 38, 49, 24, 50, 61, 30, 54,
        6, 62, 50, 34, 61, 15, 37, 34, 61, 26, 49, 21, 59, 30, 31, 17, 14, 51, 14, 60, 30, 34, 1,
        49, 25, 58, 1, 41, 19, 49, 34, 42, 2, 55, 11, 17, 40, 34, 25, 13, 26, 56, 19, 19, 61, 2, 2,
        45, 24, 53, 10, 31, 46, 61, 49, 38, 10, 14, 28, 26, 19, 20, 42, 18, 34, 44, 55, 1, 0, 37,
        41, 1, 33, 1, 25, 46, 52,
    ];
    assert!(
        huffman_weight_sum_is_power_of_two(&build_limited_weights(TRIGGER, 11)),
        "height limiter left a non-power-of-two weight sum on a real-shaped histogram"
    );
    let _ = HuffmanTable::build_from_counts_gated(TRIGGER, false);

    // A modest randomized sweep keeps ongoing breadth in the default suite
    // without dominating its wall-clock; the deep 300k sweep is the separate
    // `#[ignore]` stress test below.
    fuzz_limited_weights_power_of_two(4_000);
}

/// Deep randomized sweep over the height limiter. Excluded from the default
/// run (it is ~30 s); invoke explicitly with `cargo nextest run --run-ignored`.
#[test]
#[ignore = "stress: 300k-case height-limiter fuzz, run with --run-ignored"]
fn build_limited_weights_power_of_two_stress() {
    fuzz_limited_weights_power_of_two(300_000);
}

/// Drive `iterations` randomized histograms through the height limiter,
/// asserting every one yields a power-of-two Kraft sum (and builds without
/// panicking on the non-search literal path).
fn fuzz_limited_weights_power_of_two(iterations: usize) {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..iterations {
        let n = 2 + (next() % 255) as usize;
        let skew = (next() % 6) as u32;
        let mut counts = alloc::vec![0usize; n];
        // Counts are byte-frequency histograms of a single block, so each is
        // bounded by the max block size; keep the fuzz within that envelope.
        const MAX_COUNT: usize = 128 * 1024;
        match skew {
            // Fibonacci-like + geometric distributions force a natural Huffman
            // depth well past 11 (a Fibonacci ladder hits ~26 distinct values
            // under MAX_COUNT), so the height limiter actually runs (and its
            // Kraft-sum restoration is exercised) instead of a shallow tree.
            4 => {
                let (mut a, mut b) = (1usize, 1usize);
                for c in counts.iter_mut() {
                    *c = a;
                    let nb = (a + b).min(MAX_COUNT);
                    a = b;
                    b = nb;
                }
            }
            5 => {
                let mut v = MAX_COUNT;
                for c in counts.iter_mut() {
                    *c = v.max(1);
                    v = (v * (2 + (next() % 2) as usize)) / 3;
                }
            }
            _ => {
                for c in counts.iter_mut() {
                    *c = match skew {
                        0 => (next() % 64) as usize,
                        1 => (next() % 4) as usize,
                        2 => (next() % 4096) as usize,
                        _ => 1 + (next() % 3) as usize,
                    };
                }
            }
        }
        if counts.iter().filter(|&&c| c > 0).count() < 2 {
            continue;
        }
        let weights = build_limited_weights(&counts, 11);
        assert!(
            huffman_weight_sum_is_power_of_two(&weights),
            "build_limited_weights broke the power-of-two invariant: counts={counts:?} weights={weights:?}"
        );
        let _ = HuffmanTable::build_from_counts_gated(&counts, false);
    }
}

/// Degenerate alphabets take the height limiter's `leaves.len() <= 1`
/// early-out: a single non-zero symbol maps to a one-bit code (power-of-two
/// weight sum), and an all-zero histogram finds no leaf and stays all-zero. A
/// real block always has at least one literal, so the all-zero input never
/// reaches production; it is asserted only for the actual early-out behavior
/// (all-zero weights), not the power-of-two invariant a zero-symbol code cannot
/// satisfy. A two-symbol alphabet is the smallest input that runs the full
/// build past the early-out.
#[test]
fn build_limited_weights_handles_degenerate_alphabets() {
    // Single non-zero symbol: the lone leaf gets weight 1, every other slot 0.
    let mut single = alloc::vec![0usize; 8];
    single[3] = 1000;
    let w = build_limited_weights(&single, 11);
    assert!(
        huffman_weight_sum_is_power_of_two(&w),
        "single-symbol weights broke the power-of-two invariant: {w:?}"
    );
    assert_eq!(w[3], 1, "the only symbol should map to a one-bit code");
    assert!(
        w.iter().enumerate().all(|(i, &x)| i == 3 || x == 0),
        "no symbol other than the single non-zero one may carry a weight: {w:?}"
    );

    // Two symbols: the smallest alphabet that runs the full build path
    // (leaves.len() > 1) rather than the degenerate early-out.
    let mut pair = alloc::vec![0usize; 8];
    pair[1] = 600;
    pair[5] = 400;
    let w2 = build_limited_weights(&pair, 11);
    assert!(
        huffman_weight_sum_is_power_of_two(&w2),
        "two-symbol weights broke the power-of-two invariant: {w2:?}"
    );

    // All-zero histogram: the early-out finds no leaf and leaves every weight
    // at zero. This never occurs for a real block (always >= 1 literal), so the
    // power-of-two invariant (which a zero-symbol code cannot meet) is not
    // claimed; assert only the actual degenerate output.
    let empty = build_limited_weights(&alloc::vec![0usize; 8], 11);
    assert!(
        empty.iter().all(|&w| w == 0),
        "all-zero histogram must yield all-zero weights: {empty:?}"
    );
}

#[test]
fn weights() {
    // assert_eq!(distribute_weights(5).as_slice(), &[1, 1, 2, 3, 4]);
    for amount in 2..=256 {
        let mut weights = distribute_weights(amount);
        assert_eq!(weights.len(), amount);
        let sum = weights
            .iter()
            .copied()
            .map(|weight| 1 << weight)
            .sum::<usize>();
        assert!(sum.is_power_of_two());

        for num_bit_limit in (amount.ilog2() as usize + 1)..=11 {
            redistribute_weights(&mut weights, num_bit_limit);
            let sum = weights
                .iter()
                .copied()
                .map(|weight| 1 << weight)
                .sum::<usize>();
            assert!(sum.is_power_of_two());
            assert!(
                sum.ilog2() <= 11,
                "Max bits too big: sum: {} {weights:?}",
                sum
            );

            let codes = HuffmanTable::build_from_weights(&weights).codes;
            for (code, num_bits) in codes.iter().copied() {
                for (code2, num_bits2) in codes.iter().copied() {
                    if num_bits == 0 || num_bits2 == 0 || (code, num_bits) == (code2, num_bits2) {
                        continue;
                    }
                    if num_bits <= num_bits2 {
                        let code2_shifted = code2 >> (num_bits2 - num_bits);
                        assert_ne!(
                            code, code2_shifted,
                            "{code:b},{num_bits:} is prefix of {code2:b},{num_bits2:}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn counts() {
    let counts = &[3, 0, 4, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    assert_eq!(table[1].1, 0);
    assert!(table[3].1 >= table[0].1);
    assert!(table[0].1 >= table[2].1);
    assert!(table[2].1 >= table[4].1);

    let counts = &[3, 0, 4, 0, 7, 2, 2, 2, 0, 2, 2, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    assert_eq!(table[1].1, 0);
    assert_eq!(table[3].1, 0);
    assert_eq!(table[8].1, 0);
    assert!(table[11].1 >= table[5].1);
    assert!(table[5].1 >= table[6].1);
    assert!(table[6].1 >= table[7].1);
    assert!(table[7].1 >= table[9].1);
    assert!(table[9].1 >= table[10].1);
    assert!(table[10].1 >= table[0].1);
    assert!(table[0].1 >= table[2].1);
    assert!(table[2].1 >= table[12].1);
    assert!(table[12].1 >= table[4].1);
}

#[test]
fn from_data() {
    let counts = &[3, 0, 4, 1, 2];
    let table = HuffmanTable::build_from_counts(counts).codes;

    let data = &[0, 2, 4, 4, 0, 3, 2, 2, 0, 2];
    let table2 = HuffmanTable::build_from_data(data).codes;

    assert_eq!(table, table2);
}

/// `cheap_desc_size_proxy` is the cheap analytic estimate used inside
/// `HuffmanTable::build_from_counts` to score `table_log` candidates
/// without paying a full FSE encode per iteration. Issue #167.
///
/// Sanity invariants checked here on synthetic weight distributions:
///
/// - The proxy is **conservative** vs the exact serialized size — it
///   may overestimate by a few bytes (entropy upper bound + 8 B FSE
///   header constant), but **never undershoots so far that the proxy
///   estimate falls below the raw nibble representation** for the same
///   weight stream. This is the guardrail that prevents the loop from
///   picking a `table_log` whose real description is larger than the
///   proxy claims.
/// - The proxy returns `Some` exactly when the real
///   `encode_weight_description` / raw fallback would also produce a
///   serializable description.
#[test]
fn cheap_desc_size_proxy_is_conservative_vs_exact() {
    // Fixtures are synthesized via `HuffmanTable::build_from_counts` so
    // every weight vector is Kraft-valid by construction (the encoder's
    // own output passes its own `huffman_weight_sum_is_power_of_two`
    // gate). Hand-curated weight arrays were prone to silently being
    // rejected by the Kraft check, leaving the test body unreached
    // (caught by CodeRabbit on PR #168).
    //
    // Each case is `(counts_input, label)` — fed through
    // `build_from_counts`, then `table.weights()` is the full weight
    // vector and `[..len-1]` is what `try_table_description_size`
    // trims internally before calling the encoder. The proxy is
    // exercised on the same trimmed slice for a fair comparison.
    let cases: &[(Vec<usize>, &str)] = &[
        (alloc::vec![5, 3, 2, 1], "4-symbol skewed"),
        (alloc::vec![1, 1, 1, 1, 1, 1, 1, 1], "8-symbol uniform"),
        (alloc::vec![100, 50, 25, 12, 6, 3, 2, 1], "geometric decay"),
        // Wider alphabet: cycle counts over 32 symbols. Build will
        // produce a valid Huffman code regardless of exact frequencies.
        ((1..=32usize).collect(), "32-symbol increasing"),
        // Very wide alphabet that pushes weight count near the raw limit.
        ((1..=120usize).collect(), "120-symbol near raw limit"),
    ];
    let mut exercised = 0usize;
    for (counts, label) in cases {
        let table = HuffmanTable::build_from_counts(counts);
        let weights = table.weights();
        if weights.is_empty() {
            // Single-cardinality fallback path can produce empty
            // weights; nothing for the proxy to score.
            continue;
        }
        // `try_table_description_size` trims internally; mirror that
        // on the proxy call so both score the same slice.
        let trimmed = &weights[..weights.len() - 1];
        let exact = table.try_table_description_size();
        let proxy = cheap_desc_size_proxy(trimmed);
        match (proxy, exact) {
            (Some(p), Some(e)) => {
                exercised += 1;
                // Raw representation floor on the trimmed slice — what
                // `write_raw_weight_description` would actually emit
                // for `trimmed`: ceil(n/2) packed nibbles + 1 length
                // byte. The proxy must either be within +2 B of the
                // exact size or at least cover this floor (overestimate
                // is fine; under-shooting raw is the bug we're
                // guarding against).
                let raw_floor = trimmed.len().div_ceil(2) + 1;
                assert!(
                    p + 2 >= e || p >= raw_floor,
                    "[{label}] proxy {p} under-shot exact {e} (raw_floor {raw_floor})"
                );
            }
            (None, None) => {} // both reject — fine (empty trimmed slice case)
            (proxy_res, exact_res) => panic!(
                "[{label}] proxy/exact disagreement on representability: proxy={proxy_res:?} exact={exact_res:?}"
            ),
        }
    }
    assert!(
        exercised > 0,
        "no fixture exercised the proxy/exact assertion — synthetic counts must produce Kraft-valid Huffman tables"
    );
}

/// Edge-case coverage for [`cheap_desc_size_proxy`] — every return arm of
/// the `(fse_ok, raw_ok)` match exercised + the `n == 0` early-out + the
/// `ratio <= 1` clamp. Plugs uncovered branches that the
/// `is_conservative_vs_exact` table didn't reach. Issue #167.
#[test]
fn cheap_desc_size_proxy_edge_cases() {
    // `n == 0` → `None` (early-out before the histogram loop).
    assert_eq!(cheap_desc_size_proxy(&[]), None);

    // `n == 1`: single symbol, ratio = 1 / 1 = 1 → `<= 1` clamp branch
    // fires (1 bit / symbol minimum). FSE estimate = 1 byte payload + 8
    // header = 9 B; raw = 1.div_ceil(2) + 1 = 2 B. Proxy picks min = 2.
    assert_eq!(cheap_desc_size_proxy(&[3]), Some(2));

    // Highly-skewed (one dominant weight): exercises the `ratio > 1`
    // branch with `bits_per_symbol == 1` for the dominant bin.
    let skew = alloc::vec![1u8; 64];
    let s = cheap_desc_size_proxy(&skew).expect("skewed-small case must be representable");
    assert!(s <= 64usize.div_ceil(2) + 1, "skewed proxy {s} ≤ raw 33");

    // Exactly at the raw boundary (`weights.len() == 128`): raw is
    // representable, both arms reachable depending on which is smaller.
    let at_limit: Vec<u8> = (0u8..13).cycle().take(128).collect();
    let s = cheap_desc_size_proxy(&at_limit).expect("len=128 stays in (_, raw_ok=true)");
    assert!(s > 0);

    // Past raw boundary (`weights.len() == 129`): `raw_ok = false`.
    // The 13-bin uniform-ish histogram still fits FSE → `(true, false)` arm.
    let over_raw: Vec<u8> = (0u8..13).cycle().take(129).collect();
    let s = cheap_desc_size_proxy(&over_raw)
        .expect("uniform 129-symbol stream still fits FSE: (true, false) arm");
    assert!(s > 0);

    // High-entropy + huge length: both representations fail →
    // `(false, false)` arm returns `None`. With 256 weights cycling
    // over 13 bins, `bits/sym ≈ ceil_log2(ceil(256/20)) = 4`. Total
    // payload bits ≈ 1024 b = 128 B, +8 header = 136 > 128 → fse_ok=false.
    // raw is also off the table (256 > 128) → None.
    let way_over: Vec<u8> = (0u8..13).cycle().take(256).collect();
    assert_eq!(
        cheap_desc_size_proxy(&way_over),
        None,
        "huge high-entropy stream hits (false, false) → None"
    );
}

#[test]
fn encoded_weight_description_roundtrips() {
    let data = &include_bytes!("../../../decodecorpus_files/z000033")[..16 * 1024];
    let table = HuffmanTable::build_from_data(data);
    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.write_table();
        writer.flush();
    }

    let mut decoded = crate::huff0::huff0_decoder::HuffmanTable::new();
    decoded.build_decoder(&encoded).unwrap();
    let decoded = decoded.to_encoder_table().unwrap();

    let table_weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.weights()
    };
    let decoded_weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&decoded, &mut writer);
        encoder.weights()
    };
    assert_eq!(table_weights, decoded_weights);
}

#[test]
fn fse_weight_descriptions_roundtrip() {
    // Regression for the FSE weight-description encode/decode bug: every weight
    // stream that `encode_weight_description` actually FSE-encodes (i.e. passes
    // the upstream-zstd early-outs) MUST decode back to the same weights, so the
    // encoder can trust its output without a runtime round-trip. Sweep many
    // (cardinality, distribution) alphabets; for each, FSE-encode the weight
    // description exactly as `encode_weight_description` does and confirm it
    // round-trips. Before the early-outs, a single-distinct-weight (uniform)
    // alphabet such as 4 symbols → weights [1,1,1] produced a description the
    // decoder rejected.
    let mut fails: Vec<(usize, u32, alloc::vec::Vec<u8>)> = alloc::vec::Vec::new();
    for card in 2usize..=255 {
        for skew in 0u32..4 {
            let mut data: Vec<u8> = Vec::new();
            for s in 0..card {
                let n = match skew {
                    0 => 1usize,
                    1 => s + 1,
                    2 => card - s,
                    _ => ((s * 7 + 1) % 17) + 1,
                };
                data.extend(core::iter::repeat_n(s as u8, n));
            }
            let table = HuffmanTable::build_from_data(&data);
            let mut weights = {
                let mut out = Vec::new();
                let mut writer = BitWriter::from(&mut out);
                let encoder = HuffmanEncoder::new(&table, &mut writer);
                encoder.weights()
            };
            weights.pop(); // serialized description omits the final weight
            if weights.len() <= 2 {
                continue;
            }
            // Call the PRODUCTION encoder directly so the test can never drift
            // from its early-out / FSE-encode logic (re-implementing the counts
            // + early-outs inline would silently diverge if the encoder
            // changed). `encode_weight_description` returns Some(fse_bytes) only
            // for streams it actually FSE-encodes; None means it chose the raw
            // description (nothing to round-trip). Every Some MUST decode back.
            let Some(encoded) = HuffmanEncoder::<Vec<u8>>::encode_weight_description(&weights)
            else {
                continue;
            };
            let mut description = Vec::with_capacity(encoded.len() + 1);
            description.push(encoded.len() as u8);
            description.extend_from_slice(&encoded);

            let mut decoded = crate::huff0::huff0_decoder::HuffmanTable::new();
            let build = decoded.build_decoder(&description);
            let decoded_weights = build
                .ok()
                .and_then(|_| decoded.to_encoder_table())
                .map(|t| {
                    let mut out = Vec::new();
                    let mut writer = BitWriter::from(&mut out);
                    let encoder = HuffmanEncoder::new(&t, &mut writer);
                    encoder.weights()
                });
            let ok = decoded_weights.as_ref().is_some_and(|dw| {
                dw.len() == weights.len() + 1 && dw[..weights.len()] == weights[..]
            });
            if !ok {
                fails.push((card, skew, weights.clone()));
            }
        }
    }
    assert!(
        fails.is_empty(),
        "{} FSE weight cases still fail to round-trip after upstream-zstd early-outs; first 5: {:?}",
        fails.len(),
        &fails[..fails.len().min(5)]
    );
}

#[test]
fn large_alphabet_weight_description_uses_fse_when_raw_is_unrepresentable() {
    let mut data = Vec::new();
    for symbol in 0u8..=255 {
        data.extend(core::iter::repeat_n(symbol, usize::from(symbol) + 1));
    }
    let table = HuffmanTable::build_from_data(&data);
    let mut weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.weights()
    };
    weights.pop();
    assert!(
        weights.len() > 128,
        "fixture must require an FSE table description"
    );

    let encoded = HuffmanEncoder::<Vec<u8>>::encode_weight_description(&weights)
        .expect("FSE weight description must be available when raw weights cannot be represented");
    let mut description = Vec::with_capacity(encoded.len() + 1);
    description.push(encoded.len() as u8);
    description.extend_from_slice(&encoded);

    // The encoder no longer round-trip-verifies at runtime (it trusts the FSE
    // encoding after the upstream-zstd early-outs, matching upstream zstd); assert the
    // decodes-back property here instead.
    let mut decoded = crate::huff0::huff0_decoder::HuffmanTable::new();
    decoded
        .build_decoder(&description)
        .expect("FSE weight description must decode");
    let decoded = decoded
        .to_encoder_table()
        .expect("decoded weight table must convert to an encoder table");
    let decoded_weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&decoded, &mut writer);
        encoder.weights()
    };
    assert_eq!(decoded_weights.len(), weights.len() + 1);
    assert_eq!(&decoded_weights[..weights.len()], &weights[..]);
}

#[cfg(feature = "std")]
#[test]
fn cached_encoded_weight_description_is_reused_for_write_table() {
    let mut data = Vec::new();
    for symbol in 0u8..=255 {
        data.extend(core::iter::repeat_n(symbol, usize::from(symbol) + 1));
    }
    let table = HuffmanTable::build_from_data(&data);
    let desc_size = table
        .writeable_table_description_size()
        .expect("table description must be writable");
    let cached = table
        .cached_encoded_weight_description
        .get()
        .and_then(Option::as_ref)
        .expect("large alphabet fixture must cache FSE description")
        .clone();
    assert_eq!(desc_size, cached.len() + 1);

    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.write_table();
        writer.flush();
    }
    assert_eq!(encoded[0] as usize, cached.len());
    assert_eq!(&encoded[1..], cached.as_slice());
}

#[cfg(feature = "std")]
#[test]
fn write_table_raw_path_initializes_none_cache() {
    let table = HuffmanTable::build_from_weights(&[1, 1]);
    assert!(table.cached_encoded_weight_description.get().is_none());

    let mut expected = Vec::new();
    let weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.weights()
    };
    {
        let mut writer = BitWriter::from(&mut expected);
        HuffmanEncoder::<Vec<u8>>::write_raw_weight_description(
            &mut writer,
            &weights[..weights.len() - 1],
        );
        writer.flush();
    }

    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.write_table();
        writer.flush();
    }
    assert_eq!(encoded, expected);
    assert!(matches!(
        table.cached_encoded_weight_description.get(),
        Some(None)
    ));
}
