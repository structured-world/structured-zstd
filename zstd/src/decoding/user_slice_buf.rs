//! User-slice-backed output buffer for the "decode straight into the
//! caller's output slice" fast path.
//!
//! When the frame's `Single_Segment_flag` is set AND the caller passed
//! a sufficiently sized `&mut [u8]` to [`crate::decoding::FrameDecoder::decode_all`],
//! we can skip the FlatBuf-as-intermediate detour entirely: literal
//! pushes and match-history copies write directly into the user's
//! slice. Compared to `DecodeBuffer<FlatBuf>`, this elides one full
//! `memmove` of the live region (the `read` drain that copies the
//! flat Vec into the user slice) and one anonymous-page allocation
//! cycle per frame. On `level_-7_fast/decodecorpus-z000033/rust_stream`
//! the drain copy + page-touch chain was measured at ~46% of total
//! decompress time on i9-9900K — see #244 for the flamegraph.
//!
//! Selected at compile time via `DecodeBuffer<UserSliceBackend<'a>>`
//! (generic [`BufferBackend`](super::buffer_backend::BufferBackend)
//! parameter). The lifetime parameter binds the backend to the
//! user-provided slice — `DecoderScratch<UserSliceBackend<'a>>` is
//! stack-local in [`crate::decoding::FrameDecoder::decode_all`] and
//! does not survive across calls. Persistent decoder state (HUF/FSE
//! tables, offset_hist, sequence cache) lives in `FrameDecoder` and
//! is borrowed in by reference for the call's duration.

use crate::io::{Error, Read};
use core::ptr;

use super::buffer_backend::{BufferBackend, WILDCOPY_OVERLENGTH};

/// Backend that writes directly into a caller-provided `&mut [u8]`
/// output slice. No internal allocation, no drain copy.
///
/// Invariants enforced by the [`BufferBackend`] surface:
/// - `head <= tail <= slice.len()`.
/// - All bytes in `slice[head..tail]` are initialised (written by
///   [`Self::extend`] / [`Self::extend_and_fill`] /
///   [`Self::extend_from_within_unchecked`] /
///   [`Self::extend_from_reader`]).
/// - Bytes in `slice[tail..]` are NOT yet initialised — the FlatBuf
///   precedent skips zero-initialising spare capacity for the same
///   reason; callers must not read past `tail`.
///
/// The caller MUST size the output slice with at least
/// `frame_content_size + WILDCOPY_OVERLENGTH` bytes so SIMD wildcopy
/// overshoots from `extend_from_within_unchecked` stay inside the
/// allocation. The dispatch site in [`crate::decoding::FrameDecoder`]
/// validates this precondition.
#[allow(dead_code)]
pub(crate) struct UserSliceBackend<'a> {
    slice: &'a mut [u8],
    /// Bytes in `slice[..head]` have been drained to the output
    /// stream and are no longer visible through the [`BufferBackend`]
    /// surface. Same semantics as `FlatBuf.head` — see that field's
    /// doc for the "drained prefix remains physically present, used
    /// by future match copies" justification. For the
    /// single-segment direct-decode path `head` stays at 0 until the
    /// frame finishes (no streaming-drain), but the field is kept
    /// for API parity with `FlatBuf` and `RingBuffer`.
    head: usize,
    tail: usize,
}

impl<'a> UserSliceBackend<'a> {
    /// Construct a backend wrapping `slice`. The slice must have at
    /// least `frame_content_size + WILDCOPY_OVERLENGTH` bytes of
    /// length so SIMD wildcopy overshoots stay inside the allocation;
    /// the dispatcher in `FrameDecoder` enforces this.
    #[allow(dead_code)]
    pub(crate) fn from_slice(slice: &'a mut [u8]) -> Self {
        Self {
            slice,
            head: 0,
            tail: 0,
        }
    }
}

impl<'a> BufferBackend for UserSliceBackend<'a> {
    /// `new()` exists for trait conformance but is not used on the
    /// direct-decode path — the slice is always provided up-front via
    /// [`Self::from_slice`]. Returns an empty backend wrapping an
    /// empty static slice; any subsequent `extend` call will panic
    /// via the capacity check.
    fn new() -> Self {
        Self {
            slice: &mut [],
            head: 0,
            tail: 0,
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }

    #[inline]
    fn reserve(&mut self, n: usize) {
        // Capacity is fixed at construction; the dispatcher sized the
        // slice to cover `frame_content_size + WILDCOPY_OVERLENGTH`.
        // `reserve` is a no-op as long as we have room. If the caller
        // requests more than we can provide it's a bug at the
        // dispatch site (frame header lied about content size, or
        // sizing math underflowed). Same shape as FlatBuf's contract:
        // reserve is best-effort; the actual capacity check is at
        // the write site via the debug_assert in extend/etc.
        debug_assert!(
            self.tail.saturating_add(n) <= self.slice.len(),
            "UserSliceBackend::reserve({n}) overflows slice (tail={}, cap={})",
            self.tail,
            self.slice.len()
        );
    }

    #[inline]
    fn len(&self) -> usize {
        self.tail - self.head
    }

    #[inline]
    fn cap(&self) -> usize {
        self.slice.len()
    }

    #[inline]
    fn tail(&self) -> usize {
        self.tail
    }

    #[inline]
    unsafe fn set_tail(&mut self, new_tail: usize) {
        debug_assert!(new_tail >= self.head);
        debug_assert!(new_tail <= self.slice.len());
        self.tail = new_tail;
    }

    #[inline]
    fn extend(&mut self, data: &[u8]) {
        let new_tail = self.tail + data.len();
        debug_assert!(
            new_tail <= self.slice.len(),
            "UserSliceBackend::extend overflows slice (tail+={}, cap={})",
            data.len(),
            self.slice.len()
        );
        self.slice[self.tail..new_tail].copy_from_slice(data);
        self.tail = new_tail;
    }

    #[inline]
    fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        let new_tail = self.tail + fill_length;
        debug_assert!(new_tail <= self.slice.len());
        for b in &mut self.slice[self.tail..new_tail] {
            *b = fill_with;
        }
        self.tail = new_tail;
    }

    fn extend_from_reader<R: Read>(
        &mut self,
        mut read: R,
        fill_length: usize,
    ) -> Result<(), Error> {
        let old = self.tail;
        let new_tail = old + fill_length;
        if new_tail > self.slice.len() {
            return Err(Error::other(
                "UserSliceBackend: raw block exceeds caller-provided output capacity",
            ));
        }
        match read.read_exact(&mut self.slice[old..new_tail]) {
            Ok(()) => {
                self.tail = new_tail;
                Ok(())
            }
            // Don't advance `tail` on failure — the upper bound from
            // the slice borrow above guarantees the `read_exact`
            // attempt didn't write past `new_tail`, but we MUST keep
            // `tail` pointing at the last fully-decoded byte so
            // checkpoint rollback / drain semantics line up with
            // FlatBuf's truncate-on-error shape.
            Err(e) => Err(e),
        }
    }

    #[inline]
    unsafe fn extend_from_within_unchecked(&mut self, start: usize, len: usize) {
        let dst_off = self.tail;
        let src_off = self.head + start;
        debug_assert!(src_off + len <= dst_off);
        debug_assert!(dst_off + len <= self.slice.len());
        // SAFETY: caller's non-overlap precondition gives
        // `src_off + len <= dst_off`. Capacity covers `dst_off + len`
        // by the dispatcher's `frame_content_size + WILDCOPY_OVERLENGTH`
        // sizing.
        unsafe {
            let ptr = self.slice.as_mut_ptr();
            ptr::copy_nonoverlapping(ptr.add(src_off), ptr.add(dst_off), len);
        }
        self.tail = dst_off + len;
    }

    #[inline]
    unsafe fn extend_from_within_unchecked_branchless(&mut self, start: usize, len: usize) {
        // Direct-slice layout never wraps — same forward to the
        // single non-overlapping copy as FlatBuf.
        unsafe { self.extend_from_within_unchecked(start, len) }
    }

    #[inline]
    fn as_slices(&self) -> (&[u8], &[u8]) {
        (&self.slice[self.head..self.tail], &[])
    }

    #[inline]
    fn drop_first_n(&mut self, n: usize) {
        self.head += n;
        debug_assert!(self.head <= self.tail);
    }
}

// `WILDCOPY_OVERLENGTH` is used implicitly via the dispatcher's
// capacity sizing — kept imported here for the doc reference and to
// surface a build error if the constant moves.
const _: () = {
    let _: usize = WILDCOPY_OVERLENGTH;
};

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    #[test]
    fn extend_writes_at_tail() {
        let mut buf = std::vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3, 4]);
        assert_eq!(b.len(), 4);
        assert_eq!(b.tail(), 4);
        b.extend(&[5, 6]);
        let (s, t) = b.as_slices();
        assert_eq!(s, &[1, 2, 3, 4, 5, 6]);
        assert!(t.is_empty());
    }

    #[test]
    fn extend_and_fill_repeats_byte() {
        let mut buf = std::vec![0u8; 16];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[0xAA]);
        b.extend_and_fill(0xBB, 4);
        let (s, _) = b.as_slices();
        assert_eq!(s, &[0xAA, 0xBB, 0xBB, 0xBB, 0xBB]);
    }

    #[test]
    fn extend_from_within_unchecked_copies_non_overlapping() {
        let mut buf = std::vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[10, 20, 30, 40, 50]);
        // SAFETY: 0+3 <= 5 = len; cap 32 covers 5+3.
        unsafe { b.extend_from_within_unchecked(0, 3) };
        let (s, _) = b.as_slices();
        assert_eq!(s, &[10, 20, 30, 40, 50, 10, 20, 30]);
    }

    #[test]
    fn drop_first_n_advances_head_keeps_history() {
        let mut buf = std::vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3, 4, 5]);
        b.drop_first_n(2);
        assert_eq!(b.len(), 3);
        let (s, _) = b.as_slices();
        assert_eq!(s, &[3, 4, 5]);
        // After drop, drained bytes remain physically present and can
        // back a match copy via `start` indexed from the post-drop head.
        unsafe { b.extend_from_within_unchecked(0, 3) };
        let (s, _) = b.as_slices();
        assert_eq!(s, &[3, 4, 5, 3, 4, 5]);
    }

    #[test]
    fn set_tail_rollback() {
        let mut buf = std::vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3]);
        let saved = b.tail();
        b.extend(&[4, 5, 6, 7]);
        assert_eq!(b.len(), 7);
        unsafe { b.set_tail(saved) };
        assert_eq!(b.len(), 3);
        let (s, _) = b.as_slices();
        assert_eq!(s, &[1, 2, 3]);
    }

    #[test]
    fn clear_resets_cursors() {
        let mut buf = std::vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3]);
        b.drop_first_n(1);
        b.clear();
        assert_eq!(b.len(), 0);
        assert_eq!(b.tail(), 0);
    }

    #[test]
    fn extend_from_reader_into_slice() {
        let mut buf = std::vec![0u8; 16];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        let src = [9u8, 8, 7, 6, 5];
        b.extend_from_reader(&src[..], 5).unwrap();
        let (s, _) = b.as_slices();
        assert_eq!(s, &[9, 8, 7, 6, 5]);
    }

    #[test]
    fn extend_from_reader_over_capacity_errors() {
        let mut buf = std::vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        let src = [9u8, 8, 7, 6, 5];
        // 5 bytes requested, only 4 cap -> error, tail unchanged.
        assert!(b.extend_from_reader(&src[..], 5).is_err());
        assert_eq!(b.tail(), 0);
    }
}
