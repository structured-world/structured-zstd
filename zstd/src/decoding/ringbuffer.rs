use crate::io::Read;
use alloc::alloc::{alloc_zeroed, dealloc};
use core::{alloc::Layout, ptr::NonNull, slice};

use super::buffer_backend::WILDCOPY_OVERLENGTH;
use super::simd_copy;

// `WILDCOPY_OVERLENGTH` is shared with `flat_buf` via
// `buffer_backend.rs` to guarantee both backends size their trailing
// slack identically — drift would invalidate the shared safety
// assumption that wildcopy SIMD overshoot stores/loads near the
// buffer boundary do not need a "min_buffer_size < copy_multiple"
// fallback. See [`super::buffer_backend::WILDCOPY_OVERLENGTH`] for
// the donor-parity rationale.

pub struct RingBuffer {
    // Safety invariants:
    //
    // 1.
    //    a.`buf` must be a valid allocation of capacity `cap`
    //    b. ...unless `cap=0`, in which case it is dangling
    // 2. If tail≥head
    //    a. `head..tail` must contain initialized memory.
    //    b. Else, `head..` and `..tail` must be initialized
    // 3. `head` and `tail` are in bounds (≥ 0 and < cap)
    // 4. `tail` is never `cap` except for a full buffer, and instead uses the value `0`. In other words, `tail` always points to the place
    //    where the next element would go (if there is space)
    buf: NonNull<u8>,
    cap: usize,
    head: usize,
    tail: usize,
}

// SAFETY: RingBuffer does not hold any thread specific values -> it can be sent to another thread -> RingBuffer is Send
unsafe impl Send for RingBuffer {}

// SAFETY: Ringbuffer does not provide unsyncronized interior mutability which makes &RingBuffer Send -> RingBuffer is Sync
unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    pub fn new() -> Self {
        RingBuffer {
            // SAFETY: Upholds invariant 1a as stated
            buf: NonNull::dangling(),
            cap: 0,
            // SAFETY: Upholds invariant 2-4
            head: 0,
            tail: 0,
        }
    }

    /// Return the number of bytes in the buffer.
    pub fn len(&self) -> usize {
        let (x, y) = self.data_slice_lengths();
        x + y
    }

    /// Current allocation capacity. Paired with `tail()` in
    /// `DecodeBuffer::checkpoint` so `restore_checkpoint` can detect an
    /// intervening reallocation (which compacts data and invalidates
    /// previously-captured tail indices). Reached via the
    /// `BufferBackend::cap` trait method; the inherent fn is kept
    /// `pub(super)` so the trait impl below has an item to forward to
    /// without touching the field through a public surface.
    #[inline]
    #[allow(dead_code)]
    pub(super) fn cap(&self) -> usize {
        self.cap
    }

    /// Current write cursor, used by `DecodeBuffer::checkpoint` to
    /// record a rollback point before speculative writes. Same
    /// surface contract as `cap()` above.
    #[inline]
    #[allow(dead_code)]
    pub(super) fn tail(&self) -> usize {
        self.tail
    }

    /// Force the write cursor back to a previously captured value, undoing
    /// any pushes / repeats issued after the corresponding `tail()` call.
    ///
    /// # Safety
    /// The caller must guarantee:
    /// - `new_tail` was returned by an earlier `tail()` call on this same
    ///   `RingBuffer` instance.
    /// - No reallocation has happened in between (a `reserve_amortized`
    ///   bump would have shifted the ring-buffer indices).
    /// - `head` has not moved since the corresponding `tail()` was
    ///   captured. A `head` advance (drain) followed by `set_tail` to an
    ///   old position would silently re-expose already-consumed bytes
    ///   through `len()` / `as_slices()` — the full-vs-empty
    ///   discriminator (invariant 4: `tail == 0` for "full", never
    ///   `cap`) only stays consistent when `head` is fixed.
    /// - The bytes between `new_tail` and the current tail are not used
    ///   afterwards (callers truncate any view that depended on them).
    ///
    /// The sole caller today is `DecodeBuffer::try_restore_checkpoint`,
    /// which is used only from the fused sequence executor — that path
    /// never drains between checkpoint and restore, so `head` is
    /// guaranteed fixed. Any future caller MUST audit the same
    /// preconditions before using this method.
    #[inline]
    pub(super) unsafe fn set_tail(&mut self, new_tail: usize) {
        debug_assert!(
            new_tail < self.cap || self.cap == 0,
            "new_tail ({}) must be < cap ({})",
            new_tail,
            self.cap
        );
        self.tail = new_tail;
    }

    /// Return the amount of available space (in bytes) of the buffer.
    pub fn free(&self) -> usize {
        let (x, y) = self.free_slice_lengths();
        (x + y).saturating_sub(1)
    }

    /// Empty the buffer and reset the head and tail.
    pub fn clear(&mut self) {
        // SAFETY: Upholds invariant 2, trivially
        // SAFETY: Upholds invariant 3; 0 is always valid
        self.head = 0;
        self.tail = 0;
    }

    /// Ensure that there's space for `amount` elements in the buffer.
    #[inline]
    pub fn reserve(&mut self, amount: usize) {
        // Flat fast path: when the data region hasn't wrapped (head ≤ tail)
        // and the write does not cross `cap`, free space is trivially
        // `cap - tail - 1 + head` ≥ `cap - tail - 1` ≥ `amount`. Skip
        // free_slice_lengths' branch + saturating_sub on the common case
        // that dominates frames fitting in the window (the same case the
        // flat extend path optimises for).
        // Use saturating arithmetic so a pathological `amount` close to
        // `usize::MAX` cannot wrap `tail + amount` and let the fast
        // path falsely report enough space — `extend` would then write
        // past the allocation.
        if self.head <= self.tail && amount < self.cap.saturating_sub(self.tail) {
            return;
        }
        let free = self.free();
        if free >= amount {
            return;
        }

        self.reserve_amortized(amount - free);
    }

    #[inline(never)]
    #[cold]
    fn reserve_amortized(&mut self, amount: usize) {
        // SAFETY: if we were successfully able to construct this layout when we allocated then it's also valid do so now
        let current_layout =
            unsafe { Layout::array::<u8>(self.cap + WILDCOPY_OVERLENGTH).unwrap_unchecked() };

        // Always have at least 1 unused element as the sentinel.
        // Use checked_add so a caller passing a huge `amount` (e.g.
        // close to usize::MAX from a malformed `match_length`) cannot
        // wrap `self.cap + amount` and produce an undersized `new_cap`
        // that subsequent unsafe writes would trust.
        let needed = self
            .cap
            .checked_add(amount)
            .expect("ringbuffer capacity overflow");
        let new_cap = usize::max(self.cap.next_power_of_two(), needed.next_power_of_two())
            .checked_add(1)
            .expect("ringbuffer capacity overflow");

        // Check that the capacity isn't bigger than isize::MAX, which is the max allowed by LLVM, or that
        // we are on a >= 64 bit system which will never allow that much memory to be allocated
        #[allow(clippy::assertions_on_constants)]
        {
            debug_assert!(usize::BITS >= 64 || new_cap < isize::MAX as usize);
        }

        // Physical allocation includes WILDCOPY_OVERLENGTH bytes of trailing
        // slack — see the const's doc comment for rationale. `new_cap` itself
        // remains the indexing capacity (head/tail wrap on it).
        let new_layout = Layout::array::<u8>(new_cap + WILDCOPY_OVERLENGTH).unwrap_or_else(|_| {
            panic!(
                "Could not create layout for u8 array of size {}",
                new_cap + WILDCOPY_OVERLENGTH
            )
        });

        // alloc_zeroed (not plain alloc) so wildcopy reads that overshoot
        // past `tail` into not-yet-written buffer bytes — or past `cap` into
        // the slack region — observe defined values (0) instead of
        // uninitialized memory. The zero content itself is irrelevant
        // (overshoot writes are wildcopy garbage the caller never reads),
        // but the read itself must not be UB.
        // TODO maybe rework this to generate an error?
        let new_buf = unsafe {
            let new_buf = alloc_zeroed(new_layout);

            NonNull::new(new_buf).expect("Allocating new space for the ringbuffer failed")
        };

        // If we had data before, copy it over to the newly alloced memory region
        if self.cap > 0 {
            let ((s1_ptr, s1_len), (s2_ptr, s2_len)) = self.data_slice_parts();

            unsafe {
                // SAFETY: Upholds invariant 2, we end up populating (0..(len₁ + len₂))
                new_buf.as_ptr().copy_from_nonoverlapping(s1_ptr, s1_len);
                new_buf
                    .as_ptr()
                    .add(s1_len)
                    .copy_from_nonoverlapping(s2_ptr, s2_len);
                dealloc(self.buf.as_ptr(), current_layout);
            }

            // SAFETY: Upholds invariant 3, head is 0 and in bounds, tail is only ever `cap` if the buffer
            // is entirely full
            self.tail = s1_len + s2_len;
            self.head = 0;
        }
        // SAFETY: Upholds invariant 1: the buffer was just allocated correctly
        self.buf = new_buf;
        self.cap = new_cap;
    }

    #[allow(dead_code)]
    pub fn push_back(&mut self, byte: u8) {
        self.reserve(1);

        // SAFETY: Upholds invariant 2 by writing initialized memory
        unsafe { self.buf.as_ptr().add(self.tail).write(byte) };
        // SAFETY: Upholds invariant 3 by wrapping `tail` around
        self.tail = (self.tail + 1) % self.cap;
    }

    /// Fetch the byte stored at the selected index from the buffer, returning it, or
    /// `None` if the index is out of bounds.
    #[allow(dead_code)]
    pub fn get(&self, idx: usize) -> Option<u8> {
        if idx < self.len() {
            // SAFETY: Establishes invariants on memory being initialized and the range being in-bounds
            // (Invariants 2 & 3)
            let idx = (self.head + idx) % self.cap;
            Some(unsafe { self.buf.as_ptr().add(idx).read() })
        } else {
            None
        }
    }
    /// Append the provided data to the end of `self`.
    ///
    /// `#[inline]` so the flat fast path below folds into the hot
    /// `execute_sequences` -> `DecodeBuffer::push` -> here chain. After the
    /// flat-extend refactor most calls return in ~5 instructions plus the
    /// inline copy; keeping a separate stack frame for that work was a
    /// noticeable fraction of the function-call overhead per literal push.
    #[inline]
    pub fn extend(&mut self, data: &[u8]) {
        let len = data.len();
        let ptr = data.as_ptr();
        if len == 0 {
            return;
        }

        self.reserve(len);

        debug_assert!(self.len() + len < self.cap);
        debug_assert!(self.free() >= len, "free: {} len: {}", self.free(), len);

        // Flat fast path: the write is one contiguous copy from `data` to
        // `buf + tail` with no wraparound, no slice split, no second
        // `simd_copy` call. Conditions:
        //   1. head ≤ tail (no prior wrap of the data region), and
        //   2. tail + len ≤ cap (the write itself does not cross `cap`).
        // For frames that fit in the window (the common case on this bench
        // and on most real-world streams), this path fires on every literal
        // push and on every `extend_from_reader` slot. `simd_copy` can use
        // the WILDCOPY_OVERLENGTH slack past `cap` for its SIMD overshoot
        // since dst ends at `cap`.
        //
        // Profile (decompress L-1 fast decodecorpus-z000033 c_stream)
        // showed `RingBuffer::extend` at 33.6% self-time AFTER the routing
        // refactor — most of that was the 4-case dispatch through
        // `free_slice_parts` for the wrap-shaped slow path on workloads
        // that never wrap. Bypassing it for the flat case directly
        // attacks that share.
        if self.head <= self.tail && self.tail + len <= self.cap {
            let dst_ptr = unsafe { self.buf.as_ptr().add(self.tail) };
            let dst_cap = (self.cap - self.tail) + WILDCOPY_OVERLENGTH;
            unsafe {
                simd_copy::copy_bytes_overshooting((ptr, len), (dst_ptr, dst_cap), len);
            }
            // Invariant 4: `tail` is never `cap` except when full (then 0).
            // After this write tail can land at `cap` if the literal push
            // exactly filled the buffer — normalize to 0 in that case.
            self.tail += len;
            if self.tail == self.cap {
                self.tail = 0;
            }
            return;
        }

        let ((f1_ptr, f1_len), (f2_ptr, f2_len)) = self.free_slice_parts();
        debug_assert!(f1_len + f2_len >= len, "{} + {} < {}", f1_len, f2_len, len);

        let in_f1 = usize::min(len, f1_len);

        let in_f2 = len - in_f1;

        debug_assert!(in_f1 + in_f2 == len);

        // Route through `simd_copy::copy_bytes_overshooting` instead of raw
        // `copy_from_nonoverlapping`. Profile (decompress L-1 fast
        // decodecorpus-z000033 c_stream) showed `_platform_memmove` at 24%
        // self-time, of which 943 ms was `execute_sequences → DecodeBuffer::
        // push → RingBuffer::extend → memmove`. The previous direct
        // `copy_from_nonoverlapping` lowered to a libc memmove call even
        // for 1..=16 byte literal pushes; simd_copy's inline byte /
        // overlapping-u64 tail path handles those without a function call.
        //
        // **Slack reachability**: `data` is an external slice with no slack
        // so `src.1` stays exact (`in_f1` / `in_f2`). For `dst.1`, the
        // first free slice ends at `self.cap` ONLY when `tail >= head` —
        // then `free_slice_parts` returns `(buf+tail, cap-tail)` and the
        // WILDCOPY_OVERLENGTH region beyond `cap` is the writable slack.
        // When `tail < head` the first free slice is the inner gap
        // `(buf+tail, head-tail)`; its end lands at `head`, so any
        // wildcopy overshoot past it would clobber still-readable buffered
        // output. f2 always ends at `head` (or is empty) so it never
        // gets the slack either.
        //
        // **Secondary safety note**: even without this gate, the current
        // `simd_copy::copy_bytes_overshooting` fast paths cannot actually
        // overshoot for `extend`'s call shape because `min_buffer_size`
        // is dominated by `src.1 == in_f1 == copy_at_least`. The
        // single-op-16 path needs `min >= 16`, which requires
        // `in_f1 >= 16` — and at that point `copy_at_least == 16` makes
        // the write exactly 16 bytes. Chunked SIMD only fires when
        // `rounded == in_f1` (multiple of chunk size), again exact-fit.
        // The gate is still applied so the inflation does not become a
        // latent UB hazard if a future `simd_copy` change derives its
        // overshoot bound from `dst.1` alone.
        let f1_dst_cap = if self.tail >= self.head {
            f1_len + WILDCOPY_OVERLENGTH
        } else {
            f1_len
        };
        unsafe {
            if in_f1 > 0 {
                simd_copy::copy_bytes_overshooting((ptr, in_f1), (f1_ptr, f1_dst_cap), in_f1);
            }
            if in_f2 > 0 {
                simd_copy::copy_bytes_overshooting(
                    (ptr.add(in_f1), in_f2),
                    (f2_ptr, f2_len),
                    in_f2,
                );
            }
        }
        // SAFETY: Upholds invariant 3 by wrapping `tail` around.
        self.tail = (self.tail + len) % self.cap;
    }

    /// Advance head past `amount` elements, effectively removing
    /// them from the buffer.
    pub fn drop_first_n(&mut self, amount: usize) {
        debug_assert!(amount <= self.len());
        let amount = usize::min(amount, self.len());
        // SAFETY: we maintain invariant 2 here since this will always lead to a smaller buffer
        // for amount≤len
        self.head = (self.head + amount) % self.cap;
    }

    /// Return the size of the two contiguous occupied sections of memory used
    /// by the buffer.
    // SAFETY: other code relies on this pointing to initialized halves of the buffer only
    fn data_slice_lengths(&self) -> (usize, usize) {
        let len_after_head;
        let len_to_tail;

        // TODO can we do this branchless?
        if self.tail >= self.head {
            len_after_head = self.tail - self.head;
            len_to_tail = 0;
        } else {
            len_after_head = self.cap - self.head;
            len_to_tail = self.tail;
        }
        (len_after_head, len_to_tail)
    }

    // SAFETY: other code relies on this pointing to initialized halves of the buffer only
    /// Return pointers to the head and tail, and the length of each section.
    fn data_slice_parts(&self) -> ((*const u8, usize), (*const u8, usize)) {
        let (len_after_head, len_to_tail) = self.data_slice_lengths();

        (
            (unsafe { self.buf.as_ptr().add(self.head) }, len_after_head),
            (self.buf.as_ptr(), len_to_tail),
        )
    }

    /// Return references to each part of the ring buffer.
    pub fn as_slices(&self) -> (&[u8], &[u8]) {
        let (s1, s2) = self.data_slice_parts();
        unsafe {
            // SAFETY: relies on the behavior of data_slice_parts for producing initialized memory
            let s1 = slice::from_raw_parts(s1.0, s1.1);
            let s2 = slice::from_raw_parts(s2.0, s2.1);
            (s1, s2)
        }
    }

    // SAFETY: other code relies on this producing the lengths of free zones
    // at the beginning/end of the buffer. Everything else must be initialized
    /// Returns the size of the two unoccupied sections of memory used by the buffer.
    fn free_slice_lengths(&self) -> (usize, usize) {
        let len_to_head;
        let len_after_tail;

        // TODO can we do this branchless?
        if self.tail < self.head {
            len_after_tail = self.head - self.tail;
            len_to_head = 0;
        } else {
            len_after_tail = self.cap - self.tail;
            len_to_head = self.head;
        }
        (len_to_head, len_after_tail)
    }

    /// Returns mutable references to the available space and the size of that available space,
    /// for the two sections in the buffer.
    // SAFETY: Other code relies on this pointing to the free zones, data after the first and before the second must
    // be valid
    fn free_slice_parts(&self) -> ((*mut u8, usize), (*mut u8, usize)) {
        let (len_to_head, len_after_tail) = self.free_slice_lengths();

        (
            (unsafe { self.buf.as_ptr().add(self.tail) }, len_after_tail),
            (self.buf.as_ptr(), len_to_head),
        )
    }

    /// Copies elements from the provided range to the end of the buffer.
    #[allow(dead_code)]
    pub fn extend_from_within(&mut self, start: usize, len: usize) {
        if start + len > self.len() {
            panic!(
                "Calls to this functions must respect start ({}) + len ({}) <= self.len() ({})!",
                start,
                len,
                self.len()
            );
        }

        self.reserve(len);

        // SAFETY: Requirements checked:
        // 1. explicitly checked above, resulting in a panic if it does not hold
        // 2. explicitly reserved enough memory
        unsafe { self.extend_from_within_unchecked(start, len) }
    }

    /// Copies data from the provided range to the end of the buffer, without
    /// first verifying that the unoccupied capacity is available.
    ///
    /// `#[inline]` is load-bearing: this is the hottest call from
    /// `DecodeBuffer::repeat` (match-copy on every non-repcode, non-
    /// overlapping sequence). Without it, the compiler cannot fold the
    /// `head < tail` flat-layout fast path into the caller and the
    /// per-block decode pays a real function-call hop. For frames that
    /// fit in the window (the dominant case — Fast-encoded blocks
    /// especially), this is the difference between one inlined SIMD
    /// copy and a non-inlined dispatch through `free_slice_parts`-shape
    /// branches.
    ///
    /// SAFETY:
    /// For this to be safe two requirements need to hold:
    /// 1. start + len <= self.len() so we do not copy uninitialised memory
    /// 2. More then len reserved space so we do not write out-of-bounds
    #[inline]
    #[warn(unsafe_op_in_unsafe_fn)]
    pub unsafe fn extend_from_within_unchecked(&mut self, start: usize, len: usize) {
        debug_assert!(start + len <= self.len());
        debug_assert!(self.free() >= len);

        if self.head < self.tail {
            // Continuous source section and possibly non continuous write section:
            //
            //            H           T
            // Read:  ____XXXXSSSSXXXX________
            // Write: ________________DDDD____
            //
            // H: Head position (first readable byte)
            // T: Tail position (first writable byte)
            // X: Uninvolved bytes in the readable section
            // S: Source bytes, to be copied to D bytes
            // D: Destination bytes, going to be copied from S bytes
            // _: Uninvolved bytes in the writable section
            let after_tail = usize::min(len, self.cap - self.tail);

            let src = (
                // SAFETY: `len <= isize::MAX` and fits the memory range of `buf`
                unsafe { self.buf.as_ptr().add(self.head + start) }.cast_const(),
                // Src length plus WILDCOPY slack — src + (tail - head - start) ends at
                // `tail` (≤ `cap`); extending by WILDCOPY_OVERLENGTH places the read
                // tail at most at `cap + WILDCOPY_OVERLENGTH`, which is still inside the
                // physical allocation (see `reserve_amortized`). The overshoot bytes
                // are wildcopy fill and are never consumed by the caller.
                (self.tail - self.head - start) + WILDCOPY_OVERLENGTH,
            );

            let dst = (
                // SAFETY: `len <= isize::MAX` and fits the memory range of `buf`
                unsafe { self.buf.as_ptr().add(self.tail) },
                // Dst length plus WILDCOPY slack — dst ends at `cap`, the slack region
                // beyond `cap` is owned by this allocation and absorbs any wildcopy
                // overshoot writes without corrupting subsequent ring-buffer state.
                (self.cap - self.tail) + WILDCOPY_OVERLENGTH,
            );

            // SAFETY: `src` points at initialized data, `dst` points to writable memory
            // (including WILDCOPY_OVERLENGTH bytes of slack past `cap`), and the
            // `(ptr, len)` capacities are sized for any rounded-up wildcopy amount
            // (`copy_len.next_multiple_of(active_chunk)`) selected by
            // `copy_bytes_overshooting`, and source/destination regions do not overlap.
            unsafe { simd_copy::copy_bytes_overshooting(src, dst, after_tail) }

            if after_tail < len {
                // The write section was not continuous:
                //
                //            H           T
                // Read:  ____XXXXSSSSXXXX__
                // Write: DD______________DD
                //
                // H: Head position (first readable byte)
                // T: Tail position (first writable byte)
                // X: Uninvolved bytes in the readable section
                // S: Source bytes, to be copied to D bytes
                // D: Destination bytes, going to be copied from S bytes
                // _: Uninvolved bytes in the writable section

                let src = (
                    // SAFETY: we are still within the memory range of `buf`
                    unsafe { src.0.add(after_tail) },
                    // Src length kept inflated by WILDCOPY_OVERLENGTH — original len
                    // already accounted for the slack above.
                    src.1 - after_tail,
                );
                let dst = (
                    self.buf.as_ptr(),
                    // Dst length is bounded by `head`; we cannot inflate by
                    // WILDCOPY_OVERLENGTH here because overshoot writes would corrupt
                    // the readable region starting at `head`.
                    self.head,
                );

                // SAFETY: `src` points at initialized data, `dst` points to writable memory,
                // and the `(ptr, len)` capacities are sized for any rounded-up wildcopy amount
                // (`copy_len.next_multiple_of(active_chunk)`) selected by `copy_bytes_overshooting`,
                // and source/destination regions do not overlap.
                unsafe { simd_copy::copy_bytes_overshooting(src, dst, len - after_tail) }
            }
        } else {
            #[allow(clippy::collapsible_else_if)]
            if self.head + start >= self.cap {
                // Continuous read section and destination section:
                //
                //                  T           H
                // Read:  XXSSSSXXXX____________XX
                // Write: __________DDDD__________
                //
                // H: Head position (first readable byte)
                // T: Tail position (first writable byte)
                // X: Uninvolved bytes in the readable section
                // S: Source bytes, to be copied to D bytes
                // D: Destination bytes, going to be copied from S bytes
                // _: Uninvolved bytes in the writable section

                let start = (self.head + start) % self.cap;

                let src = (
                    // SAFETY: `len <= isize::MAX` and fits the memory range of `buf`
                    unsafe { self.buf.as_ptr().add(start) }.cast_const(),
                    // Src ends at `tail`; extending by WILDCOPY_OVERLENGTH reads at
                    // most into the trailing slack region of the allocation.
                    (self.tail - start) + WILDCOPY_OVERLENGTH,
                );

                let dst = (
                    // SAFETY: `len <= isize::MAX` and fits the memory range of `buf`
                    unsafe { self.buf.as_ptr().add(self.tail) },
                    // Dst length is bounded by `head` — wildcopy overshoot past `head`
                    // would clobber readable data, so the slack does not apply here.
                    self.head - self.tail,
                );

                // SAFETY: `src` points at initialized data, `dst` points to writable memory,
                // and the `(ptr, len)` capacities are sized for any rounded-up wildcopy amount
                // (`copy_len.next_multiple_of(active_chunk)`) selected by `copy_bytes_overshooting`,
                // and source/destination regions do not overlap.
                unsafe { simd_copy::copy_bytes_overshooting(src, dst, len) }
            } else {
                // Possibly non continuous read section and continuous destination section:
                //
                //            T           H
                // Read:  XXXX____________XXSSSSXX
                // Write: ____DDDD________________
                //
                // H: Head position (first readable byte)
                // T: Tail position (first writable byte)
                // X: Uninvolved bytes in the readable section
                // S: Source bytes, to be copied to D bytes
                // D: Destination bytes, going to be copied from S bytes
                // _: Uninvolved bytes in the writable section

                let after_start = usize::min(len, self.cap - self.head - start);

                let src = (
                    // SAFETY: `len <= isize::MAX` and fits the memory range of `buf`
                    unsafe { self.buf.as_ptr().add(self.head + start) }.cast_const(),
                    // Src ends at `cap`; the WILDCOPY_OVERLENGTH slack region is
                    // physically reachable from this read.
                    (self.cap - self.head - start) + WILDCOPY_OVERLENGTH,
                );

                let dst = (
                    // SAFETY: `len <= isize::MAX` and fits the memory range of `buf`
                    unsafe { self.buf.as_ptr().add(self.tail) },
                    // Dst length is bounded by `head` — no slack room to inflate.
                    self.head - self.tail,
                );

                // SAFETY: `src` points at initialized data, `dst` points to writable memory,
                // and the `(ptr, len)` capacities are sized for any rounded-up wildcopy amount
                // (`copy_len.next_multiple_of(active_chunk)`) selected by `copy_bytes_overshooting`,
                // and source/destination regions do not overlap.
                unsafe { simd_copy::copy_bytes_overshooting(src, dst, after_start) }

                if after_start < len {
                    // The read section was not continuous:
                    //
                    //                T           H
                    // Read:  SSXXXXXX____________XXSS
                    // Write: ________DDDD____________
                    //
                    // H: Head position (first readable byte)
                    // T: Tail position (first writable byte)
                    // X: Uninvolved bytes in the readable section
                    // S: Source bytes, to be copied to D bytes
                    // D: Destination bytes, going to be copied from S bytes
                    // _: Uninvolved bytes in the writable section

                    let src = (
                        self.buf.as_ptr().cast_const(),
                        // Src ends at `tail`; inflate by WILDCOPY_OVERLENGTH to let
                        // the SIMD fast paths fire on small `len - after_start`.
                        self.tail + WILDCOPY_OVERLENGTH,
                    );

                    let dst = (
                        // SAFETY: we are still within the memory range of `buf`
                        unsafe { dst.0.add(after_start) },
                        // Dst length bounded by `head` — overshoot past `head`
                        // would clobber readable data, so cannot inflate.
                        dst.1 - after_start,
                    );

                    // SAFETY: `src` points at initialized data, `dst` points to writable memory,
                    // and the `(ptr, len)` capacities are sized for any rounded-up wildcopy amount
                    // (`copy_len.next_multiple_of(active_chunk)`) selected by `copy_bytes_overshooting`,
                    // and source/destination regions do not overlap.
                    unsafe { simd_copy::copy_bytes_overshooting(src, dst, len - after_start) }
                }
            }
        }

        self.tail = (self.tail + len) % self.cap;
    }

    pub fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        if fill_length == 0 {
            return;
        }
        self.reserve(fill_length);
        let ((ptr1, len1), (ptr2, len2)) = self.free_slice_parts();
        debug_assert!(len1 + len2 >= fill_length);
        let fill1 = usize::min(len1, fill_length);
        unsafe {
            ptr1.write_bytes(fill_with, fill1);
        }
        if fill1 < fill_length {
            let fill2 = fill_length - fill1;
            debug_assert_eq!(fill_length, fill1 + fill2);
            unsafe {
                ptr2.write_bytes(fill_with, fill2);
            }
        }
        self.tail = (self.tail + fill_length) % self.cap;
    }

    pub fn extend_from_reader<R: Read>(
        &mut self,
        mut read: R,
        fill_length: usize,
    ) -> Result<(), crate::io::Error> {
        if fill_length == 0 {
            return Ok(());
        }
        self.reserve(fill_length);
        let ((ptr1, len1), (ptr2, len2)) = self.free_slice_parts();
        debug_assert!(len1 + len2 >= fill_length);
        let fill1 = usize::min(len1, fill_length);
        let s1 = unsafe {
            ptr1.write_bytes(0, fill1);
            slice::from_raw_parts_mut(ptr1, fill1)
        };
        read.read_exact(s1)?;
        if fill1 < fill_length {
            let fill2 = fill_length - fill1;
            debug_assert_eq!(fill_length, fill1 + fill2);
            let s2 = unsafe {
                ptr2.write_bytes(0, fill2);
                slice::from_raw_parts_mut(ptr2, fill2)
            };
            read.read_exact(s2)?;
        }
        self.tail = (self.tail + fill_length) % self.cap;
        Ok(())
    }

    #[allow(dead_code)]
    /// This function is functionally the same as [RingBuffer::extend_from_within_unchecked],
    /// but it does not contain any branching operations.
    ///
    /// NOTE on WILDCOPY_OVERLENGTH: unlike `extend_from_within_unchecked` and
    /// `extend`, this path passes exact-fit `(ptr, len)` capacities through to
    /// `copy_with_nobranch_check` / `simd_copy::copy_bytes_overshooting`. It
    /// therefore cannot trigger `simd_copy`'s SIMD fast paths that require
    /// `min(src.1, dst.1) >= 16` — short copies always take the
    /// inline byte / overlapping-u64 fallback instead of single_op_copy_16.
    /// This is intentional for now: the per-pointer head/tail relationship
    /// needed to decide which capacities are safe to inflate is not
    /// available inside `copy_with_nobranch_check`, and the branchless path
    /// is gated to x86 targets via `decode_buffer::use_branchless_wildcopy`
    /// where measurable x86 perf is needed to justify the extra plumbing.
    /// On aarch64 (the profiling target for the WILDCOPY_OVERLENGTH work)
    /// the unconditional `extend_from_within_unchecked` path is used, so
    /// the slack contract is exercised end-to-end there.
    ///
    /// SAFETY:
    /// Needs start + len <= self.len()
    /// And more then len reserved space
    #[inline]
    pub unsafe fn extend_from_within_unchecked_branchless(&mut self, start: usize, len: usize) {
        // SAFETY: caller guarantees the source range is valid and enough free
        // space exists; the raw-pointer arithmetic and copy stay within those bounds.
        unsafe {
            // data slices in raw parts
            let ((s1_ptr, s1_len), (s2_ptr, s2_len)) = self.data_slice_parts();

            debug_assert!(len <= s1_len + s2_len, "{} > {} + {}", len, s1_len, s2_len);

            // calc the actually wanted slices in raw parts
            let start_in_s1 = usize::min(s1_len, start);
            let end_in_s1 = usize::min(s1_len, start + len);
            let m1_ptr = s1_ptr.add(start_in_s1);
            let m1_len = end_in_s1 - start_in_s1;

            debug_assert!(end_in_s1 <= s1_len);
            debug_assert!(start_in_s1 <= s1_len);

            let start_in_s2 = start.saturating_sub(s1_len);
            let end_in_s2 = start_in_s2 + (len - m1_len);
            let m2_ptr = s2_ptr.add(start_in_s2);
            let m2_len = end_in_s2 - start_in_s2;

            debug_assert!(start_in_s2 <= s2_len);
            debug_assert!(end_in_s2 <= s2_len);

            debug_assert_eq!(len, m1_len + m2_len);

            // the free slices, must hold: f1_len + f2_len >= m1_len + m2_len
            let ((f1_ptr, f1_len), (f2_ptr, f2_len)) = self.free_slice_parts();

            debug_assert!(f1_len + f2_len >= m1_len + m2_len);

            // calc how many from where bytes go where
            let m1_in_f1 = usize::min(m1_len, f1_len);
            let m1_in_f2 = m1_len - m1_in_f1;
            let m2_in_f1 = usize::min(f1_len - m1_in_f1, m2_len);
            let m2_in_f2 = m2_len - m2_in_f1;

            debug_assert_eq!(m1_len, m1_in_f1 + m1_in_f2);
            debug_assert_eq!(m2_len, m2_in_f1 + m2_in_f2);
            debug_assert!(f1_len >= m1_in_f1 + m2_in_f1);
            debug_assert!(f2_len >= m1_in_f2 + m2_in_f2);
            debug_assert_eq!(len, m1_in_f1 + m2_in_f1 + m1_in_f2 + m2_in_f2);

            debug_assert!(self.buf.as_ptr().add(self.cap) >= f1_ptr.add(m1_in_f1 + m2_in_f1));
            debug_assert!(self.buf.as_ptr().add(self.cap) >= f2_ptr.add(m1_in_f2 + m2_in_f2));

            debug_assert!((m1_in_f2 > 0) ^ (m2_in_f1 > 0) || (m1_in_f2 == 0 && m2_in_f1 == 0));

            copy_with_nobranch_check(
                m1_ptr, m2_ptr, f1_ptr, f2_ptr, m1_in_f1, m2_in_f1, m1_in_f2, m2_in_f2,
            );
            self.tail = (self.tail + len) % self.cap;
        }
    }
}

impl super::buffer_backend::BufferBackend for RingBuffer {
    #[inline]
    fn new() -> Self {
        Self::new()
    }
    #[inline]
    fn clear(&mut self) {
        Self::clear(self);
    }
    #[inline]
    fn reserve(&mut self, n: usize) {
        Self::reserve(self, n);
    }
    #[inline]
    fn len(&self) -> usize {
        Self::len(self)
    }
    #[inline]
    fn cap(&self) -> usize {
        self.cap
    }
    #[inline]
    fn tail(&self) -> usize {
        self.tail
    }
    #[inline]
    unsafe fn set_tail(&mut self, new_tail: usize) {
        // SAFETY: forwarded; trait contract matches the inherent
        // method's invariants documented in `set_tail` above.
        unsafe { Self::set_tail(self, new_tail) };
    }
    #[inline]
    fn extend(&mut self, data: &[u8]) {
        Self::extend(self, data);
    }
    #[inline]
    fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        Self::extend_and_fill(self, fill_with, fill_length);
    }
    #[inline]
    fn extend_from_reader<R: crate::io::Read>(
        &mut self,
        read: R,
        fill_length: usize,
    ) -> Result<(), crate::io::Error> {
        Self::extend_from_reader(self, read, fill_length)
    }
    #[inline]
    unsafe fn extend_from_within_unchecked(&mut self, start: usize, len: usize) {
        // SAFETY: forwarded.
        unsafe { Self::extend_from_within_unchecked(self, start, len) };
    }
    #[inline]
    unsafe fn extend_from_within_unchecked_branchless(&mut self, start: usize, len: usize) {
        // SAFETY: forwarded.
        unsafe { Self::extend_from_within_unchecked_branchless(self, start, len) };
    }
    #[inline]
    fn as_slices(&self) -> (&[u8], &[u8]) {
        Self::as_slices(self)
    }
    #[inline]
    fn drop_first_n(&mut self, n: usize) {
        Self::drop_first_n(self, n);
    }
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        if self.cap == 0 {
            return;
        }

        // SAFETY: if we were successfully able to construct this layout when we allocated then it's also valid do so now.
        // Layout matches `reserve_amortized` which inflates by WILDCOPY_OVERLENGTH.
        // Relies on / establishes invariant 1
        let current_layout =
            unsafe { Layout::array::<u8>(self.cap + WILDCOPY_OVERLENGTH).unwrap_unchecked() };

        unsafe {
            dealloc(self.buf.as_ptr(), current_layout);
        }
    }
}

#[allow(dead_code)]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn copy_without_checks(
    m1_ptr: *const u8,
    m2_ptr: *const u8,
    f1_ptr: *mut u8,
    f2_ptr: *mut u8,
    m1_in_f1: usize,
    m2_in_f1: usize,
    m1_in_f2: usize,
    m2_in_f2: usize,
) {
    unsafe {
        f1_ptr.copy_from_nonoverlapping(m1_ptr, m1_in_f1);
        f1_ptr
            .add(m1_in_f1)
            .copy_from_nonoverlapping(m2_ptr, m2_in_f1);

        f2_ptr.copy_from_nonoverlapping(m1_ptr.add(m1_in_f1), m1_in_f2);
        f2_ptr
            .add(m1_in_f2)
            .copy_from_nonoverlapping(m2_ptr.add(m2_in_f1), m2_in_f2);
    }
}

/// Reference implementation used only by the `copy_with_nobranch_check`
/// equivalence test (`copy_with_nobranch_check_matches_checked_for_all_valid_case_masks`).
/// Production code never calls this — `extend_from_within_unchecked_branchless`
/// dispatches to `copy_with_nobranch_check` directly on x86.
///
/// Like its branchless sibling, this helper does **not** inflate the
/// `src.1` / `dst.1` capacities passed to `simd_copy::copy_bytes_overshooting`
/// by `WILDCOPY_OVERLENGTH`. The exact-fit lengths mean `simd_copy`'s
/// `min_buffer_size >= 16` SIMD fast paths cannot trigger here — short
/// copies always fall through to the inline byte / overlapping-u64 tail
/// instead of `single_op_copy_16`. This is intentional for parity with the
/// production branchless path (which has the same property for the same
/// per-pointer head/tail reason documented on
/// `extend_from_within_unchecked_branchless`).
#[allow(dead_code)]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn copy_with_checks(
    m1_ptr: *const u8,
    m2_ptr: *const u8,
    f1_ptr: *mut u8,
    f2_ptr: *mut u8,
    m1_in_f1: usize,
    m2_in_f1: usize,
    m1_in_f2: usize,
    m2_in_f2: usize,
) {
    unsafe {
        let m1_src_cap = m1_in_f1 + m1_in_f2;
        let m2_src_cap = m2_in_f1 + m2_in_f2;
        let f1_dst_cap = m1_in_f1 + m2_in_f1;
        let f2_dst_cap = m1_in_f2 + m2_in_f2;

        if m1_in_f1 != 0 {
            simd_copy::copy_bytes_overshooting(
                (m1_ptr, m1_src_cap),
                (f1_ptr, f1_dst_cap),
                m1_in_f1,
            );
        }
        if m2_in_f1 != 0 {
            simd_copy::copy_bytes_overshooting(
                (m2_ptr, m2_src_cap),
                (f1_ptr.add(m1_in_f1), m2_in_f1),
                m2_in_f1,
            );
        }

        if m1_in_f2 != 0 {
            simd_copy::copy_bytes_overshooting(
                (m1_ptr.add(m1_in_f1), m1_in_f2),
                (f2_ptr, f2_dst_cap),
                m1_in_f2,
            );
        }
        if m2_in_f2 != 0 {
            simd_copy::copy_bytes_overshooting(
                (m2_ptr.add(m2_in_f1), m2_in_f2),
                (f2_ptr.add(m1_in_f2), m2_in_f2),
                m2_in_f2,
            );
        }
    }
}

/// 16-way case dispatch over the four `(m1|m2)_in_f(1|2)` non-empty
/// combinations, used by `extend_from_within_unchecked_branchless` on x86
/// to avoid the branch-misprediction pattern of the unconditional
/// `extend_from_within_unchecked` path. `#[allow(dead_code)]` because the
/// only caller is gated to x86 by `decode_buffer::use_branchless_wildcopy`
/// — on aarch64 / wasm / etc. rustc sees this as unreachable.
///
/// **WILDCOPY_OVERLENGTH note:** like `copy_with_checks` above, the
/// `(ptr, len)` tuples passed to `simd_copy::copy_bytes_overshooting`
/// below are exact-fit. The caller (the branchless path) does not thread
/// per-pointer head/tail context through to here, so we cannot safely
/// inflate by `WILDCOPY_OVERLENGTH` the way `extend_from_within_unchecked`
/// does. Consequence: short copies on the x86 branchless path always fall
/// into `simd_copy`'s inline byte / overlapping-u64 tail rather than the
/// `min_buffer_size >= 16` `single_op_copy_16` fast path. Aarch64 hot
/// decode (the WILDCOPY profiling target) uses the unconditional path,
/// which does inflate, so the slack contract is exercised end-to-end
/// there.
#[allow(dead_code)]
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn copy_with_nobranch_check(
    m1_ptr: *const u8,
    m2_ptr: *const u8,
    f1_ptr: *mut u8,
    f2_ptr: *mut u8,
    m1_in_f1: usize,
    m2_in_f1: usize,
    m1_in_f2: usize,
    m2_in_f2: usize,
) {
    unsafe {
        let m1_src_cap = m1_in_f1 + m1_in_f2;
        let m2_src_cap = m2_in_f1 + m2_in_f2;
        let f1_dst_cap = m1_in_f1 + m2_in_f1;
        let f2_dst_cap = m1_in_f2 + m2_in_f2;

        let case = (m1_in_f1 > 0) as usize
            | (((m2_in_f1 > 0) as usize) << 1)
            | (((m1_in_f2 > 0) as usize) << 2)
            | (((m2_in_f2 > 0) as usize) << 3);

        match case {
            0 => {}

            // one bit set
            1 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m1_in_f1,
                );
            }
            2 => {
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m2_in_f1,
                );
            }
            4 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f2_ptr, f2_dst_cap),
                    m1_in_f2,
                );
            }
            8 => {
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f2_ptr, f2_dst_cap),
                    m2_in_f2,
                );
            }

            // two bit set
            3 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m1_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f1_ptr.add(m1_in_f1), m2_in_f1),
                    m2_in_f1,
                );
            }
            5 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m1_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr.add(m1_in_f1), m1_in_f2),
                    (f2_ptr, f2_dst_cap),
                    m1_in_f2,
                );
            }
            6 => core::hint::unreachable_unchecked(),
            7 => core::hint::unreachable_unchecked(),
            9 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m1_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f2_ptr, f2_dst_cap),
                    m2_in_f2,
                );
            }
            10 => {
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m2_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr.add(m2_in_f1), m2_in_f2),
                    (f2_ptr, f2_dst_cap),
                    m2_in_f2,
                );
            }
            12 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f2_ptr, f2_dst_cap),
                    m1_in_f2,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f2_ptr.add(m1_in_f2), m2_in_f2),
                    m2_in_f2,
                );
            }

            // three bit set
            11 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m1_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f1_ptr.add(m1_in_f1), m2_in_f1),
                    m2_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr.add(m2_in_f1), m2_in_f2),
                    (f2_ptr, f2_dst_cap),
                    m2_in_f2,
                );
            }
            13 => {
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr, m1_src_cap),
                    (f1_ptr, f1_dst_cap),
                    m1_in_f1,
                );
                simd_copy::copy_bytes_overshooting(
                    (m1_ptr.add(m1_in_f1), m1_in_f2),
                    (f2_ptr, f2_dst_cap),
                    m1_in_f2,
                );
                simd_copy::copy_bytes_overshooting(
                    (m2_ptr, m2_src_cap),
                    (f2_ptr.add(m1_in_f2), m2_in_f2),
                    m2_in_f2,
                );
            }
            14 => core::hint::unreachable_unchecked(),
            15 => core::hint::unreachable_unchecked(),
            _ => core::hint::unreachable_unchecked(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{RingBuffer, copy_with_checks, copy_with_nobranch_check};
    use crate::decoding::simd_copy;

    fn assert_buffers_equal(expected: &RingBuffer, actual: &RingBuffer) {
        assert_eq!(expected.len(), actual.len());
        assert_eq!(expected.as_slices(), actual.as_slices());
        assert_eq!(expected.head, actual.head);
        assert_eq!(expected.tail, actual.tail);
        assert_eq!(expected.cap, actual.cap);
    }

    fn assert_branchless_matches_checked(
        mut checked: RingBuffer,
        mut branchless: RingBuffer,
        start: usize,
        len: usize,
    ) {
        assert!(checked.free() >= len);
        assert!(branchless.free() >= len);

        unsafe {
            checked.extend_from_within_unchecked(start, len);
            branchless.extend_from_within_unchecked_branchless(start, len);
        }

        assert_buffers_equal(&checked, &branchless);
    }

    #[test]
    fn smoke() {
        let mut rb = RingBuffer::new();

        rb.reserve(15);
        assert_eq!(17, rb.cap);

        rb.extend(b"0123456789");
        assert_eq!(rb.len(), 10);
        assert_eq!(rb.as_slices().0, b"0123456789");
        assert_eq!(rb.as_slices().1, b"");

        rb.drop_first_n(5);
        assert_eq!(rb.len(), 5);
        assert_eq!(rb.as_slices().0, b"56789");
        assert_eq!(rb.as_slices().1, b"");

        rb.extend_from_within(2, 3);
        assert_eq!(rb.len(), 8);
        assert_eq!(rb.as_slices().0, b"56789789");
        assert_eq!(rb.as_slices().1, b"");

        rb.extend_from_within(0, 3);
        assert_eq!(rb.len(), 11);
        assert_eq!(rb.as_slices().0, b"56789789567");
        assert_eq!(rb.as_slices().1, b"");

        rb.extend_from_within(0, 2);
        assert_eq!(rb.len(), 13);
        assert_eq!(rb.as_slices().0, b"567897895675");
        assert_eq!(rb.as_slices().1, b"6");

        rb.drop_first_n(11);
        assert_eq!(rb.len(), 2);
        assert_eq!(rb.as_slices().0, b"5");
        assert_eq!(rb.as_slices().1, b"6");

        rb.extend(b"0123456789");
        assert_eq!(rb.len(), 12);
        assert_eq!(rb.as_slices().0, b"5");
        assert_eq!(rb.as_slices().1, b"60123456789");

        rb.drop_first_n(11);
        assert_eq!(rb.len(), 1);
        assert_eq!(rb.as_slices().0, b"9");
        assert_eq!(rb.as_slices().1, b"");

        rb.extend(b"0123456789");
        assert_eq!(rb.len(), 11);
        assert_eq!(rb.as_slices().0, b"9012345");
        assert_eq!(rb.as_slices().1, b"6789");
    }

    #[test]
    fn edge_cases() {
        // Fill exactly, then empty then fill again
        let mut rb = RingBuffer::new();
        rb.reserve(16);
        assert_eq!(17, rb.cap);
        rb.extend(b"0123456789012345");
        assert_eq!(17, rb.cap);
        assert_eq!(16, rb.len());
        assert_eq!(0, rb.free());
        rb.drop_first_n(16);
        assert_eq!(0, rb.len());
        assert_eq!(16, rb.free());
        rb.extend(b"0123456789012345");
        assert_eq!(16, rb.len());
        assert_eq!(0, rb.free());
        assert_eq!(17, rb.cap);
        assert_eq!(1, rb.as_slices().0.len());
        assert_eq!(15, rb.as_slices().1.len());

        rb.clear();

        // data in both slices and then reserve
        rb.extend(b"0123456789012345");
        rb.drop_first_n(8);
        rb.extend(b"67890123");
        assert_eq!(16, rb.len());
        assert_eq!(0, rb.free());
        assert_eq!(17, rb.cap);
        assert_eq!(9, rb.as_slices().0.len());
        assert_eq!(7, rb.as_slices().1.len());
        rb.reserve(1);
        assert_eq!(16, rb.len());
        assert_eq!(16, rb.free());
        assert_eq!(33, rb.cap);
        assert_eq!(16, rb.as_slices().0.len());
        assert_eq!(0, rb.as_slices().1.len());

        rb.clear();

        // fill exactly, then extend from within
        rb.extend(b"0123456789012345");
        rb.extend_from_within(0, 16);
        assert_eq!(32, rb.len());
        assert_eq!(0, rb.free());
        assert_eq!(33, rb.cap);
        assert_eq!(32, rb.as_slices().0.len());
        assert_eq!(0, rb.as_slices().1.len());

        // extend from within cases
        let mut rb = RingBuffer::new();
        rb.reserve(8);
        rb.extend(b"01234567");
        rb.drop_first_n(5);
        rb.extend_from_within(0, 3);
        assert_eq!(4, rb.as_slices().0.len());
        assert_eq!(2, rb.as_slices().1.len());

        rb.drop_first_n(2);
        assert_eq!(2, rb.as_slices().0.len());
        assert_eq!(2, rb.as_slices().1.len());
        rb.extend_from_within(0, 4);
        assert_eq!(2, rb.as_slices().0.len());
        assert_eq!(6, rb.as_slices().1.len());

        rb.drop_first_n(2);
        assert_eq!(6, rb.as_slices().0.len());
        assert_eq!(0, rb.as_slices().1.len());
        rb.drop_first_n(2);
        assert_eq!(4, rb.as_slices().0.len());
        assert_eq!(0, rb.as_slices().1.len());
        rb.extend_from_within(0, 4);
        assert_eq!(7, rb.as_slices().0.len());
        assert_eq!(1, rb.as_slices().1.len());

        let mut rb = RingBuffer::new();
        rb.reserve(8);
        rb.extend(b"11111111");
        rb.drop_first_n(7);
        rb.extend(b"111");
        assert_eq!(2, rb.as_slices().0.len());
        assert_eq!(2, rb.as_slices().1.len());
        rb.extend_from_within(0, 4);
        assert_eq!(b"11", rb.as_slices().0);
        assert_eq!(b"111111", rb.as_slices().1);
    }

    #[test]
    fn extend_from_within_branchless_matches_checked_across_layouts() {
        let contiguous = || {
            let mut rb = RingBuffer::new();
            rb.reserve(16);
            rb.extend(b"0123456789");
            rb
        };
        assert_branchless_matches_checked(contiguous(), contiguous(), 2, 5);

        let wrapped_write = || {
            let mut rb = RingBuffer::new();
            rb.reserve(16);
            rb.extend(b"0123456789ABC");
            rb.drop_first_n(2);
            rb
        };
        assert_branchless_matches_checked(wrapped_write(), wrapped_write(), 1, 5);

        let wrapped_data = || {
            let mut rb = RingBuffer::new();
            rb.reserve(32);
            rb.extend(b"0123456789abcdefghijklmn");
            rb.drop_first_n(18);
            rb.extend(b"wxyz012345");
            rb
        };
        assert_branchless_matches_checked(wrapped_data(), wrapped_data(), 8, 2);
        assert_branchless_matches_checked(wrapped_data(), wrapped_data(), 2, 8);
    }

    #[test]
    fn copy_with_nobranch_check_matches_checked_for_all_valid_case_masks() {
        let cases = [
            (0, 0, 0, 0),
            (1, 0, 0, 0),
            (0, 1, 0, 0),
            (0, 0, 1, 0),
            (0, 0, 0, 1),
            (1, 1, 0, 0),
            (1, 0, 1, 0),
            (1, 0, 0, 1),
            (0, 1, 0, 1),
            (0, 0, 1, 1),
            (1, 1, 0, 1),
            (1, 0, 1, 1),
        ];

        for (m1_in_f1, m2_in_f1, m1_in_f2, m2_in_f2) in cases {
            let m1 = [11_u8, 12, 13, 14];
            let m2 = [21_u8, 22, 23, 24];
            let mut expected = [0_u8; 8];
            let mut actual = [0_u8; 8];

            unsafe {
                copy_with_checks(
                    m1.as_ptr(),
                    m2.as_ptr(),
                    expected.as_mut_ptr(),
                    expected.as_mut_ptr().add(4),
                    m1_in_f1,
                    m2_in_f1,
                    m1_in_f2,
                    m2_in_f2,
                );
                copy_with_nobranch_check(
                    m1.as_ptr(),
                    m2.as_ptr(),
                    actual.as_mut_ptr(),
                    actual.as_mut_ptr().add(4),
                    m1_in_f1,
                    m2_in_f1,
                    m1_in_f2,
                    m2_in_f2,
                );
            }

            assert_eq!(
                expected, actual,
                "case=({}, {}, {}, {})",
                m1_in_f1, m2_in_f1, m1_in_f2, m2_in_f2
            );
        }
    }

    #[test]
    fn copy_bytes_overshooting_preserves_prefix_for_runtime_chunk_lengths() {
        // Validate correctness for lengths derived from the active runtime chunk:
        // - single chunk (`chunk`)
        // - multi chunk (`2 * chunk`)
        // - fallback shape (`chunk + 1`)
        // This checks copy semantics across runtime-selected strategies.
        let chunk = simd_copy::active_chunk_size_for_tests();
        let single_len = chunk;
        let multi_len = chunk * 2;
        let fallback_len = chunk + 1;
        let overshoot_cap = chunk * 2;
        let cap = multi_len + chunk;

        let src_single = vec![1_u8; cap];
        let mut dst_single = vec![0_u8; cap];
        unsafe {
            simd_copy::copy_bytes_overshooting(
                (src_single.as_ptr(), single_len),
                (dst_single.as_mut_ptr(), single_len),
                single_len,
            );
        }
        assert_eq!(&dst_single[..single_len], &src_single[..single_len]);

        let src_multi = vec![2_u8; cap];
        let mut dst_multi = vec![0_u8; cap];
        unsafe {
            simd_copy::copy_bytes_overshooting(
                (src_multi.as_ptr(), multi_len),
                (dst_multi.as_mut_ptr(), multi_len),
                multi_len,
            );
        }
        assert_eq!(&dst_multi[..multi_len], &src_multi[..multi_len]);

        let src_fallback = vec![3_u8; cap];
        let mut dst_fallback = vec![0_u8; cap];
        unsafe {
            simd_copy::copy_bytes_overshooting(
                (src_fallback.as_ptr(), fallback_len),
                (dst_fallback.as_mut_ptr(), fallback_len),
                fallback_len,
            );
        }
        assert_eq!(&dst_fallback[..fallback_len], &src_fallback[..fallback_len]);

        let src_overshoot = vec![4_u8; cap + 1];
        let mut dst_overshoot = vec![0_u8; cap + 1];
        unsafe {
            simd_copy::copy_bytes_overshooting(
                (src_overshoot.as_ptr().add(1), overshoot_cap),
                (dst_overshoot.as_mut_ptr().add(1), overshoot_cap),
                fallback_len,
            );
        }
        assert_eq!(
            &dst_overshoot[1..1 + fallback_len],
            &src_overshoot[1..1 + fallback_len]
        );
    }

    /// Helper: drive the ringbuffer into a wrapped layout where the two
    /// free-slice halves straddle the physical end of the backing buffer.
    /// Returns a buffer whose `len() == fill_len` of `pre_byte` data and
    /// whose free region wraps.
    fn build_wrapped_buffer(cap: usize, fill_len: usize, pre_byte: u8) -> RingBuffer {
        let mut rb = RingBuffer::new();
        rb.reserve(cap);
        let actual_cap = rb.cap;
        // Push to near-end of the physical buffer.
        let pre_len = actual_cap - 2;
        let prefix = alloc::vec![pre_byte; pre_len];
        rb.extend(&prefix);
        // Drop those bytes so head advances past them; tail now sits near
        // the end of `cap`, head is in the middle. Subsequent inserts will
        // wrap across the physical end.
        rb.drop_first_n(pre_len - fill_len);
        assert_eq!(rb.len(), fill_len);
        assert!(rb.tail > rb.head, "tail should still trail tape end");
        rb
    }

    #[test]
    fn extend_and_fill_contiguous_layout() {
        let mut rb = RingBuffer::new();
        rb.extend_and_fill(0xAB, 7);
        assert_eq!(rb.len(), 7);
        let (s1, s2) = rb.as_slices();
        let mut combined = alloc::vec::Vec::with_capacity(7);
        combined.extend_from_slice(s1);
        combined.extend_from_slice(s2);
        assert_eq!(combined, alloc::vec![0xAB; 7]);
    }

    /// Construct a RingBuffer in the wrapped state where `tail < head`.
    /// Data occupies `[head, cap)` followed by `[0, tail)`, leaving the
    /// free region as `[tail, head)` — i.e. the **first** free slice
    /// returned by `free_slice_parts` is the inner gap, NOT a span that
    /// ends at `cap`.
    fn build_tail_before_head(cap_hint: usize, head_pos: usize, tail_pos: usize) -> RingBuffer {
        assert!(tail_pos < head_pos);
        let mut rb = RingBuffer::new();
        rb.reserve(cap_hint);
        let actual_cap = rb.cap;
        // Fill almost the whole buffer so we can carve out tail < head.
        let fill_len = actual_cap - 2;
        let prefix = alloc::vec![0xCD; fill_len];
        rb.extend(&prefix);
        // Drop bytes so `head` lands at the target. Tail stays at fill_len.
        rb.drop_first_n(head_pos);
        // Now extend just enough to wrap tail past `cap` and land it at
        // `tail_pos`. From the current state (head=head_pos, tail=fill_len),
        // we need `tail_pos = (fill_len + extra) % cap`, so
        // `extra = (tail_pos + cap - fill_len) % cap`.
        let extra = (tail_pos + actual_cap - fill_len) % actual_cap;
        rb.extend(&alloc::vec![0xCD; extra]);
        assert_eq!(rb.head, head_pos);
        assert_eq!(rb.tail, tail_pos);
        assert!(
            rb.tail < rb.head,
            "expected wrapped layout: tail={} head={}",
            rb.tail,
            rb.head
        );
        rb
    }

    #[test]
    fn extend_wrapped_layout_preserves_bytes_past_head() {
        // Regression test for the WILDCOPY_OVERLENGTH inflation bug.
        // When `tail < head` the first free slice returned by
        // `free_slice_parts` is the inner `[tail, head)` gap — it does NOT
        // end at `cap`. A previous version of `RingBuffer::extend` always
        // added WILDCOPY_OVERLENGTH to the destination capacity passed to
        // `simd_copy::copy_bytes_overshooting`, which on this layout would
        // allow wildcopy overshoot writes to clobber bytes at `[head,
        // head+16)` — still-readable data the caller has not consumed yet.
        // The current code gates the inflation on `tail >= head`; this
        // test asserts that bytes at `head..head+16` survive an extend()
        // that fills the inner gap nearly to capacity.
        let mut rb = build_tail_before_head(128, 80, 10);
        let head_before = rb.head;
        let cap = rb.cap;
        // Sample a window of bytes immediately past `head` and confirm
        // they are the prefill (0xCD); these are the bytes any erroneous
        // wildcopy overshoot would corrupt.
        let mut sentinel = [0u8; 16];
        unsafe {
            for (i, slot) in sentinel.iter_mut().enumerate() {
                *slot = rb.buf.as_ptr().add((head_before + i) % cap).read();
            }
        }
        assert!(sentinel.iter().all(|&b| b == 0xCD), "pre-state sentinel");

        // Fill the inner free region with a recognisable pattern, leaving
        // exactly the 1-byte sentinel gap the RingBuffer always reserves.
        let free_before = rb.free();
        let payload = alloc::vec![0x42; free_before];
        rb.extend(&payload);

        // The bytes at [head, head+16) must still be the original 0xCD
        // prefill — any overshoot would have written 0x42 over them.
        unsafe {
            for i in 0..16usize {
                let actual = rb.buf.as_ptr().add((head_before + i) % cap).read();
                assert_eq!(
                    actual,
                    0xCD,
                    "byte at head+{i} (raw idx {}) was clobbered: got {:#04x}",
                    (head_before + i) % cap,
                    actual
                );
            }
        }
    }

    #[test]
    fn extend_and_fill_wrapped_layout() {
        // Pre-fill so the free region straddles the wrap boundary, then
        // verify both halves are written with the fill byte.
        let mut rb = build_wrapped_buffer(16, 2, 0x11);
        let extra = rb.cap - 2; // fills exactly to capacity, forcing a wrap
        rb.extend_and_fill(0x22, extra);
        assert_eq!(rb.len(), 2 + extra);
        let (s1, s2) = rb.as_slices();
        let mut combined = alloc::vec::Vec::with_capacity(rb.len());
        combined.extend_from_slice(s1);
        combined.extend_from_slice(s2);
        let mut expected = alloc::vec![0x11; 2];
        expected.extend(alloc::vec![0x22; extra]);
        assert_eq!(combined, expected);
    }

    #[test]
    fn extend_from_reader_contiguous_layout() {
        let mut rb = RingBuffer::new();
        let src: [u8; 6] = [1, 2, 3, 4, 5, 6];
        rb.extend_from_reader(&src[..], 6).unwrap();
        assert_eq!(rb.len(), 6);
        let (s1, s2) = rb.as_slices();
        let mut combined = alloc::vec::Vec::with_capacity(6);
        combined.extend_from_slice(s1);
        combined.extend_from_slice(s2);
        assert_eq!(combined, src);
    }

    #[test]
    fn extend_from_reader_wrapped_layout() {
        let mut rb = build_wrapped_buffer(16, 3, 0xAA);
        let extra = rb.cap - 3;
        let src: alloc::vec::Vec<u8> = (0..extra as u8).collect();
        rb.extend_from_reader(src.as_slice(), extra).unwrap();
        assert_eq!(rb.len(), 3 + extra);
        let (s1, s2) = rb.as_slices();
        let mut combined = alloc::vec::Vec::with_capacity(rb.len());
        combined.extend_from_slice(s1);
        combined.extend_from_slice(s2);
        let mut expected = alloc::vec![0xAA; 3];
        expected.extend_from_slice(&src);
        assert_eq!(combined, expected);
    }

    #[test]
    fn extend_from_reader_eof_leaves_state_unchanged() {
        let mut rb = RingBuffer::new();
        rb.extend(b"prefix");
        let snapshot_len = rb.len();
        let snapshot_slices: (alloc::vec::Vec<u8>, alloc::vec::Vec<u8>) = {
            let (a, b) = rb.as_slices();
            (a.to_vec(), b.to_vec())
        };

        // Reader yields only 2 bytes but we ask for 10 → `read_exact` fails
        // on the second chunk, and `tail` must not advance.
        let short: [u8; 2] = [0xCC, 0xDD];
        let err = rb.extend_from_reader(&short[..], 10);
        assert!(err.is_err(), "short reader must propagate IO error");
        assert_eq!(rb.len(), snapshot_len, "len() must be unchanged on error");
        let (a, b) = rb.as_slices();
        assert_eq!(a, snapshot_slices.0.as_slice());
        assert_eq!(b, snapshot_slices.1.as_slice());
    }
}
