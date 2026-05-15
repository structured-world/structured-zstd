// rand 0.10: SmallRng is available with default features (no `small_rng` flag needed).
// Use RngExt::fill() instead of RngCore::fill_bytes(); RngCore removed from rand's public root in 0.10.
use rand::{RngExt, SeedableRng, rngs::SmallRng};
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

/// Benchmark levels mapped to comparable Rust and FFI compression settings.
/// Read `STRUCTURED_ZSTD_BENCH_LEVEL_FILTER` and return the comma-
/// separated list of level names to keep. Empty or unset means
/// "run every level". Used by CI to split the bench matrix across
/// one runner per level.
pub(crate) fn level_filter_from_env() -> Option<Vec<String>> {
    let raw = env::var("STRUCTURED_ZSTD_BENCH_LEVEL_FILTER").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts: Vec<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() { None } else { Some(parts) }
}

/// Same as [`supported_levels`] but honours `STRUCTURED_ZSTD_BENCH_
/// LEVEL_FILTER` so a CI job can run a single named level. Panics
/// if any requested name in the filter is not a known level — that
/// catches typos in the CI matrix entry early instead of letting the
/// shard succeed silently with no samples (which would skip the
/// downstream regression alert for that level). A partial match
/// (`STRUCTURED_ZSTD_BENCH_LEVEL_FILTER=default,typo`) also panics,
/// so a typo never hides behind a valid sibling token.
pub(crate) fn supported_levels_filtered() -> Vec<LevelConfig> {
    let all = supported_levels();
    let Some(keep) = level_filter_from_env() else {
        return all;
    };
    let known: Vec<&'static str> = all.iter().map(|cfg| cfg.name).collect();
    let unknown: Vec<String> = keep
        .iter()
        .filter(|name| !known.contains(&name.as_str()))
        .cloned()
        .collect();
    assert!(
        unknown.is_empty(),
        "STRUCTURED_ZSTD_BENCH_LEVEL_FILTER contained unknown level(s) {unknown:?}; \
         supported: {known:?} — fix the CI matrix entry or rename the level in \
         `supported_levels()`."
    );
    all.into_iter()
        .filter(|cfg| keep.iter().any(|name| name == cfg.name))
        .collect()
}

/// Bench-side mirror of `StrategyTag::for_compression_level`. Returns
/// the lowercase tag suffix used in bench IDs and CI shard labels so
/// the dashboard can render `level -7 :: Fast`, `level 3 :: Dfast`,
/// `level 22 :: BtUltra2`, etc. without re-deriving the strategy from
/// the numeric level on the consumer side.
///
/// Negative levels share the `fast` ultra-fast strategy (donor maps
/// any `cParams.cLevel <= 1` to `ZSTD_fast`). The 1..=22 split mirrors
/// `clevels.h` and `StrategyTag::for_level` exactly.
fn strategy_suffix(level: i32) -> &'static str {
    match level {
        // Negative levels uniformly map to the ultra-fast `Fast`
        // strategy (donor `cParams.strategy = ZSTD_fast` for any
        // `cLevel <= 1`). Level 0 is intentionally NOT classified
        // here — donor treats it as a sentinel for "use default"
        // (= 3, `Dfast`), and `supported_levels()` omits it to keep
        // bench labels unambiguous. A future caller that does pass
        // `0` should pre-resolve it to `3` before reaching this
        // helper rather than have it silently aliased to `fast`.
        i32::MIN..=-1 => "fast",
        0 => unreachable!(
            "strategy_suffix(0) called; level 0 is the donor sentinel for \
             'use default' (= 3). `supported_levels()` skips it so it never \
             reaches this helper. Resolve to the canonical numeric level \
             before calling."
        ),
        1 => "fast",
        2 | 3 => "dfast",
        4 => "greedy",
        5..=15 => "lazy",
        16 | 17 => "btopt",
        18 | 19 => "btultra",
        _ => "btultra2",
    }
}

/// Canonical bench level inventory: `-7..=-1` (ultra-fast) plus
/// `1..=22` (the donor advertised range). Level 0 is omitted because
/// the donor treats it as a sentinel for "use default" (= 3) — a
/// distinct bench entry would just duplicate level 3's numbers.
///
/// Each entry's `name` field is the canonical `level_<N>_<strategy>`
/// label consumed by:
///   - bench IDs in criterion output (`compress/level_3_dfast/...`)
///   - the CI matrix `level:` keys in `.github/workflows/ci.yml`
///   - the `STRUCTURED_ZSTD_BENCH_LEVEL_FILTER` env var
///
/// Renaming an entry requires synchronising all three call sites. The
/// `level_filter_from_env()` panic on unknown names is the safety net
/// that catches the drift in CI before any silent skips.
///
/// The inventory is built once per process via [`LazyLock`] so the
/// `Box::leak` that backs each `&'static str` `name` happens exactly
/// 29 times total — the criterion bench loops call this helper many
/// times per scenario, and a naive per-call rebuild would compound
/// the leak proportionally.
pub(crate) fn supported_levels() -> Vec<LevelConfig> {
    static INVENTORY: std::sync::LazyLock<Vec<LevelConfig>> =
        std::sync::LazyLock::new(build_supported_levels);
    INVENTORY.clone()
}

fn build_supported_levels() -> Vec<LevelConfig> {
    let mut levels = Vec::with_capacity(29);
    // Ultra-fast tier: `-7..=-1`. Donor strategy = Fast.
    for n in -7..=-1i32 {
        levels.push(LevelConfig {
            name: leak_owned(format!("level_{n}_{}", strategy_suffix(n))),
            rust_level: CompressionLevel::Level(n),
            ffi_level: n,
        });
    }
    // Standard tier: `1..=22`. Strategy mirrors `clevels.h`. Use
    // `CompressionLevel::Level(n)` directly — NOT
    // `CompressionLevel::from_level(n)` — so the bench label
    // `level_<N>_<strategy>` matches the variant exercised by the
    // encoder. `from_level(11)` collapses to `Best`, and the named
    // `Best` variant bypasses `donor_pre_split_level`'s
    // `Level(11..=15) -> Some(0)` arm (the borders pre-splitter
    // landed in #140), so the numbers would silently diverge from
    // what a user calling `compress_to_vec(input, Level(11))` sees.
    // Same divergence applies to 1 (`Fastest`), 3 (`Default`),
    // 7 (`Better`) on any future preset-only branch.
    for n in 1..=22i32 {
        levels.push(LevelConfig {
            name: leak_owned(format!("level_{n}_{}", strategy_suffix(n))),
            rust_level: CompressionLevel::Level(n),
            ffi_level: n,
        });
    }
    levels
}

/// Convert a one-shot owned `String` (built by `format!`) into a
/// `&'static str`. Called exactly once per level inside
/// [`build_supported_levels`], whose result is cached in a
/// `LazyLock` so the total leak is bounded to the 29 strings the
/// bench inventory needs — no proportional growth even when
/// `supported_levels()` is called inside criterion's per-scenario
/// loops.
fn leak_owned(name: String) -> &'static str {
    Box::leak(name.into_boxed_str())
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

    pub(crate) fn throughput_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut bytes = vec![0u8; len];
    rng.fill(&mut bytes[..]);
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

    // Resolution order:
    //   1. `STRUCTURED_ZSTD_BENCH_CORPUS_PATH` — explicit absolute path
    //      to the corpus file. CI sets this so the prebuilt bench
    //      binary (invoked directly via `STRUCTURED_ZSTD_BENCH_BIN`,
    //      bypassing cargo) can still locate the fixture even though
    //      `CARGO_MANIFEST_DIR` is not in its environment.
    //   2. `CARGO_MANIFEST_DIR/decodecorpus_files/z000033` — local
    //      `cargo bench` runs, where cargo injects the manifest dir.
    //   3. Synthetic 1 MiB fallback — packaged sources / hand-run
    //      binaries with no fixture access.
    let candidate_paths: Vec<std::path::PathBuf> = {
        let mut paths = Vec::new();
        if let Ok(explicit) = env::var("STRUCTURED_ZSTD_BENCH_CORPUS_PATH") {
            let trimmed = explicit.trim();
            if !trimmed.is_empty() {
                paths.push(std::path::PathBuf::from(trimmed));
            }
        }
        if let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") {
            paths.push(Path::new(&manifest_dir).join("decodecorpus_files/z000033"));
        }
        paths
    };

    if candidate_paths.is_empty() {
        eprintln!(
            "BENCH_WARN neither STRUCTURED_ZSTD_BENCH_CORPUS_PATH nor \
             CARGO_MANIFEST_DIR is set; using synthetic decode corpus fallback"
        );
    }
    for path in &candidate_paths {
        match fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => {
                return Scenario::new(REAL_ID, REAL_LABEL, bytes, ScenarioClass::Corpus);
            }
            Ok(_) => {
                eprintln!(
                    "BENCH_WARN decode corpus fixture is empty at {}, trying next candidate",
                    path.display()
                );
            }
            Err(err) => {
                eprintln!(
                    "BENCH_WARN failed to read decode corpus fixture at {}: {}. Trying next candidate",
                    path.display(),
                    err
                );
            }
        }
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
