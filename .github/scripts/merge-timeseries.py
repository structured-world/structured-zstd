#!/usr/bin/env python3
"""Merge one run's records into a persisted gh-pages timeseries.

Mirrors `merge-benchmarks.py`'s accumulate-then-trim behaviour for the
dashboard sections that keep their own history (the wasm bench in
`benchmark-wasm.json`, the cross-validation in `benchmark-xval.json`): an
append-only list of (kind, scenario, level, engine) datapoints across
commits. Each push adds one snapshot; old snapshots age out after
`RETENTION_DAYS`. The run payload's other top-level fields (reference
engine, engines) replace the persisted ones; anything that must stay true of
an old snapshot lives on its records.

Env vars:
  TIMESERIES_RUN_FILE       this run's payload (required)
  TIMESERIES_EXISTING_FILE  persisted history to merge into (optional; a
                            fresh gh-pages has none)
  TIMESERIES_OUTPUT_FILE    merged output path (required)
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
    try:
        payload = json.loads(p.read_text())
    except (json.JSONDecodeError, ValueError) as exc:
        # A corrupted persisted file (partial write, manual tamper) must not
        # block every future publish. Warn loudly (visible in CI logs)
        # and rebuild from this run's records — history re-accumulates over
        # subsequent pushes rather than wedging the shard permanently.
        print(f"WARN: corrupted existing file {path}, starting fresh: {exc}", file=sys.stderr)
        return []
    return payload.get("records", [])


def main():
    run_file = os.environ.get("TIMESERIES_RUN_FILE")
    existing_file = os.environ.get("TIMESERIES_EXISTING_FILE")
    output_file = os.environ.get("TIMESERIES_OUTPUT_FILE")
    if not run_file or not output_file:
        print("ERROR: set TIMESERIES_RUN_FILE and TIMESERIES_OUTPUT_FILE", file=sys.stderr)
        return 2

    run_path = Path(run_file)
    if not run_path.is_file():
        print(f"ERROR: TIMESERIES_RUN_FILE={run_file} not found", file=sys.stderr)
        return 2
    run_payload = json.loads(run_path.read_text())
    run_records = run_payload.get("records", [])
    if not run_records:
        print("ERROR: this run produced no records to merge", file=sys.stderr)
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

    # Snapshot-level fields (commit, timestamp) live on every record; the
    # section-level ones (reference engine, engines) carry over.
    snapshot_fields = {"records", "commit_sha", "commit_message", "generated_at"}
    payload = {
        **{k: v for k, v in run_payload.items() if k not in snapshot_fields},
        "version": 1,
        "records": retained,
    }
    Path(output_file).write_text(json.dumps(payload, indent=2) + "\n")
    print(
        f"Merged {len(run_records)} new + {len(merged) - len(run_records)} "
        f"existing → {len(retained)} retained records → {output_file}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
