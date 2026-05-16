//! Memory peak benchmark: structured-zstd (pure Rust) vs zstd (C FFI).
//!
//! Strictly separate from the timing/ratio bench (`compare_ffi.rs`).
//! This binary installs a `#[global_allocator]` tracking wrapper and
//! routes libzstd's `ZSTD_customMem` callbacks through the same Rust
//! allocator, so a SINGLE observer counts bytes on both sides of the
//! comparison. The tracking allocator adds per-allocation overhead
//! that biases criterion timing — that's the whole reason this lives
//! in its own binary instead of being a mode flag on the timing bench.
//!
//! Output: `REPORT_MEM` lines (same format `compare_ffi.rs` used to
//! emit before being stripped). Aggregator script (`run-benchmarks.sh`)
//! parses both binaries' output uniformly.
//!
//! Bench-only: lives entirely in `zstd/benches/`; never linked into the
//! published `structured-zstd` crate.

// `support` is shared with `compare_ffi.rs`. This bench uses only a
// subset of `Scenario`'s fields/methods, but the others are public
// API for the timing bench — silence dead_code per this binary only.
#[allow(dead_code)]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use structured_zstd::decoding::FrameDecoder;
use support::{LevelConfig, Scenario, benchmark_scenarios, supported_levels_filtered};

/// Process-wide byte tracker. Counts EVERY allocation in this binary
/// once `TRACKING_ENABLED` flips true — Rust-side `Vec`/`Box`/encoder
/// internals via the `#[global_allocator]` route, FFI-side libzstd
/// requests via the `ZSTD_customMem` callbacks (which themselves call
/// `std::alloc::alloc`, going through this same wrapper). Both sides
/// share the counter, so the reported peak is by construction
/// symmetric — no cross-observer ambiguity.
struct TrackingAllocator;

static ALLOC_CURRENT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BASELINE: AtomicUsize = AtomicUsize::new(0);
/// `false` until the measurement window opens. Allocations made
/// before this flag flips (criterion harness setup, scenario corpus
/// loading) are not counted. Per-allocation `dealloc` uses a header
/// flag (see below) instead of `TRACKING_ENABLED` so allocations made
/// inside the window but dropped outside it still subtract cleanly.
static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);

/// 16-byte header in front of every user pointer. First byte stores
/// "was this alloc counted?" so `dealloc` can balance the counter
/// regardless of current `TRACKING_ENABLED` state. The remaining 15
/// bytes are padding to keep the user pointer aligned for SSE/NEON.
const HEADER_BYTES: usize = 16;
const FLAG_UNCOUNTED: u8 = 0;
const FLAG_COUNTED: u8 = 1;

#[inline]
fn augmented_layout(layout: Layout) -> Option<Layout> {
    let total = layout.size().checked_add(HEADER_BYTES)?;
    Layout::from_size_align(total, layout.align().max(HEADER_BYTES)).ok()
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(augmented) = augmented_layout(layout) else {
            return core::ptr::null_mut();
        };
        let header = layout.align().max(HEADER_BYTES);
        // SAFETY: `augmented` is a valid Layout (size+header, alignment ≥ header).
        let raw = unsafe { System.alloc(augmented) };
        if raw.is_null() {
            return raw;
        }
        let counted = TRACKING_ENABLED.load(Ordering::Relaxed);
        // SAFETY: first `header` bytes of `raw` belong to us.
        unsafe {
            *raw = if counted {
                FLAG_COUNTED
            } else {
                FLAG_UNCOUNTED
            };
        }
        if counted {
            let prev = ALLOC_CURRENT.fetch_add(layout.size(), Ordering::Relaxed);
            ALLOC_PEAK.fetch_max(prev + layout.size(), Ordering::Relaxed);
        }
        // SAFETY: `raw + header` is aligned to `layout.align()` (header is a
        // multiple of align since header = max(align, 16)).
        unsafe { raw.add(header) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let header = layout.align().max(HEADER_BYTES);
        // SAFETY: `ptr` came from our `alloc`, header sits exactly `header`
        // bytes earlier.
        let raw = unsafe { ptr.sub(header) };
        let counted = unsafe { *raw } == FLAG_COUNTED;
        if counted {
            ALLOC_CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        let augmented = Layout::from_size_align(layout.size() + header, header)
            .expect("layout round-trips on dealloc");
        // SAFETY: `(raw, augmented)` matches the pair from `alloc` above.
        unsafe { System.dealloc(raw, augmented) };
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

/// Snapshot `ALLOC_CURRENT` as baseline, enable counting, run `f`,
/// disable counting, return peak bytes above baseline. RAII guard so
/// a panic inside `f` still flips counting off.
fn measure_peak<R>(f: impl FnOnce() -> R) -> (R, usize) {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            TRACKING_ENABLED.store(false, Ordering::Relaxed);
        }
    }
    let baseline = ALLOC_CURRENT.load(Ordering::Relaxed);
    ALLOC_BASELINE.store(baseline, Ordering::Relaxed);
    ALLOC_PEAK.store(baseline, Ordering::Relaxed);
    TRACKING_ENABLED.store(true, Ordering::Relaxed);
    let _g = Guard;
    let result = f();
    let peak = ALLOC_PEAK.load(Ordering::Relaxed);
    (result, peak.saturating_sub(baseline))
}

/// `ZSTD_customMem` allocate callback. Routes through `std::alloc::alloc`
/// so libzstd's heap is observed by `TrackingAllocator` exactly like
/// Rust-side allocations.
unsafe extern "C" fn ffi_alloc(
    _opaque: *mut core::ffi::c_void,
    size: usize,
) -> *mut core::ffi::c_void {
    const FFI_HEADER: usize = 16;
    const FFI_ALIGN: usize = 16;
    let Some(total) = size.checked_add(FFI_HEADER) else {
        return core::ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, FFI_ALIGN) else {
        return core::ptr::null_mut();
    };
    // SAFETY: layout validated above; std::alloc::alloc routes through
    // `#[global_allocator] = TrackingAllocator`.
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    // Store the libzstd-requested size in the first 8 bytes of our
    // 16-byte header so `ffi_free` can recover it (libzstd's free
    // callback receives only a pointer).
    unsafe {
        core::ptr::write(raw as *mut usize, size);
    }
    unsafe { raw.add(FFI_HEADER) as *mut core::ffi::c_void }
}

unsafe extern "C" fn ffi_free(_opaque: *mut core::ffi::c_void, address: *mut core::ffi::c_void) {
    const FFI_HEADER: usize = 16;
    const FFI_ALIGN: usize = 16;
    if address.is_null() {
        return;
    }
    // SAFETY: `address` came from `ffi_alloc` above; size header sits
    // FFI_HEADER bytes earlier.
    let header_ptr = unsafe { (address as *mut u8).sub(FFI_HEADER) };
    let size = unsafe { core::ptr::read(header_ptr as *const usize) };
    let layout = Layout::from_size_align(size + FFI_HEADER, FFI_ALIGN)
        .expect("layout round-trips from ffi_alloc");
    // SAFETY: `(header_ptr, layout)` matches the pair from `ffi_alloc`.
    unsafe { std::alloc::dealloc(header_ptr, layout) };
}

fn ffi_custom_mem() -> zstd::zstd_safe::zstd_sys::ZSTD_customMem {
    zstd::zstd_safe::zstd_sys::ZSTD_customMem {
        customAlloc: Some(ffi_alloc),
        customFree: Some(ffi_free),
        opaque: core::ptr::null_mut(),
    }
}

/// FFI encode via `ZSTD_compressStream2` with customMem hooks. Same
/// settings as the timing bench's `ffi_encode_to_vec` (level, checksum,
/// content-size, tiny-source window override) — only the customMem is
/// added so the libzstd heap is observed.
fn ffi_encode(input: &[u8], level: i32) -> Vec<u8> {
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: `ZSTD_createCCtx_advanced` returns null on OOM; we
    // assert and free below.
    let cctx = unsafe { zstd_sys::ZSTD_createCCtx_advanced(ffi_custom_mem()) };
    assert!(!cctx.is_null(), "ZSTD_createCCtx_advanced returned null");
    unsafe {
        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_compressionLevel,
            level,
        );
        assert!(zstd_sys::ZSTD_isError(rc) == 0);
        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_checksumFlag,
            if cfg!(feature = "hash") { 1 } else { 0 },
        );
        assert!(zstd_sys::ZSTD_isError(rc) == 0);
        let rc = zstd_sys::ZSTD_CCtx_setParameter(
            cctx,
            zstd_sys::ZSTD_cParameter::ZSTD_c_contentSizeFlag,
            1,
        );
        assert!(zstd_sys::ZSTD_isError(rc) == 0);
        if input.len() <= (1 << 14) {
            let rc = zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_windowLog,
                14,
            );
            assert!(zstd_sys::ZSTD_isError(rc) == 0);
        }
        let rc = zstd_sys::ZSTD_CCtx_setPledgedSrcSize(cctx, input.len() as u64);
        assert!(zstd_sys::ZSTD_isError(rc) == 0);

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
                assert!(zstd_sys::ZSTD_isError(remaining) == 0);
                output.extend_from_slice(&chunk[..zout.pos]);
                let frame_done =
                    matches!(mode, zstd_sys::ZSTD_EndDirective::ZSTD_e_end) && remaining == 0;
                let chunk_done = matches!(mode, zstd_sys::ZSTD_EndDirective::ZSTD_e_continue)
                    && zin.pos == zin.size;
                if frame_done || chunk_done {
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

fn ffi_decode(compressed: &[u8], expected_len: usize) -> Vec<u8> {
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: same lifetime contract as `ffi_encode`.
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
        assert!(zstd_sys::ZSTD_isError(written) == 0);
        output.truncate(written);
        zstd_sys::ZSTD_freeDCtx(dctx);
        output
    }
}

fn escape_report_label(label: &str) -> String {
    label.replace('\\', "\\\\").replace('\"', "\\\"")
}

fn emit_report(
    scenario: &Scenario,
    level: LevelConfig,
    stage: &str,
    rust_peak: usize,
    ffi_peak: usize,
) {
    let escaped = escape_report_label(&scenario.label);
    println!(
        "REPORT_MEM scenario={} label=\"{}\" level={} stage={} rust_peak_alloc_bytes={} ffi_peak_alloc_bytes={}",
        scenario.id, escaped, level.name, stage, rust_peak, ffi_peak
    );
}

fn main() {
    let scenarios = benchmark_scenarios();
    for scenario in &scenarios {
        for level in supported_levels_filtered() {
            // Compress
            let (rust_compressed, rust_peak) = measure_peak(|| {
                structured_zstd::encoding::compress_to_vec(&scenario.bytes[..], level.rust_level)
            });
            let (ffi_compressed, ffi_peak) =
                measure_peak(|| ffi_encode(&scenario.bytes[..], level.ffi_level));
            emit_report(scenario, level, "compress", rust_peak, ffi_peak);

            let expected_len = scenario.len();

            // Decode the Rust-encoded frame on both sides for the
            // `rust_stream` decode stage; mirror with the FFI-encoded
            // frame for `c_stream`. Matches the timing bench's two
            // decode source variants.
            for (source, compressed) in [
                ("rust_stream", &rust_compressed),
                ("c_stream", &ffi_compressed),
            ] {
                let (_, rust_decode_peak) = measure_peak(|| {
                    let mut target = vec![0u8; expected_len];
                    let mut decoder = FrameDecoder::new();
                    let written = decoder
                        .decode_all(compressed.as_slice(), &mut target)
                        .unwrap();
                    assert_eq!(written, expected_len);
                    target
                });
                let (_, ffi_decode_peak) =
                    measure_peak(|| ffi_decode(compressed.as_slice(), expected_len));
                emit_report(
                    scenario,
                    level,
                    &format!("decompress-{source}"),
                    rust_decode_peak,
                    ffi_decode_peak,
                );
            }
        }
    }
}
