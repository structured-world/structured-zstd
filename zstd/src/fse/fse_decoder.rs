use crate::bit_io::{BitReader, BitReaderReversed};
use crate::cpu_kernel::CpuKernel;
use crate::decoding::errors::{FSEDecoderError, FSETableError};
use alloc::vec::Vec;

pub struct FSEDecoder<'table> {
    /// An FSE state value represents an index in the FSE table.
    pub state: Entry,
    /// A reference to the table used for decoding.
    table: &'table FSETable,
}

impl<'t> FSEDecoder<'t> {
    /// Initialize a new Finite State Entropy decoder.
    pub fn new(table: &'t FSETable) -> FSEDecoder<'t> {
        FSEDecoder {
            state: table.decode.first().copied().unwrap_or(Entry {
                new_state: 0,
                symbol: 0,
                num_bits: 0,
                base_value: 0,
                num_additional_bits: 0,
            }),
            table,
        }
    }

    /// Returns the byte associated with the symbol the internal cursor is pointing at.
    pub fn decode_symbol(&self) -> u8 {
        self.state.symbol
    }

    /// Initialize internal state and prepare for decoding. After this, `decode_symbol` can be called
    /// to read the first symbol and `update_state` can be called to prepare to read the next symbol.
    pub fn init_state<K: CpuKernel>(
        &mut self,
        bits: &mut BitReaderReversed<'_, K>,
    ) -> Result<(), FSEDecoderError> {
        if self.table.accuracy_log == 0 {
            return Err(FSEDecoderError::TableIsUninitialized);
        }
        // Externally-constructible-table guard: `FSETable::decode`
        // and `FSETable::accuracy_log` are `pub` fields, so safe
        // external callers (not just `fuzz_exports` harnesses) can
        // hand the decoder a table whose `decode.len()` doesn't match
        // `1 << accuracy_log`. Validate the shape invariant up-front
        // and surface as a typed `InvalidTableShape` error (distinct
        // from `TableIsUninitialized` to keep error triage
        // unambiguous) — without this, `read_entry`'s unchecked
        // indexing (the `cfg(not(fuzz_exports))` arm) hits UB in
        // release builds on a malformed table. `checked_shl` covers
        // the pathological case where `accuracy_log >= usize::BITS`.
        // Branch cost is a single per-call check; the per-sequence
        // hot path (`update_state_fast`) is unaffected.
        let accuracy_log = self.table.accuracy_log;
        let decode_len = self.table.decode.len();
        let expected =
            1usize
                .checked_shl(accuracy_log.into())
                .ok_or(FSEDecoderError::InvalidTableShape {
                    decode_len,
                    accuracy_log,
                })?;
        if decode_len != expected {
            return Err(FSEDecoderError::InvalidTableShape {
                decode_len,
                accuracy_log,
            });
        }
        let new_state = bits.get_bits(self.table.accuracy_log);
        // SAFETY: `accuracy_log` bits read from the bitstream produce
        // `new_state < (1 << accuracy_log) = table_size = decode.len()`.
        // `build_decoding_table` ensures the table is sized exactly
        // `1 << accuracy_log` entries. The bounds check that the
        // checked indexing would emit is provably redundant. Under
        // `feature = "fuzz_exports"` `read_entry` falls back to the
        // bounds-checked path — see comment on `read_entry`.
        self.state = self.read_entry(new_state as usize);

        Ok(())
    }

    /// Advance the internal state to decode the next symbol in the bitstream.
    ///
    /// # Panics
    ///
    /// Panics if called on an `FSEDecoder` whose backing `FSETable` has
    /// not been built yet (empty `decode` vec). `FSEDecoder::new`
    /// produces such a decoder with a zero-default `state`; the
    /// well-behaved pipeline is `new` → `init_state` → `update_state*`,
    /// and `init_state` returns `Err` on an uninitialized table. This
    /// assertion converts what would otherwise be UB (from the
    /// unchecked indexing in `read_entry`) into a clear fail-fast
    /// panic that surfaces the API misuse immediately instead of
    /// leaving the bitstream and decode state silently desynchronised.
    pub fn update_state<K: CpuKernel>(&mut self, bits: &mut BitReaderReversed<'_, K>) {
        // Public-API safety guard: `FSEDecoder::new` builds a decoder
        // with a zero-default `state` (Entry { new_state: 0, num_bits:
        // 0, symbol: 0 }) regardless of whether the table was actually
        // populated. A caller that constructs the decoder and then
        // calls `update_state` BEFORE a successful `init_state` would
        // hit `read_entry(0)` → `get_unchecked(0)` on an empty
        // `decode` vec — UB in release mode, since `debug_assert!` is
        // stripped. Fail-fast with `assert!` instead of silently
        // returning so that misuse surfaces immediately rather than
        // leaving the bitstream advanced by some bits but the decode
        // state stuck at the zero-default Entry — a corruption mode
        // that the caller has no way to diagnose. The well-behaved
        // decode pipeline always pairs `new` → `init_state` →
        // `update_state*`, so this branch is strongly biased "not
        // taken" and the predictor amortises it to zero cost on the
        // hot path. The corresponding `update_state_fast` is
        // `pub(crate)` with controlled callers, so it relies on the
        // documented precondition instead of paying for a per-call
        // check.
        assert!(
            !self.table.decode.is_empty(),
            concat!(
                "FSEDecoder::update_state called on an uninitialized table; ",
                "call init_state successfully before any update_state* call",
            ),
        );
        let num_bits = self.state.num_bits;
        let add = bits.get_bits(num_bits);
        let next_state = usize::from(self.state.new_state) + add as usize;
        // SAFETY: same invariant as `update_state_fast` below —
        // `new_state` and `num_bits` were paired by
        // `calc_baseline_and_numbits` during table construction such
        // that `new_state + (1 << num_bits) - 1 < table_size =
        // decode.len()`. `add < 1 << num_bits` by definition of the
        // `num_bits`-wide read, so `next_state < decode.len()`.
        self.state = self.read_entry(next_state);
    }

    /// Read `decode[idx]` — bounds-checked under `fuzz_exports`, unchecked
    /// otherwise. The call sites all hold the FSE invariant `idx <
    /// decode.len()` by construction (`init_state` reads
    /// `accuracy_log` bits, `update_state*` derive `next_state` from
    /// `Entry.new_state + add` where `calc_baseline_and_numbits`
    /// guarantees `new_state + (1 << num_bits) - 1 < table_size`).
    /// Under `fuzz_exports` external code can construct a mis-shaped
    /// table that violates the invariant — fall back to checked
    /// indexing so a fuzz harness sees a panic rather than UB, even
    /// when the fuzz binary is built in release mode (which makes
    /// `debug_assert!` a no-op and is the default for `cargo fuzz`).
    #[inline(always)]
    fn read_entry(&self, idx: usize) -> Entry {
        #[cfg(feature = "fuzz_exports")]
        {
            self.table.decode[idx]
        }
        #[cfg(not(feature = "fuzz_exports"))]
        // SAFETY: see comments at the individual call sites — `idx` is
        // invariant-bounded by the FSE table-build / state-transition
        // contract. LLVM cannot prove this on its own because the
        // invariant spans `build_decoding_table` and decode call sites.
        unsafe {
            *self.table.decode.get_unchecked(idx)
        }
    }

    /// Advance the internal state **without** an individual refill check.
    ///
    /// # Preconditions (caller-enforced)
    ///
    /// 1. **Bit budget:** enough bits MUST be available in the bit
    ///    reader (e.g. via [`BitReaderReversed::ensure_bits`] with a
    ///    budget that covers this and any other unchecked reads in the
    ///    same batch).
    /// 2. **State initialisation:** [`init_state`] MUST have returned
    ///    `Ok` on this decoder before any `update_state_fast` call.
    ///    Calling `update_state_fast` on a fresh `FSEDecoder::new`
    ///    output (which holds a zero-default `state` and may reference
    ///    an empty `decode` vec) would resolve to
    ///    `read_entry(0).get_unchecked(0)` on an empty slice — UB.
    ///    The empty-table guard in [`update_state`] is intentionally
    ///    omitted here to keep the per-sequence fast path branch-free;
    ///    the only call site (`decode_and_execute_sequences`) always
    ///    succeeds `init_state` before entering the per-sequence loop,
    ///    so the precondition holds by construction.
    ///
    /// This is the "fast path" used in the interleaved sequence decode loop
    /// where a single refill check covers all three FSE state updates.
    ///
    /// [`init_state`]: Self::init_state
    /// [`update_state`]: Self::update_state
    #[inline(always)]
    pub(crate) fn update_state_fast<K: CpuKernel>(&mut self, bits: &mut BitReaderReversed<'_, K>) {
        let num_bits = self.state.num_bits;
        let add = bits.get_bits_unchecked(num_bits);
        let next_state = usize::from(self.state.new_state) + add as usize;
        // SAFETY: `new_state` and `num_bits` were paired by
        // `calc_baseline_and_numbits` during table construction such that
        // `new_state + (2.pow(num_bits) - 1) < table_size = self.table.decode.len()`.
        // `add` is the value of `num_bits` bits read from the bitstream, so
        // `add < 2.pow(num_bits)` by construction of `BitReaderReversed::get_bits_unchecked`.
        // Therefore `next_state < self.table.decode.len()` and the indexed read
        // is in bounds; LLVM cannot prove this invariant on its own because it
        // spans the table-build and decode call sites. Under
        // `feature = "fuzz_exports"` `read_entry` falls back to bounds-checked
        // indexing — see comment on `read_entry`.
        self.state = self.read_entry(next_state);
    }
}

/// FSE decoding involves a decoding table that describes the probabilities of
/// all literals from 0 to the highest present one
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#fse-table-description>
#[derive(Debug, Clone)]
pub struct FSETable {
    /// The maximum symbol in the table (inclusive). Limits the probabilities length to max_symbol + 1.
    max_symbol: u8,
    /// The actual table containing the decoded symbol and the compression data
    /// connected to that symbol.
    pub decode: Vec<Entry>, //used to decode symbols, and calculate the next state
    /// Reused scratch buffer for symbol spreading to avoid per-build allocations.
    symbol_spread_buffer: Vec<u8>,
    /// The size of the table is stored in logarithm base 2 format,
    /// with the **size of the table** being equal to `(1 << accuracy_log)`.
    /// This value is used so that the decoder knows how many bits to read from the bitstream.
    pub accuracy_log: u8,
    /// In this context, probability refers to the likelihood that a symbol occurs in the given data.
    /// Given this info, the encoder can assign shorter codes to symbols that appear more often,
    /// and longer codes that appear less often, then the decoder can use the probability
    /// to determine what code was assigned to what symbol.
    ///
    /// The probability of a single symbol is a value representing the proportion of times the symbol
    /// would fall within the data.
    ///
    /// If a symbol probability is set to `-1`, it means that the probability of a symbol
    /// occurring in the data is less than one.
    pub symbol_probabilities: Vec<i32>, //used while building the decode Vector
}

impl FSETable {
    /// Initialize a new empty Finite State Entropy decoding table.
    pub fn new(max_symbol: u8) -> FSETable {
        FSETable {
            max_symbol,
            symbol_probabilities: Vec::with_capacity(256), //will never be more than 256 symbols because u8
            symbol_spread_buffer: Vec::new(),
            decode: Vec::new(), //depending on acc_log.
            accuracy_log: 0,
        }
    }

    /// Reset `self` and update `self`'s state to mirror the provided table.
    pub fn reinit_from(&mut self, other: &Self) {
        self.reset();
        self.symbol_probabilities
            .extend_from_slice(&other.symbol_probabilities);
        self.symbol_spread_buffer
            .reserve(other.symbol_spread_buffer.len());
        self.decode.extend_from_slice(&other.decode);
        self.accuracy_log = other.accuracy_log;
    }

    /// Empty the table and clear all internal state.
    pub fn reset(&mut self) {
        self.symbol_probabilities.clear();
        self.symbol_spread_buffer.clear();
        self.decode.clear();
        self.accuracy_log = 0;
    }

    /// Build the equivalent encoder-side table from a parsed decoder table.
    pub(crate) fn to_encoder_table(&self) -> Option<crate::fse::fse_encoder::FSETable> {
        if self.accuracy_log == 0 || self.symbol_probabilities.is_empty() {
            return None;
        }

        Some(crate::fse::fse_encoder::build_table_from_probabilities(
            &self.symbol_probabilities,
            self.accuracy_log,
        ))
    }

    /// returns how many BYTEs (not bits) were read while building the decoder
    pub fn build_decoder(&mut self, source: &[u8], max_log: u8) -> Result<usize, FSETableError> {
        let max_log = max_log.min(ENTRY_MAX_ACCURACY_LOG);
        self.accuracy_log = 0;

        let bytes_read = self.read_probabilities(source, max_log)?;
        self.build_decoding_table()?;

        Ok(bytes_read)
    }

    /// Populate each entry's `base_value` and `num_additional_bits`
    /// from a packed code-metadata table. The packed format matches
    /// the donor-style `(baseline << 0) | (extra_bits << 24)` layout
    /// used by the sequence section's `LL_META` / `ML_META`
    /// constants. Call site indexes into `packed` by entry symbol.
    ///
    /// MUST be called after `build_decoding_table` for tables whose
    /// per-sequence decode path expects pre-computed metadata (LL /
    /// ML sub-tables of the sequence section). Tables that don't
    /// (HUF-weight stream) skip the call and leave the new fields
    /// at their zero default.
    ///
    /// Symbols outside `packed.len()` are treated as a corrupt
    /// table — both fields stay at 0 for that entry. Upstream
    /// `build_decoding_table` already caps symbols at
    /// `max_symbol + 1`, so this branch is reachable only on
    /// `feature = "fuzz_exports"` builds where external code can
    /// stuff arbitrary entries; the safe default keeps the decode
    /// hot path from observing UB even in that contrived case.
    pub(crate) fn enrich_with_packed_seq_meta(&mut self, packed: &[u32]) {
        for entry in self.decode.iter_mut() {
            let idx = entry.symbol as usize;
            if idx < packed.len() {
                let meta = packed[idx];
                entry.base_value = meta & 0x00FF_FFFF;
                entry.num_additional_bits = (meta >> 24) as u8;
            } else {
                // Out-of-range symbol — reachable only on
                // `feature = "fuzz_exports"` builds where external
                // code can stuff arbitrary entries into the table.
                // Explicitly zero both fields so the next decode
                // pass observes a clean state instead of stale
                // metadata from a previous (well-formed) enrich
                // call. The downstream `decode_one_sequence_inline`
                // hot path still surfaces the corruption via the
                // existing bitstream checks; this clears prior
                // values rather than introducing a new fast-fail.
                entry.base_value = 0;
                entry.num_additional_bits = 0;
            }
        }
    }

    /// Populate each entry's `base_value` and `num_additional_bits`
    /// for the sequence section's offset table.
    ///
    /// Offset codes follow a different shape than LL / ML: each
    /// offset code `c` decodes to `base = 1 << c`, with `c` extra
    /// bits read from the stream. No external meta table — the
    /// computation is closed-form on the symbol value.
    ///
    /// Per the format spec, valid offset codes are 0..=31 (offsets
    /// up to 2³¹). Larger symbols would overflow the `1 << c` shift
    /// — the entry's fields stay at zero so the decode path
    /// surfaces the corruption downstream instead of producing a
    /// wraparound shift here.
    pub(crate) fn enrich_for_offsets(&mut self) {
        for entry in self.decode.iter_mut() {
            // Reset before the bound check so out-of-range
            // symbols clear stale metadata instead of carrying it
            // over from a previous enrich pass.
            entry.base_value = 0;
            entry.num_additional_bits = 0;
            let code = entry.symbol;
            if code < 32 {
                entry.base_value = 1u32 << code;
                entry.num_additional_bits = code;
            }
        }
    }

    /// Given the provided accuracy log, build a decoding table from that log.
    pub fn build_from_probabilities(
        &mut self,
        acc_log: u8,
        probs: &[i32],
    ) -> Result<(), FSETableError> {
        if acc_log == 0 {
            return Err(FSETableError::AccLogIsZero);
        }
        if acc_log > ENTRY_MAX_ACCURACY_LOG {
            return Err(FSETableError::AccLogTooBig {
                got: acc_log,
                max: ENTRY_MAX_ACCURACY_LOG,
            });
        }
        // Probability sum check: `build_decoding_table` assumes the
        // sum of positive probabilities plus the count of `-1`
        // entries (each contributing one slot at the top of the
        // table) equals exactly `1 << acc_log`. Without this guard
        // the wire-format `parse_wire` path validates the sum
        // upstream, but callers entering through
        // `build_from_probabilities` directly (the Predefined cache
        // and any fuzz / external user) would silently produce a
        // table where `calc_baseline_and_numbits` is given
        // `symbol_count` values exceeding the symbol's actual
        // `prob`, yielding `new_state` / `num_bits` pairs that can
        // overshoot `decode.len()` on the unchecked `read_entry`
        // hot path. Surface as a typed error so the caller can
        // distinguish a malformed input from an internal failure.
        // Strict probability range validation: RFC 8478 §4.1.1 admits
        // only `{-1, 0, 1..=table_size}` as probability values. The
        // wire-format parser never emits anything else, but
        // `build_from_probabilities` is a public entry point reachable
        // from fuzz harnesses and external users — leaving the gate
        // open invites two attack shapes:
        //
        //   1. Silent malformed table: clamping `p < -1` to 0 (the
        //      previous `p.max(0)`) lets `[-2, ...]` satisfy a sum
        //      check whose remaining terms happen to add up to
        //      `table_size`, producing a quietly broken table.
        //   2. DoS via probability-sum overflow: `[i32::MAX, i32::MAX,
        //      0x42] as u32` wraps to `0x40 = 64 = 1 << 6`, satisfies
        //      the sum check, then `build_decoding_table` runs
        //      `for _ in 0..prob` against `prob = i32::MAX`, looping
        //      2^31-1 times per such symbol (worst case: spread-array
        //      out-of-bounds panic, best case: minutes of CPU).
        //
        // Reject any `p > table_size` upfront so the subsequent u32
        // sum is bounded (see the sum's own comment for the
        // wrap-impossibility argument).
        let table_size = 1u32 << acc_log;
        for &p in probs {
            if p < -1 || p > table_size as i32 {
                return Err(FSETableError::InvalidProbability {
                    value: p,
                    table_size,
                    accuracy_log: acc_log,
                });
            }
        }
        // Sum the validated probs in u32. Per-element validation
        // above bounds each non-`-1` value by `table_size <= 1 << 16`,
        // and `probs.len() <= 256`, so the worst-case sum
        // `256 * 65536 = 16M` fits u32 with 8 bits of headroom — no
        // wrap possible. Keeps the public
        // `ProbabilityCounterMismatch.got` field at u32 (no public
        // API break).
        let probability_sum: u32 = probs
            .iter()
            .map(|&p| if p == -1 { 1u32 } else { p as u32 })
            .sum();
        if probability_sum != table_size {
            return Err(FSETableError::ProbabilityCounterMismatch {
                got: probability_sum,
                expected_sum: table_size,
                symbol_probabilities: probs.to_vec(),
            });
        }
        self.symbol_probabilities = probs.to_vec();
        self.accuracy_log = acc_log;
        self.build_decoding_table()
    }

    /// Build the actual decoding table after probabilities have been
    /// read. Donor-shape single-pass build: spread symbols into the
    /// scratch buffer, then write `decode` entries in one linear pass
    /// — no intermediate zero-init Vec::resize, no per-call heap
    /// allocation for the symbol counter (stack-allocated since the
    /// max symbol count is bounded by `u8::MAX + 1 = 256`).
    ///
    /// Wraps `build_decoding_table_inner` so the `symbol_spread_buffer`
    /// scratch is unconditionally restored to `self` on every exit
    /// path (success OR error) — otherwise an early `Err` from the
    /// inner pass would drop the taken buffer and force a fresh
    /// allocation on the next build.
    ///
    /// On `Err` the table is also fully `reset()` after the buffer
    /// restore: the inner pass mutates `self.decode` (partial push)
    /// while `self.accuracy_log` / `self.symbol_probabilities` were
    /// already set by the caller (`build_from_probabilities`); leaving
    /// that inconsistency in place would let a subsequent `init_state`
    /// pass the `accuracy_log != 0` gate and read from a partial
    /// `decode` vec — UB. After `reset()` the table is in the same
    /// well-defined empty state a freshly-constructed `FSETable` has;
    /// any subsequent `init_state` returns `TableIsUninitialized`.
    fn build_decoding_table(&mut self) -> Result<(), FSETableError> {
        let mut spread = core::mem::take(&mut self.symbol_spread_buffer);
        let result = self.build_decoding_table_inner(&mut spread);
        self.symbol_spread_buffer = spread;
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn build_decoding_table_inner(&mut self, spread: &mut Vec<u8>) -> Result<(), FSETableError> {
        let nb_symbols = self.symbol_probabilities.len();
        if nb_symbols > self.max_symbol as usize + 1 {
            return Err(FSETableError::TooManySymbols { got: nb_symbols });
        }

        let table_size = 1 << self.accuracy_log;

        // === Spread step ===
        // Reuse the persistent scratch buffer; clear then resize to
        // table_size. The resize is the ONLY zero-init in the entire
        // build path now (previous impl also zero-init'd `decode`
        // through `Vec::resize` with a default `Entry`, doubling the
        // write traffic on the build path).
        spread.clear();
        spread.resize(table_size, 0);

        // Pass 1: place -1 probability symbols at the top of the table.
        let mut negative_idx = table_size;
        for symbol in 0..nb_symbols {
            if self.symbol_probabilities[symbol] == -1 {
                negative_idx -= 1;
                spread[negative_idx] = symbol as u8;
            }
        }

        // Pass 2: distribute positive-probability symbols across the
        // [0, negative_idx) range using the donor's `next_position`
        // walker.
        let mut position = 0usize;
        for symbol in 0..nb_symbols {
            let prob = self.symbol_probabilities[symbol];
            if prob <= 0 {
                continue;
            }
            let symbol_u8 = symbol as u8;
            for _ in 0..prob {
                spread[position] = symbol_u8;
                position = next_position(position, table_size);
                while position >= negative_idx {
                    position = next_position(position, table_size);
                }
            }
        }

        // === Build step ===
        // Stack-allocated counter (max 256 symbols since the symbol
        // alphabet is bounded by `u8`). Replaces the previous
        // `Vec<u32>` field that was cleared + resized per call —
        // about 200 bytes of heap-alloc churn dropped from the hot
        // build path. Zero-init at declaration covers every slot.
        let mut counter = [0u32; 256];
        let accuracy_log = self.accuracy_log;

        // Single linear pass: write every entry exactly once. The
        // previous impl had three passes (zero-init resize, symbol
        // copy, build), all touching the same `Vec<Entry>` cache
        // line — now collapsed into one.
        self.decode.clear();
        self.decode.reserve(table_size);
        for (idx, &symbol) in spread.iter().enumerate().take(table_size) {
            if idx < negative_idx {
                // Positive-probability slot.
                let prob = self.symbol_probabilities[symbol as usize];
                let symbol_count = counter[symbol as usize];
                counter[symbol as usize] = symbol_count + 1;
                let (bl, nb) =
                    calc_baseline_and_numbits(table_size as u32, prob as u32, symbol_count);
                // FSE invariant gate (release-mode safety): the
                // decoder's unchecked `read_entry` path relies on
                // `new_state + (1 << num_bits) - 1 < table_size`,
                // which factors through `nb <= accuracy_log`. The
                // wire-format `parse_wire` path establishes this by
                // construction, but `build_from_probabilities` is a
                // public surface — a crafted but in-range probability
                // vector could theoretically drive
                // `calc_baseline_and_numbits` to violate the invariant.
                // Reject at build time so the unchecked indexing
                // contract holds in release as well as debug.
                if nb > accuracy_log {
                    return Err(FSETableError::TableInvariantViolation {
                        prob,
                        symbol,
                        num_bits: nb,
                        accuracy_log,
                    });
                }
                let new_state = u16::try_from(bl).map_err(|_| FSETableError::AccLogTooBig {
                    got: accuracy_log,
                    max: ENTRY_MAX_ACCURACY_LOG,
                })?;
                self.decode.push(Entry {
                    new_state,
                    symbol,
                    num_bits: nb,
                    base_value: 0,
                    num_additional_bits: 0,
                });
            } else {
                // -1 probability slot: no baseline, num_bits =
                // accuracy_log (decoder takes the full state width on
                // these slots, by zstd FSE spec).
                self.decode.push(Entry {
                    new_state: 0,
                    symbol,
                    num_bits: accuracy_log,
                    base_value: 0,
                    num_additional_bits: 0,
                });
            }
        }

        Ok(())
    }

    /// Read the accuracy log and the probability table from the source and return the number of bytes
    /// read. If the size of the table is larger than the provided `max_log`, return an error.
    fn read_probabilities(&mut self, source: &[u8], max_log: u8) -> Result<usize, FSETableError> {
        self.symbol_probabilities.clear(); //just clear, we will fill a probability for each entry anyways. No need to force new allocs here

        let mut br = BitReader::new(source);
        self.accuracy_log = ACC_LOG_OFFSET + (br.get_bits(4)? as u8);
        if self.accuracy_log > ENTRY_MAX_ACCURACY_LOG {
            return Err(FSETableError::AccLogTooBig {
                got: self.accuracy_log,
                max: ENTRY_MAX_ACCURACY_LOG,
            });
        }
        if self.accuracy_log > max_log {
            return Err(FSETableError::AccLogTooBig {
                got: self.accuracy_log,
                max: max_log,
            });
        }
        if self.accuracy_log == 0 {
            return Err(FSETableError::AccLogIsZero);
        }

        let probability_sum = 1 << self.accuracy_log;
        let mut probability_counter = 0;

        while probability_counter < probability_sum {
            let max_remaining_value = probability_sum - probability_counter + 1;
            let bits_to_read = highest_bit_set(max_remaining_value);

            let unchecked_value = br.get_bits(bits_to_read as usize)? as u32;

            let low_threshold = ((1 << bits_to_read) - 1) - (max_remaining_value);
            let mask = (1 << (bits_to_read - 1)) - 1;
            let small_value = unchecked_value & mask;

            let value = if small_value < low_threshold {
                br.return_bits(1);
                small_value
            } else if unchecked_value > mask {
                unchecked_value - low_threshold
            } else {
                unchecked_value
            };
            //println!("{}, {}, {}", self.symbol_probablilities.len(), unchecked_value, value);

            let prob = (value as i32) - 1;

            self.symbol_probabilities.push(prob);
            if prob != 0 {
                if prob > 0 {
                    probability_counter += prob as u32;
                } else {
                    // probability -1 counts as 1
                    assert!(prob == -1);
                    probability_counter += 1;
                }
            } else {
                //fast skip further zero probabilities
                loop {
                    let skip_amount = br.get_bits(2)? as usize;

                    self.symbol_probabilities
                        .resize(self.symbol_probabilities.len() + skip_amount, 0);
                    if skip_amount != 3 {
                        break;
                    }
                }
            }
        }

        if probability_counter != probability_sum {
            return Err(FSETableError::ProbabilityCounterMismatch {
                got: probability_counter,
                expected_sum: probability_sum,
                symbol_probabilities: self.symbol_probabilities.clone(),
            });
        }
        if self.symbol_probabilities.len() > self.max_symbol as usize + 1 {
            return Err(FSETableError::TooManySymbols {
                got: self.symbol_probabilities.len(),
            });
        }

        let bytes_read = if br.bits_read().is_multiple_of(8) {
            br.bits_read() / 8
        } else {
            (br.bits_read() / 8) + 1
        };

        Ok(bytes_read)
    }
}

/// A single entry in an FSE table.
///
/// The first four bytes (`new_state`, `symbol`, `num_bits`) mirror the
/// classical donor `FSE_decode_t` layout used by every FSE-backed
/// decoder in the crate (sequence-section LL/ML/OF, HUF-weight
/// stream). The trailing `base_value` + `num_additional_bits`
/// fields are populated only by the LL / ML / OF tables in the
/// sequence-section decoder (donor `ZSTD_seqSymbol` shape) so the
/// per-sequence hot path can read them directly off the active
/// entry instead of issuing a second lookup into a separate
/// metadata table. HUF tables leave these two fields at their
/// default zero — the extra eight bytes per slot are a fixed cost
/// on the small HUF FSE table (≤ 64 entries) and the dominant
/// savings live in the sequence section.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    /// Base index for the next state. The low bits read from the bitstream are
    /// added to this value to produce the final state index.
    pub new_state: u16,
    /// The byte that should be put in the decode output when encountering this state.
    pub symbol: u8,
    /// How many bits should be read from the stream when decoding this entry.
    pub num_bits: u8,
    /// For LL / ML / OF tables: pre-computed code baseline.
    /// `actual_value = base_value + extra_bits_read`. Donor
    /// `ZSTD_seqSymbol::baseValue`. Populated by the per-table
    /// `enrich_for_*` post-build pass; stays 0 for FSE tables that
    /// don't need it (HUF-weight stream).
    pub base_value: u32,
    /// For LL / ML / OF tables: number of bits to read from the
    /// bitstream after the symbol has been decoded, to obtain the
    /// additional value to add to `base_value`. Donor
    /// `ZSTD_seqSymbol::nbAdditionalBits`. Populated alongside
    /// `base_value`; stays 0 for FSE tables that don't need it.
    pub num_additional_bits: u8,
}

#[cfg(target_endian = "little")]
const _: [(); 0] = [(); core::mem::offset_of!(Entry, new_state)];
#[cfg(target_endian = "little")]
const _: [(); 2] = [(); core::mem::offset_of!(Entry, symbol)];
#[cfg(target_endian = "little")]
const _: [(); 3] = [(); core::mem::offset_of!(Entry, num_bits)];
#[cfg(target_endian = "little")]
const _: [(); 4] = [(); core::mem::offset_of!(Entry, base_value)];
#[cfg(target_endian = "little")]
const _: [(); 8] = [(); core::mem::offset_of!(Entry, num_additional_bits)];
// 12 bytes: 4 (header) + 4 (base_value) + 1 (num_additional_bits)
// + 3 bytes tail padding for natural u32 alignment.
#[cfg(target_endian = "little")]
const _: [(); 12] = [(); core::mem::size_of::<Entry>()];

/// This value is added to the first 4 bits of the stream to determine the
/// `Accuracy_Log`
const ACC_LOG_OFFSET: u8 = 5;
const ENTRY_MAX_ACCURACY_LOG: u8 = 16;

fn highest_bit_set(x: u32) -> u32 {
    assert!(x > 0);
    u32::BITS - x.leading_zeros()
}

//utility functions for building the decoding table from probabilities
/// Calculate the position of the next entry of the table given the current
/// position and size of the table.
fn next_position(mut p: usize, table_size: usize) -> usize {
    p += (table_size >> 1) + (table_size >> 3) + 3;
    p &= table_size - 1;
    p
}

fn calc_baseline_and_numbits(
    num_states_total: u32,
    num_states_symbol: u32,
    state_number: u32,
) -> (u32, u8) {
    if num_states_symbol == 0 {
        return (0, 0);
    }
    let num_state_slices = if 1 << (highest_bit_set(num_states_symbol) - 1) == num_states_symbol {
        num_states_symbol
    } else {
        1 << (highest_bit_set(num_states_symbol))
    }; //always power of two

    let num_double_width_state_slices = num_state_slices - num_states_symbol; //leftovers to the power of two need to be distributed
    let num_single_width_state_slices = num_states_symbol - num_double_width_state_slices; //these will not receive a double width slice of states
    let slice_width = num_states_total / num_state_slices; //size of a single width slice of states
    let num_bits = highest_bit_set(slice_width) - 1; //number of bits needed to read for one slice

    if state_number < num_double_width_state_slices {
        let baseline = num_single_width_state_slices * slice_width + state_number * slice_width * 2;
        (baseline, num_bits as u8 + 1)
    } else {
        let index_shifted = state_number - num_double_width_state_slices;
        ((index_shifted * slice_width), num_bits as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoding::errors::FSETableError;

    #[test]
    fn build_from_probabilities_rejects_sum_too_small() {
        // acc_log=4 → table_size=16. Valid sum (treating -1 as 1)
        // is 16. Use a distribution that sums to 8 (probs sum 8,
        // no -1s).
        let mut t = FSETable::new(8);
        let probs: [i32; 4] = [4, 2, 1, 1];
        let result = t.build_from_probabilities(4, &probs);
        assert!(
            matches!(
                result,
                Err(FSETableError::ProbabilityCounterMismatch { .. })
            ),
            "expected ProbabilityCounterMismatch for sum=8 vs expected=16, got {result:?}",
        );
    }

    #[test]
    fn build_from_probabilities_rejects_sum_too_large() {
        // acc_log=4 → table_size=16. Probs summing to 20 (>16)
        // would over-fill the spread.
        let mut t = FSETable::new(8);
        let probs: [i32; 4] = [8, 6, 4, 2];
        let result = t.build_from_probabilities(4, &probs);
        assert!(
            matches!(
                result,
                Err(FSETableError::ProbabilityCounterMismatch { .. })
            ),
            "expected ProbabilityCounterMismatch for sum=20 vs expected=16, got {result:?}",
        );
    }

    #[test]
    fn build_from_probabilities_accepts_negative_one_in_sum() {
        // acc_log=4 → table_size=16. Probs: 12 positive + 4 × -1
        // (counted as 1 each) sum to 16. Should succeed.
        let mut t = FSETable::new(8);
        let probs: [i32; 6] = [8, 4, -1, -1, -1, -1];
        let result = t.build_from_probabilities(4, &probs);
        assert!(
            result.is_ok(),
            "expected Ok for sum=16 with -1s, got {result:?}"
        );
    }

    #[test]
    fn build_from_probabilities_rejects_overflow_in_sum() {
        // DoS-shaped exploit: two i32::MAX values + 0x42 cast to u32
        // and summed wrapping wraps to 0x40 = 64 = 1 << acc_log. The
        // pre-fix u32 sum check would accept the input, then
        // build_decoding_table would run `for _ in 0..prob` against
        // prob = i32::MAX, looping 2^31-1 times per such symbol.
        let mut t = FSETable::new(8);
        let probs: [i32; 3] = [i32::MAX, i32::MAX, 0x42];
        let result = t.build_from_probabilities(6, &probs);
        assert!(
            matches!(result, Err(FSETableError::InvalidProbability { .. })),
            "expected InvalidProbability for overflow-shaped exploit, got {result:?}",
        );
    }

    #[test]
    fn build_from_probabilities_rejects_negative_below_minus_one() {
        // RFC 8478 §4.1.1: probability values are in {-1, 0, positive}.
        // p = -2 is outside the spec; clamping to 0 silently is wrong.
        let mut t = FSETable::new(8);
        let probs: [i32; 4] = [-2, 8, 4, 4];
        let result = t.build_from_probabilities(4, &probs);
        assert!(
            matches!(
                result,
                Err(FSETableError::InvalidProbability { value: -2, .. })
            ),
            "expected InvalidProbability{{value: -2, ..}}, got {result:?}",
        );
    }

    #[test]
    fn build_from_probabilities_accepts_exact_sum() {
        // Sum 4+4+4+4 = 16 = 1 << 4. No -1s.
        let mut t = FSETable::new(8);
        let probs: [i32; 4] = [4, 4, 4, 4];
        let result = t.build_from_probabilities(4, &probs);
        assert!(result.is_ok(), "expected Ok for exact sum, got {result:?}");
    }
}
