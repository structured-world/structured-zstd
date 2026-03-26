use alloc::{boxed::Box, vec::Vec};

use crate::{
    bit_io::BitWriter,
    encoding::frame_compressor::{CompressState, FseTables, PreviousFseTable},
    encoding::{Matcher, Sequence},
    fse::fse_encoder::{FSETable, State, build_table_from_symbol_counts},
    huff0::huff0_encoder,
};

/// Compile-time guarantee that MAX_BLOCK_SIZE fits in the 18-bit size format.
const _: () = assert!(crate::common::MAX_BLOCK_SIZE <= 262_143);

/// A block of [`crate::common::BlockType::Compressed`]
pub fn compress_block<M: Matcher>(state: &mut CompressState<M>, output: &mut Vec<u8>) {
    let mut literals_vec = Vec::new();
    let mut sequences = Vec::new();
    let offset_hist = &mut state.offset_hist;
    state.matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => literals_vec.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            let ll = literals.len() as u32;
            literals_vec.extend_from_slice(literals);
            let actual_offset = offset as u32;
            let of = encode_offset_with_history(actual_offset, ll, offset_hist);
            sequences.push(crate::blocks::sequence_section::Sequence {
                ll,
                ml: match_len as u32,
                of,
            });
        }
    });

    // literals section

    let mut writer = BitWriter::from(output);
    if literals_vec.len() > 1024 {
        if let Some(table) =
            compress_literals(&literals_vec, state.last_huff_table.as_ref(), &mut writer)
        {
            state.last_huff_table.replace(table);
        }
    } else {
        raw_literals(&literals_vec, &mut writer);
    }

    // sequences section

    if sequences.is_empty() {
        writer.write_bits(0u8, 8);
    } else {
        encode_seqnum(sequences.len(), &mut writer);

        // Choose the tables
        let ll_mode = choose_table(
            previous_table(
                state.fse_tables.ll_previous.as_ref(),
                &state.fse_tables.ll_default,
            ),
            &state.fse_tables.ll_default,
            sequences.iter().map(|seq| encode_literal_length(seq.ll).0),
            9,
        );
        let ml_mode = choose_table(
            previous_table(
                state.fse_tables.ml_previous.as_ref(),
                &state.fse_tables.ml_default,
            ),
            &state.fse_tables.ml_default,
            sequences.iter().map(|seq| encode_match_len(seq.ml).0),
            9,
        );
        let of_mode = choose_table(
            previous_table(
                state.fse_tables.of_previous.as_ref(),
                &state.fse_tables.of_default,
            ),
            &state.fse_tables.of_default,
            sequences.iter().map(|seq| encode_offset(seq.of).0),
            8,
        );

        writer.write_bits(encode_fse_table_modes(&ll_mode, &ml_mode, &of_mode), 8);

        encode_table(&ll_mode, &mut writer);
        encode_table(&of_mode, &mut writer);
        encode_table(&ml_mode, &mut writer);

        encode_sequences(
            &sequences,
            &mut writer,
            ll_mode.as_ref(),
            ml_mode.as_ref(),
            of_mode.as_ref(),
        );

        let ll_last = into_last_used_table(ll_mode);
        let ml_last = into_last_used_table(ml_mode);
        let of_last = into_last_used_table(of_mode);
        remember_last_used_tables(&mut state.fse_tables, ll_last, ml_last, of_last);
    }
    writer.flush();
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum FseTableMode<'a> {
    Predefined(&'a FSETable),
    Encoded(FSETable),
    RepeatLast(&'a FSETable),
}

impl FseTableMode<'_> {
    pub fn as_ref(&self) -> &FSETable {
        match self {
            Self::Predefined(t) => t,
            Self::RepeatLast(t) => t,
            Self::Encoded(t) => t,
        }
    }
}

/// Estimate the encoding cost (in bits) of the given symbol distribution using a table.
/// Returns `None` if the table cannot encode all symbols present in the data.
fn estimate_encoding_cost(counts: &[usize; 256], total: usize, table: &FSETable) -> Option<usize> {
    if total == 0 {
        return Some(0);
    }
    let table_size = table.table_size as f64;
    let mut cost_bits = 0.0f64;
    for (symbol, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let prob = table.symbol_probability(symbol as u8);
        if prob == 0 {
            // Table cannot encode this symbol
            return None;
        }
        let effective_prob = if prob == -1 { 1 } else { prob as usize };
        // Keep the same entropy-style estimate for every candidate table. A
        // cheaper integer proxy perturbs close Encoded vs Repeat/Predefined
        // decisions, which is harder to recover from than this small setup cost.
        // Bits per symbol ≈ log2(table_size / probability)
        let bits_per_symbol = (table_size / effective_prob as f64).log2();
        cost_bits += count as f64 * bits_per_symbol;
    }
    Some(cost_bits as usize)
}

fn choose_table<'a>(
    previous: Option<&'a FSETable>,
    default_table: &'a FSETable,
    data: impl Iterator<Item = u8>,
    max_log: u8,
) -> FseTableMode<'a> {
    // Collect symbol distribution
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for symbol in data {
        counts[symbol as usize] += 1;
        total += 1;
    }

    if total == 0 {
        return FseTableMode::Predefined(default_table);
    }

    // Build a new table from the actual data distribution
    let max_symbol = counts
        .iter()
        .rposition(|&count| count > 0)
        .unwrap_or_default();
    let distinct_symbols = counts.iter().filter(|&&count| count > 0).take(2).count();
    // Sequence-section RLE mode is not implemented yet, so one-symbol streams
    // stay on the Predefined/Repeat paths unless those tables cannot encode the
    // stream at all. For non-degenerate inputs we still build the dynamic candidate
    // here instead of adding a heuristic short-circuit: exact cost comparison is
    // what lets Repeat, Predefined, and Encoded compete without hard-coded ratio
    // regressions.
    let new_table = (distinct_symbols > 1)
        .then(|| build_table_from_symbol_counts(&counts[..=max_symbol], max_log, true));

    // Estimate costs: encoding cost + table header cost. `None` means the
    // candidate table cannot encode the observed distribution.
    let new_total_cost = new_table.as_ref().and_then(|table| {
        estimate_encoding_cost(&counts, total, table)
            .map(|cost| cost.saturating_add(table.table_header_bits()))
    });

    // Predefined table: zero header cost
    let predefined_cost = estimate_encoding_cost(&counts, total, default_table);

    // Previous table: zero header cost (repeat mode)
    let previous_cost = previous.and_then(|table| estimate_encoding_cost(&counts, total, table));

    enum Choice {
        Previous,
        Predefined,
        New,
    }

    let mut best: Option<(usize, Choice)> = None;

    if let Some(cost) = previous_cost {
        best = Some((cost, Choice::Previous));
    }

    if let Some(cost) = predefined_cost {
        match best {
            Some((best_cost, _)) if best_cost <= cost => {}
            _ => best = Some((cost, Choice::Predefined)),
        }
    }

    if let Some(cost) = new_total_cost {
        match best {
            Some((best_cost, _)) if best_cost <= cost => {}
            _ => best = Some((cost, Choice::New)),
        }
    }

    match best.map(|(_, choice)| choice) {
        Some(Choice::Previous) => previous
            .map(FseTableMode::RepeatLast)
            .unwrap_or(FseTableMode::Predefined(default_table)),
        Some(Choice::Predefined) => FseTableMode::Predefined(default_table),
        Some(Choice::New) => new_table
            .map(FseTableMode::Encoded)
            .unwrap_or(FseTableMode::Predefined(default_table)),
        None => {
            let fallback_counts = [counts[0], 0];
            let fallback = if max_symbol == 0 {
                // `build_table_from_symbol_counts` needs at least two entries, so
                // single-symbol streams use a phantom zero-count second slot here.
                build_table_from_symbol_counts(&fallback_counts, max_log, true)
            } else {
                build_table_from_symbol_counts(&counts[..=max_symbol], max_log, true)
            };
            FseTableMode::Encoded(fallback)
        }
    }
}

fn encode_table(mode: &FseTableMode<'_>, writer: &mut BitWriter<&mut Vec<u8>>) {
    match mode {
        FseTableMode::Predefined(_) => {}
        FseTableMode::RepeatLast(_) => {}
        FseTableMode::Encoded(table) => table.write_table(writer),
    }
}

fn encode_fse_table_modes(
    ll_mode: &FseTableMode<'_>,
    ml_mode: &FseTableMode<'_>,
    of_mode: &FseTableMode<'_>,
) -> u8 {
    fn mode_to_bits(mode: &FseTableMode<'_>) -> u8 {
        match mode {
            FseTableMode::Predefined(_) => 0,
            FseTableMode::Encoded(_) => 2,
            FseTableMode::RepeatLast(_) => 3,
        }
    }
    mode_to_bits(ll_mode) << 6 | mode_to_bits(of_mode) << 4 | mode_to_bits(ml_mode) << 2
}

fn remember_last_used_tables(
    fse_tables: &mut FseTables,
    ll_last: Option<PreviousFseTable>,
    ml_last: Option<PreviousFseTable>,
    of_last: Option<PreviousFseTable>,
) {
    remember_last_used_table(&mut fse_tables.ll_previous, ll_last);
    remember_last_used_table(&mut fse_tables.ml_previous, ml_last);
    remember_last_used_table(&mut fse_tables.of_previous, of_last);
}

fn previous_table<'a>(
    previous: Option<&'a PreviousFseTable>,
    default: &'a FSETable,
) -> Option<&'a FSETable> {
    previous.map(|previous| previous.as_table(default))
}

fn remember_last_used_table(slot: &mut Option<PreviousFseTable>, next: Option<PreviousFseTable>) {
    if let Some(next) = next {
        *slot = Some(next);
    }
}

fn into_last_used_table(mode: FseTableMode<'_>) -> Option<PreviousFseTable> {
    match mode {
        FseTableMode::Encoded(table) => Some(PreviousFseTable::Custom(Box::new(table))),
        FseTableMode::Predefined(_) => Some(PreviousFseTable::Default),
        FseTableMode::RepeatLast(_) => None,
    }
}

fn encode_sequences(
    sequences: &[crate::blocks::sequence_section::Sequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    ll_table: &FSETable,
    ml_table: &FSETable,
    of_table: &FSETable,
) {
    let sequence = sequences[sequences.len() - 1];
    let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
    let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
    let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);
    let mut ll_state: &State = ll_table.start_state(ll_code);
    let mut ml_state: &State = ml_table.start_state(ml_code);
    let mut of_state: &State = of_table.start_state(of_code);

    writer.write_bits(ll_add_bits, ll_num_bits);
    writer.write_bits(ml_add_bits, ml_num_bits);
    writer.write_bits(of_add_bits, of_num_bits);

    // encode backwards so the decoder reads the first sequence first
    if sequences.len() > 1 {
        for sequence in (0..=sequences.len() - 2).rev() {
            let sequence = sequences[sequence];
            let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
            let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
            let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);

            {
                let next = of_table.next_state(of_code, of_state.index);
                let diff = of_state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                of_state = next;
            }
            {
                let next = ml_table.next_state(ml_code, ml_state.index);
                let diff = ml_state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                ml_state = next;
            }
            {
                let next = ll_table.next_state(ll_code, ll_state.index);
                let diff = ll_state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                ll_state = next;
            }

            writer.write_bits(ll_add_bits, ll_num_bits);
            writer.write_bits(ml_add_bits, ml_num_bits);
            writer.write_bits(of_add_bits, of_num_bits);
        }
    }
    writer.write_bits(ml_state.index as u64, ml_table.table_size.ilog2() as usize);
    writer.write_bits(of_state.index as u64, of_table.table_size.ilog2() as usize);
    writer.write_bits(ll_state.index as u64, ll_table.table_size.ilog2() as usize);

    let bits_to_fill = writer.misaligned();
    if bits_to_fill == 0 {
        writer.write_bits(1u32, 8);
    } else {
        writer.write_bits(1u32, bits_to_fill);
    }
}

fn encode_seqnum(seqnum: usize, writer: &mut BitWriter<impl AsMut<Vec<u8>>>) {
    const UPPER_LIMIT: usize = 0xFFFF + 0x7F00;
    match seqnum {
        1..=127 => writer.write_bits(seqnum as u32, 8),
        128..=0x7FFF => {
            let upper = ((seqnum >> 8) | 0x80) as u8;
            let lower = seqnum as u8;
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        0x8000..=UPPER_LIMIT => {
            let encode = seqnum - 0x7F00;
            let upper = (encode >> 8) as u8;
            let lower = encode as u8;
            writer.write_bits(255u8, 8);
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        _ => unreachable!(),
    }
}

fn encode_literal_length(len: u32) -> (u8, u32, usize) {
    match len {
        0..=15 => (len as u8, 0, 0),
        16..=17 => (16, len - 16, 1),
        18..=19 => (17, len - 18, 1),
        20..=21 => (18, len - 20, 1),
        22..=23 => (19, len - 22, 1),
        24..=27 => (20, len - 24, 2),
        28..=31 => (21, len - 28, 2),
        32..=39 => (22, len - 32, 3),
        40..=47 => (23, len - 40, 3),
        48..=63 => (24, len - 48, 4),
        64..=127 => (25, len - 64, 6),
        128..=255 => (26, len - 128, 7),
        256..=511 => (27, len - 256, 8),
        512..=1023 => (28, len - 512, 9),
        1024..=2047 => (29, len - 1024, 10),
        2048..=4095 => (30, len - 2048, 11),
        4096..=8191 => (31, len - 4096, 12),
        8192..=16383 => (32, len - 8192, 13),
        16384..=32767 => (33, len - 16384, 14),
        32768..=65535 => (34, len - 32768, 15),
        65536..=131071 => (35, len - 65536, 16),
        131072.. => unreachable!(),
    }
}

fn encode_match_len(len: u32) -> (u8, u32, usize) {
    match len {
        0..=2 => unreachable!(),
        3..=34 => (len as u8 - 3, 0, 0),
        35..=36 => (32, len - 35, 1),
        37..=38 => (33, len - 37, 1),
        39..=40 => (34, len - 39, 1),
        41..=42 => (35, len - 41, 1),
        43..=46 => (36, len - 43, 2),
        47..=50 => (37, len - 47, 2),
        51..=58 => (38, len - 51, 3),
        59..=66 => (39, len - 59, 3),
        67..=82 => (40, len - 67, 4),
        83..=98 => (41, len - 83, 4),
        99..=130 => (42, len - 99, 5),
        131..=258 => (43, len - 131, 7),
        259..=514 => (44, len - 259, 8),
        515..=1026 => (45, len - 515, 9),
        1027..=2050 => (46, len - 1027, 10),
        2051..=4098 => (47, len - 2051, 11),
        4099..=8194 => (48, len - 4099, 12),
        8195..=16386 => (49, len - 8195, 13),
        16387..=32770 => (50, len - 16387, 14),
        32771..=65538 => (51, len - 32771, 15),
        65539..=131074 => (52, len - 65539, 16),
        131075.. => unreachable!(),
    }
}

/// Convert an actual byte offset into the encoded offset code, using repeat offset
/// history per RFC 8878 §3.1.2.5. Updates `offset_hist` in place.
///
/// Encoded offset codes: 1/2/3 = repeat offsets, N+3 = new absolute offset N.
pub(in crate::encoding) fn encode_offset_with_history(
    actual_offset: u32,
    lit_len: u32,
    offset_hist: &mut [u32; 3],
) -> u32 {
    let encoded = if lit_len > 0 {
        if actual_offset == offset_hist[0] {
            1
        } else if actual_offset == offset_hist[1] {
            2
        } else if actual_offset == offset_hist[2] {
            3
        } else {
            actual_offset + 3
        }
    } else {
        // When lit_len == 0, repeat offset mapping shifts per RFC 8878:
        // code 1 → rep[1], code 2 → rep[2], code 3 → rep[0]-1
        if actual_offset == offset_hist[1] {
            1
        } else if actual_offset == offset_hist[2] {
            2
        } else if actual_offset == offset_hist[0].wrapping_sub(1) && offset_hist[0] > 1 {
            3
        } else {
            actual_offset + 3
        }
    };

    // Update history (same rules as decoder)
    if lit_len > 0 {
        match encoded {
            1 => { /* rep[0] stays the same */ }
            2 => {
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
            _ => {
                offset_hist[2] = offset_hist[1];
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
        }
    } else {
        match encoded {
            1 => {
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
            2 => {
                offset_hist[2] = offset_hist[1];
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
            _ => {
                offset_hist[2] = offset_hist[1];
                offset_hist[1] = offset_hist[0];
                offset_hist[0] = actual_offset;
            }
        }
    }

    encoded
}

fn encode_offset(len: u32) -> (u8, u32, usize) {
    let log = len.ilog2();
    let lower = len & ((1 << log) - 1);
    (log as u8, lower, log as usize)
}

fn raw_literals(literals: &[u8], writer: &mut BitWriter<&mut Vec<u8>>) {
    writer.write_bits(0u8, 2);
    writer.write_bits(0b11u8, 2);
    writer.write_bits(literals.len() as u32, 20);
    writer.append_bytes(literals);
}

fn compress_literals(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    writer: &mut BitWriter<&mut Vec<u8>>,
) -> Option<huff0_encoder::HuffmanTable> {
    let reset_idx = writer.index();

    let new_encoder_table = huff0_encoder::HuffmanTable::build_from_data(literals);

    let (encoder_table, new_table) = if let Some(_table) = last_table {
        if let Some(diff) = _table.can_encode(&new_encoder_table) {
            // TODO this is a very simple heuristic, maybe we should try to do better
            if diff > 5 {
                (&new_encoder_table, true)
            } else {
                (_table, false)
            }
        } else {
            (&new_encoder_table, true)
        }
    } else {
        (&new_encoder_table, true)
    };

    if new_table {
        writer.write_bits(2u8, 2); // compressed literals type
    } else {
        writer.write_bits(3u8, 2); // treeless compressed literals type
    }

    // RFC 8878 §3.1.1.3.1.1 Size_Format (spec limits):
    //   0b00: single stream, 10-bit (≤ 1023)  |  0b01: 4 streams, 10-bit (≤ 1023)
    //   0b10: 4 streams, 14-bit (≤ 16383)     |  0b11: 4 streams, 18-bit (≤ 262143)
    //
    // The encoder currently only calls this function for literals > 1024 bytes
    // (smaller literals use raw_literals), so only formats 0b10 and 0b11 are
    // reachable in practice. The 0b00/0b01 arms are kept for completeness.
    //
    // Runtime: hard guard — truncated 18-bit writes produce corrupt streams.
    // Note: format args omitted intentionally to avoid uncoverable dead code in coverage.
    assert!(
        literals.len() <= 262_143,
        "literals exceed RFC 8878 18-bit size limit (262143)"
    );
    let (size_format, size_bits) = match literals.len() {
        0..6 => (0b00u8, 10),
        6..1024 => (0b01, 10),
        1024..16384 => (0b10, 14),
        _ => (0b11, 18),
    };

    writer.write_bits(size_format, 2);
    writer.write_bits(literals.len() as u32, size_bits);
    let size_index = writer.index();
    writer.write_bits(0u32, size_bits);
    let index_before = writer.index();
    let mut encoder = huff0_encoder::HuffmanEncoder::new(encoder_table, writer);
    if size_format == 0 {
        encoder.encode(literals, new_table)
    } else {
        encoder.encode4x(literals, new_table)
    };
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    // If encoded len is bigger than the raw literals we are better off just writing the raw literals here
    if total_len >= literals.len() {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
        None
    } else if new_table {
        Some(new_encoder_table)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use super::{
        FseTableMode, choose_table, encode_match_len, encode_offset_with_history, previous_table,
        remember_last_used_tables,
    };
    use crate::encoding::frame_compressor::{FseTables, PreviousFseTable};
    use crate::fse::fse_encoder::build_table_from_symbol_counts;

    fn tables_match(
        lhs: &crate::fse::fse_encoder::FSETable,
        rhs: &crate::fse::fse_encoder::FSETable,
    ) -> bool {
        lhs.table_size == rhs.table_size
            && (0..=255u8)
                .all(|symbol| lhs.symbol_probability(symbol) == rhs.symbol_probability(symbol))
    }

    #[test]
    fn repeat_offset_codes_follow_rfc_mapping() {
        let mut hist = [10, 20, 30];
        assert_eq!(encode_offset_with_history(10, 5, &mut hist), 1);
        assert_eq!(hist, [10, 20, 30]);

        let mut hist = [10, 20, 30];
        assert_eq!(encode_offset_with_history(20, 5, &mut hist), 2);
        assert_eq!(hist, [20, 10, 30]);

        let mut hist = [10, 20, 30];
        assert_eq!(encode_offset_with_history(30, 5, &mut hist), 3);
        assert_eq!(hist, [30, 10, 20]);

        let mut hist = [10, 20, 30];
        assert_eq!(encode_offset_with_history(20, 0, &mut hist), 1);
        assert_eq!(hist, [20, 10, 30]);

        let mut hist = [10, 20, 30];
        assert_eq!(encode_offset_with_history(30, 0, &mut hist), 2);
        assert_eq!(hist, [30, 10, 20]);

        let mut hist = [10, 20, 30];
        assert_eq!(encode_offset_with_history(9, 0, &mut hist), 3);
        assert_eq!(hist, [9, 10, 20]);
    }

    #[test]
    fn encode_match_len_uses_correct_upper_range_base() {
        assert_eq!(encode_match_len(65539), (52, 0, 16));
        assert_eq!(encode_match_len(65540), (52, 1, 16));
        assert_eq!(encode_match_len(131074), (52, 65535, 16));
    }

    #[test]
    fn remember_last_used_tables_keeps_predefined_and_repeat_modes() {
        let mut fse_tables = FseTables::new();

        remember_last_used_tables(
            &mut fse_tables,
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
        );

        assert!(tables_match(
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default).unwrap(),
            &fse_tables.ll_default
        ));
        assert!(tables_match(
            previous_table(fse_tables.ml_previous.as_ref(), &fse_tables.ml_default).unwrap(),
            &fse_tables.ml_default
        ));
        assert!(tables_match(
            previous_table(fse_tables.of_previous.as_ref(), &fse_tables.of_default).unwrap(),
            &fse_tables.of_default
        ));

        let sample_codes = [0u8, 1u8];
        let ll_repeat = choose_table(
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default),
            &fse_tables.ll_default,
            sample_codes.iter().copied(),
            9,
        );
        let ml_repeat = choose_table(
            previous_table(fse_tables.ml_previous.as_ref(), &fse_tables.ml_default),
            &fse_tables.ml_default,
            sample_codes.iter().copied(),
            9,
        );
        let of_repeat = choose_table(
            previous_table(fse_tables.of_previous.as_ref(), &fse_tables.of_default),
            &fse_tables.of_default,
            sample_codes.iter().copied(),
            8,
        );

        assert!(matches!(ll_repeat, FseTableMode::RepeatLast(_)));
        assert!(matches!(ml_repeat, FseTableMode::RepeatLast(_)));
        assert!(matches!(of_repeat, FseTableMode::RepeatLast(_)));
    }

    #[test]
    fn remember_last_used_tables_reuses_existing_custom_slot_for_repeat() {
        let mut fse_tables = FseTables::new();
        let custom = build_table_from_symbol_counts(&[1, 1], 5, false);
        fse_tables.ll_previous = Some(PreviousFseTable::Custom(Box::new(custom)));

        let before = core::ptr::from_ref(
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default).unwrap(),
        );

        remember_last_used_tables(
            &mut fse_tables,
            None,
            Some(PreviousFseTable::Default),
            Some(PreviousFseTable::Default),
        );

        let after = core::ptr::from_ref(
            previous_table(fse_tables.ll_previous.as_ref(), &fse_tables.ll_default).unwrap(),
        );

        assert_eq!(before, after);
        assert!(matches!(
            fse_tables.ll_previous.as_ref(),
            Some(PreviousFseTable::Custom(_))
        ));
    }

    #[test]
    fn choose_table_handles_single_symbol_distribution() {
        let fse_tables = FseTables::new();
        let mode = choose_table(
            None,
            &fse_tables.ll_default,
            core::iter::repeat_n(0u8, 32),
            9,
        );
        assert!(matches!(mode, FseTableMode::Predefined(_)));
    }

    #[test]
    fn choose_table_without_previous_does_not_unwrap_none() {
        let only_zero_table = build_table_from_symbol_counts(&[1], 5, false);
        let mode = choose_table(None, &only_zero_table, core::iter::repeat_n(1u8, 32), 5);
        assert!(matches!(mode, FseTableMode::Encoded(_)));
    }
}
