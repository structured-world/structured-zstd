//! BMI2-tier sequence-section decoder.
//!
//! Issue #279 round 3 Phase 4: BMI2-tier execute wrappers currently
//! delegate to the safe K-agnostic `execute_one_sequence_pipelined`.
//! Future commits land BMI2-specific match-copy divergence (SSE2 path
//! direct, no AVX2 chunks). The Phase 3 hot-site divergence (direct
//! `peek_bits_triple_bmi2`) is already active via the macro-generated
//! `decode_one_sequence_bmi2`.

crate::decoding::seq_decoder_x86_kernel::define_x86_seq_decoder_tier! {
    kernel = crate::cpu_kernel::Bmi2Kernel,
    target_feature = "bmi2",
    decode_fn = decode_and_execute_sequences_bmi2,
    loop_fn = run_pipelined_loop_bmi2,
    decode_one_fn = decode_one_sequence_bmi2,
    exec_one_fn = crate::decoding::sequence_section_decoder::execute_one_sequence_pipelined_bmi2,
    exec_one_resolved_fn = crate::decoding::sequence_section_decoder::execute_one_sequence_pipelined_resolved_bmi2,
}
