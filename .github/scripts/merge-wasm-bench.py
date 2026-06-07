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
    payload = json.loads(p.read_text())
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

    merged = {}
    for row in load_records(existing_file) + run_records:
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
