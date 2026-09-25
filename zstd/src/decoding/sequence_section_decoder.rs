use super::super::blocks::sequence_section::ModeType;
use super::super::blocks::sequence_section::Sequence;
use super::super::blocks::sequence_section::SequencesHeader;
use super::scratch::FSEScratch;
use crate::bit_io::BitReaderReversed;
use crate::blocks::sequence_section::{
    MAX_LITERAL_LENGTH_CODE, MAX_MATCH_LENGTH_CODE, MAX_OFFSET_CODE,
};
use crate::cpu_kernel::CpuKernelTag;
use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError, ExecuteSequencesError};
use crate::fse::SeqFSEDecoder;

// 8-slot software pipeline mirroring upstream zstd
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

/// Upstream zstd `ZSTD_decompressBlock_internal` long-pipeline gate. Engages
/// the 8-deep lookahead-ring decoder when (a) the block has enough
/// sequences to amortise prefill+drain (`num_sequences >= ADVANCE * 2`)
/// AND (b) either the dict is cold (first block after attach) OR total
/// history exceeds 16 MB AND the FSE offset distribution carries
/// enough long-distance codes to make prefetch worthwhile.
///
/// `MIN_LONG_OFFSET_SHARE`: upstream zstd `minShare = MEM_64bits() ? 7 : 20` —
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

/// Shared cold preamble for every CPU-tier sequence decoder (the portable
/// body in `seq_decoder_scalar`, which the BMI2 entry also runs, and the
/// x86 monoliths in `seq_decoder_{avx2,vbmi2}`).
///
/// Consumes the one-shot `ddict_is_cold` flag, rebuilds the FSE tables
/// if the block's mode bytes call for it, skips the start-of-stream
/// padding, initialises the LL/OF/ML decoder states, reserves the
/// block's output capacity AND arms the per-block output ceiling (the
/// decompression-bomb guard that bounds growth at `len + block_maximum`),
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
    dict: Option<&'fse crate::decoding::dictionary::Dictionary>,
) -> Result<SeqStreamSetup<'src, 'fse, K>, DecompressBlockError>
where
    B: super::buffer_backend::BufferBackend,
    K: crate::cpu_kernel::CpuKernel,
{
    // Consume the one-shot `ddict_is_cold` flag BEFORE any early return
    // (padding validation) so a later block's gate can't mis-apply a
    // cold-dict signal that no longer holds. Upstream zstd
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
    //
    // Resolve each axis ONCE: a `Predefined` axis answers out of a `OnceLock`,
    // so a second call for `accuracy_log` below would repeat that synchronised
    // load per block.
    let ll_src = fse.ll_table(dict);
    let ml_src = fse.ml_table(dict);
    let of_src = fse.of_table(dict);

    let mut ll_dec = SeqFSEDecoder::new(ll_src);
    let mut ml_dec = SeqFSEDecoder::new(ml_src);
    let mut of_dec = SeqFSEDecoder::new(of_src);

    ll_dec
        .init_state(&mut br)
        .map_err(DecodeSequenceError::from)?;
    of_dec
        .init_state(&mut br)
        .map_err(DecodeSequenceError::from)?;
    ml_dec
        .init_state(&mut br)
        .map_err(DecodeSequenceError::from)?;

    let max_update_bits = ll_src.accuracy_log + ml_src.accuracy_log + of_src.accuracy_log;
    debug_assert!(
        max_update_bits <= 56,
        "sequence section update bits exceed 56-bit budget"
    );

    // The block's output room is reserved and its ceiling armed by the block
    // decoder before it calls in: that is where the frame's block maximum is
    // known, and keeping the arithmetic out of this body keeps it out of the
    // per-kernel monomorphs this function is inlined into.
    let old_buffer_size = buffer.len();
    let num_sequences = section.num_sequences as usize;

    // Overflow is only reachable on 32-bit `usize` (a 4 GiB-class
    // window_size plus a dict). The gate below asks "does history exceed
    // the prefetch threshold", so on the overflow path the clamped maximum
    // is the correct answer, not a wrapped small value.
    let total_history = match buffer
        .window_size
        .checked_add(buffer.dict_content(dict).len())
    {
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
/// Upstream zstd parity: zstd's `ZSTD_decompressSequences_body` interleaves
/// `ZSTD_decodeSequence` and `ZSTD_execSequence` in one loop, keeping
/// the `seq_t` in registers. We were paying ~24 B/seq × 2 (write + read)
/// of L1↔L2 traffic on the dropped `Vec<Sequence>` roundtrip plus the
/// per-iter `Vec::push` overhead.
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
// One argument over the lint's threshold, and the extra one is the resolved
// kernel tag: bundling it into a struct with the unrelated section/scratch
// borrows would obscure that it is a plain Copy value carried down from the
// decoder, not more state to thread.
#[allow(clippy::too_many_arguments)]
pub fn decode_and_execute_sequences<'fse, B: super::buffer_backend::BufferBackend>(
    section: &SequencesHeader,
    source: &[u8],
    fse: &'fse mut FSEScratch,
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    offset_hist: &mut [u32; 3],
    literals_buffer: &[u8],
    literals_len: usize,
    dict: Option<&'fse crate::decoding::dictionary::Dictionary>,
    kernel: CpuKernelTag,
) -> Result<(), DecompressBlockError> {
    // Feature detection happened once, before the first block. This is the
    // dispatch: one branch selecting the per-tier monomorph, which then runs
    // the whole sequence loop with the tier baked in.
    match kernel {
        CpuKernelTag::Scalar => {
            super::seq_decoder_scalar::decode_and_execute_sequences_scalar::<B>(
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
        #[cfg(all(target_arch = "x86_64", feature = "kernel-sse"))]
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
                literals_len,
                dict,
            )
        }
        // 32-bit x86 reaches the BMI2 tier for the entropy tables (the HUF
        // state advance takes `bzhi` through `K`), but the BMI2 sequence entry
        // is x86_64-only, so the portable walk runs here.
        #[cfg(all(target_arch = "x86", feature = "kernel-bmi2"))]
        CpuKernelTag::Bmi2 => super::seq_decoder_scalar::decode_and_execute_sequences_scalar::<B>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
            literals_len,
            dict,
        ),
        #[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
        CpuKernelTag::Bmi2 => {
            // SAFETY: `detect_cpu_kernel()` only returns Bmi2 when
            // `is_x86_feature_detected!("bmi2")` confirmed BMI2 is
            // available. The entry runs the shared portable body under
            // `target_feature(bmi2)`, so the kernel's masks compile to `bzhi`.
            unsafe {
                super::seq_decoder_bmi2::decode_and_execute_sequences_bmi2::<B>(
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
        }
        #[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
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
                    literals_len,
                    dict,
                )
            }
        }
        #[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
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
                    literals_len,
                    dict,
                )
            }
        }
        #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
        // NEON and SVE use the same scalar bit operations. Their copy kernels
        // are selected by the buffer; share the optimized sequence loop.
        CpuKernelTag::Neon => super::seq_decoder_scalar::decode_and_execute_sequences_scalar::<B>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
            literals_len,
            dict,
        ),
        #[cfg(all(
            target_arch = "aarch64",
            feature = "kernel-sve",
            any(feature = "std", target_feature = "sve"),
        ))]
        CpuKernelTag::Sve => super::seq_decoder_scalar::decode_and_execute_sequences_scalar::<B>(
            section,
            source,
            fse,
            buffer,
            offset_hist,
            literals_buffer,
            literals_len,
            dict,
        ),
    }
}

// Per-tier x86 trampolines (`decode_and_execute_sequences_{bmi2,avx2,vbmi2}`)
// live in `seq_decoder_bmi2.rs` / `seq_decoder_avx2.rs` /
// `seq_decoder_vbmi2.rs`. Each owns its `#[target_feature]` attribute
// and is called from the dispatch matcher above. See issue #279
// round 3 for the per-kernel architecture rationale.

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

/// Pipelined-path executor variant: takes the offset already resolved
/// by the decode-ahead `shadow_hist` walk, so `do_offset_history` is
/// NOT called here (caller mutated only the shadow). Routes the match
/// copy through `repeat_lookahead_prefetched`, which skips only the
/// in-loop `prefetch_match_source` (redundant because the lookahead
/// pipeline already issued a PREFETCH_L1 ADVANCE iterations earlier).
/// The per-call `buffer.reserve(match_length)` is preserved by that
/// variant — required for memory safety against malformed inputs whose
/// `match_length` exceeds the upfront block-maximum headroom.
#[inline(always)]
// Grouping the arguments into a struct would push them off the argument
// registers and onto memory loads, which is the cost this per-sequence
// boundary exists to avoid.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_one_sequence_pipelined<B: super::buffer_backend::BufferBackend>(
    buffer: &mut super::decode_buffer::DecodeBuffer<B>,
    dict: Option<&crate::decoding::dictionary::Dictionary>,
    dict_content: &[u8],
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

    // Upstream zstd-shape inline dispatch — when the backend opts in
    // (`UserSliceBackend` on x86_64 today, per its
    // `SUPPORTS_INLINE_SEQUENCE_EXEC = true` const) we collapse the
    // literal copy + match copy into a single straight-line body
    // that mirrors upstream zstd `ZSTD_execSequence`
    // (zstd_decompress_block.c:1008-1105). The const branch is
    // compile-time per backend monomorphisation, so the dead arm
    // carries no runtime cost on either side.
    //
    // **Literal-source slack** (the read-side port contract): the inline
    // copiers read past the declared literal end twice, an unconditional
    // `copy16` whatever the literal length, and the wildcopy tail's last chunk
    // when the length exceeds 16. Both reads are covered for the whole block by
    // the literals decoder, which hands this loop a buffer carrying
    // `WILDCOPY_OVERLENGTH` readable bytes after the literals: a Raw section is
    // borrowed only when the block has that room after it, and every other
    // section is materialised with it. Upstream decides the same thing in the
    // same place (`zstd_decompress_block.c:275`), which is why its executor
    // tests only `iLitEnd > litLimit` and never the slack.
    let inline_literals_ok = B::SUPPORTS_INLINE_SEQUENCE_EXEC;
    let offset = resolved_offset as usize;
    // Where the match source lives (matches `repeat()`'s `offset >
    // buffer.len()` → dict path gate). `checked_add` against adversarial
    // input: if `buffer.len() + lits.len()` would wrap `usize`, treat the
    // offset as out-of-range rather than letting wrapping addition classify a
    // wildly out-of-range one as resident and hand the inline path an OOB
    // match-source pointer.
    let prefix_resident = buffer
        .len()
        .checked_add(lits.len())
        .is_some_and(|end| offset <= end);
    if !prefix_resident {
        // Match source reaches outside what's been written in this frame:
        // upstream zstd's `extDict` arm. When the whole match sits inside
        // reachable dictionary content it is one more inline copy, the way
        // upstream keeps that branch inside `ZSTD_execSequence`; every other
        // shape (spanning dictionary and output, out of window, out of range)
        // stays on the slow `repeat()` path, which reports the errors.
        //
        // The gate is the DICTIONARY one: the source is the dictionary, a
        // separate allocation, so the output-resident bound does not apply and
        // asking for it would refuse every such match on a wrapped ring.
        if inline_literals_ok
            && buffer
                .buffer_mut()
                .inline_exec_dict_ok(seq.ll as usize, seq.ml as usize)
            && let Some(dict_src) =
                buffer.dict_match_source(dict_content, lits.len(), offset, seq.ml as usize)
        {
            // SAFETY: as for the prefix-resident call below (parent-slice
            // provenance for the literals, dispatch-site gate for their
            // 16-byte read), and the match source is the dictionary slice,
            // whose length covers `seq.ml` by the selector's contract.
            let lit_src = unsafe { literals.as_ptr().add(lit_cur_before) };
            unsafe {
                buffer
                    .buffer_mut()
                    .exec_sequence_inline_dict(lit_src, seq.ll as usize, dict_src, seq.ml as usize)
                    .map_err(DecompressBlockError::ExecuteSequencesError)?;
            }
            if B::INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER {
                buffer.advance_output_counter((seq.ll + seq.ml) as u64);
            }
            return Ok(());
        }
        buffer.try_push(lits).map_err(ExecuteSequencesError::from)?;
        buffer
            .repeat_lookahead_prefetched(dict, offset, seq.ml as usize)
            .map_err(ExecuteSequencesError::from)?;
        return Ok(());
    }
    let inline_path_safe = inline_literals_ok
        && buffer
            .buffer_mut()
            .inline_exec_ok(seq.ll as usize, seq.ml as usize, offset);
    if inline_path_safe {
        // SAFETY:
        // - Backend opted in (compile-time const).
        // - `lits` is a non-aliased slice of the literals block.
        // - Source-side slack: `lit_cur_before + 16 <= lit_len`
        //   (gated above), so `lits.as_ptr().add(16)` reads stay
        //   inside the literals buffer. Upstream zstd unconditional
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
        // - Caller's upfront block-maximum reserve plus the
        //   `WILDCOPY_OVERLENGTH = 32` slack on the user slice
        //   guarantees the writable tail has room for
        //   `lit_length + match_length + 15` (max wildcopy
        //   overshoot is 15 bytes past the declared end).
        // SAFETY: `literals.as_ptr().add(lit_cur_before)` has the
        // provenance of the FULL `literals` slice (not `lits`, the
        // sub-slice). The 16-byte unconditional `copy16` inside the
        // upstream zstd body reads up to `lit_cur_before + 16` bytes from
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
        .repeat_lookahead_prefetched(dict, resolved_offset as usize, seq.ml as usize)
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
    dict: Option<&crate::decoding::dictionary::Dictionary>,
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
                .repeat_lookahead_prefetched(dict, offset, seq.ml as usize)
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
        .repeat_lookahead_prefetched(dict, resolved_offset as usize, seq.ml as usize)
        .map_err(ExecuteSequencesError::from)?;
    Ok(())
}

/// Packed (baseline, extra_bits) pairs for literal-length codes.
/// Upstream zstd parity: `LL_base` + `LL_bits` from the zstd reference
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
/// Upstream zstd parity: `ML_base` + `ML_bits`. Codes 0..=52 per Zstandard
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

/// Walk the offsets FSE decode table and return the upstream zstd-shaped
/// "share of long offsets" signal: count entries whose symbol (offset
/// code) is > 22 (raw offset ≥ 2²³ = 8 MiB), then scale up to the
/// upstream zstd `OffFSELog = 8` reference so a fine-grained table still
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
            let bytes = scratch.literal_lengths.build_decoder_fused(
                source,
                LL_MAX_LOG,
                crate::fse::SeqMeta::Packed(&LL_META),
            )?;
            bytes_read += bytes;

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
            // Default LL distribution → read the cached table in place.
            #[cfg(feature = "std")]
            {
                scratch.mark_ll_predefined(predefined_ll_table());
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
                scratch.mark_ll_local();
            }
        }
        ModeType::Repeat => {
            vprintln!("Repeat ll table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };
    // Copy-on-write "write" step: an FSE / RLE rebuild wrote the local table,
    // so the axis no longer reads the shared dictionary's. Predefined mode
    // sets its own source in its arm, Repeat keeps whatever the axis had.
    if matches!(ll_mode, ModeType::FSECompressed | ModeType::RLE) {
        scratch.mark_ll_local();
    }

    let of_source = &source[bytes_read..];

    let of_mode = modes.of_mode();
    match of_mode {
        ModeType::FSECompressed => {
            let bytes = scratch.offsets.build_decoder_fused(
                of_source,
                OF_MAX_LOG,
                crate::fse::SeqMeta::Offsets,
            )?;
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
            // Default OF distribution → cached table read in place. The share
            // is taken from the constant rather than from the cache: reading it
            // there would probe the `OnceLock` on a path that otherwise never
            // touches the table, and the pipeline gate would then pay two
            // probes per block for this one axis.
            #[cfg(feature = "std")]
            {
                scratch.mark_of_predefined(predefined_of_table().0);
                scratch.offsets_long_share = PREDEFINED_OF_LONG_SHARE;
            }
            #[cfg(not(feature = "std"))]
            {
                scratch
                    .offsets
                    .build_from_probabilities(OF_DEFAULT_ACC_LOG, &OFFSET_DEFAULT_DISTRIBUTION)?;
                scratch.offsets.enrich_for_offsets();
                scratch.offsets_long_share = compute_offsets_long_share(&scratch.offsets);
                scratch.mark_of_local();
            }
        }
        ModeType::Repeat => {
            vprintln!("Repeat of table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };
    if matches!(of_mode, ModeType::FSECompressed | ModeType::RLE) {
        scratch.mark_of_local();
    }

    let ml_source = &source[bytes_read..];

    let ml_mode = modes.ml_mode();
    match ml_mode {
        ModeType::FSECompressed => {
            let bytes = scratch.match_lengths.build_decoder_fused(
                ml_source,
                ML_MAX_LOG,
                crate::fse::SeqMeta::Packed(&ML_META),
            )?;
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
            // Default ML distribution → read the cached table in place.
            #[cfg(feature = "std")]
            {
                scratch.mark_ml_predefined(predefined_ml_table());
            }
            #[cfg(not(feature = "std"))]
            {
                scratch.match_lengths.build_from_probabilities(
                    ML_DEFAULT_ACC_LOG,
                    &MATCH_LENGTH_DEFAULT_DISTRIBUTION,
                )?;
                scratch.match_lengths.enrich_with_packed_seq_meta(&ML_META);
                scratch.mark_ml_local();
            }
        }
        ModeType::Repeat => {
            vprintln!("Repeat ml table");
            /* Nothing to do — cached enriched values stay valid. */
        }
    };
    if matches!(ml_mode, ModeType::FSECompressed | ModeType::RLE) {
        scratch.mark_ml_local();
    }

    Ok(bytes_read)
}

// The default Literal Length decoding table uses an accuracy logarithm of 6 bits.
const LL_DEFAULT_ACC_LOG: u8 = 6;
/// If [ModeType::Predefined] is selected for a symbol type, its FSE decoding
/// table is generated using a predefined distribution table.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#literals-length>
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
pub(crate) fn predefined_ll_table() -> &'static crate::fse::SeqFSETable {
    use super::scratch::AlignedFSETable;
    use std::sync::OnceLock;
    static CACHED: OnceLock<AlignedFSETable> = OnceLock::new();
    CACHED.get_or_init(|| {
        let mut t = crate::fse::SeqFSETable::new(MAX_LITERAL_LENGTH_CODE);
        t.build_from_probabilities(LL_DEFAULT_ACC_LOG, &LITERALS_LENGTH_DEFAULT_DISTRIBUTION)
            .expect("LITERALS_LENGTH_DEFAULT_DISTRIBUTION is a static RFC 8878 constant");
        t.enrich_with_packed_seq_meta(&LL_META);
        t.into()
    })
}

#[cfg(feature = "std")]
pub(crate) fn predefined_ml_table() -> &'static crate::fse::SeqFSETable {
    use super::scratch::AlignedFSETable;
    use std::sync::OnceLock;
    static CACHED: OnceLock<AlignedFSETable> = OnceLock::new();
    CACHED.get_or_init(|| {
        let mut t = crate::fse::SeqFSETable::new(MAX_MATCH_LENGTH_CODE);
        t.build_from_probabilities(ML_DEFAULT_ACC_LOG, &MATCH_LENGTH_DEFAULT_DISTRIBUTION)
            .expect("MATCH_LENGTH_DEFAULT_DISTRIBUTION is a static RFC 8878 constant");
        t.enrich_with_packed_seq_meta(&ML_META);
        t.into()
    })
}

#[cfg(feature = "std")]
pub(crate) fn predefined_of_table() -> (&'static crate::fse::SeqFSETable, u32) {
    use super::scratch::AlignedFSETable;
    use std::sync::OnceLock;
    static CACHED: OnceLock<(AlignedFSETable, u32)> = OnceLock::new();
    let cache = CACHED.get_or_init(|| {
        let mut t = crate::fse::SeqFSETable::new(MAX_OFFSET_CODE);
        t.build_from_probabilities(OF_DEFAULT_ACC_LOG, &OFFSET_DEFAULT_DISTRIBUTION)
            .expect("OFFSET_DEFAULT_DISTRIBUTION is a static RFC 8878 constant");
        t.enrich_for_offsets();
        let share = compute_offsets_long_share(&t);
        (t.into(), share)
    });
    (&cache.0, cache.1)
}

/// Long-offset share of the predefined offsets table. That table is built from
/// a distribution fixed by the format, so the share it yields is fixed too, and
/// naming it here keeps the Predefined arm from resolving the cached table just
/// to read one number. A test pins it against the builder.
#[cfg(feature = "std")]
const PREDEFINED_OF_LONG_SHARE: u32 = 48;

// The default Match Length decoding table uses an accuracy logarithm of 6 bits.
const ML_DEFAULT_ACC_LOG: u8 = 6;
/// If [ModeType::Predefined] is selected for a symbol type, its FSE decoding
/// table is generated using a predefined distribution table.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#match-length>
const MATCH_LENGTH_DEFAULT_DISTRIBUTION: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

// The default Match Length decoding table uses an accuracy logarithm of 5 bits.
const OF_DEFAULT_ACC_LOG: u8 = 5;
/// If [ModeType::Predefined] is selected for a symbol type, its FSE decoding
/// table is generated using a predefined distribution table.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#offset-codes>
const OFFSET_DEFAULT_DISTRIBUTION: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

#[cfg(test)]
mod tests;
