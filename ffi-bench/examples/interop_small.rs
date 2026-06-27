//! Cross-implementation interop check for small frames: our encoder now sizes
//! the window the C-faithful way (`get_cparams` clamps it to the source, e.g.
//! window_log 10 for a 1 KiB input), dropping the old MIN_HINTED_WINDOW_LOG
//! 16 KiB floor. This verifies a frame WE produce with such a small window
//! still decodes in the C reference decoder (the interop the old floor guarded).

use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};

fn load(name: &str) -> Option<Vec<u8>> {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../zstd/decodecorpus_files")
        .join(name);
    std::fs::read(p).ok()
}

fn check(name: &str, data: &[u8]) {
    for lvl in [-5i32, -1, 1, 2, 3, 4] {
        let ours = compress_slice_to_vec(data, CompressionLevel::Level(lvl));
        match zstd::bulk::decompress(&ours, data.len().max(1)) {
            Ok(dec) if dec == data => {
                println!(
                    "OK   {name:<16} L{lvl:<3} {} -> {} B, C-decoded",
                    data.len(),
                    ours.len()
                );
            }
            Ok(dec) => panic!(
                "{name} L{lvl}: C decoded {} bytes but != input ({} bytes)",
                dec.len(),
                data.len()
            ),
            Err(e) => panic!("{name} L{lvl}: C FAILED to decode our frame: {e}"),
        }
    }
}

fn main() {
    // Synthetic small inputs that drive the window below the old 16 KiB floor.
    let pat512: Vec<u8> = (0..512).map(|i| (i % 37) as u8).collect();
    let pat1k: Vec<u8> = (0..1024).map(|i| ((i * 7) % 251) as u8).collect();
    let pat4k: Vec<u8> = (0..4096).map(|i| ((i * 3 + i / 11) % 97) as u8).collect();
    check("synthetic-512", &pat512);
    check("synthetic-1k", &pat1k);
    check("synthetic-4k", &pat4k);
    if let Some(d) = load("z000001") {
        check("z000001", &d);
    }
    if let Some(d) = load("z000002") {
        check("z000002", &d);
    }
    println!("\nALL small-window frames round-tripped through the C decoder.");
}
