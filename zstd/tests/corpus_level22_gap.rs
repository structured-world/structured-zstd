use std::fs;
use std::path::Path;
use structured_zstd::encoding::{CompressionLevel, compress_to_vec};

#[test]
#[ignore = "manual probe — run with --ignored"]
fn corpus_z000033_level22_ratio_breakdown() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("decodecorpus_files/z000033");
    let data = fs::read(&path).expect("z000033 corpus file present");
    println!("Corpus z000033 size: {} bytes", data.len());

    let rust = compress_to_vec(&data[..], CompressionLevel::Level(22));
    let c = zstd::bulk::compress(&data[..], 22).unwrap();
    let delta = rust.len() as i64 - c.len() as i64;
    let pct = (rust.len() as f64 / c.len() as f64 - 1.0) * 100.0;
    println!(
        "Level 22 output:  rust={}  c={}  delta={delta:+}b ({pct:+.4}%)",
        rust.len(),
        c.len()
    );

    // Count block types in each output so we can tell if the gap is
    // structural (different block split decisions) or content
    // (per-block entropy differences within identical splits).
    fn block_breakdown(label: &str, frame: &[u8]) -> (usize, usize, usize, usize) {
        if frame.len() < 6 || &frame[..4] != b"\x28\xb5\x2f\xfd" {
            return (0, 0, 0, 0);
        }
        let fhd = frame[4];
        let single_segment = (fhd & 0x20) != 0;
        let fcs_flag = fhd >> 6;
        let dict_id_flag = fhd & 0x03;
        let dict_id_len = match dict_id_flag {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let window_descriptor_len = if single_segment { 0 } else { 1 };
        let fcs_len = match fcs_flag {
            0 => {
                if single_segment {
                    1
                } else {
                    0
                }
            }
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let mut off = 5 + window_descriptor_len + dict_id_len + fcs_len;
        let mut raw = 0;
        let mut rle = 0;
        let mut compressed = 0;
        let mut total = 0;
        for _ in 0..256 {
            if off + 3 > frame.len() {
                break;
            }
            let h = u32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], 0]);
            let last = h & 1;
            let bt = (h >> 1) & 3;
            let bs = (h >> 3) as usize;
            match bt {
                0 => raw += 1,
                1 => rle += 1,
                2 => compressed += 1,
                _ => {}
            }
            total += bs;
            off += 3 + if bt == 1 { 1 } else { bs };
            if last == 1 {
                break;
            }
        }
        println!(
            "  {label}: raw={raw}, rle={rle}, compressed={compressed}, total_block_bytes={total}"
        );
        (raw, rle, compressed, total)
    }
    block_breakdown("Rust", &rust);
    block_breakdown("C   ", &c);

    // Sanity: outputs must roundtrip.
    let rt = zstd::bulk::decompress(&rust, data.len() + 64).unwrap();
    assert_eq!(
        rt.len(),
        data.len(),
        "rust output decompresses to wrong size"
    );
    assert_eq!(rt, data, "rust output decompresses to different bytes");
}
