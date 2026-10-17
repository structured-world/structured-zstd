//! Raw-pointer ↔ slice helpers shared by the wrappers.
//!
//! All upstream entry points tolerate a `(NULL, 0)` buffer pair, so a zero
//! length never dereferences the pointer.

/// Build a shared slice from a C `(ptr, len)` pair of `len` elements.
///
/// # Safety
/// When `len > 0`, `ptr` must be non-null, aligned and valid for reads of
/// `len` elements for the slice's lifetime.
pub(crate) unsafe fn in_slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

/// Build a mutable slice from a C `(ptr, len)` pair, with the same `len == 0`
/// NULL tolerance as [`in_slice`].
///
/// # Safety
/// When `len > 0`, `ptr` must be non-null, aligned, valid for reads and
/// writes of `len` elements, and unaliased for the slice's lifetime.
pub(crate) unsafe fn out_slice<'a, T>(ptr: *mut T, len: usize) -> &'a mut [T] {
    if len == 0 {
        &mut []
    } else {
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }
}
