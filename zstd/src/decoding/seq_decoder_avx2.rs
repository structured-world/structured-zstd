//! AVX2-tier sequence-section decoder.
//!
//! Issue #279 round 3 Phase 4: full per-tier divergence at the
//! match-copy chain. `execute_one_sequence_pipelined_avx2` (and its
//! ExecSeq-unpack wrapper) route the no-overlap match wildcopy
//! through `wildcopy_no_overlap_avx2` (32-byte ymm stride) instead of
//! the SSE2 16-byte default. AVX2-tier divergence on i9-class CPUs:
//! 2× write throughput on the match-copy hot path.

crate::define_x86_seq_decoder_tier! {
    kernel = crate::cpu_kernel::Avx2Kernel,
    target_feature = "bmi2,avx2",
    decode_fn = decode_and_execute_sequences_avx2,
    loop_fn = run_pipelined_loop_avx2,
    decode_one_fn = decode_one_sequence_avx2,
    exec_one_fn = crate::decoding::sequence_section_decoder::execute_one_sequence_pipelined_avx2,
    exec_one_resolved_fn = crate::decoding::sequence_section_decoder::execute_one_sequence_pipelined_resolved_avx2,
}
