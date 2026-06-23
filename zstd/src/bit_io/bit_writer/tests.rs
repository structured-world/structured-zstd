use super::BitWriter;
use alloc::vec;

#[test]
fn from_existing() {
    // Define an existing vec, write some bits into it
    let mut existing_vec = vec![255_u8];
    let mut bw = BitWriter::from(&mut existing_vec);
    bw.write_bits(0u8, 8);
    bw.flush();
    assert_eq!(vec![255, 0], existing_vec);
}

#[test]
fn change_bits() {
    let mut writer = BitWriter::new();
    writer.write_bits(0u32, 24);
    writer.change_bits(8, 0xFFu8, 8);
    assert_eq!(vec![0, 0xFF, 0], writer.dump());

    let mut writer = BitWriter::new();
    writer.write_bits(0u32, 24);
    writer.change_bits(6, 0x0FFFu16, 12);
    assert_eq!(vec![0b11000000, 0xFF, 0b00000011], writer.dump());
}

#[test]
fn single_byte_written_4_4() {
    // Write the first 4 bits as 1s and the last 4 bits as 0s
    // 1010 is used where values should never be read from.
    let mut bw = BitWriter::new();
    bw.write_bits(0b1111u8, 4);
    bw.write_bits(0b0000u8, 4);
    let output = bw.dump();
    assert!(
        output.len() == 1,
        "Single byte written into writer returned a vec that wasn't one byte, vec was {} elements long",
        output.len()
    );
    assert_eq!(
        0b0000_1111, output[0],
        "4 bits and 4 bits written into buffer"
    );
}

#[test]
fn single_byte_written_3_5() {
    // Write the first 3 bits as 1s and the last 5 bits as 0s
    let mut bw = BitWriter::new();
    bw.write_bits(0b111u8, 3);
    bw.write_bits(0b0_0000u8, 5);
    let output = bw.dump();
    assert!(
        output.len() == 1,
        "Single byte written into writer return a vec that wasn't one byte, vec was {} elements long",
        output.len()
    );
    assert_eq!(0b0000_0111, output[0], "3 and 5 bits written into buffer");
}

#[test]
fn single_byte_written_1_7() {
    // Write the first bit as a 1 and the last 7 bits as 0s
    let mut bw = BitWriter::new();
    bw.write_bits(0b1u8, 1);
    bw.write_bits(0u8, 7);
    let output = bw.dump();
    assert!(
        output.len() == 1,
        "Single byte written into writer return a vec that wasn't one byte, vec was {} elements long",
        output.len()
    );
    assert_eq!(0b0000_0001, output[0], "1 and 7 bits written into buffer");
}

#[test]
fn single_byte_written_8() {
    // Write an entire byte
    let mut bw = BitWriter::new();
    bw.write_bits(1u8, 8);
    let output = bw.dump();
    assert!(
        output.len() == 1,
        "Single byte written into writer return a vec that wasn't one byte, vec was {} elements long",
        output.len()
    );
    assert_eq!(1, output[0], "1 and 7 bits written into buffer");
}

#[test]
fn multi_byte_clean_boundary_4_4_4_4() {
    // Writing 4 bits at a time for 2 bytes
    let mut bw = BitWriter::new();
    bw.write_bits(0u8, 4);
    bw.write_bits(0b1111u8, 4);
    bw.write_bits(0b1111u8, 4);
    bw.write_bits(0u8, 4);
    assert_eq!(vec![0b1111_0000, 0b0000_1111], bw.dump());
}

#[test]
fn multi_byte_clean_boundary_16_8() {
    // Writing 16 bits at once
    let mut bw = BitWriter::new();
    bw.write_bits(0x0100u16, 16);
    bw.write_bits(69u8, 8);
    assert_eq!(vec![0, 1, 69], bw.dump())
}

#[test]
fn multi_byte_boundary_crossed_4_12() {
    // Writing 4 1s and then 12 zeros
    let mut bw = BitWriter::new();
    bw.write_bits(0b1111u8, 4);
    bw.write_bits(0b0000_0011_0100_0010u16, 12);
    assert_eq!(vec![0b0010_1111, 0b0011_0100], bw.dump());
}

#[test]
fn multi_byte_boundary_crossed_4_5_7() {
    // Writing 4 1s and then 5 zeros then 7 1s
    let mut bw = BitWriter::new();
    bw.write_bits(0b1111u8, 4);
    bw.write_bits(0b0_0000u8, 5);
    bw.write_bits(0b111_1111u8, 7);
    assert_eq!(vec![0b0000_1111, 0b1111_1110], bw.dump());
}

#[test]
fn multi_byte_boundary_crossed_1_9_6() {
    // Writing 1 1 and then 9 zeros then 6 1s
    let mut bw = BitWriter::new();
    bw.write_bits(0b1u8, 1);
    bw.write_bits(0b0_0000_0000u16, 9);
    bw.write_bits(0b11_1111u8, 6);
    assert_eq!(vec![0b0000_0001, 0b1111_1100], bw.dump());
}

#[test]
#[should_panic]
fn catches_unaligned_dump() {
    // Write a single bit in then dump it, making sure
    // the correct error is returned
    let mut bw = BitWriter::new();
    bw.write_bits(0u8, 1);
    bw.dump();
}

#[test]
#[should_panic]
fn catches_dirty_upper_bits() {
    let mut bw = BitWriter::new();
    bw.write_bits(10u8, 1);
}

#[test]
fn add_multiple_aligned() {
    let mut bw = BitWriter::new();
    bw.write_bits(0x00_0F_F0_FFu32, 32);
    assert_eq!(vec![0xFF, 0xF0, 0x0F, 0x00], bw.dump());
}

// #[test]
// fn catches_more_than_in_buf() {
//     todo!();
// }
