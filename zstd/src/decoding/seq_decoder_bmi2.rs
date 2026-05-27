//! BMI2-tier sequence-section decoder entry point.
//!
//! Foundation for issue #279 round 3 per-kernel decoder refactor:
//! the BMI2 trampoline lives in its own module so future per-kernel
//! divergence (inline `bzhi` instead of K::mask_lower_bits CALL, inline
//! `pext` instead of `extract_triple_pext` CALL, BMI2-specific FSE
//! state-update shape) can be applied IN PLACE here without touching
//! the AVX2 / VBMI2 / Scalar variants. Today the body still delegates
//! into the shared K-generic `decode_and_execute_sequences_impl`; the
//! file exists so the divergence has a home to land in.

use alloc::vec::Vec;

use crate::blocks::sequence_section::{Sequence, SequencesHeader};
use crate::decoding::buffer_backend::BufferBackend;
use crate::decoding::decode_buffer::DecodeBuffer;
use crate::decoding::errors::DecompressBlockError;
use crate::decoding::scratch::FSEScratch;
use crate::decoding::sequence_section_decoder::decode_and_execute_sequences_impl;

/// `#[target_feature(enable = "bmi2")]` trampoline for the BMI2 arm.
/// Wraps the K-generic impl with K = `Bmi2Kernel` so the inner
/// `K::mask_lower_bits` / `peek_bits_triple` paths land inside a
/// BMI2-scoped caller, letting LLVM lift `_bzhi_u64` / `_pext_u64`
/// inline at every leaf call site.
///
/// # Safety
/// Caller must ensure BMI2 is available on the runtime CPU; the
/// dispatcher in `sequence_section_decoder::decode_and_execute_sequences`
/// gates this through `detect_cpu_kernel() == CpuKernelTag::Bmi2`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
pub(crate) unsafe fn decode_and_execute_sequences_bmi2<B: BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, crate::cpu_kernel::Bmi2Kernel>(
        section,
        source,
        fse,
        buffer,
        offset_hist,
        literals_buffer,
        rle_fallback_sequences,
    )
}
