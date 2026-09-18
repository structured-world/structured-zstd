//! AVX2-tier monolithic sequence-section decoder.
//!
//! One self-contained `#[target_feature(enable = "bmi2,avx2")]` function
//! with the entire decode + execute pipeline as ONE body. Sequence-decode
//! and sequence-execute logic lives in `macro_rules!` blocks that expand
//! textually at every callsite — no inner function CALL boundaries, no
//! reliance on the LLVM inline cost-model (which would not inline through
//! `target_feature` + multiple callsites + `Result<>` panic landings).
//! Macros expand BEFORE LLVM sees the code, guaranteeing zero call
//! overhead regardless of cost-model decisions.
//!
//! BitReader pinned to `Avx2Kernel`; triple-bit extract goes directly
//! through `peek_bits_triple_bmi2` (`_pext_u64` inline at every callsite).
//! Match copy routes to `BufferBackend::exec_sequence_inline_avx2`
//! (32-byte ymm wildcopy).

#![cfg(target_arch = "x86_64")]

use super::buffer_backend::BufferBackend;
use super::decode_buffer::DecodeBuffer;
use super::exec_sequence_inline::{
    MAX_WILDCOPY_OVERSHOOT, exec_sequence_avx2_dict_inline, exec_sequence_avx2_inline_at,
};
use super::scratch::FSEScratch;
use super::sequence_section_decoder::{
    ADVANCE, ADVANCE_MASK, ExecSeq, SeqStreamSetup, init_sequence_stream,
};
use crate::blocks::sequence_section::{MAX_OFFSET_CODE, Sequence, SequencesHeader};
use crate::cpu_kernel::Avx2Kernel;
use crate::decoding::errors::{DecodeSequenceError, DecompressBlockError, ExecuteSequencesError};
use crate::decoding::sequence_execution::do_offset_history;

/// Textual expansion of per-sequence decode. Reads LL/ML/OF state,
/// performs triple-bit extract via `peek_bits_triple_bmi2` (`_pext_u64`
/// inline when vendor cache enables it), advances the bit cursor.
/// Expands at every callsite inside the AVX2 monolith — no function
/// boundary survives compilation.
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

        let sum_wide = u16::from(of_num_bits) + u16::from(ml_num_bits) + u16::from(ll_num_bits);
        let (obits, ml_add, ll_add) = if sum_wide <= 56 {
            // Upstream `ZSTD_decodeSequence` reads OF/ML/LL as three separate
            // `BIT_readBitsFast` after one reload. The fields are independent so
            // the CPU pipelines them; the old PEXT-triple folded all three into
            // one serial `pext` dependency. One `ensure_bits` up front, then
            // three unchecked reads (same bit order, byte-identical).
            $br.ensure_bits(sum_wide as u8);
            (
                $br.get_bits_unchecked(of_num_bits),
                $br.get_bits_unchecked(ml_num_bits),
                $br.get_bits_unchecked(ll_num_bits),
            )
        } else {
            (
                $br.get_bits(of_num_bits),
                $br.get_bits(ml_num_bits),
                $br.get_bits(ll_num_bits),
            )
        };
        let offset = obits as u32 + of_base;
        debug_assert_ne!(offset, 0);

        Sequence {
            ll: ll_value + ll_add as u32,
            ml: ml_value + ml_add as u32,
            of: offset,
        }
    }};
}

/// Branchy offset/repcode resolution + ml/ll reads, parameterised by the
/// bit-reader method `$rd` (`get_bits` = demand-refilled, or
/// `get_bits_unchecked` = no per-read refill check after a prior
/// `ensure_bits`). The control flow mirrors upstream `ZSTD_decodeSequence`:
/// the offset extra-bit read is folded INTO the `ofBits>1 / ==0 / ==1`
/// branches and the repcode history rotation is resolved inline. Expands to
/// `(ll, ml, actual_offset)`; rotates `$hist` in place.
macro_rules! cshape_resolve {
    (
        $rd:ident, $ll_base:expr, $ml_base:expr, $of_base:expr,
        $ll_bits:expr, $ml_bits:expr, $of_bits:expr, $br:expr, $hist:expr
    ) => {{
        let ll_base = $ll_base;
        let of_base = $of_base;
        let ml_bits = $ml_bits;
        let ll_bits = $ll_bits;
        let of_bits = $of_bits;

        let actual_offset: u32 = if of_bits > 1 {
            // Real offset: read ofBits, no repcode. offBase = of_base + raw >= 4.
            let raw = $br.$rd(of_bits) as u32;
            let resolved = (of_base + raw).wrapping_sub(3);
            $hist[2] = $hist[1];
            $hist[1] = $hist[0];
            $hist[0] = resolved;
            resolved
        } else {
            let ll0 = ll_base == 0;
            if of_bits == 0 {
                // Repcode 0 (most common): no offset bits consumed.
                let idx = usize::from(ll0);
                let resolved = $hist[idx];
                $hist[1] = $hist[idx ^ 1];
                $hist[0] = resolved;
                resolved
            } else {
                // ofBits == 1: one bit selects among rep1..rep3. The upstream
                // repcode base for this arm is 1 (our of_base is 2 here).
                let bit = $br.$rd(1) as u32;
                let off_code = 1 + u32::from(ll0) + bit; // in {1,2,3}
                let mut temp = if off_code == 3 {
                    $hist[0].wrapping_sub(1)
                } else {
                    $hist[off_code as usize]
                };
                // 0 is not a valid offset: force corruption to surface downstream
                // (upstream `temp -= !temp`; our executor rejects offset 0).
                temp = temp.wrapping_sub(u32::from(temp == 0));
                if off_code != 1 {
                    $hist[2] = $hist[1];
                }
                $hist[1] = $hist[0];
                $hist[0] = temp;
                temp
            }
        };
        debug_assert_ne!(actual_offset, 0);

        // === Match length + literal length extra bits ===
        let ml = $ml_base
            + if ml_bits > 0 {
                $br.$rd(ml_bits) as u32
            } else {
                0
            };
        let ll = ll_base
            + if ll_bits > 0 {
                $br.$rd(ll_bits) as u32
            } else {
                0
            };

        (ll, ml, actual_offset)
    }};
}

/// Fused decode + offset-resolution for one sequence (upstream zstd shape).
///
/// Mirrors upstream zstd `ZSTD_decodeSequence` (zstd_decompress_block.c:1228-1346)
/// branch-for-branch on the 64-bit path (see [`cshape_resolve`]), with our
/// optimisation woven back in: the common `total <= 56` case does ONE
/// `ensure_bits` up front, then all of/ml/ll reads go through
/// `get_bits_unchecked` (no per-field refill branch). Only the rare
/// wide-offset case (`total > 56`) falls back to demand-refilled `get_bits`.
/// This keeps the winning branchy offset/repcode shape while reclaiming the
/// single-refill efficiency the old PEXT-triple path had.
///
/// Our offset table stores `base_value = 1 << ofCode`, so `of_base + raw` is the
/// offBase domain (1/2/3 = repcodes, >=4 = real offset + 3). The arithmetic here
/// converts to the real-offset domain that the executor consumes and that
/// `offset_hist` records (verified equal to `do_offset_history` across its full
/// test matrix). Expands to `(ll, ml, actual_offset)`; rotates `$hist` in place.
/// The `total > 56` arm of [`decode_seq_fused_cshape`], out of line.
///
/// One `ensure_bits` cannot cover a sequence whose three fields ask for more
/// bits than the reader holds, so that case re-checks per read. Reaching it
/// needs a wide offset: with `ll` and `ml` capped at 16 bits each, `of` must
/// ask for more than 24, which takes a window above 16 MB. Inline it was a
/// second full copy of the resolve logic sitting beside the hot one, in a
/// function already carrying seven times the reference's code, for a branch
/// most frames never take.
#[cold]
#[inline(never)]
// The decoded state this resolves from is eight independent scalars; bundling
// them into a struct to satisfy the lint would put them through memory on a
// path whose whole point is to keep the hot one clear.
#[allow(clippy::too_many_arguments)]
fn resolve_sequence_wide<K: crate::cpu_kernel::CpuKernel>(
    br: &mut crate::bit_io::BitReaderReversed<'_, K>,
    ll_base: u32,
    ml_base: u32,
    of_base: u32,
    ll_bits: u8,
    ml_bits: u8,
    of_bits: u8,
    hist: &mut [u32; 3],
) -> (u32, u32, u32) {
    cshape_resolve!(
        get_bits, ll_base, ml_base, of_base, ll_bits, ml_bits, of_bits, br, hist
    )
}

macro_rules! decode_seq_fused_cshape {
    ($ll_dec:expr, $ml_dec:expr, $of_dec:expr, $br:expr, $hist:expr,
     $update_bits:expr, $advance_states:expr) => {{
        let ll_state = $ll_dec.state;
        let ml_state = $ml_dec.state;
        let of_state = $of_dec.state;

        let ll_base = ll_state.base_value;
        let ml_base = ml_state.base_value;
        let of_base = of_state.base_value;
        let ll_bits = ll_state.num_additional_bits;
        let ml_bits = ml_state.num_additional_bits;
        let of_bits = of_state.num_additional_bits;

        debug_assert!(of_bits <= MAX_OFFSET_CODE);

        // total = exact bits consumed by this sequence in every arm (rep-0
        // reads 0 offset bits so total = ml+ll; rep-1 reads 1; real reads ofBits).
        let total = u16::from(of_bits) + u16::from(ml_bits) + u16::from(ll_bits);
        // The state advance belongs to the decode, as it does upstream
        // (`ZSTD_decodeSequence` ends by advancing all three unless this is the
        // last sequence), and it is here so that ONE refill check covers this
        // sequence's values AND the transition bits that follow them. Asking
        // separately, as two `ensure_bits` calls, was the loop's hottest line.
        let update_bits = u16::from($update_bits);
        if total + update_bits <= 56 {
            $br.ensure_bits((total + update_bits) as u8);
            let resolved = cshape_resolve!(
                get_bits_unchecked,
                ll_base,
                ml_base,
                of_base,
                ll_bits,
                ml_bits,
                of_bits,
                $br,
                $hist
            );
            if $advance_states {
                $ll_dec.update_state_fast($br);
                $ml_dec.update_state_fast($br);
                $of_dec.update_state_fast($br);
            }
            resolved
        } else {
            let resolved = if total <= 56 {
                $br.ensure_bits(total as u8);
                cshape_resolve!(
                    get_bits_unchecked,
                    ll_base,
                    ml_base,
                    of_base,
                    ll_bits,
                    ml_bits,
                    of_bits,
                    $br,
                    $hist
                )
            } else {
                resolve_sequence_wide(
                    $br, ll_base, ml_base, of_base, ll_bits, ml_bits, of_bits, $hist,
                )
            };
            if $advance_states {
                $br.ensure_bits($update_bits);
                $ll_dec.update_state_fast($br);
                $ml_dec.update_state_fast($br);
                $of_dec.update_state_fast($br);
            }
            resolved
        }
    }};
}

/// Textual expansion of per-sequence execute. Fast path: the inlined AVX2
/// match-copy macro [`exec_sequence_avx2_inline`]. Cold path: legacy
/// try_push + repeat_lookahead_prefetched. Expands as a statement-block
/// returning `Result<(), DecompressBlockError>` so the caller can `?`
/// or branch on it as needed.
/// Where the block's output stands, carried in locals for the length of the
/// sequence loop.
///
/// Upstream hoists `op`, `oend`, `litPtr`, `prefixStart` and the rest out of its
/// context before the loop and touches none of them through it
/// (`zstd_decompress_block.c:1620-1670`); its per-sequence gates are then
/// comparisons between registers. Ours asked the buffer instead, and a question
/// asked through `&mut` after a write is a reload the optimiser cannot hoist.
/// The cold paths still own the buffer, so they publish this cursor before
/// running and take it back afterwards.
struct OutCursor {
    /// Start of the linear output. Null when the backend has no inline path,
    /// where it is never read.
    base: *mut u8,
    /// Write position, the backend's `tail`.
    op: usize,
    /// Where writes must stop: the backend's `cap`.
    cap: usize,
    /// `cap` less the wildcopy overshoot, so the per-sequence question "does
    /// this fit, overshoot included" is one comparison against a value the
    /// block computed once. Upstream carries the same thing as a pointer
    /// (`oend_w = oend - WILDCOPY_OVERLENGTH`).
    cap_w: usize,
    /// What `op` is measured against to get the live length: the loop advances
    /// one cursor rather than two, since this does not move while a block
    /// decodes.
    ///
    /// It is a VIRTUAL coordinate, not a position in the buffer. A wrapped
    /// `RingBuffer` has a live length larger than its physical `tail` (it spans
    /// the segment above `head` and the one below `tail`), so this is below
    /// zero there and both it and the subtraction that reads it back are
    /// modular. The buffer's own `head` would not do: `op - head` is not the
    /// live length once the ring wraps.
    live_base: core::num::Wrapping<usize>,
}

impl OutCursor {
    /// Take the buffer's position into locals.
    #[inline(always)]
    fn capture<B: BufferBackend>(buffer: &mut DecodeBuffer<B>) -> Self {
        let live = buffer.len();
        let backend = buffer.buffer_mut();
        let base = if B::SUPPORTS_INLINE_SEQUENCE_EXEC {
            // SAFETY: the const says this backend is linear and overrides it.
            unsafe { backend.inline_exec_base_ptr() }
        } else {
            core::ptr::null_mut()
        };
        let op = backend.tail();
        // The write limit, not the raw capacity: it carries the per-block
        // output ceiling, which a carried cursor cannot ask the backend about
        // once it is ahead of it.
        let cap = backend.inline_write_limit();
        Self {
            base,
            op,
            cap,
            // Saturating is the meaning here, not a masked bound: an output
            // shorter than the overshoot leaves no room for an overshooting
            // write at all, and an end of zero says exactly that, sending every
            // sequence to the exact copier. This is a value computed once per
            // block, not the per-sequence gate; the gate is the comparison
            // against it.
            cap_w: cap.saturating_sub(MAX_WILDCOPY_OVERSHOOT),
            live_base: core::num::Wrapping(op) - core::num::Wrapping(live),
        }
    }

    /// Bytes the block can reach back into, which is what a match offset is
    /// checked against.
    #[inline(always)]
    fn live(&self) -> usize {
        (core::num::Wrapping(self.op) - self.live_base).0
    }

    /// Hand the position back to the buffer, before anything that reads or
    /// writes through it.
    #[inline(always)]
    fn publish<B: BufferBackend>(&self, buffer: &mut DecodeBuffer<B>) {
        if B::SUPPORTS_INLINE_SEQUENCE_EXEC {
            // SAFETY: every byte below `op` was written by the copies this
            // cursor tracked, and `op <= cap` held at each of them.
            unsafe { buffer.buffer_mut().inline_exec_commit(self.op) };
        }
    }
}

macro_rules! execute_one_body {
    (
        $cur:expr,
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
        // Labeled-block expansion — every early exit is
        // `break 'exec_inner Err(...)`, no closure, no `?` operator,
        // so the macro body inlines into the caller with zero CALL
        // boundary even at -Copt-level=0.
        let _result: Result<(), DecompressBlockError> = 'exec_inner: {
            let seq_ll_v: u32 = $seq_ll;
            let seq_ml_v: u32 = $seq_ml;
            let resolved_offset_v: u32 = $resolved_offset;
            let literals_buffer_len_v: usize = $literals_buffer_len;
            let lit_cur_before = *$lit_cur;
            let high = match lit_cur_before
                .checked_add(seq_ll_v as usize)
                .filter(|&h| h <= literals_buffer_len_v)
            {
                Some(h) => h,
                None => {
                    break 'exec_inner Err(ExecuteSequencesError::NotEnoughBytesForSequence {
                        wanted: lit_cur_before.saturating_add(seq_ll_v as usize),
                        have: literals_buffer_len_v,
                    }
                    .into());
                }
            };
            // SAFETY: high <= literals_buffer_len_v, lit_cur_before <= high.
            let lits = unsafe { $literals_buffer.get_unchecked(lit_cur_before..high) };
            *$lit_cur = high;

            // A zero offset IS reachable on malformed input and is rejected
            // here, not asserted. The argument for asserting it was that an
            // offset entry's base value is `1 << ofCode` and the table build
            // bounds the code, so a resolved offset is at least 1; fuzzing
            // disproved it, on the lookahead arm, whose offsets come through
            // `do_offset_history` rather than the fused resolve. Without the
            // test a release build would take a zero as a match source of
            // itself and decode corrupt input to garbage instead of an error,
            // so the ~1.2 instructions per sequence it costs are the price of
            // rejecting it.
            if resolved_offset_v == 0 {
                break 'exec_inner Err(ExecuteSequencesError::ZeroOffset.into());
            }

            // The literal-source slack both inline paths need (their `copy16`
            // reads 16 bytes whatever the length, and the wildcopy regime reads
            // the length rounded up to its stride) is guaranteed for the whole
            // block by the literals decoder, which either borrows a Raw section
            // that has the room after it or copies into a buffer that does.
            // Upstream settles it the same way and at the same place
            // (zstd_decompress_block.c:275), which is why its executor has no
            // such test either.
            let inline_literals_ok = B::SUPPORTS_INLINE_SEQUENCE_EXEC;
            let offset = resolved_offset_v as usize;
            // A wrapping backend keeps its own position: the gate below and the
            // commit that normalises the wrap both read the backend's `tail`,
            // so it gets the cursor back before each sequence rather than at
            // the end of the block. Const-folded away for the linear backends,
            // which is where the carried cursor was measured.
            if !B::CURSOR_IS_BLOCK_STABLE {
                *$cur = OutCursor::capture($buffer);
            }
            // Both terms are bounded (the live output by the window cap, the
            // literal run by a block), so this cannot wrap on any target this
            // builds for, and it reads locals rather than the buffer.
            let prefix_resident = offset <= $cur.live() + lits.len();

            // `inline_exec_ok` lets a wrapping backend (RingBuffer) veto the
            // inline path when the live region is not contiguous at `tail`;
            // linear backends fold it to a capacity question.
            if prefix_resident {
                // The backend gate answers two things: a wrapping backend's
                // "does this write stay contiguous", and a linear one's
                // per-block ceiling. The ceiling is already folded into the
                // limit this cursor carries, and the cursor is ahead of the
                // backend's own length, so for a carried cursor that gate is
                // strictly weaker than the comparison below and only costs the
                // loads it takes to ask. The ring, which cannot carry a cursor,
                // still needs it.
                if inline_literals_ok
                    && (B::CURSOR_IS_BLOCK_STABLE
                        || $buffer.buffer_mut().inline_exec_ok(
                            seq_ll_v as usize,
                            seq_ml_v as usize,
                            offset,
                        ))
                {
                    // SAFETY: parent-slice provenance; offset prefix-resident.
                    let lit_src = unsafe { $literals_buffer.as_ptr().add(lit_cur_before) };
                    // Inline the AVX2 exec body at the call site (no trait-method
                    // call boundary; see `exec_sequence_avx2_inline`), addressed
                    // by the cursor this loop carries.
                    let r = exec_sequence_avx2_inline_at!(
                        $cur.base,
                        $cur.op,
                        $cur.cap,
                        $cur.cap_w,
                        lit_src,
                        seq_ll_v as usize,
                        offset,
                        seq_ml_v as usize
                    );
                    match r {
                        Ok(total) => {
                            $cur.op += total;
                            // A wrapping backend has to see the write now, so
                            // its commit can normalise the wrap before the next
                            // sequence asks the gate above.
                            if !B::CURSOR_IS_BLOCK_STABLE {
                                $cur.publish($buffer);
                            }
                            // Inline path bypasses the wrapper's output counter;
                            // keep it current for backends that read it
                            // (Ring/Flat resume + dict gate). Const-folded away
                            // for UserSliceBackend.
                            if B::INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER {
                                $buffer.advance_output_counter((seq_ll_v + seq_ml_v) as u64);
                            }
                            break 'exec_inner Ok(());
                        }
                        Err(e) => {
                            break 'exec_inner Err(DecompressBlockError::ExecuteSequencesError(e));
                        }
                    }
                }
                // Reaches past the output into the dictionary. When the whole match
                // sits inside reachable dictionary content the copy is one more
                // inline copy, the way upstream handles its extDict branch;
                // anything else is the cold path's. The gate is the DICTIONARY one:
                // the source is a separate allocation, so the output-resident bound
                // does not apply and asking for it would refuse every such match on
                // a wrapped ring.
            }

            // Everything below reads or writes through the buffer, so it gets
            // the cursor back first and this loop re-reads it afterwards.
            $cur.publish($buffer);

            // Reaches past the output into the dictionary. When the whole match
            // sits inside reachable dictionary content the copy is one more
            // inline copy, the way upstream handles its extDict branch;
            // anything else is the cold path's. The gate is the DICTIONARY one:
            // the source is a separate allocation, so the output-resident bound
            // does not apply and asking for it would refuse every such match on
            // a wrapped ring.
            if !prefix_resident
                && inline_literals_ok
                && $buffer
                    .buffer_mut()
                    .inline_exec_dict_ok(seq_ll_v as usize, seq_ml_v as usize)
                && let Some(dict_src) = $buffer.dict_match_source(
                    $dict_content,
                    seq_ll_v as usize,
                    offset,
                    seq_ml_v as usize,
                )
            {
                // SAFETY: parent-slice provenance, as above.
                let lit_src = unsafe { $literals_buffer.as_ptr().add(lit_cur_before) };
                let r = exec_sequence_avx2_dict_inline!(
                    $buffer,
                    lit_src,
                    seq_ll_v as usize,
                    dict_src,
                    seq_ml_v as usize
                );
                if r.is_ok() && B::INLINE_EXEC_MAINTAINS_OUTPUT_COUNTER {
                    $buffer.advance_output_counter((seq_ll_v + seq_ml_v) as u64);
                }
                *$cur = OutCursor::capture($buffer);
                break 'exec_inner r.map_err(DecompressBlockError::ExecuteSequencesError);
            }

            // Cold fallback.
            let cold = 'cold: {
                if let Err(e) = $buffer.try_push(lits) {
                    break 'cold Err(ExecuteSequencesError::from(e).into());
                }
                match $buffer.repeat_lookahead_prefetched(
                    $dict,
                    resolved_offset_v as usize,
                    seq_ml_v as usize,
                ) {
                    Ok(()) => Ok(()),
                    Err(e) => Err(ExecuteSequencesError::from(e).into()),
                }
            };
            *$cur = OutCursor::capture($buffer);
            cold
        };
        _result
    }};
}

/// AVX2-tier monolithic decode + execute. Outer init, RLE dispatch, FSE
/// state init, both pipeline arms, sequence-decode (via
/// `decode_one_body!`) and sequence-execute (via `execute_one_body!`)
/// all live in one function body. Macros guarantee textual expansion
/// at every callsite — no inner function boundaries.
///
/// # Safety
/// Caller must have verified that the runtime CPU advertises BMI2 + AVX2.
/// The dispatcher in `decode_and_execute_sequences` gates this on
/// `detect_cpu_kernel() == Avx2`.
#[target_feature(enable = "bmi2,avx2")]
#[allow(clippy::too_many_lines)]
// The block's inputs, each already a scalar or a borrow the caller holds.
// Grouping them into a struct to satisfy the lint would marshal a record per
// block for a function whose whole point is to keep the loop's operands in
// registers; upstream passes the same set the same way.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn decode_and_execute_sequences_avx2<'fse, B: BufferBackend>(
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
    } = init_sequence_stream::<B, Avx2Kernel>(section, source, fse, buffer, dict)?;
    // `literals_buffer` runs past the literals by the copiers' read slack, so
    // the literal count is the parameter, never the slice's length. The slack
    // is the source-read contract of the inline copiers, which read sixteen
    // bytes whatever the literal length and round the wildcopy up to its
    // stride, so assert it rather than the count alone: a sequence section
    // exists here, and the literals decoder pads whenever one does.
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

    let buffer_checkpoint = buffer.checkpoint();
    let saved_offset_hist = *offset_hist;
    // Where the output stands, in locals for the length of the loop. Published
    // back to the buffer on every path that leaves it.
    let mut cur = OutCursor::capture(buffer);

    if use_long_pipeline {
        // === Long-pipeline arm (8-deep lookahead ring) ===
        let mut prefetch_pos: usize = old_buffer_size;
        let mut shadow_hist: [u32; 3] = *offset_hist;
        let mut ring: [ExecSeq; ADVANCE] = [ExecSeq {
            ll: 0,
            ml: 0,
            actual_offset: 0,
        }; ADVANCE];

        // The probe below is bounded by the BACKEND's length, which a carried
        // cursor leaves behind for the whole block, so every source the block
        // itself produced is refused. That is deliberate, and it was measured:
        // addressing the probe through the cursor instead (128 KiB block, one
        // cold dictionary, so 281 of its 391 probes are the refused ones) cost
        // 3.0% — 9.07 s against 9.34 s over 20k decodes, ranges disjoint across
        // three interleaved pairs. A source this block wrote is at most a block
        // back and microseconds old, so warming it buys nothing while the two
        // hints and the bound cost per sequence. The probe exists for the long
        // offsets the arm is gated on, and those sit below the backend's length
        // already.
        //
        // Prefill ring with ADVANCE decoded+prefetched sequences.
        for slot in ring.iter_mut() {
            let seq = decode_one_body!(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
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

        let mut pipeline_err: Option<DecompressBlockError> = None;
        for i in ADVANCE..num_sequences {
            let seq = decode_one_body!(&mut ll_dec, &mut ml_dec, &mut of_dec, &mut br);
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
                &mut cur,
                buffer,
                dict,
                dict_content,
                literals_buffer,
                &mut lit_cur,
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
                ll_dec.update_state_fast(&mut br);
                ml_dec.update_state_fast(&mut br);
                of_dec.update_state_fast(&mut br);
            }
        }

        // Drain the remaining ADVANCE ring slots.
        if pipeline_err.is_none() {
            for k in 0..ADVANCE {
                let slot = (num_sequences + k) & ADVANCE_MASK;
                let exec_seq = ring[slot];
                let r = execute_one_body!(
                    &mut cur,
                    buffer,
                    dict,
                    dict_content,
                    literals_buffer,
                    &mut lit_cur,
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

        cur.publish(buffer);
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
            // The states advance for the NEXT sequence inside the decode, ahead
            // of executing this one, as upstream advances them at the end of
            // `ZSTD_decodeSequence` and only then calls `ZSTD_execSequence`. The
            // execute reads no bits, so the bitstream order is unchanged, and
            // the three FSE states plus the bit reader stop being live across
            // the heavy match copy.
            let (seq_ll, seq_ml, resolved_offset) = decode_seq_fused_cshape!(
                &mut ll_dec,
                &mut ml_dec,
                &mut of_dec,
                &mut br,
                &mut shadow_hist,
                max_update_bits,
                i + 1 < num_sequences
            );
            let r = execute_one_body!(
                &mut cur,
                buffer,
                dict,
                dict_content,
                literals_buffer,
                &mut lit_cur,
                literals_buffer_len,
                seq_ll,
                seq_ml,
                resolved_offset
            );
            if let Err(e) = r {
                fallback_err = Some(e);
                break;
            }
            seq_sum = seq_sum.wrapping_add(seq_ll).wrapping_add(seq_ml);
        }
        cur.publish(buffer);
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
