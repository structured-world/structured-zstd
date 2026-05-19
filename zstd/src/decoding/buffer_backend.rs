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
/// without leaving the allocation.
#[allow(dead_code)] // Used by `flat_buf` (Phase 2 wiring still pending).
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

    /// Physical allocation capacity. Used as the realloc-detection
    /// sentinel in [`super::decode_buffer::DecodeBufferCheckpoint`].
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
}
