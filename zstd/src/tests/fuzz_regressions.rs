#[test]
fn test_all_artifacts() {
    extern crate std;
    use crate::decoding::BlockDecodingStrategy;
    use crate::decoding::FrameDecoder;
    use std::borrow::ToOwned;
    use std::fs;
    use std::fs::File;

    let mut frame_dec = FrameDecoder::new();

    for file in fs::read_dir("./fuzz/artifacts/decode").unwrap() {
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
/// the load is always in-bounds for the live history, matching donor
/// `ilimit = iend - HASH_READ_SIZE` in `zstd_double_fast.c`.
///
/// Artifact: `zstd/fuzz/artifacts/interop/crash-01be...0dc7`. Base64
/// `BGAuICAKIA==` → bytes `04 60 2e 20 20 0a 20`. CI fuzz run that
/// produced this artifact:
/// https://github.com/structured-world/structured-zstd/actions/runs/25974756307
#[test]
fn interop_7_byte_input_does_not_oob_in_dfast_fast_loop() {
    extern crate std;
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
    // regression to the donor `zstd` crate. The OOB load shows up as
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
