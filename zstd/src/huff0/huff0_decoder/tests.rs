use super::*;
use alloc::vec;

fn test_table() -> HuffmanTable {
    // Packed `symbol | (num_bits << 8)` per state index (upstream zstd `HUF_DEltX1`).
    let packed_decode = vec![
        u16::from(b'A') | (1u16 << 8),
        u16::from(b'B') | (2u16 << 8),
        u16::from(b'C') | (1u16 << 8),
        u16::from(b'D') | (2u16 << 8),
    ];

    HuffmanTable {
        packed_decode,
        weights: Vec::new(),
        max_num_bits: 2,
        state_mask: 0b11,
        bits: Vec::new(),
        bit_ranks: Vec::new(),
        weight_sum: 0,
        weight_rank_count: [0; (MAX_MAX_NUM_BITS as usize) + 1],
        last_weight: 0,
        fse_table: FSETable::new(255),
    }
}

#[test]
fn build_decoder_rejects_fse_streams_with_256_explicit_weights() {
    // The format caps explicit weights at 255: symbols are u8 and one
    // more weight is inferred, so 256 explicit weights would create a
    // 257-symbol table whose last index wraps through `symbol as u8`.
    // FSE-encode exactly 256 weights (alternating 1/2 keeps the table
    // otherwise valid: weight_sum 384, leftover 128 = 2^7) the same way
    // the encoder's weight-description path does, and require a loud
    // `TooManyWeights` instead of acceptance.
    use crate::bit_io::BitWriter;
    use crate::fse::fse_encoder::{FSEEncoder, build_table_from_symbol_counts};

    let weights: Vec<u8> = (0..256).map(|i| if i % 2 == 0 { 1 } else { 2 }).collect();

    let mut encoded = Vec::new();
    {
        let mut writer = BitWriter::from(&mut encoded);
        let mut counts = [0usize; 13];
        for &w in &weights {
            counts[w as usize] += 1;
        }
        let mut encoder = FSEEncoder::new(
            build_table_from_symbol_counts(&counts, 6, false),
            &mut writer,
        );
        encoder.encode_interleaved(&weights);
        writer.flush();
    }
    assert!(
        encoded.len() < 128,
        "fixture must fit the FSE-described header byte, got {}",
        encoded.len()
    );

    let mut description = Vec::with_capacity(encoded.len() + 1);
    description.push(encoded.len() as u8);
    description.extend_from_slice(&encoded);

    let mut table = HuffmanTable::new();
    let result = table.build_decoder(description.as_slice());
    assert!(
        matches!(result, Err(HuffmanTableError::TooManyWeights { .. })),
        "256 explicit weights must be rejected, got {result:?}"
    );
}

#[test]
fn decode_symbol_and_advance_scalar_matches_manual_transition() {
    let table = test_table();
    let initial_state = 1_u64;
    let packed = table.packed_decode[initial_state as usize];
    let entry_num_bits = (packed >> 8) as u8;
    let entry_symbol = packed as u8;
    let mut manual_br =
        BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&[0b10101010, 0b01010101]);
    let expected_new_bits = manual_br.get_bits(entry_num_bits);
    let expected_state = ((initial_state << entry_num_bits) & table.state_mask) | expected_new_bits;

    let mut decoder = HuffmanDecoder {
        table: &table,
        state: initial_state,
    };
    let mut br =
        BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&[0b10101010, 0b01010101]);
    let symbol = decoder.decode_symbol_and_advance(&mut br);

    assert_eq!(symbol, entry_symbol);
    assert_eq!(decoder.state, expected_state);
}

/// The state advance is the kernel's own instruction (`bzhi` where the tier
/// has it, a mask elsewhere) and the two must agree bit for bit, since the
/// stream they decode does not know which one ran.
#[test]
fn every_kernel_advances_the_state_alike() {
    let table = test_table();
    let source = [0b10101010, 0b01010101];

    let mut scalar = HuffmanDecoder::new(&table);
    let mut scalar_br = BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&source);
    let scalar_symbol = scalar.decode_symbol_and_advance(&mut scalar_br);

    #[cfg(all(target_arch = "x86_64", feature = "kernel-bmi2"))]
    if std::arch::is_x86_feature_detected!("bmi2") {
        let mut bmi2 = HuffmanDecoder::new(&table);
        let mut bmi2_br = BitReaderReversed::<crate::cpu_kernel::Bmi2Kernel>::new(&source);
        assert_eq!(bmi2.decode_symbol_and_advance(&mut bmi2_br), scalar_symbol);
        assert_eq!(bmi2.state, scalar.state);
    }

    #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
    {
        let mut neon = HuffmanDecoder::new(&table);
        let mut neon_br = BitReaderReversed::<crate::cpu_kernel::NeonKernel>::new(&source);
        assert_eq!(neon.decode_symbol_and_advance(&mut neon_br), scalar_symbol);
        assert_eq!(neon.state, scalar.state);
    }
}
