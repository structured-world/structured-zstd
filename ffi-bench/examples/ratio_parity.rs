//! Ground-truth ratio comparison: OUR real compressed frame vs the C reference's
//! real compressed frame (zstd crate), byte sizes per level. Used to confirm the
//! negative-level over-compression (ours < C bytes) that ZSTD_generateSequences
//! does NOT reflect, and to drive ratio parity with C.

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

fn main() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../zstd/decodecorpus_files")
        .join(std::env::args().nth(1).unwrap_or_else(|| "z000002".into()));
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    println!("fixture {path:?} ({} bytes)", data.len());
    for lvl in [-7i32, -5, -3, -1, 1, 3] {
        let ours = compress_slice_to_vec(&data, CompressionLevel::Level(lvl));
        let c = zstd::bulk::compress(&data, lvl).expect("c compress");
        // Sanity: ours MUST round-trip through the C decoder (drop-in contract).
        // Fail loud on the exact regression this example exists to catch instead
        // of printing `false` and exiting 0.
        let c_decoded =
            zstd::bulk::decompress(&ours, data.len().max(1)).expect("C decoder rejected ours");
        assert_eq!(c_decoded, data, "C decoder mismatch for L{lvl}");
        println!(
            "L{lvl:<3} ours={:>5}B  C={:>5}B  ours/C={:.3}  (C decodes ours: ok)",
            ours.len(),
            c.len(),
            ours.len() as f64 / c.len() as f64,
        );
        // Byte-level diff: first differing index + a hex window around it from
        // each frame, so a 1-byte header divergence is pinpointed exactly.
        if ours != c {
            let first = (0..ours.len().min(c.len()))
                .find(|&i| ours[i] != c[i])
                .unwrap_or(ours.len().min(c.len()));
            let lo = first.saturating_sub(4);
            let hi_o = (first + 6).min(ours.len());
            let hi_c = (first + 6).min(c.len());
            let hx = |s: &[u8]| {
                s.iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            println!(
                "   DIFF L{lvl}: first differ at byte {first} (ours {}B vs C {}B)\n     ours[{lo}..]: {}\n     C   [{lo}..]: {}",
                ours.len(),
                c.len(),
                hx(&ours[lo..hi_o]),
                hx(&c[lo..hi_c]),
            );
        }
    }
}
