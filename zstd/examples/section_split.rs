//! Diagnostic: split each compressed block into its literals section
//! (Huffman) and sequences section (FSE) byte counts, for the pure-Rust
//! encoder vs the C FFI encoder, on a fixed (corpus, level). When the
//! sequence streams are byte-identical (see compare_ffi_sequences) but the
//! final size differs, this localizes the gap to literals vs sequences.
//!
//! Build: cargo build --release -p structured-zstd --example section_split --features dict_builder
//! Run:   ./target/release/examples/section_split [corpus] [level]

use std::env;
use std::fs;

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};
use zstd::zstd_safe::zstd_sys;

const MAGIC: u32 = 0xFD2F_B528;

/// Parse the literals-section header at `body[0..]`; return
/// `(lit_section_total_bytes, lit_type)`. lit_type: 0=Raw 1=RLE
/// 2=Compressed 3=Treeless.
fn lit_section_len(body: &[u8]) -> (usize, u8) {
    let b0 = body[0] as usize;
    let lit_type = (b0 & 0x3) as u8;
    let sf = (b0 >> 2) & 0x3;
    match lit_type {
        0 | 1 => {
            // Raw / RLE: 1/2/3-byte header carrying Regenerated_Size.
            let (hdr, regen) = match sf {
                0 | 2 => (1usize, b0 >> 3),
                1 => (2, (b0 >> 4) | ((body[1] as usize) << 4)),
                _ => (
                    3,
                    (b0 >> 4) | ((body[1] as usize) << 4) | ((body[2] as usize) << 12),
                ),
            };
            // Raw payload = regen bytes; RLE payload = 1 byte.
            let payload = if lit_type == 0 { regen } else { 1 };
            (hdr + payload, lit_type)
        }
        _ => {
            // Compressed / Treeless: size fields start at bit 4.
            // Fixed-width u64: the 5-byte (sf 3) header packs up to bit 35
            // (`body[4] << 28`), which truncates in a 32-bit `usize`.
            let (hdr, compressed) = match sf {
                0 | 1 => {
                    let v = ((b0 as u64) >> 4) | ((body[1] as u64) << 4) | ((body[2] as u64) << 12);
                    (3, ((v >> 10) & 0x3FF) as usize)
                }
                2 => {
                    let v = ((b0 as u64) >> 4)
                        | ((body[1] as u64) << 4)
                        | ((body[2] as u64) << 12)
                        | ((body[3] as u64) << 20);
                    (4, ((v >> 14) & 0x3FFF) as usize)
                }
                _ => {
                    let v = ((b0 as u64) >> 4)
                        | ((body[1] as u64) << 4)
                        | ((body[2] as u64) << 12)
                        | ((body[3] as u64) << 20)
                        | ((body[4] as u64) << 28);
                    (5, ((v >> 18) & 0x3FFFF) as usize)
                }
            };
            (hdr + compressed, lit_type)
        }
    }
}

/// Skip magic + frame header, returning the offset of the first block.
fn frame_header_len(frame: &[u8]) -> usize {
    assert_eq!(
        u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]),
        MAGIC,
        "not a zstd frame"
    );
    let fhd = frame[4];
    let single_segment = (fhd >> 5) & 1;
    let checksum = (fhd >> 2) & 1;
    let _ = checksum;
    let dict_id_flag = fhd & 0x3;
    let fcs_flag = (fhd >> 6) & 0x3;
    let mut pos = 5usize; // magic(4) + FHD(1)
    if single_segment == 0 {
        pos += 1; // Window_Descriptor
    }
    pos += match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let fcs_bytes = match fcs_flag {
        0 => {
            if single_segment == 1 {
                1
            } else {
                0
            }
        }
        1 => 2,
        2 => 4,
        _ => 8,
    };
    pos + fcs_bytes
}

struct Split {
    blocks: usize,
    raw_blocks: usize,
    rle_blocks: usize,
    comp_blocks: usize,
    lit_bytes: usize,
    seq_bytes: usize,
    lit_type_counts: [usize; 4],
}

fn analyze(frame: &[u8]) -> Split {
    let mut pos = frame_header_len(frame);
    let mut s = Split {
        blocks: 0,
        raw_blocks: 0,
        rle_blocks: 0,
        comp_blocks: 0,
        lit_bytes: 0,
        seq_bytes: 0,
        lit_type_counts: [0; 4],
    };
    loop {
        let bh =
            frame[pos] as u32 | ((frame[pos + 1] as u32) << 8) | ((frame[pos + 2] as u32) << 16);
        let last = bh & 1;
        let btype = (bh >> 1) & 0x3;
        let bsize = (bh >> 3) as usize;
        pos += 3;
        s.blocks += 1;
        match btype {
            0 => {
                s.raw_blocks += 1;
                pos += bsize;
            }
            1 => {
                s.rle_blocks += 1;
                pos += 1; // physical RLE body is one byte
            }
            2 => {
                s.comp_blocks += 1;
                let body = &frame[pos..pos + bsize];
                let (lit_total, lit_type) = lit_section_len(body);
                assert!(
                    lit_total <= bsize,
                    "invalid block split: literals exceed block size \
                     (lit_total={lit_total}, bsize={bsize})"
                );
                s.lit_type_counts[lit_type as usize] += 1;
                s.lit_bytes += lit_total;
                s.seq_bytes += bsize - lit_total;
                pos += bsize;
            }
            _ => panic!("unexpected block type {btype}"),
        }
        if last == 1 {
            break;
        }
    }
    s
}

fn print_split(label: &str, total: usize, s: &Split) {
    println!(
        "{label}: total={total} blocks={} (raw={} rle={} comp={}) lit_section={} seq_section={} lit_types[raw/rle/comp/treeless]={:?}",
        s.blocks,
        s.raw_blocks,
        s.rle_blocks,
        s.comp_blocks,
        s.lit_bytes,
        s.seq_bytes,
        s.lit_type_counts
    );
}

fn main() {
    let corpus = env::args()
        .nth(1)
        .unwrap_or_else(|| "zstd/decodecorpus_files/z000033".to_string());
    let level: i32 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let bytes = fs::read(&corpus).expect("read corpus");

    let rust = compress_slice_to_vec(&bytes, CompressionLevel::Level(level));

    let cap = unsafe { zstd_sys::ZSTD_compressBound(bytes.len()) };
    let mut cbuf = vec![0u8; cap];
    let rc = unsafe {
        zstd_sys::ZSTD_compress(
            cbuf.as_mut_ptr() as *mut core::ffi::c_void,
            cap,
            bytes.as_ptr() as *const core::ffi::c_void,
            bytes.len(),
            level,
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(rc) },
        0,
        "ZSTD_compress failed"
    );
    let ffi = &cbuf[..rc];

    println!(
        "=== section_split corpus={corpus} input={} level={level} ===",
        bytes.len()
    );
    let rs = analyze(&rust);
    let fs_ = analyze(ffi);
    print_split("rust", rust.len(), &rs);
    print_split("ffi ", ffi.len(), &fs_);
    println!(
        "DELTA: total={:+} lit_section={:+} seq_section={:+}",
        rust.len() as i64 - ffi.len() as i64,
        rs.lit_bytes as i64 - fs_.lit_bytes as i64,
        rs.seq_bytes as i64 - fs_.seq_bytes as i64,
    );
}
