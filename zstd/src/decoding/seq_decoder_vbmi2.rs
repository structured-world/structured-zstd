//! AVX-512 VBMI2 + AVX2 + BMI2 tier sequence-section decoder entry point.
//!
//! Foundation for issue #279 round 3 per-kernel decoder refactor:
//! the VBMI2 trampoline lives in its own module so future per-kernel
//! divergence (VPSHUFB tile lookup in HUF burst, AVX-512 broadcast for
//! `extend_and_fill`) can be applied IN PLACE here. Today the body
//! still delegates into the shared K-generic
//! `decode_and_execute_sequences_impl` with `K = Vbmi2Kernel`.

use alloc::vec::Vec;

use crate::blocks::sequence_section::{Sequence, SequencesHeader};
use crate::decoding::buffer_backend::BufferBackend;
use crate::decoding::decode_buffer::DecodeBuffer;
use crate::decoding::errors::DecompressBlockError;
use crate::decoding::scratch::FSEScratch;
use crate::decoding::sequence_section_decoder::decode_and_execute_sequences_impl;

/// `#[target_feature(enable = "...AVX-512 VBMI2 family + BMI2 + AVX2")]`
/// trampoline for the Vbmi2 arm. Enables the full feature set the
/// `select_x86_kernel` precedence requires so VBMI2-only intrinsics
/// (VPSHUFB-driven HUF burst, AVX-512 mask ops) can be folded inline.
///
/// # Safety
/// Caller must ensure the full feature set is available; gated by
/// `detect_cpu_kernel() == CpuKernelTag::Vbmi2`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw")]
pub(crate) unsafe fn decode_and_execute_sequences_vbmi2<B: BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, crate::cpu_kernel::Vbmi2Kernel>(
        section,
        source,
        fse,
        buffer,
        offset_hist,
        literals_buffer,
        rle_fallback_sequences,
    )
}
