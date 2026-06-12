use crate::io::{Error, Read, Write};
use alloc::vec::Vec;
#[cfg(feature = "hash")]
use core::hash::Hasher;

use super::buffer_backend::BufferBackend;
use super::prefetch;
use super::ringbuffer::RingBuffer;
use crate::decoding::errors::DecodeBufferError;

/// Generic decode-side output buffer parameterised over the storage
/// backend ([`BufferBackend`]). The default `RingBuffer` parameter
/// preserves the historical API for callers that don't want to opt
/// into the flat-buffer fast path.
///
/// Two concrete instantiations are used by the decoder:
/// - `DecodeBuffer<RingBuffer>` — wrap-aware ring (default; the
///   pre-existing decode path).
/// - `DecodeBuffer<FlatBuf>` — non-wrapping Vec-backed fast path,
///   selected by [`super::frame_decoder::FrameDecoder`] (via
///   `DecoderScratchKind`) when the frame's `Single_Segment_flag`
///   is set. The compiler emits a separate monomorphisation per
///   backend so wrap dispatch is eliminated entirely on the flat
///   side at compile time rather than branched at runtime — see
///   backlog item #132.
pub struct DecodeBuffer<B: BufferBackend = RingBuffer> {
    buffer: B,
    /// Active dictionary, held by shared handle (`Arc`/`Rc`) rather than a
    /// per-frame owned copy. `repeat_from_dict` reads match bytes straight
    /// out of the handle's content (donor `ZSTD_refDDict` semantics): one
    /// dictionary copy is shared across every frame AND across decoder
    /// instances on other threads (`Arc<Dictionary>` is `Send + Sync`), so
    /// reusing a dictionary costs a refcount bump, never a content memcpy.
    /// `None` = no dictionary (a no-dict frame can never read a stale one).
    pub(crate) dict: Option<crate::decoding::dictionary::DictionaryHandle>,

    pub window_size: usize,
    total_output_counter: u64,
    #[cfg(feature = "hash")]
    pub hash: twox_hash::XxHash64,
    /// Whether drain hashes the bytes it emits into `hash`. `false` lets a
    /// `FrameDecoder` set to [`ContentChecksum::None`](crate::decoding::ContentChecksum::None)
    /// skip the XXH64 pass on the streaming path. Persists across `reset`
    /// (it mirrors a decoder-level setting, not per-frame state); the frame
    /// layer re-applies it before each decode.
    #[cfg(feature = "hash")]
    compute_hash: bool,
}

/// Rollback token produced by [`DecodeBuffer::checkpoint`].
///
/// Snapshots tail / counter / cap. Hash state is NOT snapshotted:
/// no mutation site between `checkpoint()` and the matched
/// `try_restore_checkpoint()` writes to `self.hash`:
///   * `push` and `extend_and_fill` only advance
///     `total_output_counter`.
///   * The inline sequence executor writes through `buffer_mut()`
///     directly, bypassing the wrapper-level
///     `total_output_counter` entirely (`UserSliceBackend::tail`
///     carries the byte count on that path; hashing is deferred to
///     the final full-slice pass in `FrameDecoder::decode_all`).
///   * `drain_to` / `read` DO write hash, but they run BETWEEN
///     blocks, never inside the fused sequence loop the checkpoint
///     guards.
#[derive(Copy, Clone)]
pub(crate) struct DecodeBufferCheckpoint {
    tail: usize,
    total_output_counter: u64,
    cap: usize,
}

impl<B: BufferBackend> Read for DecodeBuffer<B> {
    fn read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        let max_amount = self.can_drain_to_window_size().unwrap_or(0);
        let amount = max_amount.min(target.len());

        let mut written = 0;
        self.drain_to(amount, |buf| {
            target[written..][..buf.len()].copy_from_slice(buf);
            written += buf.len();
            (buf.len(), Ok(()))
        })?;
        Ok(amount)
    }
}

impl<B: BufferBackend> DecodeBuffer<B> {
    pub fn new(window_size: usize) -> DecodeBuffer<B> {
        DecodeBuffer {
            buffer: B::new(),
            dict: None,
            window_size,
            total_output_counter: 0,
            #[cfg(feature = "hash")]
            hash: twox_hash::XxHash64::with_seed(0),
            #[cfg(feature = "hash")]
            compute_hash: true,
        }
    }

    /// Wrap a pre-constructed backend (e.g. `FlatBuf::with_capacity`
    /// sized for a single-segment frame) into a `DecodeBuffer`. Used
    /// by `FrameDecoder` (via `DecoderScratchKind::new_flat`) to
    /// supply a `FlatBuf` pre-sized for `frame_content_size` —
    /// the default `new()` constructor would otherwise produce a
    /// zero-capacity backend and force a realloc on the first push.
    ///
    /// Calls `buffer.clear()` so the logical counters (set to zero
    /// here) are not inconsistent with a physically-non-empty backend
    /// the caller might have handed in. On a fresh backend (the only
    /// real call shape today) `clear()` is a no-op — the two stores
    /// it issues vanish in the per-frame reset noise.
    pub fn from_backend(mut buffer: B, window_size: usize) -> DecodeBuffer<B> {
        buffer.clear();
        DecodeBuffer {
            buffer,
            dict: None,
            window_size,
            total_output_counter: 0,
            #[cfg(feature = "hash")]
            hash: twox_hash::XxHash64::with_seed(0),
            #[cfg(feature = "hash")]
            compute_hash: true,
        }
    }

    /// Enable or disable the drain-time XXH64 pass. Set by the frame layer
    /// from the decoder's [`ContentChecksum`](crate::decoding::ContentChecksum)
    /// mode before each decode (`false` for `None`).
    #[cfg(feature = "hash")]
    #[inline]
    pub(crate) fn set_compute_hash(&mut self, compute: bool) {
        self.compute_hash = compute;
    }

    /// Arm the per-block decompressed-output ceiling for the block about to
    /// be decoded: the growable backend is told it may grow only up to
    /// `current_len + max_block_output`, so a match that would push this
    /// block's output past `MAX_BLOCK_SIZE` fails its `try_reserve` on the
    /// cold growth path instead of growing the ring to gigabytes (a
    /// decompression-bomb OOM) before the post-block validity check runs.
    /// Called once per block by the sequence decoder, before the sequence
    /// loop. The growable backends (`RingBuffer`, `FlatBuf`) enforce it in
    /// `try_reserve`; fixed-capacity backends (`UserSliceBackend`) are already
    /// bounded and take the trait's no-op default.
    #[inline]
    pub(crate) fn set_block_output_ceiling(&mut self, max_block_output: usize) {
        // Plain add (not saturating): `len()` is the bytes decoded so far and
        // `max_block_output` is one block's `MAX_BLOCK_SIZE` (128 KiB), so the
        // sum is nowhere near `usize::MAX` — overflow is unreachable. Saturating
        // would silently turn the ceiling into `usize::MAX` (no guard at all)
        // if that invariant were ever broken, masking the bug instead of
        // tripping the debug overflow check at its cause.
        let ceiling = self.buffer.len() + max_block_output;
        self.buffer.set_max_capacity(ceiling);
    }

    pub fn reset(&mut self, window_size: usize) {
        self.window_size = window_size;
        self.buffer.clear();
        // No reserve here: capacity decisions are pushed up to the frame
        // layer. Direct-decode frames (`run_direct_decode`) write through
        // `UserSliceBackend` and never touch this buffer, so a long-lived
        // `FrameDecoder` reused across direct-eligible frames pays zero
        // allocation for the window. The non-direct path is pre-reserved
        // by `FrameDecoder::decode_all_impl` / `decode_blocks` via
        // `DecoderScratchKind::reserve_buffer(window_size)` before any
        // block writes — that is the only call site that knows whether
        // the frame will actually hit this buffer.
        self.dict = None;
        self.total_output_counter = 0;
        // Lift the per-block growth ceiling between frames; the sequence
        // decoder re-arms it per block. Non-block callers stay unbounded.
        self.buffer.set_max_capacity(usize::MAX);
        #[cfg(feature = "hash")]
        {
            self.hash = twox_hash::XxHash64::with_seed(0);
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Allocated byte capacity of the backing buffer (the decode window).
    /// Backs the workspace-footprint reporting; the value is the backend's
    /// `cap()` (RingBuffer's ring-indexing capacity / FlatBuf's `Vec`
    /// capacity), so it tracks the real heap reservation.
    pub fn capacity(&self) -> usize {
        self.buffer.cap()
    }

    /// Active dictionary content bytes, borrowed through the shared handle
    /// (no copy). Empty slice when no dictionary is attached.
    #[inline]
    pub(crate) fn dict_content(&self) -> &[u8] {
        match &self.dict {
            Some(h) => &h.as_dict().dict_content,
            None => &[],
        }
    }

    /// Attach a dictionary by shared handle. This is a refcount bump
    /// (`Arc`/`Rc` clone) — the dictionary content is shared, never copied,
    /// so the same dictionary is free to reuse across frames and across
    /// decoder instances on other threads.
    #[inline]
    pub(crate) fn set_dict(&mut self, handle: crate::decoding::dictionary::DictionaryHandle) {
        self.dict = Some(handle);
    }

    /// Return the last `n` bytes of the visible buffer as two
    /// contiguous slices (`(s1, s2)` matching the wrap semantics of
    /// the underlying backend). `n` must be `<= self.len()`. Used by
    /// the per-block checksum path to hash bytes that were appended
    /// during the most recent block decode without copying.
    ///
    /// Returns empty slices if `n == 0`.
    #[cfg(all(feature = "lsm", feature = "hash"))]
    pub(crate) fn last_n_as_slices(&self, n: usize) -> (&[u8], &[u8]) {
        let (s1, s2) = self.buffer.as_slices();
        let total = s1.len() + s2.len();
        debug_assert!(n <= total);
        let start = total - n;
        if start >= s1.len() {
            (&[][..], &s2[start - s1.len()..])
        } else {
            (&s1[start..], s2)
        }
    }

    /// Capture a rollback point covering the buffer's write cursor and the
    /// total-output counter. Pair with [`restore_checkpoint`] to undo
    /// speculative pushes/repeats made after the capture — used by the fused
    /// sequence executor to roll back when the post-loop bitstream
    /// validation rejects a malformed block, restoring the
    /// transactional-on-error semantics the legacy two-pass pipeline had.
    #[inline]
    pub(crate) fn checkpoint(&self) -> DecodeBufferCheckpoint {
        DecodeBufferCheckpoint {
            tail: self.buffer.tail(),
            total_output_counter: self.total_output_counter,
            cap: self.buffer.cap(),
        }
    }

    /// Attempt to restore a checkpoint captured by [`checkpoint`].
    ///
    /// Returns `true` if the rollback was performed; `false` if an
    /// intervening reallocation invalidated the captured tail index
    /// (no state is mutated in that case).
    ///
    /// On a well-formed zstd block the upfront `reserve(MAX_BLOCK_SIZE)`
    /// rules out reallocation, so this returns `true` on the hot path.
    /// On a malformed block whose sequence section decodes past
    /// `MAX_BLOCK_SIZE`, `RingBuffer::reserve_amortized` compacts the
    /// buffer (head=0, tail=s1+s2) and the captured tail index becomes
    /// meaningless — `false` is returned and the caller surfaces a
    /// normal decode `Err` instead of restoring stale state. Reaching
    /// this branch implies the frame is already corrupt; the partial
    /// data left in the buffer is discarded by the `Err` return.
    #[inline]
    pub(crate) fn try_restore_checkpoint(&mut self, cp: DecodeBufferCheckpoint) -> bool {
        if self.buffer.cap() != cp.cap {
            return false;
        }
        // SAFETY: cap-equality above proves the underlying allocation
        // has not been reseated, so the captured `tail` still refers to
        // the same logical and physical position. The caller is also
        // responsible for treating any bytes between the captured tail
        // and the current tail as discarded.
        unsafe { self.buffer.set_tail(cp.tail) };
        self.total_output_counter = cp.total_output_counter;
        // No hash restore: see `DecodeBufferCheckpoint` doc. No
        // mutation site between `checkpoint()` and this call writes
        // to `self.hash` (drain runs between blocks, not inside the
        // fused sequence loop; the inline sequence executor bypasses
        // the wrapper counter entirely via `buffer_mut()`, leaving
        // hashing for the post-block full-slice pass).
        true
    }

    /// Pre-allocate capacity for `amount` additional bytes ahead of a batch
    /// of `push`/`repeat` operations, growing exactly (see
    /// `BufferBackend::reserve_exact`): every call site is a one-shot
    /// window-scale reservation where amortized doubling would hold up to
    /// 2x the window.
    #[inline]
    pub fn reserve_exact(&mut self, amount: usize) {
        self.buffer.reserve_exact(amount);
    }

    /// Mutable backend handle. Lets the inline sequence executor
    /// write straight into the backend's physical storage; the
    /// `tail()` cursor on the backend is the authoritative output
    /// length, so no separate buffer-level counter update is needed.
    /// Crate-internal; gated to the
    /// `BufferBackend::SUPPORTS_INLINE_SEQUENCE_EXEC = true` dispatch
    /// site.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn buffer_mut(&mut self) -> &mut B {
        &mut self.buffer
    }

    /// Immutable backend handle. `run_direct_decode`'s post-block FCS
    /// check reads `tail()` straight from the backend rather than
    /// going through `total_output_counter`: the inline sequence
    /// executor (see
    /// `sequence_section_decoder::execute_one_sequence_pipelined`)
    /// writes directly through `buffer_mut`, so the
    /// `total_output_counter` field on the wrapper is not maintained
    /// on that path and `tail()` is the only accurate output length.
    #[inline(always)]
    pub(crate) fn buffer_ref(&self) -> &B {
        &self.buffer
    }

    /// Fill `fill_length` bytes of the output with the literal `fill_with`,
    /// advancing the ringbuffer cursor in place. Used by the RLE block path
    /// (upstream commit `fbc1f2ca`) so the decoder doesn't need a stack
    /// scratch buffer to materialise repeated bytes before pushing them.
    /// Mirrors `push`'s `total_output_counter` bookkeeping so
    /// dictionary-repeat validation in `repeat_from_dict` stays accurate
    /// after RLE blocks.
    pub fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        self.buffer.extend_and_fill(fill_with, fill_length);
        self.total_output_counter += fill_length as u64;
    }

    /// Read `fill_length` bytes from `read` directly into the ringbuffer's
    /// free slots. Used by the Raw block path (upstream commit `29a56160`)
    /// so the decoder doesn't need a 128 KB stack scratch buffer to stage
    /// each chunk before pushing it. Mirrors `push`'s
    /// `total_output_counter` bookkeeping — only after the read succeeds,
    /// so an EOF/IO error leaves the counter (and `tail`) unchanged.
    pub fn extend_from_reader<R: Read>(
        &mut self,
        read: R,
        fill_length: usize,
    ) -> Result<(), crate::io::Error> {
        self.buffer.extend_from_reader(read, fill_length)?;
        self.total_output_counter += fill_length as u64;
        Ok(())
    }

    #[inline]
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend(data);
        self.total_output_counter += data.len() as u64;
    }

    /// Add `n` to the cumulative produced-byte counter for output produced
    /// outside `push` / `repeat` — namely the inline `exec_sequence_inline`
    /// path, which writes through the backend directly and so bypasses the
    /// counter those methods maintain. Called by the sequence dispatch only
    /// for backends whose `INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER` is `true`
    /// (`RingBuffer` / `FlatBuf`), keeping `total_output()` (resume
    /// `output_offset`) and the dict-reachability gate accurate.
    #[inline(always)]
    pub(crate) fn advance_output_counter(&mut self, n: u64) {
        self.total_output_counter += n;
    }

    /// Fallible variant of [`Self::push`]. Returns `Err(BackendOverflow)`
    /// when the underlying backend's `try_extend` rejects the write
    /// (only possible on fixed-capacity backends like
    /// `UserSliceBackend`). Used by the Raw block fast path on the
    /// direct-decode pipeline so a malformed Raw block whose declared
    /// `Block_Size` exceeds the caller's output slice surfaces as a
    /// structured error instead of panicking. Compressed-block
    /// sequence execution is a follow-up.
    #[inline(always)]
    pub fn try_push(&mut self, data: &[u8]) -> Result<(), super::buffer_backend::BackendOverflow> {
        self.buffer.try_extend(data)?;
        self.total_output_counter += data.len() as u64;
        Ok(())
    }

    /// Fallible variant of [`Self::extend_and_fill`]. Same contract
    /// as [`Self::try_push`].
    #[inline]
    pub fn try_extend_and_fill(
        &mut self,
        fill_with: u8,
        fill_length: usize,
    ) -> Result<(), super::buffer_backend::BackendOverflow> {
        self.buffer.try_extend_and_fill(fill_with, fill_length)?;
        self.total_output_counter += fill_length as u64;
        Ok(())
    }

    pub fn repeat(&mut self, offset: usize, match_length: usize) -> Result<(), DecodeBufferError> {
        self.repeat_inner::<false>(offset, match_length)
    }

    /// Same as [`repeat`] but the caller asserts a lookahead
    /// prefetch was already issued for this match source ADVANCE
    /// iterations ago, so the in-loop `prefetch_match_source` would
    /// be redundant issue-port pressure on top of the L1 line that's
    /// by now warm. Per-call `reserve` is KEPT — on malformed input
    /// the `extend_from_within_unchecked*` writes assume the buffer
    /// has the required free capacity (only `debug_assert` checks in
    /// release), and a single missing reserve here would turn a
    /// fuzz-corrupt block into out-of-bounds UB. The reserve is
    /// already amortised by the caller's upfront
    /// `reserve(MAX_BLOCK_SIZE)`, so this is a cheap capacity-check
    /// branch, not a real allocation. Used exclusively by the
    /// pipelined sequence executor in
    /// [`crate::decoding::sequence_section_decoder`].
    #[inline(always)]
    pub(crate) fn repeat_lookahead_prefetched(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), DecodeBufferError> {
        self.repeat_inner::<true>(offset, match_length)
    }

    #[inline(always)]
    fn repeat_inner<const SKIP_PREFETCH: bool>(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), DecodeBufferError> {
        if offset == 0 {
            return Err(DecodeBufferError::ZeroOffset);
        }

        if match_length == 0 {
            return Ok(());
        }

        // The per-block decompression-bomb ceiling is NOT checked here on
        // every match (that compare bloated the inlined hot loop). It is
        // enforced once on the cold growth path: `set_block_output_ceiling`
        // lowers the backend's `max_capacity` per block, so the
        // `try_reserve` below rejects an over-producing match exactly when
        // it would have to grow the buffer past the ceiling — bounding the
        // OOM on every target while a well-formed block (covered by the
        // upfront `reserve(MAX_BLOCK_SIZE)`) never reaches the check.

        if offset > self.buffer.len() {
            self.repeat_from_dict(offset, match_length)
        } else {
            let buf_len = self.buffer.len();
            let start_idx = buf_len - offset;
            let end_idx = start_idx + match_length;

            // Reserve unconditionally — `extend_from_within_unchecked*`
            // assumes the required free capacity exists; skipping it
            // would turn a malformed block (match_length past the
            // upfront `reserve(MAX_BLOCK_SIZE)`) into release-build
            // UB. The fallible variant surfaces a structured error for
            // both fixed-capacity backends (`UserSliceBackend`: write
            // past the user's slice) and the growable `RingBuffer` (a
            // grow past the per-block `max_capacity` ceiling — the
            // decompression-bomb guard). On the common path the upfront
            // `reserve(MAX_BLOCK_SIZE)` already covers the write, so this
            // is a cheap capacity check, not an allocation.
            self.buffer.try_reserve(match_length).map_err(|o| {
                DecodeBufferError::OutputBufferOverflow {
                    tail: o.tail,
                    requested: o.requested,
                    capacity: o.capacity,
                }
            })?;

            // Record the copy-shape histogram only after the reserve
            // succeeds: on `OutputBufferOverflow` the repeat never runs, so
            // counting it here would inflate the diagnostic with match
            // traffic that was never materialised.
            #[cfg(feature = "copy_shape_stats")]
            crate::decoding::simd_copy::shape_stats::record_repeat(
                offset,
                match_length,
                end_idx > buf_len,
            );

            if !SKIP_PREFETCH {
                self.prefetch_match_source(start_idx, match_length);
            }
            if end_idx > buf_len {
                self.repeat_overlapping(offset, match_length, start_idx);
            } else {
                // SAFETY: start_idx + match_length <= self.buffer.len()
                // (start_idx = buf_len - offset, end_idx = start_idx +
                // match_length, end_idx <= buf_len). The `reserve`
                // above guarantees the destination has enough free
                // capacity for `match_length` more bytes.
                unsafe {
                    if offset >= 16 && use_branchless_wildcopy() {
                        self.buffer
                            .extend_from_within_unchecked_branchless(start_idx, match_length);
                    } else {
                        self.buffer
                            .extend_from_within_unchecked(start_idx, match_length);
                    }
                };
            }

            self.total_output_counter += match_length as u64;
            Ok(())
        }
    }

    #[inline(always)]
    fn repeat_overlapping(&mut self, offset: usize, match_length: usize, start_idx: usize) {
        if offset >= 16 {
            self.repeat_in_chunks(offset, match_length, start_idx, use_branchless_wildcopy());
        } else if offset >= 8 {
            self.repeat_in_chunks(offset, match_length, start_idx, false);
        } else {
            self.repeat_short_offset(offset, match_length, start_idx);
        }
    }

    /// Materialise an overlapping match (`offset < match_length`) by
    /// exponential doubling rather than fixed `offset`-sized chunks.
    ///
    /// The period occupies `[start_idx, start_idx + offset)` and the output
    /// grows from the current tail. Each step copies the *entire* contiguous
    /// run already materialised from the period base — length `buffer.len() -
    /// start_idx` — and appends it directly after itself: src = `[start_idx,
    /// start_idx + n)`, dst = the tail, with `n <= buffer.len() - start_idx`
    /// so `start_idx + n <= tail`. Every copy is therefore a clean
    /// *non-overlapping adjacent* block that satisfies
    /// `extend_from_within_unchecked`'s contract, and `start_idx` never moves.
    ///
    /// Because the materialised run doubles after each step, this emits
    /// `O(log2(match_length / offset))` copies — each as large as the run so
    /// far — instead of `match_length / offset` offset-sized ones. Fewer call
    /// boundaries and larger contiguous copies (better for the hardware
    /// prefetcher / store streaming) while the bytes produced are identical:
    /// the output is periodic with period `offset`, and copying any prefix of
    /// that period onto its own tail preserves the periodicity.
    ///
    /// `#[inline]` (hint, not force): this is the overlapping-match cold-ish
    /// arm of `repeat_inner`. Forcing it inline bloats the hot fully-inlined
    /// sequence executor and measurably shifts codegen of the common
    /// non-overlapping path (a small-match regression on realistic corpora);
    /// the doubling win comes from issuing fewer/larger copies, not from
    /// inlining, so the per-call boundary here is irrelevant next to the
    /// copy work it dispatches.
    #[inline]
    fn repeat_in_chunks(
        &mut self,
        offset: usize,
        match_length: usize,
        start_idx: usize,
        use_branchless_copy: bool,
    ) {
        debug_assert!(offset >= 8, "doubling path expects offset >= 8");
        let mut remaining = match_length;
        while remaining > 0 {
            // Contiguous, already-correct run from the period base. Since
            // `extend_from_within` keeps `start_idx` fixed and appends at the
            // tail, the run length is exactly `tail - start_idx`; it starts at
            // `offset` and doubles each iteration until capped by `remaining`.
            let run = self.buffer.len() - start_idx;
            let n = usize::min(run, remaining);

            // SAFETY: `n <= run = buffer.len() - start_idx` gives
            // `start_idx + n <= buffer.len()` (the tail / dst start), so the
            // source `[start_idx, start_idx + n)` and the destination at the
            // tail are adjacent and non-overlapping. `repeat()` reserved
            // `match_length` destination capacity up front.
            unsafe {
                if use_branchless_copy {
                    self.buffer
                        .extend_from_within_unchecked_branchless(start_idx, n);
                } else {
                    self.buffer.extend_from_within_unchecked(start_idx, n);
                }
            };
            remaining -= n;
        }
    }

    #[inline(always)]
    fn repeat_short_offset(&mut self, offset: usize, match_length: usize, start_idx: usize) {
        debug_assert!(
            offset > 0,
            "offset must be non-zero to avoid modulo by zero in short-offset path"
        );

        // Read the repeating period (`offset` bytes) from the existing
        // buffer surface. Cap the read at 7 so callers with offset > 7
        // never reach this function — `repeat_overlapping` dispatches
        // the offset >= 8 cases elsewhere.
        debug_assert!(offset <= 7, "repeat_short_offset is the offset<8 path");
        let mut base = [0u8; 7];
        for (i, slot) in base.iter_mut().take(offset).enumerate() {
            *slot = self.byte_at(start_idx + i);
        }

        // Fast path: offset ∈ {1, 2, 4} — the period divides 16, so
        // every 16-byte window of the repeating pattern is identical
        // and one pre-built chunk feeds the entire loop with zero
        // phase tracking. Inner loop = one 16-byte SIMD store + one
        // add.
        //
        // The chunk-build is materialised with literal constants per
        // arm rather than `chunk16[i] = base[i % offset]`. The naive
        // form gets unrolled by LLVM into 14×`divb` (8-bit divides)
        // because the compiler does not propagate `offset == 1|2|4`
        // from the outer match arm into the inner loop's modulo —
        // divb cost ~1% per byte = ~14% of decode time on
        // `decompress/level_-1_fast/decodecorpus-z000033`. Explicit
        // literal arms eliminate the divide entirely.
        if matches!(offset, 1 | 2 | 4) {
            let b0 = base[0];
            let b1 = base[1];
            let b2 = base[2];
            let b3 = base[3];
            let chunk16: [u8; 16] = match offset {
                1 => [b0; 16],
                2 => [
                    b0, b1, b0, b1, b0, b1, b0, b1, b0, b1, b0, b1, b0, b1, b0, b1,
                ],
                4 => [
                    b0, b1, b2, b3, b0, b1, b2, b3, b0, b1, b2, b3, b0, b1, b2, b3,
                ],
                // SAFETY: outer `matches!(offset, 1 | 2 | 4)` rejects
                // any other value; this arm is statically dead and
                // exists only to satisfy match exhaustiveness without
                // a runtime branch.
                _ => unsafe { core::hint::unreachable_unchecked() },
            };
            let mut copied = 0usize;
            while copied + 16 <= match_length {
                self.buffer.extend(&chunk16);
                copied += 16;
            }
            if copied < match_length {
                let tail = match_length - copied;
                self.buffer.extend(&chunk16[..tail]);
            }
            return;
        }

        // offset ∈ {3, 5, 6, 7}: 8-byte phase-pattern path. Each phase
        // is the 8-byte view of the repeating period starting at that
        // sub-position; advancing the cursor by 8 bytes shifts the
        // phase by `8 % offset` (mod offset).
        //
        // A 16-byte version (LCM(offset, 16) ∈ {48, 80, 48, 112}) was
        // measured on Intel i9-9900K — the doubled inner-loop store
        // width was offset by a 7×16 = 112-byte phase-pattern setup
        // cost (2× the 8-byte setup). On `decodecorpus-z000033`
        // short-offset matches are short enough that setup dominates
        // total cost, so the 16-byte version was a net regression on
        // every level except `level_1_fast` (where it broke even). The
        // 8-byte path retained here keeps the setup small (7×8 = 56 B)
        // and is the fastest measured option for these offsets on
        // realistic input.
        let mut phase_patterns = [[0u8; 8]; 7];
        for phase in 0..offset {
            for i in 0..8 {
                phase_patterns[phase][i] = base[(phase + i) % offset];
            }
        }

        let phase_step = 8 % offset;
        let mut phase = 0usize;
        let mut copied = 0usize;
        while copied + 8 <= match_length {
            self.buffer.extend(&phase_patterns[phase]);
            copied += 8;
            phase = (phase + phase_step) % offset;
        }

        if copied < match_length {
            let tail = match_length - copied;
            self.buffer.extend(&phase_patterns[phase][..tail]);
        }
    }

    #[inline(always)]
    fn byte_at(&self, idx: usize) -> u8 {
        let (s1, s2) = self.buffer.as_slices();
        if idx < s1.len() {
            s1[idx]
        } else {
            s2[idx - s1.len()]
        }
    }

    #[inline(always)]
    fn prefetch_match_source(&self, start_idx: usize, match_length: usize) {
        if match_length < 64 {
            return;
        }
        let (s1, s2) = self.buffer.as_slices();
        if start_idx < s1.len() {
            prefetch::prefetch_slice_t1(&s1[start_idx..]);
        } else {
            let idx = start_idx - s1.len();
            if idx < s2.len() {
                prefetch::prefetch_slice_t1(&s2[idx..]);
            }
        }
    }

    /// Lookahead-friendly prefetch issued ahead of execute. The
    /// in-loop `prefetch_match_source` above fires at the moment of
    /// the copy, so it can't hide DRAM latency for cold long-distance
    /// match sources. Pipelined callers compute the match source
    /// logical index 3-4 sequences in advance and call this helper —
    /// by the time the corresponding `repeat()` reaches the actual
    /// load, the line is already in-flight.
    ///
    /// `start_idx` is a logical index into the current buffer (same
    /// frame as `buffer.len()`). Indices outside `[0, buffer.len())`
    /// are silently dropped — the cases this guards against include
    /// intra-block self-overlap (source falls past the not-yet-
    /// written cursor), `wrapping_sub` underflow on a caller that
    /// computed `match_start - offset` with an offset larger than
    /// match_start (e.g. a stale or malformed sequence), and
    /// dictionary-sourced matches whose logical position predates
    /// the buffer's current frame. The donor (`PREFETCH_L1` in
    /// `ZSTD_prefetchMatch` — we mirror that with `prefetch_slice`
    /// → `_MM_HINT_T0` / `pldl1keep`, see the body comment) tolerates
    /// invalid addresses by spec, but in
    /// safe Rust the cheapest equivalent is to bound-check the
    /// logical position before chasing the slice.
    #[inline(always)]
    pub(crate) fn prefetch_lookahead_match_source(&self, start_idx: usize) {
        if start_idx >= self.buffer.len() {
            return;
        }
        // Donor's `ZSTD_prefetchMatch` issues two `PREFETCH_L1` hints
        // per match — one at `match`, one at `match + CACHELINE_SIZE`.
        // We mirror that intent via `prefetch_slice` (`_MM_HINT_T0` on
        // x86 / `pldl1keep` on aarch64 → L1 destination) with extent
        // capped at 2 × 64 B = 128 B. In the contiguous case the helper
        // emits at most two prefetch instructions, matching donor
        // exactly. In the wrap-boundary case the same 128 B budget is
        // split across `s1_tail` and `s2[0..]`, which can emit up to
        // four cache-line prefetches total (two per slice when each
        // side covers a full 64 B) — still bounded, still L1, still
        // less than the helper's MAX_LINES = 4 ceiling. The lookahead
        // depth (ADVANCE) is small enough that L1 should hold the line
        // across the gap; if profiling later shows L1 eviction
        // pressure we can revisit T1/L2.
        const PREFETCH_EXTENT: usize = 128;
        const CACHE_LINE: usize = 64;
        let (s1, s2) = self.buffer.as_slices();
        if start_idx < s1.len() {
            let s1_tail = &s1[start_idx..];
            let s1_bound = core::cmp::min(s1_tail.len(), PREFETCH_EXTENT);
            // `prefetch_slice` no-ops on slices shorter than one cache
            // line — sensible for bulk prefetch, but wrong for the
            // wrap-boundary case where the cache line containing
            // `start_idx` IS the line we need warmed even if the
            // remaining contiguous extent is < 64 B. Fall back to the
            // single-line variant in that case so the match-start
            // line is always hinted.
            if s1_bound >= CACHE_LINE {
                prefetch::prefetch_slice(&s1_tail[..s1_bound]);
            } else {
                prefetch::prefetch_first_line_l1(&s1_tail[..s1_bound]);
            }
            // Wrap continuation: when the match source straddles the
            // s1/s2 boundary and the s1 tail is shorter than the
            // PREFETCH_EXTENT we asked for, top up the rest from
            // s2[0..]. Without this the donor's "up to two cache
            // lines" intent silently collapses to one (or zero if
            // s1_tail is the last sub-line of s1).
            if s1_bound < PREFETCH_EXTENT {
                let remaining = PREFETCH_EXTENT - s1_bound;
                let s2_bound = core::cmp::min(s2.len(), remaining);
                if s2_bound >= CACHE_LINE {
                    prefetch::prefetch_slice(&s2[..s2_bound]);
                } else if s2_bound > 0 {
                    prefetch::prefetch_first_line_l1(&s2[..s2_bound]);
                }
            }
        } else {
            // `start_idx < self.buffer.len()` from the early return,
            // `buffer.len() == s1.len() + s2.len()`, and the else
            // branch establishes `start_idx >= s1.len()`. So
            // `idx = start_idx - s1.len() < s2.len()` by construction
            // — no explicit `idx < s2.len()` guard needed.
            let idx = start_idx - s1.len();
            let tail = &s2[idx..];
            let bound = core::cmp::min(tail.len(), PREFETCH_EXTENT);
            if bound >= CACHE_LINE {
                prefetch::prefetch_slice(&tail[..bound]);
            } else {
                prefetch::prefetch_first_line_l1(&tail[..bound]);
            }
        }
    }

    #[cold]
    fn repeat_from_dict(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), DecodeBufferError> {
        // Reachability gate: dict-source matches are only valid while
        // the dictionary content is still inside the visible window.
        // `RingBuffer` / `FlatBuf` maintain `total_output_counter`
        // (push / repeat / inline-exec counter bump). On the direct
        // path (`UserSliceBackend`) the inline executor skips the
        // counter, but the backend never drains (`head` stays 0), so
        // `buffer.len()` IS the cumulative output — take the max of
        // the two so the gate is accurate for every backend.
        let total_output = self.total_output_counter.max(self.buffer.len() as u64);
        if total_output <= self.window_size as u64 {
            // at least part of that repeat is from the dictionary content
            let bytes_from_dict = offset - self.buffer.len();

            // Borrow the dictionary content through the shared handle as a
            // field access (`self.dict`), kept disjoint from the `self.buffer`
            // mutation below so the borrow checker allows the read+extend
            // without an intermediate copy. `None` → empty slice → the
            // length guard rejects (matches the old empty-`dict_content`
            // behaviour on the direct/no-dict path).
            let dict_content: &[u8] = match &self.dict {
                Some(h) => &h.as_dict().dict_content,
                None => &[],
            };
            let dict_len = dict_content.len();

            if bytes_from_dict > dict_len {
                return Err(DecodeBufferError::NotEnoughBytesInDictionary {
                    got: dict_len,
                    need: bytes_from_dict,
                });
            }

            // Enforce the per-block bomb ceiling on the dictionary-backed
            // output too: the `extend` below appends `dict_slice` directly
            // (the inline `push`/`repeat` guard never runs for it), so without
            // this an over-producing match satisfied from the dictionary could
            // grow past the armed ceiling. Reserving the full `match_length`
            // covers both the dict portion here and the buffer-history
            // remainder the recursive `repeat` appends.
            self.buffer.try_reserve(match_length).map_err(|o| {
                DecodeBufferError::OutputBufferOverflow {
                    tail: o.tail,
                    requested: o.requested,
                    capacity: o.capacity,
                }
            })?;

            if bytes_from_dict < match_length {
                let dict_slice = &dict_content[dict_len - bytes_from_dict..];
                prefetch::prefetch_slice(dict_slice);
                self.buffer.extend(dict_slice);

                self.total_output_counter += bytes_from_dict as u64;
                return self.repeat(self.buffer.len(), match_length - bytes_from_dict);
            } else {
                let low = dict_len - bytes_from_dict;
                let high = low + match_length;
                let dict_slice = &dict_content[low..high];
                prefetch::prefetch_slice(dict_slice);
                self.buffer.extend(dict_slice);
                self.total_output_counter += match_length as u64;
            }
            Ok(())
        } else {
            Err(DecodeBufferError::OffsetTooBig {
                offset,
                buf_len: self.buffer.len(),
            })
        }
    }

    /// Check if and how many bytes can currently be drawn from the buffer
    pub fn can_drain_to_window_size(&self) -> Option<usize> {
        if self.buffer.len() > self.window_size {
            Some(self.buffer.len() - self.window_size)
        } else {
            None
        }
    }

    //How many bytes can be drained if the window_size does not have to be maintained
    pub fn can_drain(&self) -> usize {
        self.buffer.len()
    }

    /// Drain as much as possible while retaining enough so that decoding si still possible with the required window_size
    /// At best call only if can_drain_to_window_size reports a 'high' number of bytes to reduce allocations
    pub fn drain_to_window_size(&mut self) -> Option<Vec<u8>> {
        //TODO investigate if it is possible to return the std::vec::Drain iterator directly without collecting here
        match self.can_drain_to_window_size() {
            None => None,
            Some(can_drain) => {
                let mut vec = Vec::with_capacity(can_drain);
                self.drain_to(can_drain, |buf| {
                    vec.extend_from_slice(buf);
                    (buf.len(), Ok(()))
                })
                .ok()?;
                Some(vec)
            }
        }
    }

    pub fn drain_to_window_size_writer(&mut self, mut sink: impl Write) -> Result<usize, Error> {
        match self.can_drain_to_window_size() {
            None => Ok(0),
            Some(can_drain) => self.drain_to(can_drain, |buf| write_all_bytes(&mut sink, buf)),
        }
    }

    /// Advance the backend's head past any bytes beyond `window_size`
    /// without producing them to a sink — the bytes remain physically
    /// present (the backend's allocation never shrinks), but they are
    /// no longer visible through [`Self::len`] / `as_slices` /
    /// `repeat`. Used by the direct-decode path on multi-segment
    /// frames where the caller's output IS the buffer, so the bytes
    /// don't need to be drained anywhere — they just need to drop
    /// out of `len()` so the offset-bound match validation
    /// (`offset <= buffer.len()`) coincides with the spec's
    /// window-size rule (`offset <= window_size`).
    ///
    /// Does NOT update the rolling content checksum. On the direct
    /// path the caller (`FrameDecoder::decode_all`) hashes the
    /// final `output[..content_size]` slice ONCE at end of decode
    /// (single sequential xxhash pass over cache-hot data) and
    /// propagates the digest into the persistent scratch's hasher.
    /// Hashing inside `drop_to_window_size` would re-hash the same
    /// bytes per block (this method runs once per block on
    /// multi-segment frames), which is wasted work — the end-of-
    /// decode walk covers the entire output uniformly.
    ///
    /// Returns the number of bytes whose visibility was discarded.
    ///
    /// Does NOT mutate `total_output_counter`: that counter tracks
    /// total bytes produced (incremented by `push` / `repeat` /
    /// `extend_and_fill`). Advancing `head` just hides
    /// already-produced bytes from the visible region; counting them
    /// again would double-count and break `repeat_from_dict`'s offset
    /// reachability check.
    pub fn drop_to_window_size(&mut self) -> usize {
        match self.can_drain_to_window_size() {
            None => 0,
            Some(can_drop) => {
                self.buffer.drop_first_n(can_drop);
                can_drop
            }
        }
    }

    /// Drop the first `n` visible bytes from the front without producing
    /// them to any sink (advances the head, like the beyond-window drop in
    /// [`Self::drop_to_window_size`] but for an exact count). Used by the
    /// subset partial-decode path to discard the window-context bytes of the
    /// skipped leading blocks once every in-range block is decoded and match
    /// resolution is complete: they were retained only to back the in-range
    /// blocks' match copies, never to be emitted. Does NOT mutate
    /// `total_output_counter` (same rationale as `drop_to_window_size`).
    ///
    /// `n` must be `<= self.len()`.
    #[cfg(feature = "lsm")]
    pub(crate) fn discard_front(&mut self, n: usize) {
        debug_assert!(n <= self.buffer.len());
        if n != 0 {
            self.buffer.drop_first_n(n);
        }
    }

    /// Prime the match window for a resumed partial decode.
    ///
    /// Loads `prefix` (the caller's already-decompressed tail) as the buffer's
    /// initial history so subsequent in-range blocks resolve their match copies
    /// against it without re-decompressing the skipped prefix, and sets the
    /// produced-byte counter to `total_output` (the true cumulative
    /// decompressed length before the resume block). The counter governs the
    /// `repeat_from_dict` reachability gate, so setting it to the real value —
    /// not just `prefix.len()` — keeps a dictionary-backed frame's match
    /// resolution byte-identical to a non-resumed decode, and closes the
    /// dictionary off for mid-frame resumes where it is out of window.
    ///
    /// The caller must have already capped `prefix` to the last `window_size`
    /// bytes (only those can ever back a match) and validated its length.
    #[cfg(feature = "lsm")]
    pub(crate) fn prime_window(&mut self, prefix: &[u8], total_output: u64) {
        self.buffer.extend(prefix);
        self.total_output_counter = total_output;
    }

    /// Total decompressed bytes produced so far. Incremented by
    /// `push`/`repeat`/`extend_and_fill`; window drops and drains do NOT
    /// decrement it, so it equals the cumulative decompressed length even after
    /// the visible buffer has been bounded to `window_size`.
    #[cfg(feature = "lsm")]
    pub(crate) fn total_output(&self) -> u64 {
        self.total_output_counter
    }

    /// drain the buffer completely
    pub fn drain(&mut self) -> Vec<u8> {
        let (slice1, slice2) = self.buffer.as_slices();
        #[cfg(feature = "hash")]
        if self.compute_hash {
            self.hash.write(slice1);
            self.hash.write(slice2);
        }

        let mut vec = Vec::with_capacity(slice1.len() + slice2.len());
        vec.extend_from_slice(slice1);
        vec.extend_from_slice(slice2);
        self.buffer.clear();
        vec
    }

    pub fn drain_to_writer(&mut self, mut sink: impl Write) -> Result<usize, Error> {
        let write_limit = self.buffer.len();
        self.drain_to(write_limit, |buf| write_all_bytes(&mut sink, buf))
    }

    pub fn read_all(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        let amount = self.buffer.len().min(target.len());

        let mut written = 0;
        self.drain_to(amount, |buf| {
            target[written..][..buf.len()].copy_from_slice(buf);
            written += buf.len();
            (buf.len(), Ok(()))
        })?;
        Ok(amount)
    }

    /// Semantics of write_bytes:
    /// Should dump as many of the provided bytes as possible to whatever sink until no bytes are left or an error is encountered
    /// Return how many bytes have actually been dumped to the sink.
    fn drain_to(
        &mut self,
        amount: usize,
        mut write_bytes: impl FnMut(&[u8]) -> (usize, Result<(), Error>),
    ) -> Result<usize, Error> {
        if amount == 0 {
            return Ok(0);
        }

        struct DrainGuard<'a, B: BufferBackend> {
            buffer: &'a mut B,
            amount: usize,
        }

        impl<B: BufferBackend> Drop for DrainGuard<'_, B> {
            fn drop(&mut self) {
                if self.amount != 0 {
                    self.buffer.drop_first_n(self.amount);
                }
            }
        }

        let mut drain_guard = DrainGuard {
            buffer: &mut self.buffer,
            amount: 0,
        };

        let (slice1, slice2) = drain_guard.buffer.as_slices();
        let n1 = slice1.len().min(amount);
        let n2 = slice2.len().min(amount - n1);

        if n1 != 0 {
            let (written1, res1) = write_bytes(&slice1[..n1]);
            #[cfg(feature = "hash")]
            if self.compute_hash {
                self.hash.write(&slice1[..written1]);
            }
            drain_guard.amount += written1;

            // Apparently this is what clippy thinks is the best way of expressing this
            res1?;

            // Only if the first call to write_bytes was not a partial write we can continue with slice2
            // Partial writes SHOULD never happen without res1 being an error, but lets just protect against it anyways.
            if written1 == n1 && n2 != 0 {
                let (written2, res2) = write_bytes(&slice2[..n2]);
                #[cfg(feature = "hash")]
                if self.compute_hash {
                    self.hash.write(&slice2[..written2]);
                }
                drain_guard.amount += written2;

                // Apparently this is what clippy thinks is the best way of expressing this
                res2?;
            }
        }

        let amount_written = drain_guard.amount;
        // Make sure we don't accidentally drop `DrainGuard` earlier.
        drop(drain_guard);

        Ok(amount_written)
    }
}

/// Like Write::write_all but returns partial write length even on error
fn write_all_bytes(mut sink: impl Write, buf: &[u8]) -> (usize, Result<(), Error>) {
    let mut written = 0;
    while written < buf.len() {
        match sink.write(&buf[written..]) {
            Ok(0) => return (written, Ok(())),
            Ok(w) => written += w,
            Err(e) => return (written, Err(e)),
        }
    }
    (written, Ok(()))
}

#[inline(always)]
fn use_branchless_wildcopy() -> bool {
    cfg!(any(target_arch = "x86", target_arch = "x86_64"))
}

#[cfg(test)]
mod tests {
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
        buf.set_dict(dict.into_handle());

        // Mimic the inline executor: produce 250 bytes without touching
        // `total_output_counter`, exceeding the 100-byte window.
        BufferBackend::extend(&mut buf.buffer, &[1u8; 250]);
        buf.drop_to_window_size();
        assert_eq!(buf.len(), 100, "visible buffer capped to the window");

        // offset 110 > len 100 reaches 10 bytes into the dictionary;
        // cumulative output (250) already exceeds the window (100), so
        // this must be rejected, not served from the dictionary.
        let result = buf.repeat_from_dict(110, 5);
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
                buffer.repeat(offset, match_length).unwrap();
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
        decode_buf.repeat(10, 90).unwrap();
        let repeats = 1000;
        for _ in 0..repeats {
            assert_eq!(decode_buf.len(), 100);
            decode_buf.repeat(10, 50).unwrap();
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
        decode_buf.repeat(10, 90).unwrap();
        let repeats = 1000;
        for _ in 0..repeats {
            assert_eq!(decode_buf.len(), 100);
            decode_buf.repeat(10, 50).unwrap();
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
            decode_buf.repeat(offset, match_len).unwrap();
            let got = decode_buf.drain();
            let expected = expected_match_expansion(seed, offset, match_len);
            assert_eq!(got, expected, "offset={offset}, match_len={match_len}");
        }
    }

    #[test]
    fn repeat_zero_offset_returns_error() {
        let mut decode_buf = DecodeBuffer::<RingBuffer>::new(1024);
        decode_buf.push(b"abcdef");
        let err = decode_buf.repeat(0, 5).unwrap_err();
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
        let err = decode_buf.repeat(4, 16).unwrap_err(); // 6 + 16 = 22 > 14
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
            .repeat(4, 8)
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
                crate::decoding::dictionary::Dictionary::from_raw_content(
                    1,
                    alloc::vec![0xABu8; 256],
                )
                .unwrap(),
            )
        };

        // Fully-dictionary match: empty buffer, offset reaches into the dict.
        let mut full = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        full.set_dict(dict());
        full.set_block_output_ceiling(8); // max_capacity = 0 + 8 = 8
        let err = full.repeat(200, 100).unwrap_err(); // 0 + 100 > 8, all from dict
        assert!(
            matches!(
                err,
                crate::decoding::errors::DecodeBufferError::OutputBufferOverflow { .. }
            ),
            "fully-dictionary over-producing match must be rejected, got {err:?}"
        );

        // Mixed match: part from dict, remainder from buffer history.
        let mut mixed = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        mixed.set_dict(dict());
        mixed.push(b"abcd"); // len = 4
        mixed.set_block_output_ceiling(8); // max_capacity = 4 + 8 = 12
        let err = mixed.repeat(10, 100).unwrap_err(); // 6 from dict + rest, 4 + 100 > 12
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
        decode_buf.set_dict(
            crate::decoding::dictionary::DictionaryHandle::from_dictionary(
                crate::decoding::dictionary::Dictionary::from_raw_content(
                    1,
                    b"0123456789".to_vec(),
                )
                .unwrap(),
            ),
        );

        decode_buf.repeat(10, 2).unwrap();
        let err = decode_buf.repeat(10, 1).unwrap_err();
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
        decode_buf.repeat(16, 16).unwrap();
        model_repeat(&mut model, 16, 16);

        let drained = decode_buf.drain_to_window_size().unwrap();
        let model_drained = model_drain_to_window(&mut model, window);
        assert_eq!(drained, model_drained);

        let cases = [(3usize, 97usize), (16usize, 64usize), (7usize, 73usize)];
        for (offset, match_len) in cases {
            decode_buf.repeat(offset, match_len).unwrap();
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
                buf.repeat(offset, match_length).unwrap_or_else(|e| {
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
}
