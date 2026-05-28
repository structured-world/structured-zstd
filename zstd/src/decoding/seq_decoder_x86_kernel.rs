//! Macro generating per-tier x86 sequence-section decoder bodies.
//!
//! Issue #279 round 3 architecture: each x86 CPU tier (BMI2 / AVX2 /
//! VBMI2) gets its OWN decoder function in its own target_feature scope,
//! with the hot `br.get_bits_triple` call site replaced by a direct
//! `peek_bits_triple_bmi2` invocation. The bmi2-tagged BitReader
//! variant inlines `_pext_u64` at the call site, eliminating the
//! `extract_triple_pext` CALL boundary that #279 design memo
//! attributed ~3.95% of decode self-time to.
//!
//! Macro-based to keep ONE source body authoritative while emitting
//! three independently-scoped functions; per-tier files
//! (`seq_decoder_{bmi2,avx2,vbmi2}.rs`) invoke this macro with their
//! kernel type and target_feature string. Future divergence (different
//! match-copy chunk size per tier, VPSHUFB shortcuts) lands either by
//! parameterising the macro further or by overriding specific arms
//! with hand-tuned per-tier code.

/// Define the per-tier sequence-section decoder trio: outer
/// `$decode_fn`, pipelined loop `$loop_fn`, and inner-sequence decoder
/// `$decode_one_fn`. All three carry `#[target_feature(enable = $tf)]`
/// and use `$kernel` as the BitReader's `K` parameter.
///
/// Crate-private — the per-tier files (`seq_decoder_{bmi2,avx2,vbmi2}.rs`)
/// invoke this via the `pub(crate) use` re-export below. NOT
/// `#[macro_export]` because it's an internal decoder-generation
/// shape; downstream crates have no reason to consume it and exposing
/// it would lock in the macro syntax as part of the crate's public
/// API surface.
macro_rules! define_x86_seq_decoder_tier {
    (
        kernel = $kernel:ty,
        target_feature = $tf:literal,
        decode_fn = $decode_fn:ident,
        loop_fn = $loop_fn:ident,
        decode_one_fn = $decode_one_fn:ident,
        exec_one_fn = $exec_one_fn:path,
        exec_one_resolved_fn = $exec_one_resolved_fn:path $(,)?
    ) => {
        /// Per-tier `decode_one_sequence_inline`. Hot site uses
        /// `peek_bits_triple_bmi2` directly (target_feature scope
        /// flowing from caller). Replaces the shared K-generic
        /// `br.get_bits_triple` → `peek_bits_triple` → CALL into
        /// `extract_triple_pext` chain with inline `_pext_u64` ops.
        ///
        /// # Safety
        /// Caller's target_feature must include BMI2.
        #[inline]
        #[target_feature(enable = $tf)]
        #[allow(dead_code)]
        unsafe fn $decode_one_fn<'a, 'b>(
            ll_dec: &mut $crate::fse::SeqFSEDecoder<'a>,
            ml_dec: &mut $crate::fse::SeqFSEDecoder<'a>,
            of_dec: &mut $crate::fse::SeqFSEDecoder<'a>,
            br: &mut $crate::bit_io::BitReaderReversed<'b, $kernel>,
        ) -> $crate::blocks::sequence_section::Sequence {
            let ll_state = ll_dec.state;
            let ml_state = ml_dec.state;
            let of_state = of_dec.state;

            let ll_value = ll_state.base_value;
            let ll_num_bits = ll_state.num_additional_bits;
            let ml_value = ml_state.base_value;
            let ml_num_bits = ml_state.num_additional_bits;
            let of_num_bits = of_state.num_additional_bits;
            let of_base = of_state.base_value;

            debug_assert!(of_num_bits <= $crate::blocks::sequence_section::MAX_OFFSET_CODE);

            // Replace `br.get_bits_triple(...)` with the inline form
            // that bypasses the `extract_triple_pext` CALL boundary,
            // BUT preserves the `use_pext_triple` vendor-policy gate:
            // AMD Zen1/Zen2 execute PEXT/PDEP through microcode and
            // perform WORSE than the scalar 3× shift+mask path
            // (`should_use_pext` policy returns false for those CPUs).
            // Reading the cached flag is one load on the hot path
            // versus the regression those CPUs would otherwise see.
            // The bmi2-tagged `peek_bits_triple_bmi2` only fires when
            // the policy permits; on slow-PEXT CPUs we fall through to
            // `peek_bits_triple` which routes to the K-trait scalar
            // path inside the same target_feature scope.
            let sum_wide = u16::from(of_num_bits) + u16::from(ml_num_bits) + u16::from(ll_num_bits);
            let (obits, ml_add, ll_add) = if sum_wide <= 56 {
                let sum = sum_wide as u8;
                br.ensure_bits(sum);
                // SAFETY: enclosing fn is `target_feature(enable = $tf)`
                // which includes BMI2; runtime CPU presence of BMI2 is
                // gated by the dispatcher at
                // `decode_and_execute_sequences::detect_cpu_kernel`.
                // Vendor policy: only call the pext-direct variant on
                // CPUs whose pext is fast (cached at BitReader::new).
                let triple = if br.use_pext_triple_fast() {
                    unsafe { br.peek_bits_triple_bmi2(sum, of_num_bits, ml_num_bits, ll_num_bits) }
                } else {
                    br.peek_bits_triple(sum, of_num_bits, ml_num_bits, ll_num_bits)
                };
                br.consume(sum);
                triple
            } else {
                (
                    br.get_bits(of_num_bits),
                    br.get_bits(ml_num_bits),
                    br.get_bits(ll_num_bits),
                )
            };
            let offset = obits as u32 + of_base;
            debug_assert_ne!(offset, 0);

            $crate::blocks::sequence_section::Sequence {
                ll: ll_value + ll_add as u32,
                ml: ml_value + ml_add as u32,
                of: offset,
            }
        }

        /// Per-tier pipelined sequence-decode + execute loop. Body
        /// mirrors the shared K-generic `run_pipelined_sequence_loop`
        /// with `K = $kernel` and `decode_one_sequence_inline`
        /// replaced by the per-tier `$decode_one_fn`.
        ///
        /// # Safety
        /// Caller's target_feature must include BMI2 (and any
        /// additional features advertised by `$tf`).
        #[allow(clippy::too_many_arguments)]
        #[inline]
        #[target_feature(enable = $tf)]
        unsafe fn $loop_fn<'a, 'b, B: $crate::decoding::buffer_backend::BufferBackend>(
            br: &mut $crate::bit_io::BitReaderReversed<'b, $kernel>,
            ll_dec: &mut $crate::fse::SeqFSEDecoder<'a>,
            ml_dec: &mut $crate::fse::SeqFSEDecoder<'a>,
            of_dec: &mut $crate::fse::SeqFSEDecoder<'a>,
            buffer: &mut $crate::decoding::decode_buffer::DecodeBuffer<B>,
            offset_hist: &mut [u32; 3],
            literals_buffer: &[u8],
            lit_cur: &mut usize,
            literals_buffer_len: usize,
            num_sequences: usize,
            old_buffer_size: usize,
            max_update_bits: u8,
            seq_sum: &mut u32,
        ) -> Result<(), $crate::decoding::errors::DecompressBlockError> {
            use $crate::decoding::sequence_execution::do_offset_history;

            // Donor-shape straight loop: decode one sequence, execute
            // it immediately, advance state, next. No lookahead ring,
            // no shadow_hist, no prefetch_pos arithmetic. Mirrors
            // `ZSTD_decompressSequences_body_bmi2_noExt_rawLit`
            // (zstd-pure-rs zstd_decompress_block.rs:2291-2344).
            let _ = old_buffer_size;
            for i in 0..num_sequences {
                let seq = unsafe { $decode_one_fn(ll_dec, ml_dec, of_dec, br) };
                let resolved_offset = do_offset_history(seq.of, seq.ll, offset_hist);

                // SAFETY: per-tier exec_one carries the same
                // target_feature as the enclosing $loop_fn scope.
                unsafe {
                    $exec_one_fn(
                        buffer,
                        literals_buffer,
                        lit_cur,
                        literals_buffer_len,
                        seq,
                        resolved_offset,
                    )?;
                }
                *seq_sum = seq_sum.wrapping_add(seq.ll).wrapping_add(seq.ml);

                if i + 1 < num_sequences {
                    br.ensure_bits(max_update_bits);
                    ll_dec.update_state_fast(br);
                    ml_dec.update_state_fast(br);
                    of_dec.update_state_fast(br);
                }
            }

            *offset_hist = shadow_hist;
            Ok(())
        }

        /// Per-tier outer `decode_and_execute_sequences`. Full impl
        /// body with `K = $kernel`, target_feature scope, calling
        /// per-tier `$loop_fn` and `$decode_one_fn`. Shared helpers
        /// (`maybe_update_fse_tables`, `decode_sequences_with_rle`,
        /// `execute_sequences_fields`, `execute_one_sequence_pipelined`)
        /// stay K-agnostic and cross the target_feature boundary as
        /// regular CALLs — they're cold path or branch-target stable
        /// enough that the boundary cost doesn't dominate.
        ///
        /// # Safety
        /// Caller must ensure the target_feature set described by
        /// `$tf` is available on the runtime CPU; dispatcher in
        /// `decode_and_execute_sequences` gates on `detect_cpu_kernel`.
        #[target_feature(enable = $tf)]
        pub(crate) unsafe fn $decode_fn<B: $crate::decoding::buffer_backend::BufferBackend>(
            section: &$crate::blocks::sequence_section::SequencesHeader,
            source: &[u8],
            fse: &mut $crate::decoding::scratch::FSEScratch,
            buffer: &mut $crate::decoding::decode_buffer::DecodeBuffer<B>,
            offset_hist: &mut [u32; 3],
            literals_buffer: &[u8],
            rle_fallback_sequences: &mut alloc::vec::Vec<
                $crate::blocks::sequence_section::Sequence,
            >,
        ) -> Result<(), $crate::decoding::errors::DecompressBlockError> {
            use $crate::bit_io::BitReaderReversed;
            use $crate::common::MAX_BLOCK_SIZE;
            use $crate::decoding::errors::{
                DecodeSequenceError, DecompressBlockError, ExecuteSequencesError,
            };
            use $crate::decoding::sequence_execution::execute_sequences_fields;
            use $crate::decoding::sequence_section_decoder::{
                ADVANCE, decode_sequences_with_rle, maybe_update_fse_tables,
            };
            use $crate::fse::SeqFSEDecoder;

            rle_fallback_sequences.clear();

            let ddict_is_cold = fse.ddict_is_cold;
            fse.ddict_is_cold = false;

            let bytes_read = maybe_update_fse_tables(section, source, fse)?;

            let bit_stream = &source[bytes_read..];
            let mut br = BitReaderReversed::<$kernel>::new(bit_stream);

            // Skip 0-padding + start-of-stream 1 bit.
            let mut skipped_bits = 0;
            loop {
                let val = br.get_bits(1);
                skipped_bits += 1;
                if val == 1 || skipped_bits > 8 {
                    break;
                }
            }
            if skipped_bits > 8 {
                return Err(DecodeSequenceError::ExtraPadding { skipped_bits }.into());
            }

            if fse.ll_rle.is_some() || fse.ml_rle.is_some() || fse.of_rle.is_some() {
                decode_sequences_with_rle(section, &mut br, fse, rle_fallback_sequences)?;
                execute_sequences_fields(
                    buffer,
                    literals_buffer,
                    offset_hist,
                    rle_fallback_sequences,
                )?;
                return Ok(());
            }

            let mut ll_dec = SeqFSEDecoder::new(&fse.literal_lengths);
            let mut ml_dec = SeqFSEDecoder::new(&fse.match_lengths);
            let mut of_dec = SeqFSEDecoder::new(&fse.offsets);

            ll_dec
                .init_state(&mut br)
                .map_err(DecodeSequenceError::from)?;
            of_dec
                .init_state(&mut br)
                .map_err(DecodeSequenceError::from)?;
            ml_dec
                .init_state(&mut br)
                .map_err(DecodeSequenceError::from)?;

            let max_update_bits = fse.literal_lengths.accuracy_log
                + fse.match_lengths.accuracy_log
                + fse.offsets.accuracy_log;
            debug_assert!(
                max_update_bits <= 56,
                "sequence section update bits exceed 56-bit budget"
            );

            buffer.reserve(MAX_BLOCK_SIZE as usize);
            let old_buffer_size = buffer.len();
            let literals_buffer_len = literals_buffer.len();
            let mut lit_cur: usize = 0;
            let mut seq_sum: u32 = 0;

            let buffer_checkpoint = buffer.checkpoint();
            let saved_offset_hist = *offset_hist;
            let num_sequences = section.num_sequences as usize;

            #[cfg(target_pointer_width = "64")]
            const MIN_LONG_OFFSET_SHARE: u32 = 7;
            #[cfg(not(target_pointer_width = "64"))]
            const MIN_LONG_OFFSET_SHARE: u32 = 20;
            let use_long_pipeline = num_sequences >= ADVANCE * 2
                && (ddict_is_cold || fse.offsets_long_share >= MIN_LONG_OFFSET_SHARE);

            if use_long_pipeline {
                // SAFETY: $loop_fn carries the same target_feature as
                // this caller, invoked from inside that scope.
                let pipeline_result = unsafe {
                    $loop_fn(
                        &mut br,
                        &mut ll_dec,
                        &mut ml_dec,
                        &mut of_dec,
                        buffer,
                        offset_hist,
                        literals_buffer,
                        &mut lit_cur,
                        literals_buffer_len,
                        num_sequences,
                        old_buffer_size,
                        max_update_bits,
                        &mut seq_sum,
                    )
                };
                if let Err(e) = pipeline_result {
                    if buffer.try_restore_checkpoint(buffer_checkpoint) {
                        *offset_hist = saved_offset_hist;
                    }
                    return Err(e);
                }
            } else {
                let mut shadow_hist = *offset_hist;
                let mut fallback_err: Option<DecompressBlockError> = None;
                for i in 0..num_sequences {
                    let seq =
                        unsafe { $decode_one_fn(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br) };
                    let resolved_offset = $crate::decoding::sequence_execution::do_offset_history(
                        seq.of,
                        seq.ll,
                        &mut shadow_hist,
                    );
                    // SAFETY: $exec_one_fn carries the same target_feature
                    // as this enclosing $decode_fn (or is safe scalar).
                    let exec_result = unsafe {
                        $exec_one_fn(
                            buffer,
                            literals_buffer,
                            &mut lit_cur,
                            literals_buffer_len,
                            seq,
                            resolved_offset,
                        )
                    };
                    if let Err(e) = exec_result {
                        fallback_err = Some(e);
                        break;
                    }
                    seq_sum = seq_sum.wrapping_add(seq.ll).wrapping_add(seq.ml);

                    if i + 1 < num_sequences {
                        br.ensure_bits(max_update_bits);
                        ll_dec.update_state_fast(&mut br);
                        ml_dec.update_state_fast(&mut br);
                        of_dec.update_state_fast(&mut br);
                    }
                }
                if let Some(e) = fallback_err {
                    let _ = buffer.try_restore_checkpoint(buffer_checkpoint);
                    return Err(e);
                }
                *offset_hist = shadow_hist;
            }

            let remaining = br.bits_remaining();
            if remaining != 0 {
                if buffer.try_restore_checkpoint(buffer_checkpoint) {
                    *offset_hist = saved_offset_hist;
                }
                if remaining < 0 {
                    return Err(DecodeSequenceError::NotEnoughBytesForNumSequences.into());
                }
                return Err(DecodeSequenceError::ExtraBits {
                    bits_remaining: remaining,
                }
                .into());
            }

            if lit_cur < literals_buffer_len {
                let rest = &literals_buffer[lit_cur..];
                buffer.try_push(rest).map_err(ExecuteSequencesError::from)?;
                seq_sum = seq_sum.wrapping_add(rest.len() as u32);
            }

            let diff = buffer.len() - old_buffer_size;
            debug_assert_eq!(
                seq_sum as usize, diff,
                "seq_sum {seq_sum} != buffer growth {diff}"
            );
            Ok(())
        }
    };
}

// Crate-local re-export so per-tier modules
// (`seq_decoder_{bmi2,avx2,vbmi2}.rs`) can invoke the macro via
// `crate::decoding::seq_decoder_x86_kernel::define_x86_seq_decoder_tier!`
// without `#[macro_export]` widening the API surface to downstream
// crates.
#[cfg(target_arch = "x86_64")]
pub(crate) use define_x86_seq_decoder_tier;
