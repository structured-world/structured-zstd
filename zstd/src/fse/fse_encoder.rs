use crate::bit_io::BitWriter;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

pub(crate) struct FSEEncoder<'output, V: AsMut<Vec<u8>>> {
    pub(super) table: FSETable,
    writer: &'output mut BitWriter<V>,
}

impl<V: AsMut<Vec<u8>>> FSEEncoder<'_, V> {
    pub fn new(table: FSETable, writer: &mut BitWriter<V>) -> FSEEncoder<'_, V> {
        FSEEncoder { table, writer }
    }

    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn into_table(self) -> FSETable {
        self.table
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Last state index
    /// * Padding bits to fill up last byte
    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn encode(&mut self, data: &[u8]) {
        self.write_table();

        let mut state = self.table.start_state(data[data.len() - 1]);
        for x in data[0..data.len() - 1].iter().rev().copied() {
            let next = self.table.next_state(x, state.index);
            let diff = state.index - next.baseline;
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

        if data.len() & 1 != 0 {
            ip -= 1;
            state_1 = self.table.start_state(data[ip]).index;
            ip -= 1;
            state_2 = self.table.start_state(data[ip]).index;
            ip -= 1;
            state_1 = self.encode_symbol_with_state(state_1, data[ip]);
        } else {
            ip -= 1;
            state_2 = self.table.start_state(data[ip]).index;
            ip -= 1;
            state_1 = self.table.start_state(data[ip]).index;
        }

        let remaining_after_init = data.len() - 2;
        if remaining_after_init & 2 != 0 {
            ip -= 1;
            state_2 = self.encode_symbol_with_state(state_2, data[ip]);
            ip -= 1;
            state_1 = self.encode_symbol_with_state(state_1, data[ip]);
        }

        while ip > 0 {
            ip -= 1;
            state_2 = self.encode_symbol_with_state(state_2, data[ip]);
            ip -= 1;
            state_1 = self.encode_symbol_with_state(state_1, data[ip]);
            if ip > 0 {
                ip -= 1;
                state_2 = self.encode_symbol_with_state(state_2, data[ip]);
                ip -= 1;
                state_1 = self.encode_symbol_with_state(state_1, data[ip]);
            }
        }

        self.writer
            .write_bits(state_2 as u64, self.acc_log() as usize);
        self.writer
            .write_bits(state_1 as u64, self.acc_log() as usize);

        let bits_to_fill = self.writer.misaligned();
        if bits_to_fill == 0 {
            self.writer.write_bits(1u32, 8);
        } else {
            self.writer.write_bits(1u32, bits_to_fill);
        }
    }

    fn encode_symbol_with_state(&mut self, state_index: usize, symbol: u8) -> usize {
        let next = self.table.next_state(symbol, state_index);
        let diff = state_index - next.baseline;
        self.writer.write_bits(diff as u64, next.num_bits as usize);
        next.index
    }

    fn write_table(&mut self) {
        self.table.write_table(self.writer);
    }

    pub(super) fn acc_log(&self) -> u8 {
        self.table.acc_log()
    }
}

#[derive(Debug, Clone)]
pub struct FSETable {
    /// Indexed by symbol
    pub(super) states: [SymbolStates; 256],
    /// Sum of all states.states.len()
    pub(crate) table_size: usize,
}

impl FSETable {
    pub(crate) fn next_state(&self, symbol: u8, idx: usize) -> &State {
        let states = &self.states[symbol as usize];
        states.get(idx, self.table_size)
    }

    pub(crate) fn start_state(&self, symbol: u8) -> &State {
        let states = &self.states[symbol as usize];
        let slot = states
            .start_state_slot
            .expect("symbol must be present in the FSE table");
        &states.states[slot]
    }

    pub fn acc_log(&self) -> u8 {
        self.table_size.ilog2() as u8
    }

    /// Get the probability assigned to a symbol (0 means absent, -1 means less-than-1).
    pub(crate) fn symbol_probability(&self, symbol: u8) -> i32 {
        self.states[symbol as usize].probability
    }

    pub(crate) fn max_num_bits_for_symbol(&self, symbol: u8) -> Option<u8> {
        let states = &self.states[symbol as usize];
        if states.probability == 0 {
            return None;
        }
        states.states.iter().map(|state| state.num_bits).max()
    }

    /// Compute the exact serialized size (in bits) of the FSE table header,
    /// including the byte-alignment padding at the end.
    /// Mirrors `write_table` but counts bits instead of writing them.
    ///
    /// The result assumes the header starts at a byte boundary, which matches
    /// all current encoder call sites.
    pub(crate) fn table_header_bits(&self) -> usize {
        let mut bits = 4; // acc_log - 5
        let mut probability_counter = 0usize;
        let probability_sum = 1 << self.acc_log();

        let mut prob_idx = 0;
        while probability_counter < probability_sum {
            let max_remaining_value = probability_sum - probability_counter + 1;
            let bits_to_write = max_remaining_value.ilog2() + 1;
            let low_threshold = ((1 << bits_to_write) - 1) - max_remaining_value;

            let prob = self.states[prob_idx].probability;
            prob_idx += 1;
            let value = (prob + 1) as u32;
            if value < low_threshold as u32 {
                bits += bits_to_write as usize - 1;
            } else {
                bits += bits_to_write as usize;
            }

            if prob == -1 {
                probability_counter += 1;
            } else if prob > 0 {
                probability_counter += prob as usize;
            } else {
                let mut zeros = 0u8;
                while prob_idx < self.states.len() && self.states[prob_idx].probability == 0 {
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

    pub(crate) fn write_table<V: AsMut<Vec<u8>>>(&self, writer: &mut BitWriter<V>) {
        assert!(
            writer.index().is_multiple_of(8),
            "FSE table headers must start on a byte boundary"
        );
        #[cfg(debug_assertions)]
        let start_idx = writer.index();
        writer.write_bits(self.acc_log() - 5, 4);
        let mut probability_counter = 0usize;
        let probability_sum = 1 << self.acc_log();

        let mut prob_idx = 0;
        while probability_counter < probability_sum {
            let max_remaining_value = probability_sum - probability_counter + 1;
            let bits_to_write = max_remaining_value.ilog2() + 1;
            let low_threshold = ((1 << bits_to_write) - 1) - (max_remaining_value);
            let mask = (1 << (bits_to_write - 1)) - 1;

            let prob = self.states[prob_idx].probability;
            prob_idx += 1;
            let value = (prob + 1) as u32;
            if value < low_threshold as u32 {
                writer.write_bits(value, bits_to_write as usize - 1);
            } else if value > mask {
                writer.write_bits(value + low_threshold as u32, bits_to_write as usize);
            } else {
                writer.write_bits(value, bits_to_write as usize);
            }

            if prob == -1 {
                probability_counter += 1;
            } else if prob > 0 {
                probability_counter += prob as usize;
            } else {
                let mut zeros = 0u8;
                while prob_idx < self.states.len() && self.states[prob_idx].probability == 0 {
                    zeros += 1;
                    prob_idx += 1;
                    if zeros == 3 {
                        writer.write_bits(3u8, 2);
                        zeros = 0;
                    }
                }
                writer.write_bits(zeros, 2);
            }
        }
        writer.write_bits(0u8, writer.misaligned());
        #[cfg(debug_assertions)]
        {
            let written_bits = writer.index() - start_idx;
            let computed = self.table_header_bits();
            debug_assert_eq!(
                written_bits, computed,
                "table_header_bits() mismatch: written={written_bits}, computed={computed}"
            );
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SymbolStates {
    /// Sorted by baseline to allow easy lookup using an index
    pub(super) states: Vec<State>,
    pub(super) probability: i32,
    start_state_slot: Option<usize>,
}

impl SymbolStates {
    fn get(&self, idx: usize, max_idx: usize) -> &State {
        let start_search_at = (idx * self.states.len()) / max_idx;
        self.states[start_search_at..]
            .iter()
            .find(|state| state.contains(idx))
            .unwrap()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct State {
    /// How many bits the range of this state needs to be encoded as
    pub(crate) num_bits: u8,
    /// The first index targeted by this state
    pub(crate) baseline: usize,
    /// The last index targeted by this state (baseline + the maximum number with numbits bits allows)
    pub(crate) last_index: usize,
    /// Index of this state in the decoding table
    pub(crate) index: usize,
}

impl State {
    fn contains(&self, idx: usize) -> bool {
        self.baseline <= idx && self.last_index >= idx
    }
}

#[cfg(any(test, feature = "fuzz_exports"))]
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

pub(crate) fn build_table_from_symbol_counts(
    counts: &[usize],
    max_log: u8,
    avoid_0_numbit: bool,
) -> FSETable {
    build_table_from_counts(counts, max_log, avoid_0_numbit)
}

fn build_table_from_counts(counts: &[usize], max_log: u8, avoid_0_numbit: bool) -> FSETable {
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
    let table_log = donor_optimal_table_log(max_log, total, max_symbol);
    let mut probs = [0i32; 256];
    donor_normalize_counts(
        &mut probs[..counts.len()],
        table_log,
        counts,
        total,
        max_symbol,
        avoid_0_numbit,
    );
    build_table_from_probabilities(&probs[..counts.len()], table_log)
}

fn donor_min_table_log(total: usize, max_symbol: usize) -> u8 {
    let min_bits_src = total.ilog2() + 1;
    let min_bits_symbols = if max_symbol == 0 {
        2
    } else {
        max_symbol.ilog2() + 2
    };
    min_bits_src.min(min_bits_symbols) as u8
}

fn donor_optimal_table_log(max_table_log: u8, total: usize, max_symbol: usize) -> u8 {
    let max_bits_src = (total - 1).ilog2().saturating_sub(2) as u8;
    let min_bits = donor_min_table_log(total, max_symbol);
    let mut table_log = max_table_log;
    if max_bits_src < table_log {
        table_log = max_bits_src;
    }
    if min_bits > table_log {
        table_log = min_bits;
    }
    table_log.clamp(5, 12)
}

fn donor_normalize_counts(
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

    for symbol in 0..=max_symbol {
        let count = counts[symbol];
        if count == 0 {
            normalized[symbol] = 0;
        } else if count <= low_threshold {
            normalized[symbol] = low_prob_count;
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
            normalized[symbol] = probability;
            still_to_distribute -= probability;
        }
    }

    if -still_to_distribute >= normalized[largest] >> 1 {
        donor_normalize_m2(
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

fn donor_normalize_m2(
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
            assert!(weight >= 1, "donor FSE normalization produced zero weight");
            normalized[symbol] = weight as i32;
            tmp_total = end;
        }
    }
}

pub(super) fn build_table_from_probabilities(probs: &[i32], acc_log: u8) -> FSETable {
    let mut states = core::array::from_fn::<SymbolStates, 256, _>(|_| SymbolStates {
        states: Vec::new(),
        probability: 0,
        start_state_slot: None,
    });
    let mut symbol_positions = core::array::from_fn::<Vec<usize>, 256, _>(|_| Vec::new());

    // distribute -1 symbols
    let mut negative_idx = (1 << acc_log) - 1;
    for (symbol, _prob) in probs
        .iter()
        .copied()
        .enumerate()
        .filter(|prob| prob.1 == -1)
    {
        states[symbol].states.push(State {
            num_bits: acc_log,
            baseline: 0,
            last_index: (1 << acc_log) - 1,
            index: negative_idx,
        });
        symbol_positions[symbol].push(negative_idx);
        states[symbol].probability = -1;
        negative_idx -= 1;
    }

    // distribute other symbols

    // Setup all needed states per symbol with their respective index
    let mut idx = 0;
    for (symbol, prob) in probs.iter().copied().enumerate() {
        if prob <= 0 {
            continue;
        }
        states[symbol].probability = prob;
        let states = &mut states[symbol].states;
        let positions = &mut symbol_positions[symbol];
        for _ in 0..prob {
            states.push(State {
                num_bits: 0,
                baseline: 0,
                last_index: 0,
                index: idx,
            });
            positions.push(idx);

            idx = next_position(idx, 1 << acc_log);
            while idx > negative_idx {
                idx = next_position(idx, 1 << acc_log);
            }
        }
        assert_eq!(states.len(), prob as usize);
    }

    // Materialize the C `stateTable`: symbols are grouped by symbol and, within
    // each symbol, ordered by table position.
    let mut state_table = Vec::with_capacity(1 << acc_log);
    for positions in &mut symbol_positions {
        positions.sort_unstable();
        state_table.extend(positions.iter().copied());
    }

    // Build encoder transitions directly from C `FSE_encodeSymbol()` formulas.
    let mut symbol_transform_total = 0usize;
    for (symbol, probability) in probs.iter().copied().enumerate() {
        if probability == 0 {
            continue;
        }
        let probability_abs = probability.unsigned_abs() as usize;
        let (delta_nb_bits, delta_find_state) = match probability {
            -1 | 1 => (
                ((acc_log as usize) << 16).saturating_sub(1usize << acc_log),
                symbol_transform_total as isize - 1,
            ),
            probability if probability > 1 => {
                let probability = probability as usize;
                let max_bits_out = acc_log as usize - (probability - 1).ilog2() as usize;
                let min_state_plus = probability << max_bits_out;
                (
                    (max_bits_out << 16).saturating_sub(min_state_plus),
                    symbol_transform_total as isize - probability as isize,
                )
            }
            _ => unreachable!(),
        };
        let state = &mut states[symbol];
        let init_nb_bits_out = (delta_nb_bits + (1 << 15)) >> 16;
        let init_value = (init_nb_bits_out << 16).saturating_sub(delta_nb_bits);
        let state_table_index = (init_value >> init_nb_bits_out) as isize + delta_find_state;
        let start_index = state_table[state_table_index as usize];
        symbol_transform_total += probability_abs;
        state.states = Vec::with_capacity(probability_abs.max(1));
        // Dedup via `BTreeSet<(baseline, last_index, index, num_bits)>` instead
        // of the previous `state.states.iter().any(...)` linear scan. With
        // `acc_log` up to 12 (table_size = 4096) the inner scan was O(N²) per
        // symbol and dominated encoder FSE table construction (~6% exclusive
        // on level22 profile). BTreeSet keeps insertion order on the
        // accepted-entries Vec untouched — important because the downstream
        // `start_state_slot = states.iter().position(|e| e.index == start_index)`
        // lookup depends on which duplicate-keyed entry shows up first.
        let mut seen: BTreeSet<(usize, usize, usize, u8)> = BTreeSet::new();
        for current_index in 0..(1usize << acc_log) {
            let current_value = (1usize << acc_log) + current_index;
            let num_bits = (current_value + delta_nb_bits) >> 16;
            let next_state_idx = (current_value >> num_bits) as isize + delta_find_state;
            let next_index = state_table[next_state_idx as usize];
            let mask = (1usize << num_bits) - 1;
            let baseline = current_index & !mask;
            let last_index = baseline + mask;
            if !seen.insert((baseline, last_index, next_index, num_bits as u8)) {
                continue;
            }
            state.states.push(State {
                num_bits: num_bits as u8,
                baseline,
                last_index,
                index: next_index,
            });
        }

        // For encoding we use the states ordered by the indexes they target
        state.states.sort_by_key(|l| l.baseline);
        state.start_state_slot = state
            .states
            .iter()
            .position(|entry| entry.index == start_index);
    }

    FSETable {
        table_size: 1 << acc_log,
        states,
    }
}

/// Calculate the position of the next entry of the table given the current
/// position and size of the table.
fn next_position(mut p: usize, table_size: usize) -> usize {
    p += (table_size >> 1) + (table_size >> 3) + 3;
    p &= table_size - 1;
    p
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
//     - Without `critical-section`: rebuild + `Box::leak` per
//       call. Lack of pointer-width atomics is **not** a guarantee
//       of non-concurrency (interrupt / preemption reentrancy on
//       bare-metal targets), and a shared `static mut` slot
//       without any synchronization would be a data race (UB).
//       Skipping the cache entirely sidesteps the synchronization
//       question; the trade-off is that repeat callers leak
//       ~few-KiB per `FrameCompressor::new` × 3 (LL/ML/OF).
//       Embedded encoders typically live for the entire process
//       so the accumulated leak is bounded in practice. Users who
//       want the cache should enable the `critical-section`
//       feature.
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
/// fresh `FSETable` and `Box::leak` it for the `'static` lifetime.
/// No caching — see the module-level header comment for the
/// rationale (interrupt / preemption reentrancy on bare-metal
/// targets makes a shared `static mut` cache without atomic
/// synchronization a data race surface, so we trade off ~few-KiB
/// per call against the UB risk; users who want the cache should
/// enable the crate's `critical-section` feature).
#[cfg(all(not(target_has_atomic = "ptr"), not(feature = "critical-section")))]
fn build_and_leak_table(probs: &[i32], acc_log: u8) -> &'static FSETable {
    let built = alloc::boxed::Box::new(build_table_from_probabilities(probs, acc_log));
    alloc::boxed::Box::leak(built)
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

pub(crate) fn default_ml_table() -> &'static FSETable {
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
        build_and_leak_table(ML_DIST, 6)
    }
}

pub(crate) fn default_ll_table() -> &'static FSETable {
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
        build_and_leak_table(LL_DIST, 6)
    }
}

pub(crate) fn default_of_table() -> &'static FSETable {
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
        build_and_leak_table(OF_DIST, 5)
    }
}
