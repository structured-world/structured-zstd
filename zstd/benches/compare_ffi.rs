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
use std::time::Duration;
use structured_zstd::decoding::FrameDecoder;
use support::{LevelConfig, Scenario, ScenarioClass, benchmark_scenarios, supported_levels};

fn bench_compress(c: &mut Criterion) {
    for scenario in benchmark_scenarios() {
        for level in supported_levels() {
            let rust_compressed =
                structured_zstd::encoding::compress_to_vec(&scenario.bytes[..], level.rust_level);
            let ffi_compressed = zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap();
            emit_report_line(&scenario, level, &rust_compressed, &ffi_compressed);

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
    for scenario in benchmark_scenarios() {
        for level in supported_levels() {
            let ffi_compressed = zstd::encode_all(&scenario.bytes[..], level.ffi_level).unwrap();
            let expected_len = scenario.len();
            let benchmark_name = format!("decompress/{}/{}/{}", level.name, scenario.id, "matrix");
            let mut group = c.benchmark_group(benchmark_name);
            configure_group(&mut group, &scenario);
            group.throughput(Throughput::Bytes(scenario.throughput_bytes()));

            group.bench_function("pure_rust", |b| {
                let mut target = vec![0u8; expected_len];
                b.iter(|| {
                    let mut decoder = FrameDecoder::new();
                    target.fill(0);
                    let written = decoder.decode_all(&ffi_compressed, &mut target).unwrap();
                    assert_eq!(written, expected_len);
                })
            });

            group.bench_function("c_ffi", |b| {
                b.iter(|| {
                    let output = zstd::decode_all(&ffi_compressed[..]).unwrap();
                    assert_eq!(output.len(), expected_len);
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

criterion_group!(benches, bench_compress, bench_decompress);
criterion_main!(benches);
