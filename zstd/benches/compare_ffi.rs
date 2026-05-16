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
use std::hint::black_box;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// OS-level resident-set-size sampling. Used by the bench to observe
/// Rust-side peak memory during one compress/decompress call without
/// installing a global allocator wrapper.
///
/// Earlier revisions installed a `#[global_allocator] TrackingAllocator`
/// that intercepted every allocation in the bench binary. Even when its
/// counting was gated by an atomic flag, the per-allocation load+branch
/// (and the extra 16-byte header) biased criterion's timing loops
/// relative to the FFI side. RSS sampling moves the observation off the
/// hot path entirely: the OS updates resident-set size on
/// `brk`/`mmap`/page fault, and a background poller reads it.
///
/// This module is compiled only into the `compare_ffi` bench binary,
/// never into the published `structured-zstd` crate.
mod rss {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    /// Current resident-set size of this process in bytes. Returns 0
    /// on unsupported platforms or if the OS query fails (callers
    /// treat 0 as "no signal" and skip max-updates).
    pub fn current() -> usize {
        #[cfg(target_os = "macos")]
        {
            current_macos()
        }
        #[cfg(target_os = "linux")]
        {
            current_linux()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            0
        }
    }

    #[cfg(target_os = "macos")]
    fn current_macos() -> usize {
        // mach_task_basic_info — same struct top(1) reads for RSIZE.
        // `resident_size` is in bytes; flavor id 20 and layout are
        // stable kernel ABI (xnu mach/task_info.h).
        const MACH_TASK_BASIC_INFO: i32 = 20;
        #[repr(C)]
        struct MachTaskBasicInfo {
            virtual_size: u64,
            resident_size: u64,
            resident_size_max: u64,
            user_time: [u32; 2],
            system_time: [u32; 2],
            policy: i32,
            suspend_count: i32,
        }
        unsafe extern "C" {
            fn mach_task_self() -> u32;
            fn task_info(target: u32, flavor: i32, info: *mut u8, count: *mut u32) -> i32;
        }
        let mut info = MachTaskBasicInfo {
            virtual_size: 0,
            resident_size: 0,
            resident_size_max: 0,
            user_time: [0; 2],
            system_time: [0; 2],
            policy: 0,
            suspend_count: 0,
        };
        // count is in u32 units (struct size / 4).
        let mut count = (core::mem::size_of::<MachTaskBasicInfo>() / 4) as u32;
        // SAFETY: `mach_task_self` returns the current task port,
        // `task_info` reads `count * 4` bytes into the buffer we
        // pass; the struct size matches `count`.
        let rc = unsafe {
            task_info(
                mach_task_self(),
                MACH_TASK_BASIC_INFO,
                core::ptr::from_mut(&mut info) as *mut u8,
                &mut count,
            )
        };
        if rc == 0 {
            info.resident_size as usize
        } else {
            0
        }
    }

    #[cfg(target_os = "linux")]
    fn current_linux() -> usize {
        // /proc/self/statm: "size resident shared text lib data dt"
        // resident column is in pages.
        let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let resident_pages: usize = s
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        resident_pages.saturating_mul(page_size())
    }

    #[cfg(target_os = "linux")]
    fn page_size() -> usize {
        use std::sync::OnceLock;
        static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
        *PAGE_SIZE.get_or_init(|| {
            unsafe extern "C" {
                fn sysconf(name: i32) -> i64;
            }
            // _SC_PAGESIZE = 30 on Linux. `sysconf` returns -1 on
            // failure; fall back to 4 KiB (matches every Linux ABI
            // we care about, including i686-gnu and x86_64-musl).
            // SAFETY: sysconf is async-signal-safe and has no
            // pointer arguments; safe to call from any thread.
            let v = unsafe { sysconf(30) };
            if v > 0 { v as usize } else { 4096 }
        })
    }

    /// Background-sampled peak-RSS observation window. `start` snaps
    /// baseline and spawns a poller that updates a peak counter every
    /// `SAMPLE_INTERVAL`; `finish` joins the poller and returns
    /// `peak.saturating_sub(baseline)`.
    ///
    /// Sampling cadence is tight enough for sub-second encode/decode
    /// operations on >100 KiB inputs (hundreds of samples per call).
    /// For sub-100 µs operations the sampler may miss intra-call
    /// peaks — that's acceptable because RSS for such tiny payloads
    /// is dominated by static process state and the metric isn't
    /// meaningful at that scale.
    const SAMPLE_INTERVAL: Duration = Duration::from_micros(250);

    pub struct PeakWindow {
        peak: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
        baseline: usize,
    }

    impl PeakWindow {
        pub fn start() -> Self {
            let baseline = current();
            let peak = Arc::new(AtomicUsize::new(baseline));
            let stop = Arc::new(AtomicBool::new(false));
            let peak_w = peak.clone();
            let stop_w = stop.clone();
            let handle = thread::spawn(move || {
                while !stop_w.load(Ordering::Relaxed) {
                    let cur = current();
                    if cur > 0 {
                        peak_w.fetch_max(cur, Ordering::Relaxed);
                    }
                    thread::sleep(SAMPLE_INTERVAL);
                }
            });
            PeakWindow {
                peak,
                stop,
                handle: Some(handle),
                baseline,
            }
        }

        /// Stop the poller and return `peak - baseline`. Also takes
        /// one final on-thread sample so a peak that landed after
        /// the poller's last loop iteration but before `finish` is
        /// still observed.
        pub fn finish(mut self) -> usize {
            let final_rss = current();
            if final_rss > 0 {
                self.peak.fetch_max(final_rss, Ordering::Relaxed);
            }
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            let peak = self.peak.load(Ordering::Relaxed);
            peak.saturating_sub(self.baseline)
        }
    }
}

use structured_zstd::decoding::FrameDecoder;
use structured_zstd::dictionary::{
    FastCoverOptions, FinalizeOptions, finalize_raw_dict, train_fastcover_raw_from_slice,
};
use support::{
    LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, supported_levels_filtered,
};

static BENCHMARK_SCENARIOS: OnceLock<Vec<Scenario>> = OnceLock::new();

/// Per-CCtx FFI memory tracker. Passed as the `opaque` field of
/// `ZSTD_customMem` so libzstd's malloc/free callbacks update this
/// struct directly. No global state, no atomic ops — the tracker
/// lives on the calling stack and is touched only from libzstd's
/// single-threaded-per-context allocator path.
///
/// Used only when memory is being measured. Criterion's timing loops
/// call the FFI helpers with `mem = None`, which routes libzstd back
/// to its default `malloc`/`free`, so timing samples pay no
/// instrumentation overhead.
///
/// Bench-only: lives entirely in this file and is never linked into
/// the published `structured-zstd` crate.
#[derive(Default)]
struct FfiMemTracker {
    current: usize,
    peak: usize,
}

impl FfiMemTracker {
    /// Build a `ZSTD_customMem` whose `opaque` points at `self`.
    /// Caller must keep `self` alive for the lifetime of the
    /// CCtx/DCtx (libzstd calls `customFree` during `ZSTD_freeCCtx` /
    /// `ZSTD_freeDCtx`, which the bench invokes before dropping the
    /// tracker).
    fn custom_mem(&mut self) -> zstd::zstd_safe::zstd_sys::ZSTD_customMem {
        zstd::zstd_safe::zstd_sys::ZSTD_customMem {
            customAlloc: Some(ffi_alloc),
            customFree: Some(ffi_free),
            opaque: core::ptr::from_mut(self) as *mut core::ffi::c_void,
        }
    }
}

/// Header bytes prepended to every customMem allocation so the size
/// can be recovered at free time (libzstd's `customFree(opaque, addr)`
/// does not pass a size). 16 also covers SSE / NEON alignment for the
/// user-visible pointer.
const FFI_HEADER_BYTES: usize = 16;
const FFI_ALIGN: usize = 16;

/// `ZSTD_customMem` allocate callback. Reserves `size + HEADER` bytes,
/// stores `size` in the header, updates the per-CCtx tracker, and
/// returns the post-header pointer.
///
/// SAFETY: `opaque` must be the `*mut FfiMemTracker` pointer that
/// `custom_mem()` produced. libzstd is single-threaded per CCtx, so
/// the `&mut FfiMemTracker` access here is race-free as long as the
/// bench doesn't share a tracker across CCtxs concurrently (it
/// doesn't — each measurement constructs a fresh tracker on its own
/// thread).
unsafe extern "C" fn ffi_alloc(
    opaque: *mut core::ffi::c_void,
    size: usize,
) -> *mut core::ffi::c_void {
    use std::alloc::Layout;
    let Some(total) = size.checked_add(FFI_HEADER_BYTES) else {
        return core::ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, FFI_ALIGN) else {
        return core::ptr::null_mut();
    };
    // SAFETY: layout is valid (validated above).
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: first 8 bytes of `raw` are owned by us (HEADER_BYTES is 16).
    unsafe {
        core::ptr::write(raw as *mut usize, size);
    }
    // SAFETY: opaque came from `FfiMemTracker::custom_mem`; per-CCtx
    // single-threaded access.
    let tracker = unsafe { &mut *(opaque as *mut FfiMemTracker) };
    tracker.current = tracker.current.saturating_add(size);
    if tracker.current > tracker.peak {
        tracker.peak = tracker.current;
    }
    // SAFETY: raw + HEADER_BYTES is in-bounds (allocation size is
    // total = size + HEADER_BYTES) and HEADER_BYTES ≥ FFI_ALIGN
    // so the returned pointer is FFI_ALIGN-aligned.
    unsafe { raw.add(FFI_HEADER_BYTES) as *mut core::ffi::c_void }
}

/// `ZSTD_customMem` free callback. Recovers `size` from the header,
/// decrements `tracker.current`, and releases the underlying System
/// allocation.
unsafe extern "C" fn ffi_free(opaque: *mut core::ffi::c_void, address: *mut core::ffi::c_void) {
    use std::alloc::Layout;
    if address.is_null() {
        return;
    }
    // SAFETY: `address` came from `ffi_alloc`, which placed a size
    // header exactly HEADER_BYTES before the returned pointer.
    let header_ptr = unsafe { (address as *mut u8).sub(FFI_HEADER_BYTES) };
    let size = unsafe { core::ptr::read(header_ptr as *const usize) };
    let layout = Layout::from_size_align(size + FFI_HEADER_BYTES, FFI_ALIGN)
        .expect("layout round-trips from ffi_alloc");
    // SAFETY: opaque is the same FfiMemTracker that `ffi_alloc` saw
    // for this CCtx; single-threaded per-CCtx.
    let tracker = unsafe { &mut *(opaque as *mut FfiMemTracker) };
    tracker.current = tracker.current.saturating_sub(size);
    // SAFETY: header_ptr + layout matches the pair from `ffi_alloc`.
    unsafe { std::alloc::dealloc(header_ptr, layout) };
}

/// Unified FFI encode helper. Used by both criterion's timing loop
/// (`mem = None`, libzstd uses default malloc — zero instrumentation
/// overhead, matches what an ordinary FFI consumer pays) and the
/// per-shard memory-measurement block (`mem = Some(&mut tracker)` so
/// every libzstd allocation flows through `FfiMemTracker` and produces
/// a precise peak number).
///
/// Both callers exercise the SAME code path — `ZSTD_compressStream2`
/// into a growing-output `Vec`. Earlier revisions of this bench had a
/// split (`zstd::stream::Encoder` for timing, raw API for memory),
/// which let the two metrics describe different operations. The
/// streaming output Vec also matches what the pure-Rust
/// `compress_to_vec` does: both sides grow output incrementally rather
/// than pre-allocating `ZSTD_compressBound`, so the headline
/// peak-memory numbers stay apples-to-apples.
fn ffi_encode_to_vec(input: &[u8], level: i32, mem: Option<&mut FfiMemTracker>) -> Vec<u8> {
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: `ZSTD_createCCtx{,_advanced}` return null on OOM and
    // are otherwise safe to call. The CCtx is freed before returning.
    // When `mem = Some`, the tracker reference outlives the CCtx
    // because we drop the CCtx (and thus issue the final customFree
    // calls) before this function returns.
    let cctx = unsafe {
        match mem {
            Some(tracker) => zstd_sys::ZSTD_createCCtx_advanced(tracker.custom_mem()),
            None => zstd_sys::ZSTD_createCCtx(),
        }
    };
    assert!(!cctx.is_null(), "ZSTD_createCCtx returned null");

    // SAFETY: every `zstd_sys` call below operates on the CCtx we
    // just created and freshly-validated parameter values. Errors
    // are converted to assertion failures so memory measurements
    // can't silently regress to default settings.
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

        // Tiny inputs use a 14-bit window so the FFI frame matches
        // the pure-Rust frame on small payloads. Without this the
        // FFI side picks a larger default window than the Rust
        // encoder emits, biasing the memory comparison.
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

        let recommended_in = zstd_sys::ZSTD_CStreamInSize();
        let recommended_out = zstd_sys::ZSTD_CStreamOutSize();
        let mut output: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; recommended_out];
        let mut in_pos: usize = 0;
        loop {
            let chunk_end = (in_pos + recommended_in).min(input.len());
            let mut zin = zstd_sys::ZSTD_inBuffer {
                src: input.as_ptr() as *const core::ffi::c_void,
                size: chunk_end,
                pos: in_pos,
            };
            let mode = if chunk_end == input.len() {
                zstd_sys::ZSTD_EndDirective::ZSTD_e_end
            } else {
                zstd_sys::ZSTD_EndDirective::ZSTD_e_continue
            };
            loop {
                let mut zout = zstd_sys::ZSTD_outBuffer {
                    dst: chunk.as_mut_ptr() as *mut core::ffi::c_void,
                    size: chunk.len(),
                    pos: 0,
                };
                let remaining = zstd_sys::ZSTD_compressStream2(cctx, &mut zout, &mut zin, mode);
                assert!(
                    zstd_sys::ZSTD_isError(remaining) == 0,
                    "ZSTD_compressStream2 failed (code = {remaining})"
                );
                output.extend_from_slice(&chunk[..zout.pos]);
                let frame_complete =
                    matches!(mode, zstd_sys::ZSTD_EndDirective::ZSTD_e_end) && remaining == 0;
                let chunk_consumed = matches!(mode, zstd_sys::ZSTD_EndDirective::ZSTD_e_continue)
                    && zin.pos == zin.size;
                if frame_complete || chunk_consumed {
                    break;
                }
            }
            in_pos = zin.pos;
            if in_pos == input.len() && matches!(mode, zstd_sys::ZSTD_EndDirective::ZSTD_e_end) {
                break;
            }
        }

        zstd_sys::ZSTD_freeCCtx(cctx);
        output
    }
}

/// Reusable FFI DCtx handle. Wraps `ZSTD_createDCtx{,_advanced}` +
/// `ZSTD_freeDCtx` lifecycle so criterion's `b.iter` timing loop can
/// call `ZSTD_decompressDCtx` repeatedly against the same context —
/// matching the pure-Rust loop which reuses one `FrameDecoder`.
/// Creating a fresh DCtx per iteration would dominate the sample at
/// small payloads (DCtx construction is ~100 KiB of allocation).
///
/// When `mem = Some`, libzstd routes its window buffer + dictionary
/// scratch through `FfiMemTracker`. When `mem = None`, default malloc
/// is used (timing loops want this).
struct FfiDCtxHandle {
    ptr: *mut zstd::zstd_safe::zstd_sys::ZSTD_DCtx_s,
}

impl FfiDCtxHandle {
    fn new(mem: Option<&mut FfiMemTracker>) -> Self {
        use zstd::zstd_safe::zstd_sys;
        // SAFETY: both constructors are safe FFI calls returning
        // null on OOM, which we assert against.
        let ptr = unsafe {
            match mem {
                Some(tracker) => zstd_sys::ZSTD_createDCtx_advanced(tracker.custom_mem()),
                None => zstd_sys::ZSTD_createDCtx(),
            }
        };
        assert!(!ptr.is_null(), "ZSTD_createDCtx returned null");
        FfiDCtxHandle { ptr }
    }

    fn decompress_into(&mut self, compressed: &[u8], output: &mut [u8]) -> usize {
        use zstd::zstd_safe::zstd_sys;
        // SAFETY: `self.ptr` is a valid DCtx, lifetime tied to
        // `self`. `output` and `compressed` are valid slices.
        let written = unsafe {
            zstd_sys::ZSTD_decompressDCtx(
                self.ptr,
                output.as_mut_ptr() as *mut core::ffi::c_void,
                output.len(),
                compressed.as_ptr() as *const core::ffi::c_void,
                compressed.len(),
            )
        };
        assert!(
            unsafe { zstd_sys::ZSTD_isError(written) } == 0,
            "ZSTD_decompressDCtx failed (code = {written})"
        );
        written
    }
}

impl Drop for FfiDCtxHandle {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was created by `Self::new` and is freed
        // exactly once here. After this, any FfiMemTracker borrowed
        // through `custom_mem` is safe to read on the same thread —
        // libzstd's final `customFree` calls run synchronously
        // inside `ZSTD_freeDCtx`.
        unsafe {
            zstd::zstd_safe::zstd_sys::ZSTD_freeDCtx(self.ptr);
        }
    }
}

/// Convenience wrapper for one-shot decompression. Used by the
/// per-shard memory measurement and the reference-equality check;
/// timing loops use `FfiDCtxHandle` directly to amortise context
/// construction.
fn ffi_decompress_into(
    compressed: &[u8],
    output: &mut [u8],
    mem: Option<&mut FfiMemTracker>,
) -> usize {
    let mut dctx = FfiDCtxHandle::new(mem);
    dctx.decompress_into(compressed, output)
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
                // Rust side: OS RSS sampling around one encode call.
                // No global allocator wrapper, so criterion's timing
                // loops below stay uninstrumented.
                let rust_window = rss::PeakWindow::start();
                let rust_compressed = structured_zstd::encoding::compress_to_vec(
                    &scenario.bytes[..],
                    level.rust_level,
                );
                let rust_peak_rss_delta_bytes = rust_window.finish();
                // FFI side: per-CCtx tracker observes every libzstd
                // malloc/free precisely. Same `ffi_encode_to_vec` the
                // timing loop below calls — only the customMem opt-in
                // differs.
                let mut ffi_tracker = FfiMemTracker::default();
                let ffi_compressed =
                    ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level, Some(&mut ffi_tracker));
                let ffi_peak_bytes = ffi_tracker.peak;
                emit_report_line(scenario, level, &rust_compressed, &ffi_compressed);
                emit_frame_header_report(scenario, level, "rust", &rust_compressed);
                emit_frame_header_report(scenario, level, "ffi", &ffi_compressed);
                emit_memory_report(
                    scenario,
                    level,
                    "compress",
                    rust_peak_rss_delta_bytes,
                    ffi_peak_bytes,
                );
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
                b.iter(|| {
                    black_box(ffi_encode_to_vec(
                        &scenario.bytes[..],
                        level.ffi_level,
                        None,
                    ))
                })
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
            // Build the FFI fixture via the unified helper with
            // `None` so this prep step pays no customMem overhead; the
            // bytes feed both `c_stream` decode benches below.
            let ffi_compressed = ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level, None);
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
        // path. Done OUTSIDE criterion's bench loop so the sample is
        // dominated by decoder internals (FrameDecoder state for
        // rust, ZSTD_DCtx + window for ffi). Rust uses OS RSS
        // sampling; FFI uses a per-DCtx customMem tracker. Both
        // observe the SAME decode call the timing loop below runs —
        // only the memory hook differs.
        let rust_window = rss::PeakWindow::start();
        {
            let mut target = vec![0u8; expected_len];
            let mut decoder = FrameDecoder::new();
            let written = decoder.decode_all(compressed, &mut target).unwrap();
            assert_eq!(written, expected_len);
            black_box(target);
        }
        let rust_peak_rss_delta_bytes = rust_window.finish();

        let mut ffi_tracker = FfiMemTracker::default();
        let mut ffi_target = vec![0u8; expected_len];
        let written = ffi_decompress_into(compressed, &mut ffi_target, Some(&mut ffi_tracker));
        assert_eq!(written, expected_len);
        let ffi_peak_bytes = ffi_tracker.peak;

        emit_memory_report(
            scenario,
            level,
            &format!("decompress-{source}"),
            rust_peak_rss_delta_bytes,
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
        // Reuse one DCtx + target buffer across iterations so the
        // timing sample reflects decode steady-state — matches the
        // pure-Rust loop above which reuses one `FrameDecoder` and
        // one `target`. Creating a fresh DCtx per iteration would
        // dominate sub-millisecond samples.
        let mut dctx = FfiDCtxHandle::new(None);
        let mut target = vec![0u8; expected_len];
        b.iter(|| {
            let written = dctx.decompress_into(black_box(compressed), &mut target);
            assert_eq!(written, expected_len);
            black_box(&target[..written]);
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

    let mut ffi_target = vec![0u8; expected_len];
    let ffi_written = ffi_decompress_into(compressed, &mut ffi_target, None);
    assert_eq!(ffi_written, expected_len);
    assert_eq!(&ffi_target[..ffi_written], scenario.bytes.as_slice());
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
    rust_peak_rss_delta_bytes: usize,
    ffi_peak_alloc_bytes: usize,
) {
    let escaped_label = escape_report_label(&scenario.label);
    // Asymmetric metric semantics — named honestly:
    //   - `ffi_peak_alloc_bytes`: precise sum of bytes requested from
    //     `ZSTD_customMem` callbacks during one CCtx/DCtx lifetime.
    //     Reflects every libzstd malloc/free.
    //   - `rust_peak_rss_delta_bytes`: OS resident-set-size growth
    //     during one encode/decode call (background-sampled via
    //     `mach_task_basic_info` / `/proc/self/statm`). Approximates
    //     peak working set, but allocations satisfied from pages
    //     already faulted in or from the allocator's cached arena do
    //     not bump RSS — warm scenarios may underreport.
    // The fields are different proxies for "memory pressure during
    // this op"; their absolute values are NOT directly comparable
    // cross-side, though the relative shape over scenarios still
    // exposes regressions. Dashboard plots them as two series under
    // the `peak_alloc_bytes` metric group — see
    // `.github/scripts/run-benchmarks.sh`.
    println!(
        "REPORT_MEM scenario={} label=\"{}\" level={} stage={} rust_peak_rss_delta_bytes={} ffi_peak_alloc_bytes={}",
        scenario.id,
        escaped_label,
        level.name,
        stage,
        rust_peak_rss_delta_bytes,
        ffi_peak_alloc_bytes
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
