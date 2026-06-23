use super::*;

fn build_expected_skippable(magic_variant: u8, payload: &[u8]) -> Vec<u8> {
    // Upstream zstd `ZSTD_writeSkippableFrame` (zstd v1.5.7
    // `lib/compress/zstd_compress.c:4751-4763`) emits exactly
    // `4-byte LE magic || 4-byte LE size || payload`. Mirror that
    // here as the byte-parity oracle. Re-implementing the upstream zstd
    // layout in the test (rather than calling out to zstd-sys)
    // keeps this test independent of the dev-dep wiring and
    // makes the parity expectation visible inline.
    let magic = (SKIPPABLE_MAGIC_START + u32::from(magic_variant)).to_le_bytes();
    let size = (payload.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(payload.len() + SKIPPABLE_HEADER_SIZE);
    out.extend_from_slice(&magic);
    out.extend_from_slice(&size);
    out.extend_from_slice(payload);
    out
}

#[test]
fn round_trip_all_sixteen_variants() {
    for variant in 0u8..=15 {
        let payload = alloc::vec![variant; 32 + variant as usize];
        let frame = SkippableFrame::new(variant, payload.clone()).expect("variant in range");
        let mut wire = Vec::new();
        frame
            .encode_into(&mut wire)
            .expect("encode into Vec succeeds");

        let mut cursor: &[u8] = wire.as_slice();
        let decoded = SkippableFrame::decode_from(&mut cursor).expect("round-trip decode");
        assert_eq!(decoded.magic_variant(), variant);
        assert_eq!(
            decoded.magic_number(),
            SKIPPABLE_MAGIC_START + u32::from(variant)
        );
        assert_eq!(decoded.payload(), payload.as_slice());
        assert!(
            cursor.is_empty(),
            "decode_from must consume exactly the frame bytes, no overshoot or undershoot"
        );
    }
}

#[test]
fn empty_payload_round_trips() {
    let frame = SkippableFrame::new(7, Vec::new()).expect("empty payload OK");
    assert_eq!(frame.serialized_size(), SKIPPABLE_HEADER_SIZE);

    let mut wire = Vec::new();
    frame.encode_into(&mut wire).unwrap();
    assert_eq!(wire.len(), SKIPPABLE_HEADER_SIZE);

    let mut cursor: &[u8] = wire.as_slice();
    let decoded = SkippableFrame::decode_from(&mut cursor).unwrap();
    assert!(decoded.payload().is_empty());
    assert_eq!(decoded.magic_variant(), 7);
}

#[test]
fn large_payload_round_trips() {
    // 1 MiB so the 4-byte length field carries a non-trivial
    // value (0x100000) — the byte-parity test below verifies the
    // LE serialisation explicitly.
    let payload = alloc::vec![0xABu8; 1024 * 1024];
    let frame = SkippableFrame::new(0, payload.clone()).unwrap();
    let mut wire = Vec::new();
    frame.encode_into(&mut wire).unwrap();
    assert_eq!(wire.len(), payload.len() + SKIPPABLE_HEADER_SIZE);

    let mut cursor: &[u8] = wire.as_slice();
    let decoded = SkippableFrame::decode_from(&mut cursor).unwrap();
    assert_eq!(decoded.payload().len(), payload.len());
    assert!(decoded.payload() == payload.as_slice());
}

#[test]
fn new_rejects_variant_sixteen() {
    let err = SkippableFrame::new(16, Vec::new()).expect_err("variant 16 out of range");
    match err {
        SkippableFrameError::InvalidMagicVariant(v) => assert_eq!(v, 16),
        other => panic!("expected InvalidMagicVariant(16), got {other:?}"),
    }
}

#[test]
fn new_rejects_variant_max() {
    // u8::MAX = 255 — clearly outside the spec's 0..=15 range.
    let err = SkippableFrame::new(255, Vec::new()).unwrap_err();
    match err {
        SkippableFrameError::InvalidMagicVariant(v) => assert_eq!(v, 255),
        other => panic!("expected InvalidMagicVariant(255), got {other:?}"),
    }
}

#[test]
fn write_function_rejects_invalid_variant() {
    let mut sink: Vec<u8> = Vec::new();
    let err = write_skippable_frame(16, b"x", &mut sink).unwrap_err();
    assert!(matches!(err, SkippableFrameError::InvalidMagicVariant(16)));
    assert!(
        sink.is_empty(),
        "no bytes must be written on rejected input"
    );
}

#[test]
fn byte_parity_with_spec_layout() {
    // For every variant + a handful of payload sizes, our output
    // bytes must equal the upstream zstd's `ZSTD_writeSkippableFrame`
    // layout byte-for-byte. This locks the wire-format contract
    // against future drift.
    for &payload_len in &[0usize, 1, 8, 256, 4096] {
        let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();
        for variant in 0u8..=15 {
            let expected = build_expected_skippable(variant, &payload);

            let mut via_struct = Vec::new();
            SkippableFrame::new(variant, payload.clone())
                .unwrap()
                .encode_into(&mut via_struct)
                .unwrap();
            assert_eq!(
                via_struct, expected,
                "struct encode mismatch: variant={variant} len={payload_len}"
            );

            let mut via_free = Vec::new();
            let written = write_skippable_frame(variant, &payload, &mut via_free).unwrap();
            assert_eq!(written, expected.len());
            assert_eq!(
                via_free, expected,
                "free-fn encode mismatch: variant={variant} len={payload_len}"
            );
        }
    }
}

#[test]
fn decode_rejects_non_skippable_magic() {
    // Zstd-1 magic 0xFD2FB528 is NOT in the skippable range.
    let mut wire = Vec::new();
    wire.extend_from_slice(&0xFD2F_B528u32.to_le_bytes());
    wire.extend_from_slice(&0u32.to_le_bytes());
    let mut cursor: &[u8] = wire.as_slice();
    let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
    match err {
        DecodeSkippableFrameError::BadMagicNumber(m) => assert_eq!(m, 0xFD2F_B528),
        other => panic!("expected BadMagicNumber, got {other:?}"),
    }
}

#[test]
fn decode_rejects_magic_above_band() {
    // 0x184D2A60 is one past the skippable band — must be
    // rejected via BadMagicNumber, not silently accepted as
    // variant 16.
    let mut wire = Vec::new();
    wire.extend_from_slice(&0x184D_2A60u32.to_le_bytes());
    wire.extend_from_slice(&0u32.to_le_bytes());
    let mut cursor: &[u8] = wire.as_slice();
    let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
    assert!(matches!(
        err,
        DecodeSkippableFrameError::BadMagicNumber(0x184D_2A60)
    ));
}

#[test]
fn decode_truncated_magic_surfaces_typed_error() {
    // Three bytes (one less than a magic) — must fail on the
    // magic read step, not panic.
    let wire = [0x50u8, 0x2A, 0x4D];
    let mut cursor: &[u8] = &wire;
    let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
    assert!(
        matches!(err, DecodeSkippableFrameError::Magic(_)),
        "expected Magic, got {err:?}"
    );
}

#[test]
fn decode_truncated_length_surfaces_typed_error() {
    // Magic OK, but length field is short (3 bytes instead of 4).
    let mut wire = Vec::new();
    wire.extend_from_slice(&SKIPPABLE_MAGIC_START.to_le_bytes());
    wire.extend_from_slice(&[0u8, 0, 0]);
    let mut cursor: &[u8] = wire.as_slice();
    let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
    assert!(
        matches!(err, DecodeSkippableFrameError::Length(_)),
        "expected Length, got {err:?}"
    );
}

#[test]
fn decode_truncated_payload_surfaces_typed_error() {
    // Header claims 16-byte payload but only 4 bytes follow.
    // The error must point at the PAYLOAD read step, not get
    // misreported as a header / descriptor read.
    let mut wire = Vec::new();
    wire.extend_from_slice(&SKIPPABLE_MAGIC_START.to_le_bytes());
    wire.extend_from_slice(&16u32.to_le_bytes());
    wire.extend_from_slice(&[0u8; 4]);
    let mut cursor: &[u8] = wire.as_slice();
    let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
    assert!(
        matches!(err, DecodeSkippableFrameError::Payload(_)),
        "expected Payload, got {err:?}"
    );
}

#[test]
fn serialized_size_matches_encoded_length() {
    for payload_len in [0usize, 1, 7, 8, 9, 255, 256, 1023, 1024] {
        let payload = alloc::vec![0u8; payload_len];
        let frame = SkippableFrame::new(3, payload).unwrap();
        let mut wire = Vec::new();
        frame.encode_into(&mut wire).unwrap();
        assert_eq!(
            wire.len(),
            frame.serialized_size(),
            "serialized_size() must match actual encode length for payload_len={payload_len}"
        );
    }
}

#[test]
fn decode_huge_length_returns_typed_error_not_oom_abort() {
    // Crafted wire declares a u32::MAX payload but provides
    // zero payload bytes. The decoder must surface a typed
    // error rather than aborting the process or panicking.
    // Three paths are acceptable, each gated by the host's
    // ABI / allocator behaviour:
    //
    // - `PayloadTooLarge { length }` — 32-bit host, where
    //   `length + 8` overflows `usize`. The decoder rejects
    //   the length before allocating.
    // - `AllocationFailed { requested }` — 64-bit host, no
    //   memory overcommit (Windows / configured Linux):
    //   `try_reserve_exact` reports failure.
    // - `Payload(io_err)` — 64-bit host, memory overcommit
    //   (Linux default / macOS): allocation succeeds for the
    //   address range, chunked read on truncated stream
    //   surfaces the I/O error after committing one page
    //   for the scratch buffer.
    //
    // What it must NOT do: abort the process on OOM or panic
    // via Vec::with_capacity / Vec::resize.
    let huge: u32 = u32::MAX;
    let mut wire = Vec::new();
    wire.extend_from_slice(&SKIPPABLE_MAGIC_START.to_le_bytes());
    wire.extend_from_slice(&huge.to_le_bytes());
    let mut cursor: &[u8] = wire.as_slice();
    let err = SkippableFrame::decode_from(&mut cursor).unwrap_err();
    match err {
        DecodeSkippableFrameError::PayloadTooLarge { length } => {
            assert_eq!(length, huge);
        }
        DecodeSkippableFrameError::AllocationFailed { requested } => {
            assert_eq!(requested, huge as usize);
        }
        DecodeSkippableFrameError::Payload(_) => {
            // Chunked read on the truncated payload surfaced
            // the I/O error after the OS overcommitted the
            // address range. Also acceptable.
        }
        other => panic!("expected PayloadTooLarge / AllocationFailed / Payload, got {other:?}"),
    }
}

#[test]
fn payload_too_large_check_branches_on_pointer_width() {
    // The `validate_payload_size` invariant is twofold:
    //
    // 1. `len > u32::MAX` is rejected on every target (the
    //    on-wire length field is u32).
    // 2. `len + SKIPPABLE_HEADER_SIZE` overflowing `usize` is
    //    rejected on every target. On 64-bit this is
    //    unreachable because `u32::MAX + 8 < usize::MAX`. On
    //    32-bit `len == u32::MAX` itself trips condition 2:
    //    `u32::MAX + 8` wraps `usize`.
    //
    // Branch the boundary expectation on pointer width so the
    // test passes on both i686 (CI cross-i686 shard) and
    // x86_64 hosts.
    #[cfg(target_pointer_width = "64")]
    {
        let result = validate_payload_size(u32::MAX as usize + 1);
        assert!(matches!(
            result,
            Err(SkippableFrameError::PayloadTooLarge(_))
        ));
        let ok = validate_payload_size(u32::MAX as usize);
        assert!(ok.is_ok(), "u32::MAX representable on 64-bit");
    }

    #[cfg(target_pointer_width = "32")]
    {
        // `u32::MAX + 1` literally cannot be expressed as
        // `usize` on 32-bit — `u32::MAX as usize + 1` wraps
        // to 0. So construct the test only through values
        // that are validly representable.
        let result = validate_payload_size(u32::MAX as usize);
        assert!(
            matches!(result, Err(SkippableFrameError::PayloadTooLarge(_))),
            "u32::MAX overflows when combined with the 8-byte header on 32-bit"
        );
        let ok = validate_payload_size((u32::MAX as usize) - SKIPPABLE_HEADER_SIZE);
        assert!(ok.is_ok(), "below the header-overflow boundary on 32-bit");
    }
}
