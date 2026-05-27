//! AVX-512 VBMI2 + AVX2 + BMI2 tier sequence-section decoder.
//!
//! Issue #279 round 3 Phase 4: VBMI2-tier execute wrappers delegate
//! to the AVX2 variant (32-byte ymm match-copy wildcopy). Future
//! commits may add zmm 64-byte wildcopy once buffer-slack contracts
//! are extended to WILDCOPY_OVERLENGTH = 64.

crate::define_x86_seq_decoder_tier! {
    kernel = crate::cpu_kernel::Vbmi2Kernel,
    target_feature = "bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw",
    decode_fn = decode_and_execute_sequences_vbmi2,
    loop_fn = run_pipelined_loop_vbmi2,
    decode_one_fn = decode_one_sequence_vbmi2,
    exec_one_fn = crate::decoding::sequence_section_decoder::execute_one_sequence_pipelined_vbmi2,
    exec_one_resolved_fn = crate::decoding::sequence_section_decoder::execute_one_sequence_pipelined_resolved_vbmi2,
}
