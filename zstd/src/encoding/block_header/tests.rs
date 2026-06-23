use super::BlockHeader;
use crate::{blocks::block::BlockType, decoding::block_decoder};
use alloc::vec::Vec;

#[test]
fn block_header_serialize() {
    let header = BlockHeader {
        last_block: true,
        block_type: super::BlockType::Compressed,
        block_size: 69,
    };
    let mut serialized_header = Vec::new();
    header.serialize(&mut serialized_header);
    let mut decoder = block_decoder::new();
    let parsed_header = decoder
        .read_block_header(serialized_header.as_slice())
        .unwrap()
        .0;

    assert!(parsed_header.last_block);
    assert_eq!(parsed_header.block_type, BlockType::Compressed);
    assert_eq!(parsed_header.content_size, 69);
}
