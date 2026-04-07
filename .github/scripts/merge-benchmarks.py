#!/usr/bin/env python3
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path


root = Path("benchmark-artifacts")
out = Path("merged")
out.mkdir(parents=True, exist_ok=True)

relative_records = []
reference_band = None
delta_records = []
target_order = []
target_counts = {}
summary_lines = [
    "# Benchmark Summary (multi-target)",
    "",
    "Generated from CI benchmark matrix artifacts.",
    "",
    "## Targets",
    "",
    "| Target | Relative rows | Delta rows |",
    "| --- | ---: | ---: |",
]
delta_md_lines = [
    "# Benchmark Delta Summary (multi-target)",
    "",
    "Canonical rows merged across benchmark targets.",
    "",
]
report_bodies = {}
delta_md_bodies = {}
targets_meta = {}
RELATIVE_RETENTION_DAYS = 180
RELATIVE_MAX_RECORDS = 20000


def register_target(target, target_meta=None):
    if target not in target_order:
        target_order.append(target)
    if target not in target_counts:
        target_counts[target] = {"relative": 0, "delta": 0}
    if target_meta:
        existing = targets_meta.get(target, {})
        merged_meta = {
            "id": target,
            "label": target_meta.get("label") or existing.get("label") or target,
            "triple": target_meta.get("triple") if target_meta.get("triple") is not None else existing.get("triple"),
        }
        targets_meta[target] = merged_meta


def trim_leading_h1(markdown):
    lines = markdown.splitlines()
    if lines and lines[0].lstrip().startswith("# "):
        lines = lines[1:]
        if lines and not lines[0].strip():
            lines = lines[1:]
    return "\n".join(lines).strip()


def parse_generated_at(row):
    stamp = row.get("generated_at")
    if not stamp:
        return None
    try:
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).astimezone(timezone.utc)
    except ValueError:
        return None


for rel_path in sorted(root.rglob("benchmark-relative.*.json")):
    payload = json.loads(rel_path.read_text())
    band = payload.get("reference_band")
    if band:
        if reference_band is not None and reference_band != band:
            raise SystemExit(f"Inconsistent reference_band in {rel_path}")
        reference_band = band
    target_payload = payload.get("target") or {}
    filename_target = rel_path.name.replace("benchmark-relative.", "").replace(".json", "")
    target = target_payload.get("id") or filename_target
    if not target:
        raise SystemExit(f"Unable to determine relative target in {rel_path.name}")
    rows = payload.get("records", [])
    register_target(target, target_payload)
    for row in rows:
        enriched = dict(row)
        if not enriched.get("target"):
            enriched["target"] = target
        elif enriched["target"] != target:
            raise SystemExit(
                f"Inconsistent relative target in {rel_path.name}: expected {target}, got {enriched['target']}"
            )
        if target_payload.get("label") and not enriched.get("target_label"):
            enriched["target_label"] = target_payload["label"]
        if target_payload.get("triple") and not enriched.get("target_triple"):
            enriched["target_triple"] = target_payload["triple"]
        relative_records.append(enriched)
    target_counts[target]["relative"] += len(rows)

for report_path in sorted(root.rglob("benchmark-report.*.md")):
    target = report_path.name.replace("benchmark-report.", "").replace(".md", "")
    register_target(target)
    report_bodies[target] = report_path.read_text().strip()

for delta_path in sorted(root.rglob("benchmark-delta.*.json")):
    rows = json.loads(delta_path.read_text())
    normalized_rows = []
    filename_target = delta_path.name.replace("benchmark-delta.", "").replace(".json", "")
    if rows:
        raw_target = rows[0].get("target")
        if raw_target:
            target = raw_target
        else:
            target = filename_target
            print(
                f"Warning: missing delta target field in {delta_path.name}; "
                f"falling back to filename target={target}"
            )
    else:
        target = filename_target
    for row in rows:
        enriched = dict(row)
        if not enriched.get("target"):
            enriched["target"] = target
        elif enriched["target"] != target:
            raise SystemExit(
                f"Inconsistent delta target in {delta_path.name}: expected {target}, got {enriched['target']}"
            )
        normalized_rows.append(enriched)
    delta_records.extend(normalized_rows)
    register_target(target)
    target_counts[target]["delta"] += len(normalized_rows)

for delta_md_path in sorted(root.rglob("benchmark-delta.*.md")):
    target = delta_md_path.name.replace("benchmark-delta.", "").replace(".md", "")
    register_target(target)
    delta_md_bodies[target] = delta_md_path.read_text().strip()

if not relative_records:
    raise SystemExit("No relative records found in benchmark artifacts")
if not delta_records:
    raise SystemExit("No delta records found in benchmark artifacts")
current_targets = list(target_order)
missing_relative = sorted(target for target in current_targets if target_counts[target]["relative"] == 0)
missing_delta = sorted(target for target in current_targets if target_counts[target]["delta"] == 0)
missing_reports = sorted(target for target in current_targets if target not in report_bodies)
missing_delta_reports = sorted(target for target in current_targets if target not in delta_md_bodies)
if missing_relative or missing_delta or missing_reports or missing_delta_reports:
    raise SystemExit(
        "Incomplete benchmark artifacts: "
        f"missing_relative={missing_relative}, "
        f"missing_delta={missing_delta}, "
        f"missing_reports={missing_reports}, "
        f"missing_delta_reports={missing_delta_reports}"
    )

existing_relative_path = Path("gh-pages/dev/bench/benchmark-relative.json")
existing_records = []
if existing_relative_path.exists():
    existing_payload = json.loads(existing_relative_path.read_text())
    existing_records = existing_payload.get("records", [])
    existing_band = existing_payload.get("reference_band")
    if reference_band is None and existing_band:
        reference_band = existing_band
    for target_meta in existing_payload.get("targets_meta", []):
        if not isinstance(target_meta, dict):
            continue
        target_id = target_meta.get("id")
        if not target_id:
            continue
        register_target(target_id, target_meta)

key = lambda row: (
    row.get("commit_sha"),
    row.get("target"),
    row.get("metric"),
    row.get("key"),
    row.get("generated_at"),
)
merged = {}
for row in existing_records + relative_records:
    merged[key(row)] = row
merged_values = sorted(
    merged.values(),
    key=lambda row: (
        parse_generated_at(row) or datetime.max.replace(tzinfo=timezone.utc),
        row.get("target") or "",
        row.get("metric") or "",
        row.get("key") or "",
    ),
)
for row in merged_values:
    target = row.get("target")
    target_meta = targets_meta.get(target) if target else None
    if target_meta:
        if target_meta.get("label") and not row.get("target_label"):
            row["target_label"] = target_meta["label"]
        if "target_triple" not in row:
            row["target_triple"] = target_meta.get("triple")
cutoff = datetime.now(timezone.utc) - timedelta(days=RELATIVE_RETENTION_DAYS)
retained_values = []
for row in merged_values:
    parsed = parse_generated_at(row)
    if parsed is None or parsed >= cutoff:
        retained_values.append(row)
if len(retained_values) > RELATIVE_MAX_RECORDS:
    retained_values = retained_values[-RELATIVE_MAX_RECORDS:]

merged_relative_payload = {
    "version": 1,
    "targets": list(target_order),
    "targets_meta": [
        targets_meta.get(target, {"id": target, "label": target, "triple": None})
        for target in target_order
    ],
    "reference_band": reference_band or {
        "delta_low": 0.99,
        "delta_high": 1.05,
    },
    "records": retained_values,
}

for target in target_order:
    counts = target_counts.get(target, {"relative": 0, "delta": 0})
    summary_lines.append(f"| `{target}` | {counts['relative']} | {counts['delta']} |")
    delta_md_lines.append(f"- `{target}`: {counts['delta']} delta rows, {counts['relative']} relative rows")

report_lines = ["# Benchmark Report (multi-target)", ""]
for target in target_order:
    body = report_bodies.get(target)
    if not body:
        continue
    report_lines.append(f"## Target `{target}`")
    report_lines.append("")
    report_lines.append(trim_leading_h1(body))
    report_lines.append("")
if len(report_lines) == 2:
    raise SystemExit("No benchmark-report.*.md artifacts found")

delta_lines = ["# Benchmark Delta Report (multi-target)", ""]
for target in target_order:
    body = delta_md_bodies.get(target)
    if not body:
        continue
    delta_lines.append(f"## Target `{target}`")
    delta_lines.append("")
    delta_lines.append(trim_leading_h1(body))
    delta_lines.append("")
if len(delta_lines) == 2:
    raise SystemExit("No benchmark-delta.*.md artifacts found")

(out / "benchmark-relative.json").write_text(json.dumps(merged_relative_payload, indent=2) + "\n")
(out / "benchmark-delta.json").write_text(json.dumps(delta_records, indent=2) + "\n")
(out / "benchmark-summary.md").write_text("\n".join(summary_lines) + "\n")
(out / "benchmark-delta-summary.md").write_text("\n".join(delta_md_lines) + "\n")
(out / "benchmark-report.md").write_text("\n".join(report_lines).strip() + "\n")
(out / "benchmark-delta.md").write_text("\n".join(delta_lines).strip() + "\n")
