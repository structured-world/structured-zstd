//! Decode-side allocation auditor with per-alloc trace. Atomic counters
//! plus a fixed-size append-only log of (size, peak_after) pairs.
//! Issue #279 allocator-pressure axis.
//!
//! Usage: alloc_audit_decode <compressed_blob> <decompressed_size>

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use structured_zstd::WILDCOPY_OVERLENGTH;
use structured_zstd::decoding::FrameDecoder;

struct AuditAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

const TRACE_CAP: usize = 256;
static TRACE_LEN: AtomicUsize = AtomicUsize::new(0);
static TRACE_SIZES: [AtomicUsize; TRACE_CAP] = {
    // Sad const-init: array of AtomicUsize with default value 0.
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; TRACE_CAP]
};
static TRACE_PEAK_AFTER: [AtomicUsize; TRACE_CAP] = {
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; TRACE_CAP]
};

unsafe impl GlobalAlloc for AuditAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let size = layout.size();
            let new_live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            let mut peak = PEAK_BYTES.load(Ordering::Relaxed);
            while new_live > peak {
                match PEAK_BYTES.compare_exchange_weak(
                    peak,
                    new_live,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => peak = actual,
                }
            }
            if TRACE_ENABLED.load(Ordering::Relaxed) {
                let idx = TRACE_LEN.fetch_add(1, Ordering::Relaxed);
                if idx < TRACE_CAP {
                    TRACE_SIZES[idx].store(size, Ordering::Relaxed);
                    TRACE_PEAK_AFTER[idx].store(new_live, Ordering::Relaxed);
                }
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: AuditAllocator = AuditAllocator;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(1)
        .expect("usage: alloc_audit_decode <blob> <size>");
    let expected: usize = args.get(2).expect("size").parse().expect("size");
    let target_len = expected.checked_add(WILDCOPY_OVERLENGTH).expect("overflow");

    let compressed = fs::read(path).expect("read");

    // Reset counters and enable trace AFTER input file read.
    LIVE_BYTES.store(0, Ordering::Relaxed);
    PEAK_BYTES.store(0, Ordering::Relaxed);
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    TRACE_LEN.store(0, Ordering::Relaxed);
    TRACE_ENABLED.store(true, Ordering::Relaxed);

    let mut target = vec![0u8; target_len];
    let mut decoder = FrameDecoder::new();
    let written = decoder
        .decode_all(compressed.as_slice(), &mut target)
        .expect("decode");
    assert_eq!(written, expected);

    TRACE_ENABLED.store(false, Ordering::Relaxed);

    let peak = PEAK_BYTES.load(Ordering::Relaxed);
    let live = LIVE_BYTES.load(Ordering::Relaxed);
    let count = ALLOC_COUNT.load(Ordering::Relaxed);
    let trace_len = TRACE_LEN.load(Ordering::Relaxed).min(TRACE_CAP);

    eprintln!("=== Allocation audit ===");
    eprintln!(
        "Peak live bytes: {} ({:.2} MB)",
        peak,
        peak as f64 / 1_048_576.0
    );
    eprintln!(
        "Live bytes after decode: {} ({:.2} MB)",
        live,
        live as f64 / 1_048_576.0
    );
    eprintln!("Total alloc calls: {}", count);
    eprintln!();
    eprintln!("Per-alloc trace (first {trace_len} of {count}):");
    eprintln!("  {:>4} {:>10} {:>14}", "idx", "size", "peak_after");
    for i in 0..trace_len {
        let size = TRACE_SIZES[i].load(Ordering::Relaxed);
        let peak_after = TRACE_PEAK_AFTER[i].load(Ordering::Relaxed);
        eprintln!("  {:>4} {:>10} {:>14}", i, size, peak_after);
    }
}
