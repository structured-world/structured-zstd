use rand::{Rng, RngExt, SeedableRng, rngs::SmallRng};
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

fn rust_size(data: &[u8], level: i32) -> usize {
    compress_to_vec(data, CompressionLevel::Level(level)).len()
}

fn c_size(data: &[u8], level: i32) -> usize {
    zstd::bulk::compress(data, level).unwrap().len()
}

fn report(name: &str, data: &[u8], level: i32) -> (usize, usize) {
    let r = rust_size(data, level);
    let c = c_size(data, level);
    let delta_bytes = r as i64 - c as i64;
    let delta_pct = (r as f64 / c as f64 - 1.0) * 100.0;
    println!(
        "  {name:<30} input={}  rust={r}  c={c}  delta={delta_bytes:+}b ({delta_pct:+.2}%)",
        data.len()
    );
    (r, c)
}

/// Probe the incompressibility detector on data that sits at the
/// boundary between "obviously random" and "obviously compressible".
/// If we emit a raw block on something C can compress, the ratio gap
/// here will be large and positive (we output much more than C).
#[test]
#[ignore = "manual probe — run with --ignored"]
fn marginal_compressibility_at_level22() {
    println!("== Level 22 marginal-compressibility ratio probe ==");

    // 1) Hard random — both should emit raw, sizes within frame
    //    overhead (a few bytes).
    let mut rng = SmallRng::seed_from_u64(0xC0FF_EE11);
    let mut hard = vec![0u8; 1024 * 1024];
    rng.fill(&mut hard[..]);
    let (r, c) = report("hard-random-1m", &hard, 22);
    assert!(
        (r as i64 - c as i64).abs() < 64,
        "hard-random output sizes should match within frame overhead"
    );

    // 2) Mixed: 75% random + 25% pattern stitched. C zstd should
    //    compress the pattern halves. If our incompressibility
    //    detector false-positives on the *block* containing pattern,
    //    we'll be much bigger than C.
    let pattern = b"coordinode:segment:0001|tenant=demo|label=orders|";
    let mut mixed = Vec::with_capacity(1024 * 1024);
    let mut rng2 = SmallRng::seed_from_u64(0xC0DE);
    while mixed.len() < 1024 * 1024 {
        // 4 KB random + 1 KB pattern in turn.
        let random_chunk_len = 4 * 1024;
        let pattern_chunk_len = 1024;
        let mut buf = vec![0u8; random_chunk_len];
        rng2.fill(&mut buf[..]);
        mixed.extend_from_slice(&buf);
        let mut written = 0;
        while written < pattern_chunk_len && mixed.len() < 1024 * 1024 {
            let take = pattern.len().min(pattern_chunk_len - written);
            mixed.extend_from_slice(&pattern[..take]);
            written += take;
        }
    }
    mixed.truncate(1024 * 1024);
    let (r_mix, c_mix) = report("mixed-rand+pattern", &mixed, 22);
    let mix_pct = (r_mix as f64 / c_mix as f64 - 1.0) * 100.0;
    println!("    => mixed gap: {mix_pct:+.2}%");

    // 3) Marginal: text with periodic structure (English-ish gibberish
    //    from a Markov-like noise source).
    let mut markov = Vec::with_capacity(1024 * 1024);
    let alphabet = b"abcdefghijklmnopqrstuvwxyz       ,.";
    let mut state = 0u32;
    let mut rng3 = SmallRng::seed_from_u64(42);
    while markov.len() < 1024 * 1024 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let mix = rng3.next_u32();
        let idx = ((state ^ mix) as usize) % alphabet.len();
        markov.push(alphabet[idx]);
    }
    let (_r_t, _c_t) = report("textish-1m", &markov, 22);

    // 4) Low entropy (highly compressible pattern) — both should hit
    //    near-zero output. Sanity check.
    let pattern_only: Vec<u8> = pattern.iter().cycle().take(1024 * 1024).copied().collect();
    let (r_p, c_p) = report("pattern-only-1m", &pattern_only, 22);
    assert!(
        r_p < 1024,
        "pattern-only must compress to <1 KiB, got {r_p}"
    );
    assert!(
        c_p < 1024,
        "pattern-only C output must compress to <1 KiB, got {c_p}"
    );

    // The acceptance signal: in the mixed scenario, if we emit raw on
    // pattern-bearing blocks we'll be many tens of KiB larger than C.
    // Tolerate up to +5% as noise from differing split decisions.
    assert!(
        mix_pct < 5.0,
        "Possible incompressibility false-positive: mixed gap {mix_pct:+.2}% (>5%) — investigate"
    );
}

/// The very FIRST block, with nothing recorded before it, still has to find a
/// repeat contained inside itself: two identical pseudorandom halves read as
/// incompressible by any sample of them, and skipping the search on that
/// verdict would send a block out raw that halves.
#[test]
fn a_block_whose_halves_repeat_is_never_written_off_as_incompressible() {
    let mut rng = SmallRng::seed_from_u64(0x5EED_1234);
    let mut half = vec![0u8; 64 * 1024];
    rng.fill(&mut half[..]);
    let mut block = half.clone();
    block.extend_from_slice(&half);

    for level in [1, 2, 3, 5] {
        let ours = rust_size(&block, level);
        assert!(
            ours < block.len() * 3 / 4,
            "level {level}: {ours} bytes from {} — the second half was not matched",
            block.len()
        );
        // Within frame overhead of what the reference manages on the same
        // input, so this cannot pass on a merely-partial match.
        let theirs = c_size(&block, level);
        assert!(
            ours as i64 - theirs as i64 <= 64,
            "level {level}: {ours} bytes against the reference's {theirs}"
        );
    }
}
