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

// This bench targets THROUGHPUT and COMPRESSION RATIO only — no memory
// observation. Memory measurement lives in the separate
// `compare_ffi_memory` binary (`zstd/benches/compare_ffi_memory.rs`) so
// criterion's timing loops here run with a vanilla system allocator
// and no `ZSTD_customMem` hooks. Conflating timing and memory in one
// run forced asymmetric observers (OS RSS for Rust vs customMem for
// FFI) which Copilot/CR correctly flagged as non-comparable across
// sides. The split bench lets a single tracking allocator observe
// BOTH sides symmetrically while leaving this file untouched on the
// timing hot path.
use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use structured_zstd::decoding::FrameDecoder;
use structured_zstd::dictionary::{
    FastCoverOptions, FinalizeOptions, finalize_raw_dict, train_fastcover_raw_from_slice,
};
use structured_zstd::encoding::{EncoderDictionary, FrameCompressor};
use support::{
    LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, build_training_samples,
    dictionary_size_for, kernel_report_line, ldm_parameters, supported_levels_filtered,
};

static BENCHMARK_SCENARIOS: OnceLock<Vec<Scenario>> = OnceLock::new();

/// Apply the matrix variant's options to an FFI bulk compressor: LDM for
/// the `*_ldm*` variants and the checksum flag (parity with both
/// `ffi_encode_to_vec` and the Rust encoder's default).
fn configure_ffi_bulk_compressor(compressor: &mut zstd::bulk::Compressor<'_>, level: &LevelConfig) {
    if level.ldm {
        compressor
            .set_parameter(zstd::zstd_safe::CParameter::EnableLongDistanceMatching(
                true,
            ))
            .expect("FFI bulk compressor accepts EnableLongDistanceMatching");
    }
    // Checksum parity with the no-dict groups: `ffi_encode_to_vec` sets
    // `ZSTD_c_checksumFlag` under the `hash` feature, but the bulk
    // compressors used by the dictionary arms default it OFF — leaving the
    // Rust side (content checksum on by default) paying the XXH64 pass the
    // FFI side skipped (~6% of frame time on small dict frames).
    if cfg!(feature = "hash") {
        compressor
            .set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .expect("FFI bulk compressor accepts ChecksumFlag");
    }
}

/// Build the matching Rust-encoder bytes for a matrix variant. The bench
/// matrix measures the FULL feature gate on both sides: the content
/// checksum is enabled explicitly here (the encoder's default mirrors the
/// upstream library default, OFF) just as `ffi_encode_to_vec` and
/// `configure_ffi_bulk_compressor` enable `ZSTD_c_checksumFlag`, all under
/// the same `hash` feature.
fn rust_encode_to_vec(input: &[u8], level: &LevelConfig) -> Vec<u8> {
    let mut output = Vec::new();
    rust_encode_into(input, level, &mut output);
    output
}

/// The same encode, writing into a caller-owned buffer.
///
/// The timing loop uses THIS one, because the C arm it is compared against
/// keeps one `dst` for the whole sample: a fresh context per iteration (both
/// sides do that) but a reused output. Allocating and freeing a
/// megabyte-sized output every iteration on our side alone is not a property
/// of the encoder — on a fresh heap the allocator maps and unmaps it, which
/// measured as four times the encode on incompressible input and made the
/// comparison against upstream a comparison of allocator behaviour.
fn rust_encode_into(input: &[u8], level: &LevelConfig, output: &mut Vec<u8>) {
    let mut enc: FrameCompressor = FrameCompressor::new(level.rust_level);
    if let Some(params) = ldm_parameters(level) {
        enc.set_parameters(&params);
    }
    enc.set_content_checksum(cfg!(feature = "hash"));
    enc.compress_independent_frame_into(input, output);
}

/// FFI encode helper used by criterion's timing loop: one-shot
/// `ZSTD_compress2` on a `ZSTD_compressBound`-sized `Vec`, the same
/// single-call frame the pure-Rust `compress_independent_frame` produces
/// (see the block-splitting note inside). When `ldm` is set,
/// `ZSTD_c_enableLongDistanceMatching` is turned on alongside the level so
/// the FFI reference mirrors the Rust LDM variant (#362).
fn ffi_encode_to_vec(input: &[u8], level: i32, ldm: bool) -> Vec<u8> {
    let mut output = Vec::new();
    ffi_encode_into(input, level, ldm, &mut output);
    output
}

/// [`ffi_encode_to_vec`] into a caller-owned buffer (the C `dst` contract):
/// the timing loop hands in one buffer reserved to `ZSTD_compressBound`
/// ONCE, so the iteration neither allocates nor zero-fills an input-sized
/// output (a per-iteration `vec![0; bound]` charged the C arm a memset the
/// Rust arm never pays, and its alloc / free churn perturbed the Rust
/// arm's heap layout enough to move small-frame samples by several %).
fn ffi_encode_into(input: &[u8], level: i32, ldm: bool, output: &mut Vec<u8>) {
    use zstd::zstd_safe::zstd_sys;
    // SAFETY: `ZSTD_createCCtx` returns null on OOM, asserted below.
    // The CCtx is freed before returning.
    let cctx = unsafe { zstd_sys::ZSTD_createCCtx() };
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

        if ldm {
            let rc = zstd_sys::ZSTD_CCtx_setParameter(
                cctx,
                zstd_sys::ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching,
                1,
            );
            assert!(
                zstd_sys::ZSTD_isError(rc) == 0,
                "set enableLongDistanceMatching failed"
            );
        }

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

        // One-shot `ZSTD_compress2`, the path `compress_independent_frame`
        // mirrors. The streaming API (`ZSTD_compressStream2`) is NOT
        // equivalent even when handed the whole input at once: it buffers
        // `blockSizeMax` (128 KB) and runs `ZSTD_compress_frameChunk` per
        // 128 KB, whose `ZSTD_optimalBlockSize` never splits a remainder
        // below 128 KB, so streaming emits at most two blocks per 128 KB
        // (14 on the 1 MB corpus) while one-shot keeps pre-splitting (~90
        // blocks, ~5% smaller output, 7x the entropy work). Pairing our
        // one-shot encoder with streaming C misstates both time and size.
        let bound = zstd_sys::ZSTD_compressBound(input.len());
        output.clear();
        output.reserve(bound);
        let written = zstd_sys::ZSTD_compress2(
            cctx,
            output.as_mut_ptr() as *mut core::ffi::c_void,
            bound,
            input.as_ptr() as *const core::ffi::c_void,
            input.len(),
        );
        assert!(
            zstd_sys::ZSTD_isError(written) == 0,
            "ZSTD_compress2 failed (code = {written})"
        );
        // `ZSTD_compress2` initialised exactly `written <= bound` bytes.
        output.set_len(written);

        zstd_sys::ZSTD_freeCCtx(cctx);
    }
}

/// Reusable FFI DCtx handle. Wraps `ZSTD_createDCtx` + `ZSTD_freeDCtx`
/// lifecycle so criterion's `b.iter` timing loop can call
/// `ZSTD_decompressDCtx` repeatedly against the same context —
/// matching the pure-Rust loop which reuses one `FrameDecoder`.
/// Creating a fresh DCtx per iteration would dominate the sample at
/// small payloads (DCtx construction is ~100 KiB of allocation).
struct FfiDCtxHandle {
    ptr: *mut zstd::zstd_safe::zstd_sys::ZSTD_DCtx_s,
}

impl FfiDCtxHandle {
    fn new() -> Self {
        use zstd::zstd_safe::zstd_sys;
        // SAFETY: `ZSTD_createDCtx` returns null on OOM, asserted below.
        let ptr = unsafe { zstd_sys::ZSTD_createDCtx() };
        assert!(!ptr.is_null(), "ZSTD_createDCtx returned null");
        FfiDCtxHandle { ptr }
    }

    fn decompress_into(&mut self, compressed: &[u8], output: &mut [u8]) -> usize {
        use zstd::zstd_safe::zstd_sys;
        // SAFETY: `self.ptr` is a valid DCtx, lifetime tied to `self`.
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
        // exactly once here.
        unsafe {
            zstd::zstd_safe::zstd_sys::ZSTD_freeDCtx(self.ptr);
        }
    }
}

/// One-shot decompress helper used by reference-equality checks.
fn ffi_decompress_into(compressed: &[u8], output: &mut [u8]) -> usize {
    let mut dctx = FfiDCtxHandle::new();
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

/// Emit the shared `REPORT_KERNEL` line exactly once per process. The kernel
/// tier is process-global, so a single line covers every bench in the run even
/// though both `bench_compress` and `bench_decompress` call this.
fn emit_kernel_report_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        println!("{}", kernel_report_line());
    });
}

fn bench_compress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    if emit_reports {
        emit_kernel_report_once();
    }
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels_filtered() {
            // Dictionary variants (`*_ldm_dict`) route through
            // `bench_dictionary`; the plain compress group only covers the
            // no-dictionary levels (numeric levels + the `*_ldm` variants).
            if level.dict {
                continue;
            }
            if emit_reports {
                let rust_compressed = rust_encode_to_vec(&scenario.bytes[..], &level);
                let ffi_compressed =
                    ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level, level.ldm);
                emit_report_line(scenario, level, &rust_compressed, &ffi_compressed);
                emit_frame_header_report(scenario, level, "rust", &rust_compressed);
                emit_frame_header_report(scenario, level, "ffi", &ffi_compressed);
                emit_block_structure_report(scenario, level, "rust", &rust_compressed);
                emit_block_structure_report(scenario, level, "ffi", &ffi_compressed);
            }

            let benchmark_name = format!("compress/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, scenario, BenchOp::Compress);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            // The paired comparison, which is what the dashboard reports. It
            // runs before the criterion arms so neither arm's own measurement
            // colours it, over the same buffers and the same per-frame shape.
            if emit_reports {
                let paired = ARENA.with_borrow_mut(|arena| {
                    let mut outputs = arena.appended_slots();
                    measure_pair(|arm, slot| {
                        let output = &mut *outputs[slot];
                        match arm {
                            Arm::Rust => rust_encode_into(&scenario.bytes[..], &level, output),
                            Arm::Ffi => ffi_encode_into(
                                &scenario.bytes[..],
                                level.ffi_level,
                                level.ldm,
                                output,
                            ),
                        }
                        black_box(&output);
                    })
                });
                emit_pair_report("compress", scenario, level.name, "na", paired);
            }

            // One output buffer per arm for the whole sample (the C caller's
            // `dst`; see `ffi_encode_into`), from the process-wide arena so
            // neither arm's pages depend on what ran before this group.
            bench_arm_pair(
                &mut group,
                "pure_rust",
                |b| {
                    ARENA.with_borrow_mut(|arena| {
                        let output = arena.appended(Arm::Rust);
                        b.iter(|| {
                            rust_encode_into(&scenario.bytes[..], &level, output);
                            black_box(&output);
                        })
                    })
                },
                "c_ffi",
                |b| {
                    ARENA.with_borrow_mut(|arena| {
                        let output = arena.appended(Arm::Ffi);
                        b.iter(|| {
                            ffi_encode_into(
                                &scenario.bytes[..],
                                level.ffi_level,
                                level.ldm,
                                output,
                            );
                            black_box(&output);
                        })
                    })
                },
            );

            group.finish();
        }
    }
}

fn bench_decompress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    if emit_reports {
        emit_kernel_report_once();
    }
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels_filtered() {
            // Dictionary variants decode via the dictionary-aware groups in
            // `bench_dictionary` (`decompress-dict/...`); the plain decode
            // group only covers the no-dictionary levels. The `*_ldm` frames
            // decode through the same dictionary-free path as numeric levels
            // (LDM is an encoder-only concern), just over LDM-encoded bytes.
            if level.dict {
                continue;
            }
            let expected_len = scenario.len();
            bench_decompress_source(
                c,
                scenario,
                level,
                "rust_stream",
                expected_len,
                emit_reports,
            );
            bench_decompress_source(c, scenario, level, "c_stream", expected_len, emit_reports);
        }
    }
}

/// Force every page of `buf` into the process's resident set with one
/// volatile write per page-sized stride. `vec![0u8; N]` returns pages
/// CoW-mapped to the kernel zero page on Linux; the first real write
/// per page in the timed iter would otherwise trigger a synchronous
/// page-fault to allocate the anon backing. On `--profile-time` runs
/// (no warmup) that accounted for 67% of total samples on z000033 L-3
/// c_stream flamegraph.
///
/// Volatile writes are required because under the bench profile's fat
/// LTO + single-codegen-unit settings, a plain `slice::fill(0)` /
/// `Vec::resize` followed by full-slice overwrite from the decoder is
/// a dead-store the optimizer can elide. `write_volatile` is a guaranteed
/// side effect — LLVM may not remove it. One write per 4 KiB stride
/// (the most common page size; larger huge pages still get touched at
/// the smaller stride) is enough to fault each anon page in.
/// Whether this round registers the C arm of a comparison first.
///
/// Criterion does not interleave samples from separate `bench_function`
/// registrations: whichever arm is registered first meets whatever state the
/// rest of the matrix left behind, and does so in every round for as long as
/// the order is fixed. Repeating the matrix does not remove an effect that sits
/// on a POSITION rather than on a moment — this repository has published a
/// fourfold difference between the two arms that came from exactly that, on
/// input the two sources encode identically.
///
/// So the runner varies the order between rounds and the parser takes each
/// arm's minimum across them, which leaves no position for such an effect to
/// sit on. Rounds are numbered from one, so odd rounds keep the declaration
/// order and even rounds reverse it; a single-round run (a local `cargo bench`)
/// keeps the declaration order.
fn reverse_arm_order() -> bool {
    use std::sync::OnceLock;
    static REVERSED: OnceLock<bool> = OnceLock::new();
    *REVERSED.get_or_init(|| {
        std::env::var("STRUCTURED_ZSTD_BENCH_ROUND")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|round| round % 2 == 0)
    })
}

/// Register the two arms of a comparison in this round's order.
///
/// Both are registered either way; only which one Criterion runs first moves.
fn bench_arm_pair<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
    rust_name: &str,
    rust_arm: impl FnMut(&mut criterion::Bencher<'_, M>),
    ffi_name: &str,
    ffi_arm: impl FnMut(&mut criterion::Bencher<'_, M>),
) {
    if reverse_arm_order() {
        group.bench_function(ffi_name, ffi_arm);
        group.bench_function(rust_name, rust_arm);
    } else {
        group.bench_function(rust_name, rust_arm);
        group.bench_function(ffi_name, ffi_arm);
    }
}

#[inline(never)]
fn pretouch_pages(buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    // 4 KiB is a common base page size; on systems with larger base
    // pages (16 KiB on Apple Silicon, 64 KiB on some aarch64 kernels)
    // we touch more often than strictly required — still correct,
    // cheap.
    const STRIDE: usize = 4096;
    let len = buf.len();
    let ptr = buf.as_mut_ptr();
    // SAFETY: `ptr` is non-null (buf non-empty above) and each
    // `ptr.add(off)` stays within `len` due to the `step_by` range
    // bound. `write_volatile` of `0u8` does not alias other live
    // references. Iterating via `(0..len).step_by(STRIDE)` guarantees
    // termination — no `usize` overflow risk that a manual `off +=
    // STRIDE` accumulator carries for buffers approaching `usize::MAX`.
    unsafe {
        for off in (0..len).step_by(STRIDE) {
            ptr.add(off).write_volatile(0);
        }
        // Also touch the final byte so the tail page is in even if
        // `len` is not a multiple of STRIDE.
        ptr.add(len - 1).write_volatile(0);
    }
}

/// A comparison measured with the two implementations alternating inside one
/// window, rather than one whole measurement after the other.
///
/// Criterion runs an arm to completion before the next one starts, which on a
/// shared runner leaves seconds between them, and the machine's speed moves on
/// that scale: the same cell has been seen at 27.2 and 35.5 us in one process,
/// and the two arms of a cell landing in different states produced a 1.28x
/// difference between the implementations out of nothing. Pinning the process
/// to one CPU narrows it but does not remove it, so the answer is not to make
/// the machine hold still, it is to stop asking the two arms about different
/// moments. Alternating per sample scales both sides by whatever the machine
/// is doing, and that cancels in the ratio.
struct PairedMeasurement {
    /// `ffi_ns / rust_ns` per sample, then the median: above 1 means ours is
    /// faster, matching the sign the dashboard already uses for speed.
    speedup_median: f64,
    speedup_min: f64,
    speedup_max: f64,
    /// The best batch either side reached, for the absolute series.
    rust_min_ns: f64,
    ffi_min_ns: f64,
    samples: usize,
    /// Each side gets its own batch size, sized so both batches last about as
    /// long. A shared count would make the slower side's batch as many times
    /// longer as it is slower, which on a pair where one side is five times
    /// the other turns a two-second cell into ten.
    rust_iters: u64,
    ffi_iters: u64,
}

/// Time `iters` calls of one arm on one arena slot and return the nanoseconds
/// one call took.
fn time_batch(run: &mut impl FnMut(Arm, usize), arm: Arm, slot: usize, iters: u64) -> f64 {
    let start = std::time::Instant::now();
    for _ in 0..iters {
        run(arm, slot);
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// One timed batch, with what it took added to the running cost of its side so
/// the visit's time cap is enforced on what actually happened rather than on
/// what the probe predicted.
fn charged_batch(
    run: &mut impl FnMut(Arm, usize),
    arm: Arm,
    slot: usize,
    iters: u64,
    cost: &mut [f64; 2],
) -> f64 {
    let per_call = time_batch(run, arm, slot, iters);
    cost[arm.side()] += per_call * iters as f64;
    per_call
}

/// One call's cost, cheaply, and what finding it out cost: a single timed
/// call, refined over about a millisecond of calls when that one call was too
/// short for the clock. The second figure is what the visit has already spent
/// on this side, which its remaining budget has to account for.
///
/// One slot is probed, not both. The slots are equal by construction: they come
/// from one process-wide arena, allocated together at one size and pre-faulted
/// before any group runs. A second probe per side would buy a slightly better
/// prediction of the batch cost and pay two more calls of setup for it, and the
/// prediction is not what enforces the time cap anyway: the sampling loop
/// charges each batch as it happens and stops when the budget is gone.
fn probe_per_call_ns(run: &mut impl FnMut(Arm, usize), arm: Arm) -> (f64, f64) {
    const REFINE_BELOW_NS: f64 = 100_000.0;
    const REFINE_FOR_NS: f64 = 1_000_000.0;
    let single = time_batch(run, arm, 0, 1);
    if single >= REFINE_BELOW_NS {
        return (single, single);
    }
    let calls = ((REFINE_FOR_NS / single.max(1.0)).ceil() as u64).clamp(1, 1 << 20);
    let refined = time_batch(run, arm, 0, calls);
    (refined, single + refined * calls as f64)
}

/// Measure both implementations alternately and report the ratio, or `None`
/// when the operation is too slow for the measurement to fit its time cap.
///
/// `run(arm, slot)` performs one call of `arm` writing into arena slot `slot`.
/// One closure rather than one per side, because both sides take turns on
/// both slots and two closures could not each hold both buffers.
fn measure_pair(mut run: impl FnMut(Arm, usize)) -> Option<PairedMeasurement> {
    // A batch has to average as deeply as a criterion sample does, or the
    // minimum over batches is a worse estimate of a side than the minimum over
    // criterion's samples and the pairing loses more than it gains. Criterion
    // spreads ten samples over its measurement budget, so one of its samples is
    // on the order of a tenth of a second; a batch matches that. Measured with
    // a batch a hundred times shorter, this figure moved 0.73% between two runs
    // of identical code against the arms' 0.49%, and up to 18.8% on a cell
    // where the arms moved 0.3%.
    //
    // Depth is a matter of TIME, not of iteration count: an operation slower
    // than the target gets one call per batch, and that call is itself longer
    // than a tenth of a second. (The 41.8% this figure once moved on the
    // slowest fixture came from single-call batches of a few milliseconds
    // under a one-millisecond target, not from single calls as such.)
    const TARGET_BATCH_NS: f64 = 100_000_000.0;
    // What one side of one visit may spend. Nothing else bounds this
    // measurement: it runs outside criterion, so neither the group's
    // measurement time nor the matrix-wide ceiling applies, and at the top
    // levels on the large fixture one call takes about a second. A visit whose
    // smallest useful size does not fit is skipped, and the published delta
    // falls back to the criterion arms for that cell.
    const VISIT_CAP_NS: f64 = 2_000_000_000.0;
    // Batches per side across the WHOLE run, shared out over its rounds. What
    // defeats this measurement is not a few slow batches, which the minimum
    // discards, but a disturbance long enough to cover every batch of a visit:
    // one cell's minimum read 328 ns against a floor of 206 with all twelve
    // batches slow. No statistic inside a visit can see past that, so the
    // defence is more visits, each shorter, with the rest of the matrix between
    // them, at the same total cost. Undisturbed deep batches barely differ
    // (one side held within 0.9% across a whole sweep), so a visit of four
    // estimates its minimum nearly as well as a visit of twelve.
    const TOTAL_SAMPLES: usize = 36;
    const MAX_SAMPLES: usize = 12;
    const MIN_SAMPLES: usize = 4;
    // Batches needed to give both sides both slots and both lead positions:
    // the lead turns every batch and the slots every second one. A visit is
    // rounded down to a whole number of these.
    const ROTATION_PERIOD: usize = 4;
    // A ceiling, so an operation that is somehow free cannot ask for an
    // unbounded batch.
    const MAX_ITERS: u64 = 1 << 26;

    // The cap is a budget per side, and setting the visit up spends from it:
    // warming both slots and probing for a batch size are calls like any other.
    // Counting only the timed batches let a side with 400 ms calls spend 1.2 s
    // on setup and then four 400 ms batches on top, overrunning a 2 s cap by
    // forty percent. Each side's spend is tracked from its first call, and only
    // what is left of the budget pays for batches.
    let mut spent = [0.0f64; 2];
    let mut warm = |arm: Arm, slot: usize, spent: &mut [f64; 2]| {
        let started = std::time::Instant::now();
        run(arm, slot);
        let elapsed = started.elapsed().as_nanos() as f64;
        spent[arm.side()] += elapsed;
        // One call so slow that the smallest visit could not fit ends it here,
        // before the other side spends anything more. One call per side is what
        // the cell would have made anyway.
        elapsed * MIN_SAMPLES as f64 <= VISIT_CAP_NS
    };
    if !warm(Arm::Rust, 0, &mut spent) || !warm(Arm::Ffi, 1, &mut spent) {
        return None;
    }
    warm(Arm::Rust, 1, &mut spent);
    warm(Arm::Ffi, 0, &mut spent);

    // One short probe per side, then each batch size follows by arithmetic. A
    // doubling ladder up to a tenth of a second would cost about as much as
    // the measurement it is sizing.
    let (rust_per_call, rust_probe_ns) = probe_per_call_ns(&mut run, Arm::Rust);
    let (ffi_per_call, ffi_probe_ns) = probe_per_call_ns(&mut run, Arm::Ffi);
    spent[Arm::Rust.side()] += rust_probe_ns;
    spent[Arm::Ffi.side()] += ffi_probe_ns;
    let batch_size = |per_call_ns: f64| -> u64 {
        ((TARGET_BATCH_NS / per_call_ns.max(1.0)).ceil() as u64).clamp(1, MAX_ITERS)
    };
    let rust_iters = batch_size(rust_per_call);
    let ffi_iters = batch_size(ffi_per_call);

    // This visit's share of the run's batches, cut down to what each side has
    // left of its budget. More rounds mean shorter visits, not more work; and
    // below the floor a minimum is a lucky draw, so the visit is not made.
    let share = (TOTAL_SAMPLES / bench_rounds() as usize).clamp(MIN_SAMPLES, MAX_SAMPLES);
    let batches_left = |arm: Arm, batch_ns: f64| -> usize {
        let left = VISIT_CAP_NS - spent[arm.side()];
        if left <= 0.0 {
            return 0;
        }
        (left / batch_ns.max(1.0)) as usize
    };
    let affordable = share
        .min(batches_left(Arm::Rust, rust_per_call * rust_iters as f64))
        .min(batches_left(Arm::Ffi, ffi_per_call * ffi_iters as f64));
    // Down to a whole number of rotation periods. The slots turn every second
    // batch and the lead turns every batch, so four batches is what it takes to
    // give both sides both slots and both positions; a count like six would
    // leave one side on one slot twice as often, and the repetitions would
    // repeat that rather than cancel it.
    let samples = affordable - affordable % ROTATION_PERIOD;
    if samples < MIN_SAMPLES {
        return None;
    }

    let mut rust_samples = Vec::with_capacity(samples);
    let mut ffi_samples = Vec::with_capacity(samples);
    // The count above was sized from the probe, which is a PREDICTION of what a
    // batch will cost; batches that run longer than predicted would carry the
    // visit past its cap. So each batch is charged to its own side as it
    // happens, and the visit ends once another rotation period would not fit in
    // what either side has left. It ends only on a period boundary: cutting
    // mid-period would leave one side a slot or a lead position more often than
    // the other, which is the bias the rotation exists to remove.
    let mut period_cost = [0.0f64; 2];
    for sample in 0..samples {
        // Which slot each side writes to turns every two samples, and which
        // side leads turns every sample, so neither an address nor a position
        // in the pair stays with one implementation. Turning the slots only
        // between rounds would weight one assignment two to one under an odd
        // round count, and a median across rounds keeps that.
        let rust_slot = (sample / 2) % 2;
        let ffi_slot = 1 - rust_slot;
        if sample.is_multiple_of(2) {
            rust_samples.push(charged_batch(
                &mut run,
                Arm::Rust,
                rust_slot,
                rust_iters,
                &mut period_cost,
            ));
            ffi_samples.push(charged_batch(
                &mut run,
                Arm::Ffi,
                ffi_slot,
                ffi_iters,
                &mut period_cost,
            ));
        } else {
            ffi_samples.push(charged_batch(
                &mut run,
                Arm::Ffi,
                ffi_slot,
                ffi_iters,
                &mut period_cost,
            ));
            rust_samples.push(charged_batch(
                &mut run,
                Arm::Rust,
                rust_slot,
                rust_iters,
                &mut period_cost,
            ));
        }
        if !(sample + 1).is_multiple_of(ROTATION_PERIOD) {
            continue;
        }
        let mut fits = true;
        for side in 0..2 {
            spent[side] += period_cost[side];
            fits &= spent[side] + period_cost[side] <= VISIT_CAP_NS;
            period_cost[side] = 0.0;
        }
        if !fits {
            break;
        }
    }

    // The ratio is formed PER SAMPLE and only then summarised. Dividing one
    // side's best by the other's best would pair two moments again, which is
    // the thing this measurement exists to avoid.
    let mut speedups: Vec<f64> = rust_samples
        .iter()
        .zip(&ffi_samples)
        .map(|(rust, ffi)| if *rust > 0.0 { ffi / rust } else { f64::NAN })
        .filter(|value| value.is_finite())
        .collect();
    speedups.sort_by(|a, b| a.partial_cmp(b).expect("filtered to finite"));
    let median = if speedups.is_empty() {
        f64::NAN
    } else {
        let mid = speedups.len() / 2;
        if speedups.len().is_multiple_of(2) {
            (speedups[mid - 1] + speedups[mid]) / 2.0
        } else {
            speedups[mid]
        }
    };
    let fold = |values: &[f64]| values.iter().copied().fold(f64::INFINITY, f64::min);

    Some(PairedMeasurement {
        speedup_median: median,
        speedup_min: speedups.first().copied().unwrap_or(f64::NAN),
        speedup_max: speedups.last().copied().unwrap_or(f64::NAN),
        rust_min_ns: fold(&rust_samples),
        ffi_min_ns: fold(&ffi_samples),
        samples: speedups.len(),
        rust_iters,
        ffi_iters,
    })
}

/// Emit one paired comparison in the shape the benchmark parser reads. A cell
/// too slow to be measured this way emits nothing, and the parser falls back
/// to the criterion arms for it.
fn emit_pair_report(
    stage: &str,
    scenario: &Scenario,
    level_name: &str,
    source: &str,
    paired: Option<PairedMeasurement>,
) {
    let Some(paired) = paired else {
        eprintln!(
            "BENCH_WARN no paired measurement for {stage}/{level_name}/{}/{source}: one visit does not fit its time cap",
            scenario.id
        );
        return;
    };
    println!(
        "REPORT_PAIR stage={stage} scenario={} level={level_name} source={source} \
         speedup_median={:.6} speedup_min={:.6} speedup_max={:.6} \
         rust_min_ns={:.3} ffi_min_ns={:.3} samples={} rust_iters={} ffi_iters={}",
        scenario.id,
        paired.speedup_median,
        paired.speedup_min,
        paired.speedup_max,
        paired.rust_min_ns,
        paired.ffi_min_ns,
        paired.samples,
        paired.rust_iters,
        paired.ffi_iters,
    );
}

/// Which side of a comparison a buffer belongs to.
#[derive(Clone, Copy)]
enum Arm {
    Rust,
    Ffi,
}

impl Arm {
    /// Which side this is, fixed for the life of the process. Distinct from
    /// `slot`, which says which BUFFER the side draws from and deliberately
    /// moves: anything accumulated per implementation has to key off this one.
    fn side(self) -> usize {
        match self {
            Arm::Rust => 0,
            Arm::Ffi => 1,
        }
    }

    /// Which of the arena's two slots this arm draws from THIS round, for the
    /// criterion arms. The paired measurement turns the slots itself, every two
    /// batches, so it does not go through here.
    ///
    /// The mapping rotates with the same round parity that swaps the
    /// registration order. Holding it fixed would leave one implementation on
    /// one address for the life of the dashboard: whatever a particular
    /// address is worth, in cache sets, page colouring or alignment, would be
    /// credited to the same side every round, and a per-arm minimum across
    /// rounds cannot cancel an advantage that never moves. Rotating it lets
    /// both implementations meet both addresses.
    ///
    /// An odd round count still weights one assignment more than the other
    /// here. It matters far less than it would for the published figure: the
    /// arms' minimum is taken over every round's samples pooled, not over a
    /// median of per-round values, so the extra round widens the draw rather
    /// than tilting a middle value. Making it exact needs an even round count,
    /// which is the runner's choice rather than this function's.
    fn slot(self) -> usize {
        let declared = match self {
            Arm::Rust => 0,
            Arm::Ffi => 1,
        };
        if reverse_arm_order() {
            1 - declared
        } else {
            declared
        }
    }
}

/// Every working buffer the matrix uses, allocated once for the process.
///
/// A buffer taken inside a group gets its pages from wherever the heap happens
/// to be when that group runs, which makes a cell's timing depend on its
/// position in the matrix rather than on the work it does. Two shapes of that
/// were measured here: criterion runs one arm to completion before the next is
/// registered, so per-arm buffers came off different heaps; and the decompress
/// groups compressed their fixture before taking their buffers, with a
/// different encoder per source, so the pair landed differently per source. The
/// second read as a threefold difference between the implementations on frames
/// that are byte-identical.
///
/// Allocating once, sized from the whole scenario set before any group runs,
/// leaves no position for an effect like that to sit on: every cell gets the
/// same addresses and the same resident pages whatever ran before it. The cost
/// is that small fixtures hold buffers sized for the largest, which is a
/// constant the measurement no longer has to control for.
///
/// Each PAIR is taken on its first use, so a run filtered to one side of the
/// matrix never allocates the other's: profiling a single kilobyte-sized
/// decompress cell would otherwise pre-touch two compression buffers sized for
/// the largest scenario, which is most of the memory and all of it wasted. What
/// stays eager is the SIZE, taken from every scenario rather than the selected
/// ones — a size that depended on the filter would put the addresses back under
/// the run's control, which is the thing this exists to prevent.
struct BenchArena {
    /// Length every `written` buffer is held at, and the capacity every
    /// `appended` one carries. Fixed before any group runs.
    append_capacity: usize,
    write_len: usize,
    /// For arms that append into a `Vec` (the compression side), held at the
    /// compression bound of the largest scenario so no arm reallocates.
    appended: Option<[Vec<u8>; 2]>,
    /// For arms that write through a `&mut [u8]` (the decompression side),
    /// held at the largest scenario's output length plus the slack the
    /// direct-write path is gated on, and sliced to what a group asks for.
    written: Option<[Vec<u8>; 2]>,
}

impl BenchArena {
    fn for_scenarios(scenarios: &[Scenario]) -> Self {
        let longest = scenarios.iter().map(|s| s.bytes.len()).max().unwrap_or(0);
        Self {
            append_capacity: zstd::zstd_safe::compress_bound(longest),
            write_len: longest + structured_zstd::WILDCOPY_OVERLENGTH,
            appended: None,
            written: None,
        }
    }

    /// Two buffers of `len` bytes with their pages faulted in.
    fn take_pair(len: usize) -> [Vec<u8>; 2] {
        release_freed_memory();
        let mut pair = [vec![0u8; len], vec![0u8; len]];
        for buffer in &mut pair {
            pretouch_pages(buffer);
        }
        pair
    }

    fn append_pair(&mut self) -> &mut [Vec<u8>; 2] {
        let capacity = self.append_capacity;
        self.appended.get_or_insert_with(|| {
            let mut pair = Self::take_pair(capacity);
            // The pages stay faulted in; only the length goes, so an appending
            // arm starts from an empty vector that cannot outgrow its capacity.
            for buffer in &mut pair {
                buffer.clear();
            }
            pair
        })
    }

    fn write_pair(&mut self) -> &mut [Vec<u8>; 2] {
        let len = self.write_len;
        self.written.get_or_insert_with(|| Self::take_pair(len))
    }

    fn appended(&mut self, arm: Arm) -> &mut Vec<u8> {
        let slot = arm.slot();
        &mut self.append_pair()[slot]
    }

    /// Both append slots at once, by slot index, for the paired measurement,
    /// which turns the two sides over both slots itself.
    fn appended_slots(&mut self) -> [&mut Vec<u8>; 2] {
        let [first, second] = self.append_pair();
        [first, second]
    }

    /// Both destination slots at once, by slot index, sliced to `len`.
    fn written_slots(&mut self, len: usize) -> [&mut [u8]; 2] {
        let [first, second] = self.write_pair();
        assert!(
            len <= first.len(),
            "arena holds {} bytes per arm, a group asked for {len}",
            first.len(),
        );
        [&mut first[..len], &mut second[..len]]
    }

    fn written(&mut self, arm: Arm, len: usize) -> &mut [u8] {
        let slot = arm.slot();
        let buffer = &mut self.write_pair()[slot];
        assert!(
            len <= buffer.len(),
            "arena holds {} bytes per arm, a group asked for {len}",
            buffer.len(),
        );
        &mut buffer[..len]
    }
}

thread_local! {
    /// Built on first use from the whole scenario set, so its addresses do not
    /// depend on which group happened to ask first. Criterion drives the
    /// benches from one thread, so one arena per thread is one arena.
    static ARENA: core::cell::RefCell<BenchArena> =
        core::cell::RefCell::new(BenchArena::for_scenarios(benchmark_scenarios_cached()));
}

/// The decoders every plain decompress cell uses, built once for the process.
///
/// A decoder allocates its own working memory when it is built, out of
/// whatever the heap looks like at that moment. Built inside a cell, that
/// placement is decided by everything the matrix did beforehand and then does
/// not move for the life of the cell, so the minimum over the cell's batches is
/// a minimum over one placement rather than over the decode. Measured across
/// nine destination offsets and two sweeps on one runner, libzstd's side held
/// 185.4-187.1 ns on `small-10k-random` while ours ranged 205.8-328.8, and the
/// slow readings covered every batch of a cell rather than a few of them:
/// sweeping the DESTINATION left that untouched, which is what points at the
/// decoder's own memory. Building both once gives every cell the same decoder
/// at the same address.
///
/// The dictionary groups keep building theirs per group: libzstd's side there
/// is bound to a dictionary and cannot be hoisted the same way, and hoisting
/// only ours would put back the asymmetry this is removing.
struct BenchDecoders {
    rust: FrameDecoder,
    ffi: FfiDCtxHandle,
}

thread_local! {
    static DECODERS: core::cell::RefCell<BenchDecoders> =
        core::cell::RefCell::new(BenchDecoders {
            rust: FrameDecoder::new(),
            ffi: FfiDCtxHandle::new(),
        });
}

fn bench_decompress_source(
    c: &mut Criterion,
    scenario: &Scenario,
    level: LevelConfig,
    source: &'static str,
    expected_len: usize,
    emit_reports: bool,
) {
    let benchmark_name = format!(
        "decompress/{}/{}/{}/matrix",
        level.name, scenario.id, source
    );
    let mut group = c.benchmark_group(benchmark_name);
    configure_group(&mut group, scenario, BenchOp::Decompress);
    group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

    // Compression of the input stream is the setup step for this group's
    // decode timings. Defer it into a OnceCell that materializes only when
    // at least one of `pure_rust`/`c_ffi` is selected by the active filter
    // — without this, `cargo bench -- --profile-time` with a tight filter
    // still paid the cost of compressing every (scenario, level, source)
    // combo upfront, swamping samply profiles with encode CPU samples and
    // hiding the decode hot path we actually wanted to inspect.
    let compressed = std::cell::OnceCell::<Vec<u8>>::new();
    let materialize = || -> &[u8] {
        compressed
            .get_or_init(|| {
                let bytes = match source {
                    "rust_stream" => rust_encode_to_vec(&scenario.bytes[..], &level),
                    "c_stream" => {
                        ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level, level.ldm)
                    }
                    other => panic!("bench_decompress_source: unknown source {other}"),
                };
                assert_decompress_matches_reference(scenario, &bytes, expected_len);
                bytes
            })
            .as_slice()
    };

    // Destinations come from the process-wide arena, so neither the source
    // that compressed this fixture nor the position of this group in the
    // matrix can decide where they land. Both arms of a group share a pair,
    // and taking that pair after compressing the fixture (a different encoder
    // per source) once read as a threefold difference between the
    // implementations on frames that are byte-identical between the sources.
    //
    // Asked for with WILDCOPY_OVERLENGTH slack so `decode_all` routes through
    // the direct-write path; the slack is the dispatcher's eligibility gate.
    // The C arm asks for the same, so a size difference cannot separate them.
    let destination_len = expected_len + structured_zstd::WILDCOPY_OVERLENGTH;

    // The paired comparison, which is what the dashboard reports. It runs
    // before the criterion arms so that neither arm's own measurement can
    // colour it, and it uses the same buffers and the same steady-state shape
    // (one decoder, one DCtx, reused across iterations) as the arms below.
    if emit_reports {
        let compressed = materialize();
        let paired = DECODERS.with_borrow_mut(|decoders| {
            let BenchDecoders { rust, ffi } = decoders;
            ARENA.with_borrow_mut(|arena| {
                let mut targets = arena.written_slots(destination_len);
                measure_pair(|arm, slot| {
                    let target = &mut *targets[slot];
                    let written = match arm {
                        Arm::Rust => rust.decode_all(black_box(compressed), target).unwrap(),
                        Arm::Ffi => ffi.decompress_into(black_box(compressed), target),
                    };
                    black_box(&target[..written]);
                })
            })
        });
        emit_pair_report("decompress", scenario, level.name, source, paired);
    }

    bench_arm_pair(
        &mut group,
        "pure_rust",
        |b| {
            let compressed = materialize();
            DECODERS.with_borrow_mut(|decoders| {
                ARENA.with_borrow_mut(|arena| {
                    let target = arena.written(Arm::Rust, destination_len);
                    b.iter(|| {
                        let written = decoders
                            .rust
                            .decode_all(black_box(compressed), target)
                            .unwrap();
                        black_box(&target[..written]);
                        assert_eq!(written, expected_len);
                    })
                })
            })
        },
        "c_ffi",
        |b| {
            let compressed = materialize();
            // The DCtx is reused across iterations, as the other arm reuses its
            // decoder: a fresh one per iteration would dominate sub-millisecond
            // samples. Both are held for the process, so neither arm's working
            // memory is placed by what the matrix did before this cell.
            DECODERS.with_borrow_mut(|decoders| {
                ARENA.with_borrow_mut(|arena| {
                    let target = arena.written(Arm::Ffi, destination_len);
                    b.iter(|| {
                        let written = decoders.ffi.decompress_into(black_box(compressed), target);
                        assert_eq!(written, expected_len);
                        black_box(&target[..written]);
                    })
                })
            })
        },
    );

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
    let ffi_written = ffi_decompress_into(compressed, &mut ffi_target);
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
        // FastCOVER (both ours and FFI's) needs MULTIPLE training
        // samples to find cross-sample redundancy — passing the
        // whole scenario as a single slice (the previous shape
        // `[scenario.bytes.as_slice()]`) was rejected by FFI as
        // "samples=1, training failed" for every scenario, which
        // skipped the dictionary loop entirely (no compress-dict /
        // decompress-dict bench ever ran). `build_training_samples`
        // mirrors `training_sample_count` exactly — same
        // chunking AND the 2-sample midpoint-split fallback for
        // tiny inputs where chunking yields only 1 sample. Without
        // the fallback, small scenarios would still hit `samples=1`
        // on the FFI side and `ffi_samples.len()` would diverge
        // from `sample_count` (the value reported in BENCH_WARN).
        let ffi_samples = build_training_samples(scenario.bytes.as_slice());
        // Lockstep gate: `ffi_samples.len()` is what gets reported in
        // BENCH_WARN below and `sample_count` is what the
        // `training_sample_count` helper computes — both walk the
        // same chunking + 2-sample fallback ladder. Future drift in
        // either helper would silently desync the diagnostic
        // numbers; debug_assert catches that during development with
        // zero cost in optimised bench runs.
        debug_assert_eq!(
            ffi_samples.len(),
            sample_count,
            "build_training_samples and training_sample_count diverged for {}",
            scenario.id
        );
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
        // Diagnostic: dump the exact trained dict per scenario so the
        // sequence comparator can diff against the SAME dict the
        // compress-dict bench/REPORT use. Gated on an env var so normal
        // bench runs are unaffected.
        if let Ok(dir) = std::env::var("STRUCTURED_ZSTD_DUMP_DICT_DIR") {
            // Diagnostic artifacts are non-critical: warn and keep benching on
            // I/O failure instead of aborting the whole run.
            let path = format!("{dir}/{}.dict", scenario.id);
            if let Err(err) = std::fs::write(&path, &ffi_dictionary) {
                eprintln!("BENCH_WARN failed to dump dict {path}: {err}");
            }
            // Scenario input bytes too, so standalone profiling binaries
            // (`encode_loop_dict` / `decode_loop_dict`) can replay the
            // exact (input, dict) pair this scenario benches.
            let path = format!("{dir}/{}.bin", scenario.id);
            if let Err(err) = std::fs::write(&path, scenario.bytes.as_slice()) {
                eprintln!("BENCH_WARN failed to dump scenario bytes {path}: {err}");
            }
        }

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
        configure_group(&mut group, scenario, BenchOp::Compress);
        group.throughput(Throughput::Bytes(total_training_bytes as u64));

        // The paired comparison, which is what the dashboard reports. Dictionary
        // training allocates inside the measured work on both sides, so unlike
        // the other groups there is no shared buffer to take from the arena;
        // what the pairing buys here is the same window for both sides.
        if emit_reports {
            let paired = measure_pair(|arm, _slot| match arm {
                Arm::Rust => {
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
                }
                Arm::Ffi => {
                    black_box(
                        zstd::dict::from_samples(&ffi_samples, dict_size)
                            .expect("ffi dictionary training should succeed")
                            .len(),
                    );
                }
            });
            emit_pair_report("dict-train", scenario, "na", "na", paired);
        }

        bench_arm_pair(
            &mut group,
            "pure_rust",
            |b| {
                release_freed_memory();
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
            },
            "c_ffi",
            |b| {
                release_freed_memory();
                b.iter(|| {
                    black_box(
                        zstd::dict::from_samples(&ffi_samples, dict_size)
                            .expect("ffi dictionary training should succeed")
                            .len(),
                    )
                })
            },
        );

        group.finish();

        // Pre-parse the `DictionaryHandle` once per scenario. The
        // handle depends only on `ffi_dictionary` (which is fixed
        // across levels), so parsing it inside the per-level loop
        // below would redo the same work N times AND emit the same
        // BENCH_WARN per level if it ever failed. If parsing fails
        // we still want the existing `compress-dict/...` groups to
        // run, so we skip ONLY the `decompress-dict/...` groups
        // (handle stays `None` and the per-level decompress
        // branch falls through).
        let rust_dict_handle = match structured_zstd::decoding::DictionaryHandle::decode_dict(
            ffi_dictionary.as_slice(),
        ) {
            Ok(handle) => Some(handle),
            Err(err) => {
                eprintln!(
                    "BENCH_WARN skipping decompress-dict for scenario {} (failed to parse FFI dict bytes into a Rust DictionaryHandle: {err:?})",
                    scenario.id
                );
                None
            }
        };

        for level in supported_levels_filtered() {
            // The pure-LDM-no-dict variants (`*_ldm`, `dict = false`) belong to
            // the plain compress/decompress groups, not the dictionary group.
            // Everything else runs here: the numeric levels (unchanged
            // behaviour) and the `*_ldm_dict` variants (`dict = true`), the
            // latter with LDM enabled on both sides via `configure_ffi_bulk_compressor` /
            // `ldm_parameters` below.
            if level.ldm && !level.dict {
                continue;
            }
            let mut no_dict = zstd::bulk::Compressor::new(level.ffi_level).unwrap();
            configure_ffi_bulk_compressor(&mut no_dict, &level);
            let mut with_dict =
                zstd::bulk::Compressor::with_dictionary(level.ffi_level, &ffi_dictionary).unwrap();
            configure_ffi_bulk_compressor(&mut with_dict, &level);
            let no_dict_bytes = no_dict.compress(&scenario.bytes).unwrap();
            let with_dict_bytes = with_dict.compress(&scenario.bytes).unwrap();

            // Diagnostic: dump the FFI dict-encoded payload next to the
            // trained dict (same env gate) so standalone profiling binaries
            // (`decode_loop_dict`) can decode the EXACT bytes the
            // `decompress-dict/...` bench arm measures.
            if let Ok(dir) = std::env::var("STRUCTURED_ZSTD_DUMP_DICT_DIR") {
                // Non-critical diagnostic: warn, do not abort the bench.
                let path = format!("{dir}/{}.{}.zst", scenario.id, level.name);
                if let Err(err) = std::fs::write(&path, &with_dict_bytes) {
                    eprintln!("BENCH_WARN failed to dump dict payload {path}: {err}");
                }
            }

            // Rust dict-compressed output size, for the compress-dict
            // compression-ratio report (rust vs FFI). Only computable when the
            // dictionary parsed into a Rust handle; mirrors the gate the
            // `pure_rust_with_dict` timing arm uses below. Reused as the
            // timing-loop preallocation hint so we compress once, not twice.
            let rust_with_dict_len: Option<usize> = if rust_dict_handle.is_some() {
                let mut warmup_compressor = FrameCompressor::new(level.rust_level);
                // Enable LDM before attaching the dictionary — `set_parameters`
                // resets the base level + installs the LDM override; the dict
                // attach below is independent and survives it.
                if let Some(params) = ldm_parameters(&level) {
                    warmup_compressor.set_parameters(&params);
                }
                // Full feature gate: checksum on, matching the FFI arms.
                warmup_compressor.set_content_checksum(cfg!(feature = "hash"));
                warmup_compressor
                    .set_dictionary_from_bytes(&ffi_dictionary)
                    .expect("dictionary should attach");
                warmup_compressor.set_source_size_hint(scenario.bytes.len() as u64);
                warmup_compressor.set_source(scenario.bytes.as_slice());
                let mut warmup_output = Vec::new();
                warmup_compressor.set_drain(&mut warmup_output);
                warmup_compressor.compress();
                Some(warmup_output.len())
            } else {
                None
            };

            if emit_reports {
                emit_dictionary_report(
                    scenario,
                    level,
                    ffi_dictionary.len(),
                    ffi_train_ms,
                    &no_dict_bytes,
                    &with_dict_bytes,
                    // When the Rust dict path is unavailable, report 0 (the
                    // CI parser treats a 0 rust size as "no rust dict ratio").
                    rust_with_dict_len.unwrap_or(0),
                );
            }

            let benchmark_name =
                format!("compress-dict/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, scenario, BenchOp::Compress);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            // No-dict baseline for this level/scenario is already measured by the
            // plain `compress/{level}/{scenario}/matrix/c_ffi` group, so this
            // dict group only times the two dictionary arms.
            //
            // c_ffi_with_dict + pure_rust_with_dict: STEADY-STATE measurement.
            // Both build the compressor + attach the dictionary ONCE before
            // `b.iter`, then compress in the loop reusing that context — the
            // real prepared-dictionary lifecycle (C `CDict` created once +
            // `ZSTD_compress_usingCDict` per frame; Rust prepared
            // `EncoderDictionary` + `compress_independent_frame` per frame).
            // The earlier per-iter shape (fresh compressor + dict parse every
            // iteration) measured one-time setup cost, not steady-state
            // throughput, and unfairly penalised the side with heavier
            // per-attach setup. `compress_independent_frame_into` reads the
            // input in place and takes the output buffer per call, so it needs
            // no `set_drain`/`set_source` — sidestepping the lifetime issue
            // (PR #277) that forced the old per-iter shape.
            // One output buffer per arm across iterations, from the arena:
            // `compress` would hand back a fresh `Vec` every time and measure
            // that side's allocator rather than its encoder. Sizing them per
            // arm gave the C arm the compression bound and ours only what our
            // side happened to produce, so the arms differed both in where
            // their pages came from and in how much spare capacity they
            // carried. The arena holds the bound, which is what
            // `compress_to_buffer` requires.
            //
            // Whether the Rust arm can run at all is settled BEFORE either arm
            // is registered, so that when both exist they alternate like every
            // other pair. Registering the C arm unconditionally and the Rust
            // arm behind the gate put the C arm first in every round, and the
            // per-arm minimum across rounds cannot cancel a position bias when
            // neither arm ever occupies the other position. The gate matches
            // the one `decompress-dict` uses below: if the per-scenario
            // dictionary parse failed, `EncoderDictionary::from_bytes` routes
            // through the same parse and would fail identically, and an
            // `.expect()` panic before `b.iter` would abort the whole suite.
            let ffi_dict_arm =
                |b: &mut criterion::Bencher<'_, criterion::measurement::WallTime>| {
                    let mut compressor =
                        zstd::bulk::Compressor::with_dictionary(level.ffi_level, &ffi_dictionary)
                            .unwrap();
                    configure_ffi_bulk_compressor(&mut compressor, &level);
                    ARENA.with_borrow_mut(|arena| {
                        let compressed = arena.appended(Arm::Ffi);
                        b.iter(|| {
                            compressed.clear();
                            compressor
                                .compress_to_buffer(&scenario.bytes, compressed)
                                .expect("dictionary compression should succeed");
                            black_box(&compressed);
                        })
                    })
                };
            let rust_dict_arm = |b: &mut criterion::Bencher<
                '_,
                criterion::measurement::WallTime,
            >| {
                // `compress_independent_frame_into` reads input in place +
                // takes the output buffer per call, so neither the source `R`
                // nor drain `W` generic is ever bound by a
                // `set_source`/`set_drain` call — pin them to the defaults so
                // inference has a concrete type.
                let mut compressor: FrameCompressor = FrameCompressor::new(level.rust_level);
                // Enable LDM before attaching the dictionary (see the warmup
                // compressor above for why the order is safe).
                if let Some(params) = ldm_parameters(&level) {
                    compressor.set_parameters(&params);
                }
                // Full feature gate: checksum on, matching the FFI arms.
                compressor.set_content_checksum(cfg!(feature = "hash"));
                compressor
                    .set_encoder_dictionary(
                        EncoderDictionary::from_bytes(&ffi_dictionary)
                            .expect("dictionary parse checked before registration"),
                    )
                    .expect("prepared dictionary should attach");
                // Reuse one output buffer across iterations (the
                // CCtx-equivalent caller-owned `dst`), from the same arena the
                // C arm draws from, so neither reallocates and neither depends
                // on what ran before it.
                ARENA.with_borrow_mut(|arena| {
                    let compressed = arena.appended(Arm::Rust);
                    b.iter(|| {
                        compressor
                            .compress_independent_frame_into(scenario.bytes.as_slice(), compressed);
                        black_box(&compressed);
                    })
                })
            };

            let rust_dict_available = rust_with_dict_len.is_some()
                && EncoderDictionary::from_bytes(&ffi_dictionary).is_ok();

            // The paired comparison, which is what the dashboard reports. Only
            // possible when both sides have a dictionary to compress with.
            if emit_reports && rust_dict_available {
                let mut ffi_compressor =
                    zstd::bulk::Compressor::with_dictionary(level.ffi_level, &ffi_dictionary)
                        .unwrap();
                configure_ffi_bulk_compressor(&mut ffi_compressor, &level);
                let mut rust_compressor: FrameCompressor = FrameCompressor::new(level.rust_level);
                if let Some(params) = ldm_parameters(&level) {
                    rust_compressor.set_parameters(&params);
                }
                rust_compressor.set_content_checksum(cfg!(feature = "hash"));
                rust_compressor
                    .set_encoder_dictionary(
                        EncoderDictionary::from_bytes(&ffi_dictionary)
                            .expect("dictionary parse checked above"),
                    )
                    .expect("prepared dictionary should attach");
                let paired = ARENA.with_borrow_mut(|arena| {
                    let mut outputs = arena.appended_slots();
                    measure_pair(|arm, slot| {
                        let output = &mut *outputs[slot];
                        match arm {
                            Arm::Rust => rust_compressor
                                .compress_independent_frame_into(scenario.bytes.as_slice(), output),
                            Arm::Ffi => {
                                output.clear();
                                ffi_compressor
                                    .compress_to_buffer(&scenario.bytes, output)
                                    .expect("dictionary compression should succeed");
                            }
                        }
                        black_box(&output);
                    })
                });
                emit_pair_report("compress-dict", scenario, level.name, "na", paired);
            }

            if rust_dict_available {
                bench_arm_pair(
                    &mut group,
                    "pure_rust_with_dict",
                    rust_dict_arm,
                    "c_ffi_with_dict",
                    ffi_dict_arm,
                );
            } else {
                group.bench_function("c_ffi_with_dict", ffi_dict_arm);
            }

            group.finish();

            // decompress-dict: measure steady-state decode throughput
            // for a dictionary-driven .zst payload, both sides. Uses
            // `with_dict_bytes` (the FFI dict-encoded payload above) as
            // the fixed input on both branches so the throughput
            // metric is apples-to-apples (Rust and FFI decode the SAME
            // bytes — what differs is the decoder implementation).
            // Dictionary parsing AND `FrameDecoder` / FFI `Decompressor`
            // construction are hoisted out of `b.iter` (the
            // per-scenario `rust_dict_handle` parse above, and the
            // per-bench-function `decoder` / `decompressor` set up once
            // before the timing loop) so the numbers reflect the
            // hot-path decode kernel rather than per-frame setup.
            //
            // CRITICAL: `with_dict_bytes` was compressed using
            // `ffi_dictionary` (`Compressor::with_dictionary(level,
            // &ffi_dictionary)` above), so the decoder MUST hold the
            // SAME dictionary bytes — the inner zstd frame's `dict_id`
            // header is derived from those bytes. Parsing the handle
            // from `rust_dictionary` (different bytes, different
            // `dict_id`) would fail one of two ways:
            //   - if the frame header carries a `dict_id`,
            //     `decode_all_with_dict_handle` returns
            //     `DictIdMismatch { expected, got }` (see
            //     `frame_decoder.rs::reset_with_dict_handle`);
            //   - if the encoder omitted the `dict_id` (some configs do),
            //     decode would SILENTLY corrupt the output by applying
            //     the wrong reference bytes, which the FFI side would
            //     mirror with `decompress_to_buffer` quietly producing
            //     garbage — even worse than a clean error.
            // Either way the bench would measure the wrong path.
            let Some(rust_dict_handle) = rust_dict_handle.as_ref() else {
                continue;
            };
            let expected_len = scenario.bytes.len();
            let decompress_dict_name = format!(
                "decompress-dict/{}/{}/{}",
                level.name, scenario.id, "matrix"
            );
            let mut group = c.benchmark_group(decompress_dict_name);
            configure_group(&mut group, scenario, BenchOp::Decompress);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            // One-time byte-equality verification BEFORE the bench loops.
            // `decode_all_with_dict_handle` explicitly warns that decoding
            // with the wrong dictionary produces silently-corrupt output
            // (no error), so verifying once against `scenario.bytes`
            // outside the timing sample catches a desynced
            // (rust_dict_handle, with_dict_bytes) pairing before it would
            // silently inflate or deflate throughput numbers. FFI side
            // gets the same treatment for parity. Matches the upstream zstd shape
            // used by `bench_decompress_source` →
            // `assert_decompress_matches_reference`.
            {
                let mut verify_decoder = FrameDecoder::new();
                let mut verify_out = vec![0u8; expected_len];
                let n = verify_decoder
                    .decode_all_with_dict_handle(
                        with_dict_bytes.as_slice(),
                        verify_out.as_mut_slice(),
                        rust_dict_handle,
                    )
                    .expect("rust decode-with-dict verification must succeed");
                assert_eq!(n, expected_len, "rust dict decode wrote a partial output");
                assert_eq!(
                    &verify_out[..n],
                    scenario.bytes.as_slice(),
                    "rust dict decode bytes diverge from scenario reference",
                );

                let mut verify_decompressor =
                    zstd::bulk::Decompressor::with_dictionary(&ffi_dictionary)
                        .expect("ffi dict verification: with_dictionary");
                let mut verify_out_ffi = vec![0u8; expected_len];
                let nf = verify_decompressor
                    .decompress_to_buffer(with_dict_bytes.as_slice(), verify_out_ffi.as_mut_slice())
                    .expect("ffi decode-with-dict verification must succeed");
                assert_eq!(nf, expected_len, "ffi dict decode wrote a partial output");
                assert_eq!(
                    &verify_out_ffi[..nf],
                    scenario.bytes.as_slice(),
                    "ffi dict decode bytes diverge from scenario reference",
                );
            }

            // The paired comparison, which is what the dashboard reports.
            if emit_reports {
                let mut decoder = FrameDecoder::new();
                let mut decompressor =
                    zstd::bulk::Decompressor::with_dictionary(&ffi_dictionary).unwrap();
                let paired = ARENA.with_borrow_mut(|arena| {
                    let mut outputs = arena.written_slots(expected_len);
                    measure_pair(|arm, slot| {
                        let output = &mut *outputs[slot];
                        let n = match arm {
                            Arm::Rust => decoder
                                .decode_all_with_dict_handle(
                                    black_box(with_dict_bytes.as_slice()),
                                    output,
                                    rust_dict_handle,
                                )
                                .expect("rust decode-with-dict must succeed"),
                            Arm::Ffi => decompressor
                                .decompress_to_buffer(black_box(with_dict_bytes.as_slice()), output)
                                .expect("ffi decode-with-dict must succeed"),
                        };
                        black_box(&output[..n]);
                    })
                });
                emit_pair_report("decompress-dict", scenario, level.name, "na", paired);
            }

            // Outputs from the arena, like every other group: an arm that takes
            // its own gets pages from a heap the other arm never saw.
            bench_arm_pair(
                &mut group,
                "pure_rust_with_dict",
                |b| {
                    let mut decoder = FrameDecoder::new();
                    ARENA.with_borrow_mut(|arena| {
                        let output = arena.written(Arm::Rust, expected_len);
                        b.iter(|| {
                            let n = decoder
                                .decode_all_with_dict_handle(
                                    black_box(with_dict_bytes.as_slice()),
                                    output,
                                    rust_dict_handle,
                                )
                                .expect("rust decode-with-dict must succeed");
                            assert_eq!(n, expected_len, "rust decode wrote a partial output");
                            black_box(&output[..n]);
                        })
                    })
                },
                "c_ffi_with_dict",
                |b| {
                    let mut decompressor =
                        zstd::bulk::Decompressor::with_dictionary(&ffi_dictionary).unwrap();
                    ARENA.with_borrow_mut(|arena| {
                        let output = arena.written(Arm::Ffi, expected_len);
                        b.iter(|| {
                            let n = decompressor
                                .decompress_to_buffer(black_box(with_dict_bytes.as_slice()), output)
                                .expect("ffi decode-with-dict must succeed");
                            assert_eq!(n, expected_len, "ffi decode wrote a partial output");
                            black_box(&output[..n]);
                        })
                    })
                },
            );

            group.finish();
        }
    }
}

/// Whether a bench group times compression or decompression. The two
/// operations sit at opposite ends of the per-iter cost / variance curve, so
/// they take different `measurement_time` budgets on the same scenario class.
#[derive(Clone, Copy)]
enum BenchOp {
    Compress,
    Decompress,
}

fn configure_group<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
    scenario: &Scenario,
    op: BenchOp,
) {
    // CI wall-time tuning (#164, #362):
    //
    // criterion 0.8 hard-asserts `sample_size >= 10` (`benchmark_group.rs:97`
    // / `lib.rs:519`). The floor is set in source and cannot be lowered
    // without forking criterion, so we tune `measurement_time` /
    // `warm_up_time` to cut per-bench wall-clock instead.
    //
    // criterion's `SamplingMode::Flat` FILLS `measurement_time` with
    // iterations at a fixed `sample_size`: any op faster than
    // `measurement_time / sample_size` is over-iterated. Cost per
    // `bench_function` ≈ `max(measurement_time, sample_size × per_iter)`.
    //
    // Compress and decompress sit at opposite ends of that curve, so #362
    // split the budget by `op` (the `bench_compress` / `bench_decompress`
    // call sites already pass it):
    //   - compress at high levels is SLOW and is the regression signal we
    //     protect: z000033 L22 ≈ 294 ms/iter (10 samples ≈ 2.9 s, right at
    //     the Corpus budget); 100 MiB L22 compress on i686 ≈ ~1 s/iter
    //     (≈ 10 s+ wall) — the reason Large compress keeps the 20 s budget,
    //     below which criterion's "increase target time" warning returns.
    //   - decompress is FAST and extremely low-variance (CI ±0.1–0.5 %):
    //     100 MiB decode ≈ ~11 ms/iter, 1 KiB ≈ 147 ns. Sharing the compress
    //     budget made Large decode burn 20 s on an 11 ms op (~1300 iters per
    //     sample — pure overbench). It now gets a much smaller budget: still
    //     hundreds+ of iters, CI stays < 1 %, no precision loss against the
    //     dashboard's regression thresholds.
    //
    // `sample_size` (10 / 30) and `warm_up_time` are unchanged: the sample
    // floor is criterion's statistical minimum, and the 30-sample Small count
    // amortises timer resolution for ns-scale decode.
    let measurement = match (scenario.class, op) {
        (ScenarioClass::Small, BenchOp::Compress) => Duration::from_millis(500),
        (ScenarioClass::Small, BenchOp::Decompress) => Duration::from_millis(300),
        // z000033 L22 compress (~0.3 s/iter on i9-x86_64) takes ≈20 s for 10
        // samples on a GitHub free runner (~6-8x slower), so it emits a benign
        // "increase target time" notice under any budget below ~22 s — a
        // pre-existing, CI-only cosmetic warning (it still collects all 10
        // samples). Raising the budget to silence it would only inflate the
        // faster corpus-compress levels (they would fill the larger budget)
        // without helping the slow level, so Corpus compress stays at the 3 s
        // tier alongside Entropy.
        (ScenarioClass::Corpus | ScenarioClass::Entropy, BenchOp::Compress) => {
            Duration::from_secs(3)
        }
        (ScenarioClass::Corpus | ScenarioClass::Entropy, BenchOp::Decompress) => {
            Duration::from_millis(1500)
        }
        (ScenarioClass::Large | ScenarioClass::Silesia, BenchOp::Compress) => {
            Duration::from_secs(20)
        }
        (ScenarioClass::Large | ScenarioClass::Silesia, BenchOp::Decompress) => {
            Duration::from_secs(3)
        }
    };
    // A whole-matrix sweep across every level and fixture is dominated by the
    // Large budget above, which is sized for CI's regression signal rather than
    // for a local A/B of two revisions. `STRUCTURED_ZSTD_BENCH_MAX_SECS` caps
    // every budget so such a sweep finishes; both sides of an A/B must be run
    // with the same value, and the dashboard leaves it unset.
    let measurement = match max_measurement_secs() {
        Some(cap) if measurement > cap => cap,
        _ => measurement,
    };
    let (samples, warm_up) = match scenario.class {
        ScenarioClass::Small => (30, Duration::from_millis(200)),
        _ => (10, Duration::from_millis(500)),
    };
    // Split both budgets across the rounds so R rounds cost what one round of
    // the full budget costs. Warm-up keeps a floor: each round re-enters the
    // code cold, and a warm-up too short to reach steady state would put the
    // ramp inside the measurement it precedes.
    let rounds = bench_rounds();
    let measurement = measurement / rounds;
    let warm_up = (warm_up / rounds).max(Duration::from_millis(50));
    group.sample_size(samples);
    group.measurement_time(measurement);
    group.warm_up_time(warm_up);
    group.sampling_mode(SamplingMode::Flat);
}

/// Hand memory the previous group freed back to the kernel, so every group
/// starts from the state a fresh process would have.
///
/// Without this a group inherits whatever the group before it released, and a
/// level that allocates tens of megabytes of tables hands the next level a warm
/// heap. One level-16 row read 4.69 ms inside a sweep and 18.99 ms on its own,
/// and which side of a comparison receives that gift depends on what its
/// PREVIOUS level happened to free — a property of the run order, not of the
/// code being measured, and the reason a row could drift fourfold between
/// sweeps.
///
/// Once per ARM, not once per group: within a group the first arm runs first
/// and frees everything it allocated, so the second one would inherit a heap the
/// first one warmed — which is exactly the run-order effect this removes, only
/// now between the two sides of the comparison rather than between levels. A
/// trim per arm is still far below anything a measurement can see; the
/// per-iteration loop never touches it.
fn release_freed_memory() {
    // glibc only: `malloc_trim` is a GNU extension, and musl neither provides
    // it nor keeps the per-arena free lists it exists to return. Declared here
    // rather than pulled from a binding crate — one symbol with a trivial
    // signature is not worth a workspace dependency, and it keeps this harness
    // droppable into an older revision for a paired measurement.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            // `int malloc_trim(size_t pad)` — the argument is a size, not an
            // int, and declaring it narrower is wrong however the two happen to
            // be passed on this ABI.
            fn malloc_trim(pad: usize) -> core::ffi::c_int;
        }
        // SAFETY: takes no pointers and only returns free pages to the kernel;
        // nothing live is touched.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Ceiling on every criterion measurement budget, in seconds, or `None` when
/// `STRUCTURED_ZSTD_BENCH_MAX_SECS` is unset or unparseable.
fn max_measurement_secs() -> Option<Duration> {
    std::env::var("STRUCTURED_ZSTD_BENCH_MAX_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
}

/// How many times the whole matrix will be run, from
/// `STRUCTURED_ZSTD_BENCH_ROUNDS` (default 1).
///
/// The two arms of a group run one after the other, so a disturbance lasting
/// longer than one arm lands entirely on that arm and the ratio records it as
/// a difference between the implementations. Splitting the budget into rounds
/// puts the arms in `A B A B ...` with the rest of the matrix between each
/// pair, and the per-arm minimum across rounds is then free of any disturbance
/// that did not cover every round.
///
/// The MEASUREMENT budget is divided, not multiplied, so most of the cost is
/// carried over rather than added. Two things do not divide: each round repeats
/// its group setup, and the per-round warm-up has a floor, so the warm-up total
/// grows once `budget / rounds` falls under it. Three rounds measured about a
/// fifth longer than one full-budget round on a decompress group; a round count
/// high enough to sit on the warm-up floor costs proportionally more than that.
fn bench_rounds() -> u32 {
    std::env::var("STRUCTURED_ZSTD_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|rounds| *rounds > 0)
        .unwrap_or(1)
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

fn emit_block_structure_report(
    scenario: &Scenario,
    level: LevelConfig,
    encoder: &'static str,
    compressed: &[u8],
) {
    if compressed.len() < 5 {
        return;
    }
    // Raw hex of the frame head: lets us read the literals-section header and
    // Huffman tree description (FSE weights vs direct nibbles) by hand when
    // comparing rust vs ffi generation byte-for-byte on a fixture.
    let head_len = compressed.len().min(48);
    let hex: String = compressed[..head_len]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "REPORT_HEX scenario={} level={} encoder={} head=[{}]",
        scenario.id, level.name, encoder, hex
    );
    let desc = compressed[4];
    let fcs_flag = desc >> 6;
    let single_segment = ((desc >> 5) & 0x1) == 1;
    let checksum = ((desc >> 2) & 0x1) == 1;
    let dict_id_bytes: usize = match desc & 0x3 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let fcs_bytes: usize = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut pos = 4 + 1 + usize::from(!single_segment) + dict_id_bytes + fcs_bytes;
    let payload_end = compressed.len() - if checksum { 4 } else { 0 };
    // A truncated/invalid frame must report `parse=error`, not fall through to a
    // normal REPORT_BLK that would make it look like a valid empty/partial frame.
    if pos > payload_end {
        println!(
            "REPORT_BLK scenario={} level={} encoder={} parse=error",
            scenario.id, level.name, encoder
        );
        return;
    }
    let mut blocks: Vec<(char, usize, bool)> = Vec::new();
    let mut saw_last = false;
    while pos + 3 <= payload_end {
        let h = compressed[pos] as usize
            | (compressed[pos + 1] as usize) << 8
            | (compressed[pos + 2] as usize) << 16;
        let last = (h & 1) == 1;
        let btype = (h >> 1) & 0x3;
        let bsize = h >> 3;
        let (kind, advance) = match btype {
            0 => ('R', bsize), // Raw
            1 => ('L', 1),     // RLE (1 byte payload)
            2 => ('C', bsize), // Compressed
            _ => {
                println!(
                    "REPORT_BLK scenario={} level={} encoder={} parse=error",
                    scenario.id, level.name, encoder
                );
                return;
            }
        };
        if pos + 3 + advance > payload_end {
            println!(
                "REPORT_BLK scenario={} level={} encoder={} parse=error",
                scenario.id, level.name, encoder
            );
            return;
        }
        blocks.push((kind, bsize, last));
        pos += 3 + advance;
        if last {
            saw_last = true;
            break;
        }
    }
    // A clean frame ends exactly on a terminal (`last`) block with no leftover
    // bytes; otherwise it was truncated (e.g. 1-2 trailing bytes that cannot
    // form a block header) and must report a parse error, not a block list.
    if !saw_last || pos != payload_end {
        println!(
            "REPORT_BLK scenario={} level={} encoder={} parse=error",
            scenario.id, level.name, encoder
        );
        return;
    }
    let list: Vec<String> = blocks
        .iter()
        .map(|(k, s, l)| format!("{k}{s}{}", if *l { "!" } else { "" }))
        .collect();
    println!(
        "REPORT_BLK scenario={} level={} encoder={} n_blocks={} blocks=[{}]",
        scenario.id,
        level.name,
        encoder,
        blocks.len(),
        list.join(","),
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
    rust_with_dict_len: usize,
) {
    let input_len = scenario.len() as f64;
    let escaped_label = escape_report_label(&scenario.label);
    let (no_dict_ratio, with_dict_ratio, rust_with_dict_ratio) = if input_len > 0.0 {
        (
            no_dict_bytes.len() as f64 / input_len,
            with_dict_bytes.len() as f64 / input_len,
            rust_with_dict_len as f64 / input_len,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    // `rust_with_dict_bytes` / `rust_with_dict_ratio` are appended after the
    // FFI fields so existing parsers that only read the leading columns keep
    // working; the CI report parser's REPORT_DICT regex captures the two new
    // trailing fields to emit a compress-dict compression-ratio series.
    println!(
        "REPORT_DICT scenario={} label=\"{}\" level={} dict_bytes={} train_ms={:.3} ffi_no_dict_bytes={} ffi_with_dict_bytes={} ffi_no_dict_ratio={:.6} ffi_with_dict_ratio={:.6} rust_with_dict_bytes={} rust_with_dict_ratio={:.6}",
        scenario.id,
        escaped_label,
        level.name,
        dict_bytes,
        train_ms,
        no_dict_bytes.len(),
        with_dict_bytes.len(),
        no_dict_ratio,
        with_dict_ratio,
        rust_with_dict_len,
        rust_with_dict_ratio
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
