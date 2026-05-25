//! This module contains the decompress_literals function, used to take a
//! parsed literals header and a source and decompress it.

use super::super::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
use super::scratch::HuffmanScratch;
use crate::bit_io::BitReaderReversed;
#[cfg(target_arch = "x86_64")]
use crate::cpu_kernel::{Avx2Kernel, Bmi2Kernel, CpuKernelTag, Vbmi2Kernel};
use crate::cpu_kernel::{CpuKernel, ScalarKernel, detect_cpu_kernel};
use crate::decoding::errors::DecompressLiteralsError;
use crate::huff0::HuffmanDecoder;
use alloc::vec::Vec;

/// Decode and decompress the provided literals section into `target`, returning the number of bytes read.
/// Test-only Vec-output wrapper retained for the existing roundtrip
/// test suite, which asserts the literal byte stream lands fully
/// in a Vec. Production callers use [`decode_literals_zerocopy`].
#[cfg(test)]
pub fn decode_literals(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    match section.ls_type {
        LiteralsSectionType::Raw => {
            target.extend(&source[0..section.regenerated_size as usize]);
            Ok(section.regenerated_size)
        }
        LiteralsSectionType::RLE => {
            target.resize(target.len() + section.regenerated_size as usize, source[0]);
            Ok(1)
        }
        LiteralsSectionType::Compressed | LiteralsSectionType::Treeless => {
            let bytes_read = decompress_literals(section, scratch, source, target)?;
            Ok(bytes_read)
        }
    }
}

/// Result of [`decode_literals_zerocopy`]. For Raw sections this is a
/// borrow straight into the input — no memcpy. For RLE / HUF
/// sections it's a borrow of the scratch `literals_buffer` where the
/// data was materialised.
pub struct LiteralsView<'a> {
    /// Decoded literal bytes available for the sequence executor.
    pub data: &'a [u8],
    /// Bytes consumed from the input literals section payload
    /// (Raw: regenerated_size; HUF: header + jump + 4 streams).
    pub bytes_used: u32,
}

/// Zero-copy variant of [`decode_literals`]. For Raw literal sections
/// returns a slice straight into `source` instead of copying bytes
/// into a Vec — eliminates one memcpy + one zero-touch wave per RAW
/// literal byte on the direct-decode path. RLE / HUF paths still go
/// through `target` because they have to produce new bytes (RLE: N
/// copies of one byte; HUF: indexed burst writes).
///
/// Donor parity: `dctx->litPtr` is set to either `src` (Raw) or
/// `dctx->litBuffer` (HUF); the seq executor reads from
/// `dctx->litPtr` uniformly.
pub fn decode_literals_zerocopy<'a>(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &'a [u8],
    target: &'a mut Vec<u8>,
) -> Result<LiteralsView<'a>, DecompressLiteralsError> {
    // Snapshot `target.len()` before any decode work — the returned
    // view must point ONLY at the newly-decoded bytes, not at any
    // pre-existing tail the caller forgot to `clear()`. The current
    // in-tree callers clear before this call, but anchoring the
    // view at `base..` makes the API robust against future
    // misuse and matches donor's `dctx->litPtr` semantics (always
    // points at the current frame's literals, never carries
    // history from earlier blocks' Vecs).
    let base = target.len();
    match section.ls_type {
        LiteralsSectionType::Raw => {
            let n = section.regenerated_size as usize;
            // Bounds check: a truncated frame can claim more raw
            // literals than the source slice carries. Return a
            // structured error instead of panicking on `source[0..n]`.
            if source.len() < n {
                return Err(DecompressLiteralsError::MissingBytesForLiterals {
                    got: source.len(),
                    needed: n,
                });
            }
            // Zero-copy: borrow the payload from source. `target` is
            // left untouched — the caller passes `LiteralsView::data`
            // to the sequence executor instead.
            Ok(LiteralsView {
                data: &source[0..n],
                bytes_used: section.regenerated_size,
            })
        }
        LiteralsSectionType::RLE => {
            // RLE expands one byte to N — has to write into target.
            // Need at least one source byte (the fill byte).
            if source.is_empty() {
                return Err(DecompressLiteralsError::MissingBytesForLiterals { got: 0, needed: 1 });
            }
            target.resize(base + section.regenerated_size as usize, source[0]);
            Ok(LiteralsView {
                data: &target[base..],
                bytes_used: 1,
            })
        }
        LiteralsSectionType::Compressed | LiteralsSectionType::Treeless => {
            let bytes_used = decompress_literals(section, scratch, source, target)?;
            Ok(LiteralsView {
                data: &target[base..],
                bytes_used,
            })
        }
    }
}

/// Decompress the provided literals section and source into the provided `target`.
/// This function is used when the literals section is `Compressed` or `Treeless`
///
/// Returns the number of bytes read.
fn decompress_literals(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    // Per-block CpuKernel dispatch. detect_cpu_kernel() is OnceLock-cached
    // so the tag is resolved once per process; the match below collapses
    // to a single cmp+jmp on subsequent calls. Each arm dispatches into a
    // target_feature-wrapped outer function so the entire impl::<K> pipeline
    // executes inside the matching target_feature context — without that
    // wrapping, LLVM cannot inline target_feature'd intrinsics (e.g.
    // _bzhi_u64 inside K::mask_lower_bits) through the trait-method call
    // boundary back into the generic caller, and the inlined-intrinsic
    // win evaporates into a function-call trampoline per mask op.
    match detect_cpu_kernel() {
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Vbmi2 => unsafe {
            decompress_literals_vbmi2(section, scratch, source, target)
        },
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Avx2 => unsafe { decompress_literals_avx2(section, scratch, source, target) },
        #[cfg(target_arch = "x86_64")]
        CpuKernelTag::Bmi2 => unsafe { decompress_literals_bmi2(section, scratch, source, target) },
        _ => decompress_literals_impl::<ScalarKernel>(section, scratch, source, target),
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2,avx2")]
unsafe fn decompress_literals_avx2(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    decompress_literals_impl::<Avx2Kernel>(section, scratch, source, target)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
unsafe fn decompress_literals_bmi2(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    decompress_literals_impl::<Bmi2Kernel>(section, scratch, source, target)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512vbmi2,avx512f,avx512vl,avx512bw,bmi2,avx2")]
unsafe fn decompress_literals_vbmi2(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    decompress_literals_impl::<Vbmi2Kernel>(section, scratch, source, target)
}

fn decompress_literals_impl<K: CpuKernel>(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    use DecompressLiteralsError as err;

    let compressed_size = section.compressed_size.ok_or(err::MissingCompressedSize)? as usize;
    let num_streams = section.num_streams.ok_or(err::MissingNumStreams)?;
    let base = target.len();
    let regen = section.regenerated_size as usize;

    target.reserve(regen);
    let source = &source[0..compressed_size];
    let mut bytes_read = 0;

    match section.ls_type {
        LiteralsSectionType::Compressed => {
            //read Huffman tree description
            bytes_read += scratch.table.build_decoder(source)?;
            vprintln!("Built huffman table using {} bytes", bytes_read);
        }
        LiteralsSectionType::Treeless if scratch.table.max_num_bits == 0 => {
            return Err(err::UninitializedHuffmanTable);
        }

        _ => { /* nothing to do, huffman tree has been provided by previous block */ }
    }

    let source = &source[bytes_read as usize..];

    if num_streams == 4 {
        //build jumptable
        if source.len() < 6 {
            return Err(err::MissingBytesForJumpHeader { got: source.len() });
        }
        let jump1 = source[0] as usize + ((source[1] as usize) << 8);
        let jump2 = jump1 + source[2] as usize + ((source[3] as usize) << 8);
        let jump3 = jump2 + source[4] as usize + ((source[5] as usize) << 8);
        bytes_read += 6;
        let source = &source[6..];

        if source.len() < jump3 {
            return Err(err::MissingBytesForLiterals {
                got: source.len(),
                needed: jump3,
            });
        }

        //decode 4 streams with interleaved operations to hide memory latency
        let streams: [&[u8]; 4] = [
            &source[..jump1],
            &source[jump1..jump2],
            &source[jump2..jump3],
            &source[jump3..],
        ];

        let mut decoders: [HuffmanDecoder<'_>; 4] = [
            HuffmanDecoder::new(&scratch.table),
            HuffmanDecoder::new(&scratch.table),
            HuffmanDecoder::new(&scratch.table),
            HuffmanDecoder::new(&scratch.table),
        ];
        let mut brs: [BitReaderReversed<'_, K>; 4] = [
            BitReaderReversed::<K>::new(streams[0]),
            BitReaderReversed::<K>::new(streams[1]),
            BitReaderReversed::<K>::new(streams[2]),
            BitReaderReversed::<K>::new(streams[3]),
        ];

        // Initialize all 4 streams: skip padding and set initial state
        for i in 0..4 {
            let mut skipped_bits = 0;
            loop {
                let val = brs[i].get_bits(1);
                skipped_bits += 1;
                if val == 1 || skipped_bits > 8 {
                    break;
                }
            }
            if skipped_bits > 8 {
                return Err(DecompressLiteralsError::ExtraPadding { skipped_bits });
            }
            decoders[i].init_state(&mut brs[i]);
        }

        let max_bits = scratch.table.max_num_bits as isize;

        // RFC 8878 §3.1.1.3.2: first 3 streams produce ceil(regen_size/4)
        // symbols each, 4th produces the remainder. Pre-allocate target and
        // decode directly into slices — no temporary Vec allocations.
        let seg = regen.div_ceil(4);

        target.resize(base + regen, 0);
        // Clamp every start/end into [base, base+regen] so cursors can
        // never index past the pre-allocated region, even with corrupted
        // frame headers that produce small regen (where N*seg > regen).
        let limit = base + regen;
        let starts: [usize; 4] = [
            base,
            (base + seg).min(limit),
            (base + 2 * seg).min(limit),
            (base + 3 * seg).min(limit),
        ];
        let ends: [usize; 4] = [starts[1], starts[2], starts[3], limit];
        let mut cursors = starts;

        // Donor-parity 4-stream HUF decode. `bits[s]` is the fused
        // state+stream+sentinel u64 register (see `run_4stream_burst_loop`).
        // Each iter decodes `symbols_per_burst` symbols × 4 streams,
        // then reloads all 4 stream registers via `ip[s] -= nb_bytes;
        // MEM_read64(ip[s]) | 1`.
        let max_num_bits = scratch.table.max_num_bits;
        // Safety constraint per donor `HUF_decompress4X1_usingDTable_internal_fast_c_loop`:
        // before each `bits[s] >> table_shift` read, the sentinel-bit position
        // must be strictly below bit `64 - max_num_bits` (i.e. outside the top
        // `max_num_bits` read region). After `s` shifts the sentinel is at bit
        // `padding_skip + s*max_num_bits`. The N-th read happens after (N-1)
        // shifts, so the inclusive bound is
        //   padding_skip + (N-1)*max_num_bits < 64 - max_num_bits
        // i.e.
        //   padding_skip + N*max_num_bits <= 63
        // Solving for N with padding_skip ≤ 8:
        //   N <= (63 - 8) / max_num_bits = 55 / max_num_bits
        // (Letter `s` is used here for shift-count to avoid colliding with
        // the surrounding generic parameter `K: CpuKernel`.)
        // For max=11: 5 symbols (donor parity — was 4 with the old off-by-one
        // formula). For max=8: 6 symbols. For max=4: 13.
        let symbols_per_burst: usize = (63 - 8) / max_num_bits as usize;
        let burst_bits = (symbols_per_burst * max_num_bits as usize) as u8;
        let table_shift = (64 - max_num_bits) as u32;
        let packed = scratch.table.packed_decode.as_slice();

        // Lockstep cursor invariant: every burst iter advances all 4
        // cursors by `symbols_per_burst` in step, so `cursors[0]`
        // tracks progress for all four streams. `cursor_exit_olimit
        // = starts[0] + min(seg_len[i])` is the cursor value at which
        // the lagging segment runs out — donor parity with
        // `huf_decompress.c` `olimit`-style single-pointer bound.
        let min_seg_len = (ends[0] - starts[0])
            .min(ends[1] - starts[1])
            .min(ends[2] - starts[2])
            .min(ends[3] - starts[3]);
        // `burst_eligible` is a load-bearing safety gate against
        // adversarial frame headers. If `min_seg_len < symbols_per_burst`
        // (small `regenerated_size` paired with large compressed
        // streams, forging a 4-stream HUF block where
        // `seg = div_ceil(regen, 4) < symbols_per_burst`) then
        // `cursor_burst_ceil` saturates to 0 and `cursors[0] <= 0`
        // is trivially true on entry, admitting a burst whose inner
        // loop would advance `cursors[i]` past `ends[i]` and panic
        // on the `target[cursors[i]]` write. Requiring
        // `min_seg_len >= symbols_per_burst` up front means the
        // burst only runs when a full burst fits inside EVERY
        // segment; the drain phase outside `run_4stream_burst_loop`
        // handles the small-`min_seg_len` case via single-symbol
        // per-stream decode.
        let burst_eligible = symbols_per_burst >= 1 && min_seg_len >= symbols_per_burst;
        let cursor_burst_ceil = (starts[0] + min_seg_len).saturating_sub(symbols_per_burst);

        let bounds = LoopBounds {
            symbols_per_burst,
            burst_bits,
            table_shift,
            cursor_burst_ceil,
            burst_eligible,
        };

        // Burst is identical across all kernels (donor parity: reads
        // `packed[idx]` u16 directly + `MEM_read64` reload pattern,
        // no SIMD intrinsics needed). Single un-genericised call.
        //
        // SAFETY: caller guarantees `brs[s].source` is the same as the
        // stream slice each decoder was initialised against; the
        // upfront `target.resize(base + regen, 0)` covers all cursor
        // writes; `packed` length matches `1 << max_num_bits` by
        // `HuffmanTable::build_decoder`'s `resize`.
        unsafe {
            run_4stream_burst_loop(
                &mut decoders,
                &mut brs,
                target,
                packed,
                &mut cursors,
                &bounds,
            );
        }

        // Drain remaining symbols from each stream, bounded by segment end
        for i in 0..4 {
            while brs[i].bits_remaining() > -max_bits && cursors[i] < ends[i] {
                target[cursors[i]] = decoders[i].decode_symbol_and_advance(&mut brs[i]);
                cursors[i] += 1;
            }
            if brs[i].bits_remaining() != -max_bits {
                target.truncate(base);
                return Err(DecompressLiteralsError::BitstreamReadMismatch {
                    read_til: brs[i].bits_remaining(),
                    expected: -max_bits,
                });
            }
        }

        // Verify total decoded count matches expected regenerated size.
        // Return error immediately rather than deferring to the downstream check.
        let decoded: usize = cursors.iter().zip(starts.iter()).map(|(c, s)| c - s).sum();
        if decoded != regen {
            // Truncate to base: segmented layout means partial decode left
            // bytes scattered across segments, so only base is a clean boundary.
            target.truncate(base);
            return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
                decoded,
                expected: regen,
            });
        }

        bytes_read += source.len() as u32;
    } else {
        //just decode the one stream
        assert!(num_streams == 1);
        let mut decoder = HuffmanDecoder::new(&scratch.table);
        let mut br = BitReaderReversed::<K>::new(source);
        let mut skipped_bits = 0;
        loop {
            let val = br.get_bits(1);
            skipped_bits += 1;
            if val == 1 || skipped_bits > 8 {
                break;
            }
        }
        if skipped_bits > 8 {
            //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
            return Err(DecompressLiteralsError::ExtraPadding { skipped_bits });
        }
        decoder.init_state(&mut br);
        while br.bits_remaining() > -(scratch.table.max_num_bits as isize) {
            target.push(decoder.decode_symbol_and_advance(&mut br));
        }
        let expected = -(scratch.table.max_num_bits as isize);
        if br.bits_remaining() != expected {
            target.truncate(base);
            return Err(DecompressLiteralsError::BitstreamReadMismatch {
                read_til: br.bits_remaining(),
                expected,
            });
        }
        bytes_read += source.len() as u32;
    }

    if target.len() != base + regen {
        let decoded = target.len() - base;
        target.truncate(base);
        return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
            decoded,
            expected: regen,
        });
    }

    Ok(bytes_read)
}

/// Loop-invariant constants for [`run_4stream_burst_loop`]. Derived
/// once per `decompress_literals` call; `Copy` so the burst can
/// destructure `*bounds` for register-resident reads.
#[derive(Copy, Clone)]
struct LoopBounds {
    symbols_per_burst: usize,
    burst_bits: u8,
    table_shift: u32,
    cursor_burst_ceil: usize,
    /// Set iff a full burst (`symbols_per_burst` symbols per stream)
    /// can fit in the lagging segment. When false the burst is
    /// hard-disabled and the drain phase outside the burst loop
    /// decodes ALL symbols via the single-symbol path. Setup-site
    /// safety rationale: adversarial / small-regen DoS guard.
    burst_eligible: bool,
}

/// Donor-parity 4-stream HUF decode burst loop. Single code path —
/// no kernel dispatch, no SIMD-fallback hybrid. Mirrors
/// `huf_decompress.c:HUF_decompress4X1_usingDTable_internal_fast_c_loop`:
/// each outer iter decodes `symbols_per_burst` symbols × 4 streams,
/// then reloads all 4 stream registers from raw source bytes via the
/// `ctz(bits[s])` → `ip[s] -= nb_bytes` → `MEM_read64(ip[s])` pattern.
///
/// State + unconsumed stream + sentinel are fused into one u64
/// per stream (`bits[s]`). The decoder's separate `state` field is
/// reconstructed once at burst exit for the drain phase below.
///
/// # Safety
///
/// All four decoders must share the same table (holds by construction —
/// built from `&scratch.table`). `target.len() >= base + regen`. Each
/// `brs[s].source` must be the slice the corresponding decoder was
/// initialised against.
#[inline(always)]
unsafe fn run_4stream_burst_loop<K: CpuKernel>(
    decoders: &mut [HuffmanDecoder<'_>; 4],
    brs: &mut [BitReaderReversed<'_, K>; 4],
    target: &mut [u8],
    packed: &[u16],
    cursors: &mut [usize; 4],
    bounds: &LoopBounds,
) {
    let LoopBounds {
        symbols_per_burst,
        burst_bits,
        table_shift,
        cursor_burst_ceil,
        burst_eligible,
    } = *bounds;
    let max_num_bits = (64 - table_shift) as u8;

    // Skip burst entirely if min_seg_len < symbols_per_burst — drain
    // (the single-symbol tail outside this function) handles ALL
    // symbols. See the `burst_eligible` doc on `LoopBounds`.
    if !burst_eligible {
        return;
    }

    // Donor-parity burst loop. `bits[s]` is the unified u64 register
    // that fuses state + unconsumed stream + sentinel:
    //   bits 63..(64-max_num_bits): current state (next index into `packed`)
    //   below:                       upcoming stream bits, top-aligned
    //   bottom:                      sentinel `1`, position grows upward
    //                                with each consumed bit
    //
    // The encoder side of HUF writes the bitstream backward such that
    // at every byte boundary the top `max_num_bits` of unconsumed
    // stream = current state. So state is implicit in `bits[s]`; we
    // do NOT carry a separate `decoder.state` inside the burst — it
    // is reconstructed via `bits[s] >> table_shift` at the burst exit
    // and written back to `decoders[s].state` for the drain phase.
    //
    // Composition matches donor `HUF_DecompressFastArgs_init` and
    // `HUF_4X1_RELOAD_STREAM` (huf_decompress.c:795-804): each iter
    // reloads `bits[s] = MEM_read64(ip[s]) | 1; bits[s] <<= nb_bits`
    // after advancing `ip[s] -= nb_bytes` (where nb_bytes/nb_bits
    // come from `ctz(bits[s])` at the end of the previous iter).
    // Initial composition exactly mirrors donor `HUF_DecompressFastArgs_init`:
    // `bits[s] = (MEM_read64(ip) | 1) << padding_skip`. Top `max_num_bits`
    // of the result is the state value implicitly (HUF stream encoding
    // ensures the top max bits of unconsumed stream at any consumption
    // point = current state machine state), so we don't inject
    // `decoders[s].state` explicitly here — the bit pattern already
    // carries it.
    //
    // `padding_skip = brs[s].bits_consumed - max_num_bits`: `init_state`
    // pre-consumed `max_num_bits` for `decoders[s].state`, so
    // `brs[s].bits_consumed = padding_skip + max_num_bits`. Donor leaves
    // state implicit; we reverse our pre-consumption by shifting only
    // by `padding_skip` (not by `bits_consumed`) so the top max bits
    // come from the unshifted stream-position-of-state.
    //
    // Sentinel ends up at bit `padding_skip` after the shift, so
    // `ctz(initial bits[s]) = padding_skip` and the first reload's
    // `nb_bytes = (padding_skip + K) / 8` matches donor's byte-cursor
    // advance from absolute stream position 0.
    let mut bits: [u64; 4] = [
        (brs[0].bit_container | 1) << (brs[0].bits_consumed - max_num_bits),
        (brs[1].bit_container | 1) << (brs[1].bits_consumed - max_num_bits),
        (brs[2].bit_container | 1) << (brs[2].bits_consumed - max_num_bits),
        (brs[3].bit_container | 1) << (brs[3].bits_consumed - max_num_bits),
    ];
    let mut ip: [usize; 4] = [brs[0].index, brs[1].index, brs[2].index, brs[3].index];
    // Sub-byte phase of the consumption point in the current 8-byte
    // window of `brs[s]`. Initial value mirrors the post-init reader
    // state: drain compatibility wants `bits_consumed = nb_bits + max_num_bits`,
    // so `nb_bits_last[s] = brs[s].bits_consumed - max_num_bits` for the
    // pre-reload writeback path (no burst iter ran). After the first
    // reload `nb_bits_last[s] = ctz & 7` (sub-byte phase of donor's
    // `MEM_read64 + shift`).
    let mut nb_bits_last: [u8; 4] = [
        brs[0].bits_consumed - max_num_bits,
        brs[1].bits_consumed - max_num_bits,
        brs[2].bits_consumed - max_num_bits,
        brs[3].bits_consumed - max_num_bits,
    ];

    // Donor `iiters` safety budget. Worst-case `nb_bytes` per iter is
    // `floor(ctz_max / 8)` where `ctz_max = pad_max + burst_bits`,
    // since at the first iter the sentinel starts at `padding_skip
    // ∈ [1, 8]` and on subsequent iters at `nb_bits ∈ [0, 7]` set by
    // the previous reload's `(MEM_read64 | 1) << nb_bits`. Taking
    // `pad_max = 8` covers both regimes — without the `+8` slack,
    // burst configurations where `burst_bits` is a multiple of 8
    // (e.g. max=8 -> burst_bits=48) accept a `min_ip` that
    // `nb_bytes` then overruns, underflowing `ip[s] -= nb_bytes`.
    // The check below ensures `ip[s] >= bytes_per_iter_upper` for
    // every stream before entering an iter, so per-iter `ip[s] -=
    // nb_bytes` plus the subsequent `source[ip[s]..ip[s]+8]` read
    // both stay in-bounds without per-stream conditionals.
    let bytes_per_iter_upper = (8 + burst_bits as usize) / 8;
    let mut any_iter = false;

    while cursors[0] <= cursor_burst_ceil {
        let min_ip = ip[0].min(ip[1]).min(ip[2]).min(ip[3]);
        if min_ip < bytes_per_iter_upper {
            break;
        }
        any_iter = true;

        // Inner: decode `symbols_per_burst` symbols × 4 streams.
        //
        // SAFETY for `packed.get_unchecked(idx)`:
        //   `idx = (bits[s] >> table_shift) as usize` with
        //   `table_shift = 64 - max_num_bits` lands in
        //   `[0, 1 << max_num_bits)`. `packed.len() == 1 << max_num_bits`
        //   by `HuffmanTable::build_decoder`'s upfront `resize`.
        //
        // SAFETY for `target.get_unchecked_mut(cursors[s])`:
        //   The outer-loop gate `cursors[0] <= cursor_burst_ceil`
        //   gives `cursors[0] + symbols_per_burst <= cursor_burst_ceil
        //   + symbols_per_burst = starts[0] + min_seg_len`. By lockstep
        //   advance, `cursors[s] - starts[s] == cursors[0] - starts[0]`
        //   for all `s`, so `cursors[s] + symbols_per_burst - 1 <
        //   starts[s] + min_seg_len <= ends[s] <= target.len()` —
        //   every write in this iter (max index `cursors[s] +
        //   symbols_per_burst - 1`) is strictly in-bounds.
        debug_assert!(cursors[0] + symbols_per_burst <= cursor_burst_ceil + symbols_per_burst);
        for _ in 0..symbols_per_burst {
            let idx0 = (bits[0] >> table_shift) as usize;
            let entry0 = unsafe { *packed.get_unchecked(idx0) };
            unsafe { *target.get_unchecked_mut(cursors[0]) = (entry0 & 0xFF) as u8 };
            cursors[0] += 1;
            bits[0] <<= (entry0 >> 8) & 0xFF;

            let idx1 = (bits[1] >> table_shift) as usize;
            let entry1 = unsafe { *packed.get_unchecked(idx1) };
            unsafe { *target.get_unchecked_mut(cursors[1]) = (entry1 & 0xFF) as u8 };
            cursors[1] += 1;
            bits[1] <<= (entry1 >> 8) & 0xFF;

            let idx2 = (bits[2] >> table_shift) as usize;
            let entry2 = unsafe { *packed.get_unchecked(idx2) };
            unsafe { *target.get_unchecked_mut(cursors[2]) = (entry2 & 0xFF) as u8 };
            cursors[2] += 1;
            bits[2] <<= (entry2 >> 8) & 0xFF;

            let idx3 = (bits[3] >> table_shift) as usize;
            let entry3 = unsafe { *packed.get_unchecked(idx3) };
            unsafe { *target.get_unchecked_mut(cursors[3]) = (entry3 & 0xFF) as u8 };
            cursors[3] += 1;
            bits[3] <<= (entry3 >> 8) & 0xFF;
        }

        // Reload all 4 streams (donor `HUF_4X1_RELOAD_STREAM`).
        //
        // SAFETY:
        //   * `ip[s] - nb_bytes >= 0`: the `min_ip >= bytes_per_iter_upper`
        //     gate at outer-loop entry guarantees `nb_bytes <= bytes_per_iter_upper`
        //     (where `nb_bytes = ctz(bits[s]) >> 3` and `ctz <= padding_skip
        //     + burst_bits <= 8 + burst_bits`, the bound `bytes_per_iter_upper`
        //     pre-computes).
        //   * `ip[s] + 8 <= source.len()`: `BitReaderReversed::new()`
        //     starts with `bits_consumed = 64`, so the very first
        //     `get_bits(1)` in the per-stream padding-skip loop
        //     above triggers `refill()`. For `source.len() >= 8` that
        //     fast-path establishes `brs[s].index = source.len() - 8`;
        //     `init_state`'s subsequent `get_bits(max_num_bits)`
        //     stays inside the same 8-byte window without another
        //     refill (only `bits_consumed` advances). The
        //     `refill_slow` path used for shorter streams leaves
        //     `index = 0` (with the partial bytes left-shifted into
        //     `bit_container`), making `min_ip = 0 <
        //     bytes_per_iter_upper` so the burst loop exits via
        //     `any_iter = false` BEFORE reaching this reload (the
        //     writeback below is unreachable on `source.len() < 8`).
        //     Within the loop, `ip[s]` only decreases via the line
        //     above this comment, preserving the upper bound.
        for s in 0..4 {
            let ctz = bits[s].trailing_zeros();
            let nb_bytes = (ctz >> 3) as usize;
            let nb_bits = (ctz & 7) as u8;
            ip[s] -= nb_bytes;
            let new_window = u64::from_le_bytes(unsafe {
                brs[s]
                    .source
                    .get_unchecked(ip[s]..ip[s] + 8)
                    .try_into()
                    .unwrap_unchecked()
            });
            // Donor `HUF_4X1_RELOAD_STREAM` order: `(MEM_read64 | 1) << nb_bits`,
            // NOT `(MEM_read64 << nb_bits) | 1`. The two are NOT equivalent —
            // the former puts the sentinel at bit `nb_bits` (so `ctz` of the
            // post-reload register accumulates the sub-byte phase into the
            // NEXT reload's `ctz`), the latter resets the sentinel to bit 0
            // and loses the phase between reloads.
            bits[s] = (new_window | 1) << nb_bits;
            nb_bits_last[s] = nb_bits;
        }
    }

    // No iter ran → nothing changed in `brs[s]` / `decoders[s]`; the
    // drain phase below picks up from the post-`init_state` reader.
    if !any_iter {
        return;
    }

    // Write back to `brs[s]` + `decoders[s].state` so the drain phase
    // (single-symbol `decode_symbol_and_advance`) picks up where the
    // burst stopped. The burst's final `bits[s]` is post-reload
    // (`= (new_window << nb_bits) | 1`), and `nb_bits_last[s]` holds
    // the sub-byte phase used in that reload. Drain's read frontier
    // sits at `nb_bits_last + max_num_bits` bits into the topmost
    // window byte: `nb_bits_last` of padding-skip already aligned by
    // the burst's reload shift, plus `max_num_bits` for the state we
    // just extracted to `decoders[s].state`.
    for s in 0..4 {
        brs[s].index = ip[s];
        brs[s].bit_container = u64::from_le_bytes(unsafe {
            brs[s]
                .source
                .get_unchecked(ip[s]..ip[s] + 8)
                .try_into()
                .unwrap_unchecked()
        });
        brs[s].bits_consumed = nb_bits_last[s] + max_num_bits;
        decoders[s].state = bits[s] >> table_shift;
    }
}

#[cfg(test)]
mod zerocopy_robustness_tests {
    //! Regression coverage for `decode_literals_zerocopy` on
    //! truncated / corrupt payloads: every branch must return a
    //! structured error instead of panicking on out-of-bounds
    //! slice indexing. Hit each `*[..n]` / `*[0]` index in the
    //! function with a payload one byte short of what the header
    //! declares.
    //
    // Tests live in a separate module so the broader `burst_gate_tests`
    // module's helpers don't have to depend on truncated-input
    // builders.
    use super::{LiteralsView, decode_literals_zerocopy};
    use crate::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
    use crate::decoding::scratch::HuffmanScratch;
    use crate::huff0::HuffmanTable;
    use alloc::vec::Vec;

    fn raw_section(regen: u32) -> LiteralsSection {
        LiteralsSection {
            ls_type: LiteralsSectionType::Raw,
            regenerated_size: regen,
            compressed_size: None,
            num_streams: None,
        }
    }

    fn rle_section(regen: u32) -> LiteralsSection {
        LiteralsSection {
            ls_type: LiteralsSectionType::RLE,
            regenerated_size: regen,
            compressed_size: None,
            num_streams: None,
        }
    }

    fn fresh_scratch() -> HuffmanScratch {
        HuffmanScratch {
            table: HuffmanTable::new(),
        }
    }

    #[test]
    fn raw_truncated_source_returns_error_no_panic() {
        // Header claims 10 raw literal bytes, source carries 3.
        // Indexing `source[0..10]` would panic; the fix must turn
        // it into a structured DecompressLiteralsError.
        let section = raw_section(10);
        let source: [u8; 3] = [1, 2, 3];
        let mut target: Vec<u8> = Vec::new();
        let mut scratch = fresh_scratch();
        let result = decode_literals_zerocopy(&section, &mut scratch, &source, &mut target);
        assert!(
            result.is_err(),
            "truncated raw source must error, not panic; got {:?}",
            result.map(|_| ())
        );
    }

    #[test]
    fn rle_empty_source_returns_error_no_panic() {
        // RLE section needs at least one source byte (the fill byte).
        // Indexing `source[0]` on an empty slice would panic.
        let section = rle_section(10);
        let source: [u8; 0] = [];
        let mut target: Vec<u8> = Vec::new();
        let mut scratch = fresh_scratch();
        let result = decode_literals_zerocopy(&section, &mut scratch, &source, &mut target);
        assert!(
            result.is_err(),
            "empty RLE source must error, not panic; got {:?}",
            result.map(|_| ())
        );
    }

    #[test]
    fn rle_view_excludes_pre_existing_target_bytes() {
        // Even if the caller forgot to clear `target`, the returned
        // LiteralsView::data must point only at the bytes this call
        // produced. The API hardening (`&target[base..]`) is what
        // makes this hold.
        let mut target: Vec<u8> = Vec::from([0xAA, 0xBB, 0xCC]);
        let section = rle_section(4);
        let source: [u8; 1] = [0x42];
        let mut scratch = fresh_scratch();
        let view = decode_literals_zerocopy(&section, &mut scratch, &source, &mut target)
            .expect("RLE with valid source must succeed");
        assert_eq!(view.data.len(), 4, "view length must match regen_size");
        assert!(
            view.data.iter().all(|&b| b == 0x42),
            "view must contain only the newly-RLE-expanded bytes, got {:?}",
            view.data
        );
        // Silence unused-warning if the compiler ever strips
        // LiteralsView fields — read bytes_used too.
        let _ = LiteralsView {
            data: view.data,
            bytes_used: view.bytes_used,
        };
    }
}

#[cfg(test)]
mod burst_gate_tests {
    //! Regression coverage for the HUF 4-stream burst-gate boundary
    //! states in `decompress_literals`:
    //!
    //!   1. `bits_consumed == max_num_bits` — lower boundary of the
    //!      burst gate, where the gate is entered with zero slack.
    //!   2. `bits_consumed + burst_bits == 64` — upper boundary, where
    //!      the burst consumes all remaining bits in the 64-bit window
    //!      without overflow.
    //!   3. SIMD-fallback → refill → burst re-entry — outer loop falls
    //!      back to the SIMD 4-symbol path, a `BitReaderReversed`
    //!      refill occurs, the next iteration re-enters the burst path
    //!      once `bits_consumed` grows back into burst range.
    //!
    //! Each named test pins an input shape chosen to drive the gate
    //! through the corresponding regime — short skewed input for the
    //! initial-entry lower-bound, long mid-cardinality streams for
    //! many upper-bound brushes, multi-segment input for repeated
    //! SIMD↔burst transitions. The sweep test covers the gate in
    //! aggregate across many `(size, alphabet)` combinations.
    //!
    //! These tests do NOT assert that a specific
    //! `(bits_consumed, burst_bits)` configuration is hit deterministically
    //! on any single iteration — that would require white-box state
    //! instrumentation that the current decoder does not expose. They
    //! assert end-to-end roundtrip correctness through the full
    //! encoder → 4-stream HUF block → `decode_literals` path; a
    //! burst-gate regression that returns the wrong symbol or
    //! desynchronises a stream produces either a
    //! `DecompressLiteralsError` from the `BitstreamReadMismatch` /
    //! `DecodedLiteralCountMismatch` guards or a mismatched decoded
    //! buffer — both fail the assertion. The `max_num_bits` range
    //! checks in the per-test helper also detect silent drift where
    //! the encoder's table-generation choice shifts the test out of
    //! the intended gate regime.
    use super::*;
    use crate::bit_io::BitWriter;
    use crate::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
    use crate::decoding::scratch::HuffmanScratch;
    use crate::huff0::huff0_encoder::{HuffmanEncoder, HuffmanTable as EncTable};
    use alloc::vec::Vec;

    /// Encode `data` as a 4-stream HUF Compressed literals block (table
    /// description + jump table + 4 padded streams) and return the
    /// matching `LiteralsSection` header plus the wire bytes.
    fn build_huf4x_block(data: &[u8]) -> (LiteralsSection, Vec<u8>) {
        assert!(data.len() >= 4, "encode4x requires at least 4 bytes");
        let table = EncTable::build_from_data(data);
        let mut source: Vec<u8> = Vec::new();
        {
            let mut writer = BitWriter::from(&mut source);
            let mut encoder = HuffmanEncoder::new(&table, &mut writer);
            encoder.encode4x(data, true);
            writer.flush();
        }
        let section = LiteralsSection {
            ls_type: LiteralsSectionType::Compressed,
            regenerated_size: data.len() as u32,
            compressed_size: Some(source.len() as u32),
            num_streams: Some(4),
        };
        (section, source)
    }

    /// Roundtrip `data` through encode4x + decode_literals and assert
    /// the decoded buffer matches byte-for-byte. Returns the HUF table's
    /// `max_num_bits` so call sites can sanity-check that they actually
    /// hit the expected burst-gate regime.
    fn roundtrip_assert(data: &[u8]) -> u8 {
        let (section, source) = build_huf4x_block(data);
        let mut scratch = HuffmanScratch::new();
        let mut target = Vec::new();
        let bytes_read = decode_literals(&section, &mut scratch, &source, &mut target)
            .expect("decode_literals must succeed on a well-formed roundtrip");
        assert_eq!(
            bytes_read as usize,
            source.len(),
            "decoder must consume every byte of the literals block"
        );
        assert_eq!(
            target, data,
            "decoded literals must match the encoder input"
        );
        scratch.table.max_num_bits
    }

    /// Roundtrip + assertion that the HUF table's `max_num_bits` falls
    /// inside the expected range — this is what selects which burst-gate
    /// regime the body runs under (`symbols_per_burst = (63 - max) / max`).
    fn roundtrip_with_max_bits_range(data: &[u8], expected: core::ops::RangeInclusive<u8>) {
        let m = roundtrip_assert(data);
        assert!(
            expected.contains(&m),
            "max_num_bits {} outside expected range {:?} for this fixture — \
             test no longer exercises the intended gate regime",
            m,
            expected
        );
    }

    /// Lower boundary: targets `bits_consumed == max_num_bits` on
    /// early burst entries.
    ///
    /// A short stream with a skewed 23-symbol alphabet keeps
    /// `max_num_bits` in the 5..=11 band and limits the number of
    /// burst iterations, so early iterations run with `bits_consumed`
    /// near the gate threshold. The decoder must not lose low stream
    /// bits when the shift formula runs close to the threshold;
    /// roundtrip correctness over short input is the regression signal.
    #[test]
    fn burst_gate_lower_boundary_short_skewed_alphabet() {
        // 36 bytes, 23 distinct symbols, skewed distribution —
        // encoder picks max_num_bits in the 5..=11 band.
        let mut data: Vec<u8> = Vec::with_capacity(36);
        data.extend_from_slice(&[
            0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
            14, 15, 16, 17, 18, 19, 20, 21, 22,
        ]);
        roundtrip_with_max_bits_range(&data, 5..=11);
    }

    /// Upper boundary: `bits_consumed + burst_bits == 64`.
    ///
    /// A long, mid-cardinality alphabet drives many full burst windows.
    /// Across thousands of iterations the burst-fits-in-64 guard
    /// (`bits_consumed + burst_bits <= 64`) is approached and met
    /// exactly. A regression that miscalculated the upper boundary
    /// would read past the loaded 8-byte window and either crash under
    /// debug bounds checks or desynchronise the stream — either way
    /// the roundtrip fails.
    #[test]
    fn burst_gate_upper_boundary_long_mid_alphabet() {
        // 4 KiB with a 97-symbol pseudo-random alphabet (kept under the
        // encoder's 128-weight raw-table limit). Broad distribution →
        // max_num_bits ≈ 7..9, thousands of burst iterations across all
        // four streams.
        let mut data: Vec<u8> = Vec::with_capacity(4096);
        for i in 0..4096u32 {
            data.push((i.wrapping_mul(0x9E37_79B1) % 97) as u8);
        }
        roundtrip_with_max_bits_range(&data, 6..=11);
    }

    /// SIMD-fallback → refill → burst re-entry transition.
    ///
    /// After a `BitReaderReversed::refill` (triggered inside
    /// `advance_state_by_bits` on the SIMD path), `bits_consumed`
    /// rebases to `[0, 7]`. Until it climbs back to `max_num_bits` the
    /// burst gate is closed and the outer loop runs the 4-symbol SIMD
    /// fallback; on the next outer-loop iteration after `bits_consumed`
    /// grows past `max_num_bits` the burst path must re-enter cleanly.
    ///
    /// Stream length of 16 KiB / 4 ≈ 4 KiB per stream encoded ⇒ each
    /// `BitReaderReversed` window crosses many refill boundaries,
    /// guaranteeing the SIMD→refill→burst transition fires repeatedly.
    #[test]
    fn burst_simd_fallback_refill_reentry_long_streams() {
        // 67-symbol modulo distribution (`i % 67`, prime modulus spreads
        // the alphabet evenly) → max_num_bits typically 7..8, which gives
        // `symbols_per_burst = (63 - max) / max ≈ 6..8`.
        let mut data: Vec<u8> = Vec::with_capacity(16 * 1024);
        for i in 0..16 * 1024u32 {
            data.push((i % 67) as u8);
        }
        roundtrip_with_max_bits_range(&data, 5..=8);
    }

    /// Parametric sweep across stream lengths and alphabet shapes.
    ///
    /// The three burst-gate states above are also hit across this matrix
    /// at varying `(bits_consumed, max_num_bits, symbols_per_burst)`
    /// configurations; any future tweak to the gate that mishandles a
    /// specific `(max_num_bits, post-refill bits_consumed)` combo trips
    /// at least one cell here.
    #[test]
    fn burst_gate_sweep_sizes_and_alphabets() {
        let sizes = [
            16usize, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 511, 512, 513, 1023,
            1024, 1025, 4096,
        ];
        for &n in &sizes {
            // Binary alphabet → max_num_bits == 1, symbols_per_burst large.
            let mut bin: Vec<u8> = Vec::with_capacity(n);
            for i in 0..n {
                bin.push((i & 1) as u8);
            }
            roundtrip_assert(&bin);

            // 16-symbol uniform alphabet → max_num_bits ≈ 4.
            let mut sm: Vec<u8> = Vec::with_capacity(n);
            for i in 0..n {
                sm.push((i % 16) as u8);
            }
            roundtrip_assert(&sm);

            // 97-symbol pseudo-random alphabet (where length permits) →
            // max_num_bits ≈ 7..9; kept under the encoder's 128-weight
            // raw-table cap so the encoder reliably succeeds.
            if n >= 128 {
                let mut wide: Vec<u8> = Vec::with_capacity(n);
                for i in 0..n {
                    wide.push((i.wrapping_mul(2_654_435_761) % 97) as u8);
                }
                roundtrip_assert(&wide);
            }
        }
    }

    /// Adversarial regression for the `burst_eligible` safety gate.
    ///
    /// Builds a valid 4-stream HUF block, then forges a `LiteralsSection`
    /// header that claims `regenerated_size = 1` while the encoded
    /// streams still contain a full block worth of symbols. The shrunk
    /// `regenerated_size` collapses `min_seg_len` below
    /// `symbols_per_burst`, the exact precondition `burst_eligible`
    /// guards against. Without that gate, the burst inner loop would
    /// advance `cursors[i]` past `ends[i]` and panic on the
    /// `target[cursors[i]]` write — a DoS surface on malformed input.
    ///
    /// With the gate, the decoder either:
    ///   - falls through to the SIMD-fallback path which immediately
    ///     hits the top-of-loop `cursor_exit_olimit` exit and returns
    ///     a count-mismatch / bitstream-mismatch error, or
    ///   - returns an error before the loop ever runs.
    ///
    /// Either way the test asserts `Err(_)` — the contract is "no
    /// panic, return an error".
    #[test]
    fn burst_gate_malformed_small_regen_returns_error() {
        // 256 bytes is well above MIN_LITERALS_FOR_4_STREAMS so the
        // encoder will happily emit a 4-stream HUF block. The modulo
        // alphabet keeps `max_num_bits` small (≤ 8), maximising
        // `symbols_per_burst` so the small forged `regenerated_size`
        // sits well below it.
        let mut data: Vec<u8> = Vec::with_capacity(256);
        for i in 0..256u32 {
            data.push((i % 67) as u8);
        }
        let (mut section, source) = build_huf4x_block(&data);

        // Forge: claim only 1 regenerated byte. Streams in `source`
        // are still encoded for the full 256-byte input.
        section.regenerated_size = 1;

        let mut scratch = HuffmanScratch::new();
        let mut target = Vec::new();
        let result = decode_literals(&section, &mut scratch, &source, &mut target);

        assert!(
            result.is_err(),
            "decoder must reject the malformed header instead of panicking; \
             got Ok({})",
            result.unwrap_or(0)
        );
    }
}
