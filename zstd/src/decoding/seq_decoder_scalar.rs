//! Portable sequence-section decoder: the scalar entry point plus the shared
//! body the Scalar, NEON, SVE and BMI2 tiers all run. It is generic over the
//! kernel, so the BMI2 entry inlines it under its own `target_feature`.

use super::buffer_backend::BufferBackend;
use super::decode_buffer::DecodeBuffer;
use super::scratch::FSEScratch;
use super::sequence_section_decoder::{
    ADVANCE, ADVANCE_MASK, ExecSeq, SeqStreamSetup, init_sequence_stream,
};
use crate::bit_io::BitReaderReversed;
use crate::blocks::sequence_section::{MAX_OFFSET_CODE, Sequence, SequencesHeader};
use crate::cpu_kernel::{CpuKernel, ScalarKernel};
use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError, ExecuteSequencesError};
use crate::decoding::sequence_execution::do_offset_history;
use crate::fse::SeqFSEDecoder;

macro_rules! decode_one_body {
    ($ll_dec:expr, $ml_dec:expr, $of_dec:expr, $br:expr) => {{
        let ll_state = $ll_dec.state;
        let ml_state = $ml_dec.state;
        let of_state = $of_dec.state;

        let ll_value = ll_state.base_value;
        let ll_num_bits = ll_state.num_additional_bits;
        let ml_value = ml_state.base_value;
        let ml_num_bits = ml_state.num_additional_bits;
        let of_num_bits = of_state.num_additional_bits;
        let of_base = of_state.base_value;

        debug_assert!(of_num_bits <= MAX_OFFSET_CODE);

        let (obits, ml_add, ll_add) = $br.get_bits_triple(of_num_bits, ml_num_bits, ll_num_bits);
        let offset = obits as u32 + of_base;
        debug_assert_ne!(offset, 0);

        Sequence {
            ll: ll_value + ll_add as u32,
            ml: ml_value + ml_add as u32,
            of: offset,
        }
    }};
}

// The validated sequence tables give widths 1..=31. Unlike a general bit
// read, this never needs a mask for width zero. The caller budgets the reads.
#[inline(always)]
fn read_additional_bits<K: CpuKernel>(br: &mut BitReaderReversed<'_, K>, n: u8) -> u32 {
    debug_assert!(n > 0 && n <= 31);
    debug_assert!(br.bits_consumed + n <= 64);
    let value = br.bit_container.wrapping_shl(u32::from(br.bits_consumed)) >> (64 - u32::from(n));
    br.consume(n);
    value as u32
}

/// Resolve repcodes while reading their offset bits, avoiding the generic
/// offset-value decoding/selection round trip for the common zero-bit code.
/// Refills before LL to cover its bits plus all three FSE updates (9+9+8).
/// Returns with at least 26 bits remaining for those updates, including at
/// stream end, where `BitReaderReversed` accounts for zero padding as extra bits.
///
/// # Safety
/// The reader must have an initialized window: if `br.index >= 8`, the
/// eight-byte window `br.source[br.index..br.index + 8]` must be in bounds.
/// On entry, `br.bits_consumed <= 8` (at least 56 buffered bits).
/// The decoders must contain validated sequence states, with additional-bit
/// widths at most 31 for OF and 16 each for LL and ML.
/// Successful `init_sequence_stream` followed by a refill establishes these
/// conditions; callers must preserve them across reads and state updates.
#[inline(always)]
pub(super) unsafe fn decode_resolved<K: CpuKernel>(
    ll_dec: &SeqFSEDecoder<'_>,
    ml_dec: &SeqFSEDecoder<'_>,
    of_dec: &SeqFSEDecoder<'_>,
    br: &mut BitReaderReversed<'_, K>,
    hist: &mut [u32; 3],
) -> Sequence {
    let ll = ll_dec.state;
    let ml = ml_dec.state;
    let of = of_dec.state;
    debug_assert!(of.num_additional_bits <= MAX_OFFSET_CODE);
    debug_assert!(ml.num_additional_bits <= 16 && ll.num_additional_bits <= 16);
    debug_assert!(br.bits_consumed <= 8);
    let offset = if of.num_additional_bits > 1 {
        let offset = of.base_value + read_additional_bits(br, of.num_additional_bits) - 3;
        hist[2] = hist[1];
        hist[1] = hist[0];
        hist[0] = offset;
        offset
    } else if of.num_additional_bits == 0 {
        debug_assert_eq!(of.base_value, 1);
        let offset = if ll.base_value == 0 { hist[1] } else { hist[0] };
        hist[1] = if ll.base_value == 0 { hist[0] } else { hist[1] };
        hist[0] = offset;
        offset
    } else {
        debug_assert_eq!(of.base_value, 2);
        let index = 1 + usize::from(ll.base_value == 0) + read_additional_bits(br, 1) as usize;
        let offset = if index == 3 {
            hist[0].wrapping_sub(1)
        } else {
            hist[index]
        };
        if index != 1 {
            hist[2] = hist[1];
        }
        hist[1] = hist[0];
        hist[0] = offset;
        offset
    };
    let ml_add = if ml.num_additional_bits == 0 {
        0
    } else {
        read_additional_bits(br, ml.num_additional_bits)
    };
    let ll_budget = ll.num_additional_bits + 9 + 9 + 8;
    debug_assert!(ll_budget <= 56);
    if br.bits_consumed + ll_budget > 64 {
        // SAFETY: the caller guarantees an initialized window and at most eight
        // consumed bits on entry. OF/ML consume at most 31+16 more, so the total
        // stays within 64 bits and the refill only retreats the valid window.
        unsafe { br.refill_sequence() };
    }
    let ll_add = if ll.num_additional_bits == 0 {
        0
    } else {
        read_additional_bits(br, ll.num_additional_bits)
    };
    Sequence {
        ll: ll.base_value + ll_add,
        ml: ml.base_value + ml_add,
        of: offset,
    }
}

/// Scalar-tier execute body. Routes through the shared
/// `execute_one_sequence_pipelined`, which uses the upstream zstd inline
/// literal+match wildcopy on backends that opt into
/// `SUPPORTS_INLINE_SEQUENCE_EXEC` (FlatBuf / UserSliceBackend, on every
/// target) and falls back to `try_push` + `repeat_lookahead_prefetched`
/// otherwise (RingBuffer, or when the per-sequence literal-slack /
/// prefix-resident gate fails). Every tier that runs this body (Scalar,
/// NEON, SVE, BMI2) reaches the inline path through it, and sharing the
/// executor keeps the literal-slack / offset gating in one place.
macro_rules! execute_one_body {
    (
        $buffer:expr,
        $dict:expr,
        $dict_content:expr,
        $literals_buffer:expr,
        $lit_cur:expr,
        $literals_buffer_len:expr,
        $seq_ll:expr,
        $seq_ml:expr,
        $resolved_offset:expr
    ) => {{
        let resolved_offset_v: u32 = $resolved_offset;
        // `of` is unused by the executor (it consumes the separately
        // resolved `resolved_offset_v`); set it to the resolved value so
        // the field carries a meaningful number rather than a sentinel.
        let seq = Sequence {
            ll: $seq_ll,
            ml: $seq_ml,
            of: resolved_offset_v,
        };
        super::sequence_section_decoder::execute_one_sequence_pipelined(
            $buffer,
            $dict,
            $dict_content,
            $literals_buffer,
            $lit_cur,
            $literals_buffer_len,
            seq,
            resolved_offset_v,
        )
    }};
}

// The long-offset pipeline has ring and prefetch state of its own. Keep its
// logic separate from the short path while allowing the caller to inline it.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn decode_long_sequences<B: BufferBackend, K: CpuKernel>(
    br: &mut BitReaderReversed<'_, K>,
    ll_dec: &mut SeqFSEDecoder<'_>,
    ml_dec: &mut SeqFSEDecoder<'_>,
    of_dec: &mut SeqFSEDecoder<'_>,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    lit_cur: &mut usize,
    literals_buffer_len: usize,
    dict: Option<&crate::decoding::dictionary::Dictionary>,
    dict_content: &[u8],
    old_buffer_size: usize,
    num_sequences: usize,
    max_update_bits: u8,
) -> Result<u32, DecompressBlockError> {
    let mut seq_sum = 0u32;
    let mut prefetch_pos: usize = old_buffer_size;
    let mut shadow_hist: [u32; 3] = *offset_hist;
    let mut ring: [ExecSeq; ADVANCE] = [ExecSeq {
        ll: 0,
        ml: 0,
        actual_offset: 0,
    }; ADVANCE];

    for slot in ring.iter_mut() {
        let seq = decode_one_body!(ll_dec, ml_dec, of_dec, br);
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
        ll_dec.update_state_fast(br);
        ml_dec.update_state_fast(br);
        of_dec.update_state_fast(br);
    }

    let mut pipeline_err: Option<DecompressBlockError> = None;
    for i in ADVANCE..num_sequences {
        let seq = decode_one_body!(ll_dec, ml_dec, of_dec, br);
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

        let r = execute_one_body!(
            buffer,
            dict,
            dict_content,
            literals_buffer,
            lit_cur,
            literals_buffer_len,
            exec_seq.ll,
            exec_seq.ml,
            exec_seq.actual_offset
        );
        if let Err(e) = r {
            pipeline_err = Some(e);
            break;
        }
        seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);

        if i + 1 < num_sequences {
            br.ensure_bits(max_update_bits);
            ll_dec.update_state_fast(br);
            ml_dec.update_state_fast(br);
            of_dec.update_state_fast(br);
        }
    }

    if pipeline_err.is_none() {
        for k in 0..ADVANCE {
            let slot = (num_sequences + k) & ADVANCE_MASK;
            let exec_seq = ring[slot];
            let r = execute_one_body!(
                buffer,
                dict,
                dict_content,
                literals_buffer,
                lit_cur,
                literals_buffer_len,
                exec_seq.ll,
                exec_seq.ml,
                exec_seq.actual_offset
            );
            if let Err(e) = r {
                pipeline_err = Some(e);
                break;
            }
            seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);
        }
    }

    if let Some(e) = pipeline_err {
        return Err(e);
    }
    *offset_hist = shadow_hist;
    Ok(seq_sum)
}

/// Scalar-tier monolithic decode + execute.
#[allow(clippy::too_many_lines)]
// The block's inputs; see the AVX2 tier for why they stay separate.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(target_arch = "x86_64", inline(never))]
pub(crate) fn decode_and_execute_sequences_scalar<'fse, B: BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &'fse mut FSEScratch,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    literals_len: usize,
    dict: Option<&'fse crate::decoding::dictionary::Dictionary>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, ScalarKernel>(
        section,
        source,
        fse,
        buffer,
        offset_hist,
        literals_buffer,
        literals_len,
        dict,
    )
}

// Shared Scalar/NEON/SVE/BMI2 sequence algorithm. Inlining keeps the chosen
// kernel and its bit-mask operations inside the caller's target-feature boundary.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[inline(always)]
pub(super) fn decode_and_execute_sequences_impl<'fse, B: BufferBackend, K: CpuKernel>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &'fse mut FSEScratch,
    buffer: &mut DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    literals_len: usize,
    dict: Option<&'fse crate::decoding::dictionary::Dictionary>,
) -> Result<(), DecompressBlockError> {
    let SeqStreamSetup {
        mut br,
        mut ll_dec,
        mut ml_dec,
        mut of_dec,
        max_update_bits,
        old_buffer_size,
        num_sequences,
        use_long_pipeline,
    } = init_sequence_stream::<B, K>(section, source, fse, buffer, dict)?;
    // `literals_buffer` runs past the literals by the copiers' read slack, so
    // the literal count is the parameter, never the slice's length.
    let literals_buffer_len = literals_len;
    debug_assert!(
        literals_buffer.len() >= literals_len + crate::WILDCOPY_OVERLENGTH,
        "literals view lacks the copiers' read slack: {} bytes for {literals_len} literals",
        literals_buffer.len(),
    );
    let mut lit_cur: usize = 0;
    let mut seq_sum: u32 = 0;
    // Invariant for the whole block, so it is resolved here rather than per
    // sequence inside the dictionary-source selector.
    let dict_content: &[u8] = match dict {
        Some(d) => &d.dict_content,
        None => &[],
    };

    // Each sequence commits its output at once, but the bitstream is only
    // checked for exhaustion after the loop. Repcodes resolve against a shadow
    // history committed on success. A sequence or bitstream failure restores
    // this checkpoint when the backend can roll back, and the history is
    // rewound only together with the output. A tail-literal overflow below
    // returns after both are committed.
    let buffer_checkpoint = buffer.checkpoint();
    let saved_offset_hist = *offset_hist;

    if use_long_pipeline {
        seq_sum = match decode_long_sequences(
            &mut br,
            &mut ll_dec,
            &mut ml_dec,
            &mut of_dec,
            buffer,
            offset_hist,
            literals_buffer,
            &mut lit_cur,
            literals_buffer_len,
            dict,
            dict_content,
            old_buffer_size,
            num_sequences,
            max_update_bits,
        ) {
            Ok(sum) => sum,
            Err(e) => {
                if buffer.try_restore_checkpoint(buffer_checkpoint) {
                    *offset_hist = saved_offset_hist;
                }
                return Err(e);
            }
        };
    } else {
        let mut shadow_hist = *offset_hist;
        let mut fallback_err: Option<DecompressBlockError> = None;
        debug_assert!(max_update_bits <= 9 + 9 + 8);
        // SAFETY: init_sequence_stream read the padding marker and FSE states,
        // establishing the reader window. The read budget keeps consumption <=64.
        unsafe { br.refill_sequence() };
        for i in 0..num_sequences {
            // SAFETY: init_sequence_stream validated the states and established
            // the reader window. The initial and per-sequence refills restore
            // bits_consumed < 8; decode_resolved reserves the state-update budget.
            let seq =
                unsafe { decode_resolved(&ll_dec, &ml_dec, &of_dec, &mut br, &mut shadow_hist) };
            let resolved_offset = seq.of;
            if i + 1 < num_sequences {
                ll_dec.update_state_fast(&mut br);
                ml_dec.update_state_fast(&mut br);
                of_dec.update_state_fast(&mut br);
                // Start the next input load before copying this sequence's output.
                // SAFETY: decode_resolved budgets these three state updates;
                // the source is unchanged and the initialized window only retreats.
                unsafe { br.refill_sequence() };
            }
            let r = execute_one_body!(
                buffer,
                dict,
                dict_content,
                literals_buffer,
                &mut lit_cur,
                literals_buffer_len,
                seq.ll,
                seq.ml,
                resolved_offset
            );
            if let Err(e) = r {
                fallback_err = Some(e);
                break;
            }
            seq_sum = seq_sum.wrapping_add(seq.ll).wrapping_add(seq.ml);
        }
        if let Some(e) = fallback_err {
            let _ = buffer.try_restore_checkpoint(buffer_checkpoint);
            return Err(e);
        }
        *offset_hist = shadow_hist;
    }

    let remaining = br.bits_remaining();
    if remaining != 0 {
        // Rewind the history only when the buffer rollback happened, or the
        // workspace would pair kept output with an older history.
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

    // Tail literals go through `try_push`, so an overshoot on the fixed-size
    // backend is an `OutputBufferOverflow`, not a panic. The per-block ceiling
    // is not re-checked: it bounds match writes, and the literals section was
    // already held to the block maximum when it was parsed.
    if lit_cur < literals_buffer_len {
        let rest = &literals_buffer[lit_cur..literals_buffer_len];
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

#[cfg(test)]
mod tests;
