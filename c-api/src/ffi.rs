//! Raw-pointer ↔ slice helpers shared by the wrappers.
//!
//! All upstream entry points tolerate a `(NULL, 0)` buffer pair, so a zero
//! length never dereferences the pointer.

/// Build a shared slice from a C `(ptr, len)` pair.
///
/// # Safety
/// When `len > 0`, `ptr` must be non-null and valid for reads of `len` bytes
/// for the slice's lifetime.
pub(crate) unsafe fn in_slice<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
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
/// When `len > 0`, `ptr` must be non-null, valid for reads and writes of
/// `len` bytes, and unaliased for the slice's lifetime.
pub(crate) unsafe fn out_slice<'a>(ptr: *mut u8, len: usize) -> &'a mut [u8] {
    if len == 0 {
        &mut []
    } else {
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }
}
