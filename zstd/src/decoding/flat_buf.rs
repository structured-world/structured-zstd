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
    /// Bytes in `buf[..head]` have already been handed to the output
    /// sink. They are retained until end-of-frame so back-references
    /// (`repeat`) into the recent history still resolve.
    head: usize,
}

impl FlatBuf {
    pub fn with_capacity(cap: usize) -> Self {
        // +WILDCOPY_OVERLENGTH so SIMD overshoot writes from the last
        // legitimate push / repeat land inside the allocation and not
        // in unrelated heap memory.
        let mut buf = Vec::with_capacity(cap + WILDCOPY_OVERLENGTH);
        // Zero the slack region once so any read of the
        // not-yet-written trailing bytes (from a wildcopy that reads
        // past tail) sees defined values. The bytes are never returned
        // to callers; this only keeps the read itself out of UB.
        unsafe {
            ptr::write_bytes(buf.as_mut_ptr(), 0, cap + WILDCOPY_OVERLENGTH);
        }
        Self { buf, head: 0 }
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
        // `Vec::reserve(additional)` is "additional bytes beyond len",
        // not "delta from capacity" — compute the gap correctly so an
        // allocation does happen when len < capacity < len+n. The
        // previous shape silently under-reserved on that case and
        // could leave fewer than `n` writable bytes available to a
        // subsequent unsafe extend.
        let available = self.buf.capacity().saturating_sub(self.buf.len());
        if available < n {
            self.buf
                .reserve((n - available).saturating_add(WILDCOPY_OVERLENGTH));
        }
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
        debug_assert!(new_tail <= self.buf.capacity());
        // SAFETY: forwarded to Vec::set_len. Slack region initialised
        // at `with_capacity`; bytes between new_tail and the prior
        // tail are discarded by the caller per `BufferBackend::set_tail`.
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
        let old = self.buf.len();
        self.buf.resize(old + fill_length, 0);
        match read.read_exact(&mut self.buf[old..old + fill_length]) {
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
    use crate::decoding::buffer_backend::BufferBackend;

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
