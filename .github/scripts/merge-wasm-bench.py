#!/usr/bin/env python3
"""Merge this run's wasm bench records into the persisted gh-pages timeseries.

Mirrors `merge-benchmarks.py`'s accumulate-then-trim behaviour for the wasm
section: the dashboard's `benchmark-wasm.json` is an append-only history of
(scenario, level, engine) datapoints across commits. Each push adds one
snapshot; old snapshots age out after `RETENTION_DAYS`.

Env vars:
  WASM_RUN_FILE       this run's records (default: benchmark-wasm-run.json)
  WASM_EXISTING_FILE  persisted history to merge into (optional — first run
                      on a fresh gh-pages has none)
  WASM_OUTPUT_FILE    merged output path (default: benchmark-wasm.json)
"""
import json
import os
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

# Must match the stamp `parse-wasm-bench.py` writes onto each record.
TIMING_ESTIMATOR = "sample-min-common-count"

# A wasm record carries its byte counts and its timings together, unlike the
# native payload where each row is one metric. So a record measured by another
# estimator cannot simply be dropped: that would take an exact `ratio` with it.
# Blank the timings instead and keep the rest. The dashboard already treats a
# non-numeric throughput as "no point here" rather than plotting a zero.
TIMING_FIELDS = (
    "compress_ns",
    "decompress_ns",
    "compress_bytes_per_sec",
    "decompress_bytes_per_sec",
)


def without_incomparable_timings(row):
    if row.get("estimator") == TIMING_ESTIMATOR:
        return row
    return {**row, **{field: None for field in TIMING_FIELDS if field in row}}

RETENTION_DAYS = 180
MAX_RECORDS = 20000


def parse_generated_at(row):
    stamp = row.get("generated_at")
    if not stamp:
        return None
    try:
        return datetime.fromisoformat(str(stamp).replace("Z", "+00:00")).astimezone(timezone.utc)
    except ValueError:
        return None


def record_key(row):
    # One datapoint per (snapshot, kind, scenario, level, engine). A re-run
    # of the same commit (CI retry) overwrites rather than duplicates.
    return (
        row.get("commit_sha"),
        row.get("generated_at"),
        row.get("kind"),
        row.get("scenario"),
        row.get("level"),
        row.get("engine"),
    )


def load_records(path):
    if not path:
        return []
    p = Path(path)
    if not p.is_file():
        return []
    try:
        payload = json.loads(p.read_text())
    except (json.JSONDecodeError, ValueError) as exc:
        # A corrupted persisted file (partial write, manual tamper) must not
        # block every future wasm publish. Warn loudly (visible in CI logs)
        # and rebuild from this run's records — history re-accumulates over
        # subsequent pushes rather than wedging the shard permanently.
        print(f"WARN: corrupted existing file {path}, starting fresh: {exc}", file=sys.stderr)
        return []
    return payload.get("records", [])


def main():
    run_file = os.environ.get("WASM_RUN_FILE", "benchmark-wasm-run.json")
    existing_file = os.environ.get("WASM_EXISTING_FILE")
    output_file = os.environ.get("WASM_OUTPUT_FILE", "benchmark-wasm.json")

    run_path = Path(run_file)
    if not run_path.is_file():
        print(f"ERROR: WASM_RUN_FILE={run_file} not found", file=sys.stderr)
        return 2
    run_payload = json.loads(run_path.read_text())
    run_records = run_payload.get("records", [])
    if not run_records:
        print("ERROR: this run produced no wasm records to merge", file=sys.stderr)
        return 1

    # Timings are comparable only within one estimator. Retained points from
    # before the stamp existed were medians of the samples, which sit away from
    # the minimum published now; plotting both as one series would draw a step
    # where only the measurement changed. Their byte counts are exact whatever
    # the estimator, so only the timings go.
    existing = load_records(existing_file)
    kept = [without_incomparable_timings(row) for row in existing]
    blanked = sum(1 for row in existing if row.get("estimator") != TIMING_ESTIMATOR)
    if blanked:
        print(
            f"INFO: blanking the timings on {blanked} retained wasm rows measured "
            f"by a different estimator than {TIMING_ESTIMATOR!r}; their ratios are "
            "kept.",
            file=sys.stderr,
        )

    merged = {}
    for row in kept + run_records:
        merged[record_key(row)] = row

    values = sorted(
        merged.values(),
        key=lambda row: (
            parse_generated_at(row) or datetime.min.replace(tzinfo=timezone.utc),
            str(row.get("kind") or ""),
            str(row.get("scenario") or ""),
            row.get("level") if isinstance(row.get("level"), int) else 0,
            str(row.get("engine") or ""),
        ),
    )

    cutoff = datetime.now(timezone.utc) - timedelta(days=RETENTION_DAYS)
    retained = [
        row for row in values
        if (parsed := parse_generated_at(row)) is None or parsed >= cutoff
    ]
    if len(retained) > MAX_RECORDS:
        retained = retained[-MAX_RECORDS:]

    payload = {
        "version": 1,
        "reference_engine": run_payload.get("reference_engine", "bokuweb"),
        "engines": run_payload.get("engines", []),
        "records": retained,
    }
    Path(output_file).write_text(json.dumps(payload, indent=2) + "\n")
    print(
        f"Merged {len(run_records)} new + {len(merged) - len(run_records)} "
        f"existing → {len(retained)} retained wasm records → {output_file}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
