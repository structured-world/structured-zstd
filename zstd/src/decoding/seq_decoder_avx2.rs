//! AVX2-tier sequence-section decoder entry point.
//!
//! Foundation for issue #279 round 3 per-kernel decoder refactor:
//! the AVX2 trampoline lives in its own module so future per-kernel
//! divergence (256-bit `_mm256_storeu_si256` match-copy chunks instead
//! of the 128-bit SSE2 default, BMI2 leaf intrinsics in scope) can be
//! applied IN PLACE here. Today the body still delegates into the
//! shared K-generic `decode_and_execute_sequences_impl` with
//! `K = Avx2Kernel`.

use alloc::vec::Vec;

use crate::blocks::sequence_section::{Sequence, SequencesHeader};
use crate::decoding::buffer_backend::BufferBackend;
use crate::decoding::decode_buffer::DecodeBuffer;
use crate::decoding::errors::DecompressBlockError;
use crate::decoding::scratch::FSEScratch;
use crate::decoding::sequence_section_decoder::decode_and_execute_sequences_impl;

/// `#[target_feature(enable = "bmi2,avx2")]` trampoline for the Avx2
/// arm. The AVX2 feature stacks onto BMI2 so any AVX2-gated codegen
/// (`_mm256_*` chunked SIMD copy, ymm-register chains) lands inside
/// the same target_feature scope as the BMI2 leaf intrinsics.
///
/// # Safety
/// Caller must ensure BMI2 + AVX2 are available; gated by
/// `detect_cpu_kernel() == CpuKernelTag::Avx2`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2")]
pub(crate) unsafe fn decode_and_execute_sequences_avx2<B: BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, crate::cpu_kernel::Avx2Kernel>(
        section,
        source,
        fse,
        buffer,
        offset_hist,
        literals_buffer,
        rle_fallback_sequences,
    )
}
