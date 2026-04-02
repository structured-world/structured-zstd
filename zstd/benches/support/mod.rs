use rand::{RngCore, SeedableRng, rngs::SmallRng};
use std::{collections::HashSet, env, fs, path::Path};
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
        load_decode_corpus_scenario(),
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

pub(crate) fn supported_levels() -> [LevelConfig; 3] {
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
        LevelConfig {
            name: "better",
            rust_level: CompressionLevel::Better,
            ffi_level: 7,
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

    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    if paths.len() > max_files {
        eprintln!(
            "BENCH_WARN limiting Silesia fixtures to first {} sorted files in {}",
            max_files,
            Path::new(&dir).display()
        );
        paths.truncate(max_files);
    }

    let mut scenarios = Vec::new();
    let mut seen_silesia_ids = HashSet::new();
    for path in paths {
        let Ok(metadata) = fs::metadata(&path) else {
            eprintln!(
                "BENCH_WARN failed to stat Silesia fixture {}",
                path.display()
            );
            continue;
        };
        let file_len = metadata.len();
        if file_len > max_file_bytes as u64 {
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
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let scenario_stem = sanitize_scenario_stem(file_name);
        let scenario_id =
            dedupe_scenario_id(format!("silesia-{scenario_stem}"), &mut seen_silesia_ids);
        scenarios.push(Scenario::new(
            scenario_id,
            format!("Silesia corpus: {file_name}"),
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

fn load_decode_corpus_scenario() -> Scenario {
    const REAL_ID: &str = "decodecorpus-z000033";
    const REAL_LABEL: &str = "Repo decode corpus sample";
    const FALLBACK_ID: &str = "decodecorpus-synthetic-1m";
    const FALLBACK_LABEL: &str = "Synthetic decode corpus fallback (1 MiB)";

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok();
    let fixture_path = manifest_dir
        .as_deref()
        .map(Path::new)
        .map(|dir| dir.join("decodecorpus_files/z000033"));

    if let Some(path) = fixture_path {
        match fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                return Scenario::new(REAL_ID, REAL_LABEL, bytes, ScenarioClass::Corpus);
            }
            Ok(_) => {
                eprintln!(
                    "BENCH_WARN decode corpus fixture is empty at {}, using synthetic fallback",
                    path.display()
                );
            }
            Err(err) => {
                eprintln!(
                    "BENCH_WARN failed to read decode corpus fixture at {}: {}. Using synthetic fallback",
                    path.display(),
                    err
                );
            }
        }
    } else {
        eprintln!(
            "BENCH_WARN CARGO_MANIFEST_DIR is not set, using synthetic decode corpus fallback"
        );
    }

    // Keep the benchmark matrix runnable from packaged sources where fixture files may be omitted.
    Scenario::new(
        FALLBACK_ID,
        FALLBACK_LABEL,
        repeated_log_lines(1024 * 1024),
        ScenarioClass::Corpus,
    )
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

fn dedupe_scenario_id(base_id: String, seen_ids: &mut HashSet<String>) -> String {
    const MAX_SUFFIX: usize = 1_000_000;

    if seen_ids.insert(base_id.clone()) {
        return base_id;
    }

    for suffix in 2..=MAX_SUFFIX {
        let candidate = format!("{base_id}-{suffix}");
        if seen_ids.insert(candidate.clone()) {
            return candidate;
        }
    }

    panic!(
        "failed to allocate unique scenario id for base '{}' after {} attempts",
        base_id, MAX_SUFFIX
    );
}
