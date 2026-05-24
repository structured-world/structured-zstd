//! User-slice-backed output buffer for the "decode straight into the
//! caller's output slice" fast path.
//!
//! Selected by [`crate::decoding::FrameDecoder::decode_to_slice`]
//! when ALL of the following hold:
//! - `frame_content_size > 0` (FCS present in the frame header).
//! - `output.len() >= frame_content_size + WILDCOPY_OVERLENGTH`
//!   (room for the SIMD wildcopy overshoot slack).
//! - No active dictionary (the persistent dict_content is not
//!   carried into the stack-local DecodeBuffer this backend
//!   builds; dict frames stay on the regular path).
//!
//! `content_checksum_flag` is NOT a precondition: when set, the
//! direct-decode caller walks `output[..content_size]` once at end
//! of decode (single sequential xxhash pass over cache-hot data)
//! and stores the digest into the persistent scratch's hasher so
//! `get_calculated_checksum()` reads the right value.
//!
//! Multi-segment frames work via the caller's per-block
//! `DecodeBuffer::drop_to_window_size` invocation — bytes drop
//! out of `len()`'s visible range once decoded output exceeds
//! `window_size`, but physically stay in the user's slice (this
//! backend's `drop_first_n` only advances `head`).
//!
//! The `drop_to_window_size` call runs only BETWEEN blocks, so
//! within a single block `len()` can temporarily exceed
//! `window_size`. `DecodeBuffer::repeat` validates match offsets
//! against `len()` (not `window_size`), so this is not a strict
//! enforcement of the spec's `offset <= window_size` rule — only
//! a coarse end-of-block cap. The fallback path
//! (`FlatBuf`/`RingBuffer`) shares the same limitation. Strict
//! in-block offset bounds would require additional validation
//! that neither path currently performs.
//!
//! When eligible, literal pushes and match-history copies write
//! directly into the user's slice. Compared to
//! `DecodeBuffer<FlatBuf>`, this elides one full `memmove` of the
//! live region (the `read` drain that copies the flat Vec into the
//! user slice) and one anonymous-page allocation cycle per frame.
//! On `level_-7_fast/decodecorpus-z000033/rust_stream` the
//! direct-write path measured -20.33% vs the FlatBuf+drain path on
//! i9-9900K — see #244 for the flamegraph.
//!
//! Selected at compile time via `DecodeBuffer<UserSliceBackend<'a>>`
//! (generic [`BufferBackend`](super::buffer_backend::BufferBackend)
//! parameter). The lifetime parameter binds the backend to the
//! user-provided slice — the backing
//! `DecodeBuffer<UserSliceBackend<'a>>` is stack-local in
//! `decode_to_slice` and does not survive across calls. Persistent
//! decoder state (HUF/FSE tables, offset_hist, sequence cache)
//! lives in `FrameDecoder` and is borrowed in by reference for the
//! call's duration via [`super::scratch::DirectScratch`].

use crate::io::{Error, Read};

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
///
/// # DoS surface on malformed Compressed blocks
///
/// The bounds checks on write entry points are release-mode
/// `assert!` (for `extend` and `extend_from_within_unchecked`) and
/// `debug_assert!` (for `extend_and_fill`). On adversarial input —
/// a frame header that declares a small `frame_content_size` AND a
/// Compressed block whose payload expands to more than the declared
/// size — the burst's per-symbol writes can reach past `slice.len()`
/// and panic (`assert!` failure or, in `extend_and_fill`'s release
/// build, a slice-indexing panic) instead of returning a structured
/// error. The frame-level `decode_to_slice` checks `produced >
/// content_size` AFTER each block; within a single block the
/// panic-on-overshoot is the only stop.
///
/// Callers handling untrusted input should use
/// [`crate::decoding::FrameDecoder::decode_all`] which routes
/// through `FlatBuf` / `RingBuffer` backends. Those backends grow
/// via `Vec::reserve` — growth doesn't return errors (it succeeds
/// or aborts on alloc failure), but the growable Vec capacity
/// means a malformed block whose decompressed output exceeds FCS
/// stays inside the allocation instead of writing OOB into a
/// fixed-size user slice. The frame-level checks then catch the
/// FCS mismatch and return `FrameContentSizeMismatch`.
///
/// Replacing the panics with `Result<_, _>`-returning writes (so
/// `decode_to_slice` itself can stay safe on adversarial input) is
/// tracked in issue #246 — it requires extending the
/// `BufferBackend` trait surface and would gate the direct path on
/// the new fallible signatures.
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
    ///
    /// The `&mut []` literal is `&'static mut [u8; 0]` (compiler-
    /// emitted empty array, no aliasing because length is 0). It
    /// coerces to `&'a mut [u8]` for any `'a` because `&'_ mut T`
    /// is covariant in its lifetime parameter (longer lifetime can
    /// be narrowed). No raw-pointer + PhantomData workaround needed
    /// — verified by `cargo check` + 587 tests passing.
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
    fn reserve(&mut self, _n: usize) {
        // No-op: capacity is fixed at construction (slice length).
        // The decoder's sequence-execution path issues
        // `buffer.reserve(MAX_BLOCK_SIZE)` upfront as a precaution
        // for FlatBuf's growable Vec; for UserSliceBackend we can
        // never satisfy that precaution because the slice can't
        // grow. The actual write-site debug_asserts in `extend` /
        // `extend_and_fill` / `extend_from_within_unchecked` /
        // `extend_from_reader` catch the real (much smaller)
        // capacity bound — `match_length` and per-block writes are
        // bounded by the well-formed-frame contract such that
        // `tail + write_size <= frame_content_size`.
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
        let len = data.len();
        let new_tail = self.tail + len;
        // Release-mode capacity assert (mirrors
        // extend_from_within_unchecked). The body issues an unsafe
        // SIMD copy that takes `total_writable` as its
        // upper-bound contract; a malformed Compressed block whose
        // literals expand past the declared frame_content_size
        // would otherwise pass through `debug_assert!` in release
        // builds and turn the unchecked copy into UB. Cost: one
        // compare on the literal-push path — same magnitude as
        // the surrounding bounds-already-baked-in writes.
        assert!(
            new_tail <= self.slice.len(),
            "UserSliceBackend::extend overflows slice (tail+={}, cap={}) — corrupt frame",
            len,
            self.slice.len()
        );
        // Literal pushes (the sequence executor calls this for the
        // literal portion of every sequence) frequently land in the
        // 1..=16 byte range on highly-compressed corpora — exactly
        // the range where libc memmove dispatch overhead dominates
        // the cost of the actual copy. Route through
        // `copy_bytes_overshooting` so short pushes inline a single
        // SIMD / overlapping-u64 store instead.
        let total_writable = self.slice.len() - self.tail;
        // SAFETY: caller-provided `data` is non-aliasing with the
        // backend's slice (it's the literals buffer or input view).
        // `new_tail <= self.slice.len()` (debug_assert above), so
        // both regions have ≥ `len` valid bytes.
        unsafe {
            super::simd_copy::copy_bytes_overshooting(
                (data.as_ptr(), len),
                (self.slice.as_mut_ptr().add(self.tail), total_writable),
                len,
            );
        }
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
        // Release-mode capacity check: the trait contract says
        // capacity for `len` bytes past the tail was reserved by
        // the caller, but UserSliceBackend's reserve is a no-op
        // (slice can't grow), so a malformed frame with an out-of-
        // bounds match could otherwise turn the unchecked
        // SIMD copy into UB in release builds. FlatBuf relies on
        // `Vec::reserve`; we have no allocator to call so the
        // check has to be inline. Cost: one compare on a path
        // that's already memory-bound.
        assert!(
            dst_off + len <= self.slice.len(),
            "UserSliceBackend: match write past slice capacity (corrupt frame)"
        );
        // Route the match copy through `simd_copy::copy_bytes_overshooting`
        // rather than `ptr::copy_nonoverlapping`. The latter goes to
        // libc `__memmove_avx_unaligned_erms` on x86_64, which costs
        // ~10ns of dispatch overhead per call — measured at 40% of
        // decode CPU on the L-1 fast c_stream flamegraph because
        // L-1 produces thousands of short matches per frame.
        // `copy_bytes_overshooting` inlines a single SIMD
        // load+store for `len <= 16` with WILDCOPY_OVERLENGTH slack
        // (typical match case), and a byte / overlapping-u64
        // sequence for shorter copies without slack — both bypass
        // the libc memmove dispatch entirely.
        let total_readable = self.tail - src_off;
        let total_writable = self.slice.len() - dst_off;
        // SAFETY: caller's non-overlap precondition gives
        // `src_off + len <= dst_off` (so src/dst regions don't
        // overlap), `total_readable >= len` follows from
        // `src_off + len <= dst_off <= self.tail`, and
        // `total_writable >= len` by the assert above. The
        // helper writes ≤ `total_writable` bytes, so it stays
        // inside the slice even when it overshoots `len`.
        unsafe {
            let base = self.slice.as_mut_ptr();
            super::simd_copy::copy_bytes_overshooting(
                (base.add(src_off), total_readable),
                (base.add(dst_off), total_writable),
                len,
            );
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
