//! Coverage for the default `try_extend_from_within` impl on
//! growable backends (`FlatBuf` / `RingBuffer` use it unchanged;
//! only `UserSliceBackend` overrides it). Tests exercise the
//! three reachable arms: success, `start + len` arithmetic
//! overflow, and source-range violation. Plus the `Display` impl
//! that the decoder formats `BackendOverflow` through.
use super::*;
use crate::decoding::flat_buf::FlatBuf;

#[test]
fn default_try_extend_from_within_happy_path_copies_from_live_region() {
    // FlatBuf uses the default impl — grow on demand, no
    // capacity overshoot path on a growable backend.
    let mut b = FlatBuf::with_capacity(32);
    b.extend(&[1u8, 2, 3, 4, 5]);
    assert_eq!(b.len(), 5);
    // Copy `[1, 2, 3]` from the head into the tail.
    b.try_extend_from_within(0, 3).expect("happy path");
    assert_eq!(b.len(), 8);
    let (s, t) = b.as_slices();
    assert_eq!(s, &[1u8, 2, 3, 4, 5, 1, 2, 3]);
    assert!(t.is_empty(), "FlatBuf does not wrap");
}

#[test]
fn default_try_extend_from_within_arithmetic_overflow_returns_err() {
    // `start.checked_add(len)` wraps `usize` only on adversarial
    // inputs (`usize::MAX`-ish values). The default impl must
    // surface that as `Err(BackendOverflow)` without touching the
    // backend.
    let mut b = FlatBuf::with_capacity(32);
    b.extend(&[1u8, 2, 3, 4]);
    let live_before = b.len();
    let err = b
        .try_extend_from_within(usize::MAX, 1)
        .expect_err("usize wrap must Err");
    assert_eq!(err.requested, 1);
    assert_eq!(b.len(), live_before, "backend untouched on Err");
}

#[test]
fn default_try_extend_from_within_source_past_live_region_returns_err() {
    // `start + len > self.len()` reads from outside the live
    // region. The default impl must Err without growing or
    // writing.
    let mut b = FlatBuf::with_capacity(32);
    b.extend(&[10u8, 20, 30]);
    let err = b
        .try_extend_from_within(2, 10)
        .expect_err("start+len past live region must Err");
    assert_eq!(err.requested, 10);
    assert_eq!(b.len(), 3, "backend untouched on Err");
}

#[test]
fn backend_overflow_display_renders_diagnostic_fields() {
    let err = BackendOverflow {
        tail: 5,
        requested: 7,
        capacity: 10,
    };
    let rendered = alloc::format!("{}", err);
    assert!(rendered.contains("tail=5"), "tail field rendered");
    assert!(rendered.contains("requested=7"), "requested field rendered");
    assert!(rendered.contains("capacity=10"), "capacity field rendered");
}
