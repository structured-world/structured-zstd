use super::{DecodeBuffer, RingBuffer};
use crate::decoding::buffer_backend::BufferBackend;
use crate::io::{Error, ErrorKind, Write};

extern crate std;
use alloc::vec;
use alloc::vec::Vec;

#[test]
fn dict_offsets_rejected_after_direct_path_window_drop() {
    // The direct decode path writes through the inline executors
    // (which skip `total_output_counter`) and bounds the visible
    // buffer with `drop_to_window_size()` between blocks. Once
    // cumulative output exceeds the window, dictionary-backed
    // offsets are out of reach per the spec; the reachability gate
    // must not reopen just because the visible length was capped
    // back to `window_size`.
    use crate::decoding::dictionary::Dictionary;
    use crate::decoding::user_slice_buf::UserSliceBackend;

    let mut out = vec![0u8; 300];
    let backend = UserSliceBackend::from_slice(out.as_mut_slice());
    let mut buf = DecodeBuffer::from_backend(backend, 100);
    let dict = Dictionary::from_raw_content(7, vec![0xAB; 64]).expect("raw-content dictionary");
    let handle = dict.into_handle();

    // Mimic the inline executor: produce 250 bytes without touching
    // `total_output_counter`, exceeding the 100-byte window.
    BufferBackend::extend(&mut buf.buffer, &[1u8; 250]);
    buf.drop_to_window_size();
    assert_eq!(buf.len(), 100, "visible buffer capped to the window");

    // offset 110 > len 100 reaches 10 bytes into the dictionary;
    // cumulative output (250) already exceeds the window (100), so
    // this must be rejected, not served from the dictionary.
    let result = buf.repeat_from_dict(Some(handle.as_dict()), 110, 5);
    assert!(
        result.is_err(),
        "dict-backed offset must be unreachable once output exceeded the window, got {result:?}"
    );
}

#[test]
fn from_backend_clears_prepopulated_backend() {
    // Regression for the round-8 review fix: `from_backend` must
    // normalise a caller-supplied backend so the logical counters
    // (total_output_counter=0, dict_content=empty) stay consistent
    // with the physical buffer contents. A future caller that
    // wires up a non-fresh backend should not silently leak stale
    // bytes into the new decode.
    let mut backend = RingBuffer::new();
    BufferBackend::extend(&mut backend, b"stale");
    assert!(BufferBackend::len(&backend) > 0);

    let mut buf = DecodeBuffer::<RingBuffer>::from_backend(backend, 1024);
    assert_eq!(buf.len(), 0, "from_backend must clear pre-populated bytes");

    buf.push(b"ok");
    assert_eq!(buf.drain(), b"ok");
}

#[test]
fn test_repeat_doubling_matches_reference_across_offsets() {
    // Naive reference: dst[i] = dst[i - offset]. The exponential-doubling
    // `repeat_in_chunks` must produce byte-identical output for every
    // offset/length, including lengths that straddle the 32-byte SIMD
    // overshoot boundary and large lengths that force many doublings.
    fn reference_repeat(prefix: &[u8], offset: usize, match_length: usize) -> Vec<u8> {
        let mut out = prefix.to_vec();
        for _ in 0..match_length {
            let src = out.len() - offset;
            out.push(out[src]);
        }
        out
    }

    let prefix: Vec<u8> = (0..200u32)
        .map(|i| i.wrapping_mul(31).wrapping_add(7) as u8)
        .collect();
    // Offsets cover every `repeat_overlapping` arm: <8 period-tiled,
    // 8..15 and >=16 doubling, exact powers of two, and the
    // non-overlapping `offset == match_length` edge.
    let offsets = [1usize, 2, 3, 5, 7, 8, 13, 16, 31, 32, 63, 64, 100, 200];
    let lengths = [
        1usize, 2, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 200, 511, 1000, 5000,
    ];
    for &offset in &offsets {
        for &match_length in &lengths {
            let prefix_slice = &prefix[..offset.max(1)];
            let mut buffer = DecodeBuffer::<RingBuffer>::new(usize::MAX);
            buffer.push(prefix_slice);
            buffer.repeat(None, offset, match_length).unwrap();
            let expected = reference_repeat(prefix_slice, offset, match_length);
            let mut got: Vec<u8> = Vec::new();
            buffer.drain_to_writer(&mut got).unwrap();
            assert_eq!(
                got.as_slice(),
                expected.as_slice(),
                "mismatch at offset={offset} match_length={match_length}",
            );
        }
    }
}

#[test]
fn checkpoint_restore_undoes_pushes() {
    // Regression test for the fused-decode transactional contract:
    // when the post-loop bitstream validation fails, the fused
    // sequence executor must restore buffer state to the moment
    // before the first per-iter side-effect. This exercises the
    // primitive that supports that rollback.
    let mut buf = DecodeBuffer::<RingBuffer>::new(1024);
    // Mirror the fused sequence executor: reserve upfront so no
    // RingBuffer reallocation happens between checkpoint and restore
    // (restore_checkpoint requires a stable underlying allocation).
    buf.reserve_exact(64);
    buf.push(&[1, 2, 3]);
    let cp = buf.checkpoint();
    buf.push(&[4, 5, 6, 7]);
    assert_eq!(buf.len(), 7);
    assert!(
        buf.try_restore_checkpoint(cp),
        "no realloc → restore must succeed"
    );
    assert_eq!(buf.len(), 3, "len must reflect the checkpoint");

    // After restore, fresh writes must land contiguously where the
    // first push left off (no stale tail bytes leaking through).
    buf.push(&[0xAA, 0xBB]);
    assert_eq!(buf.len(), 5);
    // Drain & verify content.
    let mut drained: Vec<u8> = Vec::new();
    buf.drain_to_writer(&mut drained).unwrap();
    assert_eq!(drained, alloc::vec![1, 2, 3, 0xAA, 0xBB]);
}

#[test]
fn restore_checkpoint_after_realloc_returns_false() {
    // Regression test: try_restore_checkpoint() must detect an
    // intervening RingBuffer reallocation (which compacts the data
    // layout and invalidates the captured tail) and refuse to
    // restore, returning false instead of corrupting state or
    // panicking. Triggered by a malformed zstd block whose sequence
    // section decodes past MAX_BLOCK_SIZE; surfacing the failure to
    // the caller as a normal decode Err is required behaviour —
    // both silent wrong output AND an unconditional panic on
    // untrusted input are unacceptable. libFuzzer artifact
    // crash-bfb3bc55... originally exercised this branch via the
    // panic guard added in the previous round.
    let mut buf = DecodeBuffer::<RingBuffer>::new(64);
    buf.push(&[0; 16]);
    let cp = buf.checkpoint();
    // Force a reallocation. RingBuffer grows by powers of two and
    // 4 MiB is well above the initial 64-byte starting capacity, so
    // reserve() must hit reserve_amortized().
    buf.reserve_exact(4 * 1024 * 1024);
    buf.push(&[0; 16]);
    assert!(
        !buf.try_restore_checkpoint(cp),
        "realloc happened → rollback must be refused"
    );
    // No state mutation when the restore is refused.
    assert_eq!(buf.len(), 32);
}

#[test]
fn short_writer() {
    struct ShortWriter {
        buf: Vec<u8>,
        write_len: usize,
    }

    impl Write for ShortWriter {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Error> {
            if buf.len() > self.write_len {
                self.buf.extend_from_slice(&buf[..self.write_len]);
                Ok(self.write_len)
            } else {
                self.buf.extend_from_slice(buf);
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> std::result::Result<(), Error> {
            Ok(())
        }
    }

    let mut short_writer = ShortWriter {
        buf: vec![],
        write_len: 10,
    };

    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(100);
    decode_buf.push(b"0123456789");
    decode_buf.repeat(None, 10, 90).unwrap();
    let repeats = 1000;
    for _ in 0..repeats {
        assert_eq!(decode_buf.len(), 100);
        decode_buf.repeat(None, 10, 50).unwrap();
        assert_eq!(decode_buf.len(), 150);
        decode_buf
            .drain_to_window_size_writer(&mut short_writer)
            .unwrap();
        assert_eq!(decode_buf.len(), 100);
    }

    assert_eq!(short_writer.buf.len(), repeats * 50);
    decode_buf.drain_to_writer(&mut short_writer).unwrap();
    assert_eq!(short_writer.buf.len(), repeats * 50 + 100);
}

#[test]
fn wouldblock_writer() {
    struct WouldblockWriter {
        buf: Vec<u8>,
        last_blocked: usize,
        block_every: usize,
    }

    impl Write for WouldblockWriter {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Error> {
            if self.last_blocked < self.block_every {
                self.buf.extend_from_slice(buf);
                self.last_blocked += 1;
                Ok(buf.len())
            } else {
                self.last_blocked = 0;
                Err(Error::from(ErrorKind::WouldBlock))
            }
        }

        fn flush(&mut self) -> std::result::Result<(), Error> {
            Ok(())
        }
    }

    let mut short_writer = WouldblockWriter {
        buf: vec![],
        last_blocked: 0,
        block_every: 5,
    };

    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(100);
    decode_buf.push(b"0123456789");
    decode_buf.repeat(None, 10, 90).unwrap();
    let repeats = 1000;
    for _ in 0..repeats {
        assert_eq!(decode_buf.len(), 100);
        decode_buf.repeat(None, 10, 50).unwrap();
        assert_eq!(decode_buf.len(), 150);
        loop {
            match decode_buf.drain_to_window_size_writer(&mut short_writer) {
                Ok(written) => {
                    if written == 0 {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        continue;
                    } else {
                        panic!("Unexpected error {:?}", e);
                    }
                }
            }
        }
        assert_eq!(decode_buf.len(), 100);
    }

    assert_eq!(short_writer.buf.len(), repeats * 50);
    loop {
        match decode_buf.drain_to_writer(&mut short_writer) {
            Ok(written) => {
                if written == 0 {
                    break;
                }
            }
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    continue;
                } else {
                    panic!("Unexpected error {:?}", e);
                }
            }
        }
    }
    assert_eq!(short_writer.buf.len(), repeats * 50 + 100);
}

#[test]
fn repeat_overlap_fast_paths_match_reference_behavior() {
    let seed = b"0123456789abcdef0123456789abcdef";
    let cases = [
        (16usize, 16usize), // non-overlapping boundary
        (16usize, 211usize),
        (8usize, 173usize),
        (7usize, 149usize),
        (3usize, 160usize),
        (1usize, 255usize),
    ];

    for (offset, match_len) in cases {
        let mut decode_buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        decode_buf.push(seed);
        decode_buf.repeat(None, offset, match_len).unwrap();
        let got = decode_buf.drain();
        let expected = expected_match_expansion(seed, offset, match_len);
        assert_eq!(got, expected, "offset={offset}, match_len={match_len}");
    }
}

#[test]
fn repeat_zero_offset_returns_error() {
    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(1024);
    decode_buf.push(b"abcdef");
    let err = decode_buf.repeat(None, 0, 5).unwrap_err();
    assert!(matches!(
        err,
        crate::decoding::errors::DecodeBufferError::ZeroOffset
    ));
}

#[test]
fn repeat_rejects_output_past_block_ceiling() {
    // A single zstd block decompresses to at most MAX_BLOCK_SIZE. The
    // per-block ceiling is enforced on the growable `RingBuffer`'s cold
    // growth path: `set_block_output_ceiling` lowers `max_capacity`, so a
    // `repeat` whose match would have to grow the buffer past the ceiling
    // fails its `try_reserve` instead of growing the ring unbounded.
    // Without this guard the over-long match drove `try_reserve` to
    // ~0.5–2 GiB across many over-producing sequences (artifact
    // `oom-66db61d9…`, fuzz `decode` target) before any post-block check
    // ran — a decompression-bomb OOM. The match needing growth surfaces
    // as `OutputBufferOverflow` (the backend's structured reject).
    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
    decode_buf.push(b"abcdef"); // len = 6
    decode_buf.set_block_output_ceiling(8); // max_capacity = 6 + 8 = 14
    let err = decode_buf.repeat(None, 4, 16).unwrap_err(); // 6 + 16 = 22 > 14
    assert!(
        matches!(
            err,
            crate::decoding::errors::DecodeBufferError::OutputBufferOverflow { .. }
        ),
        "over-producing match must be rejected, got {err:?}"
    );
}

#[test]
fn repeat_within_block_ceiling_still_succeeds() {
    // A match that keeps the block's output at/below the armed ceiling
    // must NOT be rejected — the guard fires only on growth past the
    // ceiling, never on legitimate in-bounds output.
    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
    decode_buf.push(b"abcdef"); // len = 6
    decode_buf.set_block_output_ceiling(8); // max_capacity = 14
    decode_buf
        .repeat(None, 4, 8)
        .expect("6 + 8 = 14 == ceiling is allowed");
    assert_eq!(decode_buf.len(), 14);
}

#[test]
fn repeat_from_dict_rejects_output_past_block_ceiling() {
    // A match satisfied (fully or partially) from the dictionary appends
    // `dict_slice` directly via `buffer.extend`, bypassing the inline
    // push/repeat guard. The per-block bomb ceiling must still bound it, so
    // a dict-backed over-producing match returns `OutputBufferOverflow`
    // instead of growing the buffer toward OOM.
    let dict = || {
        crate::decoding::dictionary::DictionaryHandle::from_dictionary(
            crate::decoding::dictionary::Dictionary::from_raw_content(1, alloc::vec![0xABu8; 256])
                .unwrap(),
        )
    };

    // Fully-dictionary match: empty buffer, offset reaches into the dict.
    let mut full = DecodeBuffer::<RingBuffer>::new(4 * 1024);
    let full_dict = dict();
    full.set_block_output_ceiling(8); // max_capacity = 0 + 8 = 8
    let err = full
        .repeat(Some(full_dict.as_dict()), 200, 100)
        .unwrap_err(); // 0 + 100 > 8, all from dict
    assert!(
        matches!(
            err,
            crate::decoding::errors::DecodeBufferError::OutputBufferOverflow { .. }
        ),
        "fully-dictionary over-producing match must be rejected, got {err:?}"
    );

    // Mixed match: part from dict, remainder from buffer history.
    let mut mixed = DecodeBuffer::<RingBuffer>::new(4 * 1024);
    let mixed_dict = dict();
    mixed.push(b"abcd"); // len = 4
    mixed.set_block_output_ceiling(8); // max_capacity = 4 + 8 = 12
    let err = mixed
        .repeat(Some(mixed_dict.as_dict()), 10, 100)
        .unwrap_err(); // 6 from dict + rest, 4 + 100 > 12
    assert!(
        matches!(
            err,
            crate::decoding::errors::DecodeBufferError::OutputBufferOverflow { .. }
        ),
        "mixed dict+buffer over-producing match must be rejected, got {err:?}"
    );
}

#[test]
fn repeat_from_dict_full_copy_updates_total_output_counter() {
    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(1);
    let handle = crate::decoding::dictionary::DictionaryHandle::from_dictionary(
        crate::decoding::dictionary::Dictionary::from_raw_content(1, b"0123456789".to_vec())
            .unwrap(),
    );

    decode_buf.repeat(Some(handle.as_dict()), 10, 2).unwrap();
    let err = decode_buf
        .repeat(Some(handle.as_dict()), 10, 1)
        .unwrap_err();
    assert!(matches!(
        err,
        crate::decoding::errors::DecodeBufferError::OffsetTooBig { .. }
    ));
}

#[test]
fn repeat_overlap_fast_paths_match_reference_behavior_with_wrapped_ringbuffer() {
    let window = 32usize;
    let seed = b"0123456789abcdef0123456789abcdef";
    let mut decode_buf = DecodeBuffer::<RingBuffer>::new(window);
    let mut model = Vec::new();

    decode_buf.push(seed);
    model_push(&mut model, seed);
    decode_buf.repeat(None, 16, 16).unwrap();
    model_repeat(&mut model, 16, 16);

    let drained = decode_buf.drain_to_window_size().unwrap();
    let model_drained = model_drain_to_window(&mut model, window);
    assert_eq!(drained, model_drained);

    let cases = [(3usize, 97usize), (16usize, 64usize), (7usize, 73usize)];
    for (offset, match_len) in cases {
        decode_buf.repeat(None, offset, match_len).unwrap();
        model_repeat(&mut model, offset, match_len);

        if let Some(got) = decode_buf.drain_to_window_size() {
            let expected = model_drain_to_window(&mut model, window);
            assert_eq!(got, expected, "offset={offset}, match_len={match_len}");
        }
    }

    assert_eq!(decode_buf.drain(), model);
}

fn expected_match_expansion(seed: &[u8], offset: usize, match_len: usize) -> Vec<u8> {
    let mut out = seed.to_vec();
    let start = out.len() - offset;
    for i in 0..match_len {
        let byte = out[start + i];
        out.push(byte);
    }
    out
}

fn model_push(model: &mut Vec<u8>, bytes: &[u8]) {
    model.extend_from_slice(bytes);
}

fn model_repeat(model: &mut Vec<u8>, offset: usize, match_len: usize) {
    let start = model.len() - offset;
    for i in 0..match_len {
        let byte = model[start + i];
        model.push(byte);
    }
}

fn model_drain_to_window(model: &mut Vec<u8>, window: usize) -> Vec<u8> {
    if model.len() <= window {
        return Vec::new();
    }
    let drain_len = model.len() - window;
    model.drain(0..drain_len).collect()
}

/// Drive `DecodeBuffer::repeat` through the short-offset path and
/// compare against the canonical `output[i] = base[i % offset]`
/// reference, covering offsets that hit both the SIMD-16 fast path
/// (1, 2, 4) and the 8-byte phase-pattern path (3, 5, 6, 7).
///
/// Regression guard for the SIMD-16 specialisation: when `period
/// divides 16` (offset ∈ {1,2,4}), the inner loop emits 16-byte
/// chunks via a pre-built `[u8; 16]` instead of 8-byte phase
/// patterns. Tail lengths span both `match_length % 16 == 0` and
/// non-zero remainders so the tail-extend codepath is also
/// exercised.
#[test]
fn repeat_short_offset_matches_canonical_for_all_offsets_and_lengths() {
    for offset in 1usize..=7 {
        let mut base = [0u8; 7];
        for (i, slot) in base.iter_mut().enumerate().take(offset) {
            *slot = b'A' + (i as u8);
        }
        for &match_length in &[
            1usize, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 23, 24, 25, 31, 32, 33, 47, 48, 49, 64,
            127, 128, 4096,
        ] {
            let mut buf = DecodeBuffer::<RingBuffer>::new(8192);
            buf.push(&base[..offset]);
            buf.repeat(None, offset, match_length).unwrap_or_else(|e| {
                panic!("repeat failed for offset={offset} match_length={match_length}: {e:?}")
            });

            let actual = buf.drain();
            let mut expected = Vec::with_capacity(offset + match_length);
            expected.extend_from_slice(&base[..offset]);
            for i in 0..match_length {
                expected.push(base[i % offset]);
            }
            assert_eq!(
                actual, expected,
                "mismatch at offset={offset} match_length={match_length}",
            );
        }
    }
}

#[test]
fn prefetch_lookahead_in_range_does_not_panic() {
    // Plain in-range lookup: start_idx well within `buffer.len()`.
    // The helper should issue prefetch hints and return cleanly.
    // Prefetch hints are unobservable from Rust — the assertion is
    // simply that the call completes without panic / UB.
    let mut buf = DecodeBuffer::<RingBuffer>::new(1024);
    buf.reserve_exact(512);
    buf.push(&[0xAA; 256]);
    buf.prefetch_lookahead_match_source(0);
    buf.prefetch_lookahead_match_source(128);
    buf.prefetch_lookahead_match_source(buf.len() - 1);
}

#[test]
fn prefetch_lookahead_out_of_range_returns_without_panic() {
    // Wrap-derived garbage / dictionary-sourced match / intra-block
    // self-overlap all produce `start_idx >= buffer.len()` here.
    // The helper must early-return (bound check) and never touch a
    // slice past the live region.
    let mut buf = DecodeBuffer::<RingBuffer>::new(1024);
    buf.reserve_exact(64);
    buf.push(&[0x55; 32]);
    buf.prefetch_lookahead_match_source(buf.len());
    buf.prefetch_lookahead_match_source(buf.len() + 1);
    buf.prefetch_lookahead_match_source(usize::MAX);
    // Empty buffer — every start_idx is out-of-range.
    let empty: DecodeBuffer<RingBuffer> = DecodeBuffer::new(1024);
    empty.prefetch_lookahead_match_source(0);
    empty.prefetch_lookahead_match_source(7);
}

#[test]
fn prefetch_lookahead_at_wrap_boundary() {
    // Force the RingBuffer into a wrapped layout where
    // `as_slices()` returns two non-empty halves: push, drain past
    // window, push again so the write cursor wraps. Then exercise
    // start_idx values at the boundary (last byte of s1, first
    // byte of s2, short s1 tail < CACHE_LINE) so the
    // `prefetch_first_line_l1` fallback path is touched too.
    let mut buf = DecodeBuffer::<RingBuffer>::new(256);
    // Fill with two passes so the underlying ringbuffer wraps.
    let payload = [0xCD_u8; 320];
    buf.push(&payload);
    // Drain to free read cursor capacity (write side can then wrap).
    let _ = buf.drain_to_window_size();
    buf.push(&payload);
    // Probe a handful of indices inside and across the wrap.
    let n = buf.len();
    if n > 0 {
        buf.prefetch_lookahead_match_source(0);
        buf.prefetch_lookahead_match_source(n / 2);
        buf.prefetch_lookahead_match_source(n - 1);
        // Out-of-range probe to exercise the early-return path on
        // a wrapped buffer.
        buf.prefetch_lookahead_match_source(n);
    }
}
