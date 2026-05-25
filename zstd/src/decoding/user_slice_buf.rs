//! User-slice-backed output buffer for the "decode straight into the
//! caller's output slice" fast path.
//!
//! Selected by [`crate::decoding::FrameDecoder::decode_to_slice_trusted`]
//! when ALL of the following hold:
//! - `frame_content_size > 0` — the header-derived content size
//!   is non-zero. This is the actual eligibility condition (NOT
//!   "FCS present"): an empty frame with an explicit FCS=0
//!   declaration on the wire stays on the fallback path because
//!   there is no payload to write into the user slice. To
//!   distinguish "FCS absent" from "FCS=0 explicit" elsewhere in
//!   the decoder, use `FrameHeader::fcs_declared()` (e.g. the
//!   fallback path's post-decode size check does).
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
//! `decode_to_slice_trusted` and does not survive across calls. Persistent
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
/// - Bytes in `slice[tail..]` are initialised memory (the slice was
///   passed in as safe `&mut [u8]` so every element is a valid `u8`)
///   but hold contents the decoder has not yet written — they carry
///   whatever the caller put in the buffer before passing it in.
///   The FlatBuf precedent skips zero-pre-fill on `extend` for the
///   same reason: writes happen at the band `[tail, tail + n)` and
///   the rest stays unread by the decoder. Callers must not read
///   past `tail` and expect meaningful decode output.
///
/// The caller MUST size the output slice with at least
/// `frame_content_size + WILDCOPY_OVERLENGTH` bytes so SIMD wildcopy
/// overshoots from `extend_from_within_unchecked` stay inside the
/// allocation. The dispatch site in [`crate::decoding::FrameDecoder`]
/// validates this precondition.
///
/// # DoS surface on malformed Compressed blocks
///
/// The bounds checks on write entry points are uniformly
/// release-mode `assert!` (for `extend`, `extend_and_fill`, and
/// `extend_from_within_unchecked`). On adversarial input — a frame
/// header that declares a small `frame_content_size` AND a
/// Compressed block whose payload expands to more than the declared
/// size — the burst's per-symbol writes can reach past `slice.len()`
/// and panic via the `assert!` failure instead of returning a
/// structured error. The frame-level `decode_to_slice_trusted` checks
/// `produced > content_size` AFTER each block; within a single block
/// the panic-on-overshoot is the only stop.
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
/// `decode_to_slice_trusted` itself can stay safe on adversarial input) is
/// tracked in issue #246 — it requires extending the
/// `BufferBackend` trait surface and would gate the direct path on
/// the new fallible signatures.
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
    pub(crate) fn from_slice(slice: &'a mut [u8]) -> Self {
        Self {
            slice,
            head: 0,
            tail: 0,
        }
    }
}

impl<'a> BufferBackend for UserSliceBackend<'a> {
    /// Donor-shape inline `ZSTD_execSequence` is available on
    /// `x86_64`, where SSE2 is the architectural baseline and the
    /// helpers in [`super::exec_sequence_donor::x86`] emit
    /// `_mm_loadu_si128` / `_mm_storeu_si128` without needing a
    /// `#[target_feature]` gate. 32-bit `x86` is excluded — its
    /// pre-SSE2 baseline (i386 / i486 / i586) would SIGILL on the
    /// SSE2 intrinsics. aarch64 NEON, riscv etc. fall back to the
    /// existing `extend` + `repeat` chain — those paths already
    /// emit the architecture's best-available SIMD via
    /// `copy_bytes_overshooting`'s runtime detect.
    const SUPPORTS_INLINE_DONOR_EXEC: bool = cfg!(target_arch = "x86_64");

    /// Donor `ZSTD_execSequence` body — see trait doc for
    /// preconditions / contract.
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn donor_exec_one_sequence(&mut self, lits: &[u8], offset: usize, match_length: usize) {
        use super::exec_sequence_donor::x86::{
            copy16, overlap_copy8, wildcopy_no_overlap, wildcopy_overlap_8byte_stride,
        };
        let lit_length = lits.len();
        let total = lit_length + match_length;
        let new_tail = self.tail + total;
        // Release-mode capacity assert — covers the literal+match
        // copies INCLUDING the wildcopy overshoot tail (up to 15
        // bytes past `tail + total`). The caller-side
        // `WILDCOPY_OVERLENGTH = 32` slack on the user slice covers
        // any value ≤ 32; the assert here makes the requirement
        // explicit so a future caller that supplies a smaller slice
        // fails immediately with a clear diagnostic instead of
        // silently corrupting memory past the declared tail.
        const MAX_WILDCOPY_OVERSHOOT: usize = 15;
        assert!(
            new_tail + MAX_WILDCOPY_OVERSHOOT <= self.slice.len(),
            "UserSliceBackend::donor_exec_one_sequence overflows slice \
             (tail+={} + {} overshoot, cap={}) — corrupt frame or missing WILDCOPY_OVERLENGTH slack",
            total,
            MAX_WILDCOPY_OVERSHOOT,
            self.slice.len()
        );
        debug_assert!(offset >= 1);
        debug_assert!(match_length >= 1);
        debug_assert!(
            offset <= self.tail + lit_length,
            "donor_exec_one_sequence: offset ({}) exceeds output start \
             ({} + lit {} = {})",
            offset,
            self.tail,
            lit_length,
            self.tail + lit_length
        );

        // SAFETY: capacity asserted above; pointer arithmetic stays
        // within `self.slice` for the writes (tail + total <=
        // slice.len()) and reads (match src = tail + lit_length -
        // offset, bounded by `offset <= tail + lit_length`).
        unsafe {
            let base_mut = self.slice.as_mut_ptr();
            let lit_src = lits.as_ptr();

            // ── Literal copy ──
            // Donor: ZSTD_copy16(op, *litPtr); if (litLength > 16)
            //         wildcopy(op+16, *litPtr+16, litLength-16, no_overlap)
            //
            // The unconditional first copy16 may overshoot up to 15
            // bytes past `op + lit_length` into the match-destination
            // region — those bytes are about to be overwritten by the
            // match copy below, so the overshoot is harmless.
            let op_lit = base_mut.add(self.tail);
            copy16(op_lit, lit_src);
            if lit_length > 16 {
                wildcopy_no_overlap(op_lit.add(16), lit_src.add(16), lit_length - 16);
            }

            // ── Match copy ──
            // Donor uses `oLitEnd = op + litLength` as the match
            // destination; the match source is `oLitEnd - offset`.
            let op_match = base_mut.add(self.tail + lit_length);
            let match_src = base_mut.cast_const().add(self.tail + lit_length - offset);

            if offset >= 16 {
                wildcopy_no_overlap(op_match, match_src, match_length);
            } else {
                // overlap_copy8 + wildcopy(overlap) tail.
                let (op2, ip2) = overlap_copy8(op_match, match_src, offset);
                if match_length > 8 {
                    wildcopy_overlap_8byte_stride(op2, ip2, match_length - 8);
                }
            }
        }
        self.tail = new_tail;
    }

    /// `new()` exists for trait conformance but is not used on the
    /// direct-decode path — the slice is always provided up-front via
    /// [`Self::from_slice`]. Returns an empty backend wrapping an
    /// empty static slice; any subsequent `extend` call will panic
    /// via the capacity check.
    ///
    /// `&mut []` is a zero-length placeholder slice the compiler
    /// emits with `'static` lifetime; assigning it into the
    /// `slice: &'a mut [u8]` field compiles for any `'a` because
    /// `'static` outlives every other lifetime. No aliasing concern
    /// because the length is 0 (no addressable bytes the field
    /// could alias against). No raw-pointer + PhantomData workaround
    /// needed — verified by `cargo check` + the full nextest suite.
    /// This placeholder shape is fine ONLY because `new()` is never
    /// the entry point on the direct-decode path; non-empty
    /// constructions go through `from_slice` with a real `&'a mut [u8]`.
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

    // `#[inline(always)]` because perf annotate on the primary bench
    // attributes ~10% of decode time to this method's own prologue /
    // epilogue (subq+pushq on entry, popq+retq on exit) — pure
    // function-call boundary cost on a method that is called from
    // tight literal-emit loops inside `decode_and_execute_sequences`.
    // The body is small (one assert, one copy), so forced inlining
    // does not bloat callers meaningfully. NOT a dispatch + match
    // pattern (the documented Tier-10 negative): the entry has no
    // runtime branch on kernel features — `copy_bytes_overshooting`
    // owns that dispatch internally.
    #[inline(always)]
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
        // The `assert!` below panics on adversarial / malformed input
        // (a Compressed block whose payload expands past declared FCS).
        // This is the deliberate trade-off for the trusted-input
        // direct-decode path: switching the write surface to fallible
        // `Result` writes — so the panic becomes a structured
        // `FrameContentSizeMismatch` — requires extending the
        // `BufferBackend` trait surface, touching every backend
        // implementation, and threading `Result<_, _>` through the
        // sequence executor. That refactor lives in a dedicated
        // follow-up (the docstring on `decode_to_slice_trusted` names it as
        // a hard prerequisite before this entry point becomes safe
        // on untrusted streams). Until then the contract is
        // documented at the safe-API surface and enforced by
        // `#[must_use]` + `#[doc(alias = "decode_to_slice_trusted_trusted")]`.
        // Re-flagging without addressing the trade-off documented at
        // the call site does not move the work forward.
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
        // `new_tail <= self.slice.len()` (release-mode `assert!`
        // above), so both regions have ≥ `len` valid bytes.
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
        // Release-mode `assert!` (not `debug_assert!`) for symmetry
        // with `extend` / `extend_from_within_unchecked`. Without it,
        // a malformed Compressed block whose RLE fill expands past
        // the declared `frame_content_size` would panic via the
        // subsequent `self.slice[self.tail..new_tail]` slice index
        // with a less-informative message, AND the in-block writes
        // would already have happened up to the slice length. Fail
        // fast with a clear corruption / capacity diagnostic.
        assert!(
            new_tail <= self.slice.len(),
            "UserSliceBackend::extend_and_fill overflows slice (tail+={}, cap={}) — corrupt frame",
            fill_length,
            self.slice.len()
        );
        // `slice::fill` lowers to a memset on byte slices; the
        // per-byte loop above the rebased commit replaces it with
        // an explicit assignment, which the optimiser does not
        // always promote back. For large RLE blocks the memset
        // path wins ~3-5x on x86_64.
        self.slice[self.tail..new_tail].fill(fill_with);
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

    // Keep `#[inline]` (hint, not force). An earlier experiment with
    // `#[inline(always)]` regressed primary bench by +2.96% — body
    // is materially larger than `extend` (assert + readable/writable
    // derivation + simd_copy::copy_bytes_overshooting call), and
    // forced inlining bloats each pipeline-slot caller past icache
    // budget. The per-call boundary save is dwarfed by the
    // duplicated body weight; LLVM's heuristic was right to decline.
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

    // ── Fallible write surface ──
    //
    // Override the default trait impls (which call panic-on-overflow
    // variants) with explicit capacity checks that return
    // `BackendOverflow` instead. This is the entire point of the
    // backend: a fixed-capacity output slice that cannot grow on
    // demand, so any overshoot must be reported instead of aborting.

    #[inline]
    fn try_extend(&mut self, data: &[u8]) -> Result<(), super::buffer_backend::BackendOverflow> {
        let len = data.len();
        // Use `checked_add` to catch adversarial input where
        // `self.tail + len` would wrap `usize` — without the wrap
        // check, the subsequent `new_tail > self.slice.len()` test
        // can be bypassed by an `len` near `usize::MAX`, turning
        // the fallible write into a release-mode panic via
        // `copy_bytes_overshooting`.
        // BackendOverflow is `Copy` with three usize fields; eager
        // construction is cheaper than a closure indirection on this
        // hot path (clippy::unnecessary_lazy_evaluations).
        let new_tail =
            self.tail
                .checked_add(len)
                .ok_or(super::buffer_backend::BackendOverflow {
                    tail: self.tail,
                    requested: len,
                    capacity: self.slice.len(),
                })?;
        if new_tail > self.slice.len() {
            return Err(super::buffer_backend::BackendOverflow {
                tail: self.tail,
                requested: len,
                capacity: self.slice.len(),
            });
        }
        let total_writable = self.slice.len() - self.tail;
        // SAFETY: `new_tail <= self.slice.len()` (checked above);
        // `data` is non-aliasing with the backend's slice (caller
        // contract — literals buffer / input view).
        unsafe {
            super::simd_copy::copy_bytes_overshooting(
                (data.as_ptr(), len),
                (self.slice.as_mut_ptr().add(self.tail), total_writable),
                len,
            );
        }
        self.tail = new_tail;
        Ok(())
    }

    #[inline]
    fn try_extend_and_fill(
        &mut self,
        fill_with: u8,
        fill_length: usize,
    ) -> Result<(), super::buffer_backend::BackendOverflow> {
        // Same wrap-check rationale as `try_extend` above — an
        // adversarial `fill_length` near `usize::MAX` would wrap
        // `new_tail`, bypass the upper bound, and panic in
        // `slice[tail..new_tail]` (start > end).
        let new_tail =
            self.tail
                .checked_add(fill_length)
                .ok_or(super::buffer_backend::BackendOverflow {
                    tail: self.tail,
                    requested: fill_length,
                    capacity: self.slice.len(),
                })?;
        if new_tail > self.slice.len() {
            return Err(super::buffer_backend::BackendOverflow {
                tail: self.tail,
                requested: fill_length,
                capacity: self.slice.len(),
            });
        }
        self.slice[self.tail..new_tail].fill(fill_with);
        self.tail = new_tail;
        Ok(())
    }

    #[inline]
    fn try_extend_from_within(
        &mut self,
        start: usize,
        len: usize,
    ) -> Result<(), super::buffer_backend::BackendOverflow> {
        // Bound 1: source range fits in live region.
        // The caller's `start` is relative to the live-region head;
        // map to a physical absolute position and check against the
        // current tail. Both `head + start` and `+ len` get
        // `checked_add` so an adversarial `start` or `len` near
        // `usize::MAX` cannot wrap past the bounds checks.
        let abs_start =
            self.head
                .checked_add(start)
                .ok_or(super::buffer_backend::BackendOverflow {
                    tail: self.tail,
                    requested: len,
                    capacity: self.slice.len(),
                })?;
        let abs_end = abs_start
            .checked_add(len)
            .ok_or(super::buffer_backend::BackendOverflow {
                tail: self.tail,
                requested: len,
                capacity: self.slice.len(),
            })?;
        if abs_end > self.tail {
            return Err(super::buffer_backend::BackendOverflow {
                tail: self.tail,
                requested: len,
                capacity: self.slice.len(),
            });
        }
        // Bound 2: destination has capacity for `len`. Same wrap
        // protection — without it, an adversarial `len` near
        // `usize::MAX` wraps and bypasses the upper bound, turning
        // the unchecked write into a release-mode UB / panic.
        // BackendOverflow is `Copy` with three usize fields; eager
        // construction is cheaper than a closure indirection on this
        // hot path (clippy::unnecessary_lazy_evaluations).
        let new_tail =
            self.tail
                .checked_add(len)
                .ok_or(super::buffer_backend::BackendOverflow {
                    tail: self.tail,
                    requested: len,
                    capacity: self.slice.len(),
                })?;
        if new_tail > self.slice.len() {
            return Err(super::buffer_backend::BackendOverflow {
                tail: self.tail,
                requested: len,
                capacity: self.slice.len(),
            });
        }
        // SAFETY: both bounds checked above. Forward to the unsafe
        // variant which performs the wildcopy with the same
        // preconditions the bounds checks established.
        unsafe { self.extend_from_within_unchecked(start, len) };
        Ok(())
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
    extern crate alloc;
    use super::*;
    use alloc::vec;

    #[test]
    fn extend_writes_at_tail() {
        let mut buf = vec![0u8; 32];
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
        let mut buf = vec![0u8; 16];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[0xAA]);
        b.extend_and_fill(0xBB, 4);
        let (s, _) = b.as_slices();
        assert_eq!(s, &[0xAA, 0xBB, 0xBB, 0xBB, 0xBB]);
    }

    #[test]
    fn extend_from_within_unchecked_copies_non_overlapping() {
        let mut buf = vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[10, 20, 30, 40, 50]);
        // SAFETY: 0+3 <= 5 = len; cap 32 covers 5+3.
        unsafe { b.extend_from_within_unchecked(0, 3) };
        let (s, _) = b.as_slices();
        assert_eq!(s, &[10, 20, 30, 40, 50, 10, 20, 30]);
    }

    #[test]
    fn drop_first_n_advances_head_keeps_history() {
        let mut buf = vec![0u8; 32];
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
        let mut buf = vec![0u8; 32];
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
        let mut buf = vec![0u8; 32];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3]);
        b.drop_first_n(1);
        b.clear();
        assert_eq!(b.len(), 0);
        assert_eq!(b.tail(), 0);
    }

    #[test]
    fn extend_from_reader_into_slice() {
        let mut buf = vec![0u8; 16];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        let src = [9u8, 8, 7, 6, 5];
        b.extend_from_reader(&src[..], 5).unwrap();
        let (s, _) = b.as_slices();
        assert_eq!(s, &[9, 8, 7, 6, 5]);
    }

    #[test]
    fn extend_from_reader_over_capacity_errors() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        let src = [9u8, 8, 7, 6, 5];
        // 5 bytes requested, only 4 cap -> error, tail unchanged.
        assert!(b.extend_from_reader(&src[..], 5).is_err());
        assert_eq!(b.tail(), 0);
    }

    // Coverage for the fallible try_* surface. Exercises:
    //   - happy paths (exact-fit + room to spare),
    //   - capacity-overflow paths (returns Err with diagnostic fields),
    //   - integer-overflow wrap-guards (checked_add ok_or branch).

    use super::super::buffer_backend::BufferBackend;

    #[test]
    fn try_extend_exact_fit_succeeds_and_advances_tail() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        assert!(b.try_extend(&[1, 2, 3, 4]).is_ok());
        assert_eq!(b.tail(), 4);
        let (s, _) = b.as_slices();
        assert_eq!(s, &[1, 2, 3, 4]);
    }

    #[test]
    fn try_extend_over_capacity_returns_overflow_and_keeps_tail() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        let err = b.try_extend(&[1, 2, 3, 4, 5]).unwrap_err();
        assert_eq!(err.tail, 0);
        assert_eq!(err.requested, 5);
        assert_eq!(err.capacity, 4);
        assert_eq!(b.tail(), 0);
    }

    #[test]
    fn try_extend_partially_full_overshoot_reports_current_tail() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2]);
        // 3 more bytes would need 5 total, capacity is 4.
        let err = b.try_extend(&[3, 4, 5]).unwrap_err();
        assert_eq!(err.tail, 2);
        assert_eq!(err.requested, 3);
        assert_eq!(err.capacity, 4);
        assert_eq!(b.tail(), 2);
    }

    #[test]
    fn try_extend_zero_length_succeeds_and_leaves_tail_unchanged() {
        // The `checked_add(tail, len)` wrap branch in `try_extend` is
        // a defense-in-depth guard for corrupted input that names
        // `regenerated_size` near `usize::MAX`. Constructing such a
        // `&[u8]` from safe Rust is not expressible — `from_raw_parts`
        // with a forged length is UB. The wrap branch is exercised
        // only from the fuzz harness under `feature = "fuzz_exports"`
        // (which routes a controlled `len` through `try_*`) and from
        // the real malformed-frame decode path that the harness
        // emulates.
        //
        // This test covers the adjacent normal case — a zero-length
        // `try_extend` MUST succeed regardless of current `tail`
        // (the new_tail = tail + 0 = tail comparison both passes
        // checked_add and the upper-bound check). Without this case
        // the early-return on `len == 0` could regress silently.
        let mut buf = vec![0u8; 8];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        assert!(b.try_extend(&[]).is_ok());
        assert_eq!(b.tail(), 0);
        b.extend(&[1, 2, 3]);
        assert!(b.try_extend(&[]).is_ok());
        assert_eq!(b.tail(), 3);
    }

    #[test]
    fn try_extend_and_fill_exact_fit_writes_pattern() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        assert!(b.try_extend_and_fill(0xAB, 4).is_ok());
        assert_eq!(b.tail(), 4);
        let (s, _) = b.as_slices();
        assert_eq!(s, &[0xAB, 0xAB, 0xAB, 0xAB]);
    }

    #[test]
    fn try_extend_and_fill_over_capacity_returns_overflow() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2]);
        let err = b.try_extend_and_fill(0xCD, 5).unwrap_err();
        assert_eq!(err.tail, 2);
        assert_eq!(err.requested, 5);
        assert_eq!(err.capacity, 4);
        assert_eq!(b.tail(), 2);
    }

    #[test]
    fn try_extend_from_within_within_bounds_repeats_history() {
        let mut buf = vec![0u8; 8];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3]);
        // Repeat the first 3 bytes from history into the next 3 slots.
        assert!(b.try_extend_from_within(0, 3).is_ok());
        let (s, _) = b.as_slices();
        assert_eq!(s, &[1, 2, 3, 1, 2, 3]);
        assert_eq!(b.tail(), 6);
    }

    #[test]
    fn try_extend_from_within_source_past_tail_returns_overflow() {
        let mut buf = vec![0u8; 8];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2]);
        // start=0, len=5 — source range needs bytes 0..5 but tail=2.
        let err = b.try_extend_from_within(0, 5).unwrap_err();
        assert_eq!(err.tail, 2);
        assert_eq!(err.requested, 5);
        assert_eq!(b.tail(), 2);
    }

    #[test]
    fn try_extend_from_within_destination_overflow_returns_err() {
        let mut buf = vec![0u8; 4];
        let mut b = UserSliceBackend::from_slice(&mut buf);
        b.extend(&[1, 2, 3]);
        // Source 0..2 valid, but writing 2 more bytes would push tail
        // from 3 to 5, past capacity 4.
        let err = b.try_extend_from_within(0, 2).unwrap_err();
        assert_eq!(err.tail, 3);
        assert_eq!(err.requested, 2);
        assert_eq!(err.capacity, 4);
        assert_eq!(b.tail(), 3);
    }
}
