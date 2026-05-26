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
use support::{
    LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, supported_levels_filtered,
};

static BENCHMARK_SCENARIOS: OnceLock<Vec<Scenario>> = OnceLock::new();

/// FFI encode helper used by criterion's timing loop. Uses
/// `ZSTD_compressStream2` into a growing-output `Vec` — same shape as
/// the pure-Rust `compress_to_vec` so output-buffer growth profiles
/// match cross-side.
fn ffi_encode_to_vec(input: &[u8], level: i32) -> Vec<u8> {
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

fn bench_compress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels_filtered() {
            if emit_reports {
                let rust_compressed = structured_zstd::encoding::compress_slice_to_vec(
                    &scenario.bytes[..],
                    level.rust_level,
                );
                let ffi_compressed = ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level);
                emit_report_line(scenario, level, &rust_compressed, &ffi_compressed);
                emit_frame_header_report(scenario, level, "rust", &rust_compressed);
                emit_frame_header_report(scenario, level, "ffi", &ffi_compressed);
            }

            let benchmark_name = format!("compress/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            group.bench_function("pure_rust", |b| {
                b.iter(|| {
                    black_box(structured_zstd::encoding::compress_slice_to_vec(
                        &scenario.bytes[..],
                        level.rust_level,
                    ))
                })
            });

            group.bench_function("c_ffi", |b| {
                b.iter(|| black_box(ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level)))
            });

            group.finish();
        }
    }
}

fn bench_decompress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels_filtered() {
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

fn bench_decompress_source(
    c: &mut Criterion,
    scenario: &Scenario,
    level: LevelConfig,
    source: &'static str,
    expected_len: usize,
    _emit_reports: bool,
) {
    let benchmark_name = format!(
        "decompress/{}/{}/{}/matrix",
        level.name, scenario.id, source
    );
    let mut group = c.benchmark_group(benchmark_name);
    configure_group(&mut group, scenario);
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
                    "rust_stream" => structured_zstd::encoding::compress_slice_to_vec(
                        &scenario.bytes[..],
                        level.rust_level,
                    ),
                    "c_stream" => ffi_encode_to_vec(&scenario.bytes[..], level.ffi_level),
                    other => panic!("bench_decompress_source: unknown source {other}"),
                };
                assert_decompress_matches_reference(scenario, &bytes, expected_len);
                bytes
            })
            .as_slice()
    };

    group.bench_function("pure_rust", |b| {
        let compressed = materialize();
        // Target sized with WILDCOPY_OVERLENGTH slack so `decode_all`
        // routes through the direct-write path (decode straight into
        // `target`, no FlatBuf drain copy). The slack is the
        // dispatcher's eligibility gate; without it the call falls
        // back to the legacy per-block drain loop. The auto-reserve
        // inside `decode_all_to_vec` provides the equivalent slack
        // transparently for Vec-based callers.
        let mut target = vec![0u8; expected_len + structured_zstd::WILDCOPY_OVERLENGTH];
        pretouch_pages(&mut target);
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
        let compressed = materialize();
        let mut dctx = FfiDCtxHandle::new();
        let mut target = vec![0u8; expected_len];
        pretouch_pages(&mut target);
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
            configure_group(&mut group, scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            // One-time byte-equality verification BEFORE the bench loops.
            // `decode_all_with_dict_handle` explicitly warns that decoding
            // with the wrong dictionary produces silently-corrupt output
            // (no error), so verifying once against `scenario.bytes`
            // outside the timing sample catches a desynced
            // (rust_dict_handle, with_dict_bytes) pairing before it would
            // silently inflate or deflate throughput numbers. FFI side
            // gets the same treatment for parity. Matches the donor shape
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

            group.bench_function("pure_rust_with_dict", |b| {
                let mut decoder = FrameDecoder::new();
                let mut output = vec![0u8; expected_len];
                b.iter(|| {
                    let n = decoder
                        .decode_all_with_dict_handle(
                            black_box(with_dict_bytes.as_slice()),
                            output.as_mut_slice(),
                            rust_dict_handle,
                        )
                        .expect("rust decode-with-dict must succeed");
                    assert_eq!(n, expected_len, "rust decode wrote a partial output");
                    black_box(&output[..n]);
                })
            });

            group.bench_function("c_ffi_with_dict", |b| {
                let mut decompressor =
                    zstd::bulk::Decompressor::with_dictionary(&ffi_dictionary).unwrap();
                let mut output = vec![0u8; expected_len];
                b.iter(|| {
                    let n = decompressor
                        .decompress_to_buffer(
                            black_box(with_dict_bytes.as_slice()),
                            output.as_mut_slice(),
                        )
                        .expect("ffi decode-with-dict must succeed");
                    assert_eq!(n, expected_len, "ffi decode wrote a partial output");
                    black_box(&output[..n]);
                })
            });

            group.finish();
        }
    }
}

fn configure_group<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
    scenario: &Scenario,
) {
    // CI wall-time tuning (#164):
    //
    // criterion 0.8 hard-asserts `sample_size >= 10` (`benchmark_group.rs:97`
    // / `lib.rs:519`). The floor is set in source and cannot be lowered
    // without forking criterion, so we tune `measurement_time` and
    // `warm_up_time` to cut per-bench wall-clock instead.
    //
    // Pre-tuning budget per `bench_function` (one side):
    //   Small:    3s measurement + 3s default warm-up = 6s
    //   Corpus/Entropy: 8s + 3s default warm-up      = 11s
    //   Large/Silesia:  10s + 0.5s warm-up           = 10.5s
    //
    // Each `pure_rust` / `c_ffi` pair doubles that. Across the 21
    // strategy shards × ~7 scenarios × 3 bench groups (compress,
    // decompress rust_stream, decompress c_stream) × 2 sides, the
    // worst shard (`lazy`, 11 levels) reached the 120-min CI cap.
    //
    // Post-tuning (criterion still gets >= 10 samples; only the
    // wall-clock budget shrinks where the measured per-iter is faster
    // than the budget — slow-per-iter benches are bound by
    // `samples × per_iter` regardless of budget):
    //   Small:    1s + 0.2s = 1.2s per side (×2 = 2.4s) — 60% cut
    //   Corpus/Entropy: 3s + 0.5s = 3.5s per side (×2 = 7s) — 68% cut
    //   Large/Silesia:  20s + 0.5s — bumped UP from 10s. The slowest
    //     combos on i686 (level_22_btultra2 / 100 MiB) need ~2 s per
    //     iter × 10 samples ≈ 20 s wall; the old 10 s budget produced
    //     persistent criterion "increase target time" warnings and
    //     occasional flaky measurements. Budget is dwarfed by the
    //     actual per-iter cost on slow combos, so this only widens the
    //     warning-free envelope — fast combos still finish under
    //     budget.
    //
    // For very small inputs (1-10 KiB) Small still keeps `sample_size(30)` to
    // amortise the per-sample fixed cost across more measurements — those
    // benches finish their 30 samples well inside 1 s thanks to tight
    // per-iter timings.
    match scenario.class {
        ScenarioClass::Small => {
            group.sample_size(30);
            group.measurement_time(Duration::from_secs(1));
            group.warm_up_time(Duration::from_millis(200));
            group.sampling_mode(SamplingMode::Flat);
        }
        ScenarioClass::Corpus | ScenarioClass::Entropy => {
            group.sample_size(10);
            group.measurement_time(Duration::from_secs(3));
            group.warm_up_time(Duration::from_millis(500));
            group.sampling_mode(SamplingMode::Flat);
        }
        ScenarioClass::Large | ScenarioClass::Silesia => {
            // Large/Silesia payloads (16-100 MiB) on slow targets
            // (i686 + level_22_btultra2) need ~2 s per iter ×
            // 10 samples ≈ 20 s wall. Old 10 s budget caused
            // "increase target time" warnings + occasional flakies;
            // widening to 20 s covers the slowest combo without
            // affecting wall on faster targets (criterion exits the
            // budget early when samples complete).
            group.sample_size(10);
            group.measurement_time(Duration::from_secs(20));
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
