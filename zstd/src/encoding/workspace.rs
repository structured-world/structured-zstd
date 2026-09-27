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
//! The layout follows upstream: tables are carved from the front, buffers from
//! the back, and every region starts on an [`ALIGN`]-byte boundary. A frame lays
//! the workspace out in two parts: the match finder opens it with the bytes its
//! tables need and carves them, then the context carves its per-block buffers
//! from what the opening reserved for it.

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
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

/// The single allocation a compression context carves its match-finder tables
/// and per-block buffers from.
///
/// It grows when a frame needs more than it holds and is kept otherwise, so a
/// context allocates once for as long as its parameters do not grow. A table
/// laid out at the same place in the same allocation as on the previous frame
/// keeps its contents, which is what lets a match finder carry a table across
/// frames.
///
/// `pub` only so it can appear in [`Matcher::reset_in_workspace`]; the module
/// is private, so no code outside this crate can name, build or receive one.
///
/// [`Matcher::reset_in_workspace`]: crate::encoding::Matcher::reset_in_workspace
pub struct Workspace {
    ptr: NonNull<u8>,
    capacity: usize,
    /// Counts allocations; a region is only the continuation of an earlier one
    /// if both were carved from the same allocation.
    generation: u64,
    /// Every byte below this offset has been written since the allocation was
    /// made, so a region inside it may be read before its holder writes it.
    valid_front: usize,
    /// Carving cursors of the current layout.
    front: usize,
    back: usize,
    /// The block ceiling the context set before the window was known, and the
    /// bytes it carves after the match finder for a given block size.
    block_target: usize,
    trailing_for: fn(usize) -> usize,
    /// The block size the open layout's trailing part was sized for.
    block_capacity: usize,
    open: bool,
    /// Layouts since the allocation was last made, for giving back one that
    /// has stayed far larger than the frames need.
    layouts_since_allocation: u32,
}

/// Upstream `ZSTD_WORKSPACETOOLARGE_FACTOR` / `_MAXDURATION`
/// (`zstd_internal.h:258-265`): a workspace left with at least three times a
/// frame's need unused, more than this many frames after it was allocated, is
/// allocated again at the need (`ZSTD_resetCCtx_internal`,
/// `zstd_compress.c:2154-2159`).
const TOO_LARGE_FACTOR: usize = 3;
const TOO_LARGE_MAX_LAYOUTS: u32 = 128;

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
            capacity: 0,
            generation: 0,
            valid_front: 0,
            front: 0,
            back: 0,
            block_target: 0,
            trailing_for: no_trailing,
            block_capacity: 0,
            open: false,
            layouts_since_allocation: 0,
        }
    }

    /// Bytes the workspace holds.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Starts a frame's layout. The context will carve `trailing_for(block)`
    /// bytes after the match finder, where `block` is `block_target` capped by
    /// the frame's window; nothing may be carved until [`Self::open`].
    pub(crate) fn begin_layout(&mut self, block_target: usize, trailing_for: fn(usize) -> usize) {
        self.block_target = block_target;
        self.trailing_for = trailing_for;
        self.open = false;
    }

    /// Whether this layout has been opened.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// The block size the open layout reserved the trailing part for: the
    /// ceiling [`Self::begin_layout`] set, capped by the window [`Self::open`]
    /// was given.
    pub(crate) fn block_capacity(&self) -> usize {
        self.block_capacity
    }

    /// Opens the layout with `leading` bytes for the caller's tables ahead of
    /// the trailing part sized for a frame with a `window`-byte window,
    /// allocating anew when the two together do not fit, or when the
    /// allocation has stayed far larger than the frames need. A new allocation
    /// discards every region carved before, so a holder whose region is not a
    /// continuation must reinitialise it.
    ///
    /// # Panics
    ///
    /// Panics when the layout is already open, or its size overflows `usize`.
    pub(crate) fn open(&mut self, leading: usize, window: usize) {
        assert!(!self.open, "a workspace layout opens once per frame");
        // Upstream `blockSize = MIN(maxBlockSize, windowSize)`; a block holds
        // at least one byte.
        self.block_capacity = self.block_target.min(window).max(1);
        let total = leading
            .checked_add((self.trailing_for)(self.block_capacity))
            .expect("workspace size overflows usize");
        // Only "more than the limit" is ever asked of the count, so pinning it
        // at the top of its range keeps the answer right on a context that
        // outlives four billion frames.
        self.layouts_since_allocation = self.layouts_since_allocation.saturating_add(1);
        let reallocate = if total > self.capacity {
            true
        } else {
            // A need so large that three of it overflow cannot be exceeded
            // three times over by what is left, so it is never wasteful.
            self.layouts_since_allocation > TOO_LARGE_MAX_LAYOUTS
                && total
                    .checked_mul(TOO_LARGE_FACTOR)
                    .is_some_and(|wasted| self.capacity - total >= wasted)
        };
        if reallocate {
            self.grow(total);
        }
        self.front = 0;
        self.back = self.capacity;
        self.open = true;
    }

    /// A table of `count` values that continues `previous` when it lands on the
    /// same bytes of the same allocation, returned with `true`; otherwise every
    /// value is set to `empty` and it is returned with `false`.
    ///
    /// # Panics
    ///
    /// Panics when the layout is not open or was sized below what it carves.
    pub(crate) fn table_after<T: Copy>(
        &mut self,
        count: usize,
        previous: &Region<T>,
        empty: T,
    ) -> (Region<T>, bool) {
        assert!(
            self.open,
            "carving from a workspace layout that is not open"
        );
        let bytes = region_bytes::<T>(count);
        assert!(
            bytes <= self.back - self.front,
            "workspace sized below its layout"
        );
        let mut region = self.region_at::<T>(self.front, count);
        let end = self.front + bytes;
        let kept = count > 0
            && previous.generation == region.generation
            && previous.ptr == region.ptr
            && previous.len == count
            && end <= self.valid_front;
        if !kept {
            region.fill(empty);
        }
        self.front = end;
        self.valid_front = self.valid_front.max(end);
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
    pub(crate) fn table<T: Copy>(&mut self, count: usize, value: T) -> Region<T> {
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
            bytes <= self.back - self.front,
            "workspace sized below its layout"
        );
        self.back -= bytes;
        // A buffer may take bytes a table of an earlier layout had written; it
        // only ever writes them, so the written prefix stays written.
        RegionVec {
            region: self.region_at::<T>(self.back, capacity),
            len: 0,
        }
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

    /// Replaces the allocation with one of `bytes`, discarding every region:
    /// the generation moves on, so no later region can pass for a continuation
    /// of one carved before, even at the same address.
    fn grow(&mut self, bytes: usize) {
        self.release();
        self.generation += 1;
        self.valid_front = 0;
        self.layouts_since_allocation = 0;
        if bytes == 0 {
            return;
        }
        let layout = Layout::from_size_align(bytes, ALIGN).expect("workspace size overflows isize");
        // SAFETY: `layout` has a non-zero size, checked above.
        let raw = unsafe { alloc(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(layout);
        };
        self.ptr = ptr;
        self.capacity = bytes;
    }

    fn release(&mut self) {
        if self.capacity == 0 {
            return;
        }
        let layout = Layout::from_size_align(self.capacity, ALIGN)
            .expect("the layout was valid when allocated");
        // SAFETY: `ptr` came from `alloc` with this same layout and has not been
        // freed; `capacity` is reset below so it is never freed twice.
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
        self.ptr = NonNull::<Aligned>::dangling().cast();
        self.capacity = 0;
    }
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

impl<T> Default for Region<T> {
    fn default() -> Self {
        Self::empty()
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
    pub(crate) fn bind(&mut self, workspace: &mut Workspace, count: usize, empty: T) -> bool {
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
