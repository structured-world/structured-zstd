#[test]
fn test_all_artifacts() {
    extern crate std;
    use crate::decoding::BlockDecodingStrategy;
    use crate::decoding::FrameDecoder;
    use std::borrow::ToOwned;
    use std::fs;
    use std::fs::File;

    let mut frame_dec = FrameDecoder::new();

    // The fuzz artifact corpus is locally produced by `cargo fuzz run`
    // and intentionally NOT tracked in git (see PR #148 — these files
    // are in `.gitignore` so release-plz can compute next versions
    // without a "tracked + ignored" conflict). When the directory is
    // absent — fresh checkout with no local fuzz runs, or a CI worker
    // that hasn't generated artifacts yet — the test passes as a
    // smoke check: there's nothing to replay against, and the
    // regression contract (no panic on the literal crash inputs
    // pinned in the other `#[test]` below, e.g.
    // `interop_7_byte_input_does_not_oob_in_dfast_fast_loop`) still
    // holds for the corpus inputs that DO exist in the upstream zstd crate.
    let entries = match fs::read_dir("./fuzz/artifacts/decode") {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => panic!("unexpected error reading fuzz artifacts dir: {err}"),
    };

    for file in entries {
        let file_name = file.unwrap().path();

        let fnstr = file_name.to_str().unwrap().to_owned();
        if !fnstr.contains("/crash-") {
            continue;
        }

        let mut f = File::open(file_name.clone()).unwrap();

        /* ignore errors. It just should never panic on invalid input */
        let _: Result<_, _> = frame_dec
            .reset(&mut f)
            .and_then(|()| frame_dec.decode_blocks(&mut f, BlockDecodingStrategy::All));
    }
}

/// Regression for the `interop` fuzz target: a 7-byte input crashed the
/// level 3 dfast hot loop because `start_matching_fast_loop` guarded the
/// loop with `pos + DFAST_MIN_MATCH_LEN <= current_len` (`MIN_MATCH = 5`)
/// but unconditionally issued 8-byte `u64` loads via raw-pointer
/// `read_unaligned` for the long-hash probe. On any block whose tail
/// landed within `[current_len - 8, current_len - 5]` the load read past
/// `history.len()`, which is UB on `*const u64::read_unaligned` even if
/// the underlying `Vec`'s spare capacity covers the bytes.
///
/// The fix tightens every fast-loop guard to `+ HASH_READ_SIZE = 8` so
/// the load is always in-bounds for the live history, matching upstream zstd
/// `ilimit = iend - HASH_READ_SIZE` in `zstd_double_fast.c`.
///
/// Artifact: `zstd/fuzz/artifacts/interop/crash-01be...0dc7`. Base64
/// `BGAuICAKIA==` → bytes `04 60 2e 20 20 0a 20`. CI fuzz run that
/// produced this artifact:
/// https://github.com/structured-world/structured-zstd/actions/runs/25974756307
///
/// SIGNAL CAVEAT: a plain `cargo nextest run` of this test may pass
/// against the pre-fix code because the OOB `*const u64::read_unaligned`
/// usually lands inside the live `Vec`'s spare capacity — the bytes are
/// well-defined for the allocator even though the read is UB. The
/// regression reliably fires only under a sanitizer that tracks valid
/// length (CI fuzz job runs ASan; `cargo +nightly miri test` also
/// catches it). Treat a green `cargo test` here as a smoke check, not
/// proof that the fast-loop guards are correct; the authoritative
/// signal for this fixture is the Linux fuzz CI job.
#[test]
fn interop_7_byte_input_does_not_oob_in_dfast_fast_loop() {
    use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
    use crate::encoding::{CompressionLevel, compress_to_vec};

    // Bytes inline; the original libFuzzer artifact file was
    // content-hashed (`crash-01be...0dc7`) so any future fuzz run
    // that re-discovers the same root cause via a different input
    // would land in a different filename anyway — the literal is
    // the canonical regression vector, not the artifact path.
    // Base64 `BGAuICAKIA==`.
    let data: &[u8] = &[0x04, 0x60, 0x2e, 0x20, 0x20, 0x0a, 0x20];

    // Pin to `Level(3)` rather than `Default` so this regression keeps
    // covering the dfast fast loop specifically — the original UB
    // surfaced through level 3 dfast, and pinning here means a future
    // retune of the `Default` alias cannot accidentally route this
    // test off the dfast path and let the regression pass without
    // exercising the fixed code. Pre-fix this panicked / produced a
    // garbage frame on Linux fuzz (ASan caught the UB).
    let compressed = compress_to_vec(data, CompressionLevel::Level(3));

    // Roundtrip through the in-tree decoder — matches the convention
    // used by `test_all_artifacts` above and avoids coupling this
    // regression to the upstream zstd `zstd` crate. The OOB load shows up as
    // a panic / decode error before this point under ASan; if we get
    // here with a parseable frame the bytes must match the input.
    let mut frame_dec = FrameDecoder::new();
    let mut cursor = compressed.as_slice();
    frame_dec.reset(&mut cursor).unwrap();
    frame_dec
        .decode_blocks(&mut cursor, BlockDecodingStrategy::All)
        .unwrap();
    // `expect` over `unwrap_or_default`: a real decoder failure must
    // surface as a "decoder returned None" panic, not as an empty
    // `decoded` that then fails `assert_eq!` with a misleading
    // "left: [] right: [04 60 ...]" diff that hides the real cause.
    let decoded = frame_dec.collect().expect("decoder returned no payload");
    assert_eq!(decoded.as_slice(), data);
}

#[test]
fn malformed_block_does_not_panic_via_restore_checkpoint() {
    // Regression for libFuzzer artifact crash-bfb3bc55... — a malformed
    // block whose sequence section decodes more output than the upfront
    // reserve(MAX_BLOCK_SIZE) could absorb. That forced RingBuffer::
    // reserve_amortized between checkpoint() and the post-loop bitstream
    // validity check, and the panic-on-cap-mismatch guard in
    // restore_checkpoint() then turned the malformed input into a hard
    // abort — an unintended DoS surface on untrusted bytes. Correct
    // behaviour is to surface a normal decode Err.
    extern crate std;
    use std::io::Read;

    let data: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x5d, 0x00, 0x00, 0xf7, 0x06, 0x5d, 0x00, 0x00, 0x5d, 0x00, 0x80,
        0xf7, 0xff, 0x5d, 0x00, 0x00, 0x01, 0xe0, 0xe0, 0xe0, 0xe0, 0xe2, 0xe0, 0xa4, 0x00, 0x0c,
        0x0c, 0x2c, 0x0c,
    ];

    // Pre-fix: `restore_checkpoint`'s cap-mismatch assert turned the
    // malformed block into a panic. Post-fix: frame construction
    // succeeds (the header is well-formed) and `read_to_end` surfaces
    // a normal decode `Err` once the corrupt block trips the bitstream
    // validity check. Assert both legs explicitly so a future
    // regression that lets the malformed block decode "successfully"
    // (or that breaks frame construction) cannot silently re-mask the
    // panic the way an `if let Ok(..) { let _ = ... }` shape would.
    let mut decoder = crate::decoding::StreamingDecoder::new(data)
        .expect("regression artifact must pass frame-header construction");
    let mut output = alloc::vec::Vec::new();
    assert!(
        decoder.read_to_end(&mut output).is_err(),
        "malformed block must surface a decode Err, not decode successfully"
    );
}

#[test]
fn over_producing_block_does_not_oom_via_unbounded_reserve() {
    // Regression for libFuzzer artifact oom-66db61d9… (decode target). A
    // crafted frame's sequences over-produce inside one block: every match
    // is small, but cumulatively they drove `RingBuffer::reserve_amortized`
    // to ~0.5–2 GiB *inside* the block-decode loop, before the post-block
    // validity check ran and before the consumer's output cap could
    // intervene (the OOM is upstream of any `Read::take`). A single block
    // decompresses to at most MAX_BLOCK_SIZE, so the per-block output
    // ceiling now rejects the over-producing match with a normal decode
    // Err instead of growing the buffer unbounded.
    extern crate std;
    use std::io::Read;

    let data: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x30, 0xb5, 0x00, 0x00, 0x2d, 0x28, 0xb5, 0x2f, 0xfd, 0x00,
        0x26, 0x02, 0x00, 0x04, 0x28, 0xb5, 0x2f, 0xfd, 0x34, 0x0e, 0x02, 0x00, 0x0a, 0x0a, 0x0a,
        0x0a, 0x0a, 0x0a, 0x00, 0x0b, 0x0b, 0x19, 0x00, 0x02, 0xfc, 0xe9, 0x98, 0x0a, 0x0a, 0x0a,
        0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a,
        0x0a, 0x0a, 0xd7, 0x0a, 0x0a, 0x0a, 0x0a, 0xd3, 0x4a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a,
        0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a,
        0x0a, 0x0a, 0x0a, 0x0a, 0xb5, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0xe5, 0x0a, 0xb5,
    ];

    // Frame headers are well-formed, so streaming construction must
    // succeed — make it mandatory so the test can't vacuously pass by
    // skipping the OOM path if construction ever regresses. The decode
    // must surface a normal Err (the per-block ceiling rejecting the
    // over-producing match), not just "no OOM": asserting Err also locks
    // in that the decoder never silently accepts this malformed frame.
    let mut decoder = crate::decoding::StreamingDecoder::new(data)
        .expect("regression artifact must pass frame-header construction");
    let mut output = alloc::vec::Vec::new();
    assert!(
        decoder.read_to_end(&mut output).is_err(),
        "over-producing block must surface a decode Err, not decode successfully"
    );
}

#[test]
fn multi_frame_flat_buf_path_does_not_panic() {
    // Regression for libFuzzer artifact crash-e33ba082... — a
    // multi-frame stream that hits the flat-buffer wiring landed in
    // Phase 4 of backlog item #132. The crash surfaced when one of
    // the later frames in the stream triggered a code path that
    // didn't exist on the ring backend. Correct behaviour: the
    // streaming decoder either accepts the bytes that decode and
    // surfaces a normal Err once a malformed frame is reached, or
    // returns successfully — never panics.
    extern crate std;
    use std::io::Read;

    let data: &[u8] = &[
        0x28, 0xB5, 0x2F, 0xFD, 0x28, 0x28, 0xF5, 0x00, 0x00, 0x2D, 0x27, 0x8C, 0xB4, 0xB4, 0x20,
        0xA0, 0x00, 0x02, 0x00, 0xF2, 0xF2, 0xF2, 0xF2, 0x85, 0x21, 0xF2, 0xF2, 0xF2, 0xF2, 0xF2,
        0xF2, 0xF2, 0xF2, 0xF2, 0xA8, 0xA8, 0xA8, 0xA8, 0x28, 0xB5, 0x2F, 0xFD, 0x30, 0x28, 0x2D,
        0x00, 0x00, 0x61, 0x6A, 0x10, 0x00, 0x2D, 0x00, 0xA8, 0xA8, 0xA8, 0xA8, 0xA8, 0xA8, 0xA8,
        0xF2, 0xF2, 0xF2, 0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x28, 0xB5, 0x2F, 0x00, 0x00, 0xB5, 0x28,
        0x00, 0x28, 0xFD, 0xB5, 0x00, 0x00, 0x2D, 0x0B, 0x8C, 0xB4, 0xB4, 0x04, 0x21, 0xA0, 0x00,
        0x00, 0x5E, 0xB4, 0x00, 0x00, 0x72, 0xA4, 0x00, 0xB4, 0x00, 0xFF, 0xFF, 0xFF, 0x28, 0x72,
        0xA4, 0x00, 0xB4, 0x00, 0x00, 0x72, 0x28, 0xCF, 0xA4, 0x00, 0xB4, 0xA8, 0x28, 0xB5, 0x2F,
        0xFD, 0x30, 0x00, 0x2D, 0x00, 0x00, 0x61, 0x6A, 0x10, 0x00, 0x2D, 0x00, 0xA8, 0xA8, 0xA8,
        0xA8, 0xA8, 0xA8, 0xA8, 0xF2, 0xF2, 0xF2, 0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x28, 0xB5, 0x00,
        0x00, 0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x28, 0xB5, 0x00, 0x02, 0x00, 0x2D, 0x0B, 0x02, 0x02,
        0x02, 0xFF, 0xFF, 0xF2, 0x00, 0x8C,
    ];

    // Reaching the assertion at all (no panic from `read_to_end`) is
    // the contract this test enforces — the return value can be
    // either Ok or Err depending on whether the constructed sequence
    // of frames terminates cleanly. Frame-header construction MUST
    // succeed (the bytes start with the zstd magic) so an
    // `if let Ok(..)` shape would silently turn this regression into
    // a no-op if a future change broke ctor for this artifact and
    // hid the flat-buffer panic path that the test actually targets.
    let mut decoder = crate::decoding::StreamingDecoder::new(data)
        .expect("regression artifact must pass frame-header construction");
    let mut output = alloc::vec::Vec::new();
    let _ = decoder.read_to_end(&mut output);
}
