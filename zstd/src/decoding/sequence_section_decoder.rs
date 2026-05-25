use super::super::blocks::sequence_section::ModeType;
use super::super::blocks::sequence_section::Sequence;
use super::super::blocks::sequence_section::SequencesHeader;
use super::scratch::FSEScratch;
use crate::bit_io::BitReaderReversed;
use crate::cpu_kernel::{CpuKernel, CpuKernelTag, ScalarKernel, detect_cpu_kernel};
#[cfg(target_arch = "x86_64")]
use crate::cpu_kernel::{Avx2Kernel, Bmi2Kernel, Vbmi2Kernel};
use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};
use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError, ExecuteSequencesError};
use crate::decoding::sequence_execution::{do_offset_history, execute_sequences_fields};
use crate::fse::FSEDecoder;
use alloc::vec::Vec;

/// Fused decode + execute pipeline: decodes each sequence from the FSE
/// bitstream and immediately executes it (literal copy + match copy)
/// without materialising the intermediate `Vec<Sequence>` round-trip.
///
/// Donor parity: zstd's `ZSTD_decompressSequences_body` interleaves
/// `ZSTD_decodeSequence` and `ZSTD_execSequence` in one loop, keeping
/// the `seq_t` in registers. We were paying ~24 B/seq × 2 (write + read)
/// of L1↔L2 traffic on the dropped Vec<Sequence> roundtrip plus the
/// per-iter Vec::push overhead.
///
/// Falls back to the legacy two-pass pipeline (`decode_sequences` +
/// `execute_sequences`) when any of LL/ML/OF is in RLE mode — that path
/// is rare on perf-relevant corpora and not worth duplicating.
pub fn decode_and_execute_sequences<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    // Per-block CpuKernel dispatch — same pattern as decompress_literals.
    // Wrapped arms put the entire decode_and_execute_sequences_impl::<B, K>
    // pipeline under the matching target_feature so K::mask_lower_bits
    // intrinsics inline through the trait-method trampolines.
    match detect_cpu_kernel() {
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Vbmi2 => unsafe { decode_and_execute_sequences_vbmi2::<B>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences) },
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Avx2 => unsafe { decode_and_execute_sequences_avx2::<B>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences) },
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Bmi2 => unsafe { decode_and_execute_sequences_bmi2::<B>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences) },
        _ => decode_and_execute_sequences_impl::<B, ScalarKernel>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences),
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2")]
unsafe fn decode_and_execute_sequences_avx2<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader, source: &[u8], fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3], literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, Avx2Kernel>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
unsafe fn decode_and_execute_sequences_bmi2<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader, source: &[u8], fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3], literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, Bmi2Kernel>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512vbmi2,avx512f,avx512vl,avx512bw,bmi2,avx2")]
unsafe fn decode_and_execute_sequences_vbmi2<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader, source: &[u8], fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3], literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, Vbmi2Kernel>(section, source, fse, buffer, offset_hist, literals_buffer, rle_fallback_sequences)
}

#[inline]
fn decode_and_execute_sequences_impl<B: super::buffer_backend::BufferBackend, K: CpuKernel>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    // Reset the fallback sequences vec on entry. The non-RLE fast path
    // never writes to it, so without this clear it would carry whatever
    // entries the previous block left behind — a stale-data hazard for
    // any external caller that inspects scratch.sequences after decode.
    rle_fallback_sequences.clear();

    let bytes_read = maybe_update_fse_tables(section, source, fse)?;
    vprintln!("Updating tables used {} bytes", bytes_read);

    let bit_stream = &source[bytes_read..];
    let mut br = BitReaderReversed::<K>::new(bit_stream);

    // Skip the 0-padding at the end of the last byte and consume the
    // start-of-stream `1` bit.
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

    // RLE-mode blocks: fall back to the legacy two-pass pipeline. These
    // are uncommon in real-world corpora; fusing them too would double
    // the source maintenance for zero observed wins.
    if fse.ll_rle.is_some() || fse.ml_rle.is_some() || fse.of_rle.is_some() {
        decode_sequences_with_rle(section, &mut br, fse, rle_fallback_sequences)?;
        execute_sequences_fields(buffer, literals_buffer, offset_hist, rle_fallback_sequences)?;
        return Ok(());
    }

    let mut ll_dec = FSEDecoder::new(&fse.literal_lengths);
    let mut ml_dec = FSEDecoder::new(&fse.match_lengths);
    let mut of_dec = FSEDecoder::new(&fse.offsets);

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

    // Transactional rollback state. The fused decode+execute commits
    // each sequence's side-effects (literal push, match repeat, offset
    // history update) immediately, but the bitstream-exhaustion check
    // happens once after the loop. If that final check fails on a
    // malformed input, restore the buffer write cursor and offset
    // history to their pre-loop values so the caller observes the
    // legacy two-pass semantics: an Err leaves no partial output and no
    // mutated repeat-history behind.
    let buffer_checkpoint = buffer.checkpoint();
    let saved_offset_hist = *offset_hist;

    // `offset_hist` mutation on the in-band success path:
    //   * Pipelined branch (long pipeline): real `offset_hist` is NOT
    //     touched per-sequence — repcodes are resolved against the
    //     local `shadow_hist`, and the resolved offset is stored in
    //     the ring alongside the decoded sequence. After successful
    //     drain we copy `shadow_hist` back into `*offset_hist` once.
    //   * Non-pipelined branch (short-block fallback): real
    //     `offset_hist` IS mutated inline via `do_offset_history` in
    //     `execute_one_sequence`.
    //   * Rollback path (post-loop bitstream check fails): restore
    //     `*offset_hist = saved_offset_hist`. Cheap no-op on the
    //     pipelined branch (real hist was never touched mid-loop),
    //     correct rewind on the non-pipelined branch.
    #[inline(always)]
    fn execute_one_sequence<B: super::buffer_backend::BufferBackend>(
        buffer: &mut super::decode_buffer::DecodeBuffer<B>,
        literals: &[u8],
        lit_cur: &mut usize,
        lit_len: usize,
        offset_hist: &mut [u32; 3],
        seq: Sequence,
    ) -> Result<(), DecompressBlockError> {
        let high = *lit_cur + seq.ll as usize;
        if high > lit_len {
            return Err(ExecuteSequencesError::NotEnoughBytesForSequence {
                wanted: high,
                have: lit_len,
            }
            .into());
        }
        // SAFETY: high <= lit_len (just verified) and *lit_cur <= high
        // (high = lit_cur + seq.ll, seq.ll >= 0).
        let lits = unsafe { literals.get_unchecked(*lit_cur..high) };
        *lit_cur = high;
        buffer.push(lits);

        let actual = do_offset_history(seq.of, seq.ll, offset_hist);
        if actual == 0 {
            return Err(ExecuteSequencesError::ZeroOffset.into());
        }
        buffer
            .repeat(actual as usize, seq.ml as usize)
            .map_err(ExecuteSequencesError::from)?;
        Ok(())
    }

    /// Pipelined-path variant: takes the offset already resolved by
    /// the decode-ahead `shadow_hist` walk, so `do_offset_history` is
    /// NOT called here (caller mutated only the shadow). Routes the
    /// match copy through `repeat_lookahead_prefetched`, which skips
    /// only the in-loop `prefetch_match_source` (redundant because
    /// the lookahead pipeline already issued a PREFETCH_L1 ADVANCE
    /// iterations earlier). The per-call `buffer.reserve(match_length)`
    /// is preserved by that variant — required for memory safety
    /// against malformed inputs whose `match_length` exceeds the
    /// upfront `reserve(MAX_BLOCK_SIZE)` headroom.
    #[inline(always)]
    fn execute_one_sequence_pipelined<B: super::buffer_backend::BufferBackend>(
        buffer: &mut super::decode_buffer::DecodeBuffer<B>,
        literals: &[u8],
        lit_cur: &mut usize,
        lit_len: usize,
        seq: Sequence,
        resolved_offset: u32,
    ) -> Result<(), DecompressBlockError> {
        let high = *lit_cur + seq.ll as usize;
        if high > lit_len {
            return Err(ExecuteSequencesError::NotEnoughBytesForSequence {
                wanted: high,
                have: lit_len,
            }
            .into());
        }
        // SAFETY: high <= lit_len (just verified) and *lit_cur <= high.
        let lits = unsafe { literals.get_unchecked(*lit_cur..high) };
        *lit_cur = high;
        buffer.push(lits);

        if resolved_offset == 0 {
            return Err(ExecuteSequencesError::ZeroOffset.into());
        }
        buffer
            .repeat_lookahead_prefetched(resolved_offset as usize, seq.ml as usize)
            .map_err(ExecuteSequencesError::from)?;
        Ok(())
    }

    let num_sequences = section.num_sequences as usize;

    // 8-slot software pipeline mirroring donor
    // `ZSTD_decompressSequencesLong_body`. Pre-decode `ADVANCE`
    // sequences ahead, prefetch each match source as we go, then
    // execute the oldest in-flight sequence per iteration while
    // decoding the next one. By the time `execute_one_sequence`
    // reaches `buffer.repeat()` for slot k, the prefetch issued
    // `ADVANCE` iterations earlier has had time to pull the source
    // line(s) into L1/L2 — hiding DRAM latency for long-distance
    // matches whose source is beyond cache residency.
    //
    // Donor parity: `STORED_SEQS = 8`. 8-deep lookahead lets the
    // prefetch issued at iteration `i` resolve through L1/L2 by the
    // time iteration `i + 8` consumes it, whereas 4-deep often
    // wasn't enough gap on the long-distance workloads we target.
    // The on-stack ring is `[(Sequence, u32); 8]` = 128 bytes (the
    // u32 carries the resolved offset from the decode-ahead shadow
    // walk so the execute side can skip do_offset_history); still
    // well within register-pressure budget.
    const ADVANCE: usize = 8;
    const ADVANCE_MASK: usize = ADVANCE - 1;
    // `i & ADVANCE_MASK` only equals `i % ADVANCE` when ADVANCE is a
    // power of two. Compile-time guard so a future ADVANCE tweak
    // can't silently corrupt the ring index if someone picks a
    // non-power-of-two value.
    const _: () = assert!(
        ADVANCE.is_power_of_two(),
        "ADVANCE must be a power of two; ring indexing uses `i & (ADVANCE - 1)` as `i % ADVANCE`"
    );

    // Donor `ZSTD_getOffsetInfo` parity. The share of FSE offset
    // codes > LONG_OFFSET_CODE_THRESHOLD (scaled to donor's
    // OffFSELog = 8 reference) is computed once per table refresh
    // and cached in `fse.offsets_long_share` — see
    // `compute_offsets_long_share` and the `maybe_update_fse_tables`
    // call sites. Repeat-mode blocks (the table didn't change)
    // re-use the cached value without re-walking 32–256 table
    // entries per block. Gate stays sequence-count-first so short /
    // no-sequence blocks don't even read the cache.
    //
    // Donor `minShare = MEM_64bits() ? 7 : 20`: the 32-bit
    // threshold is higher because the prefetch pipeline needs a
    // stronger long-offset signal to outpace the narrower load
    // window on those targets.
    #[cfg(target_pointer_width = "64")]
    const MIN_LONG_OFFSET_SHARE: u32 = 7;
    #[cfg(not(target_pointer_width = "64"))]
    const MIN_LONG_OFFSET_SHARE: u32 = 20;
    let use_long_pipeline =
        num_sequences >= ADVANCE * 2 && fse.offsets_long_share >= MIN_LONG_OFFSET_SHARE;
    // Donor also engages the prefetch decoder when the dictionary is
    // cold or when the format-level `isLongOffset` flag is set. We
    // don't track dictionary-coldness on this decode path and the
    // 32-bit `isLongOffset` shortcut is irrelevant on the
    // u32-indexed decoder, so the FSE-share signal carries the
    // whole decision.

    if use_long_pipeline {
        // The pipelined branch must roll `offset_hist` back to
        // `saved_offset_hist` on ANY mid-loop error, not just the
        // post-loop bitstream-validation path. Without this, an
        // `execute_one_sequence_pipelined` Err (NotEnoughBytesForSequence
        // / ZeroOffset / OOB match) propagated via `?` would exit with
        // `*offset_hist` still at its pre-block value while the buffer
        // had N-1 partial writes — diverging from the non-pipelined
        // path (which mutates hist in lockstep per executed sequence)
        // and leaving scratch internally inconsistent for any
        // post-Err reuse. Wrap the entire pipelined work in an IIFE so
        // a single rollback site catches all mid-loop Errs uniformly.
        let pipeline_result: Result<(), DecompressBlockError> = (|| {
            // `prefetch_pos` is the logical buffer index (same frame as
            // `buffer.len()`) at which the NEXT not-yet-decoded sequence
            // will start pushing literals. We pre-decode `ADVANCE` ahead, so we
            // accumulate (ll + ml) per decoded seq to keep this position
            // synchronised with where execute will eventually be.
            let mut prefetch_pos: usize = old_buffer_size;
            // Shadow copy of `offset_hist`, advanced by
            // `do_offset_history` for every decoded-ahead sequence. The
            // REAL `offset_hist` is only mutated inside
            // `execute_one_sequence` (preserving the legacy 'partial
            // output, no rewound history' rollback contract), but the
            // prefetch needs the exact post-resolution offset for repcode
            // 1..=3 cases that read history — a stale read would skip
            // the long-distance prefetch precisely when a fresh huge
            // offset is followed by a repcode that aliases it. The
            // shadow is a local `[u32; 3]` (12 bytes) so the simulation cost
            // is negligible.
            let mut shadow_hist: [u32; 3] = *offset_hist;
            // Stack ring of `(decoded_seq, resolved_offset)` pairs. The
            // decode-ahead phase resolves repcodes against `shadow_hist`
            // and stores the resolved offset alongside the raw sequence,
            // so the execute phase consumes a pre-resolved offset and
            // skips `do_offset_history` entirely — saves one function
            // call + one cache write on real `offset_hist` per sequence.
            // The real `offset_hist` is updated ONCE from `shadow_hist`
            // after a successful drain (below); on a malformed-block
            // rollback the saved snapshot is restored, so real hist is
            // never observed in a partial mid-pipeline state.
            let mut ring: [(Sequence, u32); ADVANCE] = [(
                Sequence {
                    ll: 0,
                    ml: 0,
                    of: 0,
                },
                0u32,
            ); ADVANCE];

            // Pre-fill the ring. The outer `num_sequences >= ADVANCE * 2`
            // gate guarantees `num_sequences > ADVANCE`, so the FSE
            // state update is needed after every prefill decode — no
            // `isLastSeq` guard required here, only in the steady-state
            // loop where `i + 1 == num_sequences` is reachable.
            for slot in ring.iter_mut() {
                let seq =
                    decode_one_sequence_inline(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
                // EXACT actual_offset via shadow history.
                let actual_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
                // wrapping_add: prefetch_pos / seq.ll / seq.ml are
                // derived from the bitstream, so a malformed frame can
                // present values that would overflow usize and panic
                // under debug. The result feeds only the prefetch
                // hint — `prefetch_lookahead_match_source` bound-checks
                // the logical position against `buffer.len()` and drops
                // wrap-derived garbage indices, so the wrap is harmless
                // here while keeping the decoder fuzz-stable.
                let match_start = prefetch_pos.wrapping_add(seq.ll as usize);
                let source_idx = match_start.wrapping_sub(actual_offset as usize);
                buffer.prefetch_lookahead_match_source(source_idx);
                prefetch_pos = match_start.wrapping_add(seq.ml as usize);
                *slot = (seq, actual_offset);
                br.ensure_bits(max_update_bits);
                ll_dec.update_state_fast(&mut br);
                ml_dec.update_state_fast(&mut br);
                of_dec.update_state_fast(&mut br);
            }

            // Steady state: decode next, prefetch its source, execute
            // the oldest slot in the ring (with its pre-resolved offset).
            for i in ADVANCE..num_sequences {
                let seq =
                    decode_one_sequence_inline(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
                let actual_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
                let match_start = prefetch_pos.wrapping_add(seq.ll as usize);
                let source_idx = match_start.wrapping_sub(actual_offset as usize);
                buffer.prefetch_lookahead_match_source(source_idx);
                prefetch_pos = match_start.wrapping_add(seq.ml as usize);

                let slot = i & ADVANCE_MASK;
                let (exec_seq, exec_offset) = ring[slot];
                ring[slot] = (seq, actual_offset);

                execute_one_sequence_pipelined(
                    buffer,
                    literals_buffer,
                    &mut lit_cur,
                    literals_buffer_len,
                    exec_seq,
                    exec_offset,
                )?;
                seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);

                if i + 1 < num_sequences {
                    br.ensure_bits(max_update_bits);
                    ll_dec.update_state_fast(&mut br);
                    ml_dec.update_state_fast(&mut br);
                    of_dec.update_state_fast(&mut br);
                }
            }

            // Drain: execute remaining ADVANCE sequences with their
            // pre-resolved offsets. Iteration order matches the ring
            // slot they occupy from the steady-state loop's final write.
            for k in 0..ADVANCE {
                let slot = (num_sequences + k) & ADVANCE_MASK;
                let (exec_seq, exec_offset) = ring[slot];
                execute_one_sequence_pipelined(
                    buffer,
                    literals_buffer,
                    &mut lit_cur,
                    literals_buffer_len,
                    exec_seq,
                    exec_offset,
                )?;
                seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);
            }

            // Single committing point for real offset history on the
            // pipelined success path. Shadow walked every queued
            // sequence already; copy that state back so the next
            // block sees the post-block repcodes. Rollback on a later
            // bitstream-failure overwrites this with
            // `saved_offset_hist`, undoing the commit.
            *offset_hist = shadow_hist;
            Ok(())
        })();
        if let Err(e) = pipeline_result {
            // Mid-loop execute Err: rollback buffer + hist so post-Err
            // scratch reuse stays consistent. `*offset_hist` is still
            // at its pre-block value (the success-only commit above
            // never ran), so restoring from `saved_offset_hist` is
            // effectively a no-op on the hist side — the explicit
            // assignment makes the intent unambiguous and protects
            // against any future refactor that moves the commit
            // earlier in the pipelined flow.
            if buffer.try_restore_checkpoint(buffer_checkpoint) {
                *offset_hist = saved_offset_hist;
            }
            return Err(e);
        }
    } else {
        // Short-block fallback: the single-pass fused loop. For
        // num_sequences < ADVANCE * 2 the pipeline's prefill + drain
        // dominates the cycles saved by prefetch lookahead, so the
        // simpler shape wins. Inlined here (rather than a separate
        // function) so the cold tail-call cost of swapping decoders
        // mid-block stays at zero.
        for i in 0..num_sequences {
            let seq = decode_one_sequence_inline(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
            execute_one_sequence(
                buffer,
                literals_buffer,
                &mut lit_cur,
                literals_buffer_len,
                offset_hist,
                seq,
            )?;
            seq_sum = seq_sum.wrapping_add(seq.ll).wrapping_add(seq.ml);

            if i + 1 < num_sequences {
                br.ensure_bits(max_update_bits);
                ll_dec.update_state_fast(&mut br);
                ml_dec.update_state_fast(&mut br);
                of_dec.update_state_fast(&mut br);
            }
        }
    }

    // Post-loop bitstream validation. On failure roll back the buffer
    // and offset history so a malformed block leaves no partial
    // side-effects behind — restoring the transactional contract the
    // legacy two-pass pipeline upheld.
    let remaining = br.bits_remaining();
    if remaining != 0 {
        // try_restore_checkpoint succeeds when no reallocation happened
        // between the checkpoint and now (the common case: upfront
        // reserve(MAX_BLOCK_SIZE) covers a well-formed block). When a
        // malformed block decodes past that bound, reserve_amortized
        // fires and compacts the ring buffer — the captured tail is no
        // longer meaningful and the rollback is skipped. Either way the
        // caller observes the same Err below; the partial data left in
        // the buffer in the latter case is discarded with the frame.
        //
        // Crucially, only restore the repcode history when the buffer
        // rollback actually happened. If the buffer keeps its
        // speculative bytes, rewinding `offset_hist` would leave the
        // workspace internally inconsistent for any subsequent reuse
        // after the `Err`.
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

    // Tail literals: any bytes in the literals_buffer that no sequence
    // claimed get pushed after the last sequence.
    if lit_cur < literals_buffer_len {
        let rest = &literals_buffer[lit_cur..];
        buffer.push(rest);
        seq_sum = seq_sum.wrapping_add(rest.len() as u32);
    }

    let diff = buffer.len() - old_buffer_size;
    debug_assert_eq!(
        seq_sum as usize, diff,
        "seq_sum {seq_sum} != buffer growth {diff}"
    );
    Ok(())
}

/// Per-sequence decode helper used by `decode_and_execute_sequences`.
/// Identical to the inner `decode_one_sequence` of
/// `decode_sequences_without_rle` — separate copy because Rust does not
/// let us share a private fn-item across two outer functions cleanly.
#[inline(always)]
fn decode_one_sequence_inline<K: CpuKernel>(
    ll_dec: &mut FSEDecoder<'_>,
    ml_dec: &mut FSEDecoder<'_>,
    of_dec: &mut FSEDecoder<'_>,
    br: &mut BitReaderReversed<'_, K>,
) -> Sequence {
    // Read base/extra-bits directly off the active FSE state's
    // `Entry` (populated by `enrich_with_packed_seq_meta` /
    // `enrich_for_offsets` during table build). Drops the previous
    // `lookup_ll_code` / `lookup_ml_code` indirections — those did
    // a second cache touch on the separate `LL_META` / `ML_META`
    // tables per sequence. The active entry was already loaded
    // when `decode_symbol` read `state.symbol`, so the new fields
    // come for free in the same cache line.
    let ll_state = ll_dec.state;
    let ml_state = ml_dec.state;
    let of_code = of_dec.state.symbol;

    let ll_value = ll_state.base_value;
    let ll_num_bits = ll_state.num_additional_bits;
    let ml_value = ml_state.base_value;
    let ml_num_bits = ml_state.num_additional_bits;

    debug_assert!(of_code <= MAX_OFFSET_CODE);

    let (obits, ml_add, ll_add) = br.get_bits_triple(of_code, ml_num_bits, ll_num_bits);
    let offset = obits as u32 + (1u32 << of_code);

    debug_assert_ne!(offset, 0);

    Sequence {
        ll: ll_value + ll_add as u32,
        ml: ml_value + ml_add as u32,
        of: offset,
    }
}

fn decode_sequences_with_rle<K: CpuKernel>(
    section: &SequencesHeader,
    br: &mut BitReaderReversed<'_, K>,
    scratch: &FSEScratch,
    target: &mut Vec<Sequence>,
) -> Result<(), DecodeSequenceError> {
    let mut ll_dec = FSEDecoder::new(&scratch.literal_lengths);
    let mut ml_dec = FSEDecoder::new(&scratch.match_lengths);
    let mut of_dec = FSEDecoder::new(&scratch.offsets);

    if scratch.ll_rle.is_none() {
        ll_dec.init_state(br)?;
    }
    if scratch.of_rle.is_none() {
        of_dec.init_state(br)?;
    }
    if scratch.ml_rle.is_none() {
        ml_dec.init_state(br)?;
    }

    target.clear();
    target.reserve(section.num_sequences as usize);

    // Only non-RLE decoders need state updates; compute their combined worst-case.
    let max_update_bits = if scratch.ll_rle.is_none() {
        scratch.literal_lengths.accuracy_log
    } else {
        0
    } + if scratch.ml_rle.is_none() {
        scratch.match_lengths.accuracy_log
    } else {
        0
    } + if scratch.of_rle.is_none() {
        scratch.offsets.accuracy_log
    } else {
        0
    };
    debug_assert!(
        max_update_bits <= 56,
        "sequence section update bits exceed 56-bit budget"
    );

    for _seq_idx in 0..section.num_sequences {
        //get the codes from either the RLE byte or from the decoder
        let ll_code = if let Some(ll_rle) = scratch.ll_rle {
            ll_rle
        } else {
            ll_dec.decode_symbol()
        };
        let ml_code = if let Some(ml_rle) = scratch.ml_rle {
            ml_rle
        } else {
            ml_dec.decode_symbol()
        };
        let of_code = if let Some(of_rle) = scratch.of_rle {
            of_rle
        } else {
            of_dec.decode_symbol()
        };

        // RLE-mode tables don't have an enriched FSE entry to read
        // from — fall back to `lookup_ll_code` / `lookup_ml_code`
        // for the RLE byte. FSE-mode tables read base / extra-bits
        // directly off the active state's enriched `Entry`. The
        // RLE-fallback path is the only place these `lookup_*`
        // helpers are still used after #247 Part 1.
        let (ll_value, ll_num_bits) = if scratch.ll_rle.is_some() {
            lookup_ll_code(ll_code)
        } else {
            (ll_dec.state.base_value, ll_dec.state.num_additional_bits)
        };
        let (ml_value, ml_num_bits) = if scratch.ml_rle.is_some() {
            lookup_ml_code(ml_code)
        } else {
            (ml_dec.state.base_value, ml_dec.state.num_additional_bits)
        };

        // OF code / offset==0 checks dropped per FSE invariants (see comment
        // in decode_sequences_without_rle). For RLE mode, the singleton
        // of_rle byte is validated at maybe_update_fse_tables; for FSE mode,
        // build_decoding_table caps symbols at MAX_OFFSET_CODE.
        debug_assert!(of_code <= MAX_OFFSET_CODE);

        let (obits, ml_add, ll_add) = br.get_bits_triple(of_code, ml_num_bits, ll_num_bits);
        let offset = obits as u32 + (1u32 << of_code);

        debug_assert_ne!(offset, 0);

        target.push(Sequence {
            ll: ll_value + ll_add as u32,
            ml: ml_value + ml_add as u32,
            of: offset,
        });

        if target.len() < section.num_sequences as usize {
            // One refill check for all non-RLE state updates (batched fast path).
            if max_update_bits > 0 {
                br.ensure_bits(max_update_bits);
            }
            if scratch.ll_rle.is_none() {
                ll_dec.update_state_fast(br);
            }
            if scratch.ml_rle.is_none() {
                ml_dec.update_state_fast(br);
            }
            if scratch.of_rle.is_none() {
                of_dec.update_state_fast(br);
            }
        }

        if br.bits_remaining() < 0 {
            return Err(DecodeSequenceError::NotEnoughBytesForNumSequences);
        }
    }

    if br.bits_remaining() > 0 {
        Err(DecodeSequenceError::ExtraBits {
            bits_remaining: br.bits_remaining(),
        })
    } else {
        Ok(())
    }
}

/// Packed (baseline, extra_bits) pairs for literal-length codes.
/// Donor parity: `LL_base` + `LL_bits` from the zstd reference
/// (`zstd_compress_internal.h`). Per Zstandard format §3.1.1.3.2.1.1.1,
/// valid codes are 0..=35; the FSE decoder guarantees codes never
/// exceed 35 (table built with `max_symbol = MAX_LITERAL_LENGTH_CODE`
/// and `build_decoding_table` rejects oversize symbol probabilities;
/// RLE bytes range-checked in `maybe_update_fse_tables`). Release
/// builds rely on those upstream gates plus the `unsafe`
/// `get_unchecked` in the helper below; `debug_assert!` there is a
/// fuzz-time tripwire for future invariant breaks, not a runtime
/// release-mode bounds check.
///
/// Layout: low 24 bits = baseline (max 65536 fits), high 8 bits =
/// extra_bits (max 16). One u32 load on the hot path returns both
/// fields — replaces the previous pair of separate `LL_BASE[idx]` +
/// `LL_EXTRA_BITS[idx]` loads (two distinct cache-line touches into
/// 144 B + 36 B = 180 B; packed table is 144 B = one contiguous
/// region).
pub(crate) const LL_META: [u32; 36] = pack_code_meta(
    &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ],
    &[
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10,
        11, 12, 13, 14, 15, 16,
    ],
);

/// Packed (baseline, extra_bits) pairs for match-length codes.
/// Donor parity: `ML_base` + `ML_bits`. Codes 0..=52 per Zstandard
/// format §3.1.1.3.2.1.1.2. Same packed layout as [`LL_META`].
pub(crate) const ML_META: [u32; 53] = pack_code_meta(
    &[
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515,
        1027, 2051, 4099, 8195, 16387, 32771, 65539,
    ],
    &[
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ],
);

/// Build the packed (baseline, extra_bits) table at compile time so the
/// const arrays above are self-validating against the source spec.
const fn pack_code_meta<const N: usize>(bases: &[u32; N], extra_bits: &[u8; N]) -> [u32; N] {
    let mut out = [0u32; N];
    let mut i = 0;
    while i < N {
        // Compile-time gate: keep the high 8 bits of `bases[i]`
        // available for the packed extra_bits field, and keep
        // extra_bits within the Zstandard format limit (max 16 bits
        // per §3.1.1.3.2.1.1). Any spec extension that violates
        // either invariant fails the build instead of silently
        // clobbering the packed payload.
        assert!(bases[i] & 0xFF00_0000 == 0, "baseline must fit in 24 bits");
        assert!(extra_bits[i] <= 16, "extra_bits exceeds zstd format limit");
        out[i] = bases[i] | ((extra_bits[i] as u32) << 24);
        i += 1;
    }
    out
}

/// Unpack the (baseline, extra_bits) tuple from a packed [`LL_META`] /
/// [`ML_META`] entry. Inlined so the shift+mask collapses to ALU ops
/// with no cross-function call overhead on the hot path.
#[inline(always)]
const fn unpack_code_meta(meta: u32) -> (u32, u8) {
    (meta & 0x00FF_FFFF, (meta >> 24) as u8)
}

/// Look up the provided state value from a literal length table predefined
/// by the Zstandard reference document. Returns a tuple of (value, number of bits).
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#appendix-a---decoding-tables-for-predefined-codes>
#[inline(always)]
fn lookup_ll_code(code: u8) -> (u32, u8) {
    // The FSE LL table is constructed with `max_symbol =
    // MAX_LITERAL_LENGTH_CODE` (35); `build_decoding_table` returns
    // `FSETableError::TooManySymbols` if `read_probabilities` produces
    // more entries than that, and the RLE byte path is range-checked
    // in `maybe_update_fse_tables`. So a `code` reaching this lookup
    // is invariant 0..=35. Keep the `debug_assert` as a tripwire in
    // case a future caller forgets one of those validations; drop the
    // release-mode `assert!` so the hot path takes a single
    // `get_unchecked` instead of a bounds-checked indexed load.
    let idx = code as usize;
    debug_assert!(
        idx < LL_META.len(),
        "Illegal literal length code was: {code}"
    );
    // SAFETY: idx < LL_META.len() == 36 per the FSE table
    // construction invariant documented above.
    unpack_code_meta(unsafe { *LL_META.get_unchecked(idx) })
}

/// Look up the provided state value from a match length table predefined
/// by the Zstandard reference document. Returns a tuple of (value, number of bits).
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#appendix-a---decoding-tables-for-predefined-codes>
#[inline(always)]
fn lookup_ml_code(code: u8) -> (u32, u8) {
    // Same invariant as `lookup_ll_code`: the ML FSE table is built
    // with `max_symbol = MAX_MATCH_LENGTH_CODE` (52) and the RLE byte
    // is range-checked, so `code` reaching this lookup is 0..=52.
    let idx = code as usize;
    debug_assert!(idx < ML_META.len(), "Illegal match length code was: {code}");
    // SAFETY: idx < ML_META.len() == 53 per the FSE table
    // construction invariant.
    unpack_code_meta(unsafe { *ML_META.get_unchecked(idx) })
}

// This info is buried in the symbol compression mode table
/// "The maximum allowed accuracy log for literals length and match length tables is 9"
pub const LL_MAX_LOG: u8 = 9;
/// "The maximum allowed accuracy log for literals length and match length tables is 9"
pub const ML_MAX_LOG: u8 = 9;
/// "The maximum accuracy log for the offset table is 8."
pub const OF_MAX_LOG: u8 = 8;

/// Walk the offsets FSE decode table and return the donor-shaped
/// "share of long offsets" signal: count entries whose symbol (offset
/// code) is > 22 (raw offset ≥ 2²³ = 8 MiB), then scale up to the
/// donor `OffFSELog = 8` reference so a fine-grained table still
/// registers comparable share. Output compares directly against
/// `MIN_LONG_OFFSET_SHARE` (7 on 64-bit, 20 on 32-bit) in the
/// pipeline-gate decision.
///
/// Called only when the offsets table is actually rebuilt (FSE /
/// Predefined modes in `maybe_update_fse_tables`). Repeat-mode
/// blocks reuse the cached value in `FSEScratch::offsets_long_share`.
pub(crate) fn compute_offsets_long_share(offsets: &crate::fse::FSETable) -> u32 {
    const OFFSET_FSE_LOG: u32 = 8;
    const LONG_OFFSET_CODE_THRESHOLD: u32 = 22;
    let table_log = offsets.accuracy_log as u32;
    let raw = offsets
        .decode
        .iter()
        .filter(|entry| u32::from(entry.symbol) > LONG_OFFSET_CODE_THRESHOLD)
        .count() as u32;
    // Format-spec bound `OF_MAX_LOG = 8` keeps `table_log <=
    // OFFSET_FSE_LOG` for every valid offsets stream, so the shift
    // is wrap-free.
    raw << OFFSET_FSE_LOG.saturating_sub(table_log)
}

fn maybe_update_fse_tables(
    section: &SequencesHeader,
    source: &[u8],
    scratch: &mut FSEScratch,
) -> Result<usize, DecodeSequenceError> {
    let modes = section
        .modes
        .ok_or(DecodeSequenceError::MissingCompressionMode)?;

    let mut bytes_read = 0;

    match modes.ll_mode() {
        ModeType::FSECompressed => {
            let bytes = scratch.literal_lengths.build_decoder(source, LL_MAX_LOG)?;
            bytes_read += bytes;
            scratch
                .literal_lengths
                .enrich_with_packed_seq_meta(&LL_META);

            vprintln!("Updating ll table");
            vprintln!("Used bytes: {}", bytes);
            scratch.ll_rle = None;
        }
        ModeType::RLE => {
            vprintln!("Use RLE ll table");
            if source.is_empty() {
                return Err(DecodeSequenceError::MissingByteForRleLlTable);
            }
            bytes_read += 1;
            if source[0] > MAX_LITERAL_LENGTH_CODE {
                return Err(DecodeSequenceError::MissingByteForRleMlTable);
            }
            scratch.ll_rle = Some(source[0]);
        }
        ModeType::Predefined => {
            vprintln!("Use predefined ll table");
            scratch.literal_lengths.build_from_probabilities(
                LL_DEFAULT_ACC_LOG,
                &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
            )?;
            scratch
                .literal_lengths
                .enrich_with_packed_seq_meta(&LL_META);
            scratch.ll_rle = None;
        }
        ModeType::Repeat => {
            vprintln!("Repeat ll table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };

    let of_source = &source[bytes_read..];

    match modes.of_mode() {
        ModeType::FSECompressed => {
            let bytes = scratch.offsets.build_decoder(of_source, OF_MAX_LOG)?;
            scratch.offsets.enrich_for_offsets();
            vprintln!("Updating of table");
            vprintln!("Used bytes: {}", bytes);
            bytes_read += bytes;
            scratch.of_rle = None;
            scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
        }
        ModeType::RLE => {
            vprintln!("Use RLE of table");
            if of_source.is_empty() {
                return Err(DecodeSequenceError::MissingByteForRleOfTable);
            }
            bytes_read += 1;
            if of_source[0] > MAX_OFFSET_CODE {
                return Err(DecodeSequenceError::MissingByteForRleMlTable);
            }
            scratch.of_rle = Some(of_source[0]);
        }
        ModeType::Predefined => {
            vprintln!("Use predefined of table");
            scratch.offsets.build_from_probabilities(
                OF_DEFAULT_ACC_LOG,
                &Vec::from(&OFFSET_DEFAULT_DISTRIBUTION[..]),
            )?;
            scratch.offsets.enrich_for_offsets();
            scratch.of_rle = None;
            scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
        }
        ModeType::Repeat => {
            vprintln!("Repeat of table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };

    let ml_source = &source[bytes_read..];

    match modes.ml_mode() {
        ModeType::FSECompressed => {
            let bytes = scratch.match_lengths.build_decoder(ml_source, ML_MAX_LOG)?;
            scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
            bytes_read += bytes;
            vprintln!("Updating ml table");
            vprintln!("Used bytes: {}", bytes);
            scratch.ml_rle = None;
        }
        ModeType::RLE => {
            vprintln!("Use RLE ml table");
            if ml_source.is_empty() {
                return Err(DecodeSequenceError::MissingByteForRleMlTable);
            }
            bytes_read += 1;
            if ml_source[0] > MAX_MATCH_LENGTH_CODE {
                return Err(DecodeSequenceError::MissingByteForRleMlTable);
            }
            scratch.ml_rle = Some(ml_source[0]);
        }
        ModeType::Predefined => {
            vprintln!("Use predefined ml table");
            scratch.match_lengths.build_from_probabilities(
                ML_DEFAULT_ACC_LOG,
                &Vec::from(&MATCH_LENGTH_DEFAULT_DISTRIBUTION[..]),
            )?;
            scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
            scratch.ml_rle = None;
        }
        ModeType::Repeat => {
            vprintln!("Repeat ml table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };

    Ok(bytes_read)
}

// The default Literal Length decoding table uses an accuracy logarithm of 6 bits.
const LL_DEFAULT_ACC_LOG: u8 = 6;
/// If [ModeType::Predefined] is selected for a symbol type, its FSE decoding
/// table is generated using a predefined distribution table.
///
/// https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#literals-length
const LITERALS_LENGTH_DEFAULT_DISTRIBUTION: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

// The default Match Length decoding table uses an accuracy logarithm of 6 bits.
const ML_DEFAULT_ACC_LOG: u8 = 6;
/// If [ModeType::Predefined] is selected for a symbol type, its FSE decoding
/// table is generated using a predefined distribution table.
///
/// https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#match-length
const MATCH_LENGTH_DEFAULT_DISTRIBUTION: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

// The default Match Length decoding table uses an accuracy logarithm of 5 bits.
const OF_DEFAULT_ACC_LOG: u8 = 5;
/// If [ModeType::Predefined] is selected for a symbol type, its FSE decoding
/// table is generated using a predefined distribution table.
///
/// https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#match-length
const OFFSET_DEFAULT_DISTRIBUTION: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

#[test]
fn test_ll_default() {
    let mut table = crate::fse::FSETable::new(MAX_LITERAL_LENGTH_CODE);
    table
        .build_from_probabilities(
            LL_DEFAULT_ACC_LOG,
            &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();

    assert!(table.decode.len() == 64);

    //just test a few values. TODO test all values
    assert!(table.decode[0].symbol == 0);
    assert!(table.decode[0].num_bits == 4);
    assert!(table.decode[0].new_state == 0);

    assert!(table.decode[19].symbol == 27);
    assert!(table.decode[19].num_bits == 6);
    assert!(table.decode[19].new_state == 0);

    assert!(table.decode[39].symbol == 25);
    assert!(table.decode[39].num_bits == 4);
    assert!(table.decode[39].new_state == 16);

    assert!(table.decode[60].symbol == 35);
    assert!(table.decode[60].num_bits == 6);
    assert!(table.decode[60].new_state == 0);

    assert!(table.decode[59].symbol == 24);
    assert!(table.decode[59].num_bits == 5);
    assert!(table.decode[59].new_state == 32);
}

#[cfg(test)]
mod offsets_long_share_tests {
    use super::compute_offsets_long_share;
    use crate::fse::{Entry, FSETable};

    /// Construct a synthetic FSETable with the given symbol per entry
    /// at the requested accuracy_log. Bypasses `build_from_probabilities`
    /// — we only need `decode[*].symbol` and `accuracy_log` populated;
    /// the long-share helper reads exactly those.
    fn synthetic_offsets_table(accuracy_log: u8, symbols: &[u8]) -> FSETable {
        let size = 1usize << accuracy_log;
        assert_eq!(
            symbols.len(),
            size,
            "symbols.len() must equal 1 << accuracy_log"
        );
        let mut t = FSETable::new(31);
        t.accuracy_log = accuracy_log;
        t.decode = symbols
            .iter()
            .map(|&s| Entry {
                new_state: 0,
                symbol: s,
                num_bits: 0,
                base_value: 0,
                num_additional_bits: 0,
            })
            .collect();
        t
    }

    #[test]
    fn zero_long_codes_returns_zero_share() {
        // A table with only short offset codes (all symbols <= 22).
        // Donor parity: share is the count of symbols > 22, scaled to
        // OffFSELog = 8 — with zero such symbols, share is 0
        // regardless of accuracy_log.
        for log in [3u8, 5, 6, 8] {
            let size = 1usize << log;
            let symbols: alloc::vec::Vec<u8> = (0..size).map(|i| (i as u8) % 22).collect();
            let table = synthetic_offsets_table(log, &symbols);
            assert_eq!(
                compute_offsets_long_share(&table),
                0,
                "log={log}: pure short-offset table must score 0"
            );
        }
    }

    #[test]
    fn long_codes_scale_to_offset_fse_log_reference() {
        // accuracy_log = 5 → 32-entry table. One symbol at code 23
        // (just above the threshold of 22), the rest at 0. Donor
        // scales the raw count by `OffFSELog - accuracy_log` =
        // `8 - 5 = 3`, so 1 << 3 = 8 should land at the 64-bit
        // `MIN_LONG_OFFSET_SHARE = 7` threshold (just over).
        let mut symbols = [0u8; 32];
        symbols[7] = 23;
        let table = synthetic_offsets_table(5, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 8);
    }

    #[test]
    fn raw_count_at_offset_fse_log_passes_through_unscaled() {
        // accuracy_log = OffFSELog = 8 → 256-entry table. No scaling
        // applied (shift by zero), so the share equals the raw count
        // of symbols > 22.
        let mut symbols = [0u8; 256];
        for sym in symbols.iter_mut().take(15) {
            *sym = 25;
        }
        let table = synthetic_offsets_table(8, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 15);
    }

    #[test]
    fn threshold_is_strict_greater_than() {
        // Symbol == LONG_OFFSET_CODE_THRESHOLD (22) does NOT count —
        // matches donor `> 22` strict-greater predicate. Only
        // symbols 23..MAX raise the share.
        let mut symbols = [0u8; 256];
        for sym in symbols.iter_mut().take(50) {
            *sym = 22;
        }
        let table = synthetic_offsets_table(8, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 0);
        symbols[0] = 23;
        let table = synthetic_offsets_table(8, &symbols);
        assert_eq!(compute_offsets_long_share(&table), 1);
    }
}
