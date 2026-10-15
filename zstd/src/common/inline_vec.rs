//! Fixed-capacity, allocation-free stand-in for the `Vec` scratch the entropy
//! table builders keep, selected by the `caller-workspace` feature so the
//! tables can live in caller-owned memory.

use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

/// A `Vec`-shaped buffer of at most `N` elements stored inline.
///
/// Writes past `N` are discarded while the length keeps counting, so a
/// builder that appends until its input runs out still reports how much it
/// tried to store, and its own length check (`TooManySymbols`,
/// `DecodedLiteralCountMismatch`, ...) rejects the input exactly as it would
/// with a growable `Vec`. Only the stored prefix is ever readable.
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
    pub(crate) const fn with_capacity(_capacity: usize) -> Self {
        Self::new()
    }

    /// Initialise an empty buffer in place without building it on the stack.
    ///
    /// # Safety
    /// `this` must be valid for writes and aligned for `Self`.
    pub(crate) unsafe fn init_in_place(this: *mut Self) {
        // SAFETY: the caller guarantees `this` is writable and aligned; the
        // element array is `MaybeUninit` and needs no initialisation.
        unsafe { core::ptr::addr_of_mut!((*this).len).write(0) };
    }

    /// Number of elements appended, which can exceed `N` after a discarded
    /// write; see the type documentation.
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

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
        debug_assert!(self.len.saturating_add(additional) <= N);
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, value: T) {
        if self.len < N {
            self.buf[self.len] = MaybeUninit::new(value);
        }
        self.len += 1;
    }

    pub(crate) fn resize(&mut self, new_len: usize, value: T) {
        let stored = self.len.min(N);
        let fill_end = new_len.min(N);
        if fill_end > stored {
            for slot in &mut self.buf[stored..fill_end] {
                *slot = MaybeUninit::new(value);
            }
        }
        self.len = new_len;
    }

    pub(crate) fn extend_from_slice(&mut self, values: &[T]) {
        let stored = self.len.min(N);
        let room = N - stored;
        let copied = values.len().min(room);
        for (slot, &value) in self.buf[stored..stored + copied].iter_mut().zip(values) {
            *slot = MaybeUninit::new(value);
        }
        self.len += values.len();
    }

    /// The writable tail past the stored prefix.
    #[inline(always)]
    pub(crate) fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<T>] {
        let stored = self.len.min(N);
        &mut self.buf[stored..]
    }

    /// # Safety
    /// `new_len <= N`, and every element below `new_len` is initialised.
    #[inline(always)]
    pub(crate) unsafe fn set_len(&mut self, new_len: usize) {
        assert!(new_len <= N, "InlineVec::set_len past its capacity");
        self.len = new_len;
    }

    #[inline(always)]
    pub(crate) fn as_slice(&self) -> &[T] {
        self
    }
}

impl<T: Copy, const N: usize> Deref for InlineVec<T, N> {
    type Target = [T];

    #[inline(always)]
    fn deref(&self) -> &[T] {
        // SAFETY: every element below `min(len, N)` was written by `push`,
        // `resize`, `extend_from_slice` or a `set_len` whose caller initialised
        // it; `MaybeUninit<T>` has `T`'s layout.
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr().cast::<T>(), self.len.min(N)) }
    }
}

impl<T: Copy, const N: usize> DerefMut for InlineVec<T, N> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        let len = self.len.min(N);
        // SAFETY: as for `deref`.
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<T>(), len) }
    }
}

impl<T: Copy, const N: usize> Clone for InlineVec<T, N> {
    fn clone(&self) -> Self {
        let mut out = Self::new();
        out.extend_from_slice(self);
        out.len = self.len;
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
