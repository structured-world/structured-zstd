//! AVX2-tier sequence-section decoder.
//!
//! Issue #279 round 3 Phase 3: full decoder body in BMI2+AVX2
//! `#[target_feature]` scope, hot site via `peek_bits_triple_bmi2`.
//! AVX2 lands in the feature list so chunked SIMD copy helpers
//! (`_mm256_storeu_si256` paths inside the backend's `repeat` /
//! `extend_and_fill`) compile to ymm-width stores when called from
//! this tier's body.

crate::define_x86_seq_decoder_tier! {
    kernel = crate::cpu_kernel::Avx2Kernel,
    target_feature = "bmi2,avx2",
    decode_fn = decode_and_execute_sequences_avx2,
    loop_fn = run_pipelined_loop_avx2,
    decode_one_fn = decode_one_sequence_avx2,
}
