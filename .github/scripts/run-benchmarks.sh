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
BENCH_RAW_FILE="$(mktemp -t structured-zstd-bench-raw.XXXXXX)"
trap 'rm -f "$BENCH_RAW_FILE"' EXIT

export STRUCTURED_ZSTD_EMIT_REPORT=1
cargo bench --bench compare_ffi -p structured-zstd -- --output-format bencher | tee "$BENCH_RAW_FILE"

echo "Parsing results..." >&2

BENCH_RAW_FILE="$BENCH_RAW_FILE" python3 - <<'PYEOF'
import json
import os
import re
import sys

BENCH_RE = re.compile(r"test (\S+)\s+\.\.\. bench:\s+([\d,]+) ns/iter")
REPORT_RE = re.compile(
    r'^REPORT scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) input_bytes=(\d+) rust_bytes=(\d+) ffi_bytes=(\d+) rust_ratio=([0-9.]+) ffi_ratio=([0-9.]+)$'
)
MEM_RE = re.compile(
    r'^REPORT_MEM scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) stage=(\S+) rust_buffer_bytes_estimate=(\d+) ffi_buffer_bytes_estimate=(\d+)$'
)
DICT_RE = re.compile(
    r'^REPORT_DICT scenario=(\S+) label="((?:[^"\\]|\\.)+)" level=(\S+) dict_bytes=(\d+) train_ms=([0-9.]+) ffi_no_dict_bytes=(\d+) ffi_with_dict_bytes=(\d+) ffi_no_dict_ratio=([0-9.]+) ffi_with_dict_ratio=([0-9.]+)$'
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
    escaped = value.replace("\\", "\\\\")
    escaped = escaped.replace("|", "\\|")
    return escaped.replace("\n", "<br>")

benchmark_results = []
timings = []
ratios = []
memory_rows = []
dictionary_rows = []
raw_path = os.environ["BENCH_RAW_FILE"]

with open(raw_path) as f:
    for raw_line in f:
        line = raw_line.strip()

        bench_match = BENCH_RE.match(line)
        if bench_match:
            name = bench_match.group(1)
            ns = int(bench_match.group(2).replace(",", ""))
            ms = ns / 1_000_000
            benchmark_results.append({
                "name": name,
                "unit": "ms",
                "value": round(ms, 3),
            })
            timings.append((name, ms))
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
            continue

        mem_match = MEM_RE.match(line)
        if mem_match:
            (
                scenario,
                label,
                level,
                stage,
                rust_buffer_bytes_estimate,
                ffi_buffer_bytes_estimate,
            ) = mem_match.groups()
            label = unescape_report_label(label)
            memory_rows.append({
                "scenario": scenario,
                "label": label,
                "level": level,
                "stage": stage,
                "rust_buffer_bytes_estimate": int(rust_buffer_bytes_estimate),
                "ffi_buffer_bytes_estimate": int(ffi_buffer_bytes_estimate),
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

if not benchmark_results:
    print("ERROR: No benchmark results parsed!", file=sys.stderr)
    sys.exit(1)

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
    print("WARN: No REPORT_DICT lines parsed; dictionary section will be empty.", file=sys.stderr)

with open("benchmark-results.json", "w") as f:
    json.dump(benchmark_results, f, indent=2)

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
    "## Buffer Size Estimates (Input + Output)",
    "",
    "| Scenario | Label | Level | Stage | Rust buffer bytes (estimate) | C buffer bytes (estimate) |",
    "| --- | --- | --- | --- | ---: | ---: |",
])

for row in sorted(memory_rows, key=lambda item: (item["scenario"], item["level"], item["stage"])):
    label = markdown_table_escape(row["label"])
    lines.append(
        f'| {row["scenario"]} | {label} | {row["level"]} | {row["stage"]} | {row["rust_buffer_bytes_estimate"]} | {row["ffi_buffer_bytes_estimate"]} |'
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

print(f"Wrote {len(benchmark_results)} timing results to benchmark-results.json", file=sys.stderr)
print(f"Wrote {len(ratios)} ratio rows to benchmark-report.md", file=sys.stderr)
print(f"Wrote {len(memory_rows)} memory rows to benchmark-report.md", file=sys.stderr)
print(f"Wrote {len(dictionary_rows)} dictionary rows to benchmark-report.md", file=sys.stderr)
PYEOF
