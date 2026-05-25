//! `BufferBackend` — the compile-time-dispatched interface for the
//! decoder's output storage.
//!
//! Two concrete impls live alongside this module:
//! [`super::ringbuffer::RingBuffer`] (full wrap-aware semantics, default)
//! and [`super::flat_buf::FlatBuf`] (no-wrap fast path used when the
//! frame header's `Single_Segment_flag` guarantees the decompressed
//! output never exceeds `window_size` and so never wraps).
//!
//! Selection happens through the generic parameter on
//! [`super::decode_buffer::DecodeBuffer<B>`] and cascades through
//! `DecoderScratch<B>` to the block-level decode functions. The
//! compiler monomorphises each backend independently and erases the
//! wrap-checking code path entirely on the flat side — see backlog
//! item #132. An earlier attempt with a runtime `enum BufferStorage`
//! paid match-dispatch overhead in every push/repeat and measured a
//! +43–58 % regression on small-frame decompress benchmarks, so the
//! compile-time generic shape is load-bearing.

use crate::io::{Error, Read};

/// Trailing-slack count both backends pad their physical allocation
/// with so SIMD wildcopy reads / writes can overshoot the live region
/// without leaving the allocation. Sized at **32 bytes** so the AVX2
/// chunked kernel in `simd_copy::copy_bytes_overshooting` (32-byte
/// stride via `_mm256_storeu_si256` on x86-64) can fire on tail copies.
/// The kernel gates on `min_buffer_size >= rounded(copy_at_least, 32)`;
/// at the end of a fixed-capacity output buffer that gate fails when
/// slack is < 32, and the dispatch falls through to whatever
/// `ptr::copy_nonoverlapping` lowers to on the target — a
/// platform-specific `memcpy`-like primitive (the source/dest regions
/// are non-overlapping by the caller's contract, so memcpy semantics
/// apply; the exact symbol the linker resolves is libc-specific and
/// not part of any guaranteed contract). Bumping slack from 16 → 32
/// keeps the AVX2 path live across every match-copy and literal-push,
/// avoiding the libc detour.
///
/// Both `RingBuffer` and `FlatBuf` reuse this single constant so the
/// slack contract cannot drift between backends.
pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;

/// Storage operations the decoder needs from its output buffer.
///
/// The trait surface mirrors the historical `RingBuffer` API the
/// `DecodeBuffer` consumed before the generic split — every method's
/// semantics match what `RingBuffer` already provides; `FlatBuf`'s
/// impl is the no-wrap shape of the same contract.
pub(crate) trait BufferBackend: Sized {
    /// Construct an empty backend. Backend-specific sizing is done
    /// via `with_capacity` constructors on the concrete types (see
    /// [`super::flat_buf::FlatBuf::with_capacity`]).
    fn new() -> Self;

    /// Empty the buffer; reset internal cursors to 0.
    fn clear(&mut self);

    /// Reserve at least `n` bytes of additional writable capacity.
    /// May or may not allocate depending on current free space.
    fn reserve(&mut self, n: usize);

    /// Live byte count: bytes between the logical head and tail.
    fn len(&self) -> usize;

    /// Realloc-detection sentinel for
    /// [`super::decode_buffer::DecodeBufferCheckpoint`]. The exact
    /// value is backend-specific (RingBuffer returns its ring-
    /// indexing capacity, which does not include the trailing
    /// [`WILDCOPY_OVERLENGTH`] slack bytes; FlatBuf returns the
    /// full `Vec::capacity` which does include them). The contract
    /// the checkpoint relies on is invariant per-instance: `cap()`
    /// stays equal across calls as long as no reallocation has
    /// happened. Equality is the only operation the checkpoint
    /// performs — the absolute value is never compared across
    /// backends.
    fn cap(&self) -> usize;

    /// Physical write cursor — paired with [`Self::set_tail`] for the
    /// rollback primitive.
    fn tail(&self) -> usize;

    /// Restore the write cursor to a previously captured `tail()`.
    ///
    /// # Safety
    /// - `new_tail` was returned by an earlier `tail()` on this same
    ///   instance.
    /// - `cap()` has not changed since (the caller validates this via
    ///   the checkpoint's `cap` snapshot — both backends would
    ///   silently corrupt their live region otherwise).
    /// - Bytes between `new_tail` and the current tail are discarded
    ///   by the caller and never read again.
    unsafe fn set_tail(&mut self, new_tail: usize);

    /// Append `data` to the tail.
    fn extend(&mut self, data: &[u8]);

    /// Append `fill_length` copies of `fill_with` to the tail.
    /// Backs the RLE block path.
    fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize);

    /// Read exactly `fill_length` bytes from `read` directly into the
    /// tail. Backs the Raw block path.
    fn extend_from_reader<R: Read>(&mut self, read: R, fill_length: usize) -> Result<(), Error>;

    /// Copy `len` bytes from logical position `start` (relative to
    /// the live region's head) to the tail. Non-overlapping case.
    ///
    /// # Safety
    /// - `start + len <= self.len()`.
    /// - Capacity for `len` additional bytes past the current tail
    ///   was reserved by the caller.
    unsafe fn extend_from_within_unchecked(&mut self, start: usize, len: usize);

    /// Branchless variant used on x86 builds where the unchecked
    /// non-overlap precondition allows the chunked wildcopy to skip
    /// the per-iteration overlap check. On backends where the
    /// distinction has no perf delta this simply forwards to
    /// `extend_from_within_unchecked`.
    ///
    /// # Safety
    /// Same as [`Self::extend_from_within_unchecked`].
    unsafe fn extend_from_within_unchecked_branchless(&mut self, start: usize, len: usize);

    /// Two-slice view of the live region. The second slice is empty
    /// on backends that don't wrap (flat path) — the API shape is
    /// preserved so drain code is shared between backends.
    fn as_slices(&self) -> (&[u8], &[u8]);

    /// Advance the head past `n` bytes — they are removed from the
    /// live window but may still be physically present (backing
    /// future match copies). Mirrors the historical
    /// `RingBuffer::drop_first_n` contract.
    fn drop_first_n(&mut self, n: usize);

    // ── Fallible write surface (DoS-safe direct decode path) ──
    //
    // Parallel `try_*` methods that return `Err(BackendOverflow)`
    // instead of panicking when the write would exceed the backend's
    // capacity. Currently wired on Raw and RLE block paths only;
    // Compressed-block sequence execution still uses the panic-on-
    // overflow unchecked writes and will be migrated in a follow-up.
    // Used by the direct-decode path (`decode_to_slice_trusted` +
    // descendants) so a malformed Raw/RLE block whose declared
    // decompressed payload exceeds the caller-provided output slice
    // surfaces as a structured `FrameDecoderError::FrameContentSizeMismatch`
    // instead of an abort.
    //
    // The growable backends (`FlatBuf`, `RingBuffer`) rely on the
    // default impls below — they delegate to the corresponding
    // panic-on-overflow method (`extend`, `extend_and_fill`,
    // `extend_from_within_unchecked`) and always return `Ok(())`.
    // Those underlying methods grow the backing `Vec` on demand, so
    // there is no capacity-mismatch case to surface as `Err`. No
    // per-backend `try_*` impl exists on `FlatBuf` / `RingBuffer`
    // because the default behaviour is exactly what they want.
    //
    // The fixed-capacity backend (`UserSliceBackend`) overrides each
    // method with an explicit capacity check that returns `Err` on
    // overshoot instead of panicking. The trade-off is one branch
    // per write on the direct-decode path; the overhead is expected
    // to be modest but has not yet been benchmarked on this branch
    // (bench validation tracked as a follow-up before merging into
    // the perf-critical path).

    /// Fallible variant of [`Self::extend`].
    /// Returns `Err(BackendOverflow)` on fixed-capacity backends
    /// (`UserSliceBackend`) when the write would exceed the slice
    /// length. Growable backends (FlatBuf / RingBuffer) cannot
    /// return `Err` for capacity reasons — their underlying `Vec`
    /// grows on demand, and a true allocation failure aborts the
    /// process rather than surfacing through `Result` (`Vec`
    /// contract). Default impl delegates to the panic-on-overflow
    /// [`Self::extend`] — backends with non-growable capacity MUST
    /// override.
    fn try_extend(&mut self, data: &[u8]) -> Result<(), BackendOverflow> {
        self.extend(data);
        Ok(())
    }

    /// Fallible variant of [`Self::extend_and_fill`]. Same contract
    /// as [`Self::try_extend`].
    fn try_extend_and_fill(
        &mut self,
        fill_with: u8,
        fill_length: usize,
    ) -> Result<(), BackendOverflow> {
        self.extend_and_fill(fill_with, fill_length);
        Ok(())
    }

    /// Fallible variant of [`Self::extend_from_within_unchecked`].
    /// Validates `start + len <= self.len()` (source bound) and then
    /// `reserve(len)` to grow capacity for the write. The default
    /// impl deliberately omits a linear `tail + len <= cap` check
    /// because `RingBuffer::tail` is a modular wrap-index where
    /// `tail + len > cap` is normal mid-stream (the write straddles
    /// the wrap point). Fixed-capacity backends (`UserSliceBackend`)
    /// override with an explicit linear capacity check that DOES
    /// validate `tail + len <= cap`. On `Err` the backend state is
    /// untouched.
    ///
    /// Unlike the unsafe variant, this is a SAFE entry point: the
    /// bounds check moves into the method, so callers don't need to
    /// satisfy the `Self::extend_from_within_unchecked` safety
    /// contract at the call site.
    ///
    /// NOTE: Currently unused on production paths. The direct
    /// decode's Compressed-block sequence executor writes via the
    /// existing unchecked path; threading `try_*` through the
    /// fused decode+execute pipeline is the next step toward
    /// unconditional adversarial-input safety. RLE/Raw blocks
    /// already use `try_extend_and_fill` / `try_extend`.
    #[allow(dead_code)]
    fn try_extend_from_within(&mut self, start: usize, len: usize) -> Result<(), BackendOverflow> {
        // Default impl: a SAFE method must NOT delegate to the
        // unsafe variant without validating its safety contract.
        // Validate the source range (`start + len <= self.len()`),
        // then `reserve(len)` to guarantee destination capacity
        // (growable-backend invariant — see the linear vs wrap-aware
        // discussion below). NO eager `tail + len <= cap` check
        // because `RingBuffer::tail` is a modular wrap-index where
        // `tail + len > cap` is normal mid-stream. Fixed-capacity
        // backends (`UserSliceBackend`) override with their own
        // wrap-unaware linear capacity check.
        let tail = self.tail();
        let capacity = self.cap();
        let src_end = start.checked_add(len).ok_or(BackendOverflow {
            tail,
            requested: len,
            capacity,
        })?;
        if src_end > self.len() {
            return Err(BackendOverflow {
                tail,
                requested: len,
                capacity,
            });
        }
        // Growth + linear destination bound:
        //
        // `reserve(len)` is the growable-backend invariant — after
        // it returns, the backend has room for `len` more bytes.
        // For `FlatBuf` that's a linear `Vec::reserve`; for
        // `RingBuffer` it's a wrap-aware grow that maintains the
        // ring invariant. EITHER way, the only check needed by the
        // default impl is the `start + len` source bound above —
        // capacity for the write is guaranteed by `reserve`.
        //
        // We deliberately do NOT add a `tail + len <= cap` check
        // here: `RingBuffer::tail` is a modular index that wraps,
        // so a `tail + len > cap` situation is normal mid-stream
        // (the write straddles the wrap and lands at the head end).
        // An eager linear check would reject valid wrap writes and
        // return `Err(BackendOverflow)` on inputs the underlying
        // `extend_from_within_unchecked` would handle correctly.
        // Fixed-capacity backends (`UserSliceBackend`) override
        // `try_extend_from_within` with their own non-wrap-aware
        // capacity check.
        self.reserve(len);
        // SAFETY: source bound `start + len <= self.len()` checked
        // above; destination capacity guaranteed by the just-called
        // `reserve(len)`, both linear (FlatBuf) and wrap-aware
        // (RingBuffer). Wrap-unaware fixed-capacity backends
        // override this method.
        unsafe { self.extend_from_within_unchecked(start, len) };
        Ok(())
    }
}

/// Backend write failed. Surfaced only by fallible `try_*` methods
/// on fixed-capacity backends (`UserSliceBackend`); growable backends
/// (`FlatBuf`, `RingBuffer`) never produce this — they grow instead.
///
/// Covers three distinct failure modes on `UserSliceBackend`:
/// 1. **Destination capacity overshoot** — `tail + len > slice.len()`:
///    the new tail would exceed the caller's output slice.
/// 2. **Arithmetic overflow** — `tail.checked_add(len)` overflowed
///    (or `head.checked_add(start)` in `try_extend_from_within`):
///    adversarial `len` near `usize::MAX` triggers the wrap-guard
///    `ok_or` branch.
/// 3. **Source-range violation** (`try_extend_from_within` only) —
///    `abs_end > self.tail`: the requested match-copy source range
///    extends past the live region.
///
/// All three modes return the same struct shape so the caller doesn't
/// need to discriminate; `tail` / `requested` / `capacity` carry the
/// diagnostic context. The decoder converts this into
/// `FrameDecoderError::FrameContentSizeMismatch` at the
/// `decode_to_slice_trusted` boundary, so callers never see
/// `BackendOverflow` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendOverflow {
    /// Current physical write cursor at the moment the write was
    /// attempted.
    pub tail: usize,
    /// Number of bytes the failing write tried to append.
    pub requested: usize,
    /// Total physical capacity of the backend.
    pub capacity: usize,
}

impl core::fmt::Display for BackendOverflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "BufferBackend overflow: tail={}, requested={}, capacity={}",
            self.tail, self.requested, self.capacity,
        )
    }
}

#[cfg(test)]
mod tests {
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
}
