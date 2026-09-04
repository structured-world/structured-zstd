//! Per-size-class allocation census of ONE WARM frame.
//!
//! The point is the shape, not the total. A buffer that is cleared but never
//! reserved climbs the same doubling ladder on every frame, and that leaves a
//! signature: many allocations spread across neighbouring size classes whose
//! average size sits just under each class bound. A buffer reserved once shows
//! up as a single allocation in one class, or not at all on a warm frame.
//!
//! Only the second frame is recorded, so lazy statics, the allocator's own
//! warm-up and everything a first frame builds for keeps are excluded; what
//! remains is what a steady-state frame costs.
//!
//! Run: `cargo run --release -p structured-zstd --example alloc_census_warm
//!        -- <level> [corpus path]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use structured_zstd::encoding::{CompressionLevel, FrameCompressor};

/// One bucket per `usize` bit width: a request of `n` bytes lands in
/// `n.leading_zeros()`, so each bucket holds one power-of-two size class.
const BUCKETS: usize = usize::BITS as usize + 1;

static RECORDING: AtomicBool = AtomicBool::new(false);
static COUNTS: [AtomicUsize; BUCKETS] = [const { AtomicUsize::new(0) }; BUCKETS];
static BYTES: [AtomicUsize; BUCKETS] = [const { AtomicUsize::new(0) }; BUCKETS];

struct Census;

// SAFETY: every method forwards to `System` unchanged; the counters are
// atomics touched before/after the forwarded call and allocate nothing
// themselves, so no re-entry into the allocator is possible.
unsafe impl GlobalAlloc for Census {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A realloc IS the regrowth this census is looking for, so it counts as
        // an allocation of the new size rather than being folded into the
        // original one.
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[inline]
fn record(size: usize) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    let bucket = size.leading_zeros() as usize;
    COUNTS[bucket].fetch_add(1, Ordering::Relaxed);
    BYTES[bucket].fetch_add(size, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Census = Census;

fn main() {
    let args: Vec<String> = env::args().collect();
    let level: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    let corpus = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("zstd/decodecorpus_files/z000033");
    let data = std::fs::read(corpus)
        .unwrap_or_else(|e| panic!("alloc_census_warm: cannot read {corpus}: {e}"));

    // `fresh` audits a compressor built inside the recorded region, which is
    // what a one-shot encode costs; the default reuses one, which is what a
    // steady-state frame costs. The two shapes allocate very differently and
    // the same change can help one and hurt the other, so both are askable.
    let fresh = args.get(3).map(|s| s == "fresh").unwrap_or(false);

    let mut out = Vec::new();
    let mut warm: Option<FrameCompressor<&[u8], &mut Vec<u8>>> = None;
    if !fresh {
        // The warm-up frame builds whatever the compressor keeps for the life
        // of the context, which is exactly what a warm census must not count.
        let mut compressor: FrameCompressor<&[u8], &mut Vec<u8>> =
            FrameCompressor::new(CompressionLevel::Level(level));
        compressor.set_source(&data[..]);
        compressor.set_drain(&mut out);
        compressor.compress();
        warm = Some(compressor);
    }

    let mut out2 = Vec::new();
    RECORDING.store(true, Ordering::SeqCst);
    match warm.as_mut() {
        Some(compressor) => {
            compressor.set_source(&data[..]);
            compressor.set_drain(&mut out2);
            compressor.compress();
        }
        None => {
            let mut compressor: FrameCompressor<&[u8], &mut Vec<u8>> =
                FrameCompressor::new(CompressionLevel::Level(level));
            compressor.set_source(&data[..]);
            compressor.set_drain(&mut out2);
            compressor.compress();
        }
    }
    RECORDING.store(false, Ordering::SeqCst);

    let mut total_count = 0usize;
    let mut total_bytes = 0usize;
    println!("level {level}, {} bytes in, warm frame:", data.len());
    println!(
        "{:>12}  {:>7}  {:>12}  {:>9}",
        "size class", "allocs", "bytes", "avg"
    );
    // Buckets are indexed by leading zeros, so the largest sizes come first.
    for bucket in 0..BUCKETS {
        let count = COUNTS[bucket].load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        let bytes = BYTES[bucket].load(Ordering::Relaxed);
        total_count += count;
        total_bytes += bytes;
        let class_floor = if bucket == BUCKETS - 1 {
            0
        } else {
            1usize << (usize::BITS as usize - 1 - bucket)
        };
        println!(
            "{:>12}  {:>7}  {:>12}  {:>9}",
            format!(">= {class_floor}"),
            count,
            bytes,
            bytes / count,
        );
    }
    println!("{:>12}  {total_count:>7}  {total_bytes:>12}", "TOTAL");
    println!("compressed to {} bytes", out2.len());
}
