//! Comparison benchmark matrix: structured-zstd (pure Rust) vs zstd (C FFI).
//!
//! The suite covers:
//! - small payloads (1-10 KiB)
//! - high entropy and low entropy payloads
//! - a large 100 MiB structured stream
//! - the repository decode corpus fixture
//! - optional Silesia corpus files via `STRUCTURED_ZSTD_SILESIA_DIR`
//!
//! Each run prints `REPORT ...` metadata lines that CI scripts can turn into a markdown report.

mod support;

use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Hand-rolled tracking allocator wrapping `System` so the bench can
/// emit a per-call peak-memory metric alongside throughput / ratio.
/// Hand-rolled (not a dep) because the wrapper is ~30 lines and stays
/// dev-only; pulling `peak_alloc` would add a transitive dep on
/// `parking_lot` for what amounts to two atomics.
///
/// Usage:
///   1. `PeakAllocTracker::reset()` — snapshot current usage as the
///      peak baseline.
///   2. Run the work to measure.
///   3. `PeakAllocTracker::peak_since_reset()` — high-water mark of
///      live bytes above the snapshot baseline.
///
/// Atomics use `Relaxed` ordering: the peak/current pair is observed
/// from the same thread that runs the work, so cross-thread ordering
/// guarantees aren't needed. The bench harness runs each measurement
/// on the calling thread.
struct TrackingAllocator;
static ALLOC_CURRENT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BASELINE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded to the system allocator unchanged; the
        // tracking just observes the size on success.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let prev = ALLOC_CURRENT.fetch_add(layout.size(), Ordering::Relaxed);
            // `fetch_max` on the new live size keeps the high-water
            // mark current without an extra load+compare.
            ALLOC_PEAK.fetch_max(prev + layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: caller guarantees the pointer/layout pair came from
        // a matching `alloc` call.
        unsafe { System.dealloc(ptr, layout) };
        ALLOC_CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded to the system allocator unchanged; the
        // tracking just observes the size on success.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            let prev = ALLOC_CURRENT.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOC_PEAK.fetch_max(prev + layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the realloc contract is honoured by `System.realloc`;
        // tracking adjusts the live count by the size delta when the
        // new allocation succeeds.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            let old = layout.size();
            if new_size >= old {
                let diff = new_size - old;
                let prev = ALLOC_CURRENT.fetch_add(diff, Ordering::Relaxed);
                ALLOC_PEAK.fetch_max(prev + diff, Ordering::Relaxed);
            } else {
                ALLOC_CURRENT.fetch_sub(old - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

struct PeakAllocTracker;
impl PeakAllocTracker {
    /// Snapshot the current live-bytes count as the baseline against
    /// which the next [`Self::peak_since_reset`] measurement is taken.
    /// Also forces the peak counter to the same baseline so a peak
    /// observed BEFORE this call doesn't leak into the next sample.
    fn reset() {
        let current = ALLOC_CURRENT.load(Ordering::Relaxed);
        ALLOC_BASELINE.store(current, Ordering::Relaxed);
        ALLOC_PEAK.store(current, Ordering::Relaxed);
    }

    /// High-water mark of live bytes above the baseline set by
    /// [`Self::reset`]. Returns `0` when the workload only freed
    /// memory below the baseline (impossible for a positive-net
    /// compress/decompress call but guards against signed underflow).
    fn peak_since_reset() -> usize {
        let peak = ALLOC_PEAK.load(Ordering::Relaxed);
        let baseline = ALLOC_BASELINE.load(Ordering::Relaxed);
        peak.saturating_sub(baseline)
    }
}

fn measure_peak_alloc<R>(f: impl FnOnce() -> R) -> (R, usize) {
    PeakAllocTracker::reset();
    let result = f();
    let peak = PeakAllocTracker::peak_since_reset();
    (result, peak)
}
use structured_zstd::decoding::FrameDecoder;
use structured_zstd::dictionary::{
    FastCoverOptions, FinalizeOptions, finalize_raw_dict, train_fastcover_raw_from_slice,
};
use support::{
    LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, supported_levels_filtered,
};

static BENCHMARK_SCENARIOS: OnceLock<Vec<Scenario>> = OnceLock::new();

/// Custom-allocator shim for libzstd. Without this libzstd's
/// `ZSTD_CCtx` / hash table / chain table / workspace allocations
/// go straight to libc `malloc` and are invisible to the Rust
/// `#[global_allocator]` wrapper. CR review of PR #143 flagged
/// this as the root cause of the misleadingly small
/// `ffi_peak_alloc_bytes` numbers in the first baseline pass —
/// the FFI side was reporting only the Rust-owned output `Vec`
/// without the actual C heap. Wiring `ZSTD_customMem` so its
/// alloc/free hooks route through `alloc::alloc::alloc` /
/// `alloc::alloc::dealloc` makes `TrackingAllocator` observe
/// every libzstd allocation, restoring apples-to-apples
/// comparison with `rust_peak_alloc_bytes`.
///
/// Layout: each block is over-allocated by `HEADER_BYTES` so the
/// allocation size can be recovered at free time (libzstd's
/// `customFree(opaque, addr)` does not receive a size). The
/// header sits at `ptr` and `ptr + HEADER_BYTES` is returned to
/// libzstd. `align = 16` covers SSE/NEON requirements; libzstd
/// internally over-aligns via `ZSTD_alignedAlloc` when it needs
/// tighter alignment, so the customMem allocator only has to
/// guarantee 16.
mod ffi_tracking_alloc {
    use core::ffi::c_void;
    use core::ptr;
    use std::alloc::Layout;

    const HEADER_BYTES: usize = 16;
    const ALIGN: usize = 16;

    /// SAFETY: libzstd contract — `size` is the request, return
    /// value is either `null_mut` or a freshly allocated block of
    /// at least `size` bytes whose pointer is `ALIGN`-aligned.
    pub(super) unsafe extern "C" fn alloc(_opaque: *mut c_void, size: usize) -> *mut c_void {
        let total = match size.checked_add(HEADER_BYTES) {
            Some(t) => t,
            None => return ptr::null_mut(),
        };
        let layout = match Layout::from_size_align(total, ALIGN) {
            Ok(l) => l,
            Err(_) => return ptr::null_mut(),
        };
        // SAFETY: `Layout` validated above; `alloc::alloc::alloc`
        // routes through `#[global_allocator] = TrackingAllocator`,
        // which forwards to `System` and updates the peak counters.
        let raw = unsafe { std::alloc::alloc(layout) };
        if raw.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: the first `HEADER_BYTES` of `raw` are owned by
        // this allocator and large enough to hold a `usize`.
        unsafe { ptr::write(raw as *mut usize, size) };
        unsafe { raw.add(HEADER_BYTES) as *mut c_void }
    }

    /// SAFETY: libzstd contract — `address` is either `null` or
    /// a pointer previously returned by `alloc` above; the size
    /// header at `address - HEADER_BYTES` was written by `alloc`.
    pub(super) unsafe extern "C" fn free(_opaque: *mut c_void, address: *mut c_void) {
        if address.is_null() {
            return;
        }
        // SAFETY: pointer arithmetic is valid because `address`
        // came from `alloc` above, which placed the size header
        // exactly `HEADER_BYTES` before the returned pointer.
        let header_ptr = unsafe { (address as *mut u8).sub(HEADER_BYTES) };
        let size = unsafe { ptr::read(header_ptr as *const usize) };
        let total = size + HEADER_BYTES;
        let layout = Layout::from_size_align(total, ALIGN).expect("layout must round-trip");
        // SAFETY: the layout matches the one passed to `alloc`,
        // so `dealloc` updates the tracker symmetrically and
        // releases the underlying System allocation.
        unsafe { std::alloc::dealloc(header_ptr, layout) };
    }
}

fn ffi_custom_mem() -> zstd::zstd_safe::zstd_sys::ZSTD_customMem {
    zstd::zstd_safe::zstd_sys::ZSTD_customMem {
        customAlloc: Some(ffi_tracking_alloc::alloc),
        customFree: Some(ffi_tracking_alloc::free),
        opaque: core::ptr::null_mut(),
    }
}

fn ffi_encode_all_aligned(input: &[u8], level: i32) -> Vec<u8> {
    // Path through raw `zstd_sys` with `ZSTD_createCCtx_advanced`
    // so the CCtx + every internal libzstd allocation (hash table,
    // chain table, working buffers, ...) lands in the
    // `TrackingAllocator` accounting via the customMem hooks.
    // `zstd::stream::Encoder` uses `ZSTD_createCCtx()` (default
    // libc malloc), so we cannot reuse it for the
    // memory-instrumented FFI path.
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: customMem hooks are valid for the lifetime of the
    // resulting CCtx; we always `ZSTD_freeCCtx` it before the
    // current scope ends so the hooks outlive every libzstd call.
    let cctx = unsafe { zstd_sys::ZSTD_createCCtx_advanced(ffi_custom_mem()) };
    assert!(!cctx.is_null(), "ZSTD_createCCtx_advanced returned null");

    unsafe {
        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
            level,
        );
        assert!(
            zstd_sys::ZSTD_isError(rc) == 0,
            "set compressionLevel failed"
        );

        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_checksumFlag,
            if cfg!(feature = "hash") { 1 } else { 0 },
        );
        assert!(zstd_sys::ZSTD_isError(rc) == 0, "set checksumFlag failed");

        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_contentSizeFlag,
            1,
        );
        assert!(
            zstd_sys::ZSTD_isError(rc) == 0,
            "set contentSizeFlag failed"
        );

        // Match the previous comparable-framing tweak for tiny
        // sources so a level-3 / small-payload comparison still
        // sits on the same window size as before this refactor.
        if input.len() <= (1 << 14) {
            let rc = zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_windowLog,
                14,
            );
            assert!(zstd_sys::ZSTD_isError(rc) == 0, "set windowLog failed");
        }

        let rc = zstd_sys::ZSTD_CCtx_setPledgedSrcSize(cctx, input.len() as u64);
        assert!(zstd_sys::ZSTD_isError(rc) == 0, "setPledgedSrcSize failed");

        let cap = zstd_sys::ZSTD_compressBound(input.len());
        let mut output = vec![0u8; cap];
        let written = zstd_sys::ZSTD_compress2(
            cctx,
            output.as_mut_ptr() as *mut core::ffi::c_void,
            output.len(),
            input.as_ptr() as *const core::ffi::c_void,
            input.len(),
        );
        assert!(
            zstd_sys::ZSTD_isError(written) == 0,
            "ZSTD_compress2 failed (code = {written})"
        );
        output.truncate(written);

        zstd_sys::ZSTD_freeCCtx(cctx);
        output
    }
}

fn ffi_decompress_via_custom_mem(compressed: &[u8], expected_len: usize) -> Vec<u8> {
    // Mirror of `ffi_encode_all_aligned`'s rationale for the
    // decode side: routes the `ZSTD_DCtx` + its internal buffers
    // through the customMem hooks so `TrackingAllocator` sees
    // every libzstd allocation on the FFI decode path.
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: customMem hooks remain valid for the lifetime of
    // the DCtx, which is freed before returning.
    let dctx = unsafe { zstd_sys::ZSTD_createDCtx_advanced(ffi_custom_mem()) };
    assert!(!dctx.is_null(), "ZSTD_createDCtx_advanced returned null");
    unsafe {
        let mut output = vec![0u8; expected_len];
        let written = zstd_sys::ZSTD_decompressDCtx(
            dctx,
            output.as_mut_ptr() as *mut core::ffi::c_void,
            output.len(),
            compressed.as_ptr() as *const core::ffi::c_void,
            compressed.len(),
        );
        assert!(
            zstd_sys::ZSTD_isError(written) == 0,
            "ZSTD_decompressDCtx failed (code = {written})"
        );
        output.truncate(written);
        zstd_sys::ZSTD_freeDCtx(dctx);
        output
    }
}

fn benchmark_scenarios_cached() -> &'static [Scenario] {
    BENCHMARK_SCENARIOS.get_or_init(benchmark_scenarios)
}

fn emit_reports_enabled() -> bool {
    std::env::var("STRUCTURED_ZSTD_EMIT_REPORT")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}

fn bench_compress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels_filtered() {
            if emit_reports {
                let (rust_compressed, rust_peak_bytes) = measure_peak_alloc(|| {
                    structured_zstd::encoding::compress_to_vec(
                        &scenario.bytes[..],
                        level.rust_level,
                    )
                });
                let (ffi_compressed, ffi_peak_bytes) = measure_peak_alloc(|| {
                    ffi_encode_all_aligned(&scenario.bytes[..], level.ffi_level)
                });
                emit_report_line(scenario, level, &rust_compressed, &ffi_compressed);
                emit_frame_header_report(scenario, level, "rust", &rust_compressed);
                emit_frame_header_report(scenario, level, "ffi", &ffi_compressed);
                emit_memory_report(scenario, level, "compress", rust_peak_bytes, ffi_peak_bytes);
            }

            let benchmark_name = format!("compress/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            group.bench_function("pure_rust", |b| {
                b.iter(|| {
                    black_box(structured_zstd::encoding::compress_to_vec(
                        &scenario.bytes[..],
                        level.rust_level,
                    ))
                })
            });

            group.bench_function("c_ffi", |b| {
                b.iter(|| black_box(ffi_encode_all_aligned(&scenario.bytes[..], level.ffi_level)))
            });

            group.finish();
        }
    }
}

fn bench_decompress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels_filtered() {
            let rust_compressed =
                structured_zstd::encoding::compress_to_vec(&scenario.bytes[..], level.rust_level);
            let ffi_compressed = ffi_encode_all_aligned(&scenario.bytes[..], level.ffi_level);
            let expected_len = scenario.len();
            bench_decompress_source(
                c,
                scenario,
                level,
                "rust_stream",
                &rust_compressed,
                expected_len,
                emit_reports,
            );
            bench_decompress_source(
                c,
                scenario,
                level,
                "c_stream",
                &ffi_compressed,
                expected_len,
                emit_reports,
            );
        }
    }
}

fn bench_decompress_source(
    c: &mut Criterion,
    scenario: &Scenario,
    level: LevelConfig,
    source: &'static str,
    compressed: &[u8],
    expected_len: usize,
    emit_reports: bool,
) {
    assert_decompress_matches_reference(scenario, compressed, expected_len);

    if emit_reports {
        // Measure peak live bytes for one full decode pass on each
        // path. Done OUTSIDE the criterion bench loop so the sample
        // is dominated by decoder internals (FrameDecoder state for
        // rust, ZSTD_DCtx + window for ffi) rather than criterion's
        // own per-iteration bookkeeping.
        let (_, rust_peak_bytes) = measure_peak_alloc(|| {
            let mut target = vec![0u8; expected_len];
            let mut decoder = FrameDecoder::new();
            let written = decoder.decode_all(compressed, &mut target).unwrap();
            assert_eq!(written, expected_len);
            target
        });
        let (_, ffi_peak_bytes) = measure_peak_alloc(|| {
            // `zstd::bulk::Decompressor::new` uses `ZSTD_createDCtx()`
            // (default malloc, invisible to TrackingAllocator). Use
            // the customMem-aware path so the C heap is part of the
            // peak. Same rationale as `ffi_encode_all_aligned`.
            ffi_decompress_via_custom_mem(compressed, expected_len)
        });
        emit_memory_report(
            scenario,
            level,
            &format!("decompress-{source}"),
            rust_peak_bytes,
            ffi_peak_bytes,
        );
    }

    let benchmark_name = format!(
        "decompress/{}/{}/{}/matrix",
        level.name, scenario.id, source
    );
    let mut group = c.benchmark_group(benchmark_name);
    configure_group(&mut group, scenario);
    group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

    group.bench_function("pure_rust", |b| {
        let mut target = vec![0u8; expected_len];
        let mut decoder = FrameDecoder::new();
        b.iter(|| {
            let written = decoder
                .decode_all(black_box(compressed), &mut target)
                .unwrap();
            black_box(&target[..written]);
            assert_eq!(written, expected_len);
        })
    });

    group.bench_function("c_ffi", |b| {
        let mut decoder = zstd::bulk::Decompressor::new().unwrap();
        let mut output = Vec::with_capacity(expected_len);
        b.iter(|| {
            output.clear();
            let written = decoder
                .decompress_to_buffer(black_box(compressed), &mut output)
                .unwrap();
            black_box(output.as_slice());
            assert_eq!(written, expected_len);
            assert_eq!(output.len(), expected_len);
        })
    });

    group.finish();
}

fn assert_decompress_matches_reference(
    scenario: &Scenario,
    compressed: &[u8],
    expected_len: usize,
) {
    let mut rust_target = vec![0u8; expected_len];
    let mut rust_decoder = FrameDecoder::new();
    let rust_written = rust_decoder
        .decode_all(compressed, &mut rust_target)
        .unwrap();
    assert_eq!(rust_written, expected_len);
    assert_eq!(&rust_target[..rust_written], scenario.bytes.as_slice());

    let mut ffi_decoder = zstd::bulk::Decompressor::new().unwrap();
    let mut ffi_output = Vec::with_capacity(expected_len);
    let ffi_written = ffi_decoder
        .decompress_to_buffer(compressed, &mut ffi_output)
        .unwrap();
    assert_eq!(ffi_written, expected_len);
    assert_eq!(ffi_output.as_slice(), scenario.bytes.as_slice());
}

fn bench_dictionary(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    for scenario in benchmark_scenarios_cached().iter() {
        if !matches!(scenario.class, ScenarioClass::Small | ScenarioClass::Corpus) {
            continue;
        }

        let sample_count = training_sample_count(&scenario.bytes);
        let total_training_bytes = scenario.bytes.len();
        let ffi_samples = [scenario.bytes.as_slice()];
        let max_dict_size = total_training_bytes.saturating_sub(64);
        let dict_size = dictionary_size_for(scenario.len())
            .max(256)
            .min(max_dict_size);
        let Ok(rust_content_budget) =
            finalized_training_content_budget(scenario.bytes.as_slice(), dict_size)
        else {
            eprintln!(
                "BENCH_WARN skipping Rust FastCOVER dictionary benchmark for {} (samples={}, total_training_bytes={}, dict_size={}) due to finalized content budget error",
                scenario.id, sample_count, total_training_bytes, dict_size
            );
            continue;
        };
        let fastcover_options = fastcover_fixed_options();

        let rust_train_started = Instant::now();
        let Ok((rust_raw_dictionary, rust_tuned)) = train_fastcover_raw_from_slice(
            scenario.bytes.as_slice(),
            rust_content_budget,
            &fastcover_options,
        ) else {
            eprintln!(
                "BENCH_WARN skipping Rust FastCOVER dictionary benchmark for {} (samples={}, total_training_bytes={}, dict_size={})",
                scenario.id, sample_count, total_training_bytes, dict_size
            );
            continue;
        };
        let Ok(rust_dictionary) = finalize_raw_dict(
            rust_raw_dictionary.as_slice(),
            scenario.bytes.as_slice(),
            dict_size,
            FinalizeOptions::default(),
        ) else {
            eprintln!(
                "BENCH_WARN skipping Rust FastCOVER finalization benchmark for {} (samples={}, total_training_bytes={}, dict_size={})",
                scenario.id, sample_count, total_training_bytes, dict_size
            );
            continue;
        };
        let rust_train_ms = rust_train_started.elapsed().as_secs_f64() * 1_000.0;

        let ffi_train_started = Instant::now();
        let Ok(ffi_dictionary) = zstd::dict::from_samples(&ffi_samples, dict_size) else {
            eprintln!(
                "BENCH_WARN skipping dictionary benchmark for {} (samples={}, total_training_bytes={}, dict_size={})",
                scenario.id,
                ffi_samples.len(),
                total_training_bytes,
                dict_size
            );
            continue;
        };
        let ffi_train_ms = ffi_train_started.elapsed().as_secs_f64() * 1_000.0;

        if emit_reports {
            emit_dictionary_training_report(
                scenario,
                DictTrainingMetrics {
                    training_bytes: total_training_bytes,
                    dict_bytes_requested: dict_size,
                    rust_train_ms,
                    ffi_train_ms,
                    rust_dict_bytes: rust_dictionary.len(),
                    ffi_dict_bytes: ffi_dictionary.len(),
                    rust_fastcover_score: rust_tuned.score,
                },
            );
        }

        let benchmark_name = format!("dict-train/na/{}/{}", scenario.id, "matrix");
        let mut group = c.benchmark_group(benchmark_name);
        configure_group(&mut group, scenario);
        group.throughput(Throughput::Bytes(total_training_bytes as u64));

        group.bench_function("pure_rust", |b| {
            b.iter(|| {
                let (raw_dict, tuned) = train_fastcover_raw_from_slice(
                    scenario.bytes.as_slice(),
                    rust_content_budget,
                    &fastcover_options,
                )
                .expect("fastcover training should succeed");
                let dict = finalize_raw_dict(
                    raw_dict.as_slice(),
                    scenario.bytes.as_slice(),
                    dict_size,
                    FinalizeOptions::default(),
                )
                .expect("fastcover dictionary finalization should succeed");
                black_box((dict.len(), tuned.score));
            })
        });

        group.bench_function("c_ffi", |b| {
            b.iter(|| {
                black_box(
                    zstd::dict::from_samples(&ffi_samples, dict_size)
                        .expect("ffi dictionary training should succeed")
                        .len(),
                )
            })
        });

        group.finish();

        for level in supported_levels_filtered() {
            let mut no_dict = zstd::bulk::Compressor::new(level.ffi_level).unwrap();
            let mut with_dict =
                zstd::bulk::Compressor::with_dictionary(level.ffi_level, &ffi_dictionary).unwrap();
            let no_dict_bytes = no_dict.compress(&scenario.bytes).unwrap();
            let with_dict_bytes = with_dict.compress(&scenario.bytes).unwrap();
            if emit_reports {
                emit_dictionary_report(
                    scenario,
                    level,
                    ffi_dictionary.len(),
                    ffi_train_ms,
                    &no_dict_bytes,
                    &with_dict_bytes,
                );
            }

            let benchmark_name =
                format!("compress-dict/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            group.bench_function("c_ffi_without_dict", |b| {
                let mut compressor = zstd::bulk::Compressor::new(level.ffi_level).unwrap();
                b.iter(|| black_box(compressor.compress(&scenario.bytes).unwrap()))
            });

            group.bench_function("c_ffi_with_dict", |b| {
                let mut compressor =
                    zstd::bulk::Compressor::with_dictionary(level.ffi_level, &ffi_dictionary)
                        .unwrap();
                b.iter(|| black_box(compressor.compress(&scenario.bytes).unwrap()))
            });

            group.finish();
        }
    }
}

fn configure_group<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
    scenario: &Scenario,
) {
    match scenario.class {
        ScenarioClass::Small => {
            group.sample_size(30);
            group.measurement_time(Duration::from_secs(3));
            group.sampling_mode(SamplingMode::Flat);
        }
        ScenarioClass::Corpus | ScenarioClass::Entropy => {
            group.sample_size(10);
            group.measurement_time(Duration::from_secs(4));
            group.sampling_mode(SamplingMode::Flat);
        }
        ScenarioClass::Large | ScenarioClass::Silesia => {
            group.sample_size(10);
            group.measurement_time(Duration::from_secs(2));
            group.warm_up_time(Duration::from_millis(500));
            group.sampling_mode(SamplingMode::Flat);
        }
    }
}

fn emit_frame_header_report(
    scenario: &Scenario,
    level: LevelConfig,
    encoder: &'static str,
    compressed: &[u8],
) {
    if compressed.len() < 5 {
        println!(
            "REPORT_HDR scenario={} level={} encoder={} parse=error",
            scenario.id, level.name, encoder
        );
        return;
    }

    let desc = compressed[4];
    let frame_content_size_flag = desc >> 6;
    let single_segment = ((desc >> 5) & 0x1) == 1;
    let checksum = ((desc >> 2) & 0x1) == 1;
    let dict_id_flag = desc & 0x3;
    let dict_id_bytes: u8 = match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        _ => unreachable!(),
    };
    let fcs_bytes: u8 = match frame_content_size_flag {
        0 => {
            if single_segment {
                1
            } else {
                0
            }
        }
        1 => 2,
        2 => 4,
        3 => 8,
        _ => unreachable!(),
    };
    let header_bytes =
        4u16 + 1 + if single_segment { 0 } else { 1 } + dict_id_bytes as u16 + fcs_bytes as u16;
    println!(
        "REPORT_HDR scenario={} level={} encoder={} header_bytes={} single_segment={} checksum={} fcs_bytes={} dict_id_bytes={}",
        scenario.id,
        level.name,
        encoder,
        header_bytes,
        single_segment,
        checksum,
        fcs_bytes,
        dict_id_bytes,
    );
}

fn emit_memory_report(
    scenario: &Scenario,
    level: LevelConfig,
    stage: &str,
    rust_peak_alloc_bytes: usize,
    ffi_peak_alloc_bytes: usize,
) {
    let escaped_label = escape_report_label(&scenario.label);
    // Field names changed from `*_buffer_bytes_estimate` (static
    // input+output approximation) to `*_peak_alloc_bytes` (real
    // high-water mark of live allocator bytes during one
    // compress/decompress pass). The aggregator regex is updated in
    // lockstep — see `.github/scripts/run-benchmarks.sh`.
    println!(
        "REPORT_MEM scenario={} label=\"{}\" level={} stage={} rust_peak_alloc_bytes={} ffi_peak_alloc_bytes={}",
        scenario.id, escaped_label, level.name, stage, rust_peak_alloc_bytes, ffi_peak_alloc_bytes
    );
}

fn emit_report_line(
    scenario: &Scenario,
    level: LevelConfig,
    rust_compressed: &[u8],
    ffi_compressed: &[u8],
) {
    let input_len = scenario.len() as f64;
    let escaped_label = escape_report_label(&scenario.label);
    let (rust_ratio, ffi_ratio) = if input_len > 0.0 {
        (
            rust_compressed.len() as f64 / input_len,
            ffi_compressed.len() as f64 / input_len,
        )
    } else {
        (0.0, 0.0)
    };
    println!(
        "REPORT scenario={} label=\"{}\" level={} input_bytes={} rust_bytes={} ffi_bytes={} rust_ratio={:.6} ffi_ratio={:.6}",
        scenario.id,
        escaped_label,
        level.name,
        scenario.len(),
        rust_compressed.len(),
        ffi_compressed.len(),
        rust_ratio,
        ffi_ratio
    );
}

fn emit_dictionary_report(
    scenario: &Scenario,
    level: LevelConfig,
    dict_bytes: usize,
    train_ms: f64,
    no_dict_bytes: &[u8],
    with_dict_bytes: &[u8],
) {
    let input_len = scenario.len() as f64;
    let escaped_label = escape_report_label(&scenario.label);
    let (no_dict_ratio, with_dict_ratio) = if input_len > 0.0 {
        (
            no_dict_bytes.len() as f64 / input_len,
            with_dict_bytes.len() as f64 / input_len,
        )
    } else {
        (0.0, 0.0)
    };
    println!(
        "REPORT_DICT scenario={} label=\"{}\" level={} dict_bytes={} train_ms={:.3} ffi_no_dict_bytes={} ffi_with_dict_bytes={} ffi_no_dict_ratio={:.6} ffi_with_dict_ratio={:.6}",
        scenario.id,
        escaped_label,
        level.name,
        dict_bytes,
        train_ms,
        no_dict_bytes.len(),
        with_dict_bytes.len(),
        no_dict_ratio,
        with_dict_ratio
    );
}

fn emit_dictionary_training_report(scenario: &Scenario, metrics: DictTrainingMetrics) {
    let escaped_label = escape_report_label(&scenario.label);
    println!(
        "REPORT_DICT_TRAIN scenario={} label=\"{}\" training_bytes={} dict_bytes_requested={} rust_train_ms={:.3} ffi_train_ms={:.3} rust_dict_bytes={} ffi_dict_bytes={} rust_fastcover_score={}",
        scenario.id,
        escaped_label,
        metrics.training_bytes,
        metrics.dict_bytes_requested,
        metrics.rust_train_ms,
        metrics.ffi_train_ms,
        metrics.rust_dict_bytes,
        metrics.ffi_dict_bytes,
        metrics.rust_fastcover_score
    );
}

struct DictTrainingMetrics {
    training_bytes: usize,
    dict_bytes_requested: usize,
    rust_train_ms: f64,
    ffi_train_ms: f64,
    rust_dict_bytes: usize,
    ffi_dict_bytes: usize,
    rust_fastcover_score: usize,
}

fn finalized_training_content_budget(sample: &[u8], dict_size: usize) -> std::io::Result<usize> {
    let probe = [0u8; 8];
    let finalized = finalize_raw_dict(
        probe.as_slice(),
        sample,
        dict_size,
        FinalizeOptions::default(),
    )?;
    let header_bytes = finalized.len().saturating_sub(probe.len());
    Ok(dict_size.saturating_sub(header_bytes))
}

fn training_sample_count(source: &[u8]) -> usize {
    let sample_size = source.len().div_ceil(16).clamp(256, 8192);
    let samples = source
        .chunks(sample_size)
        .take(64)
        .filter(|chunk| chunk.len() >= 64)
        .count();
    if samples < 2 {
        let midpoint = source.len() / 2;
        let left = &source[..midpoint];
        let right = &source[midpoint..];
        if left.len() >= 64 && right.len() >= 64 {
            2
        } else {
            eprintln!(
                "BENCH_WARN tiny dictionary training input ({} bytes), using a single sample fallback",
                source.len()
            );
            1
        }
    } else {
        samples
    }
}

fn dictionary_size_for(input_len: usize) -> usize {
    input_len.div_ceil(8).clamp(256, 16 * 1024)
}

fn fastcover_fixed_options() -> FastCoverOptions {
    FastCoverOptions {
        optimize: false,
        accel: 4,
        k: 256,
        d: 8,
        f: 20,
        ..FastCoverOptions::default()
    }
}

fn escape_report_label(label: &str) -> String {
    label.replace('\\', "\\\\").replace('\"', "\\\"")
}

criterion_group!(benches, bench_compress, bench_decompress, bench_dictionary);
criterion_main!(benches);
