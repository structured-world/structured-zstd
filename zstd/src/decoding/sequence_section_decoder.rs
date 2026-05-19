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
    // Reset the fallback sequences vec on entry. The non-RLE fast path
    // never writes to it, so without this clear it would carry whatever
    // entries the previous block left behind — a stale-data hazard for
    // any external caller that inspects scratch.sequences after decode.
    rle_fallback_sequences.clear();

    let bytes_read = maybe_update_fse_tables(section, source, fse)?;
    vprintln!("Updating tables used {} bytes", bytes_read);

    let bit_stream = &source[bytes_read..];
    let mut br = BitReaderReversed::new(bit_stream);

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

    // Single fused loop over all sequences. The state-update step
    // (`ensure_bits` + per-decoder `update_state_fast`) is skipped on
    // the final iteration — matching the donor `isLastSeq` template
    // pattern (no pre-fetch of the next state when there is no next
    // sequence). Under `#[inline(always)]` the trailing `if i + 1 <
    // num_sequences` collapses to the same generated code as the
    // previous split-shape version, but keeps the per-sequence body in
    // one place so future edits to the hot path stay in sync.
    let num_sequences = section.num_sequences as usize;
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
fn decode_one_sequence_inline(
    ll_dec: &mut FSEDecoder<'_>,
    ml_dec: &mut FSEDecoder<'_>,
    of_dec: &mut FSEDecoder<'_>,
    br: &mut BitReaderReversed<'_>,
) -> Sequence {
    let ll_code = ll_dec.decode_symbol();
    let ml_code = ml_dec.decode_symbol();
    let of_code = of_dec.decode_symbol();

    let (ll_value, ll_num_bits) = lookup_ll_code(ll_code);
    let (ml_value, ml_num_bits) = lookup_ml_code(ml_code);

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

fn decode_sequences_with_rle(
    section: &SequencesHeader,
    br: &mut BitReaderReversed<'_>,
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

        let (ll_value, ll_num_bits) = lookup_ll_code(ll_code);
        let (ml_value, ml_num_bits) = lookup_ml_code(ml_code);

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
/// exceed 35, so callers index this array unconditionally and rely on
/// the assert in the helper below to catch any future invariant break.
///
/// Layout: low 24 bits = baseline (max 65536 fits), high 8 bits =
/// extra_bits (max 16). One u32 load on the hot path returns both
/// fields — replaces the previous pair of separate `LL_BASE[idx]` +
/// `LL_EXTRA_BITS[idx]` loads (two distinct cache-line touches into
/// 144 B + 36 B = 180 B; packed table is 144 B = one contiguous
/// region).
const LL_META: [u32; 36] = pack_code_meta(
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
const ML_META: [u32; 53] = pack_code_meta(
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
            scratch.ll_rle = None;
        }
        ModeType::Repeat => {
            vprintln!("Repeat ll table");
            /* Nothing to do */
        }
    };

    let of_source = &source[bytes_read..];

    match modes.of_mode() {
        ModeType::FSECompressed => {
            let bytes = scratch.offsets.build_decoder(of_source, OF_MAX_LOG)?;
            vprintln!("Updating of table");
            vprintln!("Used bytes: {}", bytes);
            bytes_read += bytes;
            scratch.of_rle = None;
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
            scratch.of_rle = None;
        }
        ModeType::Repeat => {
            vprintln!("Repeat of table");
            /* Nothing to do */
        }
    };

    let ml_source = &source[bytes_read..];

    match modes.ml_mode() {
        ModeType::FSECompressed => {
            let bytes = scratch.match_lengths.build_decoder(ml_source, ML_MAX_LOG)?;
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
            scratch.ml_rle = None;
        }
        ModeType::Repeat => {
            vprintln!("Repeat ml table");
            /* Nothing to do */
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
