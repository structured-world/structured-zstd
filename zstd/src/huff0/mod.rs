/// Huffman coding is a method of encoding where symbols are assigned a code,
/// and more commonly used symbols get shorter codes, and less commonly
/// used symbols get longer codes. Codes are prefix free, meaning no two codes
/// will start with the same sequence of bits.
pub(crate) mod huff0_decoder;
pub use huff0_decoder::*;
pub(crate) mod huf_cstream;
pub mod huff0_encoder;

/// Only needed for testing.
///
/// Encodes the data with a table built from that data
/// Decodes the result again by first decoding the table and then the data
/// Asserts that the decoded data equals the input
#[cfg(any(test, feature = "fuzz_exports"))]
pub fn round_trip(data: &[u8]) {
    use crate::bit_io::{BitReaderReversed, BitWriter};
    use alloc::vec::Vec;

    if data.len() < 2 {
        return;
    }
    if data.iter().all(|x| *x == data[0]) {
        return;
    }
    let mut writer = BitWriter::new();
    let encoder_table = huff0_encoder::HuffmanTable::build_from_data(data);
    encoder_table
        .writeable_table_description_size()
        .expect("round_trip must only build Huffman tables with a writeable description");
    let mut encoder = huff0_encoder::HuffmanEncoder::new(&encoder_table, &mut writer);

    encoder.encode(data, true);
    let encoded = writer.dump();
    let mut decoder_table = HuffmanTable::new();
    let table_bytes = decoder_table.build_decoder(&encoded).unwrap();
    let mut decoder = HuffmanDecoder::new(&decoder_table);

    let mut br =
        BitReaderReversed::<crate::cpu_kernel::ScalarKernel>::new(&encoded[table_bytes as usize..]);
    let mut skipped_bits = 0;
    loop {
        let val = br.get_bits(1);
        skipped_bits += 1;
        if val == 1 || skipped_bits > 8 {
            break;
        }
    }
    if skipped_bits > 8 {
        //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
        panic!("Corrupted end marker");
    }

    decoder.init_state(&mut br);
    let mut decoded = Vec::new();
    while br.bits_remaining() > -(decoder_table.max_num_bits as isize) {
        decoded.push(decoder.decode_symbol_and_advance(&mut br));
    }
    assert_eq!(&decoded, data);
}

#[cfg(test)]
mod tests;
