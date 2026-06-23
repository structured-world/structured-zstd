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
        kernel: HuffmanDecodeKernel::Scalar,
        state: initial_state,
    };
    let mut br =
        BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&[0b10101010, 0b01010101]);
    let symbol = decoder.decode_symbol_and_advance(&mut br);

    assert_eq!(symbol, entry_symbol);
    assert_eq!(decoder.state, expected_state);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[test]
fn select_x86_kernel_ordering_is_stable() {
    assert_eq!(
        select_x86_huffman_decode_kernel(true, true, true, true, true, true),
        HuffmanDecodeKernel::X86Vbmi2
    );
    assert_eq!(
        select_x86_huffman_decode_kernel(false, false, false, false, true, true),
        HuffmanDecodeKernel::X86Avx2
    );
    assert_eq!(
        select_x86_huffman_decode_kernel(false, false, false, false, true, false),
        HuffmanDecodeKernel::X86Bmi2
    );
    assert_eq!(
        select_x86_huffman_decode_kernel(false, false, false, false, false, true),
        HuffmanDecodeKernel::Scalar
    );
}
