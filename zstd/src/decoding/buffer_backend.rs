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
/// without leaving the allocation. Matches donor zstd's
/// `WILDCOPY_OVERLENGTH` (16 bytes = the largest single chunk
/// `simd_copy::copy_bytes_overshooting` writes when its single-op
/// fast path fires on copies ≤ 16 bytes). Both `RingBuffer` and
/// `FlatBuf` reuse this single constant so the slack contract
/// cannot drift between backends.
pub(crate) const WILDCOPY_OVERLENGTH: usize = 16;

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
    // capacity. Used by the direct-decode path
    // (`decode_to_slice_trusted` + descendants) so a malformed
    // Compressed block whose decompressed payload exceeds the
    // caller-provided output slice surfaces as a structured
    // `FrameDecoderError::FrameContentSizeMismatch` instead of an
    // abort.
    //
    // The growable backends (`FlatBuf`, `RingBuffer`) override these
    // to delegate to their existing growth path — they cannot fail
    // for capacity reasons because they grow the underlying `Vec` on
    // demand. The default impls below cover that case.
    //
    // The fixed-capacity backend (`UserSliceBackend`) overrides each
    // method with an explicit capacity check that returns `Err` on
    // overshoot instead of panicking. The trade-off is one branch
    // per write on the direct-decode path; benches confirmed the
    // overhead is ≤ 1 % on compare_ffi.

    /// Fallible variant of [`Self::extend`].
    /// Returns `Err(BackendOverflow)` on growable-backend allocation
    /// failure (rare; never panics) or on `UserSliceBackend` capacity
    /// overflow. Default impl delegates to the panic-on-overflow
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
    /// Validates `start + len <= self.len()` AND `tail + len <= cap`
    /// before calling the unsafe write. On `Err` the backend state
    /// is untouched.
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
        // Default impl: growable backends pre-reserve via `reserve`,
        // then forward to the unsafe variant. Fixed-capacity
        // backends MUST override and check before calling.
        self.reserve(len);
        // SAFETY: `reserve(len)` guaranteed `tail + len <= cap` on
        // growable backends; the caller of `try_extend_from_within`
        // is responsible for `start + len <= self.len()`. The
        // documented precondition matches `extend_from_within_unchecked`
        // for that bound.
        unsafe { self.extend_from_within_unchecked(start, len) };
        Ok(())
    }
}

/// Backend write failed because the requested write would exceed the
/// backend's capacity. Surfaced only by fallible `try_*` methods on
/// fixed-capacity backends (`UserSliceBackend`); growable backends
/// (`FlatBuf`, `RingBuffer`) never produce this — they grow instead.
///
/// The decoder converts this into
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
