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
//! the back, and every region starts on a [`ALIGN`]-byte boundary.

use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use core::marker::PhantomData;
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

/// The allocation a context's regions live in. Grows when a frame needs more
/// than it holds and is otherwise kept, so a context allocates once for as
/// long as its parameters do not grow.
pub(crate) struct Workspace {
    ptr: NonNull<u8>,
    capacity: usize,
}

// SAFETY: the workspace owns its allocation outright; the raw pointer is what
// makes the type `!Send`/`!Sync` by default, not any shared state.
unsafe impl Send for Workspace {}
// SAFETY: as above; `&Workspace` exposes no access to the bytes.
unsafe impl Sync for Workspace {}

impl Workspace {
    /// An empty workspace. Allocates nothing until [`Self::ensure`].
    pub(crate) const fn new() -> Self {
        Self {
            ptr: NonNull::<Aligned>::dangling().cast(),
            capacity: 0,
        }
    }

    /// Bytes the workspace holds.
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Makes the workspace hold at least `bytes`. Returns `true` when it had to
    /// reallocate, in which case every region carved before is gone and its
    /// holders must be bound again; the new memory is uninitialised.
    pub(crate) fn ensure(&mut self, bytes: usize) -> bool {
        if bytes <= self.capacity {
            return false;
        }
        self.release();
        let layout = Layout::from_size_align(bytes, ALIGN).expect("workspace size overflows isize");
        // SAFETY: `layout` has a non-zero size, since `bytes > capacity >= 0`.
        let raw = unsafe { alloc(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(layout);
        };
        self.ptr = ptr;
        self.capacity = bytes;
        true
    }

    /// A carver over the whole workspace, starting empty. Regions carved by an
    /// earlier carver overlap the ones this one hands out, so each call is the
    /// start of a new layout that every holder is bound into again.
    pub(crate) fn carver(&mut self) -> WorkspaceCarver<'_> {
        WorkspaceCarver {
            base: self.ptr,
            front: 0,
            back: self.capacity,
            _workspace: PhantomData,
        }
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

/// A type whose alignment is [`ALIGN`], for the dangling pointer of an empty
/// workspace.
#[repr(align(64))]
struct Aligned;

const _: () = assert!(align_of::<Aligned>() == ALIGN);

/// Hands out the regions of one workspace layout. Tables come from the front
/// and buffers from the back, as in upstream `ZSTD_cwksp`.
///
/// Matchers built into this crate take their tables from the context's
/// workspace through [`crate::encoding::Matcher::bind_workspace`]; a matcher
/// defined elsewhere keeps its own allocations and ignores it.
pub struct WorkspaceCarver<'a> {
    base: NonNull<u8>,
    front: usize,
    back: usize,
    _workspace: PhantomData<&'a mut Workspace>,
}

impl WorkspaceCarver<'_> {
    /// Bytes not yet carved.
    pub(crate) fn remaining(&self) -> usize {
        self.back - self.front
    }

    /// A table of `count` values, every one set to `value`.
    ///
    /// # Panics
    ///
    /// Panics when the workspace was sized smaller than its layout needs, which
    /// is a sizing bug in the caller.
    pub(crate) fn table<T: Copy>(&mut self, count: usize, value: T) -> Region<T> {
        let bytes = region_bytes::<T>(count);
        assert!(
            bytes <= self.remaining(),
            "workspace sized below its layout"
        );
        let region = self.region_at::<T>(self.front, count);
        self.front += bytes;
        let mut region = region;
        region.initialise(value);
        region
    }

    /// An empty buffer with room for `capacity` values, filled by pushing.
    ///
    /// # Panics
    ///
    /// Panics when the workspace was sized smaller than its layout needs.
    pub(crate) fn buffer<T: Copy>(&mut self, capacity: usize) -> RegionVec<T> {
        let bytes = region_bytes::<T>(capacity);
        assert!(
            bytes <= self.remaining(),
            "workspace sized below its layout"
        );
        self.back -= bytes;
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
        // SAFETY: `offset + region_bytes::<T>(count) <= capacity` was checked by
        // the caller, so the pointer stays inside the allocation.
        let ptr = unsafe { self.base.as_ptr().add(offset) };
        Region {
            // SAFETY: an offset into a non-null allocation is non-null.
            ptr: unsafe { NonNull::new_unchecked(ptr.cast::<T>()) },
            len: count,
        }
    }
}

/// A fixed-length run of values in a workspace.
///
/// Holders keep it for as long as the layout it was carved in: the context
/// binds every holder again whenever it lays the workspace out anew, and the
/// workspace outlives every holder because both live in the same context.
pub(crate) struct Region<T> {
    ptr: NonNull<T>,
    len: usize,
}

// SAFETY: a region is the sole handle to its values; the carver never hands
// the same bytes to two holders within one layout.
unsafe impl<T: Send> Send for Region<T> {}
// SAFETY: `&Region` only reads.
unsafe impl<T: Sync> Sync for Region<T> {}

impl<T> Region<T> {
    /// A region of no values, the state of a holder before its first bind.
    pub(crate) const fn empty() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
        }
    }

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
        // SAFETY: the region is live (see the type's contract), every value was
        // written by `initialise` or by a holder since, and `&self` excludes a
        // concurrent `&mut`.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, with `&mut self` making the access exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Sets every value, including ones never written, so the region is
    /// initialised before any slice of it exists.
    fn initialise(&mut self, value: T) {
        let base = self.ptr.as_ptr();
        for i in 0..self.len {
            // SAFETY: `i < len`, inside the region; `write` never reads the
            // uninitialised value it replaces.
            unsafe { base.add(i).write(value) };
        }
    }
}

impl<T> Default for Region<T> {
    fn default() -> Self {
        Self::empty()
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

    pub(crate) fn truncate(&mut self, len: usize) {
        if len < self.len {
            self.len = len;
        }
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
