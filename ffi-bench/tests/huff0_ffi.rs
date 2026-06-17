//! Cross-implementation conformance for the Huffman (HUF) encoder: the weight
//! description, the 4-stream payload, and every HUF literal section a level-22
//! frame emits must be accepted by the C `zstd` HUF reader. The encoder-side
//! captures come through the `structured_zstd::testing` facade (pure Rust); the
//! C readers are linked only here, never in the library crate.
#![cfg(feature = "bench_internals")]

use structured_zstd::encoding::{CompressionLevel, compress_to_vec};
use structured_zstd::testing::{huf_encode4x, huf_weight_description};

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

const CORPUS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../zstd/decodecorpus_files/z000033"
));

fn assert_zstd_ok(code: usize, context: &str) {
    assert_eq!(
        unsafe { zstd::zstd_safe::zstd_sys::ZSTD_isError(code) },
        0,
        "{context}: {}",
        zstd::zstd_safe::get_error_name(code)
    );
}

#[test]
fn encoded_weight_description_is_accepted_by_reference_huf_reader() {
    let data = &CORPUS[..16 * 1024];
    let (description, weights) = huf_weight_description(data);

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
    assert_zstd_ok(read, "HUF_readStats rejected weight description");
    assert_eq!(read, description.len());
    assert_eq!(&huff_weight[..weights.len()], weights.as_slice());
}

#[test]
fn encoded_huffman_payload_is_accepted_by_reference_huf_reader() {
    let data = &CORPUS[..16 * 1024];
    let encoded = huf_encode4x(data);

    let mut decoded = vec![0u8; data.len()];
    let mut dtable = vec![0u32; 1 + (1 << 12)];
    dtable[0] = 12 * 0x01010101;
    let mut workspace = vec![0u64; 1 << 15];
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
    assert_zstd_ok(read, "HUF_decompress4X_hufOnly_wksp rejected payload");
    assert_eq!(read, data.len());
    assert_eq!(decoded.as_slice(), data);
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

#[test]
fn level22_emitted_literal_sections_are_accepted_by_reference_huf_reader() {
    let frame = compress_to_vec(CORPUS, CompressionLevel::Level(22));
    let mut pos = frame_blocks_offset(&frame);
    let mut dtable = vec![0u32; 1 + (1 << 12)];
    dtable[0] = 12 * 0x01010101;
    let mut workspace = vec![0u64; 1 << 15];
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
                    let mut decoded = vec![0u8; lit_size];
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
                    assert_zstd_ok(
                        code,
                        &format!(
                            "C HUF rejected block {block_idx} lit_type={lit_type} single={single_stream} lit_size={lit_size} lit_c_size={lit_c_size}"
                        ),
                    );
                    assert_eq!(code, lit_size, "C HUF decoded short block {block_idx}");
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
