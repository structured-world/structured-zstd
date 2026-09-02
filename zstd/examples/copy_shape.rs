//! Diagnostic: capture the decode-path copy-call shape (counts + size
//! buckets + overshoot byte totals) for a chosen level on the
//! low-entropy corpus. The distribution is deterministic from the
//! compressed input, so this runs identically on any CPU tier — use it
//! to reason about call shape without an idle bench host.
//!
//! Build/run (feature gates the atomic counters):
//!   cargo run --release -p structured-zstd \
//!     --features "copy-shape-stats dict-builder" \
//!     --example copy_shape -- 18
//!
//! Arg 1 = compression level (default 18). Arg 2 = iters (default 1).

use std::env;

use structured_zstd::WILDCOPY_OVERLENGTH;
use structured_zstd::decoding::FrameDecoder;
use structured_zstd::decoding::shape_stats;
use zstd::zstd_safe::zstd_sys;

/// Replica of the `low_entropy_bytes` bench corpus: runs of 8..=31
/// identical bytes, the byte value advancing by 37 (mod 256) per run,
/// up to 1 MiB.
fn low_entropy_bytes() -> Vec<u8> {
    let n = 1_048_576usize;
    let mut out = Vec::with_capacity(n + 32);
    let mut val: u8 = 0;
    while out.len() < n {
        let run = 8 + (val as usize % 24); // 8..=31
        for _ in 0..run {
            out.push(val);
        }
        val = val.wrapping_add(37);
    }
    out.truncate(n);
    out
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(18);
    let iters: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);

    let src = low_entropy_bytes();
    let n = src.len();

    let dst_cap = unsafe { zstd_sys::ZSTD_compressBound(src.len()) };
    let mut compressed = vec![0u8; dst_cap];
    let written = unsafe {
        zstd_sys::ZSTD_compress(
            compressed.as_mut_ptr().cast::<core::ffi::c_void>(),
            dst_cap,
            src.as_ptr().cast::<core::ffi::c_void>(),
            src.len(),
            level,
        )
    };
    assert_eq!(
        unsafe { zstd_sys::ZSTD_isError(written) },
        0,
        "encode failed"
    );
    compressed.truncate(written);
    eprintln!(
        "level {level}: {n} bytes -> {written} bytes (ratio {:.3}x)",
        n as f64 / written as f64
    );

    let mut target = vec![0u8; n + WILDCOPY_OVERLENGTH];
    let mut decoder = FrameDecoder::new();

    // Warm decode once, then reset counters so the reported shape is for a
    // single clean decode (multiply by iters if >1).
    let _ = decoder
        .decode_all(compressed.as_slice(), &mut target)
        .expect("decode_all");
    let _ = shape_stats::take();
    let _ = shape_stats::take_repeat();

    for _ in 0..iters {
        let got = decoder
            .decode_all(compressed.as_slice(), &mut target)
            .expect("decode_all");
        assert_eq!(got, n, "decoded size mismatch");
    }

    let repeat = shape_stats::take_repeat();
    let [le8, b9_16, b17_32, gt32, req_gt32, written_gt32, max_len] = shape_stats::take();
    let total_calls = le8 + b9_16 + b17_32 + gt32;
    eprintln!("--- copy_bytes_overshooting call shape ({iters} iter(s)) ---");
    eprintln!("  total calls      : {total_calls}");
    eprintln!("  <=8 bytes        : {le8} ({:.1}%)", pct(le8, total_calls));
    eprintln!(
        "  9..=16 bytes     : {b9_16} ({:.1}%)",
        pct(b9_16, total_calls)
    );
    eprintln!(
        "  17..=32 bytes    : {b17_32} ({:.1}%)",
        pct(b17_32, total_calls)
    );
    eprintln!(
        "  >32 bytes        : {gt32} ({:.1}%)  <- the copy_avx2 chunk path",
        pct(gt32, total_calls)
    );
    if gt32 > 0 {
        eprintln!(
            "  >32 avg req len  : {:.1} bytes",
            req_gt32 as f64 / gt32 as f64
        );
        eprintln!(
            "  >32 overshoot    : {} req -> {} written (+{:.2}% waste)",
            req_gt32,
            written_gt32,
            100.0 * (written_gt32 as f64 - req_gt32 as f64) / req_gt32 as f64
        );
    }
    eprintln!("  max single copy  : {max_len} bytes");
    eprintln!(
        "  decoded bytes/it : {n}  (>32 written/it covers {:.1}% of output)",
        100.0 * (written_gt32 as f64 / iters as f64) / n as f64
    );

    let labels = [
        "non-overlap     ",
        "ovl offset <8   ",
        "ovl offset 8-15 ",
        "ovl offset 16-31",
        "ovl offset 32-63",
        "ovl offset >=64 ",
    ];
    let total_match_bytes: u64 = repeat.iter().map(|(_, b)| b).sum();
    eprintln!("--- match-repeat shape by offset bucket ---");
    for (lab, (cnt, bytes)) in labels.iter().zip(repeat.iter()) {
        eprintln!(
            "  {lab} : {cnt:>8} calls  {bytes:>12} bytes ({:.1}% of match bytes)",
            pct(*bytes, total_match_bytes)
        );
    }
    eprintln!("  total match bytes: {total_match_bytes}");
    let chunked_ovl: u64 = repeat[3].1 + repeat[4].1 + repeat[5].1;
    eprintln!(
        "  offset>=16 overlapping (chunked-by-offset; C single-passes): {chunked_ovl} bytes ({:.1}% of output)",
        pct(chunked_ovl, n as u64)
    );
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}
