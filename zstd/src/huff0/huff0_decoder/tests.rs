use super::*;
use alloc::vec;

fn test_table() -> HuffmanTable {
    // Packed `num_bits | (symbol << 8)` per state index (upstream zstd `HUF_DEltX1`).
    let packed_decode = vec![
        1u16 | (u16::from(b'A') << 8),
        2u16 | (u16::from(b'B') << 8),
        1u16 | (u16::from(b'C') << 8),
        2u16 | (u16::from(b'D') << 8),
    ];

    HuffmanTable {
        packed_decode,
        weights: Vec::new(),
        max_num_bits: 2,
        state_mask: 0b11,
        bits: Vec::new(),
        bit_ranks: [0; (MAX_MAX_NUM_BITS as usize) + 1],
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
    let entry_num_bits = packed as u8;
    let entry_symbol = (packed >> 8) as u8;
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
/// has it, the table's mask elsewhere) and every tier must agree bit for bit,
/// since the stream they decode does not know which one ran. Every kernel the
/// dispatcher can select on this build runs here, not just the first one.
///
/// The state starts nonzero and the entry decodes fewer bits than the table's
/// width, so the masked value is nonzero too: a kernel that masked wrongly
/// would show it.
#[test]
fn every_kernel_advances_the_state_alike() {
    let mut table = test_table();
    // State 3 decoding one bit leaves `(3 << 1) & 0b11 == 0b10` behind, so the
    // mask has something to keep and a kernel that masked wrongly would show.
    table.packed_decode[3] = 1u16 | (u16::from(b'D') << 8);
    let source = [0b10101010, 0b01010101];
    const START: u64 = 3;

    let mut scalar = HuffmanDecoder::new(&table);
    scalar.state = START;
    let mut scalar_br = BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&source);
    let scalar_symbol = scalar.decode_symbol_and_advance(&mut scalar_br);
    assert_ne!(scalar.state, 0, "the masked state must be nonzero");

    /// Run one kernel over the same bits from the same state and compare.
    macro_rules! same_as_scalar {
        ($kernel:ty) => {{
            let mut decoder = HuffmanDecoder::new(&table);
            decoder.state = START;
            let mut reader = BitReaderReversed::<$kernel>::new(&source);
            assert_eq!(
                decoder.decode_symbol_and_advance(&mut reader),
                scalar_symbol,
                "{} decoded another symbol",
                stringify!($kernel)
            );
            assert_eq!(
                decoder.state,
                scalar.state,
                "{} advanced the state differently",
                stringify!($kernel)
            );
        }};
    }

    // A fresh scalar decoder over the same bits must repeat itself: the
    // baseline every tier below is held to, and the one comparison a build
    // with no SIMD tier still runs.
    same_as_scalar!(crate::cpu_kernel::ScalarKernel);
    #[cfg(all(
        any(target_arch = "x86", target_arch = "x86_64"),
        feature = "kernel-bmi2"
    ))]
    if std::arch::is_x86_feature_detected!("bmi2") {
        same_as_scalar!(crate::cpu_kernel::Bmi2Kernel);
    }
    #[cfg(all(target_arch = "x86_64", feature = "kernel-avx2"))]
    if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("bmi2") {
        same_as_scalar!(crate::cpu_kernel::Avx2Kernel);
    }
    // The same predicate the kernel selection uses, in full: the tier mixes
    // VBMI2 with AVX2 widths and BMI2 masking, so a CPU that offers VBMI2 while
    // masking any of the rest must not reach this monomorph. It would decode
    // through instructions it does not have.
    #[cfg(all(target_arch = "x86_64", feature = "kernel-vbmi2"))]
    if std::arch::is_x86_feature_detected!("avx512vbmi2")
        && std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512vl")
        && std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("bmi2")
        && std::arch::is_x86_feature_detected!("avx2")
    {
        same_as_scalar!(crate::cpu_kernel::Vbmi2Kernel);
    }
    #[cfg(all(target_arch = "aarch64", feature = "kernel-neon"))]
    same_as_scalar!(crate::cpu_kernel::NeonKernel);
    #[cfg(all(target_arch = "aarch64", feature = "kernel-sve", feature = "std"))]
    if std::arch::is_aarch64_feature_detected!("sve") {
        same_as_scalar!(crate::cpu_kernel::SveKernel);
    }
}
