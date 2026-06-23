use super::FrameHeader;
use crate::decoding::frame::{FrameDescriptor, read_frame_header};
use alloc::vec::Vec;

#[test]
fn frame_header_descriptor_decode() {
    let header = FrameHeader {
        frame_content_size: Some(1),
        single_segment: true,
        content_checksum: false,
        dictionary_id: None,
        window_size: None,
        magicless: false,
    };
    let descriptor = header.descriptor();
    let decoded_descriptor = FrameDescriptor(descriptor);
    assert_eq!(decoded_descriptor.frame_content_size_bytes().unwrap(), 1);
    assert!(!decoded_descriptor.content_checksum_flag());
    assert_eq!(decoded_descriptor.dictionary_id_bytes().unwrap(), 0);
}

#[test]
fn frame_header_decode() {
    let header = FrameHeader {
        frame_content_size: Some(1),
        single_segment: true,
        content_checksum: false,
        dictionary_id: None,
        window_size: None,
        magicless: false,
    };

    let mut serialized_header = Vec::new();
    header.serialize(&mut serialized_header);
    let parsed_header = read_frame_header(serialized_header.as_slice()).unwrap().0;
    assert!(parsed_header.dictionary_id().is_none());
    assert_eq!(parsed_header.frame_content_size(), 1);
}

// Locks the descriptor/FCS field-size class boundaries: 255/256 is the
// 1-byte (single-segment) vs 2-byte edge, 65791/65792 the 2-byte
// (+256 offset) vs 4-byte edge. A drift between `find_fcs_field_size`,
// the descriptor flag mapping, and the FCS write would round-trip to a
// wrong size through the decoder.
#[test]
fn frame_header_fcs_boundaries_round_trip() {
    for (frame_content_size, single_segment) in [
        (255, true),
        (256, true),
        (65791, true),
        (65792, true),
        (255, false),
        (256, false),
        (65791, false),
        (65792, false),
    ] {
        let header = FrameHeader {
            frame_content_size: Some(frame_content_size),
            single_segment,
            content_checksum: false,
            dictionary_id: None,
            window_size: if single_segment { None } else { Some(1024) },
            magicless: false,
        };

        let mut serialized_header = Vec::new();
        header.serialize(&mut serialized_header);
        let parsed_header = read_frame_header(serialized_header.as_slice())
            .expect("serialized header must parse")
            .0;
        assert_eq!(
            parsed_header.frame_content_size(),
            frame_content_size,
            "FCS must round-trip at boundary {frame_content_size} (single_segment={single_segment})"
        );
    }
}

// The dictionary-id field-size classes (1/2/4 bytes by value magnitude)
// share the descriptor's bits 0-1; lock their boundaries through the
// decoder as well.
#[test]
fn frame_header_dictionary_id_boundaries_round_trip() {
    for dictionary_id in [1, 255, 256, 65535, 65536, u32::MAX as u64] {
        let header = FrameHeader {
            frame_content_size: Some(4096),
            single_segment: false,
            content_checksum: false,
            dictionary_id: Some(dictionary_id),
            window_size: Some(1024),
            magicless: false,
        };

        let mut serialized_header = Vec::new();
        header.serialize(&mut serialized_header);
        let parsed_header = read_frame_header(serialized_header.as_slice())
            .expect("serialized header must parse")
            .0;
        assert_eq!(
            parsed_header.dictionary_id(),
            Some(dictionary_id as u32),
            "dictionary id must round-trip at boundary {dictionary_id}"
        );
    }
}

#[test]
#[should_panic]
fn catches_single_segment_no_fcs() {
    let header = FrameHeader {
        frame_content_size: None,
        single_segment: true,
        content_checksum: false,
        dictionary_id: None,
        window_size: Some(1),
        magicless: false,
    };

    let mut serialized_header = Vec::new();
    header.serialize(&mut serialized_header);
}

#[test]
#[should_panic]
fn catches_single_segment_no_winsize() {
    let header = FrameHeader {
        frame_content_size: Some(7),
        single_segment: false,
        content_checksum: false,
        dictionary_id: None,
        window_size: None,
        magicless: false,
    };

    let mut serialized_header = Vec::new();
    header.serialize(&mut serialized_header);
}
