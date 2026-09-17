#!/usr/bin/env python3
"""Parse the wasm bench harness stdout into per-run dashboard records.

`zstd-wasm/bench/bench.mjs` emits one `REPORT` line per (scenario, level,
engine) and one `REPORT_DICT` line per (dict scenario, level, engine),
comparing our two wasm payloads (`ours-simd128`, `ours-scalar`) against the
most popular npm competitor (`bokuweb` = `@bokuweb/zstd-wasm`). Unlike the
native matrix this is an engine *triplet*, not a Rust↔FFI pair, so it lands
in its own dashboard section rather than the relative deltas payload.

This script reads the captured stdout and emits one flat record per
(kind, scenario, level, engine) carrying ratio + compress/decompress
throughput. `merge-wasm-bench.py` folds these into the persisted
`benchmark-wasm.json` timeseries on gh-pages.

Env vars:
  WASM_BENCH_RAW_FILE   path to the captured `node bench.mjs` stdout (required)
  GITHUB_SHA            commit sha stamped onto every record (optional)
  STRUCTURED_ZSTD_BENCH_GENERATED_AT
                        snapshot timestamp; defaults to now (UTC) (optional)
  STRUCTURED_ZSTD_BENCH_COMMIT_MESSAGE
                        commit subject; falls back to `git log -1` (optional)

Output: `benchmark-wasm-run.json` in the current working directory.
"""
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

# Both report kinds share the same key=value field set; only the leading
# token differs. `REPORT_STREAM` (streaming-vs-one-shot, ours only) is
# intentionally ignored — it has no competitor baseline so it doesn't
# belong in the vs-bokuweb section.
REPORT_KINDS = {"REPORT": "plain", "REPORT_DICT": "dict"}


def parse_kv(tokens):
    """Turn `key=value` tokens into a dict; tokens without `=` are skipped."""
    out = {}
    for tok in tokens:
        if "=" not in tok:
            continue
        key, _, value = tok.partition("=")
        out[key] = value
    return out


def throughput_bps(input_bytes, ns):
    """Bytes/sec relative to the original input size (matches the native
    dashboard's `input_bytes / seconds` throughput convention). Returns
    None when timing is missing or non-positive so the dashboard can skip
    the point rather than plot a bogus zero."""
    if ns is None or ns <= 0 or input_bytes is None:
        return None
    return input_bytes / (ns / 1_000_000_000.0)


def resolve_commit_message(commit_sha):
    explicit = os.environ.get("STRUCTURED_ZSTD_BENCH_COMMIT_MESSAGE")
    if explicit:
        first = explicit.strip().splitlines()
        return first[0].strip() if first else None
    try:
        subject = subprocess.run(
            ["git", "log", "-1", "--format=%s", commit_sha or "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        return subject or None
    except Exception:
        return None


def main():
    raw_path = os.environ.get("WASM_BENCH_RAW_FILE")
    if not raw_path:
        print("ERROR: set WASM_BENCH_RAW_FILE=<captured bench.mjs stdout>", file=sys.stderr)
        return 2
    raw = Path(raw_path)
    if not raw.is_file():
        print(f"ERROR: WASM_BENCH_RAW_FILE={raw_path} does not exist", file=sys.stderr)
        return 2

    commit_sha = os.environ.get("GITHUB_SHA")
    commit_message = resolve_commit_message(commit_sha)
    generated_at = (
        os.environ.get("STRUCTURED_ZSTD_BENCH_GENERATED_AT")
        or datetime.now(timezone.utc).isoformat()
    )

    records = []
    engines = []
    roundtrip_failures = []
    for raw_line in raw.read_text().splitlines():
        line = raw_line.strip()
        if not line:
            continue
        tokens = line.split()
        kind = REPORT_KINDS.get(tokens[0])
        if kind is None:
            continue
        kv = parse_kv(tokens[1:])
        scenario = kv.get("scenario")
        engine = kv.get("engine")
        level_raw = kv.get("level")
        if scenario is None or engine is None or level_raw is None:
            print(f"WARN: skipping malformed {tokens[0]} line: {line}", file=sys.stderr)
            continue
        try:
            level = int(level_raw)
            input_bytes = int(kv["input_bytes"])
            framed_bytes = int(kv["framed_bytes"])
            ratio = float(kv["ratio"])
            compress_ns = int(kv["compress_ns"])
            decompress_ns = int(kv["decompress_ns"])
        except (KeyError, ValueError) as exc:
            print(f"WARN: skipping {tokens[0]} line (bad field {exc}): {line}", file=sys.stderr)
            continue
        roundtrip_ok = kv.get("roundtrip") == "ok"
        if not roundtrip_ok:
            roundtrip_failures.append(f"{kind}/{scenario}/L{level}/{engine}")
        if engine not in engines:
            engines.append(engine)
        records.append({
            "kind": kind,
            "scenario": scenario,
            "level": level,
            "engine": engine,
            "input_bytes": input_bytes,
            "framed_bytes": framed_bytes,
            "ratio": ratio,
            "compress_ns": compress_ns,
            "decompress_ns": decompress_ns,
            "compress_bytes_per_sec": throughput_bps(input_bytes, compress_ns),
            "decompress_bytes_per_sec": throughput_bps(input_bytes, decompress_ns),
            "roundtrip_ok": roundtrip_ok,
            "commit_sha": commit_sha,
            "commit_message": commit_message,
            "generated_at": generated_at,
        })

    if not records:
        print("ERROR: no REPORT / REPORT_DICT lines parsed from wasm bench output", file=sys.stderr)
        return 1

    payload = {
        "version": 1,
        # The competitor every `ours-*` engine is benchmarked against; the
        # dashboard divides our throughput by this engine's to show the
        # speed ratio over time.
        "reference_engine": "bokuweb",
        "engines": engines,
        "commit_sha": commit_sha,
        "commit_message": commit_message,
        "generated_at": generated_at,
        "records": records,
    }
    Path("benchmark-wasm-run.json").write_text(json.dumps(payload, indent=2) + "\n")
    print(
        f"Parsed {len(records)} wasm records "
        f"({len(engines)} engines: {', '.join(engines)})",
        file=sys.stderr,
    )
    if roundtrip_failures:
        # A round-trip failure means a payload produced a frame that did not
        # decode back to the input — the numbers for that cell are
        # meaningless. Surface it loudly; the bench harness itself already
        # exits non-zero, so CI fails regardless, but we keep the records so
        # the dashboard can still show the surrounding (valid) series.
        print(
            f"WARN: {len(roundtrip_failures)} round-trip failure(s): "
            + ", ".join(roundtrip_failures),
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
