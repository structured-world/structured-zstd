use rand::{RngCore, SeedableRng, rngs::SmallRng};
use std::{env, fs, path::Path};
use structured_zstd::encoding::CompressionLevel;

pub(crate) struct Scenario {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) class: ScenarioClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScenarioClass {
    Small,
    Corpus,
    Entropy,
    Large,
    Silesia,
}

#[derive(Clone, Copy)]
pub(crate) struct LevelConfig {
    pub(crate) name: &'static str,
    pub(crate) rust_level: CompressionLevel,
    pub(crate) ffi_level: i32,
}

pub(crate) fn benchmark_scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![
        Scenario::new(
            "small-1k-random",
            "Small random payload (1 KiB)",
            random_bytes(1024, 0x5EED_1000),
            ScenarioClass::Small,
        ),
        Scenario::new(
            "small-10k-random",
            "Small random payload (10 KiB)",
            random_bytes(10 * 1024, 0x0005_EED1_0000),
            ScenarioClass::Small,
        ),
        Scenario::new(
            "small-4k-log-lines",
            "Small structured log lines (4 KiB)",
            repeated_log_lines(4 * 1024),
            ScenarioClass::Small,
        ),
        Scenario::new(
            "decodecorpus-z000033",
            "Repo decode corpus sample",
            include_bytes!("../../decodecorpus_files/z000033").to_vec(),
            ScenarioClass::Corpus,
        ),
        Scenario::new(
            "high-entropy-1m",
            "High entropy random payload (1 MiB)",
            random_bytes(1024 * 1024, 0xC0FF_EE11),
            ScenarioClass::Entropy,
        ),
        Scenario::new(
            "low-entropy-1m",
            "Low entropy patterned payload (1 MiB)",
            repeated_pattern_bytes(1024 * 1024),
            ScenarioClass::Entropy,
        ),
        Scenario::new(
            "large-log-stream",
            "Large structured stream",
            repeated_log_lines(large_stream_len()),
            ScenarioClass::Large,
        ),
    ];

    scenarios.extend(load_silesia_from_env());
    scenarios
}

pub(crate) fn supported_levels() -> [LevelConfig; 2] {
    [
        LevelConfig {
            name: "fastest",
            rust_level: CompressionLevel::Fastest,
            ffi_level: 1,
        },
        LevelConfig {
            name: "default",
            rust_level: CompressionLevel::Default,
            ffi_level: 3,
        },
    ]
}

impl Scenario {
    fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        bytes: Vec<u8>,
        class: ScenarioClass,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            bytes,
            class,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(crate) fn throughput_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut bytes = vec![0u8; len];
    rng.fill_bytes(&mut bytes);
    bytes
}

fn repeated_pattern_bytes(len: usize) -> Vec<u8> {
    let pattern = b"coordinode:segment:0001|tenant=demo|label=orders|";
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let remaining = len - bytes.len();
        bytes.extend_from_slice(&pattern[..pattern.len().min(remaining)]);
    }
    bytes
}

fn repeated_log_lines(len: usize) -> Vec<u8> {
    const LINES: &[&str] = &[
        "ts=2026-03-26T21:39:28Z level=INFO msg=\"flush memtable\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:29Z level=INFO msg=\"rotate segment\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:30Z level=INFO msg=\"compact level\" tenant=demo table=orders region=eu-west\n",
        "ts=2026-03-26T21:39:31Z level=INFO msg=\"write block\" tenant=demo table=orders region=eu-west\n",
    ];

    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        for line in LINES {
            if bytes.len() == len {
                break;
            }
            let remaining = len - bytes.len();
            bytes.extend_from_slice(&line.as_bytes()[..line.len().min(remaining)]);
        }
    }
    bytes
}

fn load_silesia_from_env() -> Vec<Scenario> {
    const DEFAULT_MAX_FILES: usize = 12;
    const DEFAULT_MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
    let Some(dir) = env::var_os("STRUCTURED_ZSTD_SILESIA_DIR") else {
        return Vec::new();
    };
    let max_files = env::var("STRUCTURED_ZSTD_SILESIA_MAX_FILES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_FILES);
    let max_file_bytes = env::var("STRUCTURED_ZSTD_SILESIA_MAX_FILE_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_FILE_BYTES);

    let Ok(entries) = fs::read_dir(Path::new(&dir)) else {
        eprintln!("BENCH_WARN failed to read STRUCTURED_ZSTD_SILESIA_DIR={dir:?}");
        return Vec::new();
    };

    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    if paths.len() > max_files {
        eprintln!(
            "BENCH_WARN limiting Silesia fixtures to first {} files from {} entries in {}",
            max_files,
            paths.len(),
            Path::new(&dir).display()
        );
        paths.truncate(max_files);
    }

    let mut scenarios = Vec::new();
    for path in paths {
        let Ok(metadata) = fs::metadata(&path) else {
            eprintln!(
                "BENCH_WARN failed to stat Silesia fixture {}",
                path.display()
            );
            continue;
        };
        let file_len = metadata.len() as usize;
        if file_len > max_file_bytes {
            eprintln!(
                "BENCH_WARN skipping Silesia fixture {} ({} bytes > max {} bytes)",
                path.display(),
                file_len,
                max_file_bytes
            );
            continue;
        }

        let Ok(bytes) = fs::read(&path) else {
            eprintln!(
                "BENCH_WARN failed to read Silesia fixture {}",
                path.display()
            );
            continue;
        };
        if bytes.is_empty() {
            eprintln!(
                "BENCH_WARN skipping empty Silesia fixture {}",
                path.display()
            );
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let scenario_stem = sanitize_scenario_stem(stem);
        scenarios.push(Scenario::new(
            format!("silesia-{scenario_stem}"),
            format!("Silesia corpus: {stem}"),
            bytes,
            ScenarioClass::Silesia,
        ));
    }

    scenarios.sort_by(|left, right| left.id.cmp(&right.id));
    scenarios
}

fn large_stream_len() -> usize {
    env::var("STRUCTURED_ZSTD_BENCH_LARGE_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(100 * 1024 * 1024)
}

fn sanitize_scenario_stem(stem: &str) -> String {
    let mut sanitized = String::with_capacity(stem.len());
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    }
}
