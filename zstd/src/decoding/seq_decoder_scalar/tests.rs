use super::*;

/// `read_additional_bits` skips the width-zero mask of the general reader; it
/// must still return the same value and leave the same bit position for every
/// width the sequence tables allow, at every consumed-bit offset.
#[test]
fn nonzero_reads_match_general_reader_at_every_bit_position() {
    let source = [
        0x13, 0xe7, 0x82, 0x40, 0xf1, 0x39, 0xab, 0x6c, 0x9d, 0x05, 0xd2, 0x71, 0xfe, 0x48, 0x36,
        0xb0,
    ];
    for width in 1..=31 {
        for consumed in 0..=64 - width {
            let mut fast = BitReaderReversed::<ScalarKernel>::new(&source);
            let mut reference = BitReaderReversed::<ScalarKernel>::new(&source);
            fast.refill();
            reference.refill();
            fast.consume(consumed);
            reference.consume(consumed);
            assert_eq!(
                u64::from(read_additional_bits(&mut fast, width)),
                reference.get_bits_unchecked(width)
            );
            assert_eq!(fast.bits_remaining(), reference.bits_remaining());
        }
    }
}
