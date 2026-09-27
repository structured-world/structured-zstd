//! One allocation per compression context, carved into the context's tables
//! and buffers (upstream zstd `ZSTD_cwksp`, `zstd_cwksp.h`).
//!
//! A context that holds each buffer as a separate `Vec` hands the allocator the
//! decision of whether those pages survive between frames, and a
//! general-purpose allocator gives them back once their sum outgrows its own
//! trim heuristic; every frame of a fresh context then faults the whole working
//! set in again. Carving everything out of one allocation keeps the pages the
//! context's own, on any allocator.
//!
//! The layout follows upstream: the match finder's tables and its input history
//! are carved from the front, the per-block buffers from the back, and every
//! region starts on an [`ALIGN`]-byte boundary. A frame lays the workspace out in
//! two parts: the match finder opens it with the bytes it needs and carves them,
//! then the context carves its per-block buffers from what the opening reserved
//! for it.

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, handle_alloc_error};
use alloc::vec::Vec;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;

/// Alignment of every region, and of the workspace itself (upstream
/// `ZSTD_CWKSP_ALIGNMENT_BYTES`).
pub(crate) const ALIGN: usize = 64;

/// Bytes a region of `count` values of `T` occupies in a workspace, rounded
/// up to [`ALIGN`] so the next region starts aligned.
///
/// # Panics
///
/// Panics when the size overflows `usize`, as a `Vec` of that length would.
pub(crate) const fn region_bytes<T>(count: usize) -> usize {
    let Some(bytes) = count.checked_mul(size_of::<T>()) else {
        panic!("workspace region size overflows usize");
    };
    let Some(padded) = bytes.checked_add(ALIGN - 1) else {
        panic!("workspace region size overflows usize");
    };
    padded & !(ALIGN - 1)
}

/// How a frame's input will reach the match finder, as the context knows it
/// before the match finder resets. This is what sizes the match finder's input
/// history and the block buffers; a length carried here is exact, an advisory
/// one comes with the source-size hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngestPlan {
    /// The frame writes its blocks raw and keeps no history.
    Raw,
    /// One contiguous slice of this many bytes, handed over whole: the match
    /// finder may scan it in place and keep no copy of it.
    Slice(usize),
    /// A stream, read block by block into the history.
    Stream,
    /// A stream of this many pledged bytes, read like [`Self::Stream`]. The
    /// context refuses input of any other length, so the size is exact.
    PledgedStream(usize),
}

impl IngestPlan {
    /// The frame's length when the plan knows it exactly.
    pub(crate) fn exact_len(self) -> Option<usize> {
        match self {
            Self::Slice(len) | Self::PledgedStream(len) => Some(len),
            Self::Raw | Self::Stream => None,
        }
    }
}

/// The single allocation a compression context carves its match-finder tables,
/// input history and per-block buffers from.
///
/// It grows when a frame needs more than it holds and is kept otherwise, so a
/// context allocates once for as long as its parameters do not grow. A region
/// laid out at the same place in the same allocation as on the previous frame
/// keeps its contents, which is what lets a match finder carry a table, or a
/// dictionary at the head of its history, across frames.
///
/// `pub` only so it can appear in [`Matcher::reset_in_workspace`]; the module
/// is private, so no code outside this crate can name, build or receive one.
///
/// [`Matcher::reset_in_workspace`]: crate::encoding::Matcher::reset_in_workspace
pub struct Workspace {
    /// Start of the usable bytes, aligned to [`ALIGN`] inside the allocation
    /// that starts at `base`.
    ptr: NonNull<u8>,
    base: NonNull<u8>,
    capacity: usize,
    /// Counts allocations; a region is only the continuation of an earlier one
    /// if both were carved from the same allocation.
    generation: u64,
    /// Carving cursors of the current layout: tables from the front up, the
    /// history from the end of the leading part down, buffers from the back
    /// down.
    front: usize,
    history_front: usize,
    leading: usize,
    back: usize,
    /// The allocation a growth replaced (its base and usable bytes), kept
    /// until the history has been bound again so its bytes can be carried
    /// over.
    retired: Option<(NonNull<u8>, usize)>,
    /// The block ceiling the context set before the window was known, and the
    /// bytes it carves after the match finder for a given block size.
    block_target: usize,
    trailing_for: fn(usize) -> usize,
    /// How the frame's input reaches the match finder, set with the layout.
    ingest: IngestPlan,
    /// The block size the open layout's trailing part was sized for.
    block_capacity: usize,
    open: bool,
    /// Consecutive layouts that found the allocation far larger than they
    /// need, for giving back one that has stayed so.
    oversized_layouts: u32,
    /// Bytes of zero-start tables in this layout when they are larger than the
    /// input expected to write them, else 0 (see [`Self::open_for_match_finder`]).
    sparse_table_bytes: usize,
    /// The open layout made the allocation, zeroed, so its regions hold
    /// zeros until their holders write them.
    zeroed: bool,
    /// See [`Self::on_fresh_pages`].
    fresh_pages: bool,
}

/// Upstream `ZSTD_WORKSPACETOOLARGE_FACTOR` / `_MAXDURATION`
/// (`zstd_internal.h:258-265`): a workspace left with at least three times a
/// frame's need unused for more than this many frames in a row is allocated
/// again at the need (`ZSTD_resetCCtx_internal`, `zstd_compress.c:2154-2159`).
const TOO_LARGE_FACTOR: usize = 3;
const TOO_LARGE_MAX_LAYOUTS: u32 = 128;

/// Size from which the target's system allocator serves every request with
/// pages fresh from the kernel, so a zeroed allocation costs no memset and
/// faults in only what is touched. Below it memory is recycled and zeroing
/// it is a memset of all of it.
///
/// musl's mallocng maps anything above `MMAP_THRESHOLD` (131052 bytes,
/// `src/malloc/mallocng/meta.h`) on its own. glibc raises its mmap
/// threshold toward whatever it last freed, so a context rebuilt per frame
/// lands on recycled heap, but never past `DEFAULT_MMAP_THRESHOLD_MAX`
/// (`malloc/malloc.c`): 512 KiB on 32-bit targets, 32 MiB on 64-bit ones.
/// Other allocators take the glibc 64-bit ceiling, the one that assumes least.
#[cfg(target_env = "musl")]
const FRESH_PAGES_FROM: usize = 128 * 1024;
#[cfg(all(not(target_env = "musl"), target_pointer_width = "32"))]
const FRESH_PAGES_FROM: usize = 512 * 1024;
#[cfg(all(not(target_env = "musl"), not(target_pointer_width = "32")))]
const FRESH_PAGES_FROM: usize = 32 * 1024 * 1024;

/// Whether the workspace allocates from the target's system allocator, the only
/// kind [`FRESH_PAGES_FROM`] describes. A `no_std` build always runs on one the
/// application supplies, often a fixed heap that clears recycled memory to
/// answer a zeroed request, so it takes a zeroed allocation only where the
/// zero tables alone justify it. With `std` an application may still install
/// its own `#[global_allocator]`, which nothing here can detect; the assumption
/// then costs one memset of the bytes a frame leaves untouched when the
/// workspace grows, never a wrong result.
const SYSTEM_ALLOCATOR: bool = cfg!(feature = "std");

// SAFETY: the workspace owns its allocation outright; the raw pointer is what
// makes the type `!Send`/`!Sync` by default, not any shared state.
unsafe impl Send for Workspace {}
// SAFETY: as above; `&Workspace` exposes no access to the bytes.
unsafe impl Sync for Workspace {}

impl Workspace {
    /// An empty workspace. Allocates nothing until a layout opens it.
    pub(crate) const fn new() -> Self {
        Self {
            ptr: NonNull::<Aligned>::dangling().cast(),
            base: NonNull::dangling(),
            capacity: 0,
            generation: 0,
            front: 0,
            history_front: 0,
            leading: 0,
            back: 0,
            retired: None,
            block_target: 0,
            trailing_for: no_trailing,
            ingest: IngestPlan::Stream,
            block_capacity: 0,
            open: false,
            oversized_layouts: 0,
            sparse_table_bytes: 0,
            zeroed: false,
            fresh_pages: false,
        }
    }

    /// Bytes the workspace holds, what the layouts carve from.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the open layout sits on pages the allocator mapped fresh for
    /// it: a new allocation from [`FRESH_PAGES_FROM`] up. The first touch of
    /// each of its pages is a fault, so work that sweeps a table costs more
    /// than on a workspace kept from an earlier frame.
    /// Settled when the layout opens, so the per-block gate reads one field.
    pub(crate) fn on_fresh_pages(&self) -> bool {
        self.fresh_pages
    }

    /// Bytes the workspace has allocated, for heap accounting: its capacity
    /// with the padding that aligns its start, and an allocation a growth
    /// retired that has not been freed yet.
    pub(crate) fn heap_bytes(&self) -> usize {
        let allocated = |capacity: usize| {
            if capacity == 0 {
                0
            } else {
                allocation_layout(capacity).size()
            }
        };
        allocated(self.capacity) + self.retired.map_or(0, |(_, capacity)| allocated(capacity))
    }

    /// Starts a frame's layout. The context will carve `trailing_for(block)`
    /// bytes after the match finder, where `block` is `block_target` capped by
    /// the frame's window, and its input reaches the match finder as `ingest`
    /// says; nothing may be carved until [`Self::open`].
    pub(crate) fn begin_layout(
        &mut self,
        block_target: usize,
        trailing_for: fn(usize) -> usize,
        ingest: IngestPlan,
    ) {
        self.release_retired();
        self.block_target = block_target;
        self.trailing_for = trailing_for;
        self.ingest = ingest;
        self.sparse_table_bytes = 0;
        self.open = false;
    }

    /// Opens the layout for a match finder, as [`Self::open`], whose tables
    /// include `zero_tables` bytes of tables that start as zeros, for a frame
    /// expected to bring `expected_input` bytes.
    ///
    /// Zero tables larger than that input keep most of their pages untouched,
    /// so a new allocation that they make up most of is then taken zeroed and
    /// those tables left as they are: only the pages the frame indexes are
    /// ever faulted in. Otherwise a plain allocation that fills only the
    /// tables costs less: the input writes dense tables anyway, and an
    /// allocator zeroes a block it hands out again in full, the history and
    /// buffers included, which for a small frame is several times its tables.
    pub(crate) fn open_for_match_finder(
        &mut self,
        leading: usize,
        zero_tables: usize,
        window: usize,
        expected_input: usize,
    ) {
        self.sparse_table_bytes = if zero_tables > expected_input {
            zero_tables
        } else {
            0
        };
        self.open(leading, window);
    }

    /// Whether this layout has been opened.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// How the frame being laid out takes its input.
    pub(crate) fn ingest(&self) -> IngestPlan {
        self.ingest
    }

    /// The largest block a frame with a `window`-byte window carries: the
    /// ceiling [`Self::begin_layout`] set, capped by the window (upstream
    /// `blockSize = MIN(maxBlockSize, windowSize)`) and by a frame length the
    /// plan knows exactly, since no block holds more than the frame; at least
    /// one byte.
    pub(crate) fn block_for_window(&self, window: usize) -> usize {
        self.block_target
            .min(window)
            .min(self.ingest.exact_len().unwrap_or(usize::MAX))
            .max(1)
    }

    /// The block size the open layout reserved the trailing part for.
    pub(crate) fn block_capacity(&self) -> usize {
        self.block_capacity
    }

    /// Opens the layout with `leading` bytes for the caller's tables and
    /// history ahead of the trailing part sized for a frame with a
    /// `window`-byte window, allocating anew when the two together do not fit,
    /// or when the allocation has stayed far larger than the frames need. A new
    /// allocation discards every table carved before, so a holder whose table
    /// is not a continuation must reinitialise it; a history carries its bytes
    /// over (see [`HistoryBuf::bind`]).
    ///
    /// # Panics
    ///
    /// Panics when the layout is already open, or its size overflows `usize`.
    pub(crate) fn open(&mut self, leading: usize, window: usize) {
        assert!(!self.open, "a workspace layout opens once per frame");
        self.block_capacity = self.block_for_window(window);
        let total = leading
            .checked_add((self.trailing_for)(self.block_capacity))
            .expect("workspace size overflows usize");
        let reallocate = if total > self.capacity {
            true
        } else {
            // What is LEFT over, not the whole allocation, is held against
            // three needs, as upstream holds its free space against them
            // (`ZSTD_cwksp_check_too_large` -> `ZSTD_cwksp_check_available(ws,
            // needed * ZSTD_WORKSPACETOOLARGE_FACTOR)`). A need so large that
            // three of it overflow cannot be exceeded three times over by what
            // is left, so it is never too large.
            let too_large = total
                .checked_mul(TOO_LARGE_FACTOR)
                .is_some_and(|wasted| self.capacity - total >= wasted);
            // Counted only while it stays too large, and reset by any layout
            // it fits (upstream `ZSTD_cwksp_bump_oversized_duration`). Only
            // "more than the limit" is ever asked of the count, so pinning it
            // at the top of its range keeps the answer right on a context that
            // outlives four billion frames.
            self.oversized_layouts = if too_large {
                self.oversized_layouts.saturating_add(1)
            } else {
                0
            };
            self.oversized_layouts > TOO_LARGE_MAX_LAYOUTS
        };
        // A zeroed allocation costs nothing on pages the kernel hands out
        // fresh and a memset of all of it on memory the allocator recycles.
        // At `FRESH_PAGES_FROM` and above the pages are fresh, so it is taken
        // whatever the tables: filling them there faults in pages the frame
        // never indexes, 36-42% of the encode at 1 MiB and level 3 on musl
        // and i686, and 70% at 10 KiB on musl. Below it, where a context
        // rebuilt per frame lands on recycled memory, it is taken only when
        // the rest is at most a third of the sparse tables, which caps that
        // case at a third over filling them. A small frame's tables sit
        // beside buffers of their own size: at 10 KiB and level 1 they were
        // 56% of the workspace, and zeroing it all measured 23% slower than
        // filling them on glibc. A small input at a high level, where the
        // tables are most of it, keeps its untouched pages (78% at level 13).
        //
        // Carving the zero tables from a zeroed allocation of their own instead
        // hands the allocator that choice per table, and measured well on musl
        // and i686. It loses the one allocation this type exists for: two
        // allocations of similar size keep glibc's heap on its trim threshold
        // (twice the largest chunk freed), so a context rebuilt per frame gave
        // its pages back and faulted them in again on every frame, 3.9x slower
        // at 1 MiB and level 3.
        debug_assert!(
            self.sparse_table_bytes <= leading,
            "zero tables are part of the leading bytes"
        );
        let zero_all = (SYSTEM_ALLOCATOR && total >= FRESH_PAGES_FROM)
            || (self.sparse_table_bytes > 0
                && total - self.sparse_table_bytes <= self.sparse_table_bytes / 3);
        if reallocate {
            self.grow(total, zero_all);
        }
        self.zeroed = reallocate && zero_all;
        self.fresh_pages = SYSTEM_ALLOCATOR && self.zeroed && self.capacity >= FRESH_PAGES_FROM;
        self.front = 0;
        self.leading = leading;
        self.history_front = leading;
        self.back = self.capacity;
        self.open = true;
    }

    /// A table of `count` values that continues `previous` when it lands on the
    /// same bytes of the same allocation, returned with `true`; otherwise every
    /// value is set to `empty` and it is returned with `false`.
    ///
    /// A continuation holds only what its holder wrote: the holder filled the
    /// region when it was first carved and is the only one to have written it
    /// since, because every holder is laid out anew with every layout and no
    /// two regions of one layout share a byte.
    ///
    /// # Panics
    ///
    /// Panics when the layout is not open or was sized below what it carves.
    pub(crate) fn table_after<T: TableValue>(
        &mut self,
        count: usize,
        previous: &Region<T>,
        empty: T,
    ) -> (Region<T>, bool) {
        let offset = self.carve_front(region_bytes::<T>(count));
        let mut region = self.region_at::<T>(offset, count);
        let kept = count > 0
            && previous.generation == region.generation
            && previous.ptr == region.ptr
            && previous.len == count;
        // An allocation this layout made is zeroed and no region of it has
        // been written yet, so a table of zeros is already in place. Leaving
        // it unwritten keeps its pages demand-zero: a large table a small
        // frame barely touches faults in only the pages it indexes.
        if !kept && !(self.zeroed && empty.is_zero()) {
            region.fill(empty);
        }
        (region, kept)
    }

    /// A table of `count` values, every one set to `value`. Holders go
    /// through [`Table::bind`], which also reports a continuation; this is the
    /// bare carve the layout tests check.
    ///
    /// # Panics
    ///
    /// As [`Self::table_after`].
    #[cfg(test)]
    pub(crate) fn table<T: TableValue>(&mut self, count: usize, value: T) -> Region<T> {
        self.table_after(count, &Region::empty(), value).0
    }

    /// An empty buffer with room for `capacity` values, filled by pushing.
    ///
    /// # Panics
    ///
    /// Panics when the layout is not open or was sized below what it carves.
    pub(crate) fn buffer<T: Copy>(&mut self, capacity: usize) -> RegionVec<T> {
        assert!(
            self.open,
            "carving from a workspace layout that is not open"
        );
        let bytes = region_bytes::<T>(capacity);
        assert!(
            bytes <= self.back - self.leading,
            "workspace sized below its layout"
        );
        self.back -= bytes;
        RegionVec {
            region: self.region_at::<T>(self.back, capacity),
            len: 0,
        }
    }

    /// Room for `capacity` history bytes at the end of the leading part, after
    /// the tables, left unwritten: a [`HistoryBuf`] only ever reads what it
    /// wrote. Its place depends only on how much of the leading part the
    /// tables take, so a history can be bound before the tables are.
    fn history_room(&mut self, capacity: usize) -> Region<u8> {
        assert!(
            self.open,
            "carving from a workspace layout that is not open"
        );
        let bytes = region_bytes::<u8>(capacity);
        assert!(
            bytes <= self.history_front - self.front,
            "workspace sized below its layout"
        );
        self.history_front -= bytes;
        self.region_at::<u8>(self.history_front, capacity)
    }

    /// Advances the front cursor past `bytes`, returning where they start.
    fn carve_front(&mut self, bytes: usize) -> usize {
        assert!(
            self.open,
            "carving from a workspace layout that is not open"
        );
        assert!(
            bytes <= self.history_front - self.front,
            "workspace sized below its layout"
        );
        let offset = self.front;
        self.front += bytes;
        offset
    }

    fn region_at<T>(&self, offset: usize, count: usize) -> Region<T> {
        const {
            assert!(
                align_of::<T>() <= ALIGN,
                "a region is aligned to ALIGN at most"
            );
        }
        // SAFETY: the callers checked `offset + region_bytes::<T>(count)` against
        // the carving cursors, which never leave `0..=capacity`.
        let ptr = unsafe { self.ptr.as_ptr().add(offset) };
        Region {
            // SAFETY: an offset into a non-null allocation is non-null.
            ptr: unsafe { NonNull::new_unchecked(ptr.cast::<T>()) },
            len: count,
            generation: self.generation,
        }
    }

    /// Replaces the allocation with one of `bytes`, discarding every table:
    /// the generation moves on, so no later region can pass for a continuation
    /// of one carved before. The old allocation is retired rather than freed,
    /// until the history has carried its bytes out of it.
    fn grow(&mut self, bytes: usize, zeroed: bool) {
        self.release_retired();
        if self.capacity != 0 {
            self.retired = Some((self.base, self.capacity));
            self.base = NonNull::dangling();
            self.ptr = NonNull::<Aligned>::dangling().cast();
            self.capacity = 0;
        }
        self.generation += 1;
        self.oversized_layouts = 0;
        if bytes == 0 {
            return;
        }
        let layout = allocation_layout(bytes);
        // Zeroed for sparse tables (see `open`), and at byte alignment so the
        // allocator can take it as a `calloc`: a large one comes back as fresh
        // pages the kernel zeroes on first touch. An over-aligned zeroed
        // request is a plain allocation followed by a memset of all of it,
        // which faults in every page up front, so the start is aligned by hand
        // instead.
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe {
            if zeroed {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };
        let Some(base) = NonNull::new(raw) else {
            handle_alloc_error(layout);
        };
        let offset = raw.align_offset(ALIGN);
        debug_assert!(offset < ALIGN, "a byte pointer aligns within ALIGN bytes");
        self.base = base;
        // SAFETY: `offset < ALIGN`, and the allocation holds `bytes + ALIGN - 1`
        // bytes, so the aligned start leaves `bytes` of them after it.
        self.ptr = unsafe { NonNull::new_unchecked(raw.add(offset)) };
        self.capacity = bytes;
    }

    /// Frees the allocation a growth retired. Every holder has been laid out
    /// in the new one by the time this runs.
    pub(crate) fn release_retired(&mut self) {
        if let Some((base, capacity)) = self.retired.take() {
            // SAFETY: `grow` retired this allocation and `take` dropped the
            // only record of it.
            unsafe { free(base, capacity) };
        }
    }

    fn release(&mut self) {
        self.release_retired();
        if self.capacity == 0 {
            return;
        }
        // SAFETY: the current allocation; `capacity` is reset below so it is
        // never freed twice.
        unsafe { free(self.base, self.capacity) };
        self.base = NonNull::dangling();
        self.ptr = NonNull::<Aligned>::dangling().cast();
        self.capacity = 0;
    }
}

/// The allocation behind a workspace of `capacity` usable bytes: byte-aligned,
/// with room to align the start to [`ALIGN`] by hand.
fn allocation_layout(capacity: usize) -> Layout {
    let bytes = capacity
        .checked_add(ALIGN - 1)
        .expect("workspace size overflows usize");
    Layout::from_size_align(bytes, 1).expect("workspace size overflows isize")
}

/// Frees a workspace allocation of `capacity` usable bytes.
///
/// # Safety
///
/// `base` must be the start of an allocation `grow` made for `capacity`
/// bytes, not yet freed, and the caller must drop its handle to it.
unsafe fn free(base: NonNull<u8>, capacity: usize) {
    // SAFETY: the caller's contract: this layout, allocated and live.
    unsafe { dealloc(base.as_ptr(), allocation_layout(capacity)) };
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.release();
    }
}

/// The trailing part of a layout with no block buffers behind the tables: a
/// match finder laying out a workspace of its own, or one nobody began.
pub(crate) fn no_trailing(_block: usize) -> usize {
    0
}

/// A type whose alignment is [`ALIGN`], for the dangling pointer of an empty
/// workspace.
#[repr(align(64))]
struct Aligned;

const _: () = assert!(align_of::<Aligned>() == ALIGN);

/// A value a workspace table holds: a plain integer, whose zero is all-zero
/// bytes, so a zeroed allocation already holds a table of zeros.
pub(crate) trait TableValue: Copy {
    fn is_zero(self) -> bool;
}

macro_rules! table_value {
    ($($int:ty),*) => {$(
        impl TableValue for $int {
            fn is_zero(self) -> bool {
                self == 0
            }
        }
    )*};
}

table_value!(u8, u32, u64);

/// A fixed-length run of values in a workspace.
///
/// Holders keep it for as long as the layout it was carved in: every holder is
/// bound again whenever the workspace is laid out anew, and the workspace
/// outlives every holder because both live in the same context.
pub(crate) struct Region<T> {
    ptr: NonNull<T>,
    len: usize,
    /// The allocation it was carved from; `0` for no allocation.
    generation: u64,
}

// SAFETY: a region is the sole handle to its values; a layout never hands the
// same bytes to two holders.
unsafe impl<T: Send> Send for Region<T> {}
// SAFETY: `&Region` only reads.
unsafe impl<T: Sync> Sync for Region<T> {}

impl<T> Region<T> {
    /// A region of no values, the state of a holder before its first bind.
    pub(crate) const fn empty() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
            generation: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr.as_ptr()
    }

    pub(crate) fn as_ptr(&self) -> *const T {
        self.ptr.as_ptr()
    }
}

impl<T: Copy> Region<T> {
    pub(crate) fn as_slice(&self) -> &[T] {
        // SAFETY: the region is live (see the type's contract) and every value
        // was written when it was carved or by its holder since; `&self`
        // excludes a concurrent `&mut`.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, with `&mut self` making the access exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Sets every value, including ones never written, so the region is
    /// initialised before any slice of it exists.
    fn fill(&mut self, value: T) {
        let base = self.ptr.as_ptr();
        for i in 0..self.len {
            // SAFETY: `i < len`, inside the region; `write` never reads the
            // value it replaces.
            unsafe { base.add(i).write(value) };
        }
    }
}

/// A match-finder table: a region of the context's workspace, or an allocation
/// of its own for a copy that must outlive a layout (a dictionary snapshot, a
/// table a caller built by hand).
///
/// Reads and writes go through one pointer either way, so the hot loops never
/// branch on where the values live.
pub(crate) struct Table<T: Copy> {
    view: Region<T>,
    /// Backs `view` when the table owns its values; empty for a workspace table.
    own: Vec<T>,
}

impl<T: Copy> Table<T> {
    /// A table with no values.
    pub(crate) const fn empty() -> Self {
        Self {
            view: Region::empty(),
            own: Vec::new(),
        }
    }

    /// A table owning `values`.
    pub(crate) fn owned(mut values: Vec<T>) -> Self {
        let view = Region {
            // SAFETY: a `Vec`'s buffer pointer is never null.
            ptr: unsafe { NonNull::new_unchecked(values.as_mut_ptr()) },
            len: values.len(),
            generation: 0,
        };
        Self { view, own: values }
    }

    /// Lays the table out in `workspace` at `count` values. Returns `true` when
    /// it continues the region it had, contents intact; otherwise every value
    /// is `empty`.
    pub(crate) fn bind(&mut self, workspace: &mut Workspace, count: usize, empty: T) -> bool
    where
        T: TableValue,
    {
        let (view, kept) = workspace.table_after(count, &self.view, empty);
        self.view = view;
        self.own = Vec::new();
        kept
    }

    pub(crate) fn len(&self) -> usize {
        self.view.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.view.len == 0
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        self.view.as_slice()
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        self.view.as_mut_slice()
    }

    /// Heap bytes the table owns; a workspace table owns none.
    pub(crate) fn owned_bytes(&self) -> usize {
        self.own.capacity() * size_of::<T>()
    }

    /// Moves the values into an allocation of their own, for a matcher leaving
    /// the context whose workspace holds them.
    pub(crate) fn leave_workspace(&mut self) {
        if self.own.is_empty() && !self.is_empty() {
            *self = self.clone();
        }
    }
}

impl<T: Copy> Clone for Table<T> {
    /// A copy owning its values, whatever the source's storage.
    fn clone(&self) -> Self {
        Self::owned(self.as_slice().to_vec())
    }

    /// Copies the values in place when the lengths match, so restoring a
    /// snapshot into a workspace table keeps it in the workspace.
    fn clone_from(&mut self, source: &Self) {
        if self.len() == source.len() {
            self.as_mut_slice().copy_from_slice(source.as_slice());
        } else {
            *self = source.clone();
        }
    }
}

impl<T: Copy> Default for Table<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T: Copy> core::ops::Deref for Table<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy> core::ops::DerefMut for Table<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

/// A match finder's input history: bytes appended block by block, trimmed at
/// the front as the window slides, used like a `Vec<u8>`.
///
/// The encoder reads input straight into it through
/// [`Matcher::fill_in_place`], so a [`Matcher`] keeps its window in one. A
/// matcher defined outside this crate owns its buffer outright.
///
/// The crate's own match finders keep theirs in the compression context's
/// single allocation at the capacity the frame was laid out for; one moves
/// into an allocation of its own if a frame brings more than that (a size
/// hint that undercounted). Only the first `len` bytes are ever read; the rest
/// of the room is left unwritten. Its bytes survive every layout: `bind`
/// carries them into the new room wherever it lands, since a matcher keeps its
/// dictionary at the head of the history from one frame to the next.
///
/// # Examples
/// ```
/// use structured_zstd::encoding::HistoryBuf;
///
/// let mut history = HistoryBuf::new();
/// history.extend_from_slice(b"older block, newer block");
/// history.drain_front(b"older block, ".len());
/// assert_eq!(&history[..], b"newer block");
/// ```
///
/// [`Matcher::fill_in_place`]: crate::encoding::Matcher::fill_in_place
/// [`Matcher`]: crate::encoding::Matcher
pub struct HistoryBuf {
    ptr: NonNull<u8>,
    capacity: usize,
    len: usize,
    /// Backs the room when the buffer owns it: its length stays `0` and its
    /// capacity is the room, which this type manages.
    own: Vec<u8>,
}

// SAFETY: the buffer is the sole handle to its room, whether carved from the
// context's workspace or owned.
unsafe impl Send for HistoryBuf {}
// SAFETY: `&HistoryBuf` only reads.
unsafe impl Sync for HistoryBuf {}

impl HistoryBuf {
    /// An empty buffer with no room.
    pub const fn new() -> Self {
        Self {
            ptr: NonNull::dangling(),
            capacity: 0,
            len: 0,
            own: Vec::new(),
        }
    }

    /// Workspace bytes [`Self::bind`] carves for a frame that needs `capacity`
    /// bytes: never less than the buffer already holds.
    pub(crate) fn workspace_bytes(&self, capacity: usize) -> usize {
        region_bytes::<u8>(capacity.max(self.len))
    }

    /// Lays the room out in `workspace` after the tables, at `capacity` bytes
    /// or the length already held if that is more, carrying the bytes over.
    ///
    /// Binds before the tables of the same layout, so nothing has written the
    /// workspace since the previous layout: the old room is intact whether it
    /// is where it was, elsewhere in the same allocation, in the allocation the
    /// workspace retired, or owned.
    pub(crate) fn bind(&mut self, workspace: &mut Workspace, capacity: usize) {
        let capacity = capacity.max(self.len);
        let room = workspace.history_room(capacity);
        if self.ptr != room.ptr {
            // SAFETY: the old room is live and holds `len` written bytes (see
            // above), and the new one has room for them; `copy` allows the two
            // to overlap within one allocation.
            unsafe { core::ptr::copy(self.ptr.as_ptr(), room.ptr.as_ptr(), self.len) };
        }
        self.ptr = room.ptr;
        self.capacity = capacity;
        self.own = Vec::new();
        workspace.release_retired();
    }

    /// Moves the bytes into a room of their own, for a matcher leaving the
    /// context whose workspace holds them.
    pub(crate) fn leave_workspace(&mut self) {
        if self.own.capacity() == 0 && self.capacity != 0 {
            self.move_room(self.len);
        }
    }

    /// Bytes held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no bytes are held.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes the buffer holds room for without moving.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Heap bytes the buffer owns; room in a context's workspace is counted
    /// with the workspace.
    pub(crate) fn owned_bytes(&self) -> usize {
        self.own.capacity()
    }

    /// Drops every byte, keeping the room.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Keeps the first `len` bytes; a no-op when fewer are held.
    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }

    /// Makes room for `additional` more bytes, at least doubling the room when
    /// it has to move, as a `Vec` does.
    ///
    /// # Panics
    ///
    /// Panics when the length would overflow `usize`.
    pub fn reserve(&mut self, additional: usize) {
        let needed = self
            .len
            .checked_add(additional)
            .expect("history length overflows usize");
        if needed > self.capacity {
            // A room too large to double is outgrown by exactly what is needed.
            let doubled = self.capacity.checked_mul(2).unwrap_or(needed);
            self.move_room(needed.max(doubled));
        }
    }

    /// Makes room for exactly `additional` more bytes when it has to move.
    pub(crate) fn reserve_exact(&mut self, additional: usize) {
        let needed = self
            .len
            .checked_add(additional)
            .expect("history length overflows usize");
        if needed > self.capacity {
            self.move_room(needed);
        }
    }

    /// Appends `bytes`.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.reserve(bytes.len());
        // SAFETY: `reserve` left room for `bytes.len()` more bytes past `len`,
        // and `bytes` is borrowed from elsewhere, never from this room.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.ptr.as_ptr().add(self.len),
                bytes.len(),
            );
        }
        self.len += bytes.len();
    }

    pub(crate) fn push(&mut self, byte: u8) {
        self.reserve(1);
        // SAFETY: `reserve` left room for one more byte past `len`.
        unsafe { self.ptr.as_ptr().add(self.len).write(byte) };
        self.len += 1;
    }

    /// Sets the length to `len`, writing `value` into any bytes it adds.
    pub(crate) fn resize(&mut self, len: usize, value: u8) {
        if len > self.len {
            self.reserve(len - self.len);
            // SAFETY: `reserve` left room up to `len`.
            unsafe {
                core::ptr::write_bytes(self.ptr.as_ptr().add(self.len), value, len - self.len);
            }
        }
        self.len = len;
    }

    /// Drops the first `count` bytes, moving the rest to the front.
    ///
    /// # Panics
    ///
    /// Panics when `count` exceeds the length.
    pub fn drain_front(&mut self, count: usize) {
        assert!(count <= self.len, "draining past the history's end");
        // SAFETY: both ranges lie within the written `len` bytes; `copy`
        // handles the overlap.
        unsafe {
            core::ptr::copy(
                self.ptr.as_ptr().add(count),
                self.ptr.as_ptr(),
                self.len - count,
            );
        }
        self.len -= count;
    }

    /// Moves the written bytes into an owned room of `capacity` bytes.
    fn move_room(&mut self, capacity: usize) {
        let mut own: Vec<u8> = Vec::with_capacity(capacity);
        // SAFETY: the new room has space for `len` bytes, and the old room
        // holds them; the two allocations are distinct.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), own.as_mut_ptr(), self.len) };
        // SAFETY: a `Vec`'s buffer pointer is never null.
        self.ptr = unsafe { NonNull::new_unchecked(own.as_mut_ptr()) };
        self.capacity = own.capacity();
        self.own = own;
    }
}

impl Default for HistoryBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for HistoryBuf {
    /// A copy owning its bytes, whatever the source's storage.
    fn clone(&self) -> Self {
        let mut copy = Self::new();
        copy.extend_from_slice(self);
        copy
    }

    /// Copies the bytes into the room this buffer already has when they fit,
    /// so restoring a snapshot keeps the history where it was laid out.
    fn clone_from(&mut self, source: &Self) {
        self.clear();
        self.extend_from_slice(source);
    }
}

/// A buffer owning `bytes`, for tests that set a history up by hand.
#[cfg(test)]
impl From<Vec<u8>> for HistoryBuf {
    fn from(mut bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        // The owned room keeps length 0 (see the field); the bytes stay
        // written, and this buffer's `len` claims them.
        // SAFETY: shrinking a `Vec`'s length to 0 needs no drop for `u8`.
        unsafe { bytes.set_len(0) };
        Self {
            // SAFETY: a `Vec`'s buffer pointer is never null.
            ptr: unsafe { NonNull::new_unchecked(bytes.as_mut_ptr()) },
            capacity: bytes.capacity(),
            len,
            own: bytes,
        }
    }
}

impl core::ops::Deref for HistoryBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: the first `len` bytes were written by this buffer, and its
        // room is live.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl core::ops::DerefMut for HistoryBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `deref`, with `&mut self` making the access exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl core::fmt::Debug for HistoryBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HistoryBuf")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .finish()
    }
}

/// What a match finder settles about its history before the next frame is laid
/// out: the floor past every position the previous frame indexed, which is
/// read off the history's length, and the resident dictionary prefix the reset
/// may keep. Taking both first lets the history drop everything else before
/// the layout sizes its room and carries its bytes into it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetiredHistory {
    pub(crate) next_floor: usize,
    pub(crate) kept: Option<usize>,
}

/// A byte buffer a block is read into: the match finder's history, or the
/// output of an uncompressed frame, which carries its blocks raw.
pub(crate) trait IngestBuffer: core::ops::DerefMut<Target = [u8]> {
    fn extend_from_slice(&mut self, bytes: &[u8]);
    fn resize(&mut self, len: usize, value: u8);
    fn truncate(&mut self, len: usize);
    fn push(&mut self, byte: u8);
}

impl IngestBuffer for Vec<u8> {
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        Vec::extend_from_slice(self, bytes);
    }

    fn resize(&mut self, len: usize, value: u8) {
        Vec::resize(self, len, value);
    }

    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len);
    }

    fn push(&mut self, byte: u8) {
        Vec::push(self, byte);
    }
}

impl IngestBuffer for HistoryBuf {
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        HistoryBuf::extend_from_slice(self, bytes);
    }

    fn resize(&mut self, len: usize, value: u8) {
        HistoryBuf::resize(self, len, value);
    }

    fn truncate(&mut self, len: usize) {
        HistoryBuf::truncate(self, len);
    }

    fn push(&mut self, byte: u8) {
        HistoryBuf::push(self, byte);
    }
}

/// A buffer in a workspace with a fixed capacity and a length, used like a
/// `Vec` that never reallocates.
pub(crate) struct RegionVec<T> {
    region: Region<T>,
    len: usize,
}

impl<T: Copy> RegionVec<T> {
    /// A buffer with no room, the state of a holder before its first bind.
    pub(crate) const fn empty() -> Self {
        Self {
            region: Region::empty(),
            len: 0,
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.region.len
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    /// # Panics
    ///
    /// Panics when the buffer is full: its capacity was sized from the most
    /// the frame can put in it, so a full buffer is a sizing bug.
    #[inline]
    pub(crate) fn push(&mut self, value: T) {
        if self.len == self.region.len {
            capacity_exhausted();
        }
        // SAFETY: `len < capacity`, so the slot is inside the region.
        unsafe { self.region.as_mut_ptr().add(self.len).write(value) };
        self.len += 1;
    }

    /// # Panics
    ///
    /// Panics when `values` does not fit, as [`Self::push`].
    pub(crate) fn extend_from_slice(&mut self, values: &[T]) {
        if values.len() > self.region.len - self.len {
            capacity_exhausted();
        }
        // SAFETY: the destination range `len..len + values.len()` is inside the
        // region by the check above, and a workspace region never overlaps a
        // slice borrowed from elsewhere.
        unsafe {
            core::ptr::copy_nonoverlapping(
                values.as_ptr(),
                self.region.as_mut_ptr().add(self.len),
                values.len(),
            );
        }
        self.len += values.len();
    }

    /// Pointer to the first value, for writes past the length that
    /// [`Self::set_len`] then claims.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut T {
        self.region.as_mut_ptr()
    }

    /// # Safety
    ///
    /// `len` must not exceed the capacity, and every value below it must have
    /// been written.
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.region.len);
        self.len = len;
    }
}

impl<T: Copy> core::ops::Deref for RegionVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` values were written by `push`,
        // `extend_from_slice` or a caller of `set_len`, and the region is live.
        unsafe { core::slice::from_raw_parts(self.region.as_ptr(), self.len) }
    }
}

impl<T: Copy> core::ops::DerefMut for RegionVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, with `&mut self` making the access exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.region.as_mut_ptr(), self.len) }
    }
}

impl<T: Copy> Default for RegionVec<T> {
    fn default() -> Self {
        Self::empty()
    }
}

#[cold]
#[inline(never)]
fn capacity_exhausted() -> ! {
    panic!("workspace buffer sized below what the frame put in it")
}

#[cfg(test)]
mod tests;
