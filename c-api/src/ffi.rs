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

/// The input of a one-shot decode into `(dst, dst_capacity)`: borrowed when
/// the two buffers are apart, and copied out first when the input lies inside
/// the output, the in-place layout `ZSTD_decompressionMargin` describes. A
/// shared slice of the input beside the decode's exclusive slice of the output
/// would alias, which Rust's slices do not allow. Call it before the output
/// slice is made; a copy that cannot be allocated is `memory_allocation`.
///
/// # Safety
/// `src` must be valid for reads of `src_size` bytes, as for [`in_slice`].
pub(crate) unsafe fn decode_input<'a>(
    src: *const u8,
    src_size: usize,
    dst: *mut u8,
    dst_capacity: usize,
) -> Result<std::borrow::Cow<'a, [u8]>, crate::error::ZSTD_ErrorCode> {
    let input = unsafe { in_slice(src, src_size) };
    let (src_at, dst_at) = (src as usize, dst as usize);
    // Both spans lie inside their allocations, so neither end overflows.
    let overlaps = src_size != 0
        && dst_capacity != 0
        && src_at < dst_at + dst_capacity
        && dst_at < src_at + src_size;
    if !overlaps {
        return Ok(std::borrow::Cow::Borrowed(input));
    }
    let mut copy = Vec::new();
    copy.try_reserve_exact(src_size)
        .map_err(|_| crate::error::ZSTD_ErrorCode::ZSTD_error_memory_allocation)?;
    copy.extend_from_slice(input);
    Ok(std::borrow::Cow::Owned(copy))
}
