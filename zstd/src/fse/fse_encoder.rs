use crate::bit_io::BitBurst;
use crate::bit_io::BitWriter;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

/// Encodes a stream with a table the caller holds, so a table built into
/// reusable storage is never moved into the encoder.
pub(crate) struct FSEEncoder<'table, 'output, V: AsMut<Vec<u8>>> {
    pub(super) table: &'table FSETable,
    writer: &'output mut BitWriter<V>,
}

impl<'table, 'output, V: AsMut<Vec<u8>>> FSEEncoder<'table, 'output, V> {
    pub fn new(table: &'table FSETable, writer: &'output mut BitWriter<V>) -> Self {
        FSEEncoder { table, writer }
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Last state index
    /// * Padding bits to fill up last byte
    #[cfg(any(test, feature = "fuzz-exports"))]
    pub fn encode(&mut self, data: &[u8]) {
        self.write_table();

        let mut state = self.table.start_state(data[data.len() - 1]);
        for x in data[0..data.len() - 1].iter().rev().copied() {
            let next = self.table.next_state(x, state.index);
            let diff = transition_bits(state.index, next.num_bits);
            self.writer.write_bits(diff as u64, next.num_bits as usize);
            state = next;
        }
        self.writer
            .write_bits(state.index as u64, self.acc_log() as usize);

        let bits_to_fill = self.writer.misaligned();
        if bits_to_fill == 0 {
            self.writer.write_bits(1u32, 8);
        } else {
            self.writer.write_bits(1u32, bits_to_fill);
        }
    }

    /// Encodes the data using the provided table but with two interleaved streams
    /// Writes
    /// * Table description
    /// * Encoded data with two interleaved states
    /// * Both Last state indexes
    /// * Padding bits to fill up last byte
    pub fn encode_interleaved(&mut self, data: &[u8]) {
        self.write_table();

        assert!(data.len() >= 2);
        let mut ip = data.len();
        let mut state_1;
        let mut state_2;

        // The stream goes through a burst, upstream's local `BIT_CStream_t`
        // (`FSE_compress_usingCTable_generic`, fse_compress.c): a checked
        // write per symbol reloaded and stored the writer's state each time.
        // A transition emits at most `acc_log <= 9` bits (`to_encoder_table`
        // caps the log), so four of them beside the under 8 bits a flush
        // leaves stay under 64, which is upstream's four-symbol step on a
        // 64-bit container. The room is what the stream can commit: `acc_log`
        // bits a symbol and for each final state and the end mark; the burst
        // counts the bytes of the table description it drains as it starts.
        // Kept tight on purpose: the Huffman weight description reuses one
        // buffer sized for the widest description, and a looser room would
        // grow it.
        let table = self.table;
        let acc_log = table.acc_log() as usize;
        debug_assert!(acc_log <= 9, "an FSE accuracy log of {acc_log}");
        let stream_bits = (data.len() + 2) * acc_log + 8;
        let mut burst = self.writer.burst(stream_bits.div_ceil(8));
        let step = |burst: &mut BitBurst<'_, V>, state: usize, symbol: u8| {
            let next = table.next_state(symbol, state);
            let diff = transition_bits(state, next.num_bits);
            // SAFETY: at most five transitions of `<= 9` bits run between
            // flushes (an odd stream's first symbol joins the first step),
            // 45 bits beside the `<= 7` a flush leaves, and a
            // transition's bits are `state & mask`, clean above its width.
            unsafe { burst.add(diff as u64, next.num_bits as usize) };
            next.index
        };

        if data.len() & 1 != 0 {
            ip -= 1;
            state_1 = table.start_state(data[ip]).index;
            ip -= 1;
            state_2 = table.start_state(data[ip]).index;
            ip -= 1;
            state_1 = step(&mut burst, state_1, data[ip]);
        } else {
            ip -= 1;
            state_2 = table.start_state(data[ip]).index;
            ip -= 1;
            state_1 = table.start_state(data[ip]).index;
        }

        let remaining_after_init = data.len() - 2;
        if remaining_after_init & 2 != 0 {
            ip -= 1;
            state_2 = step(&mut burst, state_2, data[ip]);
            ip -= 1;
            state_1 = step(&mut burst, state_1, data[ip]);
            // SAFETY: the room covers every bit the stream commits (above).
            unsafe { burst.flush() };
        }

        while ip > 0 {
            ip -= 1;
            state_2 = step(&mut burst, state_2, data[ip]);
            ip -= 1;
            state_1 = step(&mut burst, state_1, data[ip]);
            if ip > 0 {
                ip -= 1;
                state_2 = step(&mut burst, state_2, data[ip]);
                ip -= 1;
                state_1 = step(&mut burst, state_1, data[ip]);
            }
            // SAFETY: as above.
            unsafe { burst.flush() };
        }

        // SAFETY: two states of `acc_log <= 9` bits beside `<= 7` held, then
        // a flush, then at most 8 bits of end mark beside `<= 7`; the room
        // counts both.
        unsafe {
            burst.add(state_2 as u64, acc_log);
            burst.add(state_1 as u64, acc_log);
            burst.flush();
        }
        let bits_to_fill = match burst.misaligned() {
            0 => 8,
            fill => fill,
        };
        // SAFETY: as above.
        unsafe {
            burst.add(1, bits_to_fill);
            burst.flush();
        }
        burst.finish();
    }

    fn write_table(&mut self) {
        self.table.write_table(self.writer);
    }

    #[cfg(any(test, feature = "fuzz-exports"))]
    pub(super) fn acc_log(&self) -> u8 {
        self.table.acc_log()
    }
}

/// Upstream zstd-parity per-symbol coding transform, mirroring upstream
/// `FSE_symbolCompressionTransform` in `lib/compress/fse_compress.c`.
/// Together with [`FSETable::state_table_flat`] this gives an O(1)
/// next-state lookup in [`FSETable::next_state`], replacing the prior
/// linear scan over `SymbolStates::states`.
///
/// Upstream zstd formulas (`FSE_encodeSymbol`, `fse_compress.c`):
///
/// ```text
/// value       = (1 << acc_log) + current_state.index
/// nb_bits_out = (value + delta_nb_bits) >> 16
/// baseline    = current_state.index & !((1 << nb_bits_out) - 1)
/// next_index  = state_table_flat[(value >> nb_bits_out) + delta_find_state]
/// ```
///
/// `delta_find_state` is intentionally signed (can be negative for
/// low-probability symbols — see the `-1 | 1` arm in
/// `build_table_from_probabilities`); the indexing arithmetic on
/// `state_table_flat` is signed-correct via the construction above
/// (`(value >> nb_bits_out)` is always large enough to cover any
/// negative offset, because of how delta_find_state is derived).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SymbolTT {
    /// Upstream zstd 16.16 fixed-point value. **`u32`, not `usize`** — on 16-bit
    /// targets (AVR / MSP430 / Cortex-M0 in the no-atomic profile this
    /// crate documents) `usize` is 16 bits, and `<<16` / `>>16` on a
    /// `usize` would silently overflow to zero, breaking the
    /// fixed-point math. Upstream zstd `fse_compress.c` uses `U32` throughout
    /// the same arithmetic for the same reason.
    pub(crate) delta_nb_bits: u32,
    /// `i32` as upstream's `int deltaFindState`: its magnitude is under the
    /// 512-entry table, and the pair then packs into 8 bytes, half a cache line
    /// for two symbols where an `isize` padded it to 16.
    pub(crate) delta_find_state: i32,
}

/// Largest next-state table any FSE encoder table can carry: the accuracy log
/// is capped at 9 (`to_encoder_table` rejects anything wider), so the table
/// never exceeds `1 << 9` entries.
pub(super) const MAX_FSE_TABLE_SIZE: usize = 1 << 9;

#[derive(Debug, Clone)]
pub struct FSETable {
    /// Each symbol's normalized probability (`0` absent, `-1` less than one),
    /// what the table header describes. A symbol's start state and widest
    /// transition follow from [`Self::symbol_tt`] and [`Self::state_table_flat`],
    /// and are derived when asked for, as upstream derives them
    /// (`FSE_initCState2`, `FSE_getMaxNbBits`), rather than per symbol per build.
    pub(super) probabilities: [i32; 256],
    /// Sum of all states.states.len()
    pub(crate) table_size: usize,
    /// Upstream zstd-parity flat next-state table — `state_table_flat[i]` is the
    /// FSE state index that the encoder transitions TO when the
    /// transition arithmetic resolves to slot `i`. Length is exactly
    /// `table_size` (= `1 << acc_log`). Mirror of upstream
    /// `nextStateTable` (`fse_compress.c`).
    /// Held inline rather than boxed: it is the table's only heap allocation,
    /// and the table is rebuilt per block and per Huffman weight description —
    /// once per built table — so boxing it cost an allocation on a path that
    /// runs about a hundred times a frame. `MAX_FSE_TABLE_SIZE` entries is a
    /// kibibyte beside the ~12 KiB of per-symbol arrays already inline, and the
    /// live region is `state_table_flat[..table_size]`.
    pub(super) state_table_flat: [u16; MAX_FSE_TABLE_SIZE],
    /// Per-symbol upstream zstd-parity coding transform — see [`SymbolTT`].
    pub(super) symbol_tt: [SymbolTT; 256],
    /// How many leading entries of `probabilities` / `symbol_tt` may hold a non-default
    /// value; everything past it is default. Lets a rebuild clear only what the
    /// previous build wrote beyond the new alphabet instead of the whole array.
    live_symbols: usize,
}

impl FSETable {
    /// A table holding nothing, for a caller that is about to fill it.
    ///
    /// Every field is overwritten by a build, so the zeroes are a starting
    /// point rather than a meaningful state; `table_size == 0` says as much.
    pub(crate) fn blank() -> Self {
        Self {
            probabilities: [0; 256],
            table_size: 0,
            state_table_flat: [0u16; MAX_FSE_TABLE_SIZE],
            symbol_tt: [SymbolTT::default(); 256],
            live_symbols: 0,
        }
    }

    /// Heap bytes this table holds beyond its inline struct: none. Every array
    /// it carries — `probabilities`, `symbol_tt` and the flat state-transition table —
    /// is fixed-size and inline, so the owner's `size_of` accounts for all of
    /// it. Kept as a method so callers summing a footprint read one rule.
    pub(crate) fn heap_size(&self) -> usize {
        0
    }

    /// O(1) next-state lookup mirroring upstream `FSE_encodeSymbol`
    /// (`lib/compress/fse_compress.c`). Was a linear scan over a
    /// `Vec<State>` per symbol; the flat tables that drive the upstream zstd
    /// arithmetic were already built during table construction and
    /// previously discarded — now they live on the FSETable
    /// ([`SymbolTT`] + [`Self::state_table_flat`]) and `next_state` is
    /// pure arithmetic + one array load. See #164.
    #[inline]
    pub(crate) fn next_state(&self, symbol: u8, idx: usize) -> State {
        // Upstream zstd formulas, transliterated:
        //   value       = (1 << acc_log) + idx
        //   nb_bits     = (value + delta_nb_bits) >> 16
        //   baseline    = idx & !((1 << nb_bits) - 1)
        //   next_index  = state_table_flat[(value >> nb_bits) + delta_find_state]
        //
        // `delta_find_state` is signed (`isize`) — for low-probability
        // symbols it is intentionally negative. The combined right-shift
        // `(value >> nb_bits)` is always large enough to keep the final
        // index non-negative; this is a construction invariant from
        // `build_table_from_probabilities`.
        // Fixed-point arithmetic uses `u32` (upstream zstd parity, 16-bit-target
        // safe — see `SymbolTT::delta_nb_bits` doc). `table_size` is
        // `1 << acc_log` with `acc_log <= 12`, so `value <= 4096 + 4095`
        // fits comfortably; the `<<16` half of the math lives entirely
        // inside `delta_nb_bits` which was already computed in u32 by
        // the builder. Result `nb_bits` is small (<= 8 for typical
        // FSE tables) — cast to `usize` only at the final indexing
        // sites (`(1usize << nb_bits)`, `value >> nb_bits` as slot).
        let tt = self.symbol_tt[symbol as usize];
        let value = (self.table_size + idx) as u32;
        let nb_bits = ((value + tt.delta_nb_bits) >> 16) as usize;
        let slot = ((value >> nb_bits) as i32 + tt.delta_find_state) as usize;
        // Inside `0..table_size` by construction, as upstream's
        // `FSE_encodeSymbol` relies on: the transition of a state of a present
        // symbol lands on one of that symbol's own slots.
        debug_assert!(
            slot < self.table_size,
            "next-state slot {slot} outside a {}-entry table",
            self.table_size
        );
        // SAFETY: `slot < table_size <= MAX_FSE_TABLE_SIZE`, asserted above
        // and held by the build.
        State {
            num_bits: nb_bits as u8,
            index: unsafe { *self.state_table_flat.get_unchecked(slot) } as usize,
        }
    }

    /// First-state lookup for the encode-init of a new FSE stream, upstream
    /// zstd `FSE_initCState2`. Called once per stream, so it is derived here
    /// rather than stored for every symbol of every build.
    pub(crate) fn start_state(&self, symbol: u8) -> State {
        assert!(
            self.probabilities[symbol as usize] != 0,
            "symbol must be present in the FSE table"
        );
        let tt = self.symbol_tt[symbol as usize];
        let nb_bits_out = (tt.delta_nb_bits + (1 << 15)) >> 16;
        // `delta_nb_bits` is `(max_bits_out << 16) - min_state_plus` with
        // `0 < min_state_plus <= table_size < 1 << 15`, so `nb_bits_out` is
        // `max_bits_out` and the difference is `min_state_plus`
        // (upstream `FSE_initCState2` subtracts unchecked as well).
        debug_assert!(
            (nb_bits_out << 16) > tt.delta_nb_bits,
            "FSE start state of symbol {symbol} below zero"
        );
        let value = (nb_bits_out << 16) - tt.delta_nb_bits;
        let slot = (value >> nb_bits_out) as i32 + tt.delta_find_state;
        // In `0..table_size` by construction (upstream `FSE_initCState2`):
        // `delta_find_state` is bounded by `total - probability`, and
        // `value >> nb_bits_out` by `2 * probability - 1`.
        debug_assert!(
            (0..self.table_size as i32).contains(&slot),
            "FSE start-state slot {slot} for symbol {symbol} outside the table"
        );
        // Callers consume only `index`; `num_bits` is a property of a
        // transition, not of the initial state, so it is zeroed.
        State {
            num_bits: 0,
            index: self.state_table_flat[slot as usize] as usize,
        }
    }

    pub fn acc_log(&self) -> u8 {
        self.table_size.ilog2() as u8
    }

    /// Get the probability assigned to a symbol (0 means absent, -1 means less-than-1).
    pub(crate) fn symbol_probability(&self, symbol: u8) -> i32 {
        self.probabilities[symbol as usize]
    }

    /// The widest transition [`Self::next_state`] takes on `symbol`, from the
    /// last state (`index = table_size - 1`); `None` for an absent symbol.
    pub(crate) fn max_num_bits_for_symbol(&self, symbol: u8) -> Option<u8> {
        if self.probabilities[symbol as usize] == 0 {
            return None;
        }
        let max_value =
            (2 * self.table_size as u32 - 1) + self.symbol_tt[symbol as usize].delta_nb_bits;
        Some((max_value >> 16) as u8)
    }

    /// Compute the exact serialized size (in bits) of the FSE table header,
    /// including the byte-alignment padding at the end.
    /// Mirrors `write_table` but counts bits instead of writing them.
    ///
    /// The result assumes the header starts at a byte boundary, which matches
    /// all current encoder call sites.
    pub(crate) fn table_header_bits(&self) -> usize {
        // Delegate to the free `ncount_header_bits` so the header sizing has a
        // single source of truth shared with the pre-build cost estimate in
        // `fse_header_bits_for_counts` (the table-mode selector prices a
        // candidate table from its normalized counts without building it).
        let probabilities = &self.probabilities;
        ncount_header_bits(probabilities.len(), |i| probabilities[i], self.acc_log())
    }

    pub(crate) fn write_table<V: AsMut<Vec<u8>>>(&self, writer: &mut BitWriter<V>) {
        let probabilities = &self.probabilities;
        write_ncount(
            probabilities.len(),
            |i| probabilities[i],
            self.acc_log(),
            writer,
        );
    }
}

/// Symbols a sequence-code stream can carry: the widest alphabet, match
/// lengths, has 53 codes (RFC 8878 3.1.1.3.2.1.1), under 64.
const SEQUENCE_CODE_SYMBOLS: usize = 64;

/// The parts of the three sequence streams' tables the sequence loop reads —
/// transforms, next states, sizes — side by side at fixed offsets, so the
/// loop reaches all three from one base and the registers three table
/// pointers held stay free for the stream states, which are on the loop's
/// dependency chain. A stream in RLE mode is a one-state table whose
/// transitions write nothing, as upstream's `FSE_buildCTable_rle`.
pub(crate) struct SequenceStateTables {
    symbol_tt: [[SymbolTT; SEQUENCE_CODE_SYMBOLS]; 3],
    /// Written only up to each stream's table size, the only part read.
    next: [[MaybeUninit<u16>; MAX_FSE_TABLE_SIZE]; 3],
    table_size: [usize; 3],
}

impl SequenceStateTables {
    /// Build the view in `storage` from the LL / ML / OF tables, `None` for a
    /// stream in RLE mode. Each stream is written once, straight from its
    /// table, with nothing cleared first.
    #[inline(always)]
    pub(crate) fn init<'view>(
        storage: &'view mut MaybeUninit<Self>,
        tables: [Option<&FSETable>; 3],
    ) -> &'view Self {
        let view = storage.as_mut_ptr();
        // SAFETY: `view` is the storage's own pointer and the streams are
        // `0..3`. The three calls write every transform and every size, so
        // what is left unwritten lies in `next` past each size, which is
        // `MaybeUninit`.
        unsafe {
            Self::write_stream(view, 0, tables[0]);
            Self::write_stream(view, 1, tables[1]);
            Self::write_stream(view, 2, tables[2]);
            storage.assume_init_ref()
        }
    }

    /// Write stream `stream`'s parts from `table`, or an RLE stream's
    /// one-state table for `None`.
    ///
    /// # Safety
    /// `view` points at storage for a view, valid for writes, and `stream < 3`.
    #[inline(always)]
    unsafe fn write_stream(view: *mut Self, stream: usize, table: Option<&FSETable>) {
        debug_assert!(stream < 3);
        // SAFETY: the caller's contract puts both rows inside the storage.
        let (tt, next) = unsafe {
            (
                (&raw mut (*view).symbol_tt)
                    .cast::<[SymbolTT; SEQUENCE_CODE_SYMBOLS]>()
                    .add(stream),
                (&raw mut (*view).next)
                    .cast::<[MaybeUninit<u16>; MAX_FSE_TABLE_SIZE]>()
                    .add(stream)
                    .cast::<u16>(),
            )
        };
        let size = match table {
            Some(table) => {
                let transforms = &table.symbol_tt[..SEQUENCE_CODE_SYMBOLS];
                let size = table.table_size;
                let states = &table.state_table_flat[..size];
                debug_assert!(size <= MAX_FSE_TABLE_SIZE);
                // SAFETY: a row holds `SEQUENCE_CODE_SYMBOLS` transforms and
                // `MAX_FSE_TABLE_SIZE >= size` states; the table is a
                // different allocation.
                unsafe {
                    tt.cast::<SymbolTT>()
                        .copy_from_nonoverlapping(transforms.as_ptr(), SEQUENCE_CODE_SYMBOLS);
                    next.copy_from_nonoverlapping(states.as_ptr(), size);
                }
                size
            }
            None => {
                // One state: `nb_bits = (1 + 0) >> 16 = 0`, slot `1 - 1 = 0`.
                // SAFETY: inside the rows, as above.
                unsafe {
                    tt.write(
                        [SymbolTT {
                            delta_nb_bits: 0,
                            delta_find_state: -1,
                        }; SEQUENCE_CODE_SYMBOLS],
                    );
                    next.write(0);
                }
                1
            }
        };
        // SAFETY: `stream < 3`, inside the sizes.
        unsafe {
            (&raw mut (*view).table_size)
                .cast::<usize>()
                .add(stream)
                .write(size);
        }
    }

    /// Stream `S`'s transition on `symbol` out of state `idx`, as
    /// [`FSETable::next_state`].
    #[inline(always)]
    pub(crate) fn next_state<const S: usize>(&self, symbol: u8, idx: usize) -> State {
        let tt = self.symbol_tt[S][symbol as usize % SEQUENCE_CODE_SYMBOLS];
        let size = self.table_size[S];
        let value = (size + idx) as u32;
        let nb_bits = ((value + tt.delta_nb_bits) >> 16) as usize;
        let slot = ((value >> nb_bits) as i32 + tt.delta_find_state) as usize;
        debug_assert!(
            slot < size,
            "next-state slot {slot} outside a {size}-entry table"
        );
        State {
            num_bits: nb_bits as u8,
            // SAFETY: `slot < table_size <= MAX_FSE_TABLE_SIZE` (asserted
            // above, held by the build that filled this stream), and the
            // view wrote every state below its table size.
            index: unsafe { self.next[S].get_unchecked(slot).assume_init() } as usize,
        }
    }
}

/// Write the NCount description of the normalized distribution of `len`
/// symbols, `prob(i)` the probability of symbol `i`, at `acc_log` (RFC 8878
/// 4.1.1), padded to a byte. The probabilities are read where they live, so a
/// built table and a bare distribution share the writer without a copy.
fn write_ncount<V: AsMut<Vec<u8>>>(
    len: usize,
    prob: impl Fn(usize) -> i32 + Copy,
    acc_log: u8,
    writer: &mut BitWriter<V>,
) {
    assert!(
        writer.index().is_multiple_of(8),
        "FSE table headers must start on a byte boundary"
    );
    #[cfg(debug_assertions)]
    let start_idx = writer.index();
    // The description goes through a burst, upstream's local bit container
    // (`FSE_writeNCount_generic`): a checked write per field reloaded and
    // stored the writer's state each time. A field is at most `acc_log + 1
    // <= 13` bits and every add is flushed, so the container never holds more
    // than 7 + 13 bits. A field costs at most `acc_log + 1` bits plus 2
    // closing a zero run. At most `1 << acc_log` fields carry a probability,
    // and a zero field opens a run that swallows the zeros after it, so zero
    // fields number at most one more; a zero inside a run costs 2/3 of a bit.
    // With the 4-bit log and the pad, that bounds what is committed without
    // sizing it by the whole alphabet.
    debug_assert!(acc_log <= 12, "an FSE accuracy log of {acc_log}");
    let fields = len.min(2 * (1 << acc_log) + 1);
    let room = (4 + fields * (acc_log as usize + 3) + len).div_ceil(8) + 1;
    let mut burst = writer.burst(room);
    // SAFETY (every add and flush below): fields fit beside the at most 7
    // bits a flush leaves, are clean above their width, and the room covers
    // every committed byte, as counted above.
    unsafe {
        burst.add(u64::from(acc_log - 5), 4);
        burst.flush();
    }
    let mut probability_counter = 0usize;
    let probability_sum = 1 << acc_log;

    let mut prob_idx = 0;
    while probability_counter < probability_sum {
        let max_remaining_value = probability_sum - probability_counter + 1;
        let bits_to_write = max_remaining_value.ilog2() + 1;
        let low_threshold = ((1 << bits_to_write) - 1) - (max_remaining_value);
        let mask = (1 << (bits_to_write - 1)) - 1;

        let p = prob(prob_idx);
        prob_idx += 1;
        let value = (p + 1) as u32;
        let (field, width) = if value < low_threshold as u32 {
            (value, bits_to_write as usize - 1)
        } else if value > mask {
            (value + low_threshold as u32, bits_to_write as usize)
        } else {
            (value, bits_to_write as usize)
        };
        unsafe {
            burst.add(u64::from(field), width);
            burst.flush();
        }

        if p == -1 {
            probability_counter += 1;
        } else if p > 0 {
            probability_counter += p as usize;
        } else {
            let mut zeros = 0u64;
            while prob_idx < len && prob(prob_idx) == 0 {
                zeros += 1;
                prob_idx += 1;
                if zeros == 3 {
                    unsafe {
                        burst.add(3, 2);
                        burst.flush();
                    }
                    zeros = 0;
                }
            }
            unsafe {
                burst.add(zeros, 2);
                burst.flush();
            }
        }
    }
    let pad = burst.misaligned();
    unsafe {
        burst.add(0, pad);
        burst.flush();
    }
    burst.finish();
    #[cfg(debug_assertions)]
    {
        let written_bits = writer.index() - start_idx;
        let computed = ncount_header_bits(len, prob, acc_log);
        debug_assert_eq!(
            written_bits, computed,
            "ncount_header_bits() mismatch: written={written_bits}, computed={computed}"
        );
    }
}

/// A transition's outcome: how many bits the encoder must emit for it, and
/// where the state lands.
///
/// Two fields and no more, so the whole thing comes back in registers on a
/// 64-bit ABI rather than through the stack — this is returned three times per
/// sequence. The transition's baseline is deliberately absent: a caller wanting
/// the bits to emit takes `index & ((1 << num_bits) - 1)` from the state it came
/// from, since the baseline is that same index with those low bits cleared.
/// Upstream leans on the same identity, emitting `statePtr->value` directly
/// under a width rather than subtracting anything (`FSE_encodeSymbol`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct State {
    /// How many bits the range of this state needs to be encoded as
    pub(crate) num_bits: u8,
    /// Index of this state in the decoding table
    pub(crate) index: usize,
}

/// The bits a transition out of `index` emits, given its width.
///
/// `index - baseline` where `baseline = index & !mask`, which is `index & mask`.
#[inline(always)]
pub(crate) fn transition_bits(index: usize, num_bits: u8) -> usize {
    index & ((1usize << num_bits) - 1)
}

#[cfg(any(test, feature = "fuzz-exports"))]
pub fn build_table_from_data(
    data: impl Iterator<Item = u8>,
    max_log: u8,
    avoid_0_numbit: bool,
) -> FSETable {
    let mut counts = [0; 256];
    let mut max_symbol = 0;
    for x in data {
        counts[x as usize] += 1;
    }
    for (idx, count) in counts.iter().copied().enumerate() {
        if count > 0 {
            max_symbol = idx;
        }
    }
    build_table_from_counts(&counts[..=max_symbol], max_log, avoid_0_numbit)
}

#[cfg(test)]
pub(crate) fn build_table_from_symbol_counts(
    counts: &[usize],
    max_log: u8,
    avoid_0_numbit: bool,
) -> FSETable {
    build_table_from_counts(counts, max_log, avoid_0_numbit)
}

/// `build_table_from_symbol_counts` writing into a caller-owned table.
pub(crate) fn build_table_from_symbol_counts_into(
    counts: &[usize],
    max_log: u8,
    avoid_0_numbit: bool,
    out: &mut FSETable,
) {
    build_table_from_counts_into(counts, max_log, avoid_0_numbit, out);
}

/// Build the FSE CTable for a sequence stream (LL / ML / OF), applying upstream
/// zstd's last-symbol adjustment (`ZSTD_buildCTable`,
/// zstd_compress_sequences.c:269-278): the final sequence's symbol is emitted
/// through the FSE initial state, not a normal transition, so when its count
/// exceeds 1 upstream drops one occurrence from the histogram and normalizes
/// against `nbSeq - 1`. The table-log is still derived from the full `nbSeq`.
/// When the count is exactly 1 upstream adjusts NEITHER — it gates both
/// `count[last]--` and `nbSeq_1--` on `count > 1` (zstd_compress_sequences.c:271-273),
/// so the histogram and the normalization total both stay at the full `nbSeq`
/// (the `else { total }` branch below). `last_code` is the stream code of the
/// last sequence (an index into `counts`, `<= max_symbol`). `use_low_prob_count`
/// follows `nbSeq_1` exactly as upstream's `ZSTD_useLowProbCount(nbSeq_1)`.
pub(crate) fn build_seq_ctable_into(
    counts: &mut [usize],
    max_log: u8,
    last_code: usize,
    out: &mut FSETable,
) {
    let total = counts.iter().sum::<usize>();
    let max_symbol = counts
        .iter()
        .rposition(|&count| count > 0)
        .unwrap_or_default();
    // Derived from the full nbSeq, matching upstream (the adjustment touches only
    // the normalized distribution, never the table-log).
    let table_log = optimal_table_log(max_log, total, max_symbol, 2);
    let mut probs_buf = [MaybeUninit::<i32>::uninit(); 256];
    let probs = zeroed_prefix(&mut probs_buf, max_symbol + 1);
    // Borrow the caller's histogram and drop the last sequence's symbol in place
    // for the normalize, then put it back — no per-table copy. `normalize_counts`
    // only re-borrows `counts` immutably, so it sees the decrement, and the
    // restore leaves the selection-time histogram untouched for the caller.
    // C gates BOTH the decrement and `nbSeq_1--` on `count > 1`
    // (zstd_compress_sequences.c:271-273). A count of exactly 1 therefore leaves
    // the histogram unchanged AND normalizes against the full `total` (= nbSeq),
    // NOT `total - 1` — using `total - 1` here would diverge from C and break the
    // byte-parity this establishes.
    let adjust = counts[last_code] > 1;
    if adjust {
        counts[last_code] -= 1;
    }
    let norm_total = if adjust { total - 1 } else { total };
    normalize_counts(
        probs,
        table_log,
        &counts[..=max_symbol],
        norm_total,
        max_symbol,
        norm_total >= 2048,
    );
    if adjust {
        counts[last_code] += 1;
    }
    build_table_from_probabilities_into(probs, table_log, out);
}

/// The first `len` slots of a stack buffer, zeroed. A build touches its own
/// alphabet: a clear of the whole buffer pulls a kibibyte of cold stack into
/// the cache on a path that runs several times a frame.
fn zeroed_prefix(buf: &mut [MaybeUninit<i32>; 256], len: usize) -> &mut [i32] {
    let live = &mut buf[..len];
    live.fill(MaybeUninit::new(0));
    // SAFETY: every slot of `live` was written just above, and `MaybeUninit<i32>`
    // has the layout of `i32`.
    unsafe { core::slice::from_raw_parts_mut(live.as_mut_ptr().cast::<i32>(), len) }
}

/// Write the description of `counts` normalized at exactly `table_log`, with
/// low-probability symbols kept at `-1` (upstream zstd `FSE_normalizeCount`
/// given the log outright, then `FSE_writeNCount`, as the dictionary finalizer
/// calls them, zdict.c `ZDICT_analyzeEntropy`). No table is built. `counts`
/// must not be too spread for the log: every symbol present needs a slot.
#[cfg(feature = "dict-builder")]
pub(crate) fn write_ncount_at_log<V: AsMut<Vec<u8>>>(
    counts: &[usize],
    table_log: u8,
    writer: &mut BitWriter<V>,
) {
    let total = counts.iter().sum::<usize>();
    let max_symbol = counts
        .iter()
        .rposition(|&count| count > 0)
        .unwrap_or_default();
    debug_assert!(total > 1 && table_log >= min_table_log(total, max_symbol));
    let mut probs = [0i32; 256];
    normalize_counts(
        &mut probs[..=max_symbol],
        table_log,
        &counts[..=max_symbol],
        total,
        max_symbol,
        true,
    );
    write_ncount(max_symbol + 1, |i| probs[i], table_log, writer);
}

#[cfg(any(test, feature = "fuzz-exports"))]
fn build_table_from_counts(counts: &[usize], max_log: u8, avoid_0_numbit: bool) -> FSETable {
    let mut out = FSETable::blank();
    build_table_from_counts_into(counts, max_log, avoid_0_numbit, &mut out);
    out
}

/// `build_table_from_counts` writing into a caller-owned table.
pub(crate) fn build_table_from_counts_into(
    counts: &[usize],
    max_log: u8,
    avoid_0_numbit: bool,
    out: &mut FSETable,
) {
    let total = counts.iter().sum::<usize>();
    // FSE table construction needs at least two samples in the histogram.
    // A single-distinct-symbol histogram (e.g. `[N, 0, ...]`) with `total
    // >= 2` is still acceptable here — some internal fallback paths feed
    // a phantom zero-count second slot when they want an FSE table for
    // an effectively-RLE distribution.
    assert!(
        total > 1,
        "FSE table requires at least 2 samples in the histogram (got {total})"
    );
    let max_symbol = counts
        .iter()
        .rposition(|&count| count > 0)
        .unwrap_or_default();
    let table_log = optimal_table_log(max_log, total, max_symbol, 2);
    let mut probs_buf = [MaybeUninit::<i32>::uninit(); 256];
    let probs = zeroed_prefix(&mut probs_buf, counts.len());
    normalize_counts(probs, table_log, counts, total, max_symbol, avoid_0_numbit);
    build_table_from_probabilities_into(probs, table_log, out);
}

/// Bit size of the serialized FSE NCount table header for the normalized
/// distribution of `len` symbols, `prob(i)` the probability of symbol `i`, at
/// `acc_log`. This is exactly what [`FSETable::table_header_bits`] returns
/// (they share this implementation), but computed from the normalized counts
/// alone — no spread / state-table / `symbolTT` construction. The table-mode
/// selector uses it to price a candidate custom table without building its
/// (otherwise discarded) state tables.
fn ncount_header_bits(len: usize, prob: impl Fn(usize) -> i32, acc_log: u8) -> usize {
    let mut bits = 4; // acc_log - 5
    let mut probability_counter = 0usize;
    let probability_sum = 1usize << acc_log;

    let mut prob_idx = 0;
    while probability_counter < probability_sum {
        let max_remaining_value = probability_sum - probability_counter + 1;
        let bits_to_write = max_remaining_value.ilog2() + 1;
        let low_threshold = ((1 << bits_to_write) - 1) - max_remaining_value;

        let p = prob(prob_idx);
        prob_idx += 1;
        let value = (p + 1) as u32;
        if value < low_threshold as u32 {
            bits += bits_to_write as usize - 1;
        } else {
            bits += bits_to_write as usize;
        }

        if p == -1 {
            probability_counter += 1;
        } else if p > 0 {
            probability_counter += p as usize;
        } else {
            let mut zeros = 0u8;
            while prob_idx < len && prob(prob_idx) == 0 {
                zeros += 1;
                prob_idx += 1;
                if zeros == 3 {
                    bits += 2;
                    zeros = 0;
                }
            }
            bits += 2;
        }
    }
    // Byte-alignment padding
    let misaligned = bits % 8;
    if misaligned != 0 {
        bits += 8 - misaligned;
    }
    bits
}

/// Header bit cost of the custom FSE table that
/// `build_table_from_symbol_counts` would produce for `counts`, without
/// building it. Mirrors the front of `build_table_from_counts` (table-log
/// selection + count normalization) and then sizes the header via
/// [`ncount_header_bits`]. Returns the exact value the built table's
/// `table_header_bits()` would, so the table-mode selection stays
/// byte-identical while skipping the state-table build for candidates that
/// lose to the predefined / repeated table.
pub(crate) fn fse_header_bits_for_counts(
    counts: &[usize],
    max_log: u8,
    avoid_0_numbit: bool,
) -> usize {
    let total = counts.iter().sum::<usize>();
    // Mirror `build_table_from_counts`'s contract: pricing must reject the
    // same inputs the builder rejects. A histogram with fewer than two
    // samples cannot form a valid FSE table, and the table-log selection
    // below takes an integer logarithm of `total` that underflows on
    // `total <= 1`. Trip the guard up front so the failure mode matches
    // the builder instead of an opaque arithmetic panic.
    assert!(
        total > 1,
        "FSE table requires at least 2 samples in the histogram (got {total})"
    );
    let max_symbol = counts
        .iter()
        .rposition(|&count| count > 0)
        .unwrap_or_default();
    let table_log = optimal_table_log(max_log, total, max_symbol, 2);
    let mut probs_buf = [MaybeUninit::<i32>::uninit(); 256];
    let probs = zeroed_prefix(&mut probs_buf, counts.len());
    normalize_counts(probs, table_log, counts, total, max_symbol, avoid_0_numbit);
    ncount_header_bits(counts.len(), |i| probs[i], table_log)
}

fn min_table_log(total: usize, max_symbol: usize) -> u8 {
    let min_bits_src = total.ilog2() + 1;
    let min_bits_symbols = if max_symbol == 0 {
        2
    } else {
        max_symbol.ilog2() + 2
    };
    min_bits_src.min(min_bits_symbols) as u8
}

/// Upstream `FSE_optimalTableLog_internal` (fse_compress.c:357). `minus` is the
/// accuracy reduction applied to the source-size bit estimate: FSE passes `2`,
/// HUF passes `1` (huf_compress.c:1286). Reused by the HUF cheap/fast tableLog
/// path so our single-shot literal tableLog matches upstream's exactly.
pub(crate) fn optimal_table_log(
    max_table_log: u8,
    total: usize,
    max_symbol: usize,
    minus: u8,
) -> u8 {
    let min_bits = min_table_log(total, max_symbol);
    let mut table_log = max_table_log;
    // Upstream subtracts in unsigned arithmetic: a source too small for the
    // reduction wraps to a huge bound, so it reduces nothing.
    if let Some(max_bits_src) = (total - 1).ilog2().checked_sub(u32::from(minus))
        && max_bits_src < u32::from(table_log)
    {
        table_log = max_bits_src as u8;
    }
    if min_bits > table_log {
        table_log = min_bits;
    }
    table_log.clamp(5, 12)
}

fn normalize_counts(
    normalized: &mut [i32],
    table_log: u8,
    counts: &[usize],
    total: usize,
    max_symbol: usize,
    use_low_prob_count: bool,
) {
    const RTB_TABLE: [u64; 8] = [
        0, 473_195, 504_333, 520_860, 550_000, 700_000, 750_000, 830_000,
    ];
    let low_prob_count = if use_low_prob_count { -1 } else { 1 };
    let scale = 62 - table_log as usize;
    let step = (1u64 << 62) / total as u64;
    let v_step = 1u64 << (scale - 20);
    let low_threshold = total >> table_log;
    let mut still_to_distribute = 1i32 << table_log;
    let mut largest = 0usize;
    let mut largest_probability = 0i32;

    // Both cut to the live symbols once, so the loop tests no index against
    // either length and keeps neither length live.
    let live = max_symbol + 1;
    for (symbol, (slot, &count)) in normalized[..live]
        .iter_mut()
        .zip(&counts[..live])
        .enumerate()
    {
        if count == 0 {
            *slot = 0;
        } else if count <= low_threshold {
            *slot = low_prob_count;
            still_to_distribute -= 1;
        } else {
            let product = count as u64 * step;
            let mut probability = (product >> scale) as i32;
            if probability < 8 {
                let rest_to_beat = v_step * RTB_TABLE[probability as usize];
                probability +=
                    u64::from(product - ((probability as u64) << scale) > rest_to_beat) as i32;
            }
            if probability > largest_probability {
                largest_probability = probability;
                largest = symbol;
            }
            *slot = probability;
            still_to_distribute -= probability;
        }
    }

    if -still_to_distribute >= normalized[largest] >> 1 {
        normalize_m2(
            normalized,
            table_log,
            counts,
            total,
            max_symbol,
            low_prob_count,
        );
    } else {
        normalized[largest] += still_to_distribute;
    }

    debug_assert_eq!(
        normalized
            .iter()
            .take(max_symbol + 1)
            .map(|&probability| probability.unsigned_abs() as usize)
            .sum::<usize>(),
        1usize << table_log
    );
}

fn normalize_m2(
    normalized: &mut [i32],
    table_log: u8,
    counts: &[usize],
    mut total: usize,
    max_symbol: usize,
    low_prob_count: i32,
) {
    const NOT_YET_ASSIGNED: i32 = -2;
    let low_threshold = total >> table_log;
    let mut low_one = (total * 3) >> (table_log as usize + 1);
    let mut distributed = 0usize;

    for symbol in 0..=max_symbol {
        let count = counts[symbol];
        if count == 0 {
            normalized[symbol] = 0;
        } else if count <= low_threshold {
            normalized[symbol] = low_prob_count;
            distributed += 1;
            total -= count;
        } else if count <= low_one {
            normalized[symbol] = 1;
            distributed += 1;
            total -= count;
        } else {
            normalized[symbol] = NOT_YET_ASSIGNED;
        }
    }

    let mut to_distribute = (1usize << table_log) - distributed;
    if to_distribute == 0 {
        return;
    }

    if total / to_distribute > low_one {
        low_one = (total * 3) / (to_distribute * 2);
        for symbol in 0..=max_symbol {
            if normalized[symbol] == NOT_YET_ASSIGNED && counts[symbol] <= low_one {
                normalized[symbol] = 1;
                distributed += 1;
                total -= counts[symbol];
            }
        }
        to_distribute = (1usize << table_log) - distributed;
    }

    if distributed == max_symbol + 1 {
        let max_symbol = counts
            .iter()
            .copied()
            .take(max_symbol + 1)
            .enumerate()
            .max_by_key(|&(_, count)| count)
            .map(|(symbol, _)| symbol)
            .unwrap_or_default();
        normalized[max_symbol] += to_distribute as i32;
        return;
    }

    if total == 0 {
        let mut symbol = 0usize;
        while to_distribute > 0 {
            if normalized[symbol] > 0 {
                normalized[symbol] += 1;
                to_distribute -= 1;
            }
            symbol = (symbol + 1) % (max_symbol + 1);
        }
        return;
    }

    let v_step_log = 62 - table_log as usize;
    let mid = (1u64 << (v_step_log - 1)) - 1;
    let r_step = (((1u64 << v_step_log) * to_distribute as u64) + mid) / total as u64;
    let mut tmp_total = mid;
    for symbol in 0..=max_symbol {
        if normalized[symbol] == NOT_YET_ASSIGNED {
            let end = tmp_total + counts[symbol] as u64 * r_step;
            let start_bucket = tmp_total >> v_step_log;
            let end_bucket = end >> v_step_log;
            let weight = end_bucket - start_bucket;
            assert!(
                weight >= 1,
                "upstream zstd FSE normalization produced zero weight"
            );
            normalized[symbol] = weight as i32;
            tmp_total = end;
        }
    }
}

pub(super) fn build_table_from_probabilities(probs: &[i32], acc_log: u8) -> FSETable {
    let mut out = FSETable::blank();
    build_table_from_probabilities_into(probs, acc_log, &mut out);
    out
}

/// [`build_table_from_probabilities`] writing into a caller-owned table.
///
/// The table is eleven kilobytes of inline arrays, so returning one builds it
/// on the stack and then moves it wherever it is going. Filling the destination
/// directly is what upstream does (`FSE_buildCTable_wksp` writes into the
/// block state's CTable), and it is what lets a compressor keep two of these
/// and alternate between them instead of producing a new one per block.
pub(super) fn build_table_from_probabilities_into(probs: &[i32], acc_log: u8, out: &mut FSETable) {
    let table_size: usize = 1 << acc_log;
    // Disjoint field borrows, so the phases below can write the state table
    // while reading the per-symbol accounting.
    let FSETable {
        table_size: out_table_size,
        probabilities,
        state_table_flat,
        symbol_tt,
        live_symbols,
    } = out;
    *out_table_size = table_size;
    // The destination may be a table from an earlier block: reset what the
    // build below does not overwrite in full. `state_table_flat` needs none of
    // it, the scatter fills every slot under `table_size` and nothing reads
    // past it; `symbol_tt` and `probabilities` are written only for
    // `probs.len()` symbols, so what an earlier build wrote above that has to
    // be cleared rather than inherited. Past `live_symbols` the arrays are
    // already default, so the clear stops there.
    if *live_symbols > probs.len() {
        probabilities[probs.len()..*live_symbols].fill(0);
        symbol_tt[probs.len()..*live_symbols].fill(SymbolTT::default());
    }
    probabilities[..probs.len()].copy_from_slice(probs);
    *live_symbols = probs.len();

    // Upstream zstd `FSE_buildCTable_wksp` (lib/compress/fse_compress.c) — build
    // `nextStateTable` (== `state_table_flat`) once via cumul + spread +
    // sorted-by-symbol sweep, without ever materializing per-symbol
    // `Vec<State>`. Previous implementation paid an O(num_symbols ×
    // table_size) enumeration with a BTreeSet dedup per symbol —
    // ~18% self time on small-input encode profiles. Drop it; upstream zstd
    // only ever needs symbolTT + nextStateTable, and we precompute
    // `start_state` / `max_num_bits` for callers via the same
    // arithmetic [`FSETable::next_state`] uses on the hot path.
    //
    // Phase 1 — distribute `-1` (low-probability) symbols at the top
    // of the table; bump the high-threshold cursor down. Build the
    // `cumul` prefix-sum table that maps each symbol to its first
    // `nextStateTable` slot in sorted-by-symbol layout.
    // `table_symbol` is a local scratch (consumed to fill `state_table_flat`,
    // never returned), bounded by the FSE sequence/weight table-log cap
    // (`table_size = 1 << table_log <= 1 << 9 = 512`). Use a stack buffer so the
    // per-build heap alloc + zero is gone. `[..table_size]` keeps the live region
    // exactly as the old `vec![0u8; table_size]`.
    // Always-on (not debug_assert): the stack scratch is fixed at 512 bytes
    // (accuracy_log <= 9). `to_encoder_table` already rejects acc_log > 9, but
    // this function is `pub(crate)` — guard the scratch bound here too so a
    // future caller can never index the buffer past its length (the `[..
    // table_size]` slice would panic on the bound; this fires first with a clear
    // message instead of an opaque slice-index panic).
    assert!(
        table_size <= MAX_FSE_TABLE_SIZE,
        "FSE table_size {table_size} exceeds the {MAX_FSE_TABLE_SIZE}-byte encoder \
         scratch (accuracy_log must be <= 9)"
    );
    let mut table_symbol_buf = [MaybeUninit::<u8>::uninit(); MAX_FSE_TABLE_SIZE];
    let table_symbol = table_symbol_buf.as_mut_ptr().cast::<u8>();
    let mut high_threshold = (table_size - 1) as isize;
    // `cumul` / running prefix-sum holds slot counts up to `table_size`.
    // Decoder accepts `accuracy_log` up to `ENTRY_MAX_ACCURACY_LOG = 16`
    // and `fse_decoder::FSETable::to_encoder_table` round-trips through
    // this builder; at `acc_log == 16` the prefix sum reaches 65 536
    // which overflows `u16` (max 65 535). Keep `cumul` / `cursor` at
    // `u32` so the cumulative count is representable for every valid
    // `acc_log`. Slot indices written into `state_table_flat` stay in
    // `0..table_size-1` (≤ u16::MAX) and remain `u16` — only the
    // running cursor needs the wider type.
    //
    // Written only for this alphabet and walked in place by the scatter below:
    // sequence tables have a few dozen symbols, and a zeroed copy of all 257
    // entries is two kibibytes of cold stack per build.
    let mut cumul = [MaybeUninit::<u32>::uninit(); 257];
    let mut cumul_total = 0u32;
    cumul[0].write(0);
    for (symbol, &prob) in probs.iter().enumerate() {
        let bump: u32 = match prob {
            -1 => {
                assert!(
                    high_threshold >= 0,
                    "FSE low-probability symbols overflow the table"
                );
                // SAFETY: `0 <= high_threshold < table_size`, which the assert
                // above bounds by the buffer's length.
                unsafe {
                    table_symbol
                        .add(high_threshold as usize)
                        .write(symbol as u8)
                };
                high_threshold -= 1;
                1
            }
            p if p > 0 => p as u32,
            _ => 0,
        };
        cumul_total += bump;
        cumul[symbol + 1].write(cumul_total);
    }

    // Phase 2 — spread positive-probability symbols across the
    // remaining slots, upstream zstd `step`-walk with low-prob area skip.
    let step = (table_size >> 1) + (table_size >> 3) + 3;
    let table_mask = table_size - 1;
    let mut position: usize = 0;
    if high_threshold == (table_size - 1) as isize && table_size >= 2 {
        // No low-probability symbol, so no slot to skip: lay the symbols out in
        // order, eight bytes at a time, and read them across in a loop with no
        // per-symbol count to mispredict (upstream `FSE_buildCTable_wksp`,
        // fse_compress.c:116-153).
        let mut spread = [core::mem::MaybeUninit::<u8>::uninit(); MAX_FSE_TABLE_SIZE + 8];
        let spread_ptr = spread.as_mut_ptr().cast::<u8>();
        let mut laid_out = 0usize;
        let mut run = 0u64;
        for &prob in probs {
            debug_assert!(prob >= 0);
            let count = prob as usize;
            assert!(
                laid_out + count <= table_size,
                "FSE probabilities must sum to the table size"
            );
            // SAFETY: the assert keeps this run inside `table_size`, and each
            // write ends within the eight spare bytes past it.
            unsafe {
                spread_ptr.add(laid_out).cast::<u64>().write_unaligned(run);
                let mut written = 8;
                while written < count {
                    spread_ptr
                        .add(laid_out + written)
                        .cast::<u64>()
                        .write_unaligned(run);
                    written += 8;
                }
            }
            laid_out += count;
            run = run.wrapping_add(0x0101_0101_0101_0101);
        }
        assert_eq!(
            laid_out, table_size,
            "FSE probabilities must sum to the table size"
        );
        let mut slot = 0;
        while slot < table_size {
            // SAFETY: `slot + 1 < table_size`, the size being even, and every
            // byte below `table_size` was written above; masked positions are
            // below `table_size`, within `table_symbol_buf`.
            unsafe {
                table_symbol
                    .add(position)
                    .write(spread_ptr.add(slot).read());
                table_symbol
                    .add((position + step) & table_mask)
                    .write(spread_ptr.add(slot + 1).read());
            }
            position = (position + 2 * step) & table_mask;
            slot += 2;
        }
    } else {
        for (symbol, &prob) in probs.iter().enumerate() {
            if prob <= 0 {
                continue;
            }
            for _ in 0..prob {
                // SAFETY: `position` is masked below `table_size`.
                unsafe { table_symbol.add(position).write(symbol as u8) };
                position = (position + step) & table_mask;
                while (position as isize) > high_threshold {
                    position = (position + step) & table_mask;
                }
            }
        }
    }
    // These two invariants are the SAFETY contract for the uninitialized
    // `table_symbol_buf` read below: the spread wrote every index in
    // `[0, table_size)` exactly once only if (a) it cycled exactly once through
    // the table (so each symbol got exactly its `cumul`-width run of slots) and
    // (b) the `cumul` cursors sum to `table_size` (so the runs partition the
    // whole table). They are asserted in ALL build profiles (not
    // `debug_assert`) so a regression in probability normalization panics
    // loudly here instead of leaving uninitialized slots that read as UB.
    assert_eq!(
        position, 0,
        "FSE spread must cycle exactly once through tableSize positions"
    );
    assert_eq!(
        cumul_total as usize, table_size,
        "FSE cumul cursors must sum to table_size before building nextStateTable"
    );
    // SAFETY: the asserts above establish that the first `table_size` bytes are
    // written, and `table_size <= MAX_FSE_TABLE_SIZE`.
    let table_symbol = unsafe { core::slice::from_raw_parts(table_symbol, table_size) };

    // Phase 3 — emit `state_table_flat` (upstream zstd `nextStateTable`)
    // ordered by `(symbol, slot)`. Walk every table slot `u`, look up
    // its owning symbol via `table_symbol[u]`, and write the raw slot
    // `u` into that symbol's running cumul cursor. The Rust convention
    // stores raw slots (`0..table_size`); upstream zstd stores `table_size + u`
    // pre-shifted and recovers `u` on read by subtracting `table_size`.
    // Both representations encode the same `(symbol → next_slot)`
    // mapping; [`FSETable::next_state`] is written against the raw-slot
    // convention so the pre-shift is intentionally skipped here.
    // Every index in `[0, table_size)` is written EXACTLY once by the scatter
    // loop below: the `cumul` cursors partition the table bijectively, which the
    // two asserts above enforce in every build profile. Slots at or past
    // `table_size` are never read — `next_state` indexes within the live region
    // — so the tail keeps whatever the zero-init left and costs nothing to
    // carry. The table lives inline in [`FSETable`], so this build allocates
    // nothing at all.
    for (u, &symbol_at_slot) in table_symbol.iter().enumerate() {
        // SAFETY: a slot holds a symbol below `probs.len()`, and `cumul` was
        // written for every index up to `probs.len()`.
        let cursor = unsafe { cumul[symbol_at_slot as usize].assume_init_mut() };
        // The Rust convention here keeps `state_table_flat[i]` as the
        // raw slot (`0..table_size`); upstream zstd stores `table_size + u`
        // and subtracts on read. `next_state` arithmetic ([`FSETable::next_state`])
        // matches the Rust convention — store the slot directly.
        state_table_flat[*cursor as usize] = u as u16;
        *cursor += 1;
    }

    // Phase 4 — `symbolTT[]` (delta_nb_bits, delta_find_state), upstream zstd
    // 16.16 fixed-point arithmetic; no per-state enumeration.
    let mut total: usize = 0;
    for (symbol, &prob) in probs.iter().enumerate() {
        if prob == 0 {
            // Upstream zstd fills `symbolTT` for prob==0 too, so `FSE_getMaxNbBits`
            // still works (returns `acc_log + 1` for absent symbols).
            // We don't expose that path, but mirror the value for parity.
            symbol_tt[symbol] = SymbolTT {
                delta_nb_bits: ((acc_log as u32 + 1) << 16).saturating_sub(1u32 << acc_log),
                delta_find_state: 0,
            };
            continue;
        }
        // `total` and `prob` are at most the 512-entry table, so the
        // difference fits `i32`.
        let (delta_nb_bits, delta_find_state): (u32, i32) = match prob {
            -1 | 1 => (
                ((acc_log as u32) << 16).saturating_sub(1u32 << acc_log),
                total as i32 - 1,
            ),
            p if p > 1 => {
                let p_u32 = p as u32;
                let max_bits_out = (acc_log as u32) - (p_u32 - 1).ilog2();
                let min_state_plus = p_u32 << max_bits_out;
                (
                    (max_bits_out << 16).saturating_sub(min_state_plus),
                    total as i32 - p,
                )
            }
            _ => unreachable!("probability is one of {{-1, 1+}} after the prob==0 gate above"),
        };
        symbol_tt[symbol] = SymbolTT {
            delta_nb_bits,
            delta_find_state,
        };
        total += prob.unsigned_abs() as usize;
    }
}

const ML_DIST: &[i32] = &[
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

const LL_DIST: &[i32] = &[
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

const OF_DIST: &[i32] = &[
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

// The three predefined LL/ML/OF distribution tables are pure functions
// of compile-time constants (`LL_DIST`, `ML_DIST`, `OF_DIST` and fixed
// `acc_log` values). Each `build_table_from_probabilities` call costs
// ~12 µs on a modern x86_64 — multiplied by three, that's ~38 µs of
// pure setup paid on every `FrameCompressor::new`. On a 1 KiB frame
// the actual compression work is ~1-5 µs, so the predefined-table
// build dominates the per-frame cost for tiny inputs.
//
// Cache each predefined table once per process and hand callers a
// `&'static FSETable`. The per-distribution cache slot is encoded by
// the static identity of the cache itself (each helper has its own
// `static`), NOT by pointer comparison on the distribution slice —
// `LL_DIST` / `ML_DIST` / `OF_DIST` are `const`, and `core::ptr::eq`
// on the materialized slice pointer of a `const` is only stable as
// long as rustc keeps lowering it to a single anonymous static. A
// future rustc change that duplicates the const at each use site
// would silently sink every call through a "unknown slice" branch
// and bypass the cache forever. Per-helper `static`s avoid that
// failure mode entirely — the cache slot is selected at compile time
// without consulting the slice pointer at all.
//
// Two implementations:
//
//   * Targets with atomic pointer support (`target_has_atomic = "ptr"`)
//     — `AtomicPtr<FSETable>` lock-free init via `compare_exchange`.
//     Works in both `std` and `no_std` builds; only `alloc` is needed
//     (always available in this crate via `extern crate alloc`).
//     Memory ordering: `Acquire` on the load so the consumer sees a
//     fully-published `FSETable`; `AcqRel` on the compare-exchange
//     so the publishing side both reads any concurrent winner
//     (`Acquire`) and publishes its store (`Release`). The first
//     writer leaks `Box<FSETable>` intentionally — the cache lives
//     for the entire program lifetime, exactly like a `LazyLock`.
//
//   * No-atomic targets (`not(target_has_atomic = "ptr")`, e.g.
//     `thumbv6m-none-eabi`, AVR, MSP430) — two sub-paths driven by
//     the `critical-section` feature:
//
//     - With `critical-section` enabled (the recommended choice for
//       any embedded build that wires up a CS impl from
//       `cortex-m-rt` / `riscv-rt` / `embassy-executor` / `esp-hal`
//       / similar): cached `static mut *mut FSETable` slot
//       protected by `critical_section::with`. First call enters
//       the CS, double-checks the slot, builds and publishes the
//       leaked `Box::into_raw` pointer, exits the CS. Subsequent
//       calls return the cached pointer. The CS is held for the
//       duration of one `build_table_from_probabilities` (~12 µs)
//       on the cold path — interrupts disabled for that window,
//       acceptable for a one-time per-table init.
//
//     - Without `critical-section`: no cache. The helpers return
//       a freshly-built `Box<FSETable>` per call (per-frame cost
//       same as the pre-cache status quo); the table is dropped
//       with the owning `FrameCompressor`, so memory does not
//       grow with the number of `FrameCompressor::new` calls.
//       Lack of pointer-width atomics is **not** a guarantee of
//       non-concurrency (interrupt / preemption reentrancy on
//       bare-metal targets), and a shared `static mut` slot
//       without any synchronization would be a data race (UB).
//       The owned-Box return is the same shape as the pre-PR
//       behaviour — no leak, no UB risk, no cache speedup. Users
//       who want the cache on no-atomic targets should enable
//       the `critical-section` feature, which routes through the
//       CS-protected `&'static` slot above.
//
// To paper over the per-target return-type difference the
// `FseDefaultTable` type alias resolves to `&'static FSETable` on
// any target/feature combination that has a cache (atomic, or
// no-atomic + `critical-section`) and to `alloc::boxed::Box<FSETable>`
// on the cache-less path. Both types `Deref` to `FSETable` so
// downstream consumers in `encoding/blocks/compressed.rs` and
// `encoding/frame_compressor.rs` borrow through `&` uniformly.
#[cfg(target_has_atomic = "ptr")]
fn get_or_init_cached_table(
    cache: &core::sync::atomic::AtomicPtr<FSETable>,
    probs: &[i32],
    acc_log: u8,
) -> &'static FSETable {
    use core::sync::atomic::Ordering;

    let cur = cache.load(Ordering::Acquire);
    if !cur.is_null() {
        // SAFETY: a non-null entry in this cache was published by a
        // previous winner of the `compare_exchange` below, which
        // leaked the `Box<FSETable>` (the cache never frees, mirroring
        // a `LazyLock` lifetime). The pointed-to allocation is
        // therefore valid for `'static` and immutable for the rest
        // of the program, so handing out a `&'static FSETable` to
        // multiple threads is sound.
        return unsafe { &*cur };
    }

    let built = alloc::boxed::Box::new(build_table_from_probabilities(probs, acc_log));
    let raw = alloc::boxed::Box::into_raw(built);
    // `AcqRel` on success rather than the minimal `Release`: the
    // success path doesn't actually need Acquire (no prior loads to
    // synchronise with at the publication point), but matching the
    // failure Acquire ordering keeps the success and failure
    // branches symmetric on rustc's atomic surface — both arms then
    // see the same memory-fence shape for whatever follows. The
    // marginal cost is one extra fence instruction on x86/aarch64;
    // this path runs at most three times per process so it never
    // shows up in a flamegraph.
    match cache.compare_exchange(
        core::ptr::null_mut(),
        raw,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            // We installed the singleton. SAFETY: `raw` is the
            // pointer we just published; no other thread can free it.
            unsafe { &*raw }
        }
        Err(existing) => {
            // Another thread beat us to the publish — reclaim our
            // throwaway allocation and use the winner. SAFETY: we own
            // `raw` here (compare_exchange did NOT store it), so
            // reconstituting the `Box` is sound. `existing` was
            // published by the winning thread and is leaked for
            // `'static`, mirroring the `cur` path.
            drop(unsafe { alloc::boxed::Box::from_raw(raw) });
            unsafe { &*existing }
        }
    }
}

/// No-atomic + no `critical-section` feature fallback: build a
/// fresh `FSETable` and return it as an owned `Box<FSETable>`. No
/// caching — see the module-level header comment for the rationale
/// (interrupt / preemption reentrancy on bare-metal targets makes
/// a shared `static mut` cache without atomic synchronization a
/// data race surface). The owned-Box return shape is paired with
/// the `FseDefaultTable` type alias so the per-frame caller stores
/// the table in `FseTables` and drops it when the compressor
/// drops — no `Box::leak`, no unbounded growth across multiple
/// `FrameCompressor::new` calls.
#[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
fn build_owned_table(probs: &[i32], acc_log: u8) -> alloc::boxed::Box<FSETable> {
    alloc::boxed::Box::new(build_table_from_probabilities(probs, acc_log))
}

/// No-atomic + `critical-section` feature: cached `static mut` slot
/// protected by `critical_section::with`. CS impl supplied by the
/// downstream embedded runtime (`cortex-m-rt`, `riscv-rt`, etc.).
///
/// SAFETY: the slot is only read / written inside the CS, so the
/// "no concurrent first-call initialization" invariant is enforced
/// by interrupt-disable rather than by an atomic primitive. The
/// `Box::leak` lifetime and `&'static FSETable` return shape match
/// the atomic-target path so the public `default_*_table()`
/// surface is identical across all target families.
#[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
fn get_or_init_cached_table_cs(
    cache: &core::cell::UnsafeCell<*mut FSETable>,
    probs: &[i32],
    acc_log: u8,
) -> &'static FSETable {
    critical_section::with(|_cs| {
        // SAFETY: the `critical_section::with` token witnesses that
        // interrupts are disabled for the duration of this closure,
        // so the slot is accessed serially even on multi-IRQ
        // bare-metal runtimes. The `UnsafeCell::get()` raw pointer
        // is dereferenced only inside this CS-protected scope.
        let slot = unsafe { &mut *cache.get() };
        if !slot.is_null() {
            // SAFETY: previously published by a CS-protected write
            // below, `Box::leak`'d → valid for `'static`.
            return unsafe { &**slot };
        }
        let built = alloc::boxed::Box::new(build_table_from_probabilities(probs, acc_log));
        *slot = alloc::boxed::Box::into_raw(built);
        // SAFETY: just assigned a leaked pointer.
        unsafe { &**slot }
    })
}

/// `UnsafeCell<*mut FSETable>` wrapper that is `Sync` because
/// access is gated by `critical_section::with`. Required because
/// `UnsafeCell` is not `Sync` by default and `static` items must
/// be.
#[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
#[repr(transparent)]
struct CsCachedTablePtr(core::cell::UnsafeCell<*mut FSETable>);

#[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
impl CsCachedTablePtr {
    const fn new() -> Self {
        Self(core::cell::UnsafeCell::new(core::ptr::null_mut()))
    }
}

// SAFETY: access to the inner `*mut FSETable` is gated by
// `critical_section::with` in `get_or_init_cached_table_cs`, so
// the slot is accessed serially even on multi-IRQ bare-metal
// runtimes. The CS impl supplied by the downstream runtime
// guarantees mutual exclusion for the duration of the closure.
#[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
unsafe impl Sync for CsCachedTablePtr {}

/// Per-helper return type. `&'static FSETable` on targets/features
/// that own a process-wide cache (zero-cost subsequent calls);
/// `Box<FSETable>` on the cache-less no-atomic path (one allocation
/// per call, dropped with the owning `FrameCompressor` — no leak).
/// Both `Deref` to `FSETable`, so downstream consumers borrow
/// through `&` without caring which arm fired.
#[cfg(any(target_has_atomic = "ptr", feature = "critical-section"))]
pub(crate) type FseDefaultTable = &'static FSETable;
#[cfg(not(any(target_has_atomic = "ptr", feature = "critical-section")))]
pub(crate) type FseDefaultTable = alloc::boxed::Box<FSETable>;

pub(crate) fn default_ml_table() -> FseDefaultTable {
    #[cfg(target_has_atomic = "ptr")]
    {
        static CACHE: core::sync::atomic::AtomicPtr<FSETable> =
            core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
        get_or_init_cached_table(&CACHE, ML_DIST, 6)
    }
    #[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
    {
        static CACHE: CsCachedTablePtr = CsCachedTablePtr::new();
        get_or_init_cached_table_cs(&CACHE.0, ML_DIST, 6)
    }
    #[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
    {
        build_owned_table(ML_DIST, 6)
    }
}

pub(crate) fn default_ll_table() -> FseDefaultTable {
    #[cfg(target_has_atomic = "ptr")]
    {
        static CACHE: core::sync::atomic::AtomicPtr<FSETable> =
            core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
        get_or_init_cached_table(&CACHE, LL_DIST, 6)
    }
    #[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
    {
        static CACHE: CsCachedTablePtr = CsCachedTablePtr::new();
        get_or_init_cached_table_cs(&CACHE.0, LL_DIST, 6)
    }
    #[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
    {
        build_owned_table(LL_DIST, 6)
    }
}

pub(crate) fn default_of_table() -> FseDefaultTable {
    #[cfg(target_has_atomic = "ptr")]
    {
        static CACHE: core::sync::atomic::AtomicPtr<FSETable> =
            core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
        get_or_init_cached_table(&CACHE, OF_DIST, 5)
    }
    #[cfg(all(not(target_has_atomic = "ptr"), feature = "critical-section"))]
    {
        static CACHE: CsCachedTablePtr = CsCachedTablePtr::new();
        get_or_init_cached_table_cs(&CACHE.0, OF_DIST, 5)
    }
    #[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
    {
        build_owned_table(OF_DIST, 5)
    }
}
