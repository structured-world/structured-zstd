//! Donor-faithful port of `HUF_CStream_t` from `lib/compress/huf_compress.c`.
//!
//! Three differences vs the generic `BitWriter`:
//!
//! 1. `add_bits` takes a packed `HUF_CElt` (`u64`) where the bottom 8 bits
//!    hold `nb_bits` and the top `(64 - nb_bits)` bits hold the value
//!    left-shifted to the high end of the word. Allows a single
//!    `shr + or + add` per symbol on x86_64 BMI2.
//! 2. The bit container is filled from the TOP DOWN (donor convention).
//!    To add an N-bit value: `container >>= N; container |= value`.
//! 3. Two indexed containers (`bit_container[0]` and `bit_container[1]`).
//!    Caller can encode into both in parallel (breaking data dependencies)
//!    then merge before flushing — the trick donor uses in the unrolled
//!    `HUF_compress1X_usingCTable_internal_body_loop` to extract
//!    instruction-level parallelism.
//!
//! All hot-path methods are `#[inline(always)]` and accept a const
//! generic `FAST: bool`. `FAST=true` skips the bottom-8-bit mask on the
//! incoming value AND skips the `ptr > end_ptr` overflow check on
//! flush; caller must guarantee a-priori that the bit container has
//! at least `HUF_TABLELOG_ABSOLUTEMAX = 12` free bits before the add
//! and that the output buffer has 8 bytes of slack before the flush.
//!
//! Donor reference: `lib/compress/huf_compress.c:824-983`.

use alloc::vec::Vec;

/// Donor `HUF_BITS_IN_CONTAINER = sizeof(size_t) * 8`. We hard-code 64
/// regardless of target pointer width. Donor's `MEM_32bits()` branch
/// switches the container to `u32` on 32-bit hosts; this crate's CI
/// includes i686, but a 32-bit `usize` host can still operate a 64-bit
/// arithmetic accumulator — the container is just `u64`, not
/// `[u8; size_of::<usize>()]`. Skipping the 32-bit branch keeps the
/// type signatures uniform across targets and matches the speed of
/// the 64-bit hot path on all supported architectures.
pub(crate) const HUF_BITS_IN_CONTAINER: usize = 64;

/// Donor `HUF_TABLELOG_ABSOLUTEMAX = 12` (defined in `common/huf.h`).
pub(crate) const HUF_TABLELOG_ABSOLUTEMAX: usize = 12;

/// Packed Huffman code element matching donor `HUF_CElt`:
/// - Bits [0, 8)            = `nb_bits`
/// - Bits [8, 64 - nb_bits) = 0
/// - Bits [64 - nb_bits, 64) = `value`
///
/// Donor `HUF_setNbBits` / `HUF_setValue` in `huf_compress.c:208-221`.
#[inline(always)]
pub(crate) fn pack_huf_celt(value: u32, nb_bits: u8) -> u64 {
    debug_assert!((nb_bits as usize) <= HUF_TABLELOG_ABSOLUTEMAX);
    if nb_bits == 0 {
        return 0;
    }
    let nb = nb_bits as u64;
    debug_assert!((value as u64) >> nb == 0, "value must fit in nb_bits");
    nb | ((value as u64) << (HUF_BITS_IN_CONTAINER as u64 - nb))
}

/// Dual-container bit packer matching donor `HUF_CStream_t`.
///
/// Operates directly on a borrowed `Vec<u8>` — the caller pre-reserves
/// enough capacity so the hot path can do unchecked 8-byte writes via
/// raw pointer without growing the Vec. [`Self::close`] is the
/// finalization API: it bumps `Vec::len()` once to the exact
/// `bytes_written` count (from the construction-time `start_idx`),
/// surfacing the committed bytes to safe Rust readers. Until `close`
/// runs, `Vec::len()` stays at its construction-time value and all
/// raw-pointer writes target spare capacity past `len`.
///
/// Lifetime / borrow rules: holds `output: &mut Vec<u8>` for its
/// lifetime; caller must finish all encoding work via this stream
/// before any other access to the Vec.
pub(crate) struct HufCStream<'a> {
    /// Top-down bit accumulators. New bits go into the high (top)
    /// `nb_bits` of `container[idx]`. Container is right-shifted by
    /// `nb_bits` before each `add` to make room at the top.
    bit_container: [u64; 2],
    /// Bit-count counters. ONLY the low 8 bits are real; upper bits
    /// carry "dirty" noise from donor's `nbBitsFast` trick and must
    /// be masked with `0xFF` on read.
    bit_pos: [u64; 2],
    /// Output buffer. `cursor` indexes into this Vec; `Vec::len()`
    /// stays at the construction-time value through the entire
    /// add/flush cycle and is advanced ONCE by [`Self::close`] via
    /// `set_len(start_idx + bytes_written)`. In-flight bytes live
    /// in spare capacity past `len`.
    output: &'a mut Vec<u8>,
    /// Byte index of the first byte this stream writes (= `output.len()`
    /// at construction). Used to compute `bytes_written` in `close`.
    start_idx: usize,
    /// Current write cursor. Always satisfies
    /// `start_idx <= cursor <= output.capacity()`. Bytes in
    /// `output[start_idx..cursor]` ARE committed by raw-pointer
    /// writes but NOT yet reflected in `output.len()` (which still
    /// points at `start_idx`); bytes in `output[cursor..cursor+8]`
    /// are scratch the next flush will overwrite. `close` is the
    /// only call that bumps `len` and surfaces the committed bytes
    /// to safe Rust readers of the `Vec`.
    cursor: usize,
    /// `cursor` must never reach this value — beyond it the 8-byte
    /// flush write would overrun the reserved capacity. `FAST=true`
    /// flushes skip the check; `FAST=false` clamps `cursor = end_ptr`
    /// on overflow (donor's `if (!kFast && ptr > endPtr) ptr = endPtr`).
    end_ptr: usize,
    /// Set to `true` by `flush_bits::<false>` when the clamp at
    /// `cursor > end_ptr` actually fires. `close()` uses this flag to
    /// emit donor's overflow result (return 0). Without it, the clamp
    /// would mask overflow: post-clamp `cursor == end_ptr`, so a
    /// `cursor >= end_ptr + 8` post-flush check could never fire, and
    /// an undersized `dst_capacity` would silently succeed with a
    /// truncated stream.
    overflow: bool,
}

impl<'a> HufCStream<'a> {
    /// Donor `HUF_initCStream`. Requires `output.capacity() >=
    /// output.len() + dst_capacity` AND `dst_capacity > 8` (else
    /// returns `None`, mirroring donor's `ERROR(dstSize_tooSmall)`).
    ///
    /// `dst_capacity` is the upper bound on bytes this stream may write;
    /// donor uses `HUF_tightCompressBound(srcSize, tableLog) + 8` slack.
    pub(crate) fn new(output: &'a mut Vec<u8>, dst_capacity: usize) -> Option<Self> {
        if dst_capacity <= 8 {
            return None;
        }
        let start_idx = output.len();
        // Reserve capacity for the worst-case write + 8 byte flush slack.
        // We DO NOT pre-zero (`resize`) the spare capacity — the hot
        // path writes via raw pointers into the spare slots and
        // `close()` calls `set_len` only after committing the actual
        // byte count. For large literal sections (table_log=11 → up
        // to 2.7 MiB per stream), the eager memset was a measurable
        // regression on the worker hot path.
        output.reserve(dst_capacity);
        Some(Self {
            bit_container: [0, 0],
            bit_pos: [0, 0],
            output,
            start_idx,
            cursor: start_idx,
            end_ptr: start_idx + dst_capacity - 8,
            overflow: false,
        })
    }

    /// Donor `HUF_addBits`: insert `elt`'s value into the top `nb_bits`
    /// of `bit_container[idx]`.
    ///
    /// `FAST=true` matches donor's `kFast=1`: caller guarantees ≥ 4
    /// free bits remain in the container post-add, so we can skip the
    /// `& !0xFF` value mask. Donor uses `HUF_getValueFast` here which
    /// is just `elt` (dirty bottom 8 bits get shifted out by the next
    /// container shr anyway).
    #[inline(always)]
    pub(crate) fn add_bits<const FAST: bool>(&mut self, elt: u64, idx: usize) {
        debug_assert!(idx <= 1);
        let nb_bits = elt & 0xFF;
        debug_assert!((nb_bits as usize) <= HUF_TABLELOG_ABSOLUTEMAX);
        // Make room at the top by right-shifting the container.
        // SAFETY: `nb_bits <= 12 < 64`, so the shift amount is in range.
        self.bit_container[idx] >>= nb_bits;
        // OR in the value. In FAST mode the bottom 8 bits of `elt`
        // (which hold nb_bits) are "dirty" but they land in the
        // already-occupied lower portion that the next shr will
        // overwrite — donor's `HUF_getValueFast` exploits this.
        let value = if FAST { elt } else { elt & !0xFFu64 };
        self.bit_container[idx] |= value;
        // Donor `HUF_getNbBitsFast(elt) = elt` — we accumulate the
        // whole word; only the low 8 bits of `bit_pos` are real on
        // any subsequent read (always masked with `0xFF`).
        let nb_add = if FAST { elt } else { nb_bits };
        self.bit_pos[idx] = self.bit_pos[idx].wrapping_add(nb_add);
    }

    /// Donor `HUF_flushBits`: write the top `nb_bytes` of
    /// `bit_container[0]` to `output[cursor..cursor+8]`, advance
    /// `cursor` by `nb_bytes`, keep the trailing `< 8` bits in the
    /// container for the next flush.
    ///
    /// `FAST=true` skips the `cursor > end_ptr` overflow clamp; caller
    /// must have pre-sized the buffer to guarantee no overrun.
    #[inline(always)]
    pub(crate) fn flush_bits<const FAST: bool>(&mut self) {
        let nb_bits = (self.bit_pos[0] & 0xFF) as usize;
        let nb_bytes = nb_bits >> 3;
        // Top `nb_bits` of the container become the next bytes.
        // Donor uses `bitContainer >> (HUF_BITS_IN_CONTAINER - nb_bits)`.
        // Guard the shift: `nb_bits == 0` would shift by 64 (UB in Rust).
        let bit_container = if nb_bits == 0 {
            0
        } else {
            self.bit_container[0] >> (HUF_BITS_IN_CONTAINER - nb_bits)
        };
        // Mask `bit_pos` to keep the leftover < 8 bits in the low 3 bits.
        self.bit_pos[0] &= 7;
        // 8-byte LE write at `cursor`. Bytes at [cursor+nb_bytes..cursor+8]
        // are overwritten by the next flush; we don't care about them.
        let bytes = bit_container.to_le_bytes();
        // SAFETY: `new()` reserved `dst_capacity` bytes via
        // `Vec::reserve` (without zeroing), so `cursor + 8 <=
        // start_idx + dst_capacity <= output.capacity()`. The write
        // targets uninitialised spare capacity; `close()` reconciles
        // `len` afterwards.
        unsafe {
            let dst = self.output.as_mut_ptr().add(self.cursor);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, 8);
        }
        self.cursor += nb_bytes;
        if !FAST && self.cursor > self.end_ptr {
            self.cursor = self.end_ptr;
            self.overflow = true;
        }
    }

    /// Donor `HUF_compress1X_usingCTable_internal_body_loop`
    /// (`huf_compress.c:991-1043`) with all mutable bit state hoisted
    /// into locals so the two containers, their bit positions, and the
    /// write cursor stay register-resident across the whole encode loop.
    ///
    /// The per-call `add_bits`/`flush_bits`/`zero_index1`/`merge_index1`
    /// path reads and writes `self.bit_container[idx]` etc. through
    /// `&mut self` every symbol; the optimizer could not prove the
    /// output-buffer raw writes in `flush_bits` don't alias those struct
    /// fields, so it conservatively reloaded the containers from memory
    /// per symbol (donor keeps them in `HUF_CStream_t` locals). Hoisting
    /// to locals here matches donor's register-resident shape. The
    /// arithmetic mirrors those four methods byte for byte, so the
    /// emitted bitstream is identical; only the codegen changes.
    ///
    /// Phases match the prior `encode_one_stream_unrolled`: (1) `n %
    /// K_UNROLL` tail symbols slow, (2) bring `n` to a multiple of
    /// `2 * K_UNROLL`, (3) dual-container main loop processing
    /// `2 * K_UNROLL` symbols per iteration. Symbols consumed in reverse
    /// (`data[--n]`).
    #[inline]
    pub(crate) fn encode_unrolled<
        const K_UNROLL: usize,
        const K_FAST_FLUSH: bool,
        const K_LAST_FAST: bool,
    >(
        &mut self,
        table: &[u64],
        data: &[u8],
    ) {
        let mut bc0 = self.bit_container[0];
        let mut bc1 = self.bit_container[1];
        let mut bp0 = self.bit_pos[0];
        let mut bp1 = self.bit_pos[1];
        let mut cursor = self.cursor;
        let mut overflow = self.overflow;
        let end_ptr = self.end_ptr;
        // Stable raw base: `new()` reserved `dst_capacity` and this method
        // never pushes to `output`, so no realloc can move the buffer and
        // every `cursor + 8 <= capacity` write targets spare capacity.
        let out_base = self.output.as_mut_ptr();

        // Mirror `add_bits`: `$fast` (a `const`-valued bool) const-folds.
        macro_rules! add0 {
            ($elt:expr, $fast:expr) => {{
                let elt = $elt;
                let nb_bits = elt & 0xFF;
                bc0 >>= nb_bits;
                bc0 |= if $fast { elt } else { elt & !0xFFu64 };
                bp0 = bp0.wrapping_add(if $fast { elt } else { nb_bits });
            }};
        }
        macro_rules! add1 {
            ($elt:expr, $fast:expr) => {{
                let elt = $elt;
                let nb_bits = elt & 0xFF;
                bc1 >>= nb_bits;
                bc1 |= if $fast { elt } else { elt & !0xFFu64 };
                bp1 = bp1.wrapping_add(if $fast { elt } else { nb_bits });
            }};
        }
        // Mirror `flush_bits` on `bc0`/`bp0`/`cursor`.
        macro_rules! flush0 {
            ($fast:expr) => {{
                let nb_bits = (bp0 & 0xFF) as usize;
                let nb_bytes = nb_bits >> 3;
                let chunk = if nb_bits == 0 {
                    0
                } else {
                    bc0 >> (HUF_BITS_IN_CONTAINER - nb_bits)
                };
                bp0 &= 7;
                let bytes = chunk.to_le_bytes();
                // SAFETY: see `out_base` above; `cursor + 8 <= capacity`.
                unsafe {
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), out_base.add(cursor), 8);
                }
                cursor += nb_bytes;
                if !$fast && cursor > end_ptr {
                    cursor = end_ptr;
                    overflow = true;
                }
            }};
        }

        let mut n = data.len();
        let rem = n % K_UNROLL;

        // Phase 1: tail symbols (< K_UNROLL) on the SLOW path.
        if rem > 0 {
            for _ in 0..rem {
                n -= 1;
                add0!(table[data[n] as usize], false);
            }
            flush0!(K_FAST_FLUSH);
        }
        debug_assert!(n.is_multiple_of(K_UNROLL));

        // Phase 2: bring n down to a multiple of 2 * K_UNROLL.
        if !n.is_multiple_of(2 * K_UNROLL) {
            for u in 1..K_UNROLL {
                add0!(table[data[n - u] as usize], true);
            }
            add0!(table[data[n - K_UNROLL] as usize], K_LAST_FAST);
            flush0!(K_FAST_FLUSH);
            n -= K_UNROLL;
        }
        debug_assert!(n.is_multiple_of(2 * K_UNROLL));

        // Phase 3: dual-container main loop.
        while n > 0 {
            for u in 1..K_UNROLL {
                add0!(table[data[n - u] as usize], true);
            }
            add0!(table[data[n - K_UNROLL] as usize], K_LAST_FAST);
            flush0!(K_FAST_FLUSH);

            bc1 = 0;
            bp1 = 0;
            for u in 1..K_UNROLL {
                add1!(table[data[n - K_UNROLL - u] as usize], true);
            }
            add1!(table[data[n - K_UNROLL - K_UNROLL] as usize], K_LAST_FAST);
            // merge_index1: fold container 1 into container 0.
            let nb_bits_1 = bp1 & 0xFF;
            bc0 >>= nb_bits_1;
            bc0 |= bc1;
            bp0 = bp0.wrapping_add(bp1);
            flush0!(K_FAST_FLUSH);

            n -= 2 * K_UNROLL;
        }
        debug_assert_eq!(n, 0);

        // Write the hoisted state back so `close()` sees the final values.
        self.bit_container[0] = bc0;
        self.bit_container[1] = bc1;
        self.bit_pos[0] = bp0;
        self.bit_pos[1] = bp1;
        self.cursor = cursor;
        self.overflow = overflow;
    }

    /// Number of bits currently buffered in `bit_container[0]`.
    /// Useful for the close-stream finalization (donor writes a final
    /// partial byte if bits remain).
    #[inline(always)]
    pub(crate) fn pending_bits(&self) -> usize {
        (self.bit_pos[0] & 0xFF) as usize
    }

    /// Donor `HUF_closeCStream`: append the 1-bit end marker (value=1,
    /// nb_bits=1), final flush, return total bytes written. Returns 0
    /// on overflow (donor convention).
    pub(crate) fn close(mut self) -> usize {
        // Donor `HUF_endMark()` returns a HUF_CElt with nbBits=1, value=1.
        // Packed: low byte = 1 (nb_bits), top bit of u64 = 1 (value).
        let end_mark: u64 = 1u64 | (1u64 << (HUF_BITS_IN_CONTAINER as u64 - 1));
        self.add_bits::<false>(end_mark, 0);
        self.flush_bits::<false>();
        let nb_bits = self.pending_bits();
        if self.overflow {
            // Overflow — donor returns 0. The clamp in
            // `flush_bits::<false>` already capped `cursor` at
            // `end_ptr`, so a post-flush `cursor >= end_ptr + 8`
            // check would never fire — we rely on the explicit
            // `overflow` flag set at the moment of the clamp.
            // `start_idx == output.len()` pre-construction (no
            // `resize` was done; we wrote into spare capacity), so
            // no truncate is needed — the Vec's logical length is
            // already correct.
            return 0;
        }
        // Total bytes: full bytes flushed + (1 byte for trailing partial bits).
        let bytes_written = (self.cursor - self.start_idx) + usize::from(nb_bits > 0);
        // Commit the previously-uninitialised spare-capacity writes
        // by advancing `len`. SAFETY: `flush_bits` wrote exactly
        // `bytes_written` bytes into spare capacity at positions
        // [start_idx, start_idx + bytes_written), all within
        // `output.capacity()` per the reserve in `new()`.
        unsafe {
            self.output.set_len(self.start_idx + bytes_written);
        }
        bytes_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roundtrip a single short symbol through HufCStream and verify
    /// the byte output decodes back to the same bit pattern.
    #[test]
    fn add_bits_single_symbol_emits_correct_byte() {
        let mut out: Vec<u8> = Vec::new();
        let mut s = HufCStream::new(&mut out, 64).expect("init ok");
        // Symbol: nb_bits=4, value=0b1011 (11). Packed: low=4, top=11<<60.
        let elt = pack_huf_celt(0b1011, 4);
        s.add_bits::<false>(elt, 0);
        let n = s.close();
        assert!(n > 0);
        assert_eq!(out.len(), 1);
        // Donor `HUF_addBits` + `HUF_flushBits` layout (top-down
        // packing in the 64-bit container, then `flushBits` shifts
        // the buffered bits down to the bottom of a 0-padded word
        // and `MEM_writeLE` stores 8 bytes little-endian — emitted
        // byte 0 is the LOW byte of that word):
        //
        // After `add_bits(pack_huf_celt(0b1011, 4), 0)`:
        //   container top 4 bits = 0b1011, bit_pos = 4
        // After `close()` prepends end-mark `(value=1, nb_bits=1)`:
        //   container top 5 bits = [1, 1, 0, 1, 1] (high → low),
        //   bit_pos = 5
        // `flush_bits` then `container >> (64 - 5)` produces 0b11011
        // = 27 = 0x1B, which lands in `out[0]`.
        assert_eq!(
            out[0], 0x1B,
            "first emitted byte must mirror donor's HUF_addBits + \
             HUF_endMark packing collapsed to a 5-bit prefix 0b11011",
        );
    }

    /// Encode multiple symbols summing to > 64 bits; expect the
    /// container to flush partway and write whole bytes to output.
    #[test]
    fn add_bits_overflowing_container_flushes_correctly() {
        let mut out: Vec<u8> = Vec::new();
        let mut s = HufCStream::new(&mut out, 256).expect("init ok");
        // 8 symbols of 8 bits each = 64 bits — exactly fills container.
        for i in 0..8 {
            let elt = pack_huf_celt(i as u32, 8);
            s.add_bits::<false>(elt, 0);
        }
        s.flush_bits::<false>();
        // After flushing 64 bits = 8 bytes; cursor advanced 8.
        assert_eq!(s.cursor - s.start_idx, 8);
        // pending bits should be 0 (cleanly flushed).
        assert_eq!(s.pending_bits(), 0);
        let n = s.close();
        // close adds 1-bit end mark + flush → 1 trailing byte for end mark.
        assert!(n >= 8);
    }

    /// Dual-container parallel encode through `encode_unrolled` (which
    /// inlines the zero/merge of container 1 into container 0). With a
    /// uniform 4-bit code over 16 symbols, the total emitted size is
    /// order-independent: 16 * 4 = 64 payload bits + a 1-bit end mark =
    /// 65 bits → 9 bytes. K_UNROLL=4 with 16 symbols runs phase 3 (the
    /// dual-container loop) twice, so the merge path is exercised.
    #[test]
    fn encode_unrolled_dual_container_size_is_deterministic() {
        let mut out: Vec<u8> = Vec::new();
        let mut s = HufCStream::new(&mut out, 64).expect("init ok");
        // Every symbol maps to the same 4-bit code (value 0b1010).
        let table = [pack_huf_celt(0b1010, 4); 256];
        let data = [0u8; 16];
        s.encode_unrolled::<4, false, false>(&table, &data);
        let n = s.close();
        assert_eq!(
            n, 9,
            "16 symbols * 4 bits + 1 end-mark bit = 65 bits = 9 bytes"
        );
    }
}
