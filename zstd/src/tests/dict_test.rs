#[test]
fn test_dict_parsing() {
    use crate::decoding::dictionary::Dictionary;
    use alloc::vec;
    let mut raw = vec![0u8; 8];

    // correct magic num
    raw[0] = 0x37;
    raw[1] = 0xA4;
    raw[2] = 0x30;
    raw[3] = 0xEC;

    //dict-id
    let dict_id = 0x47232101;
    raw[4] = 0x01;
    raw[5] = 0x21;
    raw[6] = 0x23;
    raw[7] = 0x47;

    // tables copied from ./dict_tests/dictionary
    let raw_tables = &[
        54, 16, 192, 155, 4, 0, 207, 59, 239, 121, 158, 116, 220, 93, 114, 229, 110, 41, 249, 95,
        165, 255, 83, 202, 254, 68, 74, 159, 63, 161, 100, 151, 137, 21, 184, 183, 189, 100, 235,
        209, 251, 174, 91, 75, 91, 185, 19, 39, 75, 146, 98, 177, 249, 14, 4, 35, 0, 0, 0, 40, 40,
        20, 10, 12, 204, 37, 196, 1, 173, 122, 0, 4, 0, 128, 1, 2, 2, 25, 32, 27, 27, 22, 24, 26,
        18, 12, 12, 15, 16, 11, 69, 37, 225, 48, 20, 12, 6, 2, 161, 80, 40, 20, 44, 137, 145, 204,
        46, 0, 0, 0, 0, 0, 116, 253, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    raw.extend(&raw_tables[..]);

    //offset history 3,10,0x00ABCDEF
    raw.extend(vec![3, 0, 0, 0]);
    raw.extend(vec![10, 0, 0, 0]);
    raw.extend(vec![0xEF, 0xCD, 0xAB, 0]);

    //just some random bytes
    let raw_content = vec![
        1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 1, 1, 123, 3, 234, 23, 234, 34, 23, 234, 34, 34, 234, 234,
    ];
    raw.extend(&raw_content);

    let dict = Dictionary::decode_dict(&raw).unwrap();

    if dict.id != dict_id {
        panic!(
            "Dict-id did not get parsed correctly. Is: {}, Should be: {}",
            dict.id, dict_id
        );
    }

    if !dict.dict_content.eq(&raw_content) {
        panic!(
            "dict content did not get parsed correctly. Is: {:?}, Should be: {:?}",
            dict.dict_content, raw_content
        );
    }

    if !dict.offset_hist.eq(&[3, 10, 0x00ABCDEF]) {
        panic!(
            "offset history did not get parsed correctly. Is: {:?}, Should be: {:?}",
            dict.offset_hist,
            [3, 10, 0x00ABCDEF]
        );
    }

    // test magic num checking
    raw[0] = 1;
    raw[1] = 1;
    raw[2] = 1;
    raw[3] = 1;
    match Dictionary::decode_dict(&raw) {
        Ok(_) => panic!("The dict got decoded but the magic num was incorrect!"),
        Err(_) => { /* This is what should happen*/ }
    }
}

#[test]
fn test_dictionary_handle_from_dictionary_roundtrips() {
    use crate::decoding::dictionary::{Dictionary, DictionaryHandle};
    use alloc::vec;

    let dict = Dictionary::from_raw_content(1, vec![1, 2, 3]).unwrap();
    let handle = DictionaryHandle::from_dictionary(dict);
    assert_eq!(handle.id(), 1);
    assert_eq!(handle.as_dict().dict_content, vec![1, 2, 3]);

    let dict = Dictionary::from_raw_content(2, vec![4]).unwrap();
    let handle: DictionaryHandle = dict.into();
    assert_eq!(handle.id(), 2);
    assert_eq!(handle.as_dict().dict_content, vec![4]);
}

#[cfg(target_has_atomic = "ptr")]
#[test]
fn test_dict_decoding() {
    extern crate std;
    use crate::decoding::BlockDecodingStrategy;
    use crate::decoding::FrameDecoder;
    use alloc::borrow::ToOwned;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::fs;
    use std::io::BufReader;
    use std::io::Read;
    use std::println;

    let mut success_counter = 0;
    let mut fail_counter_diff = 0;
    let mut fail_counter_size = 0;
    let mut fail_counter_bytes_read = 0;
    let mut total_counter = 0;
    let mut failed: Vec<String> = Vec::new();

    let mut speeds = Vec::new();
    let mut speeds_read = Vec::new();

    let mut files: Vec<_> = fs::read_dir("./dict_tests/files").unwrap().collect();
    let dict = BufReader::new(fs::File::open("./dict_tests/dictionary").unwrap());
    let dict: Vec<u8> = dict.bytes().map(|x| x.unwrap()).collect();

    files.sort_by_key(|x| match x {
        Err(_) => "".to_owned(),
        Ok(entry) => entry.path().to_str().unwrap().to_owned(),
    });

    let mut frame_dec = FrameDecoder::new();
    let dict = crate::decoding::DictionaryHandle::decode_dict(&dict).unwrap();
    frame_dec.add_dict_handle(dict).unwrap();

    for file in files {
        let f = file.unwrap();
        let metadata = f.metadata().unwrap();
        let file_size = metadata.len();

        let p = String::from(f.path().to_str().unwrap());
        if !p.ends_with(".zst") {
            continue;
        }
        println!("Trying file: {p}");

        let mut content = fs::File::open(f.path()).unwrap();

        frame_dec.reset(&mut content).unwrap();

        let start_time = std::time::Instant::now();
        /////DECODING
        frame_dec
            .decode_blocks(&mut content, BlockDecodingStrategy::All)
            .unwrap();
        let result = frame_dec.collect().unwrap();
        let end_time = start_time.elapsed();

        match frame_dec.get_checksum_from_data() {
            Some(chksum) => {
                #[cfg(feature = "hash")]
                if frame_dec.get_calculated_checksum().unwrap() != chksum {
                    println!(
                        "Checksum did not match! From data: {}, calculated while decoding: {}\n",
                        chksum,
                        frame_dec.get_calculated_checksum().unwrap()
                    );
                } else {
                    println!("Checksums are ok!\n");
                }
                #[cfg(not(feature = "hash"))]
                println!(
                    "Checksum feature not enabled, skipping. From data: {}\n",
                    chksum
                );
            }
            None => println!("No checksums to test\n"),
        }

        let mut original_p = p.clone();
        original_p.truncate(original_p.len() - 4);
        let original_f = BufReader::new(fs::File::open(original_p).unwrap());
        let original: Vec<u8> = original_f.bytes().map(|x| x.unwrap()).collect();

        println!("Results for file: {}", p.clone());
        let mut success = true;

        if original.len() != result.len() {
            println!(
                "Result has wrong length: {}, should be: {}",
                result.len(),
                original.len()
            );
            success = false;
            fail_counter_size += 1;
        }

        if frame_dec.bytes_read_from_source() != file_size {
            println!(
                "Framedecoder counted wrong amount of bytes: {}, should be: {}",
                frame_dec.bytes_read_from_source(),
                file_size
            );
            success = false;
            fail_counter_bytes_read += 1;
        }

        let mut counter = 0;
        let min = if original.len() < result.len() {
            original.len()
        } else {
            result.len()
        };
        for idx in 0..min {
            if original[idx] != result[idx] {
                counter += 1;
                //println!(
                //    "Original {} not equal to result {} at byte: {}",
                //    original[idx], result[idx], idx,
                //);
            }
        }

        if counter > 0 {
            println!("Result differs in at least {counter} bytes from original");
            success = false;
            fail_counter_diff += 1;
        }

        if success {
            success_counter += 1;
        } else {
            failed.push(p.clone().to_string());
        }
        total_counter += 1;

        let dur = end_time.as_micros() as usize;
        let speed = result.len() / if dur == 0 { 1 } else { dur };
        let speed_read = file_size as usize / if dur == 0 { 1 } else { dur };
        println!("SPEED: {speed}");
        println!("SPEED_read: {speed_read}");
        speeds.push(speed);
        speeds_read.push(speed_read);
    }

    println!("###################");
    println!("Summary:");
    println!("###################");
    println!(
        "Total: {total_counter}, Success: {success_counter}, WrongSize: {fail_counter_size}, WrongBytecount: {fail_counter_bytes_read}, Diffs: {fail_counter_diff}"
    );
    println!("Failed files: ");
    for f in &failed {
        println!("{f}");
    }

    let speed_len = speeds.len();
    let sum_speed: usize = speeds.into_iter().sum();
    let avg_speed = sum_speed / speed_len;
    let avg_speed_bps = avg_speed * 1_000_000;
    if avg_speed_bps < 1000 {
        println!("Average speed: {avg_speed_bps} B/s");
    } else if avg_speed_bps < 1_000_000 {
        println!("Average speed: {} KB/s", avg_speed_bps / 1000);
    } else {
        println!("Average speed: {} MB/s", avg_speed_bps / 1_000_000);
    }

    let speed_read_len = speeds_read.len();
    let sum_speed_read: usize = speeds_read.into_iter().sum();
    let avg_speed_read = sum_speed_read / speed_read_len;
    let avg_speed_read_bps = avg_speed_read * 1_000_000;
    if avg_speed_read_bps < 1000 {
        println!("Average speed reading: {avg_speed_read_bps} B/s");
    } else if avg_speed_bps < 1_000_000 {
        println!("Average speed reading: {} KB/s", avg_speed_read_bps / 1000);
    } else {
        println!(
            "Average speed reading: {} MB/s",
            avg_speed_read_bps / 1_000_000
        );
    }

    assert!(failed.is_empty());
}

#[test]
fn test_decode_all_skips_skippable_frames() {
    extern crate std;
    use crate::decoding::FrameDecoder;
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec;
    use alloc::vec::Vec;

    let payload = b"decode-all-skip-frame";
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut input = Vec::new();
    let magic = 0x184D2A50u32.to_le_bytes();
    let skippable_payload = [0xAA, 0xBB, 0xCC, 0xDD];
    let length = (skippable_payload.len() as u32).to_le_bytes();
    input.extend_from_slice(&magic);
    input.extend_from_slice(&length);
    input.extend_from_slice(&skippable_payload);
    input.extend_from_slice(&compressed);

    let mut output = vec![0u8; payload.len()];
    let written = FrameDecoder::new()
        .decode_all(input.as_slice(), &mut output)
        .expect("decode_all should succeed");
    assert_eq!(written, payload.len());
    assert_eq!(output, payload);
}

#[test]
fn test_decode_all_reports_target_too_small() {
    extern crate std;
    use crate::decoding::FrameDecoder;
    use crate::decoding::errors::FrameDecoderError;
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec;
    use alloc::vec::Vec;

    let payload = b"decode-all-target-too-small";
    let mut compressor = FrameCompressor::new(CompressionLevel::Default);
    compressor.set_source(payload.as_slice());
    let mut compressed = Vec::new();
    compressor.set_drain(&mut compressed);
    compressor.compress();

    let mut output = vec![0u8; payload.len() - 1];
    let result = FrameDecoder::new().decode_all(compressed.as_slice(), &mut output);
    assert!(matches!(result, Err(FrameDecoderError::TargetTooSmall)));
}

#[test]
fn test_decode_all_with_dict_helpers() {
    extern crate std;
    use crate::decoding::{DictionaryHandle, FrameDecoder, StreamingDecoder};
    use alloc::vec;
    use alloc::vec::Vec;
    use std::fs;
    use std::io::Read;

    let dict_raw = fs::read("./dict_tests/dictionary").expect("dictionary should load");
    let (compressed, original) = load_sample_dict_frame();
    let handle = DictionaryHandle::decode_dict(&dict_raw).expect("dictionary should parse");

    let mut output = vec![0u8; original.len()];
    let written = FrameDecoder::new()
        .decode_all_with_dict_handle(compressed.as_slice(), &mut output, &handle)
        .expect("decode_all_with_dict_handle should succeed");
    assert_eq!(written, original.len());
    assert_eq!(output, original);

    let mut output = vec![0u8; original.len()];
    let written = FrameDecoder::new()
        .decode_all_with_dict_bytes(compressed.as_slice(), &mut output, &dict_raw)
        .expect("decode_all_with_dict_bytes should succeed");
    assert_eq!(written, original.len());
    assert_eq!(output, original);

    let mut decoder = StreamingDecoder::new_with_dictionary_handle(compressed.as_slice(), &handle)
        .expect("streaming decoder should init");
    let mut streamed = Vec::new();
    Read::read_to_end(&mut decoder, &mut streamed).expect("streaming read should succeed");
    assert_eq!(streamed, original);

    let mut decoder = StreamingDecoder::new_with_dictionary_bytes(compressed.as_slice(), &dict_raw)
        .expect("streaming decoder should init");
    let mut streamed = Vec::new();
    Read::read_to_end(&mut decoder, &mut streamed).expect("streaming read should succeed");
    assert_eq!(streamed, original);
}

#[test]
fn test_add_dict_from_bytes_allows_decode_all() {
    extern crate std;
    use crate::decoding::FrameDecoder;
    use crate::decoding::dictionary::Dictionary;
    use crate::decoding::errors::FrameDecoderError;
    use alloc::vec;
    use std::fs;

    let dict_raw = fs::read("./dict_tests/dictionary").expect("dictionary should load");
    let (compressed, original) = load_sample_dict_frame();

    let mut output = vec![0u8; original.len()];
    let mut decoder = FrameDecoder::new();
    decoder
        .add_dict_from_bytes(&dict_raw)
        .expect("dict bytes should parse");
    let written = decoder
        .decode_all(compressed.as_slice(), &mut output)
        .expect("decode_all should succeed");
    assert_eq!(written, original.len());
    assert_eq!(output, original);

    let mut decoder = FrameDecoder::new();
    let dict = Dictionary::from_raw_content(7, vec![1u8]).expect("dict should build");
    decoder.add_dict(dict).expect("dict should insert");
    let dict = Dictionary::from_raw_content(7, vec![2u8]).expect("dict should build");
    let result = decoder.add_dict(dict);
    assert!(matches!(
        result,
        Err(FrameDecoderError::DictAlreadyRegistered { dict_id }) if dict_id == 7
    ));
}

#[cfg(target_has_atomic = "ptr")]
#[test]
fn test_add_dict_handle_rejects_existing_owned_id() {
    extern crate std;
    use crate::decoding::FrameDecoder;
    use crate::decoding::dictionary::{Dictionary, DictionaryHandle};
    use crate::decoding::errors::FrameDecoderError;
    use alloc::vec;

    let mut decoder = FrameDecoder::new();
    let dict = Dictionary::from_raw_content(9, vec![1u8]).expect("dict should build");
    decoder.add_dict(dict).expect("dict should insert");

    let handle = DictionaryHandle::from_dictionary(
        Dictionary::from_raw_content(9, vec![2u8]).expect("dict should build"),
    );
    let result = decoder.add_dict_handle(handle);

    assert!(matches!(
        result,
        Err(FrameDecoderError::DictAlreadyRegistered { dict_id }) if dict_id == 9
    ));
}

#[test]
fn test_reset_with_dict_handle_rejects_mismatched_id() {
    extern crate std;
    use crate::decoding::FrameDecoder;
    use crate::decoding::dictionary::{Dictionary, DictionaryHandle};
    use crate::decoding::errors::FrameDecoderError;
    use alloc::vec;
    use std::fs;

    let dict_raw = fs::read("./dict_tests/dictionary").expect("dictionary should load");
    let expected_dict_id = DictionaryHandle::decode_dict(&dict_raw)
        .expect("dictionary should parse")
        .id();
    let mismatched_id = if expected_dict_id == u32::MAX {
        expected_dict_id - 1
    } else {
        expected_dict_id + 1
    };
    let mismatched = Dictionary::from_raw_content(mismatched_id, vec![1u8])
        .expect("mismatched dictionary should build");
    let handle = DictionaryHandle::from_dictionary(mismatched);

    let (compressed, _original) = load_sample_dict_frame();
    let mut decoder = FrameDecoder::new();
    let result = decoder.reset_with_dict_handle(compressed.as_slice(), &handle);

    assert!(matches!(
        result,
        Err(FrameDecoderError::DictNotProvided { dict_id }) if dict_id == expected_dict_id
    ));
}

#[cfg(test)]
fn load_sample_dict_frame() -> (alloc::vec::Vec<u8>, alloc::vec::Vec<u8>) {
    extern crate std;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::fs;

    let mut files: Vec<std::io::Result<std::fs::DirEntry>> = fs::read_dir("./dict_tests/files")
        .expect("dict test files should exist")
        .collect();
    files.sort_by_key(|entry| match entry {
        Err(_) => String::new(),
        Ok(dir_entry) => dir_entry.path().to_string_lossy().to_string(),
    });

    let (file_path, compressed) = files
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find_map(|path| {
            let is_zst = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "zst")
                .unwrap_or(false);
            if !is_zst {
                return None;
            }

            let compressed = fs::read(&path).ok()?;
            let mut header_src = compressed.as_slice();
            let (header, _) = crate::decoding::frame::read_frame_header(&mut header_src).ok()?;
            if header.dictionary_id().is_some() {
                Some((path, compressed))
            } else {
                None
            }
        })
        .expect("expected at least one dictionary-backed .zst file in dict_tests/files");
    let original_path = file_path
        .to_str()
        .expect("dict test path should be utf-8")
        .trim_end_matches(".zst")
        .to_string();
    let original = fs::read(original_path).expect("original data should load");

    (compressed, original)
}
