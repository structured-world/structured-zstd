#[inline(always)]
pub(crate) fn prefetch_slice(slice: &[u8]) {
    prefetch_slice_impl_l1(slice);
}

#[inline(always)]
pub(crate) fn prefetch_slice_t1(slice: &[u8]) {
    prefetch_slice_impl_t1(slice);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn prefetch_slice_impl_l1(slice: &[u8]) {
    use core::arch::x86_64::_MM_HINT_T0;
    prefetch_stride_x86_64::<{ _MM_HINT_T0 }>(slice);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn prefetch_slice_impl_t1(slice: &[u8]) {
    use core::arch::x86_64::_MM_HINT_T1;
    prefetch_stride_x86_64::<{ _MM_HINT_T1 }>(slice);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn prefetch_stride_x86_64<const HINT: i32>(slice: &[u8]) {
    use core::arch::x86_64::_mm_prefetch;
    const CACHE_LINE: usize = 64;
    const MAX_LINES: usize = 4;

    if slice.len() < CACHE_LINE {
        return;
    }

    let line_count = slice.len().div_ceil(CACHE_LINE).min(MAX_LINES);
    let base = slice.as_ptr();
    for i in 0..line_count {
        let ptr = unsafe { base.add(i * CACHE_LINE) };
        unsafe { _mm_prefetch(ptr.cast(), HINT) };
    }
}

#[cfg(all(target_arch = "x86", target_feature = "sse"))]
#[inline(always)]
fn prefetch_slice_impl_l1(slice: &[u8]) {
    use core::arch::x86::_MM_HINT_T0;
    prefetch_stride_x86::<{ _MM_HINT_T0 }>(slice);
}

#[cfg(all(target_arch = "x86", target_feature = "sse"))]
#[inline(always)]
fn prefetch_slice_impl_t1(slice: &[u8]) {
    use core::arch::x86::_MM_HINT_T1;
    prefetch_stride_x86::<{ _MM_HINT_T1 }>(slice);
}

#[cfg(all(target_arch = "x86", target_feature = "sse"))]
#[inline(always)]
fn prefetch_stride_x86<const HINT: i32>(slice: &[u8]) {
    use core::arch::x86::_mm_prefetch;
    const CACHE_LINE: usize = 64;
    const MAX_LINES: usize = 4;

    if slice.len() < CACHE_LINE {
        return;
    }

    let line_count = slice.len().div_ceil(CACHE_LINE).min(MAX_LINES);
    let base = slice.as_ptr();
    for i in 0..line_count {
        let ptr = unsafe { base.add(i * CACHE_LINE) };
        unsafe { _mm_prefetch(ptr.cast(), HINT) };
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn prefetch_slice_impl_l1(slice: &[u8]) {
    prefetch_stride_aarch64(slice, PrefetchHintAarch64::L1);
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn prefetch_slice_impl_t1(slice: &[u8]) {
    prefetch_stride_aarch64(slice, PrefetchHintAarch64::L2);
}

#[cfg(target_arch = "aarch64")]
#[derive(Copy, Clone)]
enum PrefetchHintAarch64 {
    L1,
    L2,
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn prefetch_stride_aarch64(slice: &[u8], hint: PrefetchHintAarch64) {
    use core::arch::asm;
    const CACHE_LINE: usize = 64;
    const MAX_LINES: usize = 4;

    if slice.len() < CACHE_LINE {
        return;
    }

    let line_count = slice.len().div_ceil(CACHE_LINE).min(MAX_LINES);
    let base = slice.as_ptr();
    for i in 0..line_count {
        let ptr = unsafe { base.add(i * CACHE_LINE) };
        match hint {
            PrefetchHintAarch64::L1 => unsafe {
                asm!("prfm pldl1keep, [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags))
            },
            PrefetchHintAarch64::L2 => unsafe {
                asm!("prfm pldl2keep, [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags))
            },
        }
    }
}

#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse"),
    target_arch = "aarch64",
)))]
#[inline(always)]
fn prefetch_slice_impl_l1(_slice: &[u8]) {}

#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse"),
    target_arch = "aarch64",
)))]
#[inline(always)]
fn prefetch_slice_impl_t1(_slice: &[u8]) {}
