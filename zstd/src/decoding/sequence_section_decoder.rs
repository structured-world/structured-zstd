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
use crate::decoding::sequence_execution::do_offset_history;
use crate::fse::SeqFSEDecoder;
#[cfg(test)]
use alloc::vec::Vec;

// 8-slot software pipeline mirroring donor
// `ZSTD_decompressSequencesLong_body`'s `STORED_SEQS = 8`. The
// 8-deep lookahead lets the prefetch issued at iteration `i`
// resolve through L1/L2 by the time iteration `i + 8` consumes it,
// whereas 4-deep often wasn't enough gap on long-distance workloads.
pub(crate) const ADVANCE: usize = 8;
pub(crate) const ADVANCE_MASK: usize = ADVANCE - 1;
// `i & ADVANCE_MASK` only equals `i % ADVANCE` when ADVANCE is a
// power of two. Compile-time guard so a future ADVANCE tweak can't
// silently corrupt the ring index.
const _: () = assert!(
    ADVANCE.is_power_of_two(),
    "ADVANCE must be a power of two; ring indexing uses `i & (ADVANCE - 1)` as `i % ADVANCE`"
);

/// Donor `ZSTD_decompressBlock_internal` long-pipeline gate. Engages
/// the 8-deep lookahead-ring decoder when (a) the block has enough
/// sequences to amortise prefill+drain (`num_sequences >= ADVANCE * 2`)
/// AND (b) either the dict is cold (first block after attach) OR total
/// history exceeds 16 MB AND the FSE offset distribution carries
/// enough long-distance codes to make prefetch worthwhile.
///
/// `MIN_LONG_OFFSET_SHARE`: donor `minShare = MEM_64bits() ? 7 : 20` —
/// the 32-bit threshold is higher because the prefetch pipeline needs
/// a stronger long-offset signal to outpace the narrower load window
/// on those targets. `HISTORY_THRESHOLD_FOR_PREFETCH = 1 << 24` (16 MB):
/// below that the history fits in L2/L3 and the hardware prefetcher
/// handles short/medium offsets; engaging the ring is pure overhead.
///
/// Single source of truth for both the K-generic dispatcher and the
/// per-tier x86 monoliths so the two paths can't diverge.
#[inline]
pub(crate) fn compute_use_long_pipeline(
    num_sequences: usize,
    ddict_is_cold: bool,
    total_history: usize,
    offsets_long_share: u32,
) -> bool {
    #[cfg(target_pointer_width = "64")]
    const MIN_LONG_OFFSET_SHARE: u32 = 7;
    #[cfg(not(target_pointer_width = "64"))]
    const MIN_LONG_OFFSET_SHARE: u32 = 20;
    const HISTORY_THRESHOLD_FOR_PREFETCH: usize = 1 << 24;
    num_sequences >= ADVANCE * 2
        && (ddict_is_cold
            || (total_history > HISTORY_THRESHOLD_FOR_PREFETCH
                && offsets_long_share >= MIN_LONG_OFFSET_SHARE))
}

/// Cold per-block sequence-stream setup, returned by [`init_sequence_stream`].
/// Carries the bit reader, the three FSE decoder states (with their
/// initial states already read), and the scalar gate values the hot
/// decode+execute loop needs. Only that loop diverges per CPU tier; this
/// preamble is identical across tiers and lives in one place.
pub(crate) struct SeqStreamSetup<'src, 'fse, K: crate::cpu_kernel::CpuKernel> {
    pub(crate) br: BitReaderReversed<'src, K>,
    pub(crate) ll_dec: SeqFSEDecoder<'fse>,
    pub(crate) ml_dec: SeqFSEDecoder<'fse>,
    pub(crate) of_dec: SeqFSEDecoder<'fse>,
    pub(crate) max_update_bits: u8,
    pub(crate) old_buffer_size: usize,
    pub(crate) num_sequences: usize,
    pub(crate) use_long_pipeline: bool,
}

/// Shared cold preamble for every CPU-tier sequence decoder (the
/// `K`-generic [`decode_and_execute_sequences_impl`] and the per-tier
/// x86 monoliths in `seq_decoder_{scalar,bmi2,avx2,vbmi2}`).
///
/// Consumes the one-shot `ddict_is_cold` flag, rebuilds the FSE tables
/// if the block's mode bytes call for it, skips the start-of-stream
/// padding, initialises the LL/OF/ML decoder states, reserves the
/// block's output capacity AND arms the per-block output ceiling (the
/// decompression-bomb guard that bounds growth at `len + MAX_BLOCK_SIZE`),
/// and computes the long-pipeline gate.
///
/// Centralising this is what keeps the ceiling (and every other
/// per-block invariant) from drifting between tiers — the per-tier
/// copies previously each had to remember to arm it.
pub(crate) fn init_sequence_stream<'src, 'fse, B, K>(
    section: &SequencesHeader,
    source: &'src [u8],
    fse: &'fse mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
) -> Result<SeqStreamSetup<'src, 'fse, K>, DecompressBlockError>
where
    B: super::buffer_backend::BufferBackend,
    K: crate::cpu_kernel::CpuKernel,
{
    // Consume the one-shot `ddict_is_cold` flag BEFORE any early return
    // (padding validation) so a later block's gate can't mis-apply a
    // cold-dict signal that no longer holds. Donor
    // `ZSTD_decompressBlock_internal` clears `dctx->ddictIsCold = 0`
    // unconditionally after the sequence-section dispatch decision.
    let ddict_is_cold = fse.ddict_is_cold;
    fse.ddict_is_cold = false;

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

    // RLE-mode axes are handled uniformly: `maybe_update_fse_tables`
    // builds a degenerate single-state table for them, so the fused
    // decode reads every axis the same way (no separate fallback).
    // Copy-on-write table source: `ll_table`/`ml_table`/`of_table`
    // resolve to the shared dictionary's table (zero-copy) on axes still
    // in `Dict` mode, else the locally-built table. `maybe_update_fse_tables`
    // above has already flipped any rebuilt axis to `Local`.
    let mut ll_dec = SeqFSEDecoder::new(fse.ll_table());
    let mut ml_dec = SeqFSEDecoder::new(fse.ml_table());
    let mut of_dec = SeqFSEDecoder::new(fse.of_table());

    ll_dec
        .init_state(&mut br)
        .map_err(DecodeSequenceError::from)?;
    of_dec
        .init_state(&mut br)
        .map_err(DecodeSequenceError::from)?;
    ml_dec
        .init_state(&mut br)
        .map_err(DecodeSequenceError::from)?;

    let max_update_bits =
        fse.ll_table().accuracy_log + fse.ml_table().accuracy_log + fse.of_table().accuracy_log;
    debug_assert!(
        max_update_bits <= 56,
        "sequence section update bits exceed 56-bit budget"
    );

    // Exact growth: this worst-case pre-block reservation is a no-op while
    // the frame-entry window reservation covers it, and on the frame's LAST
    // block (where the remaining content is smaller than a full block) the
    // amortized policy would DOUBLE the window-sized buffer for a tail
    // worth a fraction of a block. The ring backend keeps its own
    // amortized growth via the trait default.
    buffer.reserve_exact(MAX_BLOCK_SIZE as usize);
    // Arm the per-block output ceiling so a malformed / adversarial block
    // whose sequences over-produce cannot grow the buffer past
    // `len + MAX_BLOCK_SIZE` (a decompression-bomb OOM on the growable
    // RingBuffer); `DecodeBuffer::repeat` rejects the crossing match.
    buffer.set_block_output_ceiling(MAX_BLOCK_SIZE as usize);
    let old_buffer_size = buffer.len();
    let num_sequences = section.num_sequences as usize;

    // Overflow is only reachable on 32-bit `usize` (a 4 GiB-class
    // window_size plus a dict). The gate below asks "does history exceed
    // the prefetch threshold", so on the overflow path the clamped maximum
    // is the correct answer, not a wrapped small value.
    let total_history = match buffer.window_size.checked_add(buffer.dict_content().len()) {
        Some(sum) => sum,
        None => usize::MAX,
    };
    let use_long_pipeline = compute_use_long_pipeline(
        num_sequences,
        ddict_is_cold,
        total_history,
        fse.offsets_long_share,
    );

    Ok(SeqStreamSetup {
        br,
        ll_dec,
        ml_dec,
        of_dec,
        max_update_bits,
        old_buffer_size,
        num_sequences,
        use_long_pipeline,
    })
}

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
) -> Result<(), DecompressBlockError> {
    #[cfg(all(target_arch = "aarch64", feature = "kernel_neon"))]
    use crate::cpu_kernel::NeonKernel;
    #[cfg(all(
        target_arch = "aarch64",
        feature = "kernel_sve",
        any(feature = "std", target_feature = "sve"),
    ))]
    use crate::cpu_kernel::SveKernel;
    use crate::cpu_kernel::{CpuKernelTag, detect_cpu_kernel};

    match detect_cpu_kernel() {
        CpuKernelTag::Scalar => {
            super::seq_decoder_scalar::decode_and_execute_sequences_scalar::<B>(
                section,
                source,
                fse,
                buffer,
                offset_hist,
                literals_buffer,
            )
        }
        #[cfg(all(target_arch = "x86_64", feature = "kernel_sse2"))]
        CpuKernelTag::Sse2 => {
            // SSE2 has no FSE-relevant divergence (no `_bzhi_u64`); the
            // mask_lower_bits hot op is identical to Scalar. SSE2's only
            // distinct body is match-copy (gated per-backend via
            // SUPPORTS_INLINE_SEQUENCE_EXEC), not the sequence FSE walk,
            // so route to the portable scalar sequence decoder.
            super::seq_decoder_scalar::decode_and_execute_sequences_scalar::<B>(
                section,
                source,
                fse,
                buffer,
                offset_hist,
                literals_buffer,
            )
        }
        #[cfg(all(target_arch = "x86_64", feature = "kernel_bmi2"))]
        CpuKernelTag::Bmi2 => {
            // SAFETY: `detect_cpu_kernel()` only returns Bmi2 when
            // `is_x86_feature_detected!("bmi2")` confirmed BMI2 is
            // available. The per-tier trampoline lives in its own
            // module (`seq_decoder_bmi2`) so future BMI2-specific
            // divergence can be applied without touching the other
            // kernels — see #279 round 3.
            unsafe {
                super::seq_decoder_bmi2::decode_and_execute_sequences_bmi2::<B>(
                    section,
                    source,
                    fse,
                    buffer,
                    offset_hist,
                    literals_buffer,
                )
            }
        }
        #[cfg(all(target_arch = "x86_64", feature = "kernel_avx2"))]
        CpuKernelTag::Avx2 => {
            // SAFETY: detect confirmed BMI2 + AVX2.
            unsafe {
                super::seq_decoder_avx2::decode_and_execute_sequences_avx2::<B>(
                    section,
                    source,
                    fse,
                    buffer,
                    offset_hist,
                    literals_buffer,
                )
            }
        }
        #[cfg(all(target_arch = "x86_64", feature = "kernel_vbmi2"))]
        CpuKernelTag::Vbmi2 => {
            // SAFETY: detect confirmed AVX-512 VBMI2 + AVX2 + BMI2
            // (see `select_x86_kernel` precedence rules).
            unsafe {
                super::seq_decoder_vbmi2::decode_and_execute_sequences_vbmi2::<B>(
                    section,
                    source,
                    fse,
                    buffer,
                    offset_hist,
                    literals_buffer,
                )
            }
        }
        #[cfg(all(target_arch = "aarch64", feature = "kernel_neon"))]
        CpuKernelTag::Neon => decode_and_execute_sequences_impl::<B, NeonKernel>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
        ),
        #[cfg(all(
            target_arch = "aarch64",
            feature = "kernel_sve",
            any(feature = "std", target_feature = "sve"),
        ))]
        CpuKernelTag::Sve => decode_and_execute_sequences_impl::<B, SveKernel>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
        ),
    }
}

// Per-tier x86 trampolines (`decode_and_execute_sequences_{bmi2,avx2,vbmi2}`)
// live in `seq_decoder_bmi2.rs` / `seq_decoder_avx2.rs` /
// `seq_decoder_vbmi2.rs`. Each owns its `#[target_feature]` attribute
// and is called from the dispatch matcher above. See issue #279
// round 3 for the per-kernel architecture rationale.

// `dead_code` on x86_64 production: the per-kernel monoliths bypass
// this shared body. Live on aarch64 (Neon/Sve dispatch arms) and on
// every test build. Allowed because the function is conditionally
// reachable per build configuration.
#[allow(dead_code)]
pub(crate) fn decode_and_execute_sequences_impl<
    B: super::buffer_backend::BufferBackend,
    K: crate::cpu_kernel::CpuKernel,
>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
) -> Result<(), DecompressBlockError> {
    // Consume the one-shot `ddict_is_cold` flag at function entry,
    // BEFORE any early returns (padding-bit validation).
    // Donor `ZSTD_decompressBlock_internal` clears
    // `dctx->ddictIsCold = 0` unconditionally after the
    // sequence-section dispatch decision; if the early-return paths
    // left the flag set, a later block's gate would mis-apply the
    // cold-dict signal that no longer holds (FSE/HUF tables are now
    // warm regardless of how the previous block decoded its sequences,
    // including RLE-mode axes handled in-line via the fused table).
    let SeqStreamSetup {
        mut br,
        mut ll_dec,
        mut ml_dec,
        mut of_dec,
        max_update_bits,
        old_buffer_size,
        num_sequences,
        use_long_pipeline,
    } = init_sequence_stream::<B, K>(section, source, fse, buffer)?;
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

    // `offset_hist` mutation on the in-band success path: both
    // pipelined and short-block fallback resolve repcodes against a
    // local `shadow_hist` and commit `*offset_hist = shadow_hist`
    // ONLY after the last sequence executes successfully. Mid-loop
    // mutation of the real `offset_hist` would leak partial state on
    // an `Err` from `execute_one_sequence*` (literal bounds check,
    // inline-exec offset gate), and the `?`-shaped early returns in
    // the fallback path bypass the post-loop rollback below — the
    // shadow + commit-on-success shape mirrors the pipelined branch
    // exactly so an `Err` ANYWHERE in the loop leaves the caller's
    // offset_hist untouched. The post-loop `*offset_hist =
    // saved_offset_hist` rollback handler still fires if the
    // bitstream-tail validation fails, covering the edge case where
    // every sequence succeeds but the bitstream has leftover bits.

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

    // The format-level `isLongOffset` shortcut from donor is
    // irrelevant on our u32-indexed decoder, so on top of the
    // long-offset share the cold-dict signal is the only other gate.

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
        //
        // Routes through `execute_one_sequence_pipelined` (resolving
        // the actual offset against a `shadow_hist` upfront) so the
        // inline donor-shape writer fires on backends that opt in
        // (`UserSliceBackend::SUPPORTS_INLINE_SEQUENCE_EXEC = true`).
        // The legacy `execute_one_sequence` path went through
        // `DecodeBuffer::repeat_inner` which incremented
        // `total_output_counter += match_length` on every sequence —
        // perf annotate on z000033 L-3 fast attributed ~6% of decode
        // time to that RMW at offset `0x40(r8)` of the wrapper
        // struct. The inline
        // executor advances `tail` directly inside the backend, so
        // the wrapper-level counter is bypassed entirely on this
        // path; the post-block FCS check in `run_direct_decode`
        // reads `tail()` instead.
        //
        // `shadow_hist` mirrors the pipelined-branch pattern: the
        // real `offset_hist` is NOT mutated mid-loop, so an early
        // `Err` from `execute_one_sequence_pipelined` (literal bounds
        // check, inline-exec offset gate, etc.) propagating through
        // the explicit Err arm below leaves the caller's offset_hist
        // untouched. On the success path we commit `shadow_hist`
        // back to `*offset_hist` once, after the loop.
        let mut shadow_hist = *offset_hist;
        let mut fallback_err: Option<DecompressBlockError> = None;
        for i in 0..num_sequences {
            let seq = decode_one_sequence_inline(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
            let resolved_offset = do_offset_history(seq.of, seq.ll, &mut shadow_hist);
            if let Err(e) = execute_one_sequence_pipelined(
                buffer,
                literals_buffer,
                &mut lit_cur,
                literals_buffer_len,
                seq,
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
            // Mirrors the pipelined branch's Err handler: roll the
            // buffer back to the pre-loop checkpoint; offset_hist
            // was never mutated mid-loop (shadow only), so no
            // restore needed there. Buffer might have absorbed
            // literal pushes / partial inline writes from the
            // failing sequence — try_restore_checkpoint handles
            // both cases via the captured tail snapshot.
            //
            // offset_hist intentionally NOT touched here regardless
            // of the rollback outcome: it still holds the pre-loop
            // value because shadow_hist absorbed all the in-band
            // mutations. The bool return from `try_restore_checkpoint`
            // is therefore irrelevant on this path — `false` means
            // an intervening reallocation invalidated the captured
            // tail, in which case the frame is already corrupted and
            // the caller surfaces the original `Err` below. We drop
            // the return value via `let _` to make the
            // intentional-discard explicit.
            let _ = buffer.try_restore_checkpoint(buffer_checkpoint);
            return Err(e);
        }
        *offset_hist = shadow_hist;
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
#[allow(clippy::too_many_arguments, dead_code)]
fn run_pipelined_sequence_loop<
    B: super::buffer_backend::BufferBackend,
    K: crate::cpu_kernel::CpuKernel,
>(
    br: &mut BitReaderReversed<'_, K>,
    ll_dec: &mut SeqFSEDecoder<'_>,
    ml_dec: &mut SeqFSEDecoder<'_>,
    of_dec: &mut SeqFSEDecoder<'_>,
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
    // Donor `ZSTD_decompressSequencesLong_body` shape: 8-deep
    // lookahead ring with `prefetch_lookahead_match_source` per
    // decoded seq, executing the OLDEST resolved sequence per
    // iteration. Used ONLY when caller selected the long-pipeline
    // arm — cold-dict / long-offset frames where DRAM prefetch
    // matters. Common hot-cache path goes through the straight
    // single-pass fused loop in `decode_and_execute_sequences_impl`.
    let mut prefetch_pos: usize = old_buffer_size;
    let mut shadow_hist: [u32; 3] = *offset_hist;
    let mut ring: [ExecSeq; ADVANCE] = [ExecSeq {
        ll: 0,
        ml: 0,
        actual_offset: 0,
    }; ADVANCE];

    for slot in ring.iter_mut() {
        let seq = decode_one_sequence_inline(ll_dec, ml_dec, of_dec, br);
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

    #[cfg(target_arch = "x86_64")]
    // SAFETY: alignment-only asm, no memory or register clobbers.
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
    for i in ADVANCE..num_sequences {
        let seq = decode_one_sequence_inline(ll_dec, ml_dec, of_dec, br);
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

        execute_one_sequence_pipelined_resolved(
            buffer,
            literals_buffer,
            lit_cur,
            literals_buffer_len,
            exec_seq,
        )?;
        *seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);

        if i + 1 < num_sequences {
            br.ensure_bits(max_update_bits);
            ll_dec.update_state_fast(br);
            ml_dec.update_state_fast(br);
            of_dec.update_state_fast(br);
        }
    }

    for k in 0..ADVANCE {
        let slot = (num_sequences + k) & ADVANCE_MASK;
        let exec_seq = ring[slot];
        execute_one_sequence_pipelined_resolved(
            buffer,
            literals_buffer,
            lit_cur,
            literals_buffer_len,
            exec_seq,
        )?;
        *seq_sum = seq_sum.wrapping_add(exec_seq.ll).wrapping_add(exec_seq.ml);
    }

    *offset_hist = shadow_hist;
    Ok(())
}

/// Post-resolve sequence shape carried by the pipelined ring. Stores
/// only the fields the executor actually reads: literal length, match
/// length, and the resolved-via-offset-history match offset. The raw
/// `Sequence.of` (offset_code) is dead by the time a slot reaches the
/// executor — `do_offset_history` already turned it into
/// `actual_offset` — so omitting it from the ring shape saves 4 bytes
/// per slot (12 bytes per `ExecSeq` vs 16 for the previous
/// `(Sequence, u32)` tuple) and the matching ring write traffic.
#[derive(Copy, Clone)]
pub(crate) struct ExecSeq {
    pub(crate) ll: u32,
    pub(crate) ml: u32,
    pub(crate) actual_offset: u32,
}

/// Pipelined-path executor wrapper: unpacks an `ExecSeq` ring slot into
/// the `(Sequence, resolved_offset)` shape that
/// `execute_one_sequence_pipelined` expects. Lives next to `ExecSeq` so
/// the post-resolve contract (raw `Sequence.of` is dead; only
/// `actual_offset` reaches the executor) is visible at one site.
#[inline(always)]
#[allow(dead_code)] // live on aarch64 + tests only; see decode_and_execute_sequences_impl
pub(crate) fn execute_one_sequence_pipelined_resolved<B: super::buffer_backend::BufferBackend>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    exec_seq: ExecSeq,
) -> Result<(), DecompressBlockError> {
    execute_one_sequence_pipelined(
        buffer,
        literals,
        lit_cur,
        lit_len,
        Sequence {
            ll: exec_seq.ll,
            ml: exec_seq.ml,
            of: 0,
        },
        exec_seq.actual_offset,
    )
}

/// BMI2-tier trivial unsafe wrapper. Today delegates to the K-agnostic
/// `execute_one_sequence_pipelined`; future Phase 4 commits replace the
/// body with a BMI2-specific match-copy path (no AVX2 chunks; SSE2
/// 16-byte direct via `wildcopy_no_overlap`). Exists now so the macro
/// expansion can call `$exec_one_fn` uniformly via `unsafe { ... }`
/// across all tiers.
///
/// # Safety
/// Same as `execute_one_sequence_pipelined` (currently safe; wrapping
/// in `unsafe fn` for macro uniformity).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
#[inline]
#[allow(dead_code)] // vestigial pre-R12 macro-dispatch helper; per-kernel monoliths now go through seq_decoder_bmi2 directly
pub(crate) unsafe fn execute_one_sequence_pipelined_bmi2<
    B: super::buffer_backend::BufferBackend,
>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    seq: Sequence,
    resolved_offset: u32,
) -> Result<(), DecompressBlockError> {
    execute_one_sequence_pipelined(buffer, literals, lit_cur, lit_len, seq, resolved_offset)
}

/// BMI2-tier ExecSeq-unpack wrapper. Delegates to safe K-agnostic
/// resolver. See [`execute_one_sequence_pipelined_bmi2`] for the
/// Phase 4 plan.
///
/// # Safety
/// Caller must be in `#[target_feature(enable = "bmi2")]` scope.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
#[inline]
#[allow(dead_code)] // vestigial pre-R12 macro-dispatch helper
pub(crate) unsafe fn execute_one_sequence_pipelined_resolved_bmi2<
    B: super::buffer_backend::BufferBackend,
>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    exec_seq: ExecSeq,
) -> Result<(), DecompressBlockError> {
    execute_one_sequence_pipelined_resolved(buffer, literals, lit_cur, lit_len, exec_seq)
}

/// VBMI2-tier exec wrapper. Currently delegates via the AVX2 variant
/// (VBMI2 hardware always has AVX2 + BMI2, so the AVX2-tier match-copy
/// divergence applies). Future commits may add VBMI2-specific match
/// paths (e.g. AVX-512 64-byte zmm wildcopy) but those require
/// architectural buffer-slack changes (WILDCOPY_OVERLENGTH → 64).
///
/// # Safety
/// Caller must be in the full VBMI2 target_feature set; VBMI2 implies
/// AVX2+BMI2 per `select_x86_kernel` precedence.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw")]
#[inline]
#[allow(dead_code)] // vestigial pre-R12 macro-dispatch helper
pub(crate) unsafe fn execute_one_sequence_pipelined_vbmi2<
    B: super::buffer_backend::BufferBackend,
>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    seq: Sequence,
    resolved_offset: u32,
) -> Result<(), DecompressBlockError> {
    // SAFETY: VBMI2 implies AVX2+BMI2; the AVX2 variant's
    // target_feature scope is a subset of ours.
    unsafe {
        execute_one_sequence_pipelined_avx2(
            buffer,
            literals,
            lit_cur,
            lit_len,
            seq,
            resolved_offset,
        )
    }
}

/// VBMI2-tier ExecSeq-unpack wrapper. Delegates to AVX2 variant.
///
/// # Safety
/// Caller must be in VBMI2 target_feature scope.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw")]
#[inline]
#[allow(dead_code)] // vestigial pre-R12 macro-dispatch helper
pub(crate) unsafe fn execute_one_sequence_pipelined_resolved_vbmi2<
    B: super::buffer_backend::BufferBackend,
>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    exec_seq: ExecSeq,
) -> Result<(), DecompressBlockError> {
    // SAFETY: VBMI2 ⊇ AVX2+BMI2.
    unsafe {
        execute_one_sequence_pipelined_resolved_avx2(buffer, literals, lit_cur, lit_len, exec_seq)
    }
}

/// AVX2-tier ExecSeq-unpack wrapper. Same shape as
/// [`execute_one_sequence_pipelined_resolved`] but routes the call to
/// [`execute_one_sequence_pipelined_avx2`] (32-byte ymm match-copy
/// wildcopy via `BufferBackend::exec_sequence_inline_avx2`).
///
/// # Safety
/// Caller MUST be in `#[target_feature(enable = "avx2,bmi2")]` scope.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2")]
#[inline]
#[allow(dead_code)] // vestigial pre-R12 macro-dispatch helper
pub(crate) unsafe fn execute_one_sequence_pipelined_resolved_avx2<
    B: super::buffer_backend::BufferBackend,
>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    exec_seq: ExecSeq,
) -> Result<(), DecompressBlockError> {
    // SAFETY: same target_feature scope as the callee.
    unsafe {
        execute_one_sequence_pipelined_avx2(
            buffer,
            literals,
            lit_cur,
            lit_len,
            Sequence {
                ll: exec_seq.ll,
                ml: exec_seq.ml,
                of: 0,
            },
            exec_seq.actual_offset,
        )
    }
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
#[allow(dead_code)] // live on aarch64 + tests only; see decode_and_execute_sequences_impl
pub(crate) fn execute_one_sequence_pipelined<B: super::buffer_backend::BufferBackend>(
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
        && buffer.buffer_mut().inline_exec_ok(
            seq.ll as usize,
            seq.ml as usize,
            resolved_offset as usize,
        )
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
        // The inline path advances the backend's `tail` directly, bypassing the
        // wrapper-level `DecodeBuffer::total_output_counter`. Backends whose
        // cumulative-output accounting reads that counter (`RingBuffer` /
        // `FlatBuf` — the resume `output_offset` and the dict-reachability gate)
        // must keep it current, so bump it here; `UserSliceBackend` (direct
        // path, reads `tail()` and never the counter) sets the const to `false`
        // and this read-modify-write is const-folded away, preserving the ~9%
        // it costs on the all-inline direct hot path (`addq <ll+ml>, …` on
        // z000033).
        if B::INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER {
            buffer.advance_output_counter((seq.ll + seq.ml) as u64);
        }
        return Ok(());
    }

    // Fallback: the legacy push + repeat chain.
    buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;
    buffer
        .repeat_lookahead_prefetched(resolved_offset as usize, seq.ml as usize)
        .map_err(ExecuteSequencesError::from)?;
    Ok(())
}

/// AVX2-tier variant of [`execute_one_sequence_pipelined`]. Differs at
/// exactly one site: the match-copy inline path routes to
/// `BufferBackend::exec_sequence_inline_avx2` (32-byte ymm wildcopy on
/// the no-overlap match path) instead of the SSE2 16-byte default.
/// Issue #279 round 3 Phase 4.
///
/// # Safety
/// Caller MUST be in `#[target_feature(enable = "avx2,bmi2")]` scope
/// AND have verified the runtime CPU advertises both features (the
/// dispatcher in `decode_and_execute_sequences` gates this on
/// `detect_cpu_kernel() == Avx2`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2")]
#[inline]
#[allow(dead_code)] // vestigial pre-R12 macro-dispatch helper
pub(crate) unsafe fn execute_one_sequence_pipelined_avx2<
    B: super::buffer_backend::BufferBackend,
>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    literals: &[u8],
    lit_cur: &mut usize,
    lit_len: usize,
    seq: Sequence,
    resolved_offset: u32,
) -> Result<(), DecompressBlockError> {
    let lit_cur_before = *lit_cur;
    let high = lit_cur_before
        .checked_add(seq.ll as usize)
        .filter(|&h| h <= lit_len)
        .ok_or(ExecuteSequencesError::NotEnoughBytesForSequence {
            wanted: lit_cur_before.saturating_add(seq.ll as usize),
            have: lit_len,
        })?;
    // SAFETY: high <= lit_len, lit_cur_before <= high (checked above).
    let lits = unsafe { literals.get_unchecked(lit_cur_before..high) };
    *lit_cur = high;

    if resolved_offset == 0 {
        return Err(ExecuteSequencesError::ZeroOffset.into());
    }

    // Same gate as the SSE2 default — 16-byte literal slack bound
    // unchanged because the AVX2 override keeps the SSE2 16-byte
    // literal copy (the divergence is on match-copy only, see
    // `UserSliceBackend::exec_sequence_inline_avx2`).
    let inline_path_safe = B::SUPPORTS_INLINE_SEQUENCE_EXEC
        && buffer.buffer_mut().inline_exec_ok(
            seq.ll as usize,
            seq.ml as usize,
            resolved_offset as usize,
        )
        && lit_cur_before.checked_add(16).is_some_and(|b| b <= lit_len)
        && (seq.ll as usize <= 16
            || lit_cur_before
                .checked_add((seq.ll as usize).next_multiple_of(16))
                .is_some_and(|b| b <= lit_len));
    if inline_path_safe {
        let buf_len = buffer.len();
        let offset = resolved_offset as usize;
        let prefix_end = buf_len.checked_add(lits.len()).filter(|end| offset <= *end);
        if prefix_end.is_none() {
            buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;
            buffer
                .repeat_lookahead_prefetched(offset, seq.ml as usize)
                .map_err(ExecuteSequencesError::from)?;
            return Ok(());
        }
        // SAFETY: lit_cur_before + 16 <= lit_len so parent-slice read
        // of 16 bytes from lit_src is in-bounds. Offset prefix-resident
        // per the prefix_end check above. exec_sequence_inline_avx2
        // requires target_feature(avx2) which the enclosing fn carries.
        let lit_src = unsafe { literals.as_ptr().add(lit_cur_before) };
        unsafe {
            buffer
                .buffer_mut()
                .exec_sequence_inline_avx2(lit_src, seq.ll as usize, offset, seq.ml as usize)
                .map_err(DecompressBlockError::ExecuteSequencesError)?;
        }
        // Inline path bypasses the wrapper's output counter; keep it current for
        // backends that read it (Ring/Flat). Const-folded away for UserSlice.
        if B::INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER {
            buffer.advance_output_counter((seq.ll + seq.ml) as u64);
        }
        return Ok(());
    }

    // Fallback: legacy push + repeat chain (K-agnostic, real CALL
    // through the target_feature boundary). Same as the SSE2 default.
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
#[allow(dead_code)] // live on aarch64 + tests only; see decode_and_execute_sequences_impl
fn decode_one_sequence_inline<K: crate::cpu_kernel::CpuKernel>(
    ll_dec: &mut SeqFSEDecoder<'_>,
    ml_dec: &mut SeqFSEDecoder<'_>,
    of_dec: &mut SeqFSEDecoder<'_>,
    br: &mut BitReaderReversed<'_, K>,
) -> Sequence {
    // Read base/extra-bits directly off the active FSE state's
    // `Entry`. LL / ML / OF all use the same uniform shape: the
    // build-time enrichment populates `state.base_value` and
    // `state.num_additional_bits` for each axis (LL/ML via
    // `enrich_with_packed_seq_meta` from the packed `LL_META` /
    // `ML_META` tables; OF via `enrich_for_offsets` which writes
    // `base_value = 1 << code` and `num_additional_bits = code`).
    // Reading `state` directly drops the previous `lookup_ll_code` /
    // `lookup_ml_code` indirections (those did a second cache touch
    // on the separate meta tables per sequence) — the active entry
    // is already cache-hot. OF reads from the same Entry layout via
    // `base_value` / `num_additional_bits` written by
    // `enrich_for_offsets` at build time; on x86_64 the codegen
    // matches the prior `1u32 << of_code` shift form (both share the
    // already-touched bit-count cache line) and the uniform read
    // shape unblocks dropping `state.symbol` from the hot path so
    // the 12-byte Entry can shrink to donor's 8-byte ZSTD_seqSymbol
    // in a follow-up tightening of the FSE table cache footprint.
    let ll_state = ll_dec.state;
    let ml_state = ml_dec.state;
    let of_state = of_dec.state;

    let ll_value = ll_state.base_value;
    let ll_num_bits = ll_state.num_additional_bits;
    let ml_value = ml_state.base_value;
    let ml_num_bits = ml_state.num_additional_bits;
    // Donor-shape uniform read: OF uses `base_value` + `num_additional_bits`
    // like LL/ML, dropping the `entry.symbol → 1 << symbol` shift. Both
    // fields are already populated by `enrich_for_offsets` (`base_value
    // = 1 << code`, `num_additional_bits = code`). On x86_64 the memory
    // load is wash vs the shift since both fields share the same Entry
    // cache line that was already touched for the bit-count read; the
    // win is that the hot path no longer reads `state.symbol`, which
    // unblocks dropping the field from `Entry` (donor's ZSTD_seqSymbol
    // is 8 bytes vs our 12 — that would tighten the FSE table cache
    // footprint by 4 bytes / entry).
    let of_num_bits = of_state.num_additional_bits;
    let of_base = of_state.base_value;

    debug_assert!(of_num_bits <= MAX_OFFSET_CODE);

    let (obits, ml_add, ll_add) = br.get_bits_triple(of_num_bits, ml_num_bits, ll_num_bits);
    let offset = obits as u32 + of_base;

    debug_assert_ne!(offset, 0);

    Sequence {
        ll: ll_value + ll_add as u32,
        ml: ml_value + ml_add as u32,
        of: offset,
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
pub(crate) fn compute_offsets_long_share(offsets: &crate::fse::SeqFSETable) -> u32 {
    const OFFSET_FSE_LOG: u32 = 8;
    const LONG_OFFSET_CODE_THRESHOLD: u32 = 22;
    let table_log = offsets.accuracy_log as u32;
    // `SeqSymbol` has no per-state byte; after `enrich_for_offsets`
    // the source offset code lives in `num_additional_bits`
    // (`code` for `code < 32`, `0` otherwise — long codes are
    // bounded by the format spec at 31).
    let raw = offsets
        .decode()
        .iter()
        .filter(|entry| u32::from(entry.num_additional_bits) > LONG_OFFSET_CODE_THRESHOLD)
        .count() as u32;
    // Format-spec bound `OF_MAX_LOG = 8` keeps `table_log <=
    // OFFSET_FSE_LOG` for every valid offsets stream, so the shift
    // is wrap-free.
    raw << OFFSET_FSE_LOG.saturating_sub(table_log)
}

pub(crate) fn maybe_update_fse_tables(
    section: &SequencesHeader,
    source: &[u8],
    scratch: &mut FSEScratch,
) -> Result<usize, DecodeSequenceError> {
    let modes = section
        .modes
        .ok_or(DecodeSequenceError::MissingCompressionMode)?;

    let mut bytes_read = 0;

    let ll_mode = modes.ll_mode();
    match ll_mode {
        ModeType::FSECompressed => {
            let bytes = scratch.literal_lengths.build_decoder(source, LL_MAX_LOG)?;
            bytes_read += bytes;
            scratch
                .literal_lengths
                .enrich_with_packed_seq_meta(&LL_META);

            vprintln!("Updating ll table");
            vprintln!("Used bytes: {}", bytes);
        }
        ModeType::RLE => {
            vprintln!("Use RLE ll table");
            if source.is_empty() {
                return Err(DecodeSequenceError::MissingByteForRleLlTable);
            }
            bytes_read += 1;
            if source[0] > MAX_LITERAL_LENGTH_CODE {
                return Err(DecodeSequenceError::InvalidRleCode {
                    axis: "LL",
                    code: source[0],
                });
            }
            scratch.literal_lengths.build_rle(source[0]);
            scratch
                .literal_lengths
                .enrich_with_packed_seq_meta(&LL_META);
        }
        ModeType::Predefined => {
            vprintln!("Use predefined ll table");
            // Default LL distribution → cached table memcpy.
            #[cfg(feature = "std")]
            {
                scratch.literal_lengths.reinit_from(predefined_ll_table());
            }
            #[cfg(not(feature = "std"))]
            {
                scratch.literal_lengths.build_from_probabilities(
                    LL_DEFAULT_ACC_LOG,
                    &LITERALS_LENGTH_DEFAULT_DISTRIBUTION,
                )?;
                scratch
                    .literal_lengths
                    .enrich_with_packed_seq_meta(&LL_META);
            }
        }
        ModeType::Repeat => {
            vprintln!("Repeat ll table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };
    // Copy-on-write "write" step: any non-Repeat rebuild wrote the local
    // table, so the axis no longer reads the shared dictionary's.
    if !matches!(ll_mode, ModeType::Repeat) {
        scratch.mark_ll_local();
    }

    let of_source = &source[bytes_read..];

    let of_mode = modes.of_mode();
    match of_mode {
        ModeType::FSECompressed => {
            let bytes = scratch.offsets.build_decoder(of_source, OF_MAX_LOG)?;
            scratch.offsets.enrich_for_offsets();
            vprintln!("Updating of table");
            vprintln!("Used bytes: {}", bytes);
            bytes_read += bytes;
            scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
        }
        ModeType::RLE => {
            vprintln!("Use RLE of table");
            if of_source.is_empty() {
                return Err(DecodeSequenceError::MissingByteForRleOfTable);
            }
            bytes_read += 1;
            if of_source[0] > MAX_OFFSET_CODE {
                return Err(DecodeSequenceError::InvalidRleCode {
                    axis: "OF",
                    code: of_source[0],
                });
            }
            // Build a degenerate 1-state table so the fused decode path
            // handles this axis uniformly (no separate RLE fallback).
            scratch.offsets.build_rle(of_source[0]);
            scratch.offsets.enrich_for_offsets();
            scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
        }
        ModeType::Predefined => {
            vprintln!("Use predefined of table");
            // Default OF distribution → cached table + cached long-share.
            #[cfg(feature = "std")]
            {
                let (cached, long_share) = predefined_of_table();
                scratch.offsets.reinit_from(cached);
                scratch.offsets_long_share = long_share;
            }
            #[cfg(not(feature = "std"))]
            {
                scratch
                    .offsets
                    .build_from_probabilities(OF_DEFAULT_ACC_LOG, &OFFSET_DEFAULT_DISTRIBUTION)?;
                scratch.offsets.enrich_for_offsets();
                scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
            }
        }
        ModeType::Repeat => {
            vprintln!("Repeat of table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };
    if !matches!(of_mode, ModeType::Repeat) {
        scratch.mark_of_local();
    }

    let ml_source = &source[bytes_read..];

    let ml_mode = modes.ml_mode();
    match ml_mode {
        ModeType::FSECompressed => {
            let bytes = scratch.match_lengths.build_decoder(ml_source, ML_MAX_LOG)?;
            scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
            bytes_read += bytes;
            vprintln!("Updating ml table");
            vprintln!("Used bytes: {}", bytes);
        }
        ModeType::RLE => {
            vprintln!("Use RLE ml table");
            if ml_source.is_empty() {
                return Err(DecodeSequenceError::MissingByteForRleMlTable);
            }
            bytes_read += 1;
            if ml_source[0] > MAX_MATCH_LENGTH_CODE {
                return Err(DecodeSequenceError::InvalidRleCode {
                    axis: "ML",
                    code: ml_source[0],
                });
            }
            scratch.match_lengths.build_rle(ml_source[0]);
            scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
        }
        ModeType::Predefined => {
            vprintln!("Use predefined ml table");
            // Default ML distribution → cached table memcpy.
            #[cfg(feature = "std")]
            {
                scratch.match_lengths.reinit_from(predefined_ml_table());
            }
            #[cfg(not(feature = "std"))]
            {
                scratch.match_lengths.build_from_probabilities(
                    ML_DEFAULT_ACC_LOG,
                    &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
                )?;
                scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
            }
        }
        ModeType::Repeat => {
            vprintln!("Repeat ml table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };
    if !matches!(ml_mode, ModeType::Repeat) {
        scratch.mark_ml_local();
    }

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
// LL / OF / ML symbol stream follows the RFC 8878 default
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
// then `reinit_from` the cached table into the per-frame scratch.
// `reinit_from` reuses the existing `decode` Vec allocation when the
// capacity already fits (it does, the scratch is re-used across
// frames), copying only the `decode` entries + `accuracy_log` +
// `symbol_probabilities` content. The build-only `symbol_spread_buffer`
// is NOT copied — `reinit_from` only `reserve`s capacity for it —
// shaving the spread-buffer memcpy that the prior `clone_from` did.
//
// Std-only because `OnceLock` lives in `std::sync` — there is no
// `core::sync::OnceLock` (the only stable OnceLock-style API
// requires std). `no_std` builds fall back to the per-call rebuild
// path via the `#[cfg(feature = "std")]` gate. The
// `critical-section` Cargo feature already flagged in the manifest
// is the planned route to extend the cache to no-atomic targets
// without pulling in `once_cell`.
//
// The build step is infallible by construction: the source
// distribution slices are compile-time constants verified against
// the RFC 8878 reference, and `build_from_probabilities` only fails
// on malformed input (sum mismatch, oversized acc_log, symbol >
// max). Treating a failure here as a panic is correct — it would
// mean a static array literal is mathematically broken, which is a
// compile-time bug, not a runtime data condition. Returning
// `&'static FSETable` (infallible) lets `OnceLock::get_or_init`
// handle the cache primitive directly without a fallible-init
// shim.
#[cfg(feature = "std")]
fn predefined_ll_table() -> &'static crate::fse::SeqFSETable {
    use std::sync::OnceLock;
    static CACHED: OnceLock<crate::fse::SeqFSETable> = OnceLock::new();
    CACHED.get_or_init(|| {
        let mut t = crate::fse::SeqFSETable::new(MAX_LITERAL_LENGTH_CODE);
        t.build_from_probabilities(LL_DEFAULT_ACC_LOG, &LITERALS_LENGTH_DEFAULT_DISTRIBUTION)
            .expect("LITERALS_LENGTH_DEFAULT_DISTRIBUTION is a static RFC 8878 constant");
        t.enrich_with_packed_seq_meta(&LL_META);
        t
    })
}

#[cfg(feature = "std")]
fn predefined_ml_table() -> &'static crate::fse::SeqFSETable {
    use std::sync::OnceLock;
    static CACHED: OnceLock<crate::fse::SeqFSETable> = OnceLock::new();
    CACHED.get_or_init(|| {
        let mut t = crate::fse::SeqFSETable::new(MAX_MATCH_LENGTH_CODE);
        t.build_from_probabilities(ML_DEFAULT_ACC_LOG, &MATCH_LENGTH_DEFAULT_DISTRIBUTION)
            .expect("MATCH_LENGTH_DEFAULT_DISTRIBUTION is a static RFC 8878 constant");
        t.enrich_with_packed_seq_meta(&ML_META);
        t
    })
}

#[cfg(feature = "std")]
fn predefined_of_table() -> (&'static crate::fse::SeqFSETable, u32) {
    use std::sync::OnceLock;
    static CACHED: OnceLock<(crate::fse::SeqFSETable, u32)> = OnceLock::new();
    let cache = CACHED.get_or_init(|| {
        let mut t = crate::fse::SeqFSETable::new(MAX_OFFSET_CODE);
        t.build_from_probabilities(OF_DEFAULT_ACC_LOG, &OFFSET_DEFAULT_DISTRIBUTION)
            .expect("OFFSET_DEFAULT_DISTRIBUTION is a static RFC 8878 constant");
        t.enrich_for_offsets();
        let share = compute_offsets_long_share(&t);
        (t, share)
    });
    (&cache.0, cache.1)
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
    use crate::fse::SeqFSETable;

    let mut ll_rebuild = SeqFSETable::new(MAX_LITERAL_LENGTH_CODE);
    ll_rebuild
        .build_from_probabilities(
            LL_DEFAULT_ACC_LOG,
            &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    ll_rebuild.enrich_with_packed_seq_meta(&LL_META);
    let ll_cached = predefined_ll_table();
    assert_eq!(ll_rebuild.accuracy_log, ll_cached.accuracy_log);
    assert_eq!(ll_rebuild.decode().len(), ll_cached.decode().len());
    for (i, (a, b)) in ll_rebuild
        .decode()
        .iter()
        .zip(ll_cached.decode().iter())
        .enumerate()
    {
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

    let mut ml_rebuild = SeqFSETable::new(MAX_MATCH_LENGTH_CODE);
    ml_rebuild
        .build_from_probabilities(
            ML_DEFAULT_ACC_LOG,
            &Vec::from(&MATCH_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    ml_rebuild.enrich_with_packed_seq_meta(&ML_META);
    let ml_cached = predefined_ml_table();
    assert_eq!(ml_rebuild.accuracy_log, ml_cached.accuracy_log);
    assert_eq!(ml_rebuild.decode().len(), ml_cached.decode().len());
    for (i, (a, b)) in ml_rebuild
        .decode()
        .iter()
        .zip(ml_cached.decode().iter())
        .enumerate()
    {
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

    let mut of_rebuild = SeqFSETable::new(MAX_OFFSET_CODE);
    of_rebuild
        .build_from_probabilities(
            OF_DEFAULT_ACC_LOG,
            &Vec::from(&OFFSET_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();
    of_rebuild.enrich_for_offsets();
    let of_rebuild_share = compute_offsets_long_share(&of_rebuild);
    let (of_cached, of_cached_share) = predefined_of_table();
    assert_eq!(of_rebuild.accuracy_log, of_cached.accuracy_log);
    assert_eq!(of_rebuild.decode().len(), of_cached.decode().len());
    assert_eq!(
        of_rebuild_share, of_cached_share,
        "OF offsets_long_share mismatch"
    );
    for (i, (a, b)) in of_rebuild
        .decode()
        .iter()
        .zip(of_cached.decode().iter())
        .enumerate()
    {
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
    let mut table = crate::fse::SeqFSETable::new(MAX_LITERAL_LENGTH_CODE);
    table
        .build_from_probabilities(
            LL_DEFAULT_ACC_LOG,
            &Vec::from(&LITERALS_LENGTH_DEFAULT_DISTRIBUTION[..]),
        )
        .unwrap();

    assert!(table.decode().len() == 64);

    //just test a few values. TODO test all values
    assert!(table.decode()[0].num_bits == 4);
    assert!(table.decode()[0].new_state == 0);

    assert!(table.decode()[19].num_bits == 6);
    assert!(table.decode()[19].new_state == 0);

    assert!(table.decode()[39].num_bits == 4);
    assert!(table.decode()[39].new_state == 16);

    assert!(table.decode()[60].num_bits == 6);
    assert!(table.decode()[60].new_state == 0);

    assert!(table.decode()[59].num_bits == 5);
    assert!(table.decode()[59].new_state == 32);
}

#[cfg(test)]
mod offsets_long_share_tests {
    use super::compute_offsets_long_share;

    /// Construct a synthetic offsets [`SeqFSETable`] with the given
    /// offset code per entry. Mirrors the post-`enrich_for_offsets`
    /// shape used by `compute_offsets_long_share`: each entry's
    /// `num_additional_bits` holds the code (== source byte for
    /// `code < 32`, the only range this helper reads).
    fn synthetic_offsets_table(accuracy_log: u8, symbols: &[u8]) -> crate::fse::SeqFSETable {
        use crate::fse::SeqSymbol;
        let size = 1usize << accuracy_log;
        assert_eq!(
            symbols.len(),
            size,
            "symbols.len() must equal 1 << accuracy_log"
        );
        let mut t = crate::fse::SeqFSETable::new(31);
        t.accuracy_log = accuracy_log;
        let entries: alloc::vec::Vec<SeqSymbol> = symbols
            .iter()
            .map(|&s| SeqSymbol {
                new_state: 0,
                num_bits: 0,
                num_additional_bits: s,
                base_value: 0,
            })
            .collect();
        t.set_decode_for_test(&entries);
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

#[cfg(test)]
mod compute_use_long_pipeline_tests {
    use super::{ADVANCE, compute_use_long_pipeline};

    /// Per-target `MIN_LONG_OFFSET_SHARE` (mirrors the cfg in
    /// `compute_use_long_pipeline`). Keep in sync with the constant
    /// in production code so the tests follow target_pointer_width.
    #[cfg(target_pointer_width = "64")]
    const MIN_SHARE: u32 = 7;
    #[cfg(not(target_pointer_width = "64"))]
    const MIN_SHARE: u32 = 20;
    const HISTORY_THRESHOLD: usize = 1 << 24;

    #[test]
    fn rejects_when_num_sequences_below_2x_advance() {
        // Below `ADVANCE * 2` (= 16): never engage the long pipeline,
        // regardless of cold-dict / history / share signals.
        assert!(!compute_use_long_pipeline(
            ADVANCE * 2 - 1,
            true,
            HISTORY_THRESHOLD + 1,
            u32::MAX
        ));
    }

    #[test]
    fn cold_dict_forces_long_pipeline_at_min_seq_count() {
        // At the `ADVANCE * 2` boundary, `ddict_is_cold == true` is a
        // sufficient override — history / share are not read.
        assert!(compute_use_long_pipeline(ADVANCE * 2, true, 0, 0));
    }

    #[test]
    fn history_at_threshold_does_not_engage() {
        // Gate uses `>` not `>=`: history exactly at threshold fails.
        assert!(!compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            HISTORY_THRESHOLD,
            MIN_SHARE,
        ));
    }

    #[test]
    fn history_just_above_threshold_engages_when_share_meets_min() {
        // Strictly above threshold + share at the per-target min: engage.
        assert!(compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            HISTORY_THRESHOLD + 1,
            MIN_SHARE,
        ));
    }

    #[test]
    fn share_below_min_blocks_engagement_even_with_large_history() {
        // Even with the share one below the per-target minimum, no engage.
        const _: () = assert!(MIN_SHARE > 0, "test invariant: MIN_SHARE > 0 required");
        assert!(!compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            HISTORY_THRESHOLD + 1,
            MIN_SHARE - 1,
        ));
    }

    #[test]
    fn saturating_history_engages_when_share_meets_min() {
        // `total_history = usize::MAX` (the saturating-add fallback path
        // from the per-tier wrappers) is well above the threshold.
        assert!(compute_use_long_pipeline(
            ADVANCE * 2,
            false,
            usize::MAX,
            MIN_SHARE,
        ));
    }
}

#[cfg(test)]
mod init_sequence_stream_tests {
    use super::super::scratch::FSEScratch;
    use super::init_sequence_stream;
    use crate::blocks::sequence_section::SequencesHeader;
    use crate::cpu_kernel::ScalarKernel;
    use crate::decoding::decode_buffer::DecodeBuffer;
    use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError};
    use crate::decoding::ringbuffer::RingBuffer;

    /// The sequence bitstream must end with a single `1` sentinel bit in
    /// the final byte; the decoder skips trailing `0` padding looking for
    /// it. A final byte (read first, MSB-down) that is all zeros has no
    /// sentinel, so more than 8 padding bits are skipped — that must be
    /// rejected as `ExtraPadding`, not decoded.
    #[test]
    fn rejects_bitstream_with_excess_padding() {
        let mut header = SequencesHeader::new();
        // `0x01` = one sequence; `0x00` modes byte = all axes Predefined,
        // so `maybe_update_fse_tables` reads zero table bytes and the
        // whole source is the (malformed) bitstream.
        header.parse_from_header(&[0x01, 0x00]).unwrap();

        let mut fse = FSEScratch::new();
        let mut buffer = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        // All-zero bitstream: no sentinel `1` bit anywhere.
        let source = [0u8; 2];

        // `SeqStreamSetup` is not `Debug`, so assert on the `Result`
        // directly via `matches!` rather than unwrapping the Ok side.
        let result = init_sequence_stream::<RingBuffer, ScalarKernel>(
            &header,
            &source,
            &mut fse,
            &mut buffer,
        );
        assert!(
            matches!(
                result,
                Err(DecompressBlockError::DecodeSequenceError(
                    DecodeSequenceError::ExtraPadding { .. }
                ))
            ),
            "all-zero padding must be rejected as ExtraPadding"
        );
    }

    /// One-sequence, all-Predefined header. Pairs with an ample non-zero
    /// bitstream so `init_sequence_stream` returns `Ok` (the sequence loop
    /// may still error afterwards). Used to drive the per-tier monolith
    /// preambles directly: on x86_64 CI only the avx2 tier is selected at
    /// runtime, so scalar / bmi2 / vbmi2 and the K-generic impl would
    /// otherwise never execute their (shared) preamble.
    fn predefined_one_sequence_header() -> SequencesHeader {
        let mut header = SequencesHeader::new();
        header.parse_from_header(&[0x01, 0x00]).unwrap();
        header
    }

    /// Drives the always-available decoders (the portable scalar tier and
    /// the K-generic impl that backs the aarch64 NEON/SVE path) through a
    /// well-formed preamble. The result is intentionally ignored: a
    /// crafted bitstream need not yield a valid sequence, only reach and
    /// pass the preamble.
    #[test]
    fn scalar_tier_and_generic_impl_run_preamble() {
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];

        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        let _ = crate::decoding::seq_decoder_scalar::decode_and_execute_sequences_scalar(
            &header,
            &source,
            &mut fse,
            &mut buf,
            &mut offset_hist,
            &lits,
        );

        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        let _ = super::decode_and_execute_sequences_impl::<RingBuffer, ScalarKernel>(
            &header,
            &source,
            &mut fse,
            &mut buf,
            &mut offset_hist,
            &lits,
        );
    }

    /// Drive the BMI2 monolith preamble directly. The runtime kernel
    /// selector prefers the avx2 tier on any CPU that has BMI2, so this
    /// tier never runs through the normal dispatch on CI hardware; call it
    /// directly (guarded on the same feature it requires).
    #[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_bmi2"))]
    #[test]
    fn bmi2_tier_runs_preamble() {
        if !std::is_x86_feature_detected!("bmi2") {
            return;
        }
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];
        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        // SAFETY: BMI2 confirmed available by the runtime check above.
        let _ = unsafe {
            crate::decoding::seq_decoder_bmi2::decode_and_execute_sequences_bmi2::<RingBuffer>(
                &header,
                &source,
                &mut fse,
                &mut buf,
                &mut offset_hist,
                &lits,
            )
        };
    }

    /// Drive the AVX2 monolith preamble directly. The avx2 tier is the
    /// production path on AVX2 hardware, so this is usually also covered by
    /// the normal decode tests; the explicit call keeps the preamble
    /// covered even on a runner that lacks AVX2.
    #[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_avx2"))]
    #[test]
    fn avx2_tier_runs_preamble() {
        if !(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("bmi2")) {
            return;
        }
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];
        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        // SAFETY: AVX2 + BMI2 confirmed available by the runtime check.
        let _ = unsafe {
            crate::decoding::seq_decoder_avx2::decode_and_execute_sequences_avx2::<RingBuffer>(
                &header,
                &source,
                &mut fse,
                &mut buf,
                &mut offset_hist,
                &lits,
            )
        };
    }

    /// Drive the VBMI2 monolith preamble directly. Requires AVX-512 VBMI2,
    /// which most CI runners lack; the test self-skips there, so this tier
    /// is only covered on AVX-512 hardware.
    #[cfg(all(feature = "std", target_arch = "x86_64", feature = "kernel_vbmi2"))]
    #[test]
    fn vbmi2_tier_runs_preamble() {
        // Mirror the production dispatch gate: the unsafe monolith is annotated
        // `target_feature(bmi2,avx2,avx512vbmi2,avx512f,avx512vl,avx512bw)`, so a
        // bare `avx512vbmi2` probe could SIGILL on a CPU that reports VBMI2 but
        // lacks a companion feature. `detect_cpu_kernel() == Vbmi2` verifies the
        // full set before we call it.
        if !matches!(
            crate::cpu_kernel::detect_cpu_kernel(),
            crate::cpu_kernel::CpuKernelTag::Vbmi2
        ) {
            return;
        }
        let header = predefined_one_sequence_header();
        let source = [0xFFu8; 8];
        let lits = [0u8; 32];
        let mut fse = FSEScratch::new();
        let mut buf = DecodeBuffer::<RingBuffer>::new(4 * 1024);
        let mut offset_hist = [1u32, 4, 8];
        // SAFETY: AVX-512 VBMI2 confirmed available by the runtime check.
        let _ = unsafe {
            crate::decoding::seq_decoder_vbmi2::decode_and_execute_sequences_vbmi2::<RingBuffer>(
                &header,
                &source,
                &mut fse,
                &mut buf,
                &mut offset_hist,
                &lits,
            )
        };
    }
}
