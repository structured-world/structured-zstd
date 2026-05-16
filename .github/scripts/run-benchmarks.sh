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
  BENCH_CMD=(cargo bench --bench compare_ffi -p structured-zstd --features dict_builder)
  if [ -n "$BENCH_TARGET_TRIPLE" ]; then
    BENCH_CMD+=(--target "$BENCH_TARGET_TRIPLE")
  fi
  "${BENCH_CMD[@]}" -- --output-format bencher | tee "$BENCH_RAW_FILE"
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
    r'^REPORT_MEM scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) stage=(\S+) rust_peak_rss_delta_bytes=(\d+) ffi_peak_alloc_bytes=(\d+)$'
)
DICT_RE = re.compile(
    r'^REPORT_DICT scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) dict_bytes=(\d+) train_ms=([0-9.]+) ffi_no_dict_bytes=(\d+) ffi_with_dict_bytes=(\d+) ffi_no_dict_ratio=([0-9.]+) ffi_with_dict_ratio=([0-9.]+)$'
)
DICT_TRAIN_RE = re.compile(
    r'^REPORT_DICT_TRAIN scenario=(\S+) label="((?:[^"\\]|\\.)+)" training_bytes=(\d+) dict_bytes_requested=(\d+) rust_train_ms=([0-9.]+) ffi_train_ms=([0-9.]+) rust_dict_bytes=(\d+) ffi_dict_bytes=(\d+) rust_fastcover_score=(\d+)$'
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

benchmark_results = []
timings = []
ratios = []
memory_rows = []
dictionary_rows = []
dictionary_training_rows = []
timing_rows = []
scenario_input_bytes = {}
scenario_training_bytes = {}
raw_path = os.environ["BENCH_RAW_FILE"]
bench_target_label = os.environ.get("BENCH_TARGET_LABEL", "host")
bench_target_triple = os.environ.get("BENCH_TARGET_TRIPLE", "")
bench_target_id = os.environ.get("BENCH_TARGET_ID", bench_target_label)
commit_sha = os.environ.get("GITHUB_SHA")
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

def parse_benchmark_name(name):
    parts = name.split("/")
    if len(parts) == 5 and parts[0] == "compress" and parts[3] == "matrix":
        return {
            "stage": "compress",
            "level": parts[1],
            "scenario": parts[2],
            "source": None,
            "implementation": parts[4],
        }
    if len(parts) == 6 and parts[0] == "decompress" and parts[4] == "matrix":
        return {
            "stage": "decompress",
            "level": parts[1],
            "scenario": parts[2],
            "source": parts[3],
            "implementation": parts[5],
        }
    if len(parts) == 5 and parts[0] == "compress-dict" and parts[3] == "matrix":
        return {
            "stage": "compress-dict",
            "level": parts[1],
            "scenario": parts[2],
            "source": None,
            "implementation": parts[4],
        }
    if len(parts) == 5 and parts[0] == "dict-train" and parts[3] == "matrix":
        return {
            "stage": "dict-train",
            "level": parts[1],
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
                rust_peak_rss_delta_bytes,
                ffi_peak_alloc_bytes,
            ) = mem_match.groups()
            label = unescape_report_label(label)
            # Field semantics differ between sides — see emit_memory_report
            # in zstd/benches/compare_ffi.rs. Rust side is OS RSS delta
            # (proxy, may underreport on warm allocator arenas); FFI side
            # is the precise sum of bytes requested from libzstd's
            # ZSTD_customMem callbacks. Both stored side-by-side so the
            # dashboard can show each as its own series.
            memory_rows.append({
                "scenario": scenario,
                "label": label,
                "level": level,
                "stage": stage,
                "rust_peak_rss_delta_bytes": int(rust_peak_rss_delta_bytes),
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
            ) = dict_match.groups()
            label = unescape_report_label(label)
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
        benchmark_results = [
            {
                "name": name,
                "unit": "ms",
                "value": round(ms, 3),
            }
            for name, ms in timings
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
    print("ERROR: No REPORT_MEM lines parsed; memory section would be empty.", file=sys.stderr)
    sys.exit(1)

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
    rust_bytes = row["rust_peak_rss_delta_bytes"]
    ffi_bytes = row["ffi_peak_alloc_bytes"]
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
    # delta_ratio = rust_peak / ffi_peak. Values > 1 mean Rust uses
    # MORE memory than FFI (worse for us — same direction as
    # compression_ratio, where >1 means Rust output is larger).
    delta_ratio = (rust_bytes / ffi_bytes) if ffi_bytes > 0 else None
    if delta_ratio is None:
        continue
    relative_rows.append(
        {
            "target": bench_target_id,
            "stage": stage,
            "scenario": row["scenario"],
            "level": row["level"],
            "source": source,
            "key": canonical_key(stage, row["scenario"], row["level"], source),
            "commit_sha": commit_sha,
            "generated_at": generated_at,
            # Single grouping key so the dashboard can plot rust vs ffi
            # side-by-side. Cross-side values are NOT directly comparable:
            # `rust_value` is OS RSS delta (proxy, underreports on warm
            # arenas); `ffi_value` is precise libzstd customMem byte
            # count. The metric label is kept stable for dashboard
            # continuity — see compare_ffi.rs `emit_memory_report` for
            # the full asymmetry note.
            "metric": "peak_alloc_bytes",
            "rust_value": rust_bytes,
            "ffi_value": ffi_bytes,
            "delta_ratio": delta_ratio,
            "delta_percent": (delta_ratio - 1.0) * 100.0,
            "status_band": "n/a",
            "interpretation": "delta>1 means Rust allocates more peak memory than FFI",
        }
    )

relative_payload = {
    "version": 1,
    "target": {
        "id": bench_target_id,
        "label": bench_target_label,
        "triple": bench_target_triple or None,
    },
    "reference_band": {
        "delta_low": DELTA_LOW,
        "delta_high": DELTA_HIGH,
    },
    "commit_sha": commit_sha,
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

lines.extend([
    "",
    "## Peak Memory Bytes",
    "",
    "Rust column is OS resident-set-size delta during one encode/decode call "
    "(background-sampled). FFI column is precise sum of bytes requested from "
    "libzstd's `ZSTD_customMem` callbacks. Values are not directly comparable "
    "cross-side — they are different proxies for memory pressure during the "
    "operation. See bench source for details.",
    "",
    "| Scenario | Label | Level | Stage | Rust peak RSS delta | C peak alloc bytes |",
    "| --- | --- | --- | --- | ---: | ---: |",
])

for row in sorted(memory_rows, key=lambda item: (item["scenario"], item["level"], item["stage"])):
    label = markdown_table_escape(row["label"])
    lines.append(
        f'| {row["scenario"]} | {label} | {row["level"]} | {row["stage"]} | {row["rust_peak_rss_delta_bytes"]} | {row["ffi_peak_alloc_bytes"]} |'
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
