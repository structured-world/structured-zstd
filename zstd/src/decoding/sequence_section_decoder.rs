use super::super::blocks::sequence_section::ModeType;
use super::super::blocks::sequence_section::Sequence;
use super::super::blocks::sequence_section::SequencesHeader;
use super::scratch::FSEScratch;
use crate::bit_io::BitReaderReversed;
use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};
use crate::common::MAX_BLOCK_SIZE;
use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError, ExecuteSequencesError};
use crate::decoding::sequence_execution::{do_offset_history, execute_sequences_fields};
use crate::fse::FSEDecoder;
use alloc::vec::Vec;

// 8-slot software pipeline mirroring donor
// `ZSTD_decompressSequencesLong_body`'s `STORED_SEQS = 8`. The
// 8-deep lookahead lets the prefetch issued at iteration `i`
// resolve through L1/L2 by the time iteration `i + 8` consumes it,
// whereas 4-deep often wasn't enough gap on long-distance workloads.
const ADVANCE: usize = 8;
const ADVANCE_MASK: usize = ADVANCE - 1;
// `i & ADVANCE_MASK` only equals `i % ADVANCE` when ADVANCE is a
// power of two. Compile-time guard so a future ADVANCE tweak can't
// silently corrupt the ring index.
const _: () = assert!(
    ADVANCE.is_power_of_two(),
    "ADVANCE must be a power of two; ring indexing uses `i & (ADVANCE - 1)` as `i % ADVANCE`"
);

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
/// Public entry. Resolves the CPU kernel — `OnceLock`-cached
/// runtime detect under `feature = "std"`, compile-time
/// `cfg(target_feature)` under `no_std` — then dispatches to a
/// kernel-monomorphised body so the inner pipeline's
/// `BitReaderReversed<K>` resolves `K::mask_lower_bits` at compile
/// time (one BMI2 `bzhi` codegen per bit-mask call, no per-call
/// kernel-selection dispatch). The per-call dispatch cost is one
/// `OnceLock::get` (std) or zero (no_std) plus a small `match` —
/// amortised over the whole block.
///
/// (Note: `BitReaderReversed::peek_bits_triple` still carries a
/// per-call `if self.use_pext_triple` branch under
/// `feature = "std"` + `target_arch = "x86_64"`, choosing between
/// scalar mask and PEXT extract. That branch is **independent** of
/// the kernel cascade and is left as-is — folding it into the
/// kernel type would force VBMI2/Avx2/Bmi2 to commit to PEXT-only
/// codegen, which is not always the fastest choice on the FSE
/// state-update extracts.)
///
/// The BMI2/AVX2/VBMI2 arms route through `#[target_feature]`-wrapped
/// trampolines so LLVM can inline the kernel's `_bzhi_u64` / pext
/// instructions across the `K::mask_lower_bits` call boundary inside
/// the impl body — otherwise the per-call target_feature boundary
/// would keep a function-call trampoline at every BitReader op.
pub fn decode_and_execute_sequences<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    use crate::cpu_kernel::{CpuKernelTag, ScalarKernel, detect_cpu_kernel};
    #[cfg(target_arch = "aarch64")]
    use crate::cpu_kernel::{NeonKernel, SveKernel};

    match detect_cpu_kernel() {
        CpuKernelTag::Scalar => decode_and_execute_sequences_impl::<B, ScalarKernel>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
            rle_fallback_sequences,
        ),
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Bmi2 => {
            // SAFETY: `detect_cpu_kernel()` only returns Bmi2 when
            // `is_x86_feature_detected!("bmi2")` confirmed BMI2 is
            // available. The `#[target_feature(enable = "bmi2")]`
            // wrapper lets LLVM emit `bzhi` directly at every
            // `K::mask_lower_bits` call site inside the impl,
            // bypassing the per-call target_feature trampoline that
            // would otherwise survive.
            unsafe {
                decode_and_execute_sequences_bmi2::<B>(
                    section,
                    source,
                    fse,
                    buffer,
                    offset_hist,
                    literals_buffer,
                    rle_fallback_sequences,
                )
            }
        }
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Avx2 => {
            // SAFETY: detect confirmed BMI2 + AVX2.
            unsafe {
                decode_and_execute_sequences_avx2::<B>(
                    section,
                    source,
                    fse,
                    buffer,
                    offset_hist,
                    literals_buffer,
                    rle_fallback_sequences,
                )
            }
        }
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Vbmi2 => {
            // SAFETY: detect confirmed AVX-512 VBMI2 + AVX2 + BMI2
            // (see `select_x86_kernel` precedence rules).
            unsafe {
                decode_and_execute_sequences_vbmi2::<B>(
                    section,
                    source,
                    fse,
                    buffer,
                    offset_hist,
                    literals_buffer,
                    rle_fallback_sequences,
                )
            }
        }
        #[cfg(target_arch = "aarch64")]
        CpuKernelTag::Neon => decode_and_execute_sequences_impl::<B, NeonKernel>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
            rle_fallback_sequences,
        ),
        #[cfg(target_arch = "aarch64")]
        CpuKernelTag::Sve => decode_and_execute_sequences_impl::<B, SveKernel>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
            rle_fallback_sequences,
        ),
    }
}

/// `#[target_feature(enable = "bmi2")]` trampoline for the BMI2 arm
/// — wraps `decode_and_execute_sequences_impl::<B, Bmi2Kernel>` so
/// LLVM can inline `_bzhi_u64` at every `K::mask_lower_bits` call
/// across the target_feature boundary.
///
/// # Safety
/// Caller must ensure BMI2 is available on the runtime CPU; the
/// dispatcher above gates on `detect_cpu_kernel() == Bmi2`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
unsafe fn decode_and_execute_sequences_bmi2<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
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

/// `#[target_feature(enable = "bmi2,avx2")]` trampoline for the
/// Avx2 arm. Same shape as the BMI2 trampoline; the AVX2 enable
/// piles onto the BMI2 feature set so any AVX2-gated codegen
/// (chunked SIMD copy via `_mm256_*`) also benefits.
///
/// # Safety
/// Caller must ensure BMI2 + AVX2 are available; gated by
/// `detect_cpu_kernel() == Avx2`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2")]
unsafe fn decode_and_execute_sequences_avx2<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, crate::cpu_kernel::Avx2Kernel>(
        section,
        source,
        fse,
        buffer,
        offset_hist,
        literals_buffer,
        rle_fallback_sequences,
    )
}

/// `#[target_feature(enable = "...AVX-512 VBMI2 family + BMI2 + AVX2")]`
/// trampoline for the Vbmi2 arm. Enables the full feature set the
/// `select_x86_kernel` precedence requires.
///
/// # Safety
/// Caller must ensure the entire feature set is available; gated by
/// `detect_cpu_kernel() == Vbmi2`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw")]
unsafe fn decode_and_execute_sequences_vbmi2<B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    rle_fallback_sequences: &mut Vec<Sequence>,
) -> Result<(), DecompressBlockError> {
    decode_and_execute_sequences_impl::<B, crate::cpu_kernel::Vbmi2Kernel>(
        section,
        source,
        fse,
        buffer,
        offset_hist,
        literals_buffer,
        rle_fallback_sequences,
    )
}

fn decode_and_execute_sequences_impl<
    B: super::buffer_backend::BufferBackend,
    K: crate::cpu_kernel::CpuKernel,
>(
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
        // `execute_sequences_fields` routes literal-pushes through
        // `DecodeBuffer::try_push` (and match-repeats through
        // `BufferBackend::try_reserve` inside `repeat_inner`), so a
        // malformed RLE-driven sequence stream whose literal or match
        // length overshoots a fixed-capacity backend (UserSliceBackend)
        // surfaces as `ExecuteSequencesError::OutputBufferOverflow`
        // rather than panicking via UserSliceBackend::extend's
        // release-mode `assert!`.
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
    // ADVANCE / ADVANCE_MASK hoisted to module scope so the extracted
    // `run_pipelined_sequence_loop` can reach them.

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
        // post-Err reuse. The pipelined work runs in a separate
        // top-level fn so a single rollback site catches all mid-loop
        // Errs uniformly AND a future `#[target_feature]` wrapper can
        // be added without dragging the outer fn into target_feature
        // scope.
        let pipeline_result = run_pipelined_sequence_loop(
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
        );
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
    // claimed get pushed after the last sequence. Routed through
    // `try_push` so a malformed block whose tail-literal length
    // overshoots the fixed-capacity backend (UserSliceBackend) surfaces
    // as `OutputBufferOverflow` instead of panicking via the per-call
    // `assert!` inside `BufferBackend::extend`. Growable backends
    // (FlatBuf, RingBuffer) accept the write infallibly.
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

/// Pipelined sequence-decode + execute loop (long-block hot path).
/// Extracted from `decode_and_execute_sequences` so it can be wrapped
/// with `#[target_feature]` in a follow-up commit — that wrapper is
/// what lets `peek_bits_triple`'s `extract_triple_pext` call inline
/// through the now-target_feature-scoped caller, eliminating the
/// `(u64,u64,u64)` sret ABI boundary that perf annotate attributed
/// ~19.96% of its own samples to (and ~3.95% of total decode time).
///
/// Caller (`decode_and_execute_sequences`) owns the rollback on Err:
/// on Err, the buffer-checkpoint restore and `*offset_hist = saved`
/// fire at the call site, NOT inside this fn. This fn only commits
/// `*offset_hist = shadow_hist` on the success-tail (after the drain
/// loop), matching the legacy IIFE contract.
///
/// 13 parameters: the closure capture set the IIFE used implicitly.
/// Grouping into a struct would push pressure off the argument
/// registers and onto memory loads, undoing the extraction's win.
#[allow(clippy::too_many_arguments)]
fn run_pipelined_sequence_loop<
    B: super::buffer_backend::BufferBackend,
    K: crate::cpu_kernel::CpuKernel,
>(
    br: &mut BitReaderReversed<'_, K>,
    ll_dec: &mut FSEDecoder<'_>,
    ml_dec: &mut FSEDecoder<'_>,
    of_dec: &mut FSEDecoder<'_>,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    lit_cur: &mut usize,
    literals_buffer_len: usize,
    num_sequences: usize,
    old_buffer_size: usize,
    max_update_bits: u8,
    seq_sum: &mut u32,
) -> Result<(), DecompressBlockError> {
    // `prefetch_pos` is the logical buffer index (same frame as
    // `buffer.len()`) at which the NEXT not-yet-decoded sequence
    // will start pushing literals. We pre-decode `ADVANCE` ahead,
    // accumulating (ll + ml) per decoded seq to keep this position
    // synchronised with where execute will eventually be.
    let mut prefetch_pos: usize = old_buffer_size;
    let mut shadow_hist: [u32; 3] = *offset_hist;
    let mut ring: [(Sequence, u32); ADVANCE] = [(
        Sequence {
            ll: 0,
            ml: 0,
            of: 0,
        },
        0u32,
    ); ADVANCE];

    // Pre-fill the ring. Outer `num_sequences >= ADVANCE * 2` gate
    // guarantees `num_sequences > ADVANCE`, so the FSE state update
    // is needed after every prefill decode.
    for slot in ring.iter_mut() {
        let seq = decode_one_sequence_inline(ll_dec, ml_dec, of_dec, br);
        let actual_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
        // wrapping_add: bitstream-derived values can overflow on
        // malformed frames; `prefetch_lookahead_match_source` bound-
        // checks against `buffer.len()` and drops wrap-derived
        // indices, so the wrap is harmless while keeping the decoder
        // fuzz-stable.
        let match_start = prefetch_pos.wrapping_add(seq.ll as usize);
        let source_idx = match_start.wrapping_sub(actual_offset as usize);
        buffer.prefetch_lookahead_match_source(source_idx);
        prefetch_pos = match_start.wrapping_add(seq.ml as usize);
        *slot = (seq, actual_offset);
        br.ensure_bits(max_update_bits);
        ll_dec.update_state_fast(br);
        ml_dec.update_state_fast(br);
        of_dec.update_state_fast(br);
    }

    // Steady state: decode next, prefetch its source, execute the
    // oldest slot in the ring (with its pre-resolved offset).
    for i in ADVANCE..num_sequences {
        let seq = decode_one_sequence_inline(ll_dec, ml_dec, of_dec, br);
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
            lit_cur,
            literals_buffer_len,
            exec_seq,
            exec_offset,
        )?;
        *seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);

        if i + 1 < num_sequences {
            br.ensure_bits(max_update_bits);
            ll_dec.update_state_fast(br);
            ml_dec.update_state_fast(br);
            of_dec.update_state_fast(br);
        }
    }

    // Drain: execute remaining ADVANCE sequences with their
    // pre-resolved offsets. Iteration order matches the ring slot
    // they occupy from the steady-state loop's final write.
    for k in 0..ADVANCE {
        let slot = (num_sequences + k) & ADVANCE_MASK;
        let (exec_seq, exec_offset) = ring[slot];
        execute_one_sequence_pipelined(
            buffer,
            literals_buffer,
            lit_cur,
            literals_buffer_len,
            exec_seq,
            exec_offset,
        )?;
        *seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);
    }

    // Single committing point for real offset history on the
    // pipelined success path. Shadow walked every queued sequence;
    // copy that state back so the next block sees the post-block
    // repcodes. Caller rolls back on Err.
    *offset_hist = shadow_hist;
    Ok(())
}

/// Per-sequence executor for the non-pipelined (short-block) fallback
/// path. Mutates the real `offset_hist` in lockstep with the sequence
/// being executed — preserving the legacy "Err leaves partial output +
/// pre-mutation hist" rollback contract.
#[inline(always)]
fn execute_one_sequence<B: super::buffer_backend::BufferBackend>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    offset_hist: &mut [u32; 3],
    seq: Sequence,
) -> Result<(), DecompressBlockError> {
    // `checked_add` on the literal cursor + sequence ll. On 32-bit
    // targets a malformed stream can push `*lit_cur + seq.ll as usize`
    // past `usize::MAX`; without the checked path the wrap would
    // produce `high < *lit_cur` and the subsequent `get_unchecked`
    // would slice into arbitrary memory (UB).
    let high = (*lit_cur)
        .checked_add(seq.ll as usize)
        .filter(|&h| h <= lit_len)
        .ok_or(ExecuteSequencesError::NotEnoughBytesForSequence {
            wanted: (*lit_cur).saturating_add(seq.ll as usize),
            have: lit_len,
        })?;
    // SAFETY: high <= lit_len (verified by the `filter` above) and
    // *lit_cur <= high (the `checked_add` succeeded, so no wrap).
    let lits = unsafe { literals.get_unchecked(*lit_cur..high) };
    *lit_cur = high;
    buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;

    let actual = do_offset_history(seq.of, seq.ll, offset_hist);
    if actual == 0 {
        return Err(ExecuteSequencesError::ZeroOffset.into());
    }
    buffer
        .repeat(actual as usize, seq.ml as usize)
        .map_err(ExecuteSequencesError::from)?;
    Ok(())
}

/// Pipelined-path executor variant: takes the offset already resolved
/// by the decode-ahead `shadow_hist` walk, so `do_offset_history` is
/// NOT called here (caller mutated only the shadow). Routes the match
/// copy through `repeat_lookahead_prefetched`, which skips only the
/// in-loop `prefetch_match_source` (redundant because the lookahead
/// pipeline already issued a PREFETCH_L1 ADVANCE iterations earlier).
/// The per-call `buffer.reserve(match_length)` is preserved by that
/// variant — required for memory safety against malformed inputs whose
/// `match_length` exceeds the upfront `reserve(MAX_BLOCK_SIZE)`
/// headroom.
#[inline(always)]
fn execute_one_sequence_pipelined<B: super::buffer_backend::BufferBackend>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    seq: Sequence,
    resolved_offset: u32,
) -> Result<(), DecompressBlockError> {
    let lit_cur_before = *lit_cur;
    // `checked_add` guards against `usize` wrap on 32-bit targets
    // when a malformed stream pushes `lit_cur_before + seq.ll` past
    // `usize::MAX`; without it the wrap produces `high < lit_cur_before`
    // and the subsequent `get_unchecked` would slice OOB (UB).
    let high = lit_cur_before
        .checked_add(seq.ll as usize)
        .filter(|&h| h <= lit_len)
        .ok_or(ExecuteSequencesError::NotEnoughBytesForSequence {
            wanted: lit_cur_before.saturating_add(seq.ll as usize),
            have: lit_len,
        })?;
    // SAFETY: high <= lit_len (verified above) and lit_cur_before <= high
    // (the `checked_add` succeeded, so no wrap).
    let lits = unsafe { literals.get_unchecked(lit_cur_before..high) };
    *lit_cur = high;

    if resolved_offset == 0 {
        return Err(ExecuteSequencesError::ZeroOffset.into());
    }

    // Donor-shape inline dispatch — when the backend opts in
    // (`UserSliceBackend` on x86_64 today, per its
    // `SUPPORTS_INLINE_SEQUENCE_EXEC = true` const) we collapse the
    // literal copy + match copy into a single straight-line body
    // that mirrors donor `ZSTD_execSequence`
    // (zstd_decompress_block.c:1008-1105). The const branch is
    // compile-time per backend monomorphisation, so the dead arm
    // carries no runtime cost on either side.
    //
    // **Literal-source slack guard** (the read-side donor-port
    // safety contract): donor's `ZSTD_copy16` reads 16 bytes
    // unconditionally regardless of `litLength`; on truncated
    // literals (the closing sequences of a block) that would read
    // past the end of the literals buffer slice — UB even when the
    // bytes happen to be valid memory inside the backing `Vec`.
    // Donor guards with `iLitEnd > litLimit` → slow path. We mirror
    // the same gate. The donor inline path issues two distinct reads
    // past the declared literal end:
    //   (1) Unconditional first `ZSTD_copy16` from `lit_cur_before`
    //       — needs `lit_cur_before + 16 <= lit_len`. THIS GATE
    //       MATTERS EVEN WHEN `seq.ll == 0`: the copy still happens,
    //       overwriting the dst region the match copy will rewrite.
    //   (2) Tail wildcopy's final 16-byte chunk — ONLY when
    //       `lit_length > 16` (the donor inline path gates the
    //       wildcopy call on that same threshold). Reads up to
    //       `lit_cur_before + lit_length + 15`, i.e. `high + 15`.
    // For `lit_length ∈ 0..=16` only (1) fires; gate (2) would
    // unnecessarily reject short-literal-tail sequences near
    // `lit_len` whose `copy16` over-read fits inside the buffer
    // (`lit_cur_before + 16 <= lit_len`) but whose `high + 15`
    // exceeds it. Apply (2) only in the wildcopy regime.
    // `checked_add` covers adversarial overflow.
    // For seq.ll > 16 the wildcopy tail's final 16-byte iteration
    // reads through `lit_cur_before + seq.ll.next_multiple_of(16)
    // - 1`. Use that exact bound rather than `high + 15`, which
    // over-counts by `15 - ((seq.ll - 1) % 16)` whenever `seq.ll %
    // 16 != 1` — keeping the donor inline path active on more
    // sequences near the end of the literals buffer.
    let inline_path_safe = B::SUPPORTS_INLINE_SEQUENCE_EXEC
        && lit_cur_before.checked_add(16).is_some_and(|b| b <= lit_len)
        && (seq.ll as usize <= 16
            || lit_cur_before
                .checked_add((seq.ll as usize).next_multiple_of(16))
                .is_some_and(|b| b <= lit_len));
    if inline_path_safe {
        // Validate match-copy offset against the live region
        // (matches `repeat()`'s `offset > buffer.len()` → dict path
        // gate). Donor inline path stays on the prefix-resident
        // case; offsets that step into dict / extDict territory fall
        // back to the layered path below.
        let buf_len = buffer.len();
        let offset = resolved_offset as usize;
        // `checked_add` against adversarial input: if `buf_len +
        // lits.len()` would wrap `usize`, treat the offset as
        // out-of-range and fall back to the layered path. Without
        // the check, wrapping addition could classify a wildly
        // out-of-range `offset` as in-range and feed the donor
        // inline path an OOB match-source pointer.
        let prefix_end = buf_len.checked_add(lits.len()).filter(|end| offset <= *end);
        if prefix_end.is_none() {
            // Match source reaches outside what's been written in this
            // frame — donor's `extDict` arm. Punt back to the slow
            // `repeat()` path; that path already routes through
            // `repeat_from_dict` for these offsets.
            buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;
            buffer
                .repeat_lookahead_prefetched(offset, seq.ml as usize)
                .map_err(ExecuteSequencesError::from)?;
            return Ok(());
        }
        // SAFETY:
        // - Backend opted in (compile-time const).
        // - `lits` is a non-aliased slice of the literals block.
        // - Source-side slack: `lit_cur_before + 16 <= lit_len`
        //   (gated above), so `lits.as_ptr().add(16)` reads stay
        //   inside the literals buffer. Donor unconditional
        //   `ZSTD_copy16` over-read of up to 16 bytes past
        //   `lits.len()` is bounded by the slack we just asserted.
        // - Offset is within the live region (prefix-resident,
        //   asserted above), so the match-copy source pointer
        //   `base + tail + lit_length - offset` is in-bounds.
        // - Match length is `>= 1` by zstd spec invariant (a
        //   sequence with `matchLength = 0` is malformed; the FSE
        //   decode produces baseline values starting at 3 for ml
        //   codes 0..3, so `seq.ml >= 3` for any valid sequence).
        //   The wildcopy helpers assert this in debug builds.
        // - Caller's upfront `reserve(MAX_BLOCK_SIZE)` plus the
        //   `WILDCOPY_OVERLENGTH = 32` slack on the user slice
        //   guarantees the writable tail has room for
        //   `lit_length + match_length + 15` (max wildcopy
        //   overshoot is 15 bytes past the declared end).
        // SAFETY: `literals.as_ptr().add(lit_cur_before)` has the
        // provenance of the FULL `literals` slice (not `lits`, the
        // sub-slice). The 16-byte unconditional `copy16` inside the
        // donor body reads up to `lit_cur_before + 16` bytes from
        // the parent buffer, which the `inline_path_safe` gate above
        // bounded by `lit_cur_before + 16 <= lit_len`. Passing
        // `lits.as_ptr()` directly would be UB when `lits.len() <
        // 16` because the sub-slice's provenance ends at its own
        // `len()` regardless of the backing buffer's extra capacity.
        let lit_src = unsafe { literals.as_ptr().add(lit_cur_before) };
        unsafe {
            buffer
                .buffer_mut()
                .exec_sequence_inline(lit_src, seq.ll as usize, offset, seq.ml as usize)
                .map_err(DecompressBlockError::ExecuteSequencesError)?;
        }
        // No `advance_output_counter` here: the donor inline path
        // advances `UserSliceBackend::tail` directly inside
        // `exec_sequence_inline`, and the post-block FCS check in
        // `run_direct_decode` now reads `tail` (via
        // `buffer_ref().tail() as u64`) instead of the separately
        // maintained `DecodeBuffer::total_output_counter`. Skipping
        // the per-sequence RMW drops the ~9% of decode time
        // measured at `addq <ll+ml>, 0x40(%r9)` on z000033
        // (perf annotate on
        // `decode_and_execute_sequences_avx2`). The legacy
        // push+repeat fallback below still goes through
        // `DecodeBuffer::try_push` / `repeat_lookahead_prefetched`,
        // which keep `total_output_counter` in sync for backends
        // that need it.
        return Ok(());
    }

    // Fallback: the legacy push + repeat chain.
    buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;
    buffer
        .repeat_lookahead_prefetched(resolved_offset as usize, seq.ml as usize)
        .map_err(ExecuteSequencesError::from)?;
    Ok(())
}

/// Per-sequence decode helper used by `decode_and_execute_sequences`.
/// Identical to the inner `decode_one_sequence` of
/// `decode_sequences_without_rle` — separate copy because Rust does not
/// let us share a private fn-item across two outer functions cleanly.
#[inline(always)]
fn decode_one_sequence_inline<K: crate::cpu_kernel::CpuKernel>(
    ll_dec: &mut FSEDecoder<'_>,
    ml_dec: &mut FSEDecoder<'_>,
    of_dec: &mut FSEDecoder<'_>,
    br: &mut BitReaderReversed<'_, K>,
) -> Sequence {
    // Read base/extra-bits directly off the active FSE state's
    // `Entry`. LL / ML are populated by `enrich_with_packed_seq_meta`
    // from the packed `LL_META` / `ML_META` tables during build;
    // OF is enriched closed-form (`1 << code`) by `enrich_for_offsets`.
    // Reading `state` directly drops the previous `lookup_ll_code` /
    // `lookup_ml_code` indirections (those did a second cache touch
    // on the separate meta tables per sequence) — the active entry
    // is already cache-hot.
    //
    // OF intentionally keeps the closed-form `1u32 << of_code`
    // baseline computation instead of reading `of_dec.state.base_value`:
    // the shift is a single ALU op vs a 4-byte memory load, both
    // produce identical codegen with `obits + ...` add, and `of_code`
    // is still required for `get_bits_triple` and the
    // `debug_assert!(of_code <= MAX_OFFSET_CODE)` precondition. The
    // 12-byte Entry layout still pays off here through LL / ML's
    // base_value / num_additional_bits reads.
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

fn decode_sequences_with_rle<K: crate::cpu_kernel::CpuKernel>(
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
            // Default LL distribution → cached table memcpy.
            #[cfg(feature = "std")]
            {
                scratch.literal_lengths.clone_from(predefined_ll_table()?);
            }
            #[cfg(not(feature = "std"))]
            {
                scratch.literal_lengths.build_from_probabilities(
                    LL_DEFAULT_ACC_LOG,
                    &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
                )?;
                scratch
                    .literal_lengths
                    .enrich_with_packed_seq_meta(&LL_META);
            }
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
            // Default OF distribution → cached table + cached long-share.
            #[cfg(feature = "std")]
            {
                let (cached, long_share) = predefined_of_table()?;
                scratch.offsets.clone_from(cached);
                scratch.offsets_long_share = long_share;
            }
            #[cfg(not(feature = "std"))]
            {
                scratch.offsets.build_from_probabilities(
                    OF_DEFAULT_ACC_LOG,
                    &Vec::from(&OFFSET_DEFAULT_DISTRIBUTION[..]),
                )?;
                scratch.offsets.enrich_for_offsets();
                scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
            }
            scratch.of_rle = None;
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
            // Default ML distribution → cached table memcpy.
            #[cfg(feature = "std")]
            {
                scratch.match_lengths.clone_from(predefined_ml_table()?);
            }
            #[cfg(not(feature = "std"))]
            {
                scratch.match_lengths.build_from_probabilities(
                    ML_DEFAULT_ACC_LOG,
                    &Vec::from(&MATCH_LENGTH_DEFAULT_DISTRIBUTION[..]),
                )?;
                scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
            }
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

// =====================================================================
//                   Predefined FSE table cache
// =====================================================================
//
// ModeType::Predefined fires whenever the encoder declares that an
// LL / OF / ML symbol stream follows the RFC 8478 default
// distribution (§3.1.1.3.2.1.1). On small-block fixtures this can
// dominate the decode budget: building the table costs O(table_size)
// per axis plus several `Vec::resize` round-trips, while the symbol
// stream itself is only a few hundred bytes.
//
// Flamegraph on `small-4k-log-lines/c_stream/pure_rust` (i9, post
// PR #263 merge) showed 66.72% of decode time in
// `FSETable::build_decoding_table`, all of it inside the Predefined
// branches.
//
// The default distributions are static — the tables they produce
// are byte-identical across calls. Pre-build once via OnceLock,
// then `clone_from` the cached table into the per-frame scratch.
// `Vec::clone_from` reuses the existing scratch allocation when the
// capacity already fits (it does, the scratch is re-used across
// frames), so each call is a fixed-size memcpy plus a few scalar
// field copies.
//
// Std-only because `OnceLock` lives in `std::sync` — there is no
// `core::sync::OnceLock` (the only stable OnceLock-style API
// requires std). `no_std` builds fall back to the per-call rebuild
// path via the `#[cfg(feature = "std")]` gate. The
// `critical-section` Cargo feature already flagged in the manifest
// is the planned route to extend the cache to no-atomic targets
// without pulling in `once_cell`.
//
// Errors from `build_from_probabilities` are propagated through a
// manual `get` / `set` race pattern (`OnceLock::get_or_try_init` is
// still unstable on the current MSRV). If two threads attempt the
// first init concurrently both build the table, but only one wins
// the `set` — the other observes the winner via the trailing
// `get()`. The cost of the redundant build is paid at most once
// per process and only under contention, so the race-write is
// preferable to the unstable API or to a `Mutex` round-trip on
// every call.
#[cfg(feature = "std")]
fn predefined_ll_table()
-> Result<&'static crate::fse::FSETable, crate::decoding::errors::FSETableError> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<crate::fse::FSETable> = OnceLock::new();
    if let Some(t) = CACHED.get() {
        return Ok(t);
    }
    let mut t = crate::fse::FSETable::new(MAX_LITERAL_LENGTH_CODE);
    t.build_from_probabilities(
        LL_DEFAULT_ACC_LOG,
        &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
    )?;
    t.enrich_with_packed_seq_meta(&LL_META);
    let _ = CACHED.set(t);
    Ok(CACHED.get().expect("just set"))
}

#[cfg(feature = "std")]
fn predefined_ml_table()
-> Result<&'static crate::fse::FSETable, crate::decoding::errors::FSETableError> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<crate::fse::FSETable> = OnceLock::new();
    if let Some(t) = CACHED.get() {
        return Ok(t);
    }
    let mut t = crate::fse::FSETable::new(MAX_MATCH_LENGTH_CODE);
    t.build_from_probabilities(
        ML_DEFAULT_ACC_LOG,
        &Vec::from(&MATCH_LENGTH_DEFAULT_DISTRIBUTION[..]),
    )?;
    t.enrich_with_packed_seq_meta(&ML_META);
    let _ = CACHED.set(t);
    Ok(CACHED.get().expect("just set"))
}

#[cfg(feature = "std")]
fn predefined_of_table()
-> Result<(&'static crate::fse::FSETable, u32), crate::decoding::errors::FSETableError> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<(crate::fse::FSETable, u32)> = OnceLock::new();
    if let Some(cache) = CACHED.get() {
        return Ok((&cache.0, cache.1));
    }
    let mut t = crate::fse::FSETable::new(MAX_OFFSET_CODE);
    t.build_from_probabilities(
        OF_DEFAULT_ACC_LOG,
        &Vec::from(&OFFSET_DEFAULT_DISTRIBUTION[..]),
    )?;
    t.enrich_for_offsets();
    let share = compute_offsets_long_share(&t);
    let _ = CACHED.set((t, share));
    let cache = CACHED.get().expect("just set");
    Ok((&cache.0, cache.1))
}

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

/// Regression gate for the predefined FSE table cache: every cached
/// table must be byte-identical to the table the rebuild path would
/// produce on the next call. If the cache ever drifts from the
/// rebuild output (different `decode` entries, different
/// `accuracy_log`, different `offsets_long_share` for OF) the
/// dispatch in `maybe_update_fse_tables` would silently decode
/// against a stale table — the bench delta would still look fine
/// but cross-validation against the donor would diverge on the
/// next ratio gate.
#[cfg(feature = "std")]
#[test]
fn predefined_fse_caches_match_rebuild_output() {
    use crate::fse::FSETable;

    let mut ll_rebuild = FSETable::new(MAX_LITERAL_LENGTH_CODE);
    ll_rebuild
        .build_from_probabilities(
            LL_DEFAULT_ACC_LOG,
            &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    ll_rebuild.enrich_with_packed_seq_meta(&LL_META);
    let ll_cached = predefined_ll_table().unwrap();
    assert_eq!(ll_rebuild.accuracy_log, ll_cached.accuracy_log);
    assert_eq!(ll_rebuild.decode.len(), ll_cached.decode.len());
    for (i, (a, b)) in ll_rebuild
        .decode
        .iter()
        .zip(ll_cached.decode.iter())
        .enumerate()
    {
        assert_eq!(a.symbol, b.symbol, "LL entry {i} symbol mismatch");
        assert_eq!(a.num_bits, b.num_bits, "LL entry {i} num_bits mismatch");
        assert_eq!(a.new_state, b.new_state, "LL entry {i} new_state mismatch");
        assert_eq!(
            a.base_value, b.base_value,
            "LL entry {i} base_value mismatch"
        );
        assert_eq!(
            a.num_additional_bits, b.num_additional_bits,
            "LL entry {i} num_additional_bits mismatch"
        );
    }

    let mut ml_rebuild = FSETable::new(MAX_MATCH_LENGTH_CODE);
    ml_rebuild
        .build_from_probabilities(
            ML_DEFAULT_ACC_LOG,
            &Vec::from(&MATCH_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    ml_rebuild.enrich_with_packed_seq_meta(&ML_META);
    let ml_cached = predefined_ml_table().unwrap();
    assert_eq!(ml_rebuild.accuracy_log, ml_cached.accuracy_log);
    assert_eq!(ml_rebuild.decode.len(), ml_cached.decode.len());
    for (i, (a, b)) in ml_rebuild
        .decode
        .iter()
        .zip(ml_cached.decode.iter())
        .enumerate()
    {
        assert_eq!(a.symbol, b.symbol, "ML entry {i} symbol mismatch");
        assert_eq!(a.num_bits, b.num_bits, "ML entry {i} num_bits mismatch");
        assert_eq!(a.new_state, b.new_state, "ML entry {i} new_state mismatch");
        assert_eq!(
            a.base_value, b.base_value,
            "ML entry {i} base_value mismatch"
        );
        assert_eq!(
            a.num_additional_bits, b.num_additional_bits,
            "ML entry {i} num_additional_bits mismatch"
        );
    }

    let mut of_rebuild = FSETable::new(MAX_OFFSET_CODE);
    of_rebuild
        .build_from_probabilities(
            OF_DEFAULT_ACC_LOG,
            &Vec::from(&OFFSET_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    of_rebuild.enrich_for_offsets();
    let of_rebuild_share = compute_offsets_long_share(&of_rebuild);
    let (of_cached, of_cached_share) = predefined_of_table().unwrap();
    assert_eq!(of_rebuild.accuracy_log, of_cached.accuracy_log);
    assert_eq!(of_rebuild.decode.len(), of_cached.decode.len());
    assert_eq!(
        of_rebuild_share, of_cached_share,
        "OF offsets_long_share mismatch"
    );
    for (i, (a, b)) in of_rebuild
        .decode
        .iter()
        .zip(of_cached.decode.iter())
        .enumerate()
    {
        assert_eq!(a.symbol, b.symbol, "OF entry {i} symbol mismatch");
        assert_eq!(a.num_bits, b.num_bits, "OF entry {i} num_bits mismatch");
        assert_eq!(a.new_state, b.new_state, "OF entry {i} new_state mismatch");
        assert_eq!(
            a.base_value, b.base_value,
            "OF entry {i} base_value mismatch"
        );
        assert_eq!(
            a.num_additional_bits, b.num_additional_bits,
            "OF entry {i} num_additional_bits mismatch"
        );
    }
}

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
