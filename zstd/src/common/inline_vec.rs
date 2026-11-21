//! Fixed-capacity, allocation-free stand-in for `Vec` scratch whose size the
//! format bounds: the Huffman encoder's tree and weights on every build, and the
//! decoders' entropy tables under the `caller-workspace` feature, so those can
//! live in caller-owned memory.

use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

/// A `Vec`-shaped buffer of at most `N` elements stored inline.
///
/// Every user is bounded by the format below `N` and checks its input against
/// that bound before appending (the Huffman weight count, the FSE alphabet), so
/// growing past `N` is a broken invariant: the bounds-checked write panics
/// rather than leaving the buffer.
pub(crate) struct InlineVec<T: Copy, const N: usize> {
    len: usize,
    buf: [MaybeUninit<T>; N],
}

impl<T: Copy, const N: usize> InlineVec<T, N> {
    pub(crate) const fn new() -> Self {
        Self {
            len: 0,
            buf: [const { MaybeUninit::uninit() }; N],
        }
    }

    /// `Vec::with_capacity` signature; the capacity is fixed at `N`.
    #[cfg(feature = "caller-workspace")]
    pub(crate) const fn with_capacity(_capacity: usize) -> Self {
        Self::new()
    }

    /// Initialise an empty buffer in place without building it on the stack.
    ///
    /// # Safety
    /// `this` must be valid for writes and aligned for `Self`.
    #[cfg(feature = "caller-workspace")]
    pub(crate) unsafe fn init_in_place(this: *mut Self) {
        // SAFETY: the caller guarantees `this` is writable and aligned; the
        // element array is `MaybeUninit` and needs no initialisation.
        unsafe { core::ptr::addr_of_mut!((*this).len).write(0) };
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(any(feature = "caller-workspace", test))]
    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    /// No-op: the storage is fixed. Callers size their requests below `N`.
    #[inline(always)]
    pub(crate) fn reserve(&mut self, additional: usize) {
        debug_assert!(self.len + additional <= N);
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, value: T) {
        self.buf[self.len] = MaybeUninit::new(value);
        self.len += 1;
    }

    pub(crate) fn resize(&mut self, new_len: usize, value: T) {
        if new_len > self.len {
            for slot in &mut self.buf[self.len..new_len] {
                *slot = MaybeUninit::new(value);
            }
        }
        self.len = new_len;
    }

    pub(crate) fn extend_from_slice(&mut self, values: &[T]) {
        let end = self.len + values.len();
        for (slot, &value) in self.buf[self.len..end].iter_mut().zip(values) {
            *slot = MaybeUninit::new(value);
        }
        self.len = end;
    }

    /// The writable tail past the live prefix.
    #[cfg(feature = "caller-workspace")]
    #[inline(always)]
    pub(crate) fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<T>] {
        &mut self.buf[self.len..]
    }

    /// # Safety
    /// `new_len <= N`, and every element below `new_len` is initialised.
    #[inline(always)]
    pub(crate) unsafe fn set_len(&mut self, new_len: usize) {
        assert!(new_len <= N, "InlineVec::set_len past its capacity");
        self.len = new_len;
    }

    #[cfg(any(feature = "caller-workspace", test))]
    #[inline(always)]
    pub(crate) fn as_slice(&self) -> &[T] {
        self
    }

    /// The whole storage's first element, valid for writes up to `N`
    /// elements: a pointer taken through the slice would only cover the live
    /// prefix, which is empty while a caller fills the buffer by hand.
    #[inline(always)]
    pub(crate) fn as_mut_ptr(&mut self) -> *mut T {
        self.buf.as_mut_ptr().cast::<T>()
    }

    #[inline(always)]
    pub(crate) fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

impl<T: Copy, const N: usize> Deref for InlineVec<T, N> {
    type Target = [T];

    #[inline(always)]
    fn deref(&self) -> &[T] {
        debug_assert!(self.len <= N);
        // SAFETY: every element below `len <= N` was written by `push`,
        // `resize`, `extend_from_slice` or a `set_len` whose caller initialised
        // it, each of which bounds `len` by `N`; `MaybeUninit<T>` has `T`'s
        // layout.
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr().cast::<T>(), self.len) }
    }
}

impl<T: Copy, const N: usize> DerefMut for InlineVec<T, N> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        debug_assert!(self.len <= N);
        // SAFETY: as for `deref`.
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<T>(), self.len) }
    }
}

impl<T: Copy, const N: usize> Clone for InlineVec<T, N> {
    fn clone(&self) -> Self {
        let mut out = Self::new();
        out.extend_from_slice(self);
        out
    }
}

impl<T: Copy, const N: usize> Default for InlineVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + core::fmt::Debug, const N: usize> core::fmt::Debug for InlineVec<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests;
