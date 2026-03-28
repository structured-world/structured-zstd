#[inline(always)]
pub(crate) fn prefetch_slice(slice: &[u8]) {
    prefetch_slice_impl(slice);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
fn prefetch_slice_impl(slice: &[u8]) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::{_MM_HINT_T0, _mm_prefetch};
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};

    if !slice.is_empty() {
        unsafe { _mm_prefetch(slice.as_ptr().cast(), _MM_HINT_T0) };
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
#[inline(always)]
fn prefetch_slice_impl(_slice: &[u8]) {}
