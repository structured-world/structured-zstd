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
    pub dict_content: Vec<u8>,

    pub window_size: usize,
    total_output_counter: u64,
    #[cfg(feature = "hash")]
    pub hash: twox_hash::XxHash64,
}

/// Rollback token produced by [`DecodeBuffer::checkpoint`].
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
            dict_content: Vec::new(),
            window_size,
            total_output_counter: 0,
            #[cfg(feature = "hash")]
            hash: twox_hash::XxHash64::with_seed(0),
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
            dict_content: Vec::new(),
            window_size,
            total_output_counter: 0,
            #[cfg(feature = "hash")]
            hash: twox_hash::XxHash64::with_seed(0),
        }
    }

    pub fn reset(&mut self, window_size: usize) {
        self.window_size = window_size;
        self.buffer.clear();
        self.buffer.reserve(self.window_size);
        self.dict_content.clear();
        self.total_output_counter = 0;
        #[cfg(feature = "hash")]
        {
            self.hash = twox_hash::XxHash64::with_seed(0);
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
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
        true
    }

    /// Pre-allocate capacity for `amount` additional bytes.
    ///
    /// Call this before a batch of `push`/`repeat` operations to avoid
    /// repeated re-allocations inside the hot decode loop.
    #[inline]
    pub fn reserve(&mut self, amount: usize) {
        self.buffer.reserve(amount);
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

    pub fn repeat(&mut self, offset: usize, match_length: usize) -> Result<(), DecodeBufferError> {
        if offset == 0 {
            return Err(DecodeBufferError::ZeroOffset);
        }

        if match_length == 0 {
            return Ok(());
        }

        if offset > self.buffer.len() {
            self.repeat_from_dict(offset, match_length)
        } else {
            let buf_len = self.buffer.len();
            let start_idx = buf_len - offset;
            let end_idx = start_idx + match_length;

            self.buffer.reserve(match_length);
            self.prefetch_match_source(start_idx, match_length);
            if end_idx > buf_len {
                self.repeat_overlapping(offset, match_length, start_idx);
            } else {
                // can just copy parts of the existing buffer
                // SAFETY: Requirements checked:
                // 1. start_idx + match_length must be <= self.buffer.len()
                //      We know that:
                //      1. start_idx = self.buffer.len() - offset
                //      2. end_idx = start_idx + match_length
                //      3. end_idx <= self.buffer.len()
                //      Thus follows: start_idx + match_length <= self.buffer.len()
                //
                // 2. explicitly reserved enough memory for the whole match_length
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

    #[inline(always)]
    fn repeat_in_chunks(
        &mut self,
        offset: usize,
        match_length: usize,
        start_idx: usize,
        use_branchless_copy: bool,
    ) {
        let mut start_idx = start_idx;
        let mut copied_counter_left = match_length;
        while copied_counter_left > 0 {
            let chunksize = usize::min(offset, copied_counter_left);

            // SAFETY: chunksize <= offset keeps each single copy in the currently readable
            // source range, and repeat() reserved enough destination capacity.
            unsafe {
                if use_branchless_copy {
                    self.buffer
                        .extend_from_within_unchecked_branchless(start_idx, chunksize);
                } else {
                    self.buffer
                        .extend_from_within_unchecked(start_idx, chunksize);
                }
            };
            copied_counter_left -= chunksize;
            start_idx += chunksize;
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
        if matches!(offset, 1 | 2 | 4) {
            let mut chunk16 = [0u8; 16];
            for i in 0..16 {
                chunk16[i] = base[i % offset];
            }
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
    /// `ZSTD_prefetchMatch` — note: we route to L2 instead, see the
    /// body comment) tolerates invalid addresses by spec, but in
    /// safe Rust the cheapest equivalent is to bound-check the
    /// logical position before chasing the slice.
    #[inline(always)]
    pub(crate) fn prefetch_lookahead_match_source(&self, start_idx: usize) {
        if start_idx >= self.buffer.len() {
            return;
        }
        // Donor's `ZSTD_prefetchMatch` issues up to two `PREFETCH_L1`
        // hints per match — one at `match`, one at
        // `match + CACHELINE_SIZE`. We use the same two-line cadence
        // but route through `prefetch_slice_t1` (`_MM_HINT_T1` on
        // x86 — L2 destination instead of L1). The deliberate
        // deviation: ADVANCE iterations of decode work sit between
        // the prefetch issue and the matching `repeat()` load, so
        // an L1-bound line would likely be evicted by intervening
        // accesses before the consumer reaches it — L2 has the
        // capacity to keep the line resident across that window.
        // The pre-existing in-loop `prefetch_match_source` makes the
        // same T1 choice for the same reason. Capping the slice
        // extent at 2 × 64 B holds `prefetch_slice_t1` to AT MOST
        // two prefetch lines (it can issue fewer when the source
        // straddles the ring buffer's `as_slices()` boundary and
        // only the head fragment is reachable here, or when the
        // remaining slice tail is shorter than 64 B) instead of the
        // 4 its `MAX_LINES` cap would otherwise allow. No
        // `match_length < 64` gate — with several sequences of
        // lookahead the prefetch overlaps work the CPU has to do
        // anyway, so even sub-cache-line matches benefit.
        const PREFETCH_EXTENT: usize = 128;
        let (s1, s2) = self.buffer.as_slices();
        if start_idx < s1.len() {
            let s1_tail = &s1[start_idx..];
            let s1_bound = core::cmp::min(s1_tail.len(), PREFETCH_EXTENT);
            prefetch::prefetch_slice_t1(&s1_tail[..s1_bound]);
            // Wrap continuation: when the match source straddles the
            // s1/s2 boundary and the s1 tail is shorter than the
            // PREFETCH_EXTENT we asked for, top up the rest from
            // s2[0..]. Without this the donor's "up to two cache
            // lines" intent silently collapses to one (or zero if
            // s1_tail is the last sub-line of s1).
            if s1_bound < PREFETCH_EXTENT {
                let remaining = PREFETCH_EXTENT - s1_bound;
                let s2_bound = core::cmp::min(s2.len(), remaining);
                if s2_bound > 0 {
                    prefetch::prefetch_slice_t1(&s2[..s2_bound]);
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
            prefetch::prefetch_slice_t1(&tail[..bound]);
        }
    }

    #[cold]
    fn repeat_from_dict(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), DecodeBufferError> {
        if self.total_output_counter <= self.window_size as u64 {
            // at least part of that repeat is from the dictionary content
            let bytes_from_dict = offset - self.buffer.len();

            if bytes_from_dict > self.dict_content.len() {
                return Err(DecodeBufferError::NotEnoughBytesInDictionary {
                    got: self.dict_content.len(),
                    need: bytes_from_dict,
                });
            }

            if bytes_from_dict < match_length {
                let dict_slice = &self.dict_content[self.dict_content.len() - bytes_from_dict..];
                prefetch::prefetch_slice(dict_slice);
                self.buffer.extend(dict_slice);

                self.total_output_counter += bytes_from_dict as u64;
                return self.repeat(self.buffer.len(), match_length - bytes_from_dict);
            } else {
                let low = self.dict_content.len() - bytes_from_dict;
                let high = low + match_length;
                let dict_slice = &self.dict_content[low..high];
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

    /// drain the buffer completely
    pub fn drain(&mut self) -> Vec<u8> {
        let (slice1, slice2) = self.buffer.as_slices();
        #[cfg(feature = "hash")]
        {
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
            self.hash.write(&slice1[..written1]);
            drain_guard.amount += written1;

            // Apparently this is what clippy thinks is the best way of expressing this
            res1?;

            // Only if the first call to write_bytes was not a partial write we can continue with slice2
            // Partial writes SHOULD never happen without res1 being an error, but lets just protect against it anyways.
            if written1 == n1 && n2 != 0 {
                let (written2, res2) = write_bytes(&slice2[..n2]);
                #[cfg(feature = "hash")]
                self.hash.write(&slice2[..written2]);
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
        buf.reserve(64);
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
        buf.reserve(4 * 1024 * 1024);
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
    fn repeat_from_dict_full_copy_updates_total_output_counter() {
        let mut decode_buf = DecodeBuffer::<RingBuffer>::new(1);
        decode_buf.dict_content = b"0123456789".to_vec();

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
}
