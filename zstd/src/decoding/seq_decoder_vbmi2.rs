//! AVX-512 VBMI2 + AVX2 + BMI2 tier sequence-section decoder.
//!
//! Issue #279 round 3 Phase 3: full decoder body in the AVX-512
//! VBMI2-family `#[target_feature]` scope. The hot site uses
//! `peek_bits_triple_bmi2` so `_pext_u64` inlines as inline pext
//! triples. AVX-512 VBMI2 enabling here lets future VPSHUFB-driven
//! HUF burst / `_mm512_*` chunked copy paths compile to AVX-512
//! width when invoked from this tier.

crate::define_x86_seq_decoder_tier! {
    kernel = crate::cpu_kernel::Vbmi2Kernel,
    target_feature = "bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw",
    decode_fn = decode_and_execute_sequences_vbmi2,
    loop_fn = run_pipelined_loop_vbmi2,
    decode_one_fn = decode_one_sequence_vbmi2,
}
