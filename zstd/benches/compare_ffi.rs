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
use std::io::Cursor;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use structured_zstd::decoding::FrameDecoder;
use structured_zstd::dictionary::{FastCoverOptions, create_fastcover_raw_dict_from_source};
use support::{LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, supported_levels};

static BENCHMARK_SCENARIOS: OnceLock<Vec<Scenario>> = OnceLock::new();

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
        for level in supported_levels() {
            if emit_reports {
                let rust_compressed = structured_zstd::encoding::compress_to_vec(
                    &scenario.bytes[..],
                    level.rust_level,
                );
                let ffi_compressed =
                    zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap();
                emit_report_line(scenario, level, &rust_compressed, &ffi_compressed);
                emit_memory_report(
                    scenario,
                    level,
                    "compress",
                    scenario.len() + rust_compressed.len(),
                    scenario.len() + ffi_compressed.len(),
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
                    black_box(zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap())
                })
            });

            group.finish();
        }
    }
}

fn bench_decompress(c: &mut Criterion) {
    let emit_reports = emit_reports_enabled();
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels() {
            let rust_compressed =
                structured_zstd::encoding::compress_to_vec(&scenario.bytes[..], level.rust_level);
            let ffi_compressed = zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap();
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
        emit_memory_report(
            scenario,
            level,
            &format!("decompress-{source}"),
            compressed.len() + expected_len,
            compressed.len() + expected_len,
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

        let training_samples = split_training_samples(&scenario.bytes);
        let sample_refs: Vec<&[u8]> = training_samples.iter().map(Vec::as_slice).collect();
        let training_blob: Vec<u8> = training_samples.concat();
        let total_training_bytes = sample_refs.iter().map(|sample| sample.len()).sum::<usize>();
        let dict_size = dictionary_size_for(scenario.len())
            .min(total_training_bytes.saturating_sub(64))
            .max(256);
        let fastcover_options = fastcover_fixed_options();

        let rust_train_started = Instant::now();
        let mut rust_dictionary = Vec::new();
        let Ok(rust_tuned) = create_fastcover_raw_dict_from_source(
            Cursor::new(training_blob.as_slice()),
            &mut rust_dictionary,
            dict_size,
            &fastcover_options,
        ) else {
            eprintln!(
                "BENCH_WARN skipping Rust FastCOVER dictionary benchmark for {} (samples={}, total_training_bytes={}, dict_size={})",
                scenario.id,
                sample_refs.len(),
                total_training_bytes,
                dict_size
            );
            continue;
        };
        let rust_train_ms = rust_train_started.elapsed().as_secs_f64() * 1_000.0;

        let ffi_train_started = Instant::now();
        let Ok(ffi_dictionary) = zstd::dict::from_samples(&sample_refs, dict_size) else {
            eprintln!(
                "BENCH_WARN skipping dictionary benchmark for {} (samples={}, total_training_bytes={}, dict_size={})",
                scenario.id,
                sample_refs.len(),
                total_training_bytes,
                dict_size
            );
            continue;
        };
        let ffi_train_ms = ffi_train_started.elapsed().as_secs_f64() * 1_000.0;

        if emit_reports {
            emit_dictionary_training_report(
                scenario,
                dict_size,
                rust_train_ms,
                ffi_train_ms,
                rust_dictionary.len(),
                ffi_dictionary.len(),
                rust_tuned.score,
            );
        }

        let benchmark_name = format!("dict-train/na/{}/{}", scenario.id, "matrix");
        let mut group = c.benchmark_group(benchmark_name);
        configure_group(&mut group, scenario);
        group.throughput(Throughput::Bytes(total_training_bytes as u64));

        group.bench_function("pure_rust", |b| {
            b.iter(|| {
                let mut out = Vec::new();
                let tuned = create_fastcover_raw_dict_from_source(
                    Cursor::new(training_blob.as_slice()),
                    &mut out,
                    dict_size,
                    &fastcover_options,
                )
                .expect("fastcover training should succeed");
                black_box((out.len(), tuned.score));
            })
        });

        group.bench_function("c_ffi", |b| {
            b.iter(|| {
                black_box(
                    zstd::dict::from_samples(&sample_refs, dict_size)
                        .expect("ffi dictionary training should succeed")
                        .len(),
                )
            })
        });

        group.finish();

        for level in supported_levels() {
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

fn emit_memory_report(
    scenario: &Scenario,
    level: LevelConfig,
    stage: &str,
    rust_buffer_bytes_estimate: usize,
    ffi_buffer_bytes_estimate: usize,
) {
    let escaped_label = escape_report_label(&scenario.label);
    println!(
        "REPORT_MEM scenario={} label=\"{}\" level={} stage={} rust_buffer_bytes_estimate={} ffi_buffer_bytes_estimate={}",
        scenario.id,
        escaped_label,
        level.name,
        stage,
        rust_buffer_bytes_estimate,
        ffi_buffer_bytes_estimate
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

fn emit_dictionary_training_report(
    scenario: &Scenario,
    dict_bytes_requested: usize,
    rust_train_ms: f64,
    ffi_train_ms: f64,
    rust_dict_bytes: usize,
    ffi_dict_bytes: usize,
    rust_fastcover_score: usize,
) {
    let escaped_label = escape_report_label(&scenario.label);
    println!(
        "REPORT_DICT_TRAIN scenario={} label=\"{}\" dict_bytes_requested={} rust_train_ms={:.3} ffi_train_ms={:.3} rust_dict_bytes={} ffi_dict_bytes={} rust_fastcover_score={}",
        scenario.id,
        escaped_label,
        dict_bytes_requested,
        rust_train_ms,
        ffi_train_ms,
        rust_dict_bytes,
        ffi_dict_bytes,
        rust_fastcover_score
    );
}

fn split_training_samples(source: &[u8]) -> Vec<Vec<u8>> {
    let sample_size = source.len().div_ceil(16).clamp(256, 8192);
    let mut samples: Vec<Vec<u8>> = source
        .chunks(sample_size)
        .take(64)
        .filter(|chunk| chunk.len() >= 64)
        .map(|chunk| chunk.to_vec())
        .collect();
    if samples.len() < 2 {
        let midpoint = source.len() / 2;
        let left = &source[..midpoint];
        let right = &source[midpoint..];
        if left.len() >= 64 && right.len() >= 64 {
            samples = vec![left.to_vec(), right.to_vec()];
        } else {
            eprintln!(
                "BENCH_WARN tiny dictionary training input ({} bytes), using a single sample fallback",
                source.len()
            );
            samples = vec![source.to_vec()];
        }
    }
    samples
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
