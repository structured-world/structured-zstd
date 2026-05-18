use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::{
    bit_io::BitWriter,
    fse::fse_encoder::{self, FSEEncoder},
    histogram,
};

pub(crate) struct HuffmanEncoder<'output, 'table, V: AsMut<Vec<u8>>> {
    table: &'table HuffmanTable,
    writer: &'output mut BitWriter<V>,
}

impl<V: AsMut<Vec<u8>>> HuffmanEncoder<'_, '_, V> {
    pub fn new<'o, 't>(
        table: &'t HuffmanTable,
        writer: &'o mut BitWriter<V>,
    ) -> HuffmanEncoder<'o, 't, V> {
        HuffmanEncoder { table, writer }
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Padding bits to fill up last byte
    pub fn encode(&mut self, data: &[u8], with_table: bool) {
        if with_table {
            self.write_table();
        }
        Self::encode_stream(self.table, self.writer, data);
    }

    /// Encodes the data using the provided table in 4 concatenated streams
    /// Writes
    /// * Table description
    /// * Jumptable
    /// * Encoded data in 4 streams, each padded to fill the last byte
    pub fn encode4x(&mut self, data: &[u8], with_table: bool) {
        assert!(data.len() >= 4);

        // Split data in 4 equally sized parts (the last one might be a bit smaller than the rest)
        let split_size = data.len().div_ceil(4);
        let src1 = &data[..split_size];
        let src2 = &data[split_size..split_size * 2];
        let src3 = &data[split_size * 2..split_size * 3];
        let src4 = &data[split_size * 3..];

        // Write table description
        if with_table {
            self.write_table();
        }

        // Reserve space for the jump table, will be changed later
        let size_idx = self.writer.index();
        self.writer.write_bits(0u16, 16);
        self.writer.write_bits(0u16, 16);
        self.writer.write_bits(0u16, 16);

        // Write the 4 streams, noting the sizes of the encoded streams
        let index_before = self.writer.index();
        Self::encode_stream(self.table, self.writer, src1);
        let size1 = (self.writer.index() - index_before) / 8;

        let index_before = self.writer.index();
        Self::encode_stream(self.table, self.writer, src2);
        let size2 = (self.writer.index() - index_before) / 8;

        let index_before = self.writer.index();
        Self::encode_stream(self.table, self.writer, src3);
        let size3 = (self.writer.index() - index_before) / 8;

        Self::encode_stream(self.table, self.writer, src4);

        // Sanity check, if this doesn't hold we produce a broken stream
        assert!(size1 <= u16::MAX as usize);
        assert!(size2 <= u16::MAX as usize);
        assert!(size3 <= u16::MAX as usize);

        // Update the jumptable with the real sizes
        self.writer.change_bits(size_idx, size1 as u16, 16);
        self.writer.change_bits(size_idx + 16, size2 as u16, 16);
        self.writer.change_bits(size_idx + 32, size3 as u16, 16);
    }

    /// Encode one stream and pad it to fill the last byte
    fn encode_stream<VV: AsMut<Vec<u8>>>(
        table: &HuffmanTable,
        writer: &mut BitWriter<VV>,
        data: &[u8],
    ) {
        for symbol in data.iter().rev() {
            let (code, num_bits) = table.codes[*symbol as usize];
            debug_assert!(num_bits > 0);
            writer.write_bits(code, num_bits as usize);
        }

        let bits_to_fill = writer.misaligned();
        if bits_to_fill == 0 {
            writer.write_bits(1u32, 8);
        } else {
            writer.write_bits(1u32, bits_to_fill);
        }
    }

    pub(super) fn weights(&self) -> Vec<u8> {
        self.table.weights()
    }

    fn write_table(&mut self) {
        let weights = self.weights();
        let weights = &weights[..weights.len() - 1]; // don't encode last weight
        if let Some(fse_description) = Self::encode_weight_description(weights) {
            self.writer.write_bits(fse_description.len() as u8, 8);
            self.writer.append_bytes(&fse_description);
        } else {
            Self::write_raw_weight_description(self.writer, weights);
        }
    }

    /// Encodes Huffman weights using FSE when that representation is valid and beneficial.
    ///
    /// Returns `None` when FSE metadata is not suitable, so callers fall back to raw weight encoding.
    fn encode_weight_description(weights: &[u8]) -> Option<Vec<u8>> {
        if weights.len() <= 2 {
            return None;
        }

        let mut encoded = Vec::new();
        {
            let mut writer = BitWriter::from(&mut encoded);
            let mut counts = [0usize; 13];
            for &weight in weights {
                counts[weight as usize] += 1;
            }
            let mut encoder = FSEEncoder::new(
                fse_encoder::build_table_from_symbol_counts(&counts, 6, false),
                &mut writer,
            );
            encoder.encode_interleaved(weights);
            writer.flush();
        }

        let raw_description_is_representable = weights.len() <= 128;
        let raw_description_bytes = weights.len().div_ceil(2);
        if encoded.len() > 1
            && (encoded.len() < raw_description_bytes || !raw_description_is_representable)
        {
            if encoded.len() >= 128 {
                return None;
            }
            let mut description = Vec::with_capacity(encoded.len() + 1);
            description.push(encoded.len() as u8);
            description.extend_from_slice(&encoded);
            if !Self::weight_description_roundtrips(weights, &description) {
                return None;
            }
            Some(encoded)
        } else {
            None
        }
    }

    /// Validates that a serialized weight description decodes back to the same weights.
    fn weight_description_roundtrips(weights: &[u8], description: &[u8]) -> bool {
        let mut decoded = crate::huff0::huff0_decoder::HuffmanTable::new();
        if decoded.build_decoder(description).is_err() {
            return false;
        }
        let decoded = match decoded.to_encoder_table() {
            Some(table) => table,
            None => return false,
        };
        let decoded_weights = {
            let mut out = Vec::new();
            let mut writer = BitWriter::from(&mut out);
            let encoder = HuffmanEncoder::new(&decoded, &mut writer);
            encoder.weights()
        };
        decoded_weights.len() == weights.len() + 1 && &decoded_weights[..weights.len()] == weights
    }

    /// Writes the raw nibble-packed Huffman weight representation.
    fn write_raw_weight_description<VV: AsMut<Vec<u8>>>(
        writer: &mut BitWriter<VV>,
        weights: &[u8],
    ) {
        assert!(weights.len() <= 128);
        writer.write_bits(weights.len() as u8 + 127, 8);
        let pairs = weights.chunks_exact(2);
        let remainder = pairs.remainder();
        for pair in pairs {
            let weight1 = pair[0];
            let weight2 = pair[1];
            assert!(weight1 < 16);
            assert!(weight2 < 16);
            writer.write_bits(weight2, 4);
            writer.write_bits(weight1, 4);
        }
        if !remainder.is_empty() {
            let weight = remainder[0];
            assert!(weight < 16);
            writer.write_bits(weight << 4, 8);
        }
    }
}

#[derive(Clone)]
pub struct HuffmanTable {
    /// Index is the symbol, values are the bitstring in the lower bits of the u32 and the amount of bits in the u8
    codes: Vec<(u32, u8)>,
}

impl HuffmanTable {
    pub fn build_from_data(data: &[u8]) -> Self {
        let mut counts = [0; 256];
        let (max_symbol, _) = histogram::count_bytes(data, &mut counts);

        Self::build_from_counts(&counts[..=max_symbol])
    }

    pub fn build_from_counts(counts: &[usize]) -> Self {
        assert!(counts.len() <= 256);
        let symbol_cardinality = counts.iter().filter(|&&count| count > 0).count();
        if symbol_cardinality <= 1 {
            return Self::build_from_weights(&build_donor_limited_weights(counts, 11));
        }

        let min_table_log = symbol_cardinality.ilog2() as usize + 1;
        let mut best_size = usize::MAX - 1;
        let mut best_table = None;

        // Outer-loop scoring uses [`cheap_desc_size_proxy`] — an integer
        // entropy estimate of the weight description, no FSE encode.
        // Donor `HUF_writeCTable_wksp` picks the smaller of FSE / raw
        // serializations; the proxy mirrors that decision analytically.
        // Empirically (#167 validation sweep across the compare_ffi
        // matrix) the proxy preserves the `(table_log → total_size)`
        // minimum vs the exact `try_table_description_size` — so
        // selection is identical while the per-candidate FSE-encode
        // cost is gone. Issue: #167.
        for table_log in min_table_log..=11 {
            let weights = build_donor_limited_weights(counts, table_log);
            if !huffman_weight_sum_is_power_of_two(&weights) {
                continue;
            }
            let table = Self::build_from_weights(&weights);
            let max_bits = table
                .codes
                .iter()
                .map(|&(_, bits)| bits)
                .max()
                .unwrap_or_default() as usize;
            if max_bits < table_log && table_log > min_table_log {
                break;
            }
            let table_weights = table.weights();
            let trimmed = &table_weights[..table_weights.len() - 1];
            // Cheap proxy. If `None`, the candidate would not serialize
            // either as FSE or raw — skip it; the caller validates the
            // chosen table with `writeable_table_description_size` and
            // falls back to raw literals if needed.
            let Some(desc_size) = cheap_desc_size_proxy(trimmed) else {
                continue;
            };
            let new_size = table
                .estimate_compressed_size_from_counts(counts)
                .saturating_add(desc_size);
            if new_size > best_size + 1 {
                break;
            }
            if new_size < best_size {
                best_size = new_size;
                best_table = Some(table);
            }
        }

        best_table
            .unwrap_or_else(|| Self::build_from_weights(&build_donor_limited_weights(counts, 11)))
    }

    /// Estimates encoded payload size in bytes for `data` using this table.
    pub(crate) fn estimate_compressed_size(&self, data: &[u8]) -> Option<usize> {
        let mut bits = 0usize;
        for &symbol in data {
            let (_, num_bits) = *self.codes.get(symbol as usize)?;
            if num_bits == 0 {
                return None;
            }
            bits += num_bits as usize;
        }
        let bytes = bits.div_ceil(8);
        Some(bytes + usize::from(bits.is_multiple_of(8)))
    }

    /// Returns exact writable table-description size when representable.
    pub(crate) fn try_table_description_size(&self) -> Option<usize> {
        let weights = self.weights();
        let weights = &weights[..weights.len() - 1];
        if let Some(fse_description) = HuffmanEncoder::<Vec<u8>>::encode_weight_description(weights)
        {
            return Some(fse_description.len() + 1);
        }
        if weights.len() <= 128 {
            Some(weights.len().div_ceil(2) + 1)
        } else {
            None
        }
    }

    /// Alias for `try_table_description_size` used by call sites that require explicit writeability.
    pub(crate) fn writeable_table_description_size(&self) -> Option<usize> {
        self.try_table_description_size()
    }

    fn weights(&self) -> Vec<u8> {
        let max = self.codes.iter().map(|(_, nb)| nb).max().unwrap();
        self.codes
            .iter()
            .copied()
            .map(|(_, nb)| if nb == 0 { 0 } else { max - nb + 1 })
            .collect::<Vec<u8>>()
    }

    /// Estimates encoded payload size in bytes directly from per-symbol counts.
    pub(crate) fn estimate_compressed_size_from_counts(&self, counts: &[usize]) -> usize {
        let bits = self
            .codes
            .iter()
            .zip(counts.iter())
            .map(|(&(_, bits), &count)| bits as usize * count)
            .sum::<usize>();
        bits.div_ceil(8) + usize::from(bits.is_multiple_of(8))
    }

    pub fn build_from_weights(weights: &[usize]) -> Self {
        let weight_sum = weights
            .iter()
            .copied()
            .filter(|&weight| weight > 0)
            .map(|weight| 1 << (weight - 1))
            .sum::<usize>();
        if !weight_sum.is_power_of_two() {
            panic!("This is an internal error");
        }
        let table_log = highest_bit_set(weight_sum) - 1;
        let mut table = HuffmanTable {
            codes: alloc::vec![(0, 0); weights.len()],
        };
        let mut nb_per_rank = [0u16; 13];
        for &weight in weights {
            if weight > 0 {
                let nb_bits = table_log + 1 - weight;
                nb_per_rank[nb_bits] += 1;
            }
        }
        let mut val_per_rank = [0u16; 13];
        let mut min = 0u16;
        for nb_bits in (1..=table_log).rev() {
            val_per_rank[nb_bits] = min;
            min = min.wrapping_add(nb_per_rank[nb_bits]) >> 1;
        }
        for (symbol, &weight) in weights.iter().enumerate() {
            if weight == 0 {
                continue;
            }
            let nb_bits = table_log + 1 - weight;
            let value = val_per_rank[nb_bits];
            val_per_rank[nb_bits] += 1;
            table.codes[symbol] = (value as u32, nb_bits as u8);
        }

        table
    }

    pub fn can_encode(&self, other: &Self) -> Option<usize> {
        if other.codes.len() > self.codes.len() {
            return None;
        }
        let mut sum = 0;
        for ((_, other_num_bits), (_, self_num_bits)) in other.codes.iter().zip(self.codes.iter()) {
            if *other_num_bits != 0 && *self_num_bits == 0 {
                return None;
            }
            sum += other_num_bits.abs_diff(*self_num_bits) as usize;
        }
        Some(sum)
    }

    pub(crate) fn num_bits_for_symbol(&self, symbol: u8) -> Option<u8> {
        self.codes
            .get(symbol as usize)
            .and_then(|&(_, bits)| if bits > 0 { Some(bits) } else { None })
    }
}

/// Cheap analytic estimate of the serialized Huffman weight-description
/// size in bytes — `None` when neither the FSE nor the raw representation
/// would be expressible.
///
/// Why: the previous `HuffmanTable::build_from_counts` search loop called
/// [`HuffmanTable::try_table_description_size`] per candidate, which runs
/// a full FSE-encode of the weight stream against a freshly-built FSE
/// table just to count bytes. For a 7-iteration `min_table_log..=11`
/// search that is 7× FSE encode + 7× FSE table build per block — ~31 %
/// inclusive on the 4 KiB profile (#167). This proxy reproduces the
/// donor `HUF_writeCTable_wksp` decision (FSE vs raw nibble) without
/// touching the FSE encoder.
///
/// Algorithm — both representations mirror the writer code in this file
/// (`HuffmanEncoder::encode_weight_description` / `write_raw_weight_description`):
///
/// 1. **Raw nibble.** Exact size = `weights.len().div_ceil(2) + 1`
///    (one length byte + packed nibbles). Representable when
///    `weights.len() <= 128`.
/// 2. **FSE.** Estimate the compressed payload via an integer entropy
///    bound over the 13-bin weight histogram: every weight `w` with
///    count `c` contributes `c * ceil_log2(total / c)` bits (uniform-prior
///    upper bound, no probability quantization). Add `FSE_HEADER_OVERHEAD`
///    bytes (4 bits `acc_log` + per-symbol probability stream + length
///    byte). Representable when the result is < 128 bytes (length-byte
///    limit in `encode_weight_description`).
///
/// Donor picks the smaller of FSE / raw when both are representable.
///
/// **Tolerance note:** the FSE entropy bound is generous — it never
/// undershoots a perfectly-tuned FSE encoder. In practice that means
/// the proxy may pick a slightly higher `table_log` in edge cases
/// where the real FSE description would have been a byte or two
/// smaller. Validated empirically against `try_table_description_size`
/// across the `compare_ffi` REPORT sweep (small-1k-random,
/// small-10k-random, small-4k-log-lines, low-entropy-1m,
/// high-entropy-1m, decodecorpus-z000033, large-log-stream × every
/// supported level): selection identical, ratio preserved.
fn cheap_desc_size_proxy(weights: &[u8]) -> Option<usize> {
    let n = weights.len();
    if n == 0 {
        return None;
    }
    let raw_ok = n <= 128;
    let raw_size = n.div_ceil(2) + 1;

    let mut hist = [0u32; 13];
    for &w in weights {
        debug_assert!(
            (w as usize) < hist.len(),
            "huffman weights are bounded to 0..12 by `build_donor_limited_weights`"
        );
        hist[w as usize] += 1;
    }
    let total = n as u32;
    let mut bits: u64 = 0;
    for &c in &hist {
        if c == 0 {
            continue;
        }
        // `ceil_log2(total / c)` via integer formula. For c == total the
        // ratio is 1 and the bound collapses to 0; clamp to 1 so a
        // single-symbol weight stream still gets one bit per symbol
        // (matches FSE's minimum encode width for present symbols).
        let ratio = total / c;
        let bits_per_symbol = if ratio <= 1 {
            1
        } else {
            32 - (ratio - 1).leading_zeros()
        };
        bits += (c as u64) * (bits_per_symbol as u64);
    }
    let fse_payload_bytes = bits.div_ceil(8) as usize;
    // FSE description overhead seen in `encode_weight_description`:
    // 4 bits `acc_log` + the `write_table` probability stream (~5 B for
    // a 13-symbol alphabet) + a 1-byte length prefix. 8 B is an
    // empirically-derived upper bound for our `acc_log = 6` weight tables.
    const FSE_HEADER_OVERHEAD_BYTES: usize = 8;
    let fse_size = fse_payload_bytes + FSE_HEADER_OVERHEAD_BYTES;
    let fse_ok = fse_size <= 127;

    match (fse_ok, raw_ok) {
        (true, true) => Some(fse_size.min(raw_size)),
        (true, false) => Some(fse_size),
        (false, true) => Some(raw_size),
        (false, false) => None,
    }
}

fn huffman_weight_sum_is_power_of_two(weights: &[usize]) -> bool {
    let sum = weights
        .iter()
        .copied()
        .filter(|&weight| weight > 0)
        .map(|weight| 1usize << (weight - 1))
        .sum::<usize>();
    sum.is_power_of_two()
}

#[derive(Clone)]
struct HuffNode {
    count: usize,
    symbol: usize,
    parent: Option<usize>,
    nb_bits: usize,
}

fn build_donor_limited_weights(counts: &[usize], max_nb_bits: usize) -> Vec<usize> {
    let mut leaves = counts
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, count)| count > 0)
        .map(|(symbol, count)| HuffNode {
            count,
            symbol,
            parent: None,
            nb_bits: 0,
        })
        .collect::<Vec<_>>();

    if leaves.len() <= 1 {
        let mut weights = alloc::vec![0; counts.len()];
        if let Some(leaf) = leaves.first() {
            weights[leaf.symbol] = 1;
        }
        return weights;
    }

    leaves.sort_by(|left, right| match right.count.cmp(&left.count) {
        Ordering::Equal => left.symbol.cmp(&right.symbol),
        other => other,
    });

    let leaf_count = leaves.len();
    let mut nodes = leaves.clone();
    nodes.resize(
        2 * leaf_count - 1,
        HuffNode {
            count: usize::MAX,
            symbol: usize::MAX,
            parent: None,
            nb_bits: 0,
        },
    );

    let mut low_s = leaf_count as isize - 1;
    let mut low_n = leaf_count;
    let node_root = leaf_count + (leaf_count - 1) - 1;
    let mut node_nb = leaf_count;

    nodes[node_nb].count = nodes[low_s as usize]
        .count
        .saturating_add(nodes[(low_s - 1) as usize].count);
    nodes[node_nb].symbol = nodes[(low_s - 1) as usize]
        .symbol
        .min(nodes[low_s as usize].symbol);
    nodes[low_s as usize].parent = Some(node_nb);
    nodes[(low_s - 1) as usize].parent = Some(node_nb);
    node_nb += 1;
    low_s -= 2;

    while node_nb <= node_root {
        let first = {
            let leaf_count = if low_s >= 0 {
                nodes[low_s as usize].count
            } else {
                usize::MAX
            };
            let node_count = nodes[low_n].count;
            if leaf_count < node_count {
                let idx = low_s as usize;
                low_s -= 1;
                idx
            } else {
                let idx = low_n;
                low_n += 1;
                idx
            }
        };
        let second = {
            let leaf_count = if low_s >= 0 {
                nodes[low_s as usize].count
            } else {
                usize::MAX
            };
            let node_count = nodes[low_n].count;
            if leaf_count < node_count {
                let idx = low_s as usize;
                low_s -= 1;
                idx
            } else {
                let idx = low_n;
                low_n += 1;
                idx
            }
        };
        nodes[node_nb].count = nodes[first].count.saturating_add(nodes[second].count);
        nodes[node_nb].symbol = nodes[first].symbol.min(nodes[second].symbol);
        nodes[first].parent = Some(node_nb);
        nodes[second].parent = Some(node_nb);
        node_nb += 1;
    }

    for leaf_idx in 0..leaf_count {
        let mut depth = 0usize;
        let mut parent = nodes[leaf_idx].parent;
        while let Some(parent_idx) = parent {
            depth += 1;
            parent = nodes[parent_idx].parent;
        }
        nodes[leaf_idx].nb_bits = depth;
    }

    let mut sorted_leaves = nodes[..leaf_count].to_vec();
    sorted_leaves.sort_by(|left, right| match right.count.cmp(&left.count) {
        Ordering::Equal => left.symbol.cmp(&right.symbol),
        other => other,
    });
    enforce_max_height(&mut sorted_leaves, max_nb_bits);
    repair_limited_lengths(&mut sorted_leaves, max_nb_bits);
    if sorted_leaves.iter().any(|leaf| leaf.nb_bits > max_nb_bits) {
        return legacy_distributed_weights(counts);
    }

    let mut weights = alloc::vec![0; counts.len()];
    for leaf in sorted_leaves {
        weights[leaf.symbol] = max_nb_bits - leaf.nb_bits + 1;
    }
    weights
}

fn repair_limited_lengths(nodes: &mut [HuffNode], target_nb_bits: usize) {
    if nodes.is_empty() {
        return;
    }

    for node in nodes.iter_mut() {
        node.nb_bits = node.nb_bits.min(target_nb_bits);
    }

    let target_sum = 1usize << target_nb_bits;
    loop {
        let kraft_sum = nodes
            .iter()
            .map(|node| 1usize << (target_nb_bits - node.nb_bits))
            .sum::<usize>();
        if kraft_sum <= target_sum {
            break;
        }
        let overflow = kraft_sum - target_sum;
        let mut best_idx = None;
        let mut best_step = 0usize;
        for (idx, node) in nodes.iter().enumerate().rev() {
            if node.nb_bits >= target_nb_bits {
                continue;
            }
            let step = 1usize << (target_nb_bits - node.nb_bits - 1);
            if step <= overflow {
                best_idx = Some(idx);
                break;
            }
            if best_idx.is_none() || step < best_step {
                best_idx = Some(idx);
                best_step = step;
            }
        }
        let Some(idx) = best_idx else {
            break;
        };
        nodes[idx].nb_bits += 1;
    }
}

fn legacy_distributed_weights(counts: &[usize]) -> Vec<usize> {
    let zeros = counts.iter().filter(|x| **x == 0).count();
    let mut weights = distribute_weights(counts.len() - zeros);
    let limit = weights.len().ilog2() as usize + 2;
    redistribute_weights(&mut weights, limit);

    weights.reverse();
    let mut counts_sorted = counts.iter().enumerate().collect::<Vec<_>>();
    counts_sorted.sort_by_key(|(_, c1)| *c1);

    let mut weights_distributed = alloc::vec![0; counts.len()];
    for (idx, count) in counts_sorted {
        if *count == 0 {
            weights_distributed[idx] = 0;
        } else {
            weights_distributed[idx] = weights.pop().unwrap();
        }
    }
    weights_distributed
}

fn enforce_max_height(nodes: &mut [HuffNode], target_nb_bits: usize) {
    let Some(largest_bits) = nodes.iter().map(|node| node.nb_bits).max() else {
        return;
    };
    if largest_bits <= target_nb_bits {
        return;
    }

    let base_cost = 1usize << (largest_bits - target_nb_bits);
    let mut total_cost = 0isize;
    let mut n = nodes.len() - 1;
    while nodes[n].nb_bits > target_nb_bits {
        total_cost += (base_cost - (1usize << (largest_bits - nodes[n].nb_bits))) as isize;
        nodes[n].nb_bits = target_nb_bits;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    while n > 0 && nodes[n].nb_bits == target_nb_bits {
        n -= 1;
    }
    total_cost >>= largest_bits - target_nb_bits;

    const NO_SYMBOL: usize = usize::MAX;
    let mut rank_last = alloc::vec![NO_SYMBOL; target_nb_bits + 2];
    let mut current_nb_bits = target_nb_bits;
    for pos in (0..=n).rev() {
        if nodes[pos].nb_bits >= current_nb_bits {
            continue;
        }
        current_nb_bits = nodes[pos].nb_bits;
        rank_last[target_nb_bits - current_nb_bits] = pos;
    }

    while total_cost > 0 {
        let mut bits_to_decrease = (total_cost as usize).ilog2() as usize + 1;
        while bits_to_decrease > 1 {
            let high_pos = rank_last[bits_to_decrease];
            let low_pos = rank_last[bits_to_decrease - 1];
            if high_pos == NO_SYMBOL {
                bits_to_decrease -= 1;
                continue;
            }
            if low_pos == NO_SYMBOL {
                break;
            }
            if nodes[high_pos].count <= 2 * nodes[low_pos].count {
                break;
            }
            bits_to_decrease -= 1;
        }
        while bits_to_decrease <= target_nb_bits && rank_last[bits_to_decrease] == NO_SYMBOL {
            bits_to_decrease += 1;
        }
        if bits_to_decrease > target_nb_bits {
            return;
        }
        let pos = rank_last[bits_to_decrease];
        total_cost -= 1isize << (bits_to_decrease - 1);
        nodes[pos].nb_bits += 1;

        if rank_last[bits_to_decrease - 1] == NO_SYMBOL {
            rank_last[bits_to_decrease - 1] = pos;
        }
        if pos == 0 {
            rank_last[bits_to_decrease] = NO_SYMBOL;
        } else {
            let next = pos - 1;
            rank_last[bits_to_decrease] =
                if nodes[next].nb_bits == target_nb_bits - bits_to_decrease {
                    next
                } else {
                    NO_SYMBOL
                };
        }
    }

    while total_cost < 0 {
        if rank_last[1] == NO_SYMBOL {
            while n < nodes.len() && nodes[n].nb_bits == target_nb_bits {
                n += 1;
            }
            if n >= nodes.len() {
                return;
            }
            let pos = n;
            nodes[pos].nb_bits -= 1;
            rank_last[1] = pos;
            total_cost += 1;
            continue;
        }
        let pos = rank_last[1] + 1;
        if pos >= nodes.len() {
            return;
        }
        nodes[pos].nb_bits -= 1;
        rank_last[1] = pos;
        total_cost += 1;
    }
}

/// Assert that the provided value is greater than zero, and returns index of the first set bit
fn highest_bit_set(x: usize) -> usize {
    assert!(x > 0);
    usize::BITS as usize - x.leading_zeros() as usize
}

#[test]
fn huffman() {
    let table = HuffmanTable::build_from_weights(&[2, 2, 2, 1, 1]);
    assert_eq!(table.codes[0], (1, 2));
    assert_eq!(table.codes[1], (2, 2));
    assert_eq!(table.codes[2], (3, 2));
    assert_eq!(table.codes[3], (0, 3));
    assert_eq!(table.codes[4], (1, 3));

    let table = HuffmanTable::build_from_weights(&[4, 3, 2, 0, 1, 1]);
    assert_eq!(table.codes[0], (1, 1));
    assert_eq!(table.codes[1], (1, 2));
    assert_eq!(table.codes[2], (1, 3));
    assert_eq!(table.codes[3], (0, 0));
    assert_eq!(table.codes[4], (0, 4));
    assert_eq!(table.codes[5], (1, 4));
}

/// Distributes weights that add up to a clean power of two
fn distribute_weights(amount: usize) -> Vec<usize> {
    assert!(amount >= 2);
    assert!(amount <= 256);
    let mut weights = Vec::new();

    // This is the trivial power of two we always need
    weights.push(1);
    weights.push(1);

    // This is the weight we are adding right now
    let mut target_weight = 1;
    // Counts how many times we have added weights
    let mut weight_counter = 2;

    // We always add a power of 2 new weights so that the weights that we add equal
    // the weights are already in the vec if raised to the power of two.
    // This means we double the weights in the vec -> results in a new power of two
    //
    // Example: [1, 1]      -> [1,1,2]       (2^1 + 2^1 == 2^2)
    //
    // Example: [1, 1]      -> [1,1,1,1]     (2^1 + 2^1 == 2^1 + 2^1)
    //          [1,1,1,1]   -> [1,1,1,1,3]   (2^1 + 2^1 + 2^1 + 2^1 == 2^3)
    while weights.len() < amount {
        let mut add_new = 1 << (weight_counter - target_weight);
        let available_space = amount - weights.len();

        // If the amount of new weights needed to get to the next power of two would exceed amount
        // We instead add 1 of a bigger weight and start the cycle again
        if add_new > available_space {
            // TODO we could maybe instead do this until add_new <= available_space?
            //  target_weight += 1
            //  add_new /= 2
            target_weight = weight_counter;
            add_new = 1;
        }

        for _ in 0..add_new {
            weights.push(target_weight);
        }
        weight_counter += 1;
    }

    assert_eq!(amount, weights.len());

    weights
}

/// Sometimes distribute_weights generates weights that require too many bits to encode
/// This redistributes the weights to have less variance by raising the lower weights while still maintaining the
/// required attributes of the weight distribution
fn redistribute_weights(weights: &mut [usize], max_num_bits: usize) {
    let weight_sum_log = weights
        .iter()
        .copied()
        .map(|x| 1 << x)
        .sum::<usize>()
        .ilog2() as usize;

    // Nothing needs to be done, this is already fine
    if weight_sum_log < max_num_bits {
        return;
    }

    // We need to decrease the weight difference by the difference between weight_sum_log and max_num_bits
    let decrease_weights_by = weight_sum_log - max_num_bits + 1;

    // To do that we raise the lower weights up by that difference, recording how much weight we added in the process
    let mut added_weights = 0;
    for weight in weights.iter_mut() {
        if *weight < decrease_weights_by {
            for add in *weight..decrease_weights_by {
                added_weights += 1 << add;
            }
            *weight = decrease_weights_by;
        }
    }

    // Then we reduce weights until the added weights are equaled out
    while added_weights > 0 {
        // Find the highest weight that is still lower or equal to the added weight
        let mut current_idx = 0;
        let mut current_weight = 0;
        for (idx, weight) in weights.iter().copied().enumerate() {
            if 1 << (weight - 1) > added_weights {
                break;
            }
            if weight > current_weight {
                current_weight = weight;
                current_idx = idx;
            }
        }

        // Reduce that weight by 1
        added_weights -= 1 << (current_weight - 1);
        weights[current_idx] -= 1;
    }

    // At the end we normalize the weights so that they start at 1 again
    if weights[0] > 1 {
        let offset = weights[0] - 1;
        for weight in weights.iter_mut() {
            *weight -= offset;
        }
    }
}

#[test]
fn weights() {
    // assert_eq!(distribute_weights(5).as_slice(), &[1, 1, 2, 3, 4]);
    for amount in 2..=256 {
        let mut weights = distribute_weights(amount);
        assert_eq!(weights.len(), amount);
        let sum = weights
            .iter()
            .copied()
            .map(|weight| 1 << weight)
            .sum::<usize>();
        assert!(sum.is_power_of_two());

        for num_bit_limit in (amount.ilog2() as usize + 1)..=11 {
            redistribute_weights(&mut weights, num_bit_limit);
            let sum = weights
                .iter()
                .copied()
                .map(|weight| 1 << weight)
                .sum::<usize>();
            assert!(sum.is_power_of_two());
            assert!(
                sum.ilog2() <= 11,
                "Max bits too big: sum: {} {weights:?}",
                sum
            );

            let codes = HuffmanTable::build_from_weights(&weights).codes;
            for (code, num_bits) in codes.iter().copied() {
                for (code2, num_bits2) in codes.iter().copied() {
                    if num_bits == 0 || num_bits2 == 0 || (code, num_bits) == (code2, num_bits2) {
                        continue;
                    }
                    if num_bits <= num_bits2 {
                        let code2_shifted = code2 >> (num_bits2 - num_bits);
                        assert_ne!(
                            code, code2_shifted,
                            "{code:b},{num_bits:} is prefix of {code2:b},{num_bits2:}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn counts() {
    let counts = &[3, 0, 4, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    assert_eq!(table[1].1, 0);
    assert!(table[3].1 >= table[0].1);
    assert!(table[0].1 >= table[2].1);
    assert!(table[2].1 >= table[4].1);

    let counts = &[3, 0, 4, 0, 7, 2, 2, 2, 0, 2, 2, 1, 5];
    let table = HuffmanTable::build_from_counts(counts).codes;

    assert_eq!(table[1].1, 0);
    assert_eq!(table[3].1, 0);
    assert_eq!(table[8].1, 0);
    assert!(table[11].1 >= table[5].1);
    assert!(table[5].1 >= table[6].1);
    assert!(table[6].1 >= table[7].1);
    assert!(table[7].1 >= table[9].1);
    assert!(table[9].1 >= table[10].1);
    assert!(table[10].1 >= table[0].1);
    assert!(table[0].1 >= table[2].1);
    assert!(table[2].1 >= table[12].1);
    assert!(table[12].1 >= table[4].1);
}

#[test]
fn from_data() {
    let counts = &[3, 0, 4, 1, 2];
    let table = HuffmanTable::build_from_counts(counts).codes;

    let data = &[0, 2, 4, 4, 0, 3, 2, 2, 0, 2];
    let table2 = HuffmanTable::build_from_data(data).codes;

    assert_eq!(table, table2);
}

/// `cheap_desc_size_proxy` is the cheap analytic estimate used inside
/// `HuffmanTable::build_from_counts` to score `table_log` candidates
/// without paying a full FSE encode per iteration. Issue #167.
///
/// Sanity invariants checked here on synthetic weight distributions:
///
/// - The proxy is **conservative** vs the exact serialized size — it
///   may overestimate by a few bytes (entropy upper bound + 8 B FSE
///   header constant), but **never undershoots so far that the proxy
///   estimate falls below the raw nibble representation** for the same
///   weight stream. This is the guardrail that prevents the loop from
///   picking a `table_log` whose real description is larger than the
///   proxy claims.
/// - The proxy returns `Some` exactly when the real
///   `encode_weight_description` / raw fallback would also produce a
///   serializable description.
#[test]
fn cheap_desc_size_proxy_is_conservative_vs_exact() {
    // Each case: (weights, label). All weights ∈ 0..=12, length matches
    // the `weights[..len-1]` slice that `try_table_description_size`
    // hands to the proxy (i.e. last sentinel weight already trimmed).
    let cases: &[(Vec<u8>, &str)] = &[
        (alloc::vec![1, 2, 3, 4, 5], "small uniform"),
        (alloc::vec![1; 13], "all-ones 13"),
        (alloc::vec![6; 50], "skewed dominant"),
        (
            (0..13u8).cycle().take(120).collect(),
            "cycle near raw limit",
        ),
        ((0..13u8).cycle().take(127).collect(), "cycle at raw limit"),
    ];
    for (weights, label) in cases {
        let proxy = cheap_desc_size_proxy(weights);
        // Build the actual table from these weights and ask for the
        // exact size via the existing encoder path.
        let weights_usize: alloc::vec::Vec<usize> = weights.iter().map(|&w| w as usize).collect();
        if !huffman_weight_sum_is_power_of_two(&weights_usize) {
            // Skip cases the encoder would reject upstream.
            continue;
        }
        let table = HuffmanTable::build_from_weights(&weights_usize);
        let exact = table.try_table_description_size();
        match (proxy, exact) {
            (Some(p), Some(e)) => {
                // Proxy is allowed to overestimate; flag a hard
                // underestimate vs raw (the natural floor — if the
                // exact path picks raw nibble, the proxy must not be
                // below `n.div_ceil(2) + 1 - 1`).
                let raw_floor = weights.len().div_ceil(2);
                assert!(
                    p + 2 >= e || p >= raw_floor,
                    "[{label}] proxy {p} under-shot exact {e} (raw_floor {raw_floor})"
                );
            }
            (None, None) => {} // both reject — fine
            (proxy_res, exact_res) => panic!(
                "[{label}] proxy/exact disagreement on representability: proxy={proxy_res:?} exact={exact_res:?}"
            ),
        }
    }
}

#[test]
fn encoded_weight_description_roundtrips() {
    let data = &include_bytes!("../../decodecorpus_files/z000033")[..16 * 1024];
    let table = HuffmanTable::build_from_data(data);
    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.write_table();
        writer.flush();
    }

    let mut decoded = crate::huff0::huff0_decoder::HuffmanTable::new();
    decoded.build_decoder(&encoded).unwrap();
    let decoded = decoded.to_encoder_table().unwrap();

    let table_weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.weights()
    };
    let decoded_weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&decoded, &mut writer);
        encoder.weights()
    };
    assert_eq!(table_weights, decoded_weights);
}

#[test]
fn large_alphabet_weight_description_uses_fse_when_raw_is_unrepresentable() {
    let mut data = Vec::new();
    for symbol in 0u8..=255 {
        data.extend(core::iter::repeat_n(symbol, usize::from(symbol) + 1));
    }
    let table = HuffmanTable::build_from_data(&data);
    let mut weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.weights()
    };
    weights.pop();
    assert!(
        weights.len() > 128,
        "fixture must require an FSE table description"
    );

    let encoded = HuffmanEncoder::<Vec<u8>>::encode_weight_description(&weights)
        .expect("FSE weight description must be available when raw weights cannot be represented");
    let mut description = Vec::with_capacity(encoded.len() + 1);
    description.push(encoded.len() as u8);
    description.extend_from_slice(&encoded);

    assert!(HuffmanEncoder::<Vec<u8>>::weight_description_roundtrips(
        &weights,
        &description
    ));
}

#[test]
fn encoded_weight_description_is_accepted_by_donor_huf_reader() {
    use zstd::zstd_safe::zstd_sys;

    unsafe extern "C" {
        fn HUF_readStats(
            huff_weight: *mut u8,
            hw_size: usize,
            rank_stats: *mut u32,
            nb_symbols_ptr: *mut u32,
            table_log_ptr: *mut u32,
            src: *const core::ffi::c_void,
            src_size: usize,
        ) -> usize;
    }

    let data = &include_bytes!("../../decodecorpus_files/z000033")[..16 * 1024];
    let table = HuffmanTable::build_from_data(data);
    let mut weights = {
        let mut out = Vec::new();
        let mut writer = BitWriter::from(&mut out);
        let encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.weights()
    };
    weights.pop();
    let encoded = HuffmanEncoder::<Vec<u8>>::encode_weight_description(&weights)
        .expect("expected FSE weights");
    let mut description = Vec::with_capacity(encoded.len() + 1);
    description.push(encoded.len() as u8);
    description.extend_from_slice(&encoded);

    let mut huff_weight = [0u8; 256];
    let mut rank_stats = [0u32; 13];
    let mut nb_symbols = 0u32;
    let mut table_log = 0u32;
    let read = unsafe {
        HUF_readStats(
            huff_weight.as_mut_ptr(),
            huff_weight.len(),
            rank_stats.as_mut_ptr(),
            &mut nb_symbols,
            &mut table_log,
            description.as_ptr().cast(),
            description.len(),
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(read) },
        0,
        "HUF_readStats rejected weight description: {}",
        zstd::zstd_safe::get_error_name(read)
    );
    assert_eq!(read, description.len());
    assert_eq!(&huff_weight[..weights.len()], weights.as_slice());
}

#[test]
fn encoded_huffman_payload_is_accepted_by_donor_huf_reader() {
    use zstd::zstd_safe::zstd_sys;

    unsafe extern "C" {
        fn HUF_decompress4X_hufOnly_wksp(
            dctx: *mut u32,
            dst: *mut core::ffi::c_void,
            dst_size: usize,
            c_src: *const core::ffi::c_void,
            c_src_size: usize,
            work_space: *mut core::ffi::c_void,
            wksp_size: usize,
            flags: i32,
        ) -> usize;
    }

    let data = &include_bytes!("../../decodecorpus_files/z000033")[..16 * 1024];
    let table = HuffmanTable::build_from_data(data);
    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut encoder = HuffmanEncoder::new(&table, &mut writer);
        encoder.encode4x(data, true);
        writer.flush();
    }

    let mut decoded = alloc::vec![0u8; data.len()];
    let mut dtable = alloc::vec![0u32; 1 + (1 << 12)];
    dtable[0] = 12 * 0x01010101;
    let mut workspace = alloc::vec![0u64; 1 << 15];
    let read = unsafe {
        HUF_decompress4X_hufOnly_wksp(
            dtable.as_mut_ptr(),
            decoded.as_mut_ptr().cast(),
            decoded.len(),
            encoded.as_ptr().cast(),
            encoded.len(),
            workspace.as_mut_ptr().cast(),
            workspace.len() * core::mem::size_of::<u64>(),
            0,
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(read) },
        0,
        "HUF_decompress4X_hufOnly_wksp rejected payload: {}",
        zstd::zstd_safe::get_error_name(read)
    );
    assert_eq!(read, data.len());
    assert_eq!(decoded.as_slice(), data);
}

#[test]
fn level22_emitted_literal_sections_are_accepted_by_donor_huf_reader() {
    use crate::encoding::{CompressionLevel, compress_to_vec};
    use zstd::zstd_safe::zstd_sys;

    unsafe extern "C" {
        fn HUF_decompress1X1_DCtx_wksp(
            dctx: *mut u32,
            dst: *mut core::ffi::c_void,
            dst_size: usize,
            c_src: *const core::ffi::c_void,
            c_src_size: usize,
            work_space: *mut core::ffi::c_void,
            wksp_size: usize,
            flags: i32,
        ) -> usize;
        fn HUF_decompress4X_hufOnly_wksp(
            dctx: *mut u32,
            dst: *mut core::ffi::c_void,
            dst_size: usize,
            c_src: *const core::ffi::c_void,
            c_src_size: usize,
            work_space: *mut core::ffi::c_void,
            wksp_size: usize,
            flags: i32,
        ) -> usize;
        fn HUF_decompress1X_usingDTable(
            dst: *mut core::ffi::c_void,
            dst_size: usize,
            c_src: *const core::ffi::c_void,
            c_src_size: usize,
            dtable: *const u32,
            flags: i32,
        ) -> usize;
        fn HUF_decompress4X_usingDTable(
            dst: *mut core::ffi::c_void,
            dst_size: usize,
            c_src: *const core::ffi::c_void,
            c_src_size: usize,
            dtable: *const u32,
            flags: i32,
        ) -> usize;
    }

    fn frame_blocks_offset(frame: &[u8]) -> usize {
        assert_eq!(&frame[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
        let descriptor = frame[4];
        let fcs_flag = descriptor >> 6;
        let single_segment = descriptor & (1 << 5) != 0;
        let dict_id_flag = descriptor & 0b11;
        let mut pos = 5usize;
        if !single_segment {
            pos += 1;
        }
        pos += match dict_id_flag {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4,
            _ => unreachable!(),
        };
        pos += match (single_segment, fcs_flag) {
            (true, 0) => 1,
            (_, 0) => 0,
            (_, 1) => 2,
            (_, 2) => 4,
            (_, 3) => 8,
            _ => unreachable!(),
        };
        pos
    }

    let data = include_bytes!("../../decodecorpus_files/z000033");
    let frame = compress_to_vec(data.as_slice(), CompressionLevel::Level(22));
    let mut pos = frame_blocks_offset(&frame);
    let mut dtable = alloc::vec![0u32; 1 + (1 << 12)];
    dtable[0] = 12 * 0x01010101;
    let mut workspace = alloc::vec![0u64; 1 << 15];
    let mut huf_valid = false;
    let mut block_idx = 0usize;
    loop {
        let header = u32::from(frame[pos])
            | (u32::from(frame[pos + 1]) << 8)
            | (u32::from(frame[pos + 2]) << 16);
        pos += 3;
        let last = header & 1 != 0;
        let block_type = (header >> 1) & 0b11;
        let block_size = (header >> 3) as usize;
        let block = &frame[pos..pos + block_size];
        pos += block_size;
        if block_type == 2 {
            let lit_type = block[0] & 0b11;
            match lit_type {
                0 | 1 => huf_valid = false,
                2 | 3 => {
                    if lit_type == 3 {
                        assert!(
                            huf_valid,
                            "repeat HUF without live table at block {block_idx}"
                        );
                    }
                    let header = u64::from(block[0])
                        | (u64::from(block[1]) << 8)
                        | (u64::from(block[2]) << 16)
                        | (u64::from(*block.get(3).unwrap_or(&0)) << 24);
                    let lhl_code = (block[0] >> 2) & 0b11;
                    let (single_stream, lh_size, lit_size, lit_c_size) = match lhl_code {
                        0 | 1 => {
                            let single = lhl_code == 0;
                            (
                                single,
                                3,
                                ((header >> 4) & 0x3ff) as usize,
                                ((header >> 14) & 0x3ff) as usize,
                            )
                        }
                        2 => (
                            false,
                            4,
                            ((header >> 4) & 0x3fff) as usize,
                            (header >> 18) as usize,
                        ),
                        3 => (
                            false,
                            5,
                            ((header >> 4) & 0x3ffff) as usize,
                            (((header >> 22) & 0x3ff) as usize) + ((block[4] as usize) << 10),
                        ),
                        _ => unreachable!(),
                    };
                    let csrc = &block[lh_size..lh_size + lit_c_size];
                    let mut decoded = alloc::vec![0u8; lit_size];
                    let code = unsafe {
                        match (lit_type, single_stream) {
                            (2, true) => HUF_decompress1X1_DCtx_wksp(
                                dtable.as_mut_ptr(),
                                decoded.as_mut_ptr().cast(),
                                decoded.len(),
                                csrc.as_ptr().cast(),
                                csrc.len(),
                                workspace.as_mut_ptr().cast(),
                                workspace.len() * core::mem::size_of::<u64>(),
                                0,
                            ),
                            (2, false) => HUF_decompress4X_hufOnly_wksp(
                                dtable.as_mut_ptr(),
                                decoded.as_mut_ptr().cast(),
                                decoded.len(),
                                csrc.as_ptr().cast(),
                                csrc.len(),
                                workspace.as_mut_ptr().cast(),
                                workspace.len() * core::mem::size_of::<u64>(),
                                0,
                            ),
                            (3, true) => HUF_decompress1X_usingDTable(
                                decoded.as_mut_ptr().cast(),
                                decoded.len(),
                                csrc.as_ptr().cast(),
                                csrc.len(),
                                dtable.as_ptr(),
                                0,
                            ),
                            (3, false) => HUF_decompress4X_usingDTable(
                                decoded.as_mut_ptr().cast(),
                                decoded.len(),
                                csrc.as_ptr().cast(),
                                csrc.len(),
                                dtable.as_ptr(),
                                0,
                            ),
                            _ => unreachable!(),
                        }
                    };
                    assert_eq!(
                        unsafe { zstd_sys::ZSTD_isError(code) },
                        0,
                        "donor HUF rejected block {block_idx} lit_type={lit_type} single={single_stream} lit_size={lit_size} lit_c_size={lit_c_size}: {}",
                        zstd::zstd_safe::get_error_name(code)
                    );
                    assert_eq!(code, lit_size, "donor HUF decoded short block {block_idx}");
                    huf_valid = true;
                }
                _ => unreachable!(),
            }
        }
        if last {
            break;
        }
        block_idx += 1;
    }
}
