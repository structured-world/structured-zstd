#!/bin/bash
# Run the Criterion benchmark matrix and produce:
# - benchmark-results.json for github-action-benchmark
# - benchmark-report.md for human review
#
# Output format note:
# - benchmark JSON uses customSmallerIsBetter (lower ms/iter is better)
# - report markdown also includes per-scenario compression size + ratio summaries
set -eo pipefail

echo "Running benchmark matrix..." >&2

if [ -n "${GITHUB_ACTIONS:-}" ] && [ -z "${STRUCTURED_ZSTD_BENCH_LARGE_BYTES:-}" ]; then
  export STRUCTURED_ZSTD_BENCH_LARGE_BYTES=16777216
fi

BENCH_TARGET_LABEL="${STRUCTURED_ZSTD_BENCH_TARGET:-host}"
BENCH_TARGET_TRIPLE="${STRUCTURED_ZSTD_BENCH_TRIPLE:-}"

# Keep emitted target IDs stable across artifacts and docs.
BENCH_TARGET_ID="$BENCH_TARGET_LABEL"

BENCH_RAW_FILE="$(mktemp -t structured-zstd-bench-raw.XXXXXX)"
trap 'rm -f "$BENCH_RAW_FILE"' EXIT

export STRUCTURED_ZSTD_EMIT_REPORT=1
# CI matrix splits build (per target) from execution (per target × level):
# the `bench-build` job hands the compiled criterion binary to every
# shard via `STRUCTURED_ZSTD_BENCH_BIN`, so we re-exec it directly
# instead of going through `cargo bench`. When the env var is not set
# (local dev runs, single-runner CI), fall back to building on demand.
if [ -n "${STRUCTURED_ZSTD_BENCH_BIN:-}" ]; then
  if [ ! -x "$STRUCTURED_ZSTD_BENCH_BIN" ]; then
    echo "STRUCTURED_ZSTD_BENCH_BIN=$STRUCTURED_ZSTD_BENCH_BIN is not executable" >&2
    exit 2
  fi
  echo "Running pre-built bench binary: $STRUCTURED_ZSTD_BENCH_BIN" >&2
  "$STRUCTURED_ZSTD_BENCH_BIN" --bench --output-format bencher | tee "$BENCH_RAW_FILE"
else
  BENCH_CMD=(cargo bench --bench compare_ffi -p ffi-bench)
  if [ -n "$BENCH_TARGET_TRIPLE" ]; then
    BENCH_CMD+=(--target "$BENCH_TARGET_TRIPLE")
  fi
  "${BENCH_CMD[@]}" -- --output-format bencher | tee "$BENCH_RAW_FILE"
fi

# Memory bench (compare_ffi_memory) runs separately when its binary is
# available — keeps the timing run on a pristine system allocator and
# scopes the `#[global_allocator]` tracking wrapper to this second
# invocation only. PR CI doesn't ship the memory binary (PRs care
# about review-cycle latency, not memory regression), main pushes do.
# Both runs append `REPORT_*` lines to the same raw file so downstream
# parsing is uniform.
if [ -n "${STRUCTURED_ZSTD_BENCH_MEMORY_BIN:-}" ]; then
  if [ ! -x "$STRUCTURED_ZSTD_BENCH_MEMORY_BIN" ]; then
    echo "STRUCTURED_ZSTD_BENCH_MEMORY_BIN=$STRUCTURED_ZSTD_BENCH_MEMORY_BIN is not executable" >&2
    exit 2
  fi
  echo "Running memory bench: $STRUCTURED_ZSTD_BENCH_MEMORY_BIN" >&2
  "$STRUCTURED_ZSTD_BENCH_MEMORY_BIN" | tee -a "$BENCH_RAW_FILE"
elif [ -z "${STRUCTURED_ZSTD_BENCH_BIN:-}" ]; then
  # Local dev path: when no prebuilt binary env was set above, the
  # cargo invocation already covered compare_ffi. Run the memory bench
  # the same way so REPORT_MEM lines land in the raw file.
  MEM_CMD=(cargo bench --bench compare_ffi_memory -p ffi-bench)
  if [ -n "$BENCH_TARGET_TRIPLE" ]; then
    MEM_CMD+=(--target "$BENCH_TARGET_TRIPLE")
  fi
  "${MEM_CMD[@]}" | tee -a "$BENCH_RAW_FILE"
fi

echo "Parsing results..." >&2

BENCH_RAW_FILE="$BENCH_RAW_FILE" \
BENCH_TARGET_LABEL="$BENCH_TARGET_LABEL" \
BENCH_TARGET_TRIPLE="$BENCH_TARGET_TRIPLE" \
BENCH_TARGET_ID="$BENCH_TARGET_ID" \
python3 - <<'PYEOF'
import json
import os
import re
import sys
from datetime import datetime, timezone
from collections import defaultdict

BENCH_RE = re.compile(r"test (\S+)\s+\.\.\. bench:\s+([\d,]+) ns/iter")
REPORT_RE = re.compile(
    r'^REPORT scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) input_bytes=(\d+) rust_bytes=(\d+) ffi_bytes=(\d+) rust_ratio=([0-9.]+) ffi_ratio=([0-9.]+)$'
)
MEM_RE = re.compile(
    r'^REPORT_MEM scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) stage=(\S+) rust_peak_alloc_bytes=(\d+) ffi_peak_alloc_bytes=(\d+)$'
)
DICT_RE = re.compile(
    r'^REPORT_DICT scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) dict_bytes=(\d+) train_ms=([0-9.]+) ffi_no_dict_bytes=(\d+) ffi_with_dict_bytes=(\d+) ffi_no_dict_ratio=([0-9.]+) ffi_with_dict_ratio=([0-9.]+)'
    r'(?: rust_with_dict_bytes=(\d+) rust_with_dict_ratio=([0-9.]+))?$'
)
DICT_TRAIN_RE = re.compile(
    r'^REPORT_DICT_TRAIN scenario=(\S+) label="((?:[^"\\]|\\.)+)" training_bytes=(\d+) dict_bytes_requested=(\d+) rust_train_ms=([0-9.]+) ffi_train_ms=([0-9.]+) rust_dict_bytes=(\d+) ffi_dict_bytes=(\d+) rust_fastcover_score=(\d+)$'
)
# Process-global CPU kernel tier the run actually selected (shared
# encode/decode entropy dispatch). One line per run; attributes every
# measurement to the kernel + arch + libc that produced it.
KERNEL_RE = re.compile(
    r'^REPORT_KERNEL kernel=(\S+) arch=(\S+) target_env=(\S+)$'
)

def unescape_report_label(value):
    output = []
    i = 0
    while i < len(value):
        ch = value[i]
        if ch == "\\" and i + 1 < len(value):
            i += 1
            output.append(value[i])
        else:
            output.append(ch)
        i += 1
    return "".join(output)

def markdown_table_escape(value):
    escaped = value.strip()
    escaped = escaped.replace("\\", "\\\\")
    escaped = escaped.replace("|", "\\|")
    escaped = escaped.replace("`", "\\`")
    escaped = escaped.replace("[", "\\[")
    escaped = escaped.replace("]", "\\]")
    escaped = escaped.replace("*", "\\*")
    escaped = escaped.replace("_", "\\_")
    escaped = escaped.replace("<", "&lt;")
    escaped = escaped.replace(">", "&gt;")
    escaped = escaped.replace("%", "&#37;")
    return escaped.replace("\n", "<br>")

def _read_sysfs(path):
    try:
        with open(path) as _f:
            return _f.read().strip()
    except OSError:
        return None


def collect_machine_info():
    """Best-effort host fingerprint for the shard that ran this bench: CPU
    model plus the ISA flags that drive our kernel dispatch (so an AVX-512
    host is visible even when the AVX2 tier was selected), core count,
    frequency, cpufreq governor, and memory. Every field is optional —
    missing ones are dropped so a locked-down runner still yields a valid
    (smaller) block instead of failing the run."""
    import platform
    import subprocess

    def _cmd(args):
        try:
            return subprocess.run(
                args, capture_output=True, text=True, timeout=10
            ).stdout
        except Exception:
            return ""

    info = {
        "os": platform.system(),
        "arch": platform.machine(),
        "hostname": platform.node() or None,
        "cpus_logical": os.cpu_count(),
    }
    system = platform.system()
    if system == "Linux":
        cpuinfo = _read_sysfs("/proc/cpuinfo") or ""
        m = re.search(r"^model name\s*:\s*(.+)$", cpuinfo, re.M)
        if m:
            info["cpu_model"] = m.group(1).strip()
        flags_m = re.search(r"^flags\s*:\s*(.+)$", cpuinfo, re.M)
        if flags_m:
            flags = set(flags_m.group(1).split())
            info["isa"] = {
                f: (f in flags)
                for f in ("sse2", "bmi2", "avx2", "avx512f", "avx512vbmi2")
            }
        gov = _read_sysfs("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        if gov:
            info["governor"] = gov
        for key, path in (
            ("max_freq_mhz", "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"),
            ("cur_freq_mhz", "/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"),
        ):
            raw = _read_sysfs(path)
            if raw and raw.isdigit():
                info[key] = round(int(raw) / 1000)
        meminfo = _read_sysfs("/proc/meminfo") or ""
        mm = re.search(r"^MemTotal:\s*(\d+)\s*kB", meminfo, re.M)
        if mm:
            info["mem_total_mb"] = round(int(mm.group(1)) / 1024)
        # Memory type/speed need DMI (root): try `sudo -n` then bare, ignore
        # failures (most hosted runners deny dmidecode — that is fine).
        dmi = _cmd(["sudo", "-n", "dmidecode", "-t", "memory"]) or _cmd(
            ["dmidecode", "-t", "memory"]
        )
        tm = re.search(r"^\s*Type:\s*(DDR\S+)", dmi, re.M)
        if tm:
            info["mem_type"] = tm.group(1)
        sm = re.search(r"Configured Memory Speed:\s*(\d+)\s*MT/s", dmi)
        if sm:
            info["mem_speed_mts"] = int(sm.group(1))
    elif system == "Darwin":
        model = _cmd(["sysctl", "-n", "machdep.cpu.brand_string"]).strip()
        if model:
            info["cpu_model"] = model
        freq = _cmd(["sysctl", "-n", "hw.cpufrequency"]).strip()
        if freq.isdigit() and int(freq) > 0:
            info["max_freq_mhz"] = round(int(freq) / 1_000_000)
        memsize = _cmd(["sysctl", "-n", "hw.memsize"]).strip()
        if memsize.isdigit():
            info["mem_total_mb"] = round(int(memsize) / 1024 / 1024)
    return {k: v for k, v in info.items() if v is not None}


benchmark_results = []
timings = []
ratios = []
memory_rows = []
dictionary_rows = []
dictionary_training_rows = []
kernel_info = None
machine_info = collect_machine_info()
timing_rows = []
scenario_input_bytes = {}
scenario_training_bytes = {}
raw_path = os.environ["BENCH_RAW_FILE"]
bench_target_label = os.environ.get("BENCH_TARGET_LABEL", "host")
bench_target_triple = os.environ.get("BENCH_TARGET_TRIPLE", "")
bench_target_id = os.environ.get("BENCH_TARGET_ID", bench_target_label)
commit_sha = os.environ.get("GITHUB_SHA")
# Commit subject for the dashboard snapshot selector — picking a run by
# date alone is hard, so each record carries the one-line message too.
# Prefer an explicit env override; otherwise read the subject from git
# (CI checks out the repo, so this resolves in the benchmark job).
commit_message = os.environ.get("STRUCTURED_ZSTD_BENCH_COMMIT_MESSAGE")
if not commit_message:
    import subprocess

    try:
        commit_message = subprocess.run(
            ["git", "log", "-1", "--format=%s", commit_sha or "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except Exception:
        commit_message = None
# Normalize to a single trimmed line. An env override may carry trailing
# newlines / surrounding whitespace from CI step output, and a stray
# newline flowing into the JSON would break the dashboard option layout.
commit_message = commit_message.strip() if commit_message else ""
commit_message = commit_message.splitlines()[0].strip() if commit_message else None
generated_at = os.environ.get("STRUCTURED_ZSTD_BENCH_GENERATED_AT") or datetime.now(timezone.utc).isoformat()
timing_point_count = 0

DELTA_LOW = 0.99
DELTA_HIGH = 1.05
REGRESSION_STAGES = {"compress", "decompress"}
REGRESSION_SCENARIOS = {
    "small-4k-log-lines",
    "decodecorpus-z000033",
    "decodecorpus-synthetic-1m",
    "low-entropy-1m",
}
# Only the canonical default-level (level_3_dfast) and max-compression
# (level_22_btultra2) shards drive the github-action-benchmark
# regression alert. Other levels still land in the dashboard JSON and
# the markdown report, but they don't fire alerts — too noisy when
# every commit measures 29 levels × 3 targets and we'd get false
# positives on the experimental fast/btopt levels that aren't yet
# tuned. Keep this in sync with the PR shard's `pr-canonical` levels
# in `.github/workflows/ci.yml`.
ALERT_LEVELS = {"level_3_dfast", "level_22_btultra2"}

def strip_dict_level_suffix(level):
    """Normalize a bench-variant level id to its real level-axis identity.

    The LDM+dict matrix variants encode the dict discriminator in the
    level NAME (`level_1_fast_ldm_dict`, `level_22_btultra2_ldm_dict` in
    `zstd/benches/support/mod.rs`), kept that way so the CI level-filter
    env var lists them verbatim. But the dashboard `level` field is the
    chart's X-axis identity, and dict/plain already split via `stage`
    (`compress-dict` / `decompress-dict`). Stripping the suffix here keeps
    the Level dropdown to real levels without losing information — the
    suffix is pure bench-variant bookkeeping, not part of the level.
    """
    return level[: -len("_dict")] if level.endswith("_dict") else level


def parse_benchmark_name(name):
    parts = name.split("/")
    if len(parts) == 5 and parts[0] == "compress" and parts[3] == "matrix":
        return {
            "stage": "compress",
            "level": strip_dict_level_suffix(parts[1]),
            "scenario": parts[2],
            "source": None,
            "implementation": parts[4],
        }
    if len(parts) == 6 and parts[0] == "decompress" and parts[4] == "matrix":
        return {
            "stage": "decompress",
            "level": strip_dict_level_suffix(parts[1]),
            "scenario": parts[2],
            "source": parts[3],
            "implementation": parts[5],
        }
    if len(parts) == 5 and parts[0] == "compress-dict" and parts[3] == "matrix":
        return {
            "stage": "compress-dict",
            "level": strip_dict_level_suffix(parts[1]),
            "scenario": parts[2],
            "source": None,
            "implementation": parts[4],
        }
    if len(parts) == 5 and parts[0] == "decompress-dict" and parts[3] == "matrix":
        return {
            "stage": "decompress-dict",
            "level": strip_dict_level_suffix(parts[1]),
            "scenario": parts[2],
            "source": None,
            "implementation": parts[4],
        }
    if len(parts) == 5 and parts[0] == "dict-train" and parts[3] == "matrix":
        return {
            "stage": "dict-train",
            "level": strip_dict_level_suffix(parts[1]),
            "scenario": parts[2],
            "source": None,
            "implementation": parts[4],
        }
    raise ValueError(f"Unsupported benchmark name format: {name} (parts={parts})")

def canonical_key(stage, scenario, level, source):
    params = [f"stage={stage}", f"level={level}"]
    if source:
        params.append(f"source={source}")
    return f"{scenario} + {', '.join(params)}"

def normalize_impl(impl):
    if impl == "pure_rust":
        return "rust"
    if impl == "c_ffi":
        return "ffi"
    # Dict-bench implementations: collapse the with-dict pair to the same
    # (rust, ffi) key shape the rest of the dashboard uses so the dict
    # stage row produces a comparable ratio/speed pair.
    # `c_ffi_without_dict` keeps its raw name as a third series: the
    # current aggregation loop only computes rust-vs-ffi deltas, so it's
    # carried through for visual inspection / future use rather than
    # entering a ratio.
    if impl == "pure_rust_with_dict":
        return "rust"
    if impl == "c_ffi_with_dict":
        return "ffi"
    return impl

def include_in_regression_set(parsed_name, regression_levels):
    return (
        parsed_name["stage"] in REGRESSION_STAGES
        and parsed_name["level"] in regression_levels
        and parsed_name["scenario"] in REGRESSION_SCENARIOS
    )

def classify_ratio_delta(delta):
    if delta is None:
        return "insufficient-data"
    if delta < DELTA_LOW:
        return "rust_better_smaller"
    if delta <= DELTA_HIGH:
        return "near_parity"
    return "rust_worse_larger"

def classify_speed_delta(delta):
    if delta is None:
        return "insufficient-data"
    if delta < DELTA_LOW:
        return "rust_slower"
    if delta <= DELTA_HIGH:
        return "near_parity"
    return "rust_faster"

with open(raw_path) as f:
    for raw_line in f:
        line = raw_line.strip()

        bench_match = BENCH_RE.match(line)
        if bench_match:
            name = bench_match.group(1)
            ns = int(bench_match.group(2).replace(",", ""))
            ms = ns / 1_000_000
            timings.append((name, ms))
            parsed = parse_benchmark_name(name)
            timing_point_count += 1
            timing_rows.append({
                "name": name,
                "stage": parsed["stage"],
                "level": parsed["level"],
                "scenario": parsed["scenario"],
                "source": parsed["source"],
                "implementation": normalize_impl(parsed["implementation"]),
                "target": bench_target_id,
                "ms_per_iter": ms,
            })
            continue

        kernel_match = KERNEL_RE.match(line)
        if kernel_match:
            k_name, k_arch, k_env = kernel_match.groups()
            kernel_info = {"kernel": k_name, "arch": k_arch, "target_env": k_env}
            continue

        report_match = REPORT_RE.match(line)
        if report_match:
            scenario, label, level, input_bytes, rust_bytes, ffi_bytes, rust_ratio, ffi_ratio = report_match.groups()
            label = unescape_report_label(label)
            ratios.append({
                "scenario": scenario,
                "label": label,
                "level": level,
                "input_bytes": int(input_bytes),
                "rust_bytes": int(rust_bytes),
                "ffi_bytes": int(ffi_bytes),
                "rust_ratio": float(rust_ratio),
                "ffi_ratio": float(ffi_ratio),
            })
            scenario_input_bytes[scenario] = int(input_bytes)
            continue

        mem_match = MEM_RE.match(line)
        if mem_match:
            (
                scenario,
                label,
                level,
                stage,
                rust_peak_alloc_bytes,
                ffi_peak_alloc_bytes,
            ) = mem_match.groups()
            label = unescape_report_label(label)
            # Both sides observed by the same `TrackingAllocator` in
            # `compare_ffi_memory.rs`: Rust allocs flow through the
            # `#[global_allocator]` wrapper, libzstd allocs flow through
            # `ZSTD_customMem` callbacks that share the same atomic
            # counters. Counts are byte-precise on both sides — values
            # are directly comparable and the downstream `delta_ratio`
            # for `peak_alloc_bytes` is meaningful.
            memory_rows.append({
                "scenario": scenario,
                "label": label,
                "level": level,
                "stage": stage,
                "rust_peak_alloc_bytes": int(rust_peak_alloc_bytes),
                "ffi_peak_alloc_bytes": int(ffi_peak_alloc_bytes),
            })
            continue

        dict_match = DICT_RE.match(line)
        if dict_match:
            (
                scenario,
                label,
                level,
                dict_bytes,
                train_ms,
                ffi_no_dict_bytes,
                ffi_with_dict_bytes,
                ffi_no_dict_ratio,
                ffi_with_dict_ratio,
                rust_with_dict_bytes,
                rust_with_dict_ratio,
            ) = dict_match.groups()
            label = unescape_report_label(label)
            # rust_with_dict_* are optional trailing fields (older bench logs
            # omit them). A reported 0 means the Rust dict path was
            # unavailable for this (scenario, level) — treat as "no rust dict
            # ratio" so no misleading compress-dict ratio series is emitted.
            rust_with_dict_bytes_val = (
                int(rust_with_dict_bytes) if rust_with_dict_bytes is not None else 0
            )
            rust_with_dict_ratio_val = (
                float(rust_with_dict_ratio) if rust_with_dict_ratio is not None else 0.0
            )
            dictionary_rows.append({
                "scenario": scenario,
                "label": label,
                "level": level,
                "dict_bytes": int(dict_bytes),
                "train_ms": float(train_ms),
                "ffi_no_dict_bytes": int(ffi_no_dict_bytes),
                "ffi_with_dict_bytes": int(ffi_with_dict_bytes),
                "ffi_no_dict_ratio": float(ffi_no_dict_ratio),
                "ffi_with_dict_ratio": float(ffi_with_dict_ratio),
                "rust_with_dict_bytes": rust_with_dict_bytes_val,
                "rust_with_dict_ratio": rust_with_dict_ratio_val,
            })
            continue

        dict_train_match = DICT_TRAIN_RE.match(line)
        if dict_train_match:
            (
                scenario,
                label,
                training_bytes,
                dict_bytes_requested,
                rust_train_ms,
                ffi_train_ms,
                rust_dict_bytes,
                ffi_dict_bytes,
                rust_fastcover_score,
            ) = dict_train_match.groups()
            label = unescape_report_label(label)
            delta = None
            rust_train_ms_float = float(rust_train_ms)
            ffi_train_ms_float = float(ffi_train_ms)
            if rust_train_ms_float > 0.0:
                delta = ffi_train_ms_float / rust_train_ms_float
            dictionary_training_rows.append({
                "scenario": scenario,
                "label": label,
                "training_bytes": int(training_bytes),
                "dict_bytes_requested": int(dict_bytes_requested),
                "rust_train_ms": rust_train_ms_float,
                "ffi_train_ms": ffi_train_ms_float,
                "rust_dict_bytes": int(rust_dict_bytes),
                "ffi_dict_bytes": int(ffi_dict_bytes),
                "rust_fastcover_score": int(rust_fastcover_score),
                "delta_ffi_over_rust": delta,
                "status": classify_speed_delta(delta),
            })
            scenario_training_bytes[scenario] = int(training_bytes)

if timing_point_count == 0:
    print("ERROR: No benchmark timings parsed from compare_ffi output.", file=sys.stderr)
    sys.exit(1)

# Restrict the alert set to the canonical pair regardless of how
# many levels this shard processed. Combined with `REGRESSION_STAGES`
# + `REGRESSION_SCENARIOS` this keeps the github-action-benchmark
# alert surface scoped to level_3_dfast / level_22_btultra2 — the
# two levels we ship as the primary public guarantees.
present_levels = {row["level"] for row in ratios}
regression_levels = ALERT_LEVELS & present_levels
benchmark_results = [
    {
        "name": row["name"],
        "unit": "ms",
        "value": round(row["ms_per_iter"], 3),
    }
    for row in timing_rows
    if include_in_regression_set(row, regression_levels)
]

if not benchmark_results:
    if regression_levels:
        # Strategy shard *does* contain at least one canonical alert
        # level but no scenario row landed in REGRESSION_SCENARIOS —
        # almost certainly a scenario-mapping issue (e.g. a renamed
        # corpus fixture). Fall back to all timings so the dashboard
        # still has data while the mapping gets fixed.
        print(
            "WARN: No regression-set benchmark rows matched smoke filter; "
            "falling back to all parsed timings for benchmark-results.json.",
            file=sys.stderr,
        )
        # Filter to canonical levels + regression stages so the
        # fallback respects the alert contract: only `level_3_dfast`
        # and `level_22_btultra2` (the ALERT_LEVELS) ever fire
        # github-action-benchmark regressions. Falling back to
        # unfiltered `timings` here would reintroduce non-canonical
        # shard siblings (e.g. level_4_dfast on the `fast-dfast` shard) and
        # they'd trip alerts they were explicitly excluded from.
        benchmark_results = [
            {
                "name": row["name"],
                "unit": "ms",
                "value": round(row["ms_per_iter"], 3),
            }
            for row in timing_rows
            if row["stage"] in REGRESSION_STAGES
            and row["level"] in regression_levels
        ]
    else:
        # Strategy shard processed only non-canonical levels (e.g.
        # `fast` / `greedy` / `btopt` groups on a main push). Emit an
        # empty `benchmark-results.json` so github-action-benchmark
        # has no rows to compare against the baseline — these levels
        # land in the dashboard via `benchmark-relative.json` but
        # never fire regression alerts. Previously the fallback
        # repopulated every timing here, which silently re-expanded
        # the alert surface to all 29 levels (CR review of #143).
        print(
            "INFO: shard processed no canonical alert levels "
            f"(present={sorted(present_levels)}, "
            f"alert_set={sorted(ALERT_LEVELS)}); writing empty "
            "benchmark-results.json so this shard contributes no alerts.",
            file=sys.stderr,
        )

if not ratios:
    print(
        "ERROR: No REPORT ratio lines parsed; benchmark-report.md would have an empty ratio section.",
        file=sys.stderr,
    )
    sys.exit(1)

if not memory_rows:
    # No REPORT_MEM lines is expected on PR shards where the memory
    # bench binary isn't passed (`STRUCTURED_ZSTD_BENCH_MEMORY_BIN`
    # empty). Memory rows only land on main pushes; on PRs we still
    # publish the timing/ratio shards and skip the memory section.
    print(
        "INFO: No REPORT_MEM lines parsed (memory bench not run on this shard); "
        "memory section will be omitted.",
        file=sys.stderr,
    )

if not dictionary_rows:
    print(
        "WARN: No REPORT_DICT lines parsed; dictionary section has no data rows; writing _n/a_ placeholder.",
        file=sys.stderr,
    )

if not dictionary_training_rows:
    print(
        "WARN: No REPORT_DICT_TRAIN lines parsed; dictionary training section has no data rows; writing _n/a_ placeholder.",
        file=sys.stderr,
    )

with open("benchmark-results.json", "w") as f:
    json.dump(benchmark_results, f, indent=2)

ratio_index = {}
for row in ratios:
    key = canonical_key("compress", row["scenario"], row["level"], None)
    ratio_delta = None
    if row["ffi_ratio"] > 0.0:
        ratio_delta = row["rust_ratio"] / row["ffi_ratio"]
    ratio_index[key] = {
        "meta": {
            "stage": "compress",
            "scenario": row["scenario"],
            "level": row["level"],
            "source": None,
        },
        "rust_ratio": row["rust_ratio"],
        "ffi_ratio": row["ffi_ratio"],
        "delta": ratio_delta,
        "status": classify_ratio_delta(ratio_delta),
    }

speed_index = defaultdict(dict)
key_meta = {}
for row in timing_rows:
    key = canonical_key(row["stage"], row["scenario"], row["level"], row["source"])
    key_meta[key] = {
        "stage": row["stage"],
        "scenario": row["scenario"],
        "level": row["level"],
        "source": row["source"],
    }
    impl = row["implementation"]
    speed_index[key][impl] = {
        "name": row["name"],
        "ms_per_iter": row["ms_per_iter"],
    }

delta_rows = []
all_keys = sorted(set(key_meta.keys()) | set(ratio_index.keys()))
for key in all_keys:
    ratio_pack = ratio_index.get(
        key,
        {
            "meta": None,
            "rust_ratio": None,
            "ffi_ratio": None,
            "delta": None,
            "status": "insufficient-data",
        },
    )
    meta = key_meta.get(key) or ratio_pack["meta"]
    stage = meta["stage"] if meta else "compress"
    scenario = meta["scenario"] if meta else key.split(" + ")[0]
    level = meta["level"] if meta else "unknown"
    source = meta["source"] if meta else None
    if stage == "dict-train":
        input_bytes = scenario_training_bytes.get(scenario)
    else:
        input_bytes = scenario_input_bytes.get(scenario)

    speed_series = {}
    for impl_name, impl_row in speed_index.get(key, {}).items():
        ms_value = impl_row["ms_per_iter"]
        bps_value = None
        if input_bytes is not None and ms_value is not None and ms_value > 0.0:
            bps_value = input_bytes / (ms_value / 1000.0)
        speed_series[impl_name] = {
            "benchmark_name": impl_row["name"],
            "ms_per_iter": ms_value,
            "bytes_per_sec": bps_value,
        }

    rust_timing = speed_series.get("rust")
    ffi_timing = speed_series.get("ffi")
    rust_ms = rust_timing["ms_per_iter"] if rust_timing else None
    ffi_ms = ffi_timing["ms_per_iter"] if ffi_timing else None
    rust_bps = rust_timing["bytes_per_sec"] if rust_timing else None
    ffi_bps = ffi_timing["bytes_per_sec"] if ffi_timing else None
    speed_delta = (
        rust_bps / ffi_bps
        if (rust_bps is not None and ffi_bps is not None and ffi_bps > 0.0)
        else (
            ffi_ms / rust_ms
            if (rust_ms is not None and ffi_ms is not None and rust_ms > 0.0)
            else None
        )
    )

    has_comparable_ratio = (
        ratio_pack["rust_ratio"] is not None and ratio_pack["ffi_ratio"] is not None
    )
    has_comparable_speed = rust_timing is not None and ffi_timing is not None
    if not has_comparable_ratio and not has_comparable_speed:
        continue

    delta_rows.append(
        {
            "key": key,
            "scenario": scenario,
            "params": {
                "stage": stage,
                "level": level,
                "source": source,
            },
            "target": bench_target_id,
            "input_bytes": input_bytes,
            "ratio": {
                "rust": ratio_pack["rust_ratio"],
                "ffi": ratio_pack["ffi_ratio"],
                "delta_rust_over_ffi": ratio_pack["delta"],
                "status": ratio_pack["status"],
                "reference_band": {
                    "delta_low": DELTA_LOW,
                    "delta_high": DELTA_HIGH,
                },
                "interpretation": "delta<1 means Rust compressed output smaller than FFI; delta>1 means larger",
            },
            "speed": {
                "series": speed_series,
                "rust_ms_per_iter": rust_ms,
                "ffi_ms_per_iter": ffi_ms,
                "rust_bytes_per_sec": rust_bps,
                "ffi_bytes_per_sec": ffi_bps,
                "delta_rust_over_ffi": speed_delta,
                "status": classify_speed_delta(speed_delta),
                "reference_band": {
                    "delta_low": DELTA_LOW,
                    "delta_high": DELTA_HIGH,
                },
                "interpretation": "delta>1 means Rust faster than FFI; throughput ratio uses rust_bytes_per_sec/ffi_bytes_per_sec when available, otherwise fallback is ffi_ms_per_iter/rust_ms_per_iter",
            },
            "meta": {
                "target_label": bench_target_label,
                "target_triple": bench_target_triple or None,
                "commit_sha": commit_sha,
                "commit_message": commit_message,
                "generated_at": generated_at,
            },
        }
    )

with open("benchmark-delta.json", "w") as f:
    json.dump(delta_rows, f, indent=2)

relative_rows = []
for row in delta_rows:
    params = row["params"]
    common = {
        "target": row["target"],
        "stage": params["stage"],
        "scenario": row["scenario"],
        "level": params["level"],
        "source": params["source"],
        "key": row["key"],
        "commit_sha": row["meta"]["commit_sha"],
        "commit_message": row["meta"].get("commit_message"),
        "generated_at": row["meta"]["generated_at"],
    }

    ratio_delta = row["ratio"]["delta_rust_over_ffi"]
    if (
        ratio_delta is not None
        and row["ratio"]["rust"] is not None
        and row["ratio"]["ffi"] is not None
    ):
        relative_rows.append(
            {
                **common,
                "metric": "compression_ratio",
                "rust_value": row["ratio"]["rust"],
                "ffi_value": row["ratio"]["ffi"],
                "delta_ratio": ratio_delta,
                "delta_percent": (ratio_delta - 1.0) * 100.0,
                "status_band": row["ratio"]["status"],
                "interpretation": row["ratio"]["interpretation"],
            }
        )

    speed_delta = row["speed"]["delta_rust_over_ffi"]
    if (
        speed_delta is not None
        and row["speed"]["rust_bytes_per_sec"] is not None
        and row["speed"]["ffi_bytes_per_sec"] is not None
    ):
        relative_rows.append(
            {
                **common,
                "metric": "throughput_bytes_per_sec",
                "rust_value": row["speed"]["rust_bytes_per_sec"],
                "ffi_value": row["speed"]["ffi_bytes_per_sec"],
                "delta_ratio": speed_delta,
                "delta_percent": (speed_delta - 1.0) * 100.0,
                "status_band": row["speed"]["status"],
                "interpretation": "delta>1 means Rust faster than FFI",
            }
        )

# Add per-(scenario, level, stage) peak-memory rows so the dashboard
# can render a third metric alongside compression_ratio +
# throughput_bytes_per_sec. The memory stage strings from the bench
# (`compress`, `decompress-rust_stream`, `decompress-c_stream`) get
# normalised into the same (stage, source) shape the speed/ratio
# records already use so the dashboard's existing per-series grouping
# keeps working unchanged.
for row in memory_rows:
    rust_bytes = row["rust_peak_alloc_bytes"]
    ffi_bytes = row["ffi_peak_alloc_bytes"]
    # The memory bench emits the SAME `compress` / `decompress-<source>`
    # stage for the plain and dict variants and encodes dict-ness only in
    # the level name (`*_ldm_dict`). Recover it from the suffix, strip the
    # suffix off the level (so the dashboard level axis stays clean), and
    # move it into `stage` — matching the timing bench's `compress-dict` /
    # `decompress-dict` convention so plain and dict rows for one level
    # stay distinct instead of colliding on `(level, stage)`.
    raw_level = row["level"]
    is_dict = raw_level.endswith("_dict")
    level = strip_dict_level_suffix(raw_level)
    raw_stage = row["stage"]
    if raw_stage == "compress":
        stage = "compress"
        source = None
    elif raw_stage.startswith("decompress-"):
        stage = "decompress"
        source = raw_stage.removeprefix("decompress-")
    else:
        stage = raw_stage
        source = None
    if is_dict and stage in ("compress", "decompress"):
        stage = f"{stage}-dict"
    # delta_ratio = rust_peak / ffi_peak. Values > 1 mean Rust uses
    # MORE memory than FFI (worse for us — same direction as
    # compression_ratio, where >1 means Rust output is larger).
    # `ffi_bytes == 0` is a real datapoint (no libzstd allocations
    # for that scenario): emit the row with `delta_ratio: null` so the
    # dashboard keeps both side values; only the ratio is undefined.
    delta_ratio = (rust_bytes / ffi_bytes) if ffi_bytes > 0 else None
    delta_percent = (delta_ratio - 1.0) * 100.0 if delta_ratio is not None else None
    relative_rows.append(
        {
            "target": bench_target_id,
            "stage": stage,
            "scenario": row["scenario"],
            "level": level,
            "source": source,
            "key": canonical_key(stage, row["scenario"], level, source),
            "commit_sha": commit_sha,
            "commit_message": commit_message,
            "generated_at": generated_at,
            # Both sides feed the SAME pair of atomic counters in
            # `compare_ffi_memory`: Rust-side via the
            # `#[global_allocator]` tracking wrapper, FFI-side via the
            # `ZSTD_customMem` callbacks which call `System.alloc` /
            # `System.dealloc` directly and manually update the same
            # counters with only the libzstd-requested `size` (bypassing
            # the wrapper to avoid double-counting the 16-byte size
            # header those callbacks prepend). Cross-side ratio is
            # meaningful — `delta_ratio > 1` says Rust allocated more
            # bytes than FFI for the same workload.
            "metric": "peak_alloc_bytes",
            "rust_value": rust_bytes,
            "ffi_value": ffi_bytes,
            "delta_ratio": delta_ratio,
            "delta_percent": delta_percent,
            "status_band": "n/a",
            "interpretation": "delta>1 means Rust allocates more peak memory than FFI",
        }
    )

# compress-dict compression-ratio rows. The dict path emits REPORT_DICT
# (not REPORT), so its ratio data lives in `dictionary_rows`, separate from
# the non-dict `ratios`/`delta_rows`. Surface a `compression_ratio` relative
# row for the `compress-dict` stage so the dashboard's existing ratio series
# renders it (previously compress-dict had a timing series but no ratio
# graph). rust_value/ffi_value are the dict-compressed-size ratios
# (compressed / input); delta = rust/ffi, >1 meaning Rust's dict output is
# larger than FFI's (a ratio regression, same direction as the non-dict
# compression_ratio metric). Skip rows where the Rust dict size is 0 (the
# bench could not run the Rust dict path for that scenario/level).
for row in dictionary_rows:
    rust_ratio = row.get("rust_with_dict_ratio", 0.0)
    ffi_ratio = row["ffi_with_dict_ratio"]
    if rust_ratio <= 0.0 or ffi_ratio <= 0.0:
        continue
    delta_ratio = rust_ratio / ffi_ratio
    level = strip_dict_level_suffix(row["level"])
    relative_rows.append(
        {
            "target": bench_target_id,
            "stage": "compress-dict",
            "scenario": row["scenario"],
            "level": level,
            "source": None,
            "key": canonical_key("compress-dict", row["scenario"], level, None),
            "commit_sha": commit_sha,
            "commit_message": commit_message,
            "generated_at": generated_at,
            "metric": "compression_ratio",
            "rust_value": rust_ratio,
            "ffi_value": ffi_ratio,
            "delta_ratio": delta_ratio,
            "delta_percent": (delta_ratio - 1.0) * 100.0,
            "status_band": classify_ratio_delta(delta_ratio),
            "interpretation": "delta>1 means Rust dict-compressed output is larger than FFI",
        }
    )

# Stamp the run's CPU kernel tier onto every relative record. The deployed
# dashboard payload concatenates records from per-target runs, so the
# per-run `target.kernel` below would be overwritten on merge — carrying it
# per-record lets the dashboard map each target to the kernel that produced
# its numbers regardless of how the per-target files are merged.
for _r in relative_rows:
    _r["kernel"] = kernel_info

relative_payload = {
    "version": 1,
    "target": {
        "id": bench_target_id,
        "label": bench_target_label,
        "triple": bench_target_triple or None,
        # CPU kernel tier (entropy/sequence dispatch, shared encode/decode)
        # this run actually selected, plus arch / libc — so a dashboard
        # reading this can attribute every record to the kernel that
        # produced it. `None` if the bench binary predates REPORT_KERNEL.
        "kernel": kernel_info,
        # Host fingerprint of the shard that ran this target (CPU model, ISA
        # flags, governor, frequency, memory). Records which hardware produced
        # the numbers — e.g. surfaces an AVX-512 runner even when the AVX2
        # kernel was selected, so a divergence from another arch is explainable.
        "machine": machine_info,
    },
    "reference_band": {
        "delta_low": DELTA_LOW,
        "delta_high": DELTA_HIGH,
    },
    "commit_sha": commit_sha,
    "commit_message": commit_message,
    "generated_at": generated_at,
    "records": relative_rows,
}

with open("benchmark-relative.json", "w") as f:
    json.dump(relative_payload, f, indent=2)

lines = [
    "# Benchmark Report",
    "",
    "Generated by `.github/scripts/run-benchmarks.sh` from `cargo bench --bench compare_ffi`.",
    "",
    "## Compression Ratios",
    "",
    "| Scenario | Label | Level | Input bytes | Rust bytes | C bytes | Rust ratio | C ratio |",
    "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |",
]

for row in sorted(ratios, key=lambda item: (item["scenario"], item["level"])):
    label = markdown_table_escape(row["label"])
    lines.append(
        f'| {row["scenario"]} | {label} | {row["level"]} | {row["input_bytes"]} | {row["rust_bytes"]} | {row["ffi_bytes"]} | {row["rust_ratio"]:.4f} | {row["ffi_ratio"]:.4f} |'
    )

# Skip the entire memory section on PR shards (no memory bench ran,
# `memory_rows` is empty). The INFO log earlier in this script
# announced the omission — emitting a heading + empty table here would
# leave a confusing blank section in `benchmark-report.md`.
if memory_rows:
    lines.extend([
        "",
        "## Peak Allocation Bytes",
        "",
        "Both columns share one pair of atomic counters in the "
        "`compare_ffi_memory` bench: Rust allocations via the "
        "`#[global_allocator]` tracking wrapper, FFI allocations via "
        "`ZSTD_customMem` callbacks that call `System.alloc` / "
        "`System.dealloc` directly and manually update the same "
        "counters with the libzstd-requested size only. Byte counts "
        "are directly comparable cross-side.",
        "",
        "| Scenario | Label | Level | Stage | Rust peak alloc | C peak alloc |",
        "| --- | --- | --- | --- | ---: | ---: |",
    ])

    for row in sorted(memory_rows, key=lambda item: (item["scenario"], item["level"], item["stage"])):
        label = markdown_table_escape(row["label"])
        lines.append(
            f'| {row["scenario"]} | {label} | {row["level"]} | {row["stage"]} | {row["rust_peak_alloc_bytes"]} | {row["ffi_peak_alloc_bytes"]} |'
        )

lines.extend([
    "",
    "## Dictionary Compression (C FFI)",
    "",
    "| Scenario | Label | Level | Dict bytes | Train ms | C bytes (no dict) | C bytes (with dict) | C ratio (no dict) | C ratio (with dict) |",
    "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
])

for row in sorted(dictionary_rows, key=lambda item: (item["scenario"], item["level"])):
    label = markdown_table_escape(row["label"])
    lines.append(
        f'| {row["scenario"]} | {label} | {row["level"]} | {row["dict_bytes"]} | {row["train_ms"]:.3f} | {row["ffi_no_dict_bytes"]} | {row["ffi_with_dict_bytes"]} | {row["ffi_no_dict_ratio"]:.4f} | {row["ffi_with_dict_ratio"]:.4f} |'
    )
if not dictionary_rows:
    lines.append("| _n/a_ | _no dictionary rows emitted in this run_ | - | - | - | - | - | - | - |")

lines.extend([
    "",
    "## Dictionary Training (Rust FastCOVER vs C FFI)",
    "",
    "| Scenario | Label | Dict bytes (requested) | Rust train ms | C train ms | Rust dict bytes | C dict bytes | Rust FastCOVER score | Delta (C/Rust) | Status |",
    "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
])

for row in sorted(dictionary_training_rows, key=lambda item: item["scenario"]):
    label = markdown_table_escape(row["label"])
    delta = row["delta_ffi_over_rust"]
    delta_cell = f"{delta:.4f}" if delta is not None else "n/a"
    lines.append(
        f'| {row["scenario"]} | {label} | {row["dict_bytes_requested"]} | {row["rust_train_ms"]:.3f} | {row["ffi_train_ms"]:.3f} | {row["rust_dict_bytes"]} | {row["ffi_dict_bytes"]} | {row["rust_fastcover_score"]} | {delta_cell} | {row["status"]} |'
    )
if not dictionary_training_rows:
    lines.append("| _n/a_ | _no dictionary training rows emitted in this run_ | - | - | - | - | - | - | - | - |")

lines.extend([
    "",
    "## Timing Metrics",
    "",
    "| Benchmark | ms/iter |",
    "| --- | ---: |",
])

for name, ms in sorted(timings):
    lines.append(f"| `{name}` | {ms:.3f} |")

with open("benchmark-report.md", "w") as f:
    f.write("\n".join(lines) + "\n")

delta_lines = [
    "# Benchmark Delta Report",
    "",
    "Generated by `.github/scripts/run-benchmarks.sh` from `cargo bench --bench compare_ffi`.",
    "",
    "## Ratio pack",
    "",
    "Interpretation: lower ratio is better (smaller compressed output).",
    "",
    "### Rust compression ratio",
    "",
    "| Key | Rust ratio |",
    "| --- | ---: |",
]

def format_ratio(value):
    return f"{value:.6g}"

for row in delta_rows:
    key = markdown_table_escape(row["key"])
    rust_ratio = row["ratio"]["rust"]
    if rust_ratio is None:
        continue
    delta_lines.append(f"| {key} | {format_ratio(rust_ratio)} |")

delta_lines.extend(
    [
        "",
        "### FFI compression ratio",
        "",
        "| Key | FFI ratio |",
        "| --- | ---: |",
    ]
)

for row in delta_rows:
    key = markdown_table_escape(row["key"])
    ffi_ratio = row["ratio"]["ffi"]
    if ffi_ratio is None:
        continue
    delta_lines.append(f"| {key} | {format_ratio(ffi_ratio)} |")

delta_lines.extend(
    [
        "",
        "### Rust/FFI ratio delta",
        "",
        f"Reference band: `{DELTA_LOW:.2f}–{DELTA_HIGH:.2f}` (near parity).",
        "",
        "| Key | Delta | Status |",
        "| --- | ---: | --- |",
    ]
)

for row in delta_rows:
    key = markdown_table_escape(row["key"])
    delta = row["ratio"]["delta_rust_over_ffi"]
    if delta is None:
        continue
    status = row["ratio"]["status"]
    delta_lines.append(f"| {key} | {delta:.4f} | {status} |")

delta_lines.extend(
    [
        "",
        "## Speed pack",
        "",
        "Interpretation: higher speed is better; delta uses `rust_bytes_per_sec / ffi_bytes_per_sec` when throughput exists, otherwise fallback is `ffi_ms_per_iter / rust_ms_per_iter`.",
        "",
        "### Rust speed",
        "",
        "| Key | Rust bytes/sec | Rust ms/iter |",
        "| --- | ---: | ---: |",
    ]
)

for row in delta_rows:
    key = markdown_table_escape(row["key"])
    bps = row["speed"]["rust_bytes_per_sec"]
    ms = row["speed"]["rust_ms_per_iter"]
    if bps is None or ms is None:
        continue
    delta_lines.append(f"| {key} | {bps:.2f} | {ms:.3f} |")

delta_lines.extend(
    [
        "",
        "### FFI speed",
        "",
        "| Key | FFI bytes/sec | FFI ms/iter |",
        "| --- | ---: | ---: |",
    ]
)

for row in delta_rows:
    key = markdown_table_escape(row["key"])
    bps = row["speed"]["ffi_bytes_per_sec"]
    ms = row["speed"]["ffi_ms_per_iter"]
    if bps is None or ms is None:
        continue
    delta_lines.append(f"| {key} | {bps:.2f} | {ms:.3f} |")

delta_lines.extend(
    [
        "",
        "### Rust/FFI speed delta",
        "",
        f"Reference band: `{DELTA_LOW:.2f}–{DELTA_HIGH:.2f}` (near parity).",
        "",
        "| Key | Delta | Status |",
        "| --- | ---: | --- |",
    ]
)

for row in delta_rows:
    key = markdown_table_escape(row["key"])
    delta = row["speed"]["delta_rust_over_ffi"]
    if delta is None:
        continue
    status = row["speed"]["status"]
    delta_lines.append(f"| {key} | {delta:.4f} | {status} |")

with open("benchmark-delta.md", "w") as f:
    f.write("\n".join(delta_lines) + "\n")

print(
    f"Wrote {len(benchmark_results)} regression timing results to benchmark-results.json (selected from {timing_point_count} total timings)",
    file=sys.stderr,
)
print(f"Wrote {len(ratios)} ratio rows to benchmark-report.md", file=sys.stderr)
print(f"Wrote {len(memory_rows)} memory rows to benchmark-report.md", file=sys.stderr)
print(f"Wrote {len(dictionary_rows)} dictionary rows to benchmark-report.md", file=sys.stderr)
print(f"Wrote {len(dictionary_training_rows)} dictionary training rows to benchmark-report.md", file=sys.stderr)
print(f"Wrote {len(delta_rows)} canonical rows to benchmark-delta.json", file=sys.stderr)
print(f"Wrote {len(delta_rows)} canonical rows to benchmark-delta.md", file=sys.stderr)
print(f"Wrote {len(relative_rows)} relative rows to benchmark-relative.json", file=sys.stderr)
PYEOF
