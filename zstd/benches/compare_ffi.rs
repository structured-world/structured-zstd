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

use criterion::{Criterion, SamplingMode, Throughput, black_box, criterion_group, criterion_main};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use structured_zstd::decoding::FrameDecoder;
use support::{LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, supported_levels};

static BENCHMARK_SCENARIOS: OnceLock<Vec<Scenario>> = OnceLock::new();

fn benchmark_scenarios_cached() -> &'static [Scenario] {
    BENCHMARK_SCENARIOS.get_or_init(benchmark_scenarios)
}

fn bench_compress(c: &mut Criterion) {
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels() {
            let rust_compressed =
                structured_zstd::encoding::compress_to_vec(&scenario.bytes[..], level.rust_level);
            let ffi_compressed = zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap();
            emit_report_line(&scenario, level, &rust_compressed, &ffi_compressed);
            emit_memory_report(
                &scenario,
                level,
                "compress",
                scenario.len() + rust_compressed.len(),
                scenario.len() + ffi_compressed.len(),
            );

            let benchmark_name = format!("compress/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, &scenario);
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
    for scenario in benchmark_scenarios_cached().iter() {
        for level in supported_levels() {
            let ffi_compressed = zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap();
            let expected_len = scenario.len();
            emit_memory_report(
                &scenario,
                level,
                "decompress",
                ffi_compressed.len() + expected_len,
                ffi_compressed.len() + expected_len,
            );
            let benchmark_name = format!("decompress/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, &scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            group.bench_function("pure_rust", |b| {
                let mut target = vec![0u8; expected_len];
                let mut decoder = FrameDecoder::new();
                b.iter(|| {
                    let written = decoder.decode_all(&ffi_compressed, &mut target).unwrap();
                    assert_eq!(written, expected_len);
                })
            });

            group.bench_function("c_ffi", |b| {
                let mut decoder = zstd::bulk::Decompressor::new().unwrap();
                let mut output = Vec::with_capacity(expected_len);
                b.iter(|| {
                    output.clear();
                    let written = decoder
                        .decompress_to_buffer(&ffi_compressed[..], &mut output)
                        .unwrap();
                    assert_eq!(written, expected_len);
                    assert_eq!(output.len(), expected_len);
                })
            });

            group.finish();
        }
    }
}

fn bench_dictionary(c: &mut Criterion) {
    for scenario in benchmark_scenarios_cached().iter() {
        if !matches!(scenario.class, ScenarioClass::Small | ScenarioClass::Corpus) {
            continue;
        }

        let training_samples = split_training_samples(&scenario.bytes);
        let sample_refs: Vec<&[u8]> = training_samples.iter().map(Vec::as_slice).collect();
        let total_training_bytes = sample_refs.iter().map(|sample| sample.len()).sum::<usize>();
        let dict_size = dictionary_size_for(scenario.len())
            .min(total_training_bytes.saturating_sub(64))
            .max(256);
        let train_started = Instant::now();
        let Ok(dictionary) = zstd::dict::from_samples(&sample_refs, dict_size) else {
            eprintln!(
                "BENCH_WARN skipping dictionary benchmark for {} (samples={}, total_training_bytes={}, dict_size={})",
                scenario.id,
                sample_refs.len(),
                total_training_bytes,
                dict_size
            );
            continue;
        };
        let train_ms = train_started.elapsed().as_secs_f64() * 1_000.0;

        for level in supported_levels() {
            let mut no_dict = zstd::bulk::Compressor::new(level.ffi_level).unwrap();
            let mut with_dict =
                zstd::bulk::Compressor::with_dictionary(level.ffi_level, &dictionary).unwrap();
            let no_dict_bytes = no_dict.compress(&scenario.bytes).unwrap();
            let with_dict_bytes = with_dict.compress(&scenario.bytes).unwrap();
            emit_dictionary_report(
                &scenario,
                level,
                dictionary.len(),
                train_ms,
                &no_dict_bytes,
                &with_dict_bytes,
            );

            let benchmark_name =
                format!("compress-dict/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, &scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            group.bench_function("c_ffi_without_dict", |b| {
                let mut compressor = zstd::bulk::Compressor::new(level.ffi_level).unwrap();
                b.iter(|| black_box(compressor.compress(&scenario.bytes).unwrap()))
            });

            group.bench_function("c_ffi_with_dict", |b| {
                let mut compressor =
                    zstd::bulk::Compressor::with_dictionary(level.ffi_level, &dictionary).unwrap();
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
    stage: &'static str,
    rust_buffer_bytes_estimate: usize,
    ffi_buffer_bytes_estimate: usize,
) {
    println!(
        "REPORT_MEM scenario={} label=\"{}\" level={} stage={} rust_buffer_bytes_estimate={} ffi_buffer_bytes_estimate={}",
        scenario.id,
        scenario.label,
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
        scenario.label,
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
        scenario.label,
        level.name,
        dict_bytes,
        train_ms,
        no_dict_bytes.len(),
        with_dict_bytes.len(),
        no_dict_ratio,
        with_dict_ratio
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

criterion_group!(benches, bench_compress, bench_decompress, bench_dictionary);
criterion_main!(benches);
