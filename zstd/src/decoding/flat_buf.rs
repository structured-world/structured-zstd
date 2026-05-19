//! Vec-backed flat output buffer for the "frame fits in window" fast path.
//!
//! When the frame's `Single_Segment_flag` is set the decompressed output
//! never exceeds `window_size`, the ring layout never wraps, and the
//! whole `DecodeBuffer` surface collapses to a growing `Vec<u8>` plus a
//! logical head index for streamed drains. Skipping the ring buffer's
//! wrap-dispatch on every push/repeat/drain is the win this module is
//! targeted at — see backlog item #132.
//!
//! Selected at compile time via `DecodeBuffer<FlatBuf>` (generic
//! [`BufferBackend`](super::buffer_backend::BufferBackend)
//! parameter). The earlier `enum BufferStorage { Ring, Flat }` attempt
//! paid runtime match overhead in every hot-path entry and measured a
//! +43–58 % regression on small-frame decompress — generic mono-
//! morphisation strips that match at compile time per call site.

use crate::io::{Error, Read};
use alloc::vec::Vec;
use core::ptr;

use super::buffer_backend::{BufferBackend, WILDCOPY_OVERLENGTH};

pub(crate) struct FlatBuf {
    buf: Vec<u8>,
    /// Bytes in `buf[..head]` have already been handed to the
    /// output sink and are no longer visible through the
    /// [`BufferBackend`] surface (`len`, `as_slices`,
    /// `extend_from_within_unchecked` all index relative to `head`).
    /// They live on physically in the allocation because the linear
    /// `Vec` layout never reuses that region — discarding them would
    /// require a memmove of the active window.
    ///
    /// Scope: `FlatBuf` is selected by `DecodeBuffer<FlatBuf>` only
    /// for frames whose `FrameHeader.descriptor.single_segment_flag()`
    /// is set. Such frames decode in a single segment of exactly
    /// `frame_content_size` bytes and never trigger
    /// `drain_to_window_size_writer` mid-stream — drain (and the
    /// corresponding `drop_first_n` head advance) only happens at
    /// end-of-frame. The "drained prefix no longer visible to
    /// `repeat`" semantics therefore match `RingBuffer`'s
    /// behaviour for the same call shape (both backends expose only
    /// `head..tail` through `len`/`as_slices`), and the FlatBuf
    /// path can't observe a streaming-drain scenario where the
    /// distinction would matter.
    head: usize,
}

impl FlatBuf {
    pub fn with_capacity(cap: usize) -> Self {
        // +WILDCOPY_OVERLENGTH so any future SIMD overshoot write from
        // a `push` / `repeat` near the buffer boundary lands inside
        // the allocation. The slack region is intentionally left
        // uninitialised: FlatBuf's current API only reads bytes
        // inside `head..buf.len()` (`as_slices`, drain helpers), and
        // its mutating helpers (`extend`, `extend_and_fill`,
        // `extend_from_within_unchecked`) only WRITE past `len`
        // before any matching `set_len`, never read it. Skipping the
        // zero pass is intentional — it avoids paying O(cap) on every
        // small single-segment frame reset.
        Self {
            buf: Vec::with_capacity(cap + WILDCOPY_OVERLENGTH),
            head: 0,
        }
    }
}

impl BufferBackend for FlatBuf {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }

    #[inline]
    fn reserve(&mut self, n: usize) {
        // `Vec::reserve(additional)` guarantees
        // `capacity >= len + additional`; passing
        // `n + WILDCOPY_OVERLENGTH` is the exact contract callers
        // need (room for `n` bytes plus the SIMD overshoot slack).
        //
        // Previous attempts computed the reserve amount as
        // `(n - available)` or `(needed - capacity)`, both of which
        // under-reserve when `len > 0`. Concrete repro: on a
        // multi-frame stream where frame 2 has `window_size > frame
        // 1's capacity` and `len == 0` post-reset, `available ==
        // old_capacity`, so `additional = (n - old_capacity) +
        // slack`; `Vec::reserve` then only ensures
        // `new_capacity >= len + additional = (n - old_capacity) +
        // slack`, which is short by `old_capacity`. Subsequent
        // `extend_from_within_unchecked` then panicked on the
        // `dst_off + len <= capacity` debug assert.
        // libFuzzer artifact crash-e33ba082… exercises exactly that
        // shape.
        self.buf.reserve(n.saturating_add(WILDCOPY_OVERLENGTH));
    }

    #[inline]
    fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    #[inline]
    fn cap(&self) -> usize {
        self.buf.capacity()
    }

    #[inline]
    fn tail(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    unsafe fn set_tail(&mut self, new_tail: usize) {
        debug_assert!(new_tail >= self.head);
        debug_assert!(new_tail <= self.buf.len());
        // SAFETY: forwarded to Vec::set_len. `new_tail` must come
        // from a previous `tail()` on this same instance (the
        // checkpoint's cap snapshot guarantees no realloc), so the
        // bytes re-exposed in `0..new_tail` were already written and
        // are initialised. Bytes between `new_tail` and the prior
        // tail are discarded by the caller per
        // `BufferBackend::set_tail` and never read again. The
        // trailing slack region past `buf.len()` is intentionally
        // uninitialised (see `with_capacity`) and never read by any
        // FlatBuf code path.
        unsafe { self.buf.set_len(new_tail) };
    }

    #[inline]
    fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    #[inline]
    fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        let new_len = self.buf.len() + fill_length;
        self.buf.resize(new_len, fill_with);
    }

    fn extend_from_reader<R: Read>(
        &mut self,
        mut read: R,
        fill_length: usize,
    ) -> Result<(), Error> {
        // Forming `&mut [u8]` over uninitialised `Vec` spare
        // capacity is UB even before any write — `&mut T` must
        // always reference initialised, valid memory of the target
        // type. Initialise via `Vec::resize(.., 0)` first, then
        // hand the resulting initialised slice to `read_exact`.
        // The earlier "read straight into spare capacity to skip
        // the zero-fill" shape traded soundness for a ~one-memset-
        // per-128-KiB-raw-block win; not worth the UB.
        // On read failure, truncate the Vec back to its pre-call
        // length so observable behaviour matches the previous
        // truncate-on-error shape.
        let old = self.buf.len();
        let new_len = old + fill_length;
        // Routes through `BufferBackend::reserve`, which keeps the
        // `WILDCOPY_OVERLENGTH` slack invariant uniform with
        // `with_capacity` / inline `reserve` growth paths.
        self.reserve(fill_length);
        self.buf.resize(new_len, 0);
        let read_slot = &mut self.buf[old..new_len];
        match read.read_exact(read_slot) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.buf.truncate(old);
                Err(e)
            }
        }
    }

    #[inline]
    unsafe fn extend_from_within_unchecked(&mut self, start: usize, len: usize) {
        let dst_off = self.buf.len();
        let src_off = self.head + start;
        debug_assert!(src_off + len <= dst_off);
        debug_assert!(dst_off + len <= self.buf.capacity());
        // SAFETY: caller's non-overlap precondition gives
        // src_off + len <= dst_off. Capacity covers dst_off + len.
        unsafe {
            let ptr = self.buf.as_mut_ptr();
            ptr::copy_nonoverlapping(ptr.add(src_off), ptr.add(dst_off), len);
            self.buf.set_len(dst_off + len);
        }
    }

    #[inline]
    unsafe fn extend_from_within_unchecked_branchless(&mut self, start: usize, len: usize) {
        // Flat layout never has overlap concerns the branchless variant
        // was designed for — forward to the single non-overlapping copy.
        // SAFETY: forwarded.
        unsafe { self.extend_from_within_unchecked(start, len) }
    }

    #[inline]
    fn as_slices(&self) -> (&[u8], &[u8]) {
        (&self.buf[self.head..], &[])
    }

    #[inline]
    fn drop_first_n(&mut self, n: usize) {
        self.head += n;
        debug_assert!(self.head <= self.buf.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_capacity_starts_empty() {
        let f = FlatBuf::with_capacity(1024);
        assert_eq!(f.len(), 0);
        assert_eq!(f.tail(), 0);
        assert!(f.cap() >= 1024 + WILDCOPY_OVERLENGTH);
    }

    #[test]
    fn extend_appends_then_len_matches() {
        let mut f = FlatBuf::with_capacity(64);
        f.extend(&[1, 2, 3, 4]);
        assert_eq!(f.len(), 4);
        f.extend(&[5, 6]);
        assert_eq!(f.len(), 6);
        let (s1, s2) = f.as_slices();
        assert_eq!(s1, &[1, 2, 3, 4, 5, 6]);
        assert!(s2.is_empty(), "flat layout never wraps");
    }

    #[test]
    fn extend_and_fill_appends_repeated_byte() {
        let mut f = FlatBuf::with_capacity(64);
        f.extend(&[0xAA]);
        f.extend_and_fill(0xBB, 5);
        let (s1, _) = f.as_slices();
        assert_eq!(s1, &[0xAA, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB]);
    }

    #[test]
    fn extend_from_within_unchecked_copies_non_overlapping() {
        let mut f = FlatBuf::with_capacity(64);
        f.extend(&[10, 20, 30, 40, 50]);
        // SAFETY: start+len=3 <= len()=5; capacity covers 5+3.
        unsafe { f.extend_from_within_unchecked(0, 3) };
        let (s1, _) = f.as_slices();
        assert_eq!(s1, &[10, 20, 30, 40, 50, 10, 20, 30]);
    }

    #[test]
    fn drop_first_n_advances_head() {
        let mut f = FlatBuf::with_capacity(64);
        f.extend(&[1, 2, 3, 4, 5]);
        f.drop_first_n(2);
        assert_eq!(f.len(), 3);
        let (s1, _) = f.as_slices();
        assert_eq!(s1, &[3, 4, 5]);
        // Drained bytes remain physically present and back match copies.
        // After head=2, logical start=0 maps to physical index 2.
        // SAFETY: start+len=3 <= len()=3.
        unsafe { f.extend_from_within_unchecked(0, 3) };
        let (s1, _) = f.as_slices();
        assert_eq!(s1, &[3, 4, 5, 3, 4, 5]);
    }

    #[test]
    fn set_tail_rolls_back() {
        let mut f = FlatBuf::with_capacity(64);
        f.extend(&[1, 2, 3]);
        let saved_tail = f.tail();
        let saved_cap = f.cap();
        f.extend(&[4, 5, 6, 7]);
        assert_eq!(f.len(), 7);
        assert_eq!(f.cap(), saved_cap, "with_capacity sized to avoid realloc");
        // SAFETY: cap unchanged; new_tail came from prior tail() call.
        unsafe { f.set_tail(saved_tail) };
        assert_eq!(f.len(), 3);
        let (s1, _) = f.as_slices();
        assert_eq!(s1, &[1, 2, 3]);
    }

    #[test]
    fn clear_resets() {
        let mut f = FlatBuf::with_capacity(64);
        f.extend(&[1, 2, 3]);
        f.drop_first_n(1);
        assert_eq!(f.len(), 2);
        f.clear();
        assert_eq!(f.len(), 0);
        assert_eq!(f.tail(), 0);
    }
}
