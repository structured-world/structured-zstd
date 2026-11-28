//! Dictionary decode of one frame, ours beside libzstd, with the output placed
//! at every 64-byte offset of a 4 KiB page: whether a decode's time depends on
//! where its output sits relative to its inputs. Prints the frame's block and
//! literal layout first, then one line per offset.
//!
//! Run: `dict_decode_offsets <payload.zst> <dict> <expected_len> [iters]`

use std::env;
use std::time::Instant;

use structured_zstd::decoding::{DictionaryHandle, FrameDecoder};

/// Block and literal layout of a single-segment frame, as far as headers say.
fn describe(frame: &[u8]) {
    let fhd = frame[4];
    let single = fhd & 0x20 != 0;
    let dict_id_len = [0usize, 1, 2, 4][usize::from(fhd & 3)];
    let fcs_len = match fhd >> 6 {
        0 => usize::from(single),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut at = 5 + usize::from(!single) + dict_id_len + fcs_len;
    println!(
        "frame {} bytes, header {at} bytes, checksum {}",
        frame.len(),
        fhd & 4 != 0
    );
    loop {
        let h =
            u32::from(frame[at]) | u32::from(frame[at + 1]) << 8 | u32::from(frame[at + 2]) << 16;
        let last = h & 1 != 0;
        let kind = (h >> 1) & 3;
        let size = (h >> 3) as usize;
        let body = at + 3;
        match kind {
            0 => println!("  raw block {size} bytes at {body}"),
            1 => println!("  rle block {size}"),
            _ => {
                let b0 = frame[body];
                let lit_type = b0 & 3;
                let (regen, comp, lhs) = match (lit_type, (b0 >> 2) & 3) {
                    (0 | 1, 0 | 2) => (usize::from(b0 >> 3), 0, 1),
                    (0 | 1, 1) => (
                        (usize::from(b0) >> 4) | usize::from(frame[body + 1]) << 4,
                        0,
                        2,
                    ),
                    (0 | 1, _) => (
                        (usize::from(b0) >> 4)
                            | usize::from(frame[body + 1]) << 4
                            | usize::from(frame[body + 2]) << 12,
                        0,
                        3,
                    ),
                    _ => {
                        let v = u64::from_le_bytes({
                            let mut w = [0u8; 8];
                            w[..5].copy_from_slice(&frame[body..body + 5]);
                            w
                        });
                        match (b0 >> 2) & 3 {
                            0 | 1 => (((v >> 4) & 0x3FF) as usize, ((v >> 14) & 0x3FF) as usize, 3),
                            2 => (
                                ((v >> 4) & 0x3FFF) as usize,
                                ((v >> 18) & 0x3FFF) as usize,
                                4,
                            ),
                            _ => (
                                ((v >> 4) & 0x3FFFF) as usize,
                                ((v >> 22) & 0x3FFFF) as usize,
                                5,
                            ),
                        }
                    }
                };
                let lit_bytes = match lit_type {
                    0 => regen,
                    1 => 1,
                    _ => comp,
                };
                let seq_at = body + lhs + lit_bytes;
                let s0 = frame[seq_at];
                let nb_seq = match s0 {
                    0..=127 => usize::from(s0),
                    128..=254 => ((usize::from(s0) - 128) << 8) + usize::from(frame[seq_at + 1]),
                    _ => {
                        usize::from(frame[seq_at + 1])
                            + (usize::from(frame[seq_at + 2]) << 8)
                            + 0x7F00
                    }
                };
                println!(
                    "  compressed block {size} bytes at {body}: literals type {lit_type} regen {regen} comp {comp} (data at {}), {nb_seq} sequences",
                    body + lhs
                );
            }
        }
        at = body + if kind == 1 { 1 } else { size };
        if last {
            break;
        }
    }
}

fn time<F: FnMut()>(iters: u32, mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(t.elapsed().as_nanos() as f64 / f64::from(iters));
    }
    best
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let payload = std::fs::read(&args[1]).expect("read payload");
    let dict = std::fs::read(&args[2]).expect("read dict");
    let len: usize = args[3].parse().expect("expected_len");
    let iters: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    describe(&payload);

    let handle = DictionaryHandle::decode_dict(dict.as_slice()).expect("parse dictionary");
    let mut decoder = FrameDecoder::new();
    let mut ffi = zstd::bulk::Decompressor::with_dictionary(&dict).expect("ffi dictionary");
    // One page-aligned arena with room for every offset.
    let mut arena = vec![0u8; len + 2 * 4096];
    let base = arena.as_ptr() as usize;
    let page = (4096 - base % 4096) % 4096;
    println!(
        "payload at {:#x} (mod 4096 {:#x}), dict file at {:#x}",
        payload.as_ptr() as usize,
        payload.as_ptr() as usize % 4096,
        dict.as_ptr() as usize
    );
    println!("offset  ours_ns  ffi_ns  ffi/ours");
    for step in 0..64 {
        let off = page + step * 64;
        let out = &mut arena[off..off + len];
        let ours = time(iters, || {
            let n = decoder
                .decode_all_with_dict_handle(core::hint::black_box(&payload), out, &handle)
                .expect("ours decode");
            core::hint::black_box(n);
        });
        let out = &mut arena[off..off + len];
        let theirs = time(iters, || {
            let n = ffi
                .decompress_to_buffer(core::hint::black_box(&payload[..]), out)
                .expect("ffi decode");
            core::hint::black_box(n);
        });
        println!(
            "{:6}  {ours:7.1}  {theirs:6.1}  {:.3}",
            step * 64,
            theirs / ours
        );
    }
}
