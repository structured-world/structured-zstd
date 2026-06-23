use super::*;

/// `LdmParams::adjust_for` must derive a representative
/// upstream zstd-btultra2 parameter set at window_log=27. Checked via
/// the parameter struct alone — instantiating the producer
/// at these knobs would allocate `1 << 23 = 8M` table entries
/// (~64 MiB), which slows nextest under parallelism and risks
/// OOM on 32-bit CI shards. The producer-construction smoke
/// path is exercised by every other test in this module with
/// a smaller hand-tuned `LdmParams`.
#[test]
fn producer_constructs_with_default_params() {
    let p = LdmParams::adjust_for(27, 9);
    // Upstream zstd defaults at btultra2: minMatch halved, hash_rate_log
    // = 4, bucket_size_log clamps to 8. See params::tests for
    // the per-knob derivations.
    assert_eq!(p.window_log, 27);
    assert_eq!(p.min_match_length, 32);
    assert_eq!(p.hash_rate_log, 4);
    assert_eq!(p.bucket_size_log, 8);
}

/// Compact `LdmParams` for unit-test producer construction —
/// keeps the table allocation at ~8 KiB instead of the
/// upstream zstd-btultra2 64 MiB so parallel nextest stays stable.
/// `min_match_length = 32` (half of upstream zstd floor) matches the
/// btultra2 derivation so engineered fixtures still trigger
/// long-range matches at 32-byte windows.
fn test_params() -> LdmParams {
    LdmParams {
        window_log: 27,
        hash_log: 10,
        hash_rate_log: 4,
        min_match_length: 32,
        bucket_size_log: 4,
    }
}

/// `clear` after `generate_into` rewinds the rolling hash to
/// the canonical init value — guards the frame-boundary
/// contract.
#[test]
fn clear_resets_rolling_hash_state() {
    let mut producer = LdmProducer::new(test_params());
    let mut out = Vec::new();
    // Feed a non-empty chunk so the rolling hash advances.
    let data = [0xAAu8; 256];
    producer.generate_into(&data, 0, 0, data.len(), &mut out);
    let advanced = producer.hash_state.rolling;
    assert_ne!(
        advanced,
        gear_hash::GEAR_HASH_INIT,
        "rolling hash should have moved after generate_into"
    );
    producer.clear();
    assert_eq!(
        producer.hash_state.rolling,
        gear_hash::GEAR_HASH_INIT,
        "clear must rewind to GEAR_HASH_INIT"
    );
}

/// End-to-end producer pipeline: a long-range repetition
/// (two copies of a 4 KiB random-like payload separated by
/// 64 KiB of unique filler) must emit at least one
/// [`HcRawSeq`] whose `offset` equals the distance between
/// the copies and whose `match_length` reaches at least the
/// upstream zstd `min_match_length` floor. Validates that
/// gear-hash → bucket-insert → checksum-filter → forward /
/// backward extend → emit all hold together.
#[test]
fn generate_into_emits_long_range_match_on_repeated_payload() {
    // Use level 22 / btultra2 / windowLog 27 to get the
    // halved min_match (32) — the default 64 would require a
    // 128 KiB+ fixture to comfortably trigger a split inside
    // the payload, which slows the test without adding
    // coverage.
    let mut producer = LdmProducer::new(test_params());
    let p = producer.params();
    assert_eq!(p.min_match_length, 32);

    // Build a fixture: payload, gap, payload, sized so the
    // second payload sits comfortably past the gear hash's
    // `2 ^ hash_rate_log` ≈ 16-byte expected split spacing.
    // 4 KiB payload + 64 KiB gap + 4 KiB payload ⇒ ~72 KiB
    // total, plenty of bytes for multiple split points inside
    // each copy.
    const PAYLOAD: usize = 4096;
    const GAP: usize = 64 * 1024;
    let mut history = Vec::with_capacity(2 * PAYLOAD + GAP);
    // Deterministic non-trivial payload: a simple LCG so the
    // bytes look random to the gear hash but the fixture
    // remains reproducible across runs.
    let mut prng: u32 = 0x1234_5678;
    let payload: alloc::vec::Vec<u8> = (0..PAYLOAD)
        .map(|_| {
            prng = prng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (prng >> 16) as u8
        })
        .collect();
    history.extend_from_slice(&payload);
    // Unique-byte gap: counter mod 251 keeps the gap
    // statistically distinct from the payload (251 prime so
    // it doesn't accidentally align with payload bytes).
    history.extend((0..GAP).map(|i| (i % 251) as u8));
    history.extend_from_slice(&payload);

    let mut out = Vec::new();
    producer.generate_into(&history, 0, 0, history.len(), &mut out);

    assert!(
        !out.is_empty(),
        "long-range repetition must produce at least one LDM sequence"
    );
    // Every emitted sequence must satisfy the upstream zstd floor.
    for seq in &out {
        assert!(
            seq.match_length >= p.min_match_length as usize,
            "every emitted match must reach min_match_length \
                 (got match_length = {})",
            seq.match_length
        );
    }
    // At least one emitted sequence must reach across the
    // gap into the first payload copy — this is the
    // long-range match LDM exists to capture. Short-distance
    // matches found inside the first payload (statistically
    // possible on random-like content) are allowed too, but
    // the long-range one must show up.
    let crossing_gap = out.iter().any(|s| s.offset >= GAP);
    assert!(
        crossing_gap,
        "at least one emitted sequence must have offset >= GAP \
             ({GAP}); offsets observed: {:?}",
        out.iter().map(|s| s.offset).collect::<alloc::vec::Vec<_>>()
    );
}

/// Regression test for PR #139 round-2 review (CodeRabbit +
/// Copilot, Major): the producer must store **absolute stream
/// positions** in its bucket-table entries so that long-range
/// matches accumulated by one block remain valid after a
/// window eviction shifts `history_abs_start` forward.
///
/// Setup: same 4 KiB payload appears twice in the per-frame
/// history. We invoke `generate_into` twice:
///   1. First call covers absolute range `[0, payload_end_0)`
///      — the producer's bucket table accumulates entries
///      whose `offset` fields hold absolute positions inside
///      the first payload copy.
///   2. Second call advances `history_abs_start` (simulates
///      window eviction) and covers the absolute range
///      containing the second payload copy. The bucket-table
///      entries from call 1 must remain reachable: their
///      absolute offsets still point at the SAME bytes
///      (now further left in the live slice), and the
///      inclusive lower-bound staleness check
///      `entry.offset < history_abs_start` keeps them
///      in-window (entries at exactly `history_abs_start`
///      survive).
///
/// If the producer had stored slice-relative indices
/// instead, call 2 would either miss the long-range match
/// entirely (slice indices from call 1 would point into the
/// wrong bytes after the slide) or underflow `offset =
/// split_abs − best.match_pos` and emit a corrupt
/// back-reference.
#[test]
fn generate_into_preserves_bucket_entries_across_history_slide() {
    let mut producer = LdmProducer::new(test_params());
    let p = producer.params();
    assert_eq!(p.min_match_length, 32);

    const PAYLOAD: usize = 4096;
    const GAP_A: usize = 32 * 1024;
    const GAP_B: usize = 32 * 1024;

    // Deterministic non-trivial payload.
    let mut prng: u32 = 0xC0FFEEEE;
    let payload: alloc::vec::Vec<u8> = (0..PAYLOAD)
        .map(|_| {
            prng = prng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (prng >> 16) as u8
        })
        .collect();

    // Build the full frame in one Vec — represents the
    // contiguous live history visible to the encoder before
    // any eviction.
    let mut frame = alloc::vec::Vec::with_capacity(2 * PAYLOAD + GAP_A + GAP_B);
    frame.extend_from_slice(&payload);
    frame.extend((0..GAP_A).map(|i| (i % 251) as u8));
    frame.extend_from_slice(&payload);
    frame.extend((0..GAP_B).map(|i| ((i + 17) % 241) as u8));

    // Call 1: cover the first half of the frame —
    // history_abs_start = 0, walks the first payload copy
    // and the start of the first gap. The bucket table
    // populates with absolute offsets inside payload #1.
    let mut out1 = Vec::new();
    let split_at = PAYLOAD + GAP_A;
    producer.generate_into(&frame[..split_at], 0, 0, split_at, &mut out1);

    // Call 2: simulate a window slide. The encoder retired
    // the leading `eviction` bytes from the live history;
    // the surviving slice is `frame[eviction..]` and its
    // byte 0 sits at absolute position `eviction`. The
    // second payload copy is at absolute position `PAYLOAD
    // + GAP_A`, which after the slide is at index
    // `PAYLOAD + GAP_A − eviction` inside the slice.
    let eviction = PAYLOAD / 2; // arbitrary — payload #1 partially evicted
    let live = &frame[eviction..];
    let history_abs_start = eviction;
    let block_start_abs = PAYLOAD + GAP_A; // start of payload #2
    let block_end_abs = block_start_abs + PAYLOAD;

    let mut out2 = Vec::new();
    producer.generate_into(
        live,
        history_abs_start,
        block_start_abs,
        block_end_abs,
        &mut out2,
    );

    // The long-range match must still fire — entries from
    // call 1 in the surviving tail of payload #1
    // (`absolute [eviction, PAYLOAD)`) are still in-window
    // and still point at the right bytes. If the producer
    // had stored slice-relative offsets, call 2 would
    // either miss or emit corrupt offsets.
    assert!(
        !out2.is_empty(),
        "cross-slide long-range match must survive a window eviction"
    );
    // Every emitted match must (a) clear the min_match
    // floor, and (b) point at bytes that still live inside
    // the post-slide history. The latter is the actual
    // round-2 review-fix invariant: if the producer had
    // stored slice-relative offsets, entries inserted by
    // call 1 would now reference the wrong absolute bytes
    // and `offset = split_abs − stale_match_pos` could
    // underflow `u32` and emit an offset > live_history
    // length.
    let live_len = live.len();
    for seq in &out2 {
        assert!(
            seq.match_length >= p.min_match_length as usize,
            "every emitted match must reach min_match_length (got {})",
            seq.match_length
        );
        assert!(
            seq.offset <= live_len,
            "back-ref offset {} must stay within the live \
                 history (= {} bytes); offsets larger than this \
                 are the smoking gun for stale slice-relative \
                 entries surviving the eviction",
            seq.offset,
            live_len
        );
    }

    // At least one emitted sequence MUST hit a long-range
    // back-reference (offset >= GAP_A, i.e. crossing from
    // payload #2 back into payload #1's surviving tail).
    // This is the long-range win LDM exists to capture and
    // the round-2 fix has to preserve.
    let crossed_gap = out2.iter().any(|s| s.offset >= GAP_A);
    assert!(
        crossed_gap,
        "at least one emitted sequence must hit a back-ref of \
             >= GAP_A ({GAP_A}) — that's the long-range match \
             into the surviving tail of payload #1 the bucket \
             entries from call 1 are supposed to produce; \
             offsets observed: {:?}",
        out2.iter()
            .map(|s| s.offset)
            .collect::<alloc::vec::Vec<_>>()
    );
}

/// `generate_into` with an empty range is a no-op — emits
/// nothing and leaves the rolling hash untouched. Guards
/// against an off-by-one in the bounds check.
#[test]
fn generate_into_empty_range_is_noop() {
    let mut producer = LdmProducer::new(test_params());
    let mut out = Vec::new();
    let data = [0u8; 128];
    let pre = producer.hash_state.rolling;
    producer.generate_into(&data, 0, 64, 64, &mut out);
    assert!(out.is_empty());
    assert_eq!(producer.hash_state.rolling, pre);
}
