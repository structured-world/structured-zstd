#!/usr/bin/env python3
"""Aggregate per-(target, level) benchmark artifacts into per-target
consolidated files, ready for `merge-benchmarks.py` (cross-target).

After the CI bench matrix was split into one runner per level, each
runner publishes one set of:
  benchmark-results.<TARGET>.<LEVEL>.json
  benchmark-report.<TARGET>.<LEVEL>.md
  benchmark-delta.<TARGET>.<LEVEL>.json
  benchmark-delta.<TARGET>.<LEVEL>.md
  benchmark-relative.<TARGET>.<LEVEL>.json

This script reads the downloaded artifacts under
`benchmark-artifacts/` and emits per-target files matching the
single-runner naming used pre-split, so the existing
`merge-benchmarks.py` cross-target merge keeps working unchanged:
  benchmark-results.<TARGET>.json
  benchmark-report.<TARGET>.md
  benchmark-delta.<TARGET>.json
  benchmark-delta.<TARGET>.md
  benchmark-relative.<TARGET>.json

Required env var:
  AGGREGATE_TARGETS=x86_64-gnu,i686-gnu,x86_64-musl

Outputs land in the current working directory.
"""
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path("benchmark-artifacts")
TARGETS = [
    t.strip()
    for t in os.environ.get("AGGREGATE_TARGETS", "").split(",")
    if t.strip()
]
if not TARGETS:
    print("ERROR: set AGGREGATE_TARGETS=<comma-separated target ids>", file=sys.stderr)
    sys.exit(2)


def files_for(target, kind, ext):
    """Match `benchmark-<kind>.<target>.<level>.<ext>` under any artifact dir."""
    pattern = re.compile(
        rf"^benchmark-{re.escape(kind)}\.{re.escape(target)}\.[^.]+\.{re.escape(ext)}$"
    )
    return sorted(p for p in ROOT.rglob("*") if p.is_file() and pattern.match(p.name))


def merge_results_json(target):
    rows = []
    for p in files_for(target, "results", "json"):
        rows.extend(json.loads(p.read_text()))
    if not rows:
        print(f"WARN[{target}]: no benchmark-results.*.json shards found", file=sys.stderr)
    Path(f"benchmark-results.{target}.json").write_text(json.dumps(rows, indent=2) + "\n")


def merge_delta_json(target):
    rows = []
    for p in files_for(target, "delta", "json"):
        rows.extend(json.loads(p.read_text()))
    if not rows:
        print(f"WARN[{target}]: no benchmark-delta.*.json shards found", file=sys.stderr)
    Path(f"benchmark-delta.{target}.json").write_text(json.dumps(rows, indent=2) + "\n")


def merge_relative_json(target):
    """Each shard has the same top-level metadata (target, reference_band,
    commit_sha, generated_at) — keep the first shard's metadata and
    concatenate `records`. Drop duplicates by (metric, key, level)."""
    payload = None
    seen = set()
    records = []
    for p in files_for(target, "relative", "json"):
        shard = json.loads(p.read_text())
        if payload is None:
            payload = {k: v for k, v in shard.items() if k != "records"}
        for rec in shard.get("records", []):
            sig = (rec.get("metric"), rec.get("key"), rec.get("level"))
            if sig in seen:
                continue
            seen.add(sig)
            records.append(rec)
    if payload is None:
        print(f"WARN[{target}]: no benchmark-relative.*.json shards found", file=sys.stderr)
        payload = {"version": 1, "target": {"id": target}, "records": []}
    payload["records"] = records
    Path(f"benchmark-relative.{target}.json").write_text(json.dumps(payload, indent=2) + "\n")


def merge_markdown(target, kind):
    """Concatenate shard reports with a short strategy-group header
    above each shard's body. Each shard now bundles every level of a
    strategy family (`fast` / `dfast` / `greedy` / `lazy` / `btopt` /
    `btultra` / `btultra2`) or, on PR runs, the canonical pair
    (`pr-canonical` = level_3 + level_22). The merge-benchmarks.py
    cross-target step consumes these — it already strips the leading
    `# Title` line, so we keep that convention."""
    shards = files_for(target, kind, "md")
    if not shards:
        print(f"WARN[{target}]: no benchmark-{kind}.*.md shards found", file=sys.stderr)
        return
    title = (
        "Benchmark Report" if kind == "report" else "Benchmark Delta Report"
    )
    lines = [f"# {title} ({target})", ""]
    for shard in shards:
        # Extract shard id from filename suffix:
        # `benchmark-<kind>.<target>.<shard_id>.md`. The shard id is
        # a strategy family on main pushes (`fast`, `dfast`, ...) or
        # `pr-canonical` on pull-request shards.
        stem = shard.name
        prefix = f"benchmark-{kind}.{target}."
        shard_id = stem[len(prefix):-len(".md")]
        body = shard.read_text().strip()
        # Drop the shard's `# ...` line so we don't get nested H1s.
        body_lines = body.splitlines()
        if body_lines and body_lines[0].lstrip().startswith("# "):
            body_lines = body_lines[1:]
            if body_lines and not body_lines[0].strip():
                body_lines = body_lines[1:]
        lines.append(f"## Strategy group: {shard_id}")
        lines.append("")
        lines.extend(body_lines)
        lines.append("")
    Path(f"benchmark-{kind}.{target}.md").write_text("\n".join(lines).strip() + "\n")


for target in TARGETS:
    print(f"Aggregating target={target}", file=sys.stderr)
    merge_results_json(target)
    merge_delta_json(target)
    merge_relative_json(target)
    merge_markdown(target, "report")
    merge_markdown(target, "delta")

print("Done.", file=sys.stderr)
