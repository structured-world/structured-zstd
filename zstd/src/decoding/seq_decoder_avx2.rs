//! AVX2-tier monolithic sequence-section decoder.
//!
//! One self-contained `#[target_feature(enable = "bmi2,avx2")]` function
//! that inlines the entire decode + execute pipeline (outer init, RLE
//! dispatch, FSE state init, both pipeline arms, sequence decode, and
//! sequence execute) with no inner CALL boundaries on the hot path.
//! BitReader is pinned to `Avx2Kernel`; the triple-bit extract goes
//! directly through `peek_bits_triple_bmi2` (`_pext_u64` inline). The
//! match copy routes to `BufferBackend::exec_sequence_inline_avx2`
//! (32-byte ymm wildcopy).

#![cfg(target_arch = "x86_64")]

use super::buffer_backend::BufferBackend;
use super::decode_buffer::DecodeBuffer;
use super::scratch::FSEScratch;
use super::sequence_section_decoder::{
    ADVANCE, ADVANCE_MASK, ExecSeq, compute_use_long_pipeline, decode_sequences_with_rle,
    maybe_update_fse_tables,
};
use crate::bit_io::BitReaderReversed;
use crate::blocks::sequence_section::{MAX_OFFSET_CODE, Sequence, SequencesHeader};
use crate::common::MAX_BLOCK_SIZE;
use crate::cpu_kernel::Avx2Kernel;
use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError, ExecuteSequencesError};
use crate::decoding::sequence_execution::{do_offset_history, execute_sequences_fields};
use crate::fse::SeqFSEDecoder;
use alloc::vec::Vec;

/// AVX2-tier monolithic decode + execute. The whole pipeline body is
/// inlined here so LLVM sees one function from outer init through every
/// sequence-decode + sequence-execute iteration to the post-loop
/// validation — no per-tier wrapper, no K-generic dispatch, no helper
/// CALL chain on the hot path.
///
/// # Safety
/// Caller must have verified that the runtime CPU advertises BMI2 + AVX2.
/// The dispatcher in `decode_and_execute_sequences` gates this on
/// `detect_cpu_kernel() == Avx2`.
#[target_feature(enable = "bmi2,avx2")]
#[allow(clippy::too_many_lines)]
pub(crate) unsafe fn decode_and_execute_sequences_avx2<B: BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    rle_fallback_sequences.clear();

    // Consume-once read of the cold-dict flag (mirrors the K-generic
    // dispatcher); RLE early-return must not leak it to a later block.
    let ddict_is_cold = fse.ddict_is_cold;
    fse.ddict_is_cold = false;

    let bytes_read = maybe_update_fse_tables(section, source, fse)?;
    let bit_stream = &source[bytes_read..];
    let mut br = BitReaderReversed::<Avx2Kernel>::new(bit_stream);

    // Skip the 0-padding + the start-of-stream `1` bit.
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

    // RLE-mode blocks: legacy two-pass pipeline. Rare on perf corpora.
    if fse.ll_rle.is_some() || fse.ml_rle.is_some() || fse.of_rle.is_some() {
        decode_sequences_with_rle(section, &mut br, fse, rle_fallback_sequences)?;
        execute_sequences_fields(buffer, literals_buffer, offset_hist, rle_fallback_sequences)?;
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

    let total_history = buffer.window_size + buffer.dict_content.len();
    let use_long_pipeline = compute_use_long_pipeline(
        num_sequences,
        ddict_is_cold,
        total_history,
        fse.offsets_long_share,
    );

    if use_long_pipeline {
        // === Long-pipeline arm (8-deep lookahead ring) ===
        let mut prefetch_pos: usize = old_buffer_size;
        let mut shadow_hist: [u32; 3] = *offset_hist;
        let mut ring: [ExecSeq; ADVANCE] = [ExecSeq {
            ll: 0,
            ml: 0,
            actual_offset: 0,
        }; ADVANCE];

        for slot in ring.iter_mut() {
            let seq = decode_one_avx2(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
            let actual_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
            let match_start = prefetch_pos.wrapping_add(seq.ll as usize);
            let source_idx = match_start.wrapping_sub(actual_offset as usize);
            buffer.prefetch_lookahead_match_source(source_idx);
            prefetch_pos = match_start.wrapping_add(seq.ml as usize);
            *slot = ExecSeq {
                ll: seq.ll,
                ml: seq.ml,
                actual_offset,
            };
            br.ensure_bits(max_update_bits);
            ll_dec.update_state_fast(&mut br);
            ml_dec.update_state_fast(&mut br);
            of_dec.update_state_fast(&mut br);
        }

        // Steady-state entry alignment — keeps body inside one DSB window.
        // SAFETY: alignment-only asm.
        unsafe {
            core::arch::asm!(
                ".p2align 6",
                "nop",
                ".p2align 5",
                "nop",
                ".p2align 3",
                options(nomem, nostack, preserves_flags)
            );
        }

        let mut pipeline_err: Option<DecompressBlockError> = None;
        for i in ADVANCE..num_sequences {
            let seq = decode_one_avx2(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
            let actual_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
            let match_start = prefetch_pos.wrapping_add(seq.ll as usize);
            let source_idx = match_start.wrapping_sub(actual_offset as usize);
            buffer.prefetch_lookahead_match_source(source_idx);
            prefetch_pos = match_start.wrapping_add(seq.ml as usize);

            let slot = i & ADVANCE_MASK;
            let exec_seq = ring[slot];
            ring[slot] = ExecSeq {
                ll: seq.ll,
                ml: seq.ml,
                actual_offset,
            };

            if let Err(e) = execute_one_avx2(
                buffer,
                literals_buffer,
                &mut lit_cur,
                literals_buffer_len,
                exec_seq.ll,
                exec_seq.ml,
                exec_seq.actual_offset,
            ) {
                pipeline_err = Some(e);
                break;
            }
            seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);

            if i + 1 < num_sequences {
                br.ensure_bits(max_update_bits);
                ll_dec.update_state_fast(&mut br);
                ml_dec.update_state_fast(&mut br);
                of_dec.update_state_fast(&mut br);
            }
        }

        if pipeline_err.is_none() {
            for k in 0..ADVANCE {
                let slot = (num_sequences + k) & ADVANCE_MASK;
                let exec_seq = ring[slot];
                if let Err(e) = execute_one_avx2(
                    buffer,
                    literals_buffer,
                    &mut lit_cur,
                    literals_buffer_len,
                    exec_seq.ll,
                    exec_seq.ml,
                    exec_seq.actual_offset,
                ) {
                    pipeline_err = Some(e);
                    break;
                }
                seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);
            }
        }

        if let Some(e) = pipeline_err {
            if buffer.try_restore_checkpoint(buffer_checkpoint) {
                *offset_hist = saved_offset_hist;
            }
            return Err(e);
        }
        *offset_hist = shadow_hist;
    } else {
        // === Short-block arm (straight single-pass fused loop) ===
        let mut shadow_hist = *offset_hist;
        let mut fallback_err: Option<DecompressBlockError> = None;
        for i in 0..num_sequences {
            let seq = decode_one_avx2(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
            let resolved_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
            if let Err(e) = execute_one_avx2(
                buffer,
                literals_buffer,
                &mut lit_cur,
                literals_buffer_len,
                seq.ll,
                seq.ml,
                resolved_offset,
            ) {
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

    // Post-loop bitstream-tail validation. Same transactional rollback.
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

/// Inline per-sequence decode. Triple-bit extract via `peek_bits_triple_bmi2`
/// (`_pext_u64` inline) when vendor policy enables it; scalar fallback
/// inside same target_feature scope otherwise.
///
/// # Safety
/// Caller must be in `#[target_feature(enable = "bmi2,avx2")]` scope.
#[inline]
#[target_feature(enable = "bmi2,avx2")]
unsafe fn decode_one_avx2(
    ll_dec: &mut SeqFSEDecoder<'_>,
    ml_dec: &mut SeqFSEDecoder<'_>,
    of_dec: &mut SeqFSEDecoder<'_>,
    br: &mut BitReaderReversed<'_, Avx2Kernel>,
) -> Sequence {
    let ll_state = ll_dec.state;
    let ml_state = ml_dec.state;
    let of_state = of_dec.state;

    let ll_value = ll_state.base_value;
    let ll_num_bits = ll_state.num_additional_bits;
    let ml_value = ml_state.base_value;
    let ml_num_bits = ml_state.num_additional_bits;
    let of_num_bits = of_state.num_additional_bits;
    let of_base = of_state.base_value;

    debug_assert!(of_num_bits <= MAX_OFFSET_CODE);

    let sum_wide = u16::from(of_num_bits) + u16::from(ml_num_bits) + u16::from(ll_num_bits);
    let (obits, ml_add, ll_add) = if sum_wide <= 56 {
        let sum = sum_wide as u8;
        br.ensure_bits(sum);
        // SAFETY: enclosing fn is target_feature(bmi2,avx2); vendor
        // policy cached at BitReader::new gates the PEXT-direct path.
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

    Sequence {
        ll: ll_value + ll_add as u32,
        ml: ml_value + ml_add as u32,
        of: offset,
    }
}

/// Inline per-sequence execute. Fast path: `exec_sequence_inline_avx2`
/// (32-byte ymm wildcopy). Cold: legacy try_push + repeat_lookahead.
///
/// # Safety
/// Caller must be in `#[target_feature(enable = "bmi2,avx2")]` scope.
#[inline]
#[target_feature(enable = "bmi2,avx2")]
unsafe fn execute_one_avx2<B: BufferBackend>(
    buffer: &mut DecodeBuffer<B>,
    literals_buffer: &[u8],
    lit_cur: &mut usize,
    literals_buffer_len: usize,
    seq_ll: u32,
    seq_ml: u32,
    resolved_offset: u32,
) -> Result<(), DecompressBlockError> {
    let lit_cur_before = *lit_cur;
    let high = lit_cur_before
        .checked_add(seq_ll as usize)
        .filter(|&h| h <= literals_buffer_len)
        .ok_or(ExecuteSequencesError::NotEnoughBytesForSequence {
            wanted: lit_cur_before.saturating_add(seq_ll as usize),
            have: literals_buffer_len,
        })?;
    // SAFETY: high <= literals_buffer_len, lit_cur_before <= high.
    let lits = unsafe { literals_buffer.get_unchecked(lit_cur_before..high) };
    *lit_cur = high;

    if resolved_offset == 0 {
        return Err(ExecuteSequencesError::ZeroOffset.into());
    }

    // Donor `ZSTD_execSequence` inline-eligibility gates: backend opted
    // in (compile-time), 16-byte literal slack, and tail wildcopy bound.
    let inline_path_safe = B::SUPPORTS_INLINE_SEQUENCE_EXEC
        && lit_cur_before
            .checked_add(16)
            .is_some_and(|b| b <= literals_buffer_len)
        && (seq_ll as usize <= 16
            || lit_cur_before
                .checked_add((seq_ll as usize).next_multiple_of(16))
                .is_some_and(|b| b <= literals_buffer_len));

    if inline_path_safe {
        let buf_len = buffer.len();
        let offset = resolved_offset as usize;
        let prefix_end_ok = buf_len
            .checked_add(lits.len())
            .is_some_and(|end| offset <= end);
        if prefix_end_ok {
            // SAFETY: parent-slice provenance for the unconditional
            // donor `copy16` over-read bounded by the slack gate above;
            // offset prefix-resident per `prefix_end_ok`.
            let lit_src = unsafe { literals_buffer.as_ptr().add(lit_cur_before) };
            unsafe {
                buffer
                    .buffer_mut()
                    .exec_sequence_inline_avx2(lit_src, seq_ll as usize, offset, seq_ml as usize)
                    .map_err(DecompressBlockError::ExecuteSequencesError)?;
            }
            return Ok(());
        }
        // extDict territory — fall through to legacy.
    }

    buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;
    buffer
        .repeat_lookahead_prefetched(resolved_offset as usize, seq_ml as usize)
        .map_err(ExecuteSequencesError::from)?;
    Ok(())
}
